//! Profiling tools: ncu_profile, ptx_inspect, compute_sanitizer.
//!
//! ncu_profile      — run Nsight Compute CLI, return per-kernel metrics as structured JSON
//! ptx_inspect      — compile .cu to PTX (or dump from binary), parse register/smem usage
//! compute_sanitizer — run compute-sanitizer --tool memcheck, return structured errors

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::state::read_cwd;

// ── ncu_profile ───────────────────────────────────────────────────────────────

/// Key metric set — covers occupancy, bandwidth, cache efficiency, SM throughput.
/// These names work for CUDA 12.x / Nsight Compute 2023+.
const NCU_METRICS: &str = "sm__warps_active.avg.pct_of_peak_sustained_active,\
     sm__throughput.avg.pct_of_peak_sustained_elapsed,\
     dram__throughput.avg.pct_of_peak_sustained_elapsed,\
     l1tex__t_sector_hit_rate.pct,\
     lts__t_sector_hit_rate.pct,\
     smsp__warp_issue_stalled_long_scoreboard_per_warp_active.pct,\
     smsp__warp_issue_stalled_short_scoreboard_per_warp_active.pct";

pub fn ncu_profile(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let cmd_str = args["cmd"]
        .as_str()
        .ok_or("ncu_profile: 'cmd' is required")?;
    let kernel_filter = args["kernel"].as_str();
    let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(120);
    let metric_set = args["metrics"].as_str().unwrap_or(NCU_METRICS);

    let ncu = which_bin("ncu").ok_or_else(|| {
        "ncu not found in PATH. Install Nsight Compute from https://developer.nvidia.com/nsight-compute".to_owned()
    })?;

    let cwd = read_cwd(state);

    let mut ncu_args: Vec<String> = vec![
        "--csv".into(),
        "--metrics".into(),
        metric_set.to_owned(),
        "--target-processes".into(),
        "all".into(),
        "--print-summary".into(),
        "per-kernel".into(),
    ];

    if let Some(k) = kernel_filter {
        ncu_args.extend_from_slice(&["--kernel-name".into(), k.to_owned()]);
    }

    // Split the user command into binary + args
    let mut parts = cmd_str.split_whitespace();
    let bin = parts.next().ok_or("ncu_profile: empty cmd")?;
    ncu_args.push(bin.to_owned());
    ncu_args.extend(parts.map(str::to_owned));

    let (stdout, stderr, exit_code, duration_ms) =
        run_with_timeout(&ncu, &ncu_args, &cwd, timeout_secs)?;

    // ncu exits non-zero when profiling succeeds but there are warnings — handle gracefully
    let raw_csv = stdout.clone();
    let kernels = parse_ncu_csv(&stdout);

    // ncu writes ==ERROR== messages to stdout, not stderr
    let combined_out = format!("{stdout}\n{stderr}");
    let perm_hint = if combined_out.contains("ERR_NVGPUCTRPERM")
        || combined_out.contains("Permission denied")
    {
        Some("ncu requires elevated permissions on Linux. Fix with: echo 0 | sudo tee /proc/sys/kernel/perf_event_paranoid")
    } else {
        None
    };

    Ok(ToolResult::json(&json!({
        "cmd":          cmd_str,
        "exit_code":    exit_code,
        "duration_ms":  duration_ms,
        "kernel_count": kernels.len(),
        "kernels":      kernels,
        "stderr":       stderr,
        "raw_csv":      raw_csv,
        "permission_hint": perm_hint,
    })))
}

/// Parse ncu --csv output into per-kernel metric objects.
/// CSV columns: ID, Process ID, Process Name, Host Name, Kernel Name, Kernel Time,
///              Context, Stream, Metric Name, Metric Unit, Metric Value
fn parse_ncu_csv(csv: &str) -> Vec<Value> {
    let mut by_kernel: std::collections::HashMap<String, serde_json::Map<String, Value>> =
        std::collections::HashMap::new();

    // Skip header line(s) starting with "ID" or "=="
    for line in csv.lines() {
        if line.starts_with('"') || line.starts_with("ID") {
            // Try to parse data line
            let cols = split_csv_line(line);
            if cols.len() < 11 {
                continue;
            }

            // Skip header row
            if cols[0].trim_matches('"') == "ID" {
                continue;
            }

            let kernel_name = cols[4].trim_matches('"').to_owned();
            let metric_name = cols[8].trim_matches('"').to_owned();
            let metric_unit = cols[9].trim_matches('"').to_owned();
            let metric_val = cols[10].trim_matches('"').to_owned();

            if kernel_name.is_empty() || metric_name.is_empty() {
                continue;
            }

            let entry = by_kernel.entry(kernel_name.clone()).or_default();

            entry
                .entry("kernel_name".to_owned())
                .or_insert_with(|| Value::String(kernel_name));

            // Store as number if parseable, else string
            let val: Value = metric_val
                .parse::<f64>()
                .map(|f| json!(f))
                .unwrap_or_else(|_| Value::String(metric_val.clone()));

            // Use short metric alias for readability
            let key = metric_alias(&metric_name);
            entry.insert(key.clone(), json!({ "value": val, "unit": metric_unit }));

            // Also add a flat "achieved_occupancy_pct" etc for easy access
            if metric_name.contains("warps_active") {
                entry.insert("achieved_occupancy_pct".into(), val);
            } else if metric_name.contains("dram__throughput") {
                entry.insert("dram_throughput_pct".into(), val);
            } else if metric_name.contains("sm__throughput") {
                entry.insert("sm_throughput_pct".into(), val);
            } else if metric_name.contains("l1tex") {
                entry.insert("l1_hit_rate_pct".into(), val);
            } else if metric_name.contains("lts__t_sector") {
                entry.insert("l2_hit_rate_pct".into(), val);
            } else if metric_name.contains("long_scoreboard") {
                entry.insert("stall_long_sb_pct".into(), val);
            }
        }
    }

    by_kernel.into_values().map(Value::Object).collect()
}

fn metric_alias(name: &str) -> String {
    if name.contains("warps_active") {
        return "occupancy".into();
    }
    if name.contains("dram__throughput") {
        return "dram_throughput".into();
    }
    if name.contains("sm__throughput") {
        return "sm_throughput".into();
    }
    if name.contains("l1tex") {
        return "l1_hit_rate".into();
    }
    if name.contains("lts__t_sector") {
        return "l2_hit_rate".into();
    }
    if name.contains("long_scoreboard") {
        return "stall_long_sb".into();
    }
    if name.contains("short_scoreboard") {
        return "stall_short_sb".into();
    }
    // Shorten long metric names
    name.split("__").last().unwrap_or(name).to_owned()
}

fn split_csv_line(line: &str) -> Vec<String> {
    let mut cols = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in line.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                cols.push(cur.clone());
                cur.clear();
            }
            c => cur.push(c),
        }
    }
    cols.push(cur);
    cols
}

// ── ptx_inspect ───────────────────────────────────────────────────────────────

pub fn ptx_inspect(args: &Value) -> Result<ToolResult, String> {
    let path = args["path"]
        .as_str()
        .ok_or("ptx_inspect: 'path' is required")?;
    let kernel_filter = args["kernel"].as_str();

    let (ptx, ptxas_verbose) = if path.ends_with(".cu") {
        compile_to_ptx(path)?
    } else {
        (dump_ptx_from_binary(path)?, None)
    };

    let kernels = parse_ptx_kernels(&ptx, kernel_filter);

    Ok(ToolResult::json(&json!({
        "path":         path,
        "kernel_count": kernels.len(),
        "kernels":      kernels,
        "ptxas_verbose": ptxas_verbose,
        "ptx_lines":    ptx.lines().count(),
    })))
}

fn compile_to_ptx(src: &str) -> Result<(String, Option<String>), String> {
    let nvcc = which_bin("nvcc").ok_or("nvcc not found in PATH")?;
    let arch = live_arch_flags().unwrap_or_else(|| vec!["-arch=compute_86".into()]);

    let out_ptx = std::env::temp_dir().join("ferrite_inspect.ptx");

    let mut cmd = std::process::Command::new(&nvcc);
    cmd.arg("--ptx")
        .arg("--ptxas-options=-v")
        .args(&arch)
        .arg("-I/usr/local/cuda/include")
        .arg("-o")
        .arg(&out_ptx)
        .arg(src);

    let out = cmd.output().map_err(|e| format!("nvcc: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if !out.status.success() {
        return Err(format!("nvcc ptx compile failed:\n{stderr}"));
    }

    let ptx = std::fs::read_to_string(&out_ptx).map_err(|e| format!("reading ptx output: {e}"))?;
    std::fs::remove_file(&out_ptx).ok();

    // ptxas -v output is in stderr — extract "Used N registers, ..."
    let verbose = if stderr.contains("Used") {
        Some(stderr)
    } else {
        None
    };

    Ok((ptx, verbose))
}

fn dump_ptx_from_binary(binary: &str) -> Result<String, String> {
    let cuobjdump = which_bin("cuobjdump").ok_or("cuobjdump not found — install CUDA toolkit")?;

    let out = std::process::Command::new(&cuobjdump)
        .args(["--dump-ptx", binary])
        .output()
        .map_err(|e| format!("cuobjdump: {e}"))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("cuobjdump failed: {err}"));
    }

    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Parse PTX source into per-kernel summaries.
fn parse_ptx_kernels(ptx: &str, filter: Option<&str>) -> Vec<Value> {
    let mut kernels: Vec<Value> = Vec::new();

    // Split into kernel blocks on .visible .entry or .entry
    let mut current_name: Option<String> = None;
    let mut current_body: Vec<&str> = Vec::new();

    for line in ptx.lines() {
        let trimmed = line.trim();

        // New kernel: .visible .entry name( or .entry name(
        if trimmed.contains(".entry ") {
            // Flush previous kernel
            if let Some(name) = current_name.take() {
                if filter.map(|f| name.contains(f)).unwrap_or(true) {
                    kernels.push(analyse_kernel_body(&name, &current_body));
                }
                current_body.clear();
            }
            // Extract name — everything between ".entry " and "("
            if let Some(after) = trimmed.split_once(".entry ").map(|x| x.1) {
                let name = after.split('(').next().unwrap_or("").trim().to_owned();
                current_name = Some(name);
            }
        } else if current_name.is_some() {
            current_body.push(line);
        }
    }

    // Last kernel
    if let Some(name) = current_name {
        if filter.map(|f| name.contains(f)).unwrap_or(true) {
            kernels.push(analyse_kernel_body(&name, &current_body));
        }
    }

    kernels
}

fn analyse_kernel_body(name: &str, lines: &[&str]) -> Value {
    let mut regs_b32 = 0u32;
    let mut regs_f32 = 0u32;
    let mut regs_b64 = 0u32;
    let mut regs_pred = 0u32;
    let mut shared_bytes = 0u64;
    let mut ld_global = 0u32;
    let mut st_global = 0u32;
    let mut ld_shared = 0u32;
    let mut st_shared = 0u32;
    let mut sync_threads = 0u32;

    for line in lines {
        let t = line.trim();

        // Register declarations: .reg .b32 %r<N>; → N regs
        if t.starts_with(".reg ") {
            if let Some(n) = extract_reg_count(t) {
                if t.contains(".b32") || t.contains(".u32") || t.contains(".s32") {
                    regs_b32 += n;
                } else if t.contains(".f32") {
                    regs_f32 += n;
                } else if t.contains(".b64")
                    || t.contains(".u64")
                    || t.contains(".s64")
                    || t.contains(".f64")
                {
                    regs_b64 += n;
                } else if t.contains(".pred") {
                    regs_pred += n;
                }
            }
        }

        // Shared memory: .shared .align 4 .b8 smem[N]; → N bytes
        if t.contains(".shared") && t.contains("[") {
            if let Some(bytes) = extract_array_size(t) {
                shared_bytes += bytes;
            }
        }

        // Memory ops
        if t.starts_with("ld.global") {
            ld_global += 1;
        } else if t.starts_with("st.global") {
            st_global += 1;
        } else if t.starts_with("ld.shared") {
            ld_shared += 1;
        } else if t.starts_with("st.shared") {
            st_shared += 1;
        } else if t.starts_with("bar.sync") || t.starts_with("membar") {
            sync_threads += 1;
        }
    }

    // Total register footprint per thread (rough: b64 counts as 2 × b32)
    let total_regs = regs_b32 + regs_f32 + regs_b64 * 2 + regs_pred.div_ceil(4);

    json!({
        "name":          name,
        "registers": {
            "b32":   regs_b32,
            "f32":   regs_f32,
            "b64":   regs_b64,
            "pred":  regs_pred,
            "total_per_thread_estimate": total_regs,
        },
        "shared_mem_bytes": shared_bytes,
        "memory_ops": {
            "ld_global":   ld_global,
            "st_global":   st_global,
            "ld_shared":   ld_shared,
            "st_shared":   st_shared,
            "sync_points": sync_threads,
        },
        "body_lines": lines.len(),
    })
}

fn extract_reg_count(decl: &str) -> Option<u32> {
    // Matches: .reg .b32 %r<16>; → 16
    // or:      .reg .f32 %f<8>; → 8
    let start = decl.find('<')? + 1;
    let end = decl.find('>')?;
    decl[start..end].parse().ok()
}

fn extract_array_size(decl: &str) -> Option<u64> {
    // .shared .align 4 .b8 smem[2048]; → 2048
    let start = decl.find('[')? + 1;
    let end = decl.find(']')?;
    decl[start..end].parse().ok()
}

fn live_arch_flags() -> Option<Vec<String>> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=compute_cap", "--format=csv,noheader"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let cap = s.lines().next()?.trim();
    let (maj, min) = cap.split_once('.')?;
    Some(vec![
        format!("-arch=compute_{maj}{min}"),
        format!("-code=sm_{maj}{min},compute_{maj}{min}"),
    ])
}

// ── compute_sanitizer ─────────────────────────────────────────────────────────

pub fn compute_sanitizer(
    args: &Value,
    state: &Arc<Mutex<ServerState>>,
) -> Result<ToolResult, String> {
    let cmd_str = args["cmd"]
        .as_str()
        .ok_or("compute_sanitizer: 'cmd' is required")?;
    let tool = args["tool"].as_str().unwrap_or("memcheck");
    let timeout = args["timeout_secs"].as_u64().unwrap_or(120);

    let valid_tools = ["memcheck", "racecheck", "initcheck", "synccheck"];
    if !valid_tools.contains(&tool) {
        return Err(format!(
            "compute_sanitizer: invalid tool '{tool}'. Choose from: {}",
            valid_tools.join(", ")
        ));
    }

    let sanitizer = which_bin("compute-sanitizer")
        .ok_or_else(|| "compute-sanitizer not found. Install CUDA toolkit (>= 11.x)".to_owned())?;

    let cwd = read_cwd(state);

    let mut san_args: Vec<String> = vec![
        "--tool".into(),
        tool.to_owned(),
        "--print-limit".into(),
        "100".into(),
    ];
    san_args.extend(cmd_str.split_whitespace().map(str::to_owned));

    let (stdout, stderr, exit_code, duration_ms) =
        run_with_timeout(&sanitizer, &san_args, &cwd, timeout)?;

    let combined = format!("{stdout}\n{stderr}");
    let errors = parse_sanitizer_errors(&combined);
    let summary = parse_sanitizer_summary(&combined);

    Ok(ToolResult::json(&json!({
        "tool":        tool,
        "cmd":         cmd_str,
        "exit_code":   exit_code,
        "duration_ms": duration_ms,
        "clean":       errors.is_empty(),
        "error_count": errors.len(),
        "errors":      errors,
        "summary":     summary,
        "raw":         combined,
    })))
}

fn parse_sanitizer_errors(output: &str) -> Vec<Value> {
    let mut errors: Vec<Value> = Vec::new();
    let mut current: Option<serde_json::Map<String, Value>> = None;

    for line in output.lines() {
        let t = line.trim();
        if !t.starts_with("=========") {
            continue;
        }
        let content = t.trim_start_matches('=').trim_start_matches(' ');

        if content.is_empty() {
            continue;
        }

        if content.starts_with("Error ") && content.contains("out of") {
            // Start of new error block
            if let Some(prev) = current.take() {
                errors.push(Value::Object(prev));
            }
            current = Some(serde_json::Map::new());
            if let Some(ref mut e) = current {
                e.insert("header".into(), Value::String(content.to_owned()));
            }
        } else if let Some(ref mut e) = current {
            // Classify the content line
            if content.contains("out of bounds")
                || content.contains("Invalid")
                || content.contains("Race condition")
                || content.contains("Uninitialized")
                || content.contains("Barrier error")
            {
                e.insert("type".into(), Value::String(content.to_owned()));
            } else if content.starts_with("by thread") {
                e.insert("thread".into(), Value::String(content.to_owned()));
            } else if content.starts_with("Address") {
                e.insert("address".into(), Value::String(content.to_owned()));
            } else if content.contains(".cu:") || content.contains(".cpp:") {
                // Device Frame: kernel_name (file.cu:42)
                let frame = content.trim_start_matches("Device Frame:");
                if let Some((loc, rest)) = frame.split_once('(') {
                    let kernel = loc.trim().to_owned();
                    let file_line = rest.trim_end_matches(')');
                    if let Some((file, line)) = file_line.rsplit_once(':') {
                        e.insert("kernel".into(), Value::String(kernel));
                        e.insert("file".into(), Value::String(file.trim().to_owned()));
                        e.insert(
                            "line".into(),
                            line.trim()
                                .parse::<u64>()
                                .map(Value::from)
                                .unwrap_or(Value::Null),
                        );
                    }
                }
            }
        }
    }

    if let Some(last) = current {
        errors.push(Value::Object(last));
    }

    errors
}

fn parse_sanitizer_summary(output: &str) -> Value {
    for line in output.lines() {
        let t = line.trim().trim_start_matches('=').trim();
        if t.starts_with("ERROR SUMMARY:") {
            let count: u64 = t
                .split_whitespace()
                .nth(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            return json!({ "total_errors": count, "raw": t });
        }
    }
    Value::Null
}

// ── shared helpers ─────────────────────────────────────────────────────────────

fn which_bin(name: &str) -> Option<String> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    path_var
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(name))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .map(|p| p.display().to_string())
}

fn run_with_timeout(
    bin: &str,
    args: &[String],
    cwd: &Path,
    timeout_secs: u64,
) -> Result<(String, String, i32, u64), String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn {bin}: {e}"))?;

    // Drain stdout/stderr on separate threads to prevent pipe-buffer deadlock
    // (if the child writes more than ~64KB without being read, it blocks forever).
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");

    let stdout_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        stdout_pipe.read_to_string(&mut buf).ok();
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        stderr_pipe.read_to_string(&mut buf).ok();
        buf
    });

    let start = Instant::now();
    let deadline = start + Duration::from_secs(timeout_secs);

    let status = loop {
        match child.try_wait().map_err(|e| format!("try_wait: {e}"))? {
            Some(s) => break s,
            None => {
                if Instant::now() > deadline {
                    child.kill().ok();
                    // Still join threads to release resources
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(format!("timed out after {timeout_secs}s"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    let code = status.code().unwrap_or(-1);

    Ok((stdout, stderr, code, duration_ms))
}
