//! Runtime debugging via GDB.
//!
//! Tools:
//!   gdb_run — run a binary under GDB in batch mode; returns structured
//!              backtrace, locals, signal info, breakpoint hits.

use crate::protocol::ToolResult;
use serde_json::{json, Value};

// ── gdb_run ───────────────────────────────────────────────────────────────────

/// Run a binary under GDB in batch mode.
///
/// Modes:
///   - Normal run:      binary + optional args + optional breakpoints
///   - Core analysis:   binary + core file → bt, registers
///   - Custom commands: pass `commands` array for full control
pub fn gdb_run(args: &Value) -> Result<ToolResult, String> {
    let binary = args["binary"]
        .as_str()
        .ok_or("gdb_run: 'binary' required")?;
    let run_args = args["args"].as_str().unwrap_or("");
    let core = args["core"].as_str().unwrap_or("");
    let timeout = args["timeout_secs"].as_u64().unwrap_or(30);

    let breakpoints: Vec<&str> = args["breakpoints"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let extra_cmds: Vec<&str> = args["commands"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    if !std::path::Path::new(binary).exists() {
        return Ok(ToolResult::error(format!(
            "gdb_run: binary not found: {binary}"
        )));
    }

    // ── build GDB command sequence ────────────────────────────────────────────
    let mut cmd = std::process::Command::new("gdb");
    cmd.arg("--batch")
        .arg("--quiet")
        .arg("-ex")
        .arg("set print pretty on")
        .arg("-ex")
        .arg("set pagination off")
        .arg("-ex")
        .arg("set confirm off");

    if !core.is_empty() {
        // Core dump analysis
        cmd.arg("-ex")
            .arg(format!("core-file {core}"))
            .arg("-ex")
            .arg("bt full")
            .arg("-ex")
            .arg("info registers")
            .arg("-ex")
            .arg("info threads");
    } else {
        // Live execution
        for bp in &breakpoints {
            cmd.arg("-ex").arg(format!("break {bp}"));
        }

        let run_line = if run_args.is_empty() {
            "run".to_owned()
        } else {
            format!("run {run_args}")
        };
        cmd.arg("-ex").arg(run_line);

        if !breakpoints.is_empty() {
            // After hitting breakpoint: print context
            cmd.arg("-ex")
                .arg("bt")
                .arg("-ex")
                .arg("info locals")
                .arg("-ex")
                .arg("info args");
        } else {
            cmd.arg("-ex").arg("bt");
        }
    }

    // User-supplied commands last
    for c in &extra_cmds {
        cmd.arg("-ex").arg(c);
    }

    cmd.arg(binary);
    if !core.is_empty() {
        cmd.arg(core);
    }

    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // ── run with timeout ─────────────────────────────────────────────────────
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(timeout);

    let mut child = cmd.spawn().map_err(|e| format!("gdb_run: {e}"))?;

    loop {
        match child.try_wait().map_err(|e| format!("try_wait: {e}"))? {
            Some(_) => break,
            None => {
                if std::time::Instant::now() > deadline {
                    child.kill().ok();
                    return Ok(ToolResult::error(format!(
                        "gdb_run: timed out after {timeout}s"
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait_with_output: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let combined = format!("{stdout}\n{stderr}");

    // ── parse structured output ───────────────────────────────────────────────
    let parsed = parse_gdb_output(&combined);

    Ok(ToolResult::json(&json!({
        "binary":          binary,
        "duration_ms":     duration_ms,
        "exit_normal":     parsed.exit_normal,
        "signal":          parsed.signal,
        "signal_addr":     parsed.signal_addr,
        "breakpoints_hit": parsed.breakpoints_hit,
        "backtrace":       parsed.backtrace,
        "locals":          parsed.locals,
        "registers":       parsed.registers,
        "output":          combined,
    })))
}

struct GdbParsed {
    exit_normal: bool,
    signal: Option<String>,
    signal_addr: Option<String>,
    breakpoints_hit: Vec<Value>,
    backtrace: Vec<Value>,
    locals: Vec<Value>,
    registers: Vec<Value>,
}

fn parse_gdb_output(output: &str) -> GdbParsed {
    let mut p = GdbParsed {
        exit_normal: false,
        signal: None,
        signal_addr: None,
        breakpoints_hit: Vec::new(),
        backtrace: Vec::new(),
        locals: Vec::new(),
        registers: Vec::new(),
    };

    let mut in_bt = false;
    let mut in_locals = false;
    let mut in_regs = false;
    let mut frame_idx = 0usize;

    for line in output.lines() {
        let t = line.trim();

        // Exit status
        if t.contains("exited normally") || t.contains("exited with code 0") {
            p.exit_normal = true;
        }

        // Signal received
        if t.starts_with("Program received signal") {
            // "Program received signal SIGSEGV, Segmentation fault."
            let parts: Vec<&str> = t.splitn(4, ' ').collect();
            p.signal = parts.get(3).map(|s| s.trim_end_matches('.').to_owned());
            continue;
        }
        if t.contains("0x") && p.signal.is_some() && p.signal_addr.is_none() {
            // Next line after signal often has the address
            if let Some(addr) = extract_hex_addr(t) {
                p.signal_addr = Some(addr);
            }
        }

        // Breakpoint hits: "Breakpoint 1, function (args) at file:line"
        if t.starts_with("Breakpoint ") && t.contains(" at ") {
            let loc = t.split(" at ").nth(1).unwrap_or("").to_owned();
            let func = t
                .split(',')
                .nth(1)
                .map(|s| s.trim().to_owned())
                .unwrap_or_default();
            p.breakpoints_hit
                .push(json!({ "function": func, "location": loc }));
            in_bt = false;
            in_locals = false;
        }

        // Backtrace frames: "#0  function (args) at file:line"
        if t.starts_with('#')
            && t.chars()
                .nth(1)
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
        {
            in_bt = true;
            in_locals = false;
            in_regs = false;

            let frame = parse_bt_frame(t);
            frame_idx = frame["frame"].as_u64().unwrap_or(0) as usize;
            p.backtrace.push(frame);
            continue;
        }

        // Locals / args
        if t.starts_with("No locals") || t.starts_with("No arguments") {
            continue;
        }

        // register lines from "info registers": "rax            0x0   0"
        if in_regs {
            let parts: Vec<&str> = t.splitn(3, ' ').filter(|s| !s.is_empty()).collect();
            if parts.len() >= 2 {
                p.registers.push(json!({
                    "reg": parts[0],
                    "hex": parts.get(1).unwrap_or(&""),
                }));
            }
        }

        if t == "Registers:" || t.starts_with("(gdb)") && in_bt {
            in_regs = true;
            in_bt = false;
        }

        // Variable assignments in locals/args: "var = value"
        if in_locals || (in_bt && frame_idx == 0) {
            if let Some((name, val)) = t.split_once(" = ") {
                if !name.contains('(') && !name.is_empty() {
                    p.locals
                        .push(json!({ "name": name.trim(), "value": val.trim() }));
                }
            }
        }

        if t == "Locals:" || t.contains("Local variables:") {
            in_locals = true;
        }
    }

    p
}

fn parse_bt_frame(line: &str) -> Value {
    // "#N  function (args) at file:line"  or  "#N  0xADDR in function () at file:line"
    let frame_no: u64 = line
        .trim_start_matches('#')
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);

    let at_part = line.split(" at ").nth(1).unwrap_or("").to_owned();
    let (file, lineno) = at_part
        .rsplit_once(':')
        .map(|(f, l)| (f.to_owned(), l.parse::<u32>().unwrap_or(0)))
        .unwrap_or((at_part, 0));

    // Extract function name (between first space and '(')
    let rest = line.splitn(2, ' ').nth(1).unwrap_or("").trim();
    let func = if rest.starts_with("0x") {
        // "0xADDR in function_name ("
        rest.split(" in ")
            .nth(1)
            .and_then(|s| s.split('(').next())
            .unwrap_or("")
            .trim()
            .to_owned()
    } else {
        rest.split('(').next().unwrap_or("").trim().to_owned()
    };

    json!({
        "frame":    frame_no,
        "function": func,
        "file":     file.trim(),
        "line":     lineno,
    })
}

fn extract_hex_addr(s: &str) -> Option<String> {
    s.split_whitespace()
        .find(|w| w.starts_with("0x") && w.len() > 4)
        .map(|w| w.trim_end_matches(',').to_owned())
}
