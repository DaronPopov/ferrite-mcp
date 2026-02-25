//! exec, build_check, and task_run tool implementations.
//!
//! exec:        Run any command. Returns stdout/stderr/exit/duration as structured JSON.
//! build_check: Smart compilation — auto-resolves CUDA flags, returns parsed error list.
//! task_run:    Write a script to a tempfile and execute it atomically in one call.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;

// ── exec ──────────────────────────────────────────────────────────────────────

pub fn exec_cmd(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let cmd = args["cmd"].as_str().ok_or("exec: 'cmd' is required")?;

    let state_cwd = state.lock()
        .map_err(|e| format!("state lock poisoned: {e}"))?
        .cwd.clone();

    let cwd = args["cwd"].as_str()
        .map(PathBuf::from)
        .unwrap_or(state_cwd);

    let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(60);
    let stdin_data   = args["stdin"].as_str().unwrap_or("").to_owned();

    // Merge any caller-supplied env vars on top of the inherited environment
    let extra_env: Vec<(String, String)> = args["env"]
        .as_object()
        .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned()))).collect())
        .unwrap_or_default();

    let result = run(cmd, &cwd, &extra_env, &stdin_data, Duration::from_secs(timeout_secs));
    Ok(ToolResult::json(&result))
}

/// Core command runner — called by both exec and build_check.
pub fn run(
    cmd: &str,
    cwd: &Path,
    extra_env: &[(String, String)],
    stdin_data: &str,
    timeout: Duration,
) -> Value {
    use std::process::{Command, Stdio};

    let start = Instant::now();

    let mut child = match Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(if stdin_data.is_empty() { Stdio::null() } else { Stdio::piped() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return json!({
            "success": false,
            "error": format!("failed to spawn: {e}"),
            "cmd": cmd,
        }),
    };

    // Write stdin if provided
    if !stdin_data.is_empty() {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(stdin_data.as_bytes());
        }
    }

    // Capture stdout/stderr via threads (avoids deadlock on large output)
    let mut stdout_reader = child.stdout.take().map(|s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::BufReader::new(s).read_to_end(&mut buf);
            buf
        })
    });
    let mut stderr_reader = child.stderr.take().map(|s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::BufReader::new(s).read_to_end(&mut buf);
            buf
        })
    });

    // Poll with timeout
    let deadline = Instant::now() + timeout;
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    break None; // timed out
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break None,
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    let timed_out = exit_status.is_none();

    let stdout = stdout_reader
        .take()
        .and_then(|h| h.join().ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();

    let stderr = stderr_reader
        .take()
        .and_then(|h| h.join().ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();

    let exit_code = exit_status
        .and_then(|s| s.code())
        .unwrap_or(-1);

    json!({
        "cmd": cmd,
        "cwd": cwd.display().to_string(),
        "success": exit_code == 0,
        "exit_code": exit_code,
        "timed_out": timed_out,
        "duration_ms": duration_ms,
        "stdout": stdout,
        "stderr": stderr,
    })
}

// ── build_check ───────────────────────────────────────────────────────────────

pub fn build_check(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let file = args["file"].as_str().ok_or("build_check: 'file' is required")?;
    let extra_flags: Vec<String> = args["flags"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();

    let state_cwd = state.lock()
        .map_err(|e| format!("state lock poisoned: {e}"))?
        .cwd.clone();

    // Resolve file path relative to cwd
    let file_path = if Path::new(file).is_absolute() {
        PathBuf::from(file)
    } else {
        state_cwd.join(file)
    };

    // Detect build type from file/directory
    let build_type = match args["type"].as_str().unwrap_or("auto") {
        "auto" => detect_build_type(&file_path),
        t      => t.to_owned(),
    };

    let result = match build_type.as_str() {
        "cuda"  => build_cuda(&file_path, &extra_flags, &state_cwd),
        "rust"  => build_rust(&file_path, &extra_flags, &state_cwd),
        "c"     => build_c(&file_path, &extra_flags, "gcc", &state_cwd),
        "cpp"   => build_c(&file_path, &extra_flags, "g++", &state_cwd),
        other   => return Ok(ToolResult::error(format!("unknown build type: {other}"))),
    };

    Ok(ToolResult::json(&result))
}

fn detect_build_type(path: &Path) -> String {
    if path.is_dir() {
        if path.join("Cargo.toml").exists() { return "rust".to_owned(); }
        if path.join("CMakeLists.txt").exists() { return "cmake".to_owned(); }
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("cu")                        => "cuda",
        Some("rs")                        => "rust",
        Some("c")                         => "c",
        Some("cc") | Some("cpp") | Some("cxx") => "cpp",
        _                                 => "unknown",
    }.to_owned()
}

// ── CUDA build ────────────────────────────────────────────────────────────────

fn build_cuda(file: &Path, extra_flags: &[String], cwd: &Path) -> Value {
    let nvcc = match which_bin("nvcc") {
        Some(p) => p,
        None => return json!({
            "success": false,
            "error": "nvcc not found — is CUDA installed and in PATH?",
        }),
    };

    // Detect compute capability from cached gpu_info
    let arch_flags = detect_cuda_arch_flags();

    let out_path = std::env::temp_dir().join("cream_build_check.o");

    let mut cmd_parts = vec![
        nvcc.clone(),
        "-c".to_owned(),                        // compile only, no link
        "-o".to_owned(),
        out_path.display().to_string(),
    ];

    cmd_parts.extend(arch_flags.clone());

    if let Some(inc) = cuda_include_dir() {
        cmd_parts.push(format!("-I{inc}"));
    }

    cmd_parts.extend(extra_flags.iter().cloned());
    cmd_parts.push(file.display().to_string());

    let cmd = cmd_parts.join(" ");
    let raw = run(&cmd, cwd, &[], "", Duration::from_secs(120));

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
                        format!("-arch=sm_{cap}"),
                        format!("-code=sm_{cap},compute_{cap}"),
                    ];
                }
            }
        }
    }

    // Conservative fallback — runs on anything Pascal+
    vec!["-arch=sm_60".to_owned(), "-code=sm_60,compute_60".to_owned()]
}

/// Parse nvcc diagnostic output into structured error/warning lists.
///
/// nvcc format: `path/to/file.cu(line,col): severity: message`
fn parse_nvcc_output(output: &str) -> (Vec<Value>, Vec<Value>) {
    let mut errors   = Vec::new();
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
    let loc  = &line[paren + 1..paren + close];

    // Parse line[,col]
    let (ln, col) = if let Some(comma) = loc.find(',') {
        (loc[..comma].parse::<u32>().ok(), loc[comma+1..].parse::<u32>().ok())
    } else {
        (loc.parse::<u32>().ok(), None)
    };

    let rest = line[paren + close + 1..].trim();  // ": severity: message"
    let rest = rest.strip_prefix(':')?;

    let colon = rest.find(':')?;
    let severity = rest[..colon].trim().to_owned();
    let message  = rest[colon+1..].trim().to_owned();

    Some(json!({
        "file": file,
        "line": ln,
        "col":  col,
        "severity": severity,
        "message": message,
    }))
}

// ── Rust / Cargo build ────────────────────────────────────────────────────────

fn build_rust(file: &Path, extra_flags: &[String], cwd: &Path) -> Value {
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
    let raw = run(&cmd, &cargo_dir, &[], "", Duration::from_secs(180));

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
    let mut errors   = Vec::new();
    let mut warnings = Vec::new();

    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<Value>(line) else { continue };
        if msg["reason"] != "compiler-message" { continue; }

        let m = &msg["message"];
        let level = m["level"].as_str().unwrap_or("");
        let text  = m["message"].as_str().unwrap_or("").to_owned();
        let code  = m["code"]["code"].as_str().map(str::to_owned);

        // Collect primary span
        let span = m["spans"].as_array()
            .and_then(|spans| spans.iter().find(|s| s["is_primary"].as_bool().unwrap_or(false)));

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
        if dir.join("Cargo.toml").exists() { return Some(dir.to_owned()); }
        dir = dir.parent()?;
    }
}

// ── C / C++ build ─────────────────────────────────────────────────────────────

fn build_c(file: &Path, extra_flags: &[String], compiler: &str, cwd: &Path) -> Value {
    let out = std::env::temp_dir().join("cream_build_check.o");
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
    let raw = run(&cmd, cwd, &[], "", Duration::from_secs(60));

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
    let mut errors   = Vec::new();
    let mut warnings = Vec::new();

    for line in output.lines() {
        let parts: Vec<&str> = line.splitn(5, ':').collect();
        if parts.len() < 5 { continue; }

        let file     = parts[0].trim();
        let ln       = parts[1].trim().parse::<u32>().ok();
        let col      = parts[2].trim().parse::<u32>().ok();
        let severity = parts[3].trim();
        let message  = parts[4].trim();

        if !matches!(severity, "error" | "warning" | "note" | "fatal error") { continue; }

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
    let script      = args["script"].as_str().ok_or("task_run: 'script' is required")?;
    let interpreter = args["interpreter"].as_str().unwrap_or("python3");
    let timeout     = args["timeout_secs"].as_u64().unwrap_or(120);

    let state_cwd = state.lock()
        .map_err(|e| format!("state lock poisoned: {e}"))?
        .cwd.clone();
    let cwd = args["cwd"].as_str().map(PathBuf::from).unwrap_or(state_cwd);

    // Whitelist interpreters — no arbitrary binary execution via this field.
    let allowed = ["python3", "python", "bash", "sh"];
    if !allowed.contains(&interpreter) {
        return Err(format!(
            "task_run: interpreter '{interpreter}' not allowed; use one of: {}",
            allowed.join(", ")
        ));
    }

    let ext = if interpreter.starts_with("python") { "py" } else { "sh" };

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let script_path = std::env::temp_dir().join(format!("cream_task_{ts}.{ext}"));

    std::fs::write(&script_path, script)
        .map_err(|e| format!("task_run: write tempfile: {e}"))?;

    let cmd = format!("{interpreter} {}", script_path.display());
    let result = run(&cmd, &cwd, &[], "", Duration::from_secs(timeout));

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

    let state_cwd = state.lock()
        .map_err(|e| format!("state lock poisoned: {e}"))?
        .cwd.clone();
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

// ── Shared helpers ─────────────────────────────────────────────────────────────

fn which_bin(name: &str) -> Option<String> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    path_var.split(':').filter(|d| !d.is_empty()).map(|d| {
        std::path::PathBuf::from(d).join(name)
    }).find(|p| {
        use std::os::unix::fs::PermissionsExt;
        p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }).map(|p| p.display().to_string())
}

fn cuda_include_dir() -> Option<String> {
    for var in ["CUDA_HOME", "CUDA_PATH", "CUDA_ROOT"] {
        if let Ok(v) = std::env::var(var) {
            let inc = format!("{v}/include");
            if Path::new(&inc).exists() { return Some(inc); }
        }
    }
    for p in ["/usr/local/cuda/include", "/usr/include/cuda"] {
        if Path::new(p).exists() { return Some(p.to_owned()); }
    }
    None
}
