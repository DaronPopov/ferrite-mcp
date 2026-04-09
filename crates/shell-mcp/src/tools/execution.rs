//! exec, build_check, and task_run tool implementations.
//!
//! exec:        Run any command. Returns stdout/stderr/exit/duration as structured JSON.
//! build_check: Smart compilation — auto-resolves CUDA flags, returns parsed error list.
//! task_run:    Write a script to a tempfile and execute it atomically in one call.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::terminal;
use crate::tools::state::read_cwd;

// ── exec ──────────────────────────────────────────────────────────────────────

pub fn exec_cmd(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let cmd = args["cmd"].as_str().ok_or("exec: 'cmd' is required")?;

    let state_cwd = read_cwd(state);
    // Re-read config from disk each call so changes take effect without restart
    let config = crate::config::FerriteConfig::load();

    let cwd = args["cwd"].as_str().map(PathBuf::from).unwrap_or(state_cwd);

    let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(60);
    let stdin_data = args["stdin"].as_str().unwrap_or("").to_owned();

    // Pre-process: rewrite command to non-interactive form + inject baseline env.
    // This is transparent — the agent always gets the best-effort non-blocking version.
    let (cmd_rewritten, perm_env) = crate::permissions::preprocess(cmd);

    // Merge env: permission baseline → caller-supplied (caller wins on conflicts).
    let mut extra_env: Vec<(String, String)> = perm_env;
    if let Some(caller_env) = args["env"].as_object() {
        for (k, v) in caller_env {
            if let Some(s) = v.as_str() {
                // Caller overrides; remove any perm baseline entry for this key first.
                extra_env.retain(|(ek, _)| ek != k);
                extra_env.push((k.clone(), s.to_owned()));
            }
        }
    }

    // Auto-recovery retry loop: up to 3 attempts on recoverable failures.
    // On each failure, classify_error() decides what to do next.
    const MAX_RETRIES: usize = 3;
    let mut current_cmd = cmd_rewritten.clone();
    let mut attempt = 0usize;
    let mut recovery_log: Vec<serde_json::Value> = Vec::new();

    let result = loop {
        let title = format!(
            "ferrite | exec: {}",
            &current_cmd[..current_cmd.len().min(40)]
        );
        let raw = run_observed(
            &current_cmd,
            &cwd,
            &extra_env,
            &stdin_data,
            Duration::from_secs(timeout_secs),
            &title,
            &config,
        );

        let exit_code = raw["exit_code"].as_i64().unwrap_or(-1);
        if exit_code == 0 || attempt >= MAX_RETRIES {
            // Attach recovery log to result when non-trivial.
            let mut out = raw;
            if !recovery_log.is_empty() {
                out["auto_recovery"] = serde_json::Value::Array(recovery_log.clone());
            }
            break out;
        }

        let stdout = raw["stdout"].as_str().unwrap_or("");
        let stderr = raw["stderr"].as_str().unwrap_or("");

        match crate::permissions::classify_error(&current_cmd, stdout, stderr) {
            Some(crate::permissions::RecoveryAction::RetryAfterDelay { secs, reason }) => {
                recovery_log.push(serde_json::json!({
                    "attempt": attempt + 1,
                    "action":  "retry_after_delay",
                    "delay_secs": secs,
                    "reason":  reason,
                }));
                std::thread::sleep(Duration::from_secs(secs));
            }
            Some(crate::permissions::RecoveryAction::RetryWithFlag { flag, reason }) => {
                let new_cmd = crate::permissions::apply_flag(&current_cmd, &flag);
                recovery_log.push(serde_json::json!({
                    "attempt": attempt + 1,
                    "action":  "retry_with_flag",
                    "flag":    flag,
                    "reason":  reason,
                    "new_cmd": new_cmd,
                }));
                current_cmd = new_cmd;
            }
            Some(crate::permissions::RecoveryAction::RetryWithSudo { reason }) => {
                let new_cmd = format!("sudo {current_cmd}");
                recovery_log.push(serde_json::json!({
                    "attempt": attempt + 1,
                    "action":  "retry_with_sudo",
                    "reason":  reason,
                    "new_cmd": new_cmd,
                }));
                current_cmd = new_cmd;
            }
            Some(crate::permissions::RecoveryAction::Unrecoverable { reason }) => {
                let mut out = raw;
                out["recovery_note"] = serde_json::Value::String(reason);
                if !recovery_log.is_empty() {
                    out["auto_recovery"] = serde_json::Value::Array(recovery_log.clone());
                }
                break out;
            }
            None => {
                let mut out = raw;
                if !recovery_log.is_empty() {
                    out["auto_recovery"] = serde_json::Value::Array(recovery_log.clone());
                }
                break out;
            }
        }

        attempt += 1;
    };

    Ok(ToolResult::json(&result))
}

/// Core command runner — called by build helpers (no observer).
pub fn run(
    cmd: &str,
    cwd: &Path,
    extra_env: &[(String, String)],
    stdin_data: &str,
    timeout: Duration,
) -> Value {
    run_observed(
        cmd,
        cwd,
        extra_env,
        stdin_data,
        timeout,
        "",
        &Default::default(),
    )
}

/// Run a command, optionally opening a terminal observer window.
///
/// When `config.terminal_always()` is true, output is teed to a temp log
/// file and a terminal window is opened showing `tail -f` of that log
/// before we block waiting for the child to finish.
fn run_observed(
    cmd: &str,
    cwd: &Path,
    extra_env: &[(String, String)],
    stdin_data: &str,
    timeout: Duration,
    window_title: &str,
    config: &crate::config::FerriteConfig,
) -> Value {
    use std::process::{Command, Stdio};

    let start = Instant::now();

    let mut child = match Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(if stdin_data.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "success": false,
                "error": format!("failed to spawn: {e}"),
                "cmd": cmd,
            })
        }
    };

    // Write stdin if provided
    if !stdin_data.is_empty() {
        if let Some(mut stdin_pipe) = child.stdin.take() {
            let _ = stdin_pipe.write_all(stdin_data.as_bytes());
        }
    }

    // Optionally tee output to a temp log file for the terminal observer.
    let log_path: Option<PathBuf> = if config.terminal_always() && !window_title.is_empty() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let p = std::env::temp_dir().join(format!("ferrite_exec_{ts}.log"));
        // Pre-create so tail -f --retry can find it immediately
        let _ = std::fs::File::create(&p);
        Some(p)
    } else {
        None
    };

    // Capture stdout/stderr via threads, optionally tee-ing to the log file.
    let stdout_handle = child.stdout.take().map(|stream| {
        let log = log_path.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut reader = std::io::BufReader::new(stream);
            let mut tmp = [0u8; 4096];
            let mut log_file = log
                .as_ref()
                .and_then(|p| std::fs::OpenOptions::new().append(true).open(p).ok());
            loop {
                match reader.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(ref mut f) = log_file {
                            let _ = f.write_all(&tmp[..n]);
                        }
                    }
                }
            }
            buf
        })
    });

    let stderr_handle = child.stderr.take().map(|stream| {
        let log = log_path.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut reader = std::io::BufReader::new(stream);
            let mut tmp = [0u8; 4096];
            // stderr also appends to same log
            let mut log_file = log
                .as_ref()
                .and_then(|p| std::fs::OpenOptions::new().append(true).open(p).ok());
            loop {
                match reader.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(ref mut f) = log_file {
                            let _ = f.write_all(&tmp[..n]);
                        }
                    }
                }
            }
            buf
        })
    });

    // Open terminal observer window now that the log file exists and the
    // reader threads are running.
    if let Some(ref log) = log_path {
        let tail_cmd = terminal::colorized_watch_cmd(log, config.terminal.keep_open);
        let _ = terminal::launch_terminal(window_title, &tail_cmd, &config.terminal.emulator);
    }

    // Poll with timeout
    let deadline = Instant::now() + timeout;
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break None,
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    let timed_out = exit_status.is_none();

    let stdout = stdout_handle
        .and_then(|h| h.join().ok())
        .map(|b| strip_ansi(String::from_utf8_lossy(&b).into_owned()))
        .unwrap_or_default();

    let stderr = stderr_handle
        .and_then(|h| h.join().ok())
        .map(|b| strip_ansi(String::from_utf8_lossy(&b).into_owned()))
        .unwrap_or_default();

    let exit_code = exit_status.and_then(|s| s.code()).unwrap_or(-1);

    // Signal the terminal observer: awk watches for this line and shows
    // a coloured final status then exits, closing the pipeline cleanly.
    if let Some(ref log) = log_path {
        let marker = format!("FERRITE_DONE:{exit_code}\n");
        let _ = std::fs::OpenOptions::new()
            .append(true)
            .open(log)
            .and_then(|mut f| f.write_all(marker.as_bytes()));
    }

    // Sidecar PID file path — only present when an observer window was launched.
    let obs = log_path.as_ref().map(|l| format!("{}.pid", l.display()));

    // Compact format for clean success (no stderr, exit 0, no timeout).
    if exit_code == 0 && stderr.is_empty() && !timed_out {
        return match obs {
            Some(pid_file) => json!({ "ok": true, "ms": duration_ms, "out": stdout,
                                      "observer_pid_file": pid_file }),
            None => json!({ "ok": true, "ms": duration_ms, "out": stdout }),
        };
    }

    let mut result = json!({
        "cmd": cmd,
        "cwd": cwd.display().to_string(),
        "success": exit_code == 0,
        "exit_code": exit_code,
        "timed_out": timed_out,
        "duration_ms": duration_ms,
        "stdout": stdout,
        "stderr": stderr,
    });
    if let Some(pid_file) = obs {
        result["observer_pid_file"] = json!(pid_file);
    }
    result
}

// ── build_check ───────────────────────────────────────────────────────────────

pub fn build_check(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let file = args["file"]
        .as_str()
        .ok_or("build_check: 'file' is required")?;
    let extra_flags: Vec<String> = args["flags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    let state_cwd = read_cwd(state);
    let config = crate::config::FerriteConfig::load();

    // Resolve file path relative to cwd
    let file_path = if Path::new(file).is_absolute() {
        PathBuf::from(file)
    } else {
        state_cwd.join(file)
    };

    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(file);
    let title = format!("ferrite | build: {filename}");

    // Detect build type from file/directory
    let build_type = match args["type"].as_str().unwrap_or("auto") {
        "auto" => detect_build_type(&file_path),
        t => t.to_owned(),
    };

    let result = match build_type.as_str() {
        "cuda" => build_cuda(&file_path, &extra_flags, &state_cwd, &title, &config),
        "rust" => build_rust(&file_path, &extra_flags, &state_cwd, &title, &config),
        "c" => build_c(&file_path, &extra_flags, "gcc", &state_cwd, &title, &config),
        "cpp" => build_c(&file_path, &extra_flags, "g++", &state_cwd, &title, &config),
        other => return Ok(ToolResult::error(format!("unknown build type: {other}"))),
    };

    Ok(ToolResult::json(&result))
}

fn detect_build_type(path: &Path) -> String {
    if path.is_dir() {
        if path.join("Cargo.toml").exists() {
            return "rust".to_owned();
        }
        if path.join("CMakeLists.txt").exists() {
            return "cmake".to_owned();
        }
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("cu") => "cuda",
        Some("rs") => "rust",
        Some("c") => "c",
        Some("cc") | Some("cpp") | Some("cxx") => "cpp",
        _ => "unknown",
    }
    .to_owned()
}

// ── CUDA build ────────────────────────────────────────────────────────────────

fn build_cuda(
    file: &Path,
    extra_flags: &[String],
    cwd: &Path,
    title: &str,
    config: &crate::config::FerriteConfig,
) -> Value {
    let nvcc = match which_bin("nvcc") {
        Some(p) => p,
        None => {
            return json!({
                "success": false,
                "error": "nvcc not found — is CUDA installed and in PATH?",
            })
        }
    };

    let source_file = match resolve_cuda_source(file) {
        Ok(path) => path,
        Err((message, candidates)) => {
            return json!({
                "success": false,
                "build_type": "cuda",
                "compiler": nvcc,
                "error": message,
                "candidate_count": candidates.len(),
                "candidates": candidates,
            })
        }
    };

    // Detect compute capability from cached gpu_info
    let arch_flags = detect_cuda_arch_flags();

    let out_path = std::env::temp_dir().join("ferrite_build_check.o");

    let mut cmd_parts = vec![
        nvcc.clone(),
        "-c".to_owned(), // compile only, no link
        "-o".to_owned(),
        out_path.display().to_string(),
    ];

    cmd_parts.extend(arch_flags.clone());

    if let Some(inc) = cuda_include_dir() {
        cmd_parts.push(format!("-I{inc}"));
    }

    cmd_parts.extend(extra_flags.iter().cloned());
    cmd_parts.push(source_file.display().to_string());

    let cmd = cmd_parts.join(" ");
    let raw = run_observed(&cmd, cwd, &[], "", Duration::from_secs(120), title, config);

    let stderr = raw["stderr"].as_str().unwrap_or("").to_owned();
    let stdout = raw["stdout"].as_str().unwrap_or("").to_owned();
    let success = raw["success"].as_bool().unwrap_or(false);

    let _ = std::fs::remove_file(&out_path);

    let (errors, warnings) = parse_nvcc_output(&stderr);

    json!({
        "success": success,
        "build_type": "cuda",
        "compiler": nvcc,
        "arch_flags": arch_flags,
        "command": cmd,
        "errors": errors,
        "warnings": warnings,
        "error_count": errors.len(),
        "warning_count": warnings.len(),
        "duration_ms": raw["duration_ms"],
        "raw_stderr": if success { Value::Null } else { Value::String(stderr) },
        "raw_stdout": if stdout.is_empty() { Value::Null } else { Value::String(stdout) },
    })
}

fn resolve_cuda_source(path: &Path) -> Result<PathBuf, (String, Vec<String>)> {
    if path.is_file() {
        if path.extension().and_then(|e| e.to_str()) == Some("cu") {
            return Ok(path.to_owned());
        }
        return Err((
            format!(
                "CUDA build_check expects a .cu file, got '{}'",
                path.display()
            ),
            Vec::new(),
        ));
    }

    if !path.is_dir() {
        return Err((
            format!("CUDA input '{}' does not exist", path.display()),
            Vec::new(),
        ));
    }

    let mut candidates = Vec::new();
    collect_cuda_sources(path, &mut candidates);
    candidates.sort();

    match candidates.len() {
        0 => Err((
            format!("No .cu files found under '{}'", path.display()),
            Vec::new(),
        )),
        1 => Ok(PathBuf::from(&candidates[0])),
        _ => Err((
            format!(
                "CUDA directory '{}' is ambiguous; found {} .cu files. Pass a specific file.",
                path.display(),
                candidates.len()
            ),
            candidates.into_iter().take(20).collect(),
        )),
    }
}

fn collect_cuda_sources(dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_cuda_sources(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("cu") {
            out.push(path.display().to_string());
        }
    }
}

fn detect_cuda_arch_flags() -> Vec<String> {
    // Try to read from cached gpu_info
    // We reproduce a minimal query here to keep tools decoupled
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=compute_cap", "--format=csv,noheader"])
        .output();

    if let Ok(o) = out {
        if o.status.success() {
            // Take the first GPU's compute cap
            if let Some(cap) = String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .map(|l| l.trim().replace('.', ""))
            {
                if !cap.is_empty() {
                    return vec![
                        format!("-arch=compute_{cap}"),
                        format!("-code=sm_{cap},compute_{cap}"),
                    ];
                }
            }
        }
    }

    // Conservative fallback — runs on anything Pascal+
    vec![
        "-arch=compute_60".to_owned(),
        "-code=sm_60,compute_60".to_owned(),
    ]
}

/// Parse nvcc diagnostic output into structured error/warning lists.
///
/// nvcc format: `path/to/file.cu(line,col): severity: message`
fn parse_nvcc_output(output: &str) -> (Vec<Value>, Vec<Value>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    for line in output.lines() {
        // Match: file(line,col): error/warning: message
        //    or: file(line): error/warning: message
        if let Some(diag) = try_parse_nvcc_line(line) {
            let sev = diag["severity"].as_str().unwrap_or("");
            if sev == "error" || sev == "fatal error" {
                errors.push(diag);
            } else if sev == "warning" || sev == "note" {
                warnings.push(diag);
            }
        }
    }

    (errors, warnings)
}

fn try_parse_nvcc_line(line: &str) -> Option<Value> {
    // Pattern: something(N,M): severity: message
    //      or: something(N): severity: message
    let paren = line.find('(')?;
    let close = line[paren..].find(')')?;
    let file = line[..paren].trim();
    let loc = &line[paren + 1..paren + close];

    // Parse line[,col]
    let (ln, col) = if let Some(comma) = loc.find(',') {
        (
            loc[..comma].parse::<u32>().ok(),
            loc[comma + 1..].parse::<u32>().ok(),
        )
    } else {
        (loc.parse::<u32>().ok(), None)
    };

    let rest = line[paren + close + 1..].trim(); // ": severity: message"
    let rest = rest.strip_prefix(':')?;

    let colon = rest.find(':')?;
    let severity = rest[..colon].trim().to_owned();
    let message = rest[colon + 1..].trim().to_owned();

    Some(json!({
        "file": file,
        "line": ln,
        "col":  col,
        "severity": severity,
        "message": message,
    }))
}

// ── Rust / Cargo build ────────────────────────────────────────────────────────

fn build_rust(
    file: &Path,
    extra_flags: &[String],
    cwd: &Path,
    title: &str,
    config: &crate::config::FerriteConfig,
) -> Value {
    // If file is a directory or Cargo.toml, run cargo check from there.
    // If it's a .rs file, cargo check the parent package.
    let cargo_dir = if file.is_dir() {
        file.to_owned()
    } else if file.file_name().map(|n| n == "Cargo.toml").unwrap_or(false) {
        file.parent().unwrap_or(cwd).to_owned()
    } else {
        // Find containing Cargo.toml by walking up
        find_cargo_root(file).unwrap_or_else(|| cwd.to_owned())
    };

    let mut flags = vec!["--message-format=json".to_owned()];
    flags.extend(extra_flags.iter().cloned());

    let cmd = format!("cargo check {}", flags.join(" "));
    let raw = run_observed(
        &cmd,
        &cargo_dir,
        &[],
        "",
        Duration::from_secs(180),
        title,
        config,
    );

    let stderr = raw["stderr"].as_str().unwrap_or("").to_owned();
    let stdout = raw["stdout"].as_str().unwrap_or("").to_owned();
    let success = raw["success"].as_bool().unwrap_or(false);

    let (errors, warnings) = parse_cargo_json_output(&stdout);

    json!({
        "success": success,
        "build_type": "rust",
        "compiler": "cargo check",
        "command": format!("cd {} && {}", cargo_dir.display(), cmd),
        "errors": errors,
        "warnings": warnings,
        "error_count": errors.len(),
        "warning_count": warnings.len(),
        "duration_ms": raw["duration_ms"],
        "raw_stderr": if success { Value::Null } else { Value::String(stderr) },
    })
}

/// Parse `cargo check --message-format=json` stdout.
fn parse_cargo_json_output(stdout: &str) -> (Vec<Value>, Vec<Value>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if msg["reason"] != "compiler-message" {
            continue;
        }

        let m = &msg["message"];
        let level = m["level"].as_str().unwrap_or("");
        let text = m["message"].as_str().unwrap_or("").to_owned();
        let code = m["code"]["code"].as_str().map(str::to_owned);

        // Collect primary span
        let span = m["spans"].as_array().and_then(|spans| {
            spans
                .iter()
                .find(|s| s["is_primary"].as_bool().unwrap_or(false))
        });

        let diag = json!({
            "severity": level,
            "message":  text,
            "code":     code,
            "file":     span.and_then(|s| s["file_name"].as_str()),
            "line":     span.and_then(|s| s["line_start"].as_u64()),
            "col":      span.and_then(|s| s["column_start"].as_u64()),
            "rendered": m["rendered"].as_str(),
        });

        if level == "error" {
            errors.push(diag);
        } else if level == "warning" {
            warnings.push(diag);
        }
    }

    (errors, warnings)
}

fn find_cargo_root(from: &Path) -> Option<PathBuf> {
    let mut dir = if from.is_file() { from.parent()? } else { from };
    loop {
        if dir.join("Cargo.toml").exists() {
            return Some(dir.to_owned());
        }
        dir = dir.parent()?;
    }
}

// ── C / C++ build ─────────────────────────────────────────────────────────────

fn build_c(
    file: &Path,
    extra_flags: &[String],
    compiler: &str,
    cwd: &Path,
    title: &str,
    config: &crate::config::FerriteConfig,
) -> Value {
    let out = std::env::temp_dir().join("ferrite_build_check.o");
    let mut parts = vec![
        compiler.to_owned(),
        "-c".to_owned(),
        "-Wall".to_owned(),
        "-Wextra".to_owned(),
        "-o".to_owned(),
        out.display().to_string(),
    ];
    parts.extend(extra_flags.iter().cloned());
    parts.push(file.display().to_string());

    let cmd = parts.join(" ");
    let raw = run_observed(&cmd, cwd, &[], "", Duration::from_secs(60), title, config);

    let stderr = raw["stderr"].as_str().unwrap_or("").to_owned();
    let success = raw["success"].as_bool().unwrap_or(false);

    let _ = std::fs::remove_file(&out);

    let (errors, warnings) = parse_gcc_output(&stderr);

    json!({
        "success": success,
        "build_type": compiler,
        "command": cmd,
        "errors": errors,
        "warnings": warnings,
        "error_count": errors.len(),
        "warning_count": warnings.len(),
        "duration_ms": raw["duration_ms"],
        "raw_stderr": if success { Value::Null } else { Value::String(stderr) },
    })
}

/// Parse GCC/Clang diagnostic output.
/// Format: `file.c:line:col: severity: message`
fn parse_gcc_output(output: &str) -> (Vec<Value>, Vec<Value>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    for line in output.lines() {
        let parts: Vec<&str> = line.splitn(5, ':').collect();
        if parts.len() < 5 {
            continue;
        }

        let file = parts[0].trim();
        let ln = parts[1].trim().parse::<u32>().ok();
        let col = parts[2].trim().parse::<u32>().ok();
        let severity = parts[3].trim();
        let message = parts[4].trim();

        if !matches!(severity, "error" | "warning" | "note" | "fatal error") {
            continue;
        }

        let diag = json!({
            "file": file,
            "line": ln,
            "col":  col,
            "severity": severity,
            "message": message,
        });

        if severity == "error" || severity == "fatal error" {
            errors.push(diag);
        } else {
            warnings.push(diag);
        }
    }

    (errors, warnings)
}

// ── task_run ──────────────────────────────────────────────────────────────────

/// Write `script` to a tempfile, execute it with `interpreter`, return results.
/// This lets Claude define an entire multi-step workflow as one atomic tool call,
/// avoiding per-step round-trips for long hardware experiments or data sweeps.
pub fn task_run(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let script = args["script"]
        .as_str()
        .ok_or("task_run: 'script' is required")?;
    let interpreter = args["interpreter"].as_str().unwrap_or("python3");
    let timeout = args["timeout_secs"].as_u64().unwrap_or(120);

    let state_cwd = read_cwd(state);
    let config = crate::config::FerriteConfig::load();
    let cwd = args["cwd"].as_str().map(PathBuf::from).unwrap_or(state_cwd);

    // Whitelist interpreters — no arbitrary binary execution via this field.
    let allowed = ["python3", "python", "bash", "sh"];
    if !allowed.contains(&interpreter) {
        return Err(format!(
            "task_run: interpreter '{interpreter}' not allowed; use one of: {}",
            allowed.join(", ")
        ));
    }

    let ext = if interpreter.starts_with("python") {
        "py"
    } else {
        "sh"
    };

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let script_path = std::env::temp_dir().join(format!("ferrite_task_{ts}.{ext}"));

    std::fs::write(&script_path, script).map_err(|e| format!("task_run: write tempfile: {e}"))?;

    let cmd = format!("{interpreter} {}", script_path.display());
    let title = format!("ferrite | task: {interpreter}");
    let result = run_observed(
        &cmd,
        &cwd,
        &[],
        "",
        Duration::from_secs(timeout),
        &title,
        &config,
    );

    let _ = std::fs::remove_file(&script_path);

    Ok(ToolResult::json(&result))
}

// ── launch ────────────────────────────────────────────────────────────────────

/// Fire-and-forget process launch. Spawns detached, returns PID immediately.
/// Use for GUI apps, long-running daemons, anything where the result doesn't
/// need to be reasoned about (Vivado, waveform viewers, terminals, etc.).
pub fn launch(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    use std::process::{Command, Stdio};

    let cmd = args["cmd"].as_str().ok_or("launch: 'cmd' is required")?;

    let state_cwd = read_cwd(state);
    let cwd = args["cwd"].as_str().map(PathBuf::from).unwrap_or(state_cwd);

    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("launch: failed to spawn: {e}"))?;

    let pid = child.id();
    // Detach — drop child handle without waiting
    std::mem::drop(child);

    Ok(ToolResult::json(&serde_json::json!({
        "pid": pid,
        "cmd": cmd,
        "cwd": cwd.display().to_string(),
    })))
}

// ── Terminal observer helpers ─────────────────────────────────────────────────

/// Build the shell command that runs inside the observer terminal window.
///
/// Pipes `tail -f` through an awk colorizer. Awk watches for the
/// `FERRITE_DONE:{code}` sentinel line written by run_observed after the
/// process exits, prints a coloured status banner, then exits — which
/// also terminates the tail pipeline via SIGPIPE.
// ── Shared helpers ─────────────────────────────────────────────────────────────

/// Strip ANSI escape sequences from a string.
/// Removes CSI sequences (ESC [ ... m), OSC sequences, and bare ESC chars.
fn strip_ansi(s: String) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i < bytes.len() {
                match bytes[i] {
                    b'[' => {
                        // CSI: ESC [ ... (final byte in 0x40–0x7E)
                        i += 1;
                        while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                            i += 1;
                        }
                        i += 1; // skip final byte
                    }
                    b']' => {
                        // OSC: ESC ] ... ST (BEL or ESC \)
                        i += 1;
                        while i < bytes.len() && bytes[i] != 0x07 {
                            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                                i += 2;
                                break;
                            }
                            i += 1;
                        }
                        if i < bytes.len() {
                            i += 1;
                        }
                    }
                    _ => {
                        i += 1;
                    } // skip whatever follows ESC
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn which_bin(name: &str) -> Option<String> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    path_var
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| std::path::PathBuf::from(d).join(name))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .map(|p| p.display().to_string())
}

// ── close_observer ────────────────────────────────────────────────────────────

/// Kill the shell process running inside an observer terminal window.
///
/// Sends SIGTERM to the PID stored in the sidecar `.pid` file that
/// `colorized_watch_cmd` writes at startup. Killing that shell kills the
/// entire `tail | awk` pipeline and causes the terminal window to close.
///
/// `pid_file`: explicit path (e.g. from exec result's `observer_pid_file`).
/// If omitted, the most-recently-created `/tmp/ferrite_exec_*.pid` is used.
pub fn close_observer(args: &Value) -> Result<ToolResult, String> {
    let pid_file = match args["pid_file"].as_str() {
        Some(f) => f.to_owned(),
        None => find_newest_pid_file()?,
    };

    let text = std::fs::read_to_string(&pid_file)
        .map_err(|e| format!("close_observer: read '{pid_file}': {e}"))?;

    let pid: i32 = text
        .trim()
        .parse()
        .map_err(|_| format!("close_observer: invalid PID '{}'", text.trim()))?;

    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    let _ = std::fs::remove_file(&pid_file);

    if rc == 0 {
        Ok(ToolResult::json(&json!({
            "ok": true,
            "closed_pid": pid,
            "pid_file": pid_file,
        })))
    } else {
        Ok(ToolResult::json(&json!({
            "ok": false,
            "error": format!("kill({pid}, SIGTERM) returned {rc} — process may have already exited"),
            "pid_file": pid_file,
        })))
    }
}

fn find_newest_pid_file() -> Result<String, String> {
    let tmp = std::env::temp_dir();
    let mut entries: Vec<_> = std::fs::read_dir(&tmp)
        .map_err(|e| format!("close_observer: read {}: {e}", tmp.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.starts_with("ferrite_exec_") && s.ends_with(".pid")
        })
        .collect();

    if entries.is_empty() {
        return Err(
            "close_observer: no active observer found — no /tmp/ferrite_exec_*.pid files"
                .to_string(),
        );
    }

    entries.sort_by_key(|e| {
        e.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    Ok(entries.last().unwrap().path().display().to_string())
}

fn cuda_include_dir() -> Option<String> {
    for var in ["CUDA_HOME", "CUDA_PATH", "CUDA_ROOT"] {
        if let Ok(v) = std::env::var(var) {
            let inc = format!("{v}/include");
            if Path::new(&inc).exists() {
                return Some(inc);
            }
        }
    }
    for p in ["/usr/local/cuda/include", "/usr/include/cuda"] {
        if Path::new(p).exists() {
            return Some(p.to_owned());
        }
    }
    None
}
