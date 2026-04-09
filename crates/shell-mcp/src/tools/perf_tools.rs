//! CPU performance profiling via Linux perf.
//!
//! Tools:
//!   flamegraph — perf record + hotspot analysis; SVG if flamegraph.pl/inferno available
//!   perf_stat  — perf stat counters for a command (cycles, instructions, cache misses, etc.)

use crate::protocol::ToolResult;
use serde_json::{json, Value};

const FLAMEGRAPH_LOCATIONS: &[&str] = &[
    "flamegraph.pl",
    "/usr/local/bin/flamegraph.pl",
    "/opt/flamegraph/flamegraph.pl",
    "inferno-flamegraph", // Rust inferno crate CLI (must be on PATH)
];

// ── flamegraph ────────────────────────────────────────────────────────────────

pub fn flamegraph(args: &Value) -> Result<ToolResult, String> {
    #[cfg(not(target_os = "linux"))]
    return Ok(ToolResult::error(
        "flamegraph: requires Linux perf. On macOS use Instruments: \
         xctrace record --template 'Time Profiler' --launch -- <cmd>",
    ));

    let cmd_str = args["cmd"].as_str().ok_or("flamegraph: 'cmd' required")?;
    let svg_out = args["output"]
        .as_str()
        .unwrap_or("/tmp/ferrite_flamegraph.svg");
    let freq = args["freq"].as_u64().unwrap_or(99);
    let timeout = args["timeout_secs"].as_u64().unwrap_or(60);

    // Verify perf is available
    if !has_binary("perf") {
        return Ok(ToolResult::error(
            "flamegraph: 'perf' not found. Install: sudo apt install linux-tools-$(uname -r)",
        ));
    }

    let perf_data = format!("/tmp/ferrite_perf_{}.data", std::process::id());
    let script_out = format!("/tmp/ferrite_perf_{}.txt", std::process::id());

    // ── perf record ──────────────────────────────────────────────────────────
    let record_cmd =
        format!("perf record -g -F {freq} --call-graph dwarf -o {perf_data} -- {cmd_str}");
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(timeout);

    let mut child = std::process::Command::new("sh")
        .args(["-c", &record_cmd])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("flamegraph: perf record: {e}"))?;

    loop {
        match child.try_wait().map_err(|e| format!("try_wait: {e}"))? {
            Some(_) => break,
            None => {
                if std::time::Instant::now() > deadline {
                    child.kill().ok();
                    let _ = std::fs::remove_file(&perf_data);
                    return Ok(ToolResult::error(format!(
                        "flamegraph: timed out after {timeout}s"
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    let duration_ms = start.elapsed().as_millis() as u64;
    let record_out = child
        .wait_with_output()
        .map_err(|e| format!("wait_with_output: {e}"))?;

    if !std::path::Path::new(&perf_data).exists() {
        let err = String::from_utf8_lossy(&record_out.stderr);
        return Ok(ToolResult::error(format!(
            "flamegraph: perf record failed (may need: echo 0 | sudo tee /proc/sys/kernel/perf_event_paranoid)\n{err}"
        )));
    }

    // ── perf script → collapsed stacks ───────────────────────────────────────
    let script_result = std::process::Command::new("perf")
        .args(["script", "-i", &perf_data])
        .output()
        .map_err(|e| format!("perf script: {e}"))?;
    std::fs::write(&script_out, &script_result.stdout).map_err(|e| format!("write script: {e}"))?;

    // ── perf report hotspots ─────────────────────────────────────────────────
    let report = std::process::Command::new("perf")
        .args([
            "report",
            "--stdio",
            "--no-children",
            "--sort=dso,sym",
            "-i",
            &perf_data,
            "--percent-limit=1",
        ])
        .output()
        .map_err(|e| format!("perf report: {e}"))?;
    let report_txt = String::from_utf8_lossy(&report.stdout).to_string();
    let hotspots = parse_perf_report(&report_txt);

    // ── optional SVG via flamegraph.pl or inferno ─────────────────────────────
    let (svg_path, svg_note) = try_generate_svg(&script_out, svg_out);

    // Cleanup
    let _ = std::fs::remove_file(&perf_data);
    let _ = std::fs::remove_file(&script_out);

    Ok(ToolResult::json(&json!({
        "command":       cmd_str,
        "duration_ms":   duration_ms,
        "svg_path":      svg_path,
        "svg_note":      svg_note,
        "hotspot_count": hotspots.len(),
        "hotspots":      hotspots,
    })))
}

fn try_generate_svg(script_out: &str, svg_out: &str) -> (String, String) {
    // Find a flamegraph renderer
    let renderer = FLAMEGRAPH_LOCATIONS
        .iter()
        .find(|&&p| has_binary(p))
        .copied();

    let Some(r) = renderer else {
        return (
            String::new(),
            "Install flamegraph: cargo install inferno  OR  get flamegraph.pl from github.com/brendangregg/FlameGraph".into(),
        );
    };

    // inferno needs stackcollapse-perf first if using inferno-flamegraph
    let pipeline = if r.contains("inferno") {
        format!("inferno-collapse-perf {script_out} | {r} > {svg_out}")
    } else {
        // flamegraph.pl from Brendan Gregg's repo: needs stackcollapse-perf.pl
        format!("stackcollapse-perf.pl {script_out} | {r} > {svg_out}")
    };

    let ok = std::process::Command::new("sh")
        .args(["-c", &pipeline])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if ok && std::path::Path::new(svg_out).exists() {
        (svg_out.to_owned(), String::new())
    } else {
        (
            String::new(),
            "SVG generation failed — perf script output saved for manual processing".into(),
        )
    }
}

fn parse_perf_report(report: &str) -> Vec<Value> {
    let mut hotspots = Vec::new();
    for line in report.lines() {
        let t = line.trim();
        // Lines like: "    12.34%  binary  [.] function_name"
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = t.splitn(4, ' ').collect();
        if parts.len() < 4 {
            continue;
        }
        let pct_str = parts[0].trim_end_matches('%');
        let Ok(pct) = pct_str.parse::<f64>() else {
            continue;
        };
        if pct < 0.5 {
            continue;
        }
        let symbol = parts[3].trim().to_owned();
        hotspots.push(json!({ "pct": pct, "symbol": symbol }));
        if hotspots.len() >= 20 {
            break;
        }
    }
    hotspots
}

// ── perf_stat ─────────────────────────────────────────────────────────────────

pub fn perf_stat(args: &Value) -> Result<ToolResult, String> {
    #[cfg(not(target_os = "linux"))]
    return Ok(ToolResult::error(
        "perf_stat: requires Linux perf. On macOS use: \
         xctrace record --template 'Time Profiler' --launch -- <cmd>",
    ));

    let cmd_str = args["cmd"].as_str().ok_or("perf_stat: 'cmd' required")?;
    let timeout = args["timeout_secs"].as_u64().unwrap_or(60);
    let events = args["events"]
        .as_str()
        .unwrap_or("cycles,instructions,cache-misses,cache-references,branch-misses,task-clock");

    if !has_binary("perf") {
        return Ok(ToolResult::error("perf_stat: 'perf' not found"));
    }

    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(timeout);

    let mut child = std::process::Command::new("perf")
        .args(["stat", "-e", events, "--", "sh", "-c", cmd_str])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("perf_stat: {e}"))?;

    loop {
        match child.try_wait().map_err(|e| format!("try_wait: {e}"))? {
            Some(_) => break,
            None => {
                if std::time::Instant::now() > deadline {
                    child.kill().ok();
                    return Ok(ToolResult::error(format!(
                        "perf_stat: timed out after {timeout}s"
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait_with_output: {e}"))?;

    // perf stat writes to stderr
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let counters = parse_perf_stat(&stderr);

    Ok(ToolResult::json(&json!({
        "command":     cmd_str,
        "duration_ms": duration_ms,
        "success":     out.status.success(),
        "counters":    counters,
        "raw":         stderr,
    })))
}

fn parse_perf_stat(output: &str) -> Vec<Value> {
    let mut counters = Vec::new();
    for line in output.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with("Performance") {
            continue;
        }
        // Format: "      1,234,567      cycles         # 2.34 GHz"
        // or:     "      1,234,567 ns   task-clock     # ..."
        let parts: Vec<&str> = t.splitn(3, ' ').filter(|s| !s.is_empty()).collect();
        if parts.len() < 2 {
            continue;
        }
        let raw_val = parts[0].replace(',', "").replace('.', "");
        let Ok(val) = raw_val.parse::<u64>() else {
            continue;
        };
        let event = parts.get(1).unwrap_or(&"").trim().to_owned();
        if event.is_empty() {
            continue;
        }
        let note = parts
            .get(2)
            .map(|s| s.trim_start_matches('#').trim().to_owned());
        counters.push(json!({
            "event": event,
            "value": val,
            "note":  note,
        }));
    }
    counters
}

fn has_binary(name: &str) -> bool {
    if name.starts_with('/') {
        std::path::Path::new(name).exists()
    } else {
        std::process::Command::new("which")
            .arg(name)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}
