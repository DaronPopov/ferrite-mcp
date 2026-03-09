//! EDA tools: Verilog simulation, cocotb testbenches, Vivado Tcl, FPGA programming.
//!
//! Tool inventory:
//!   verilog_lint   — iverilog -tnull syntax/semantic check
//!   verilog_sim    — iverilog + vvp simulation
//!   cocotb_run     — cocotb 2.x pytest runner
//!   vivado_tcl     — Vivado batch-mode Tcl (synthesis, impl, queries)
//!   fpga_boards    — list connected JTAG targets via Vivado hw_manager
//!   fpga_program   — program a bitstream to a connected FPGA
//!   waveform_query — parse VCD dump, return signal timeline

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::protocol::ToolResult;

const VIVADO: &str = "/opt/2025.2/Vivado/bin/vivado";
const XVLOG: &str = "/opt/2025.2/Vivado/bin/xvlog";
const XELAB: &str = "/opt/2025.2/Vivado/bin/xelab";
const XILINX_UNISIMS_DIR: &str = "/opt/2025.2/Vivado/data/verilog/src/unisims";
const XILINX_GLBL_V: &str = "/opt/2025.2/Vivado/ids_lite/ISE/verilog/src/glbl.v";
const XILINX_XPM_MEMORY_SV: &str = "/opt/2025.2/Vivado/data/ip/xpm/xpm_memory/hdl/xpm_memory.sv";

// ── verilog_lint ──────────────────────────────────────────────────────────────

/// Lint Verilog/SystemVerilog files with `iverilog -tnull`.
/// Returns structured diagnostics: [{severity, file, line, message}].
pub fn verilog_lint(args: &Value) -> Result<ToolResult, String> {
    let files = collect_str_array(args, "files", "verilog_lint")?;
    let top          = args["top"].as_str().unwrap_or("");
    let include_dirs = collect_str_array(args, "include_dirs", "verilog_lint").unwrap_or_default();
    let libraries    = collect_optional_str_array(args, "libraries", "verilog_lint")?;
    let standard     = args["standard"].as_str().unwrap_or("2012");

    let mut cmd = std::process::Command::new("iverilog");
    cmd.arg("-tnull").arg(format!("-g{standard}"));
    if !top.is_empty() { cmd.args(["-s", top]); }
    for d in &include_dirs { cmd.args(["-I", d]); }
    let resolved_libraries = apply_iverilog_libraries(&mut cmd, &libraries, "verilog_lint")?;
    if uses_xilinx_glbl(&libraries) {
        cmd.args(["-s", "glbl"]);
    }
    for f in &files        { cmd.arg(f); }

    let out = cmd.output().map_err(|e| format!("iverilog: {e}"))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let diagnostics = parse_iverilog_diag(&combined);
    let error_count   = diagnostics.iter().filter(|d| d["severity"] == "error").count();
    let warning_count = diagnostics.iter().filter(|d| d["severity"] == "warning").count();

    Ok(ToolResult::json(&json!({
        "success":       out.status.success(),
        "error_count":   error_count,
        "warning_count": warning_count,
        "libraries_used": resolved_libraries,
        "diagnostics":   diagnostics,
    })))
}

/// Parse iverilog diagnostic lines.
/// Formats: "path/file.v:10: error: msg"  or  "path/file.v:10: warning: msg"
fn parse_iverilog_diag(output: &str) -> Vec<Value> {
    let mut results = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }

        let severity = if line.contains(": error:") {
            "error"
        } else if line.contains(": warning:") {
            "warning"
        } else if line.contains(": note:") {
            "note"
        } else {
            continue;
        };

        // "file.v:LINE: severity: message"
        let parts: Vec<&str> = line.splitn(4, ':').collect();
        let (file, line_no, message) = if parts.len() >= 4 {
            let file_part = parts[0].trim();
            let line_part = parts[1].trim().parse::<u32>().unwrap_or(0);
            let msg = parts[3].trim();
            (file_part, line_part, msg)
        } else {
            ("", 0, line)
        };

        results.push(json!({
            "severity": severity,
            "file":     file,
            "line":     line_no,
            "message":  message,
        }));
    }
    results
}

fn combine_output(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn collect_prefixed_lines(output: &str, prefixes: &[&str]) -> Vec<String> {
    output.lines()
        .map(str::trim)
        .filter(|line| prefixes.iter().any(|prefix| line.starts_with(prefix)))
        .map(str::to_owned)
        .collect()
}

// ── verilog_sim ───────────────────────────────────────────────────────────────

/// Compile Verilog with iverilog then simulate with vvp.
/// Returns {success, finished, assertion_failures, stdout, stderr, duration_ms}.
pub fn verilog_sim(args: &Value) -> Result<ToolResult, String> {
    let files        = collect_str_array(args, "files", "verilog_sim")?;
    let top          = args["top"].as_str().unwrap_or("tb");
    let include_dirs = collect_str_array(args, "include_dirs", "verilog_sim").unwrap_or_default();
    let libraries    = collect_optional_str_array(args, "libraries", "verilog_sim")?;
    let vcd_out      = args["vcd_out"].as_str();
    let standard     = args["standard"].as_str().unwrap_or("2012");
    let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30);

    let bin = format!("/tmp/ferrite_sim_{}", std::process::id());

    // ── Compile ──────────────────────────────────────────────────────────────
    let mut compile = std::process::Command::new("iverilog");
    compile.arg(format!("-g{standard}")).args(["-s", top, "-o", &bin]);
    for d in &include_dirs { compile.args(["-I", d]); }
    let resolved_libraries = apply_iverilog_libraries(&mut compile, &libraries, "verilog_sim")?;
    if uses_xilinx_glbl(&libraries) {
        compile.args(["-s", "glbl"]);
    }
    if let Some(vcd) = vcd_out {
        // Emit VCD — user must have $dumpfile/$dumpvars in testbench,
        // but we pass the preferred path via plusarg
        compile.arg(format!("+FERRITE_VCD={vcd}"));
    }
    for f in &files { compile.arg(f); }

    let compile_out = compile.output().map_err(|e| format!("iverilog compile: {e}"))?;
    let compile_diag = format!(
        "{}{}",
        String::from_utf8_lossy(&compile_out.stdout),
        String::from_utf8_lossy(&compile_out.stderr)
    );

    if !compile_out.status.success() {
        let _ = std::fs::remove_file(&bin);
        return Ok(ToolResult::json(&json!({
            "phase":    "compile",
            "success":  false,
            "libraries_used": resolved_libraries,
            "diagnostics": parse_iverilog_diag(&compile_diag),
            "raw":      compile_diag,
        })));
    }

    // ── Simulate ─────────────────────────────────────────────────────────────
    let start    = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(timeout_secs);

    let mut child = std::process::Command::new("vvp")
        .arg(&bin)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("vvp: {e}"))?;

    loop {
        match child.try_wait().map_err(|e| format!("try_wait: {e}"))? {
            Some(_) => break,
            None => {
                if std::time::Instant::now() > deadline {
                    child.kill().ok();
                    let _ = std::fs::remove_file(&bin);
                    return Ok(ToolResult::error(format!("verilog_sim: timed out after {timeout_secs}s")));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    let out = child.wait_with_output().map_err(|e| format!("wait_with_output: {e}"))?;
    let _ = std::fs::remove_file(&bin);

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    let finished  = stdout.contains("$finish") || stderr.contains("$finish");
    let asserts: Vec<&str> = stdout.lines()
        .filter(|l| {
            let low = l.to_lowercase();
            low.contains("assertion failed") || low.contains("assert fail")
        })
        .collect();

    Ok(ToolResult::json(&json!({
        "phase":               "simulate",
        "success":             out.status.success(),
        "libraries_used":      resolved_libraries,
        "finished":            finished,
        "assertion_failures":  asserts.len(),
        "duration_ms":         duration_ms,
        "stdout":              stdout,
        "stderr":              stderr,
    })))
}

/// Run Vivado Simulator front-end checks via xvlog/xelab.
/// Uses precompiled Xilinx libraries so XPM and UNISIM-heavy designs elaborate cleanly.
pub fn xsim_elab(args: &Value) -> Result<ToolResult, String> {
    let files = collect_str_array(args, "files", "xsim_elab")?;
    let top = args["top"].as_str().ok_or("xsim_elab: 'top' required")?;
    let include_dirs = collect_optional_str_array(args, "include_dirs", "xsim_elab")?;
    let libraries = collect_optional_str_array(args, "libraries", "xsim_elab")?;
    let defines = collect_optional_str_array(args, "defines", "xsim_elab")?;
    let standard = args["standard"].as_str().unwrap_or("2012");
    let snapshot = args["snapshot"].as_str().unwrap_or("ferrite_xsim");

    if !Path::new(XVLOG).exists() {
        return Err(format!("xsim_elab: missing xvlog: {XVLOG}"));
    }
    if !Path::new(XELAB).exists() {
        return Err(format!("xsim_elab: missing xelab: {XELAB}"));
    }

    let workdir = std::env::temp_dir().join(format!(
        "ferrite_xsim_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&workdir).map_err(|e| format!("xsim_elab: create workdir: {e}"))?;

    let resolved_libraries = resolve_xsim_libraries(&libraries, "xsim_elab")?;
    let compile_files = xsim_compile_files(&files, &libraries)?;
    let elaborate_tops = xsim_elab_tops(top, &libraries);

    let mut xvlog = std::process::Command::new(XVLOG);
    xvlog.current_dir(&workdir);
    for flag in xsim_compile_flags(standard) {
        xvlog.arg(flag);
    }
    for dir in &include_dirs {
        xvlog.args(["-i", dir]);
    }
    for define in &defines {
        xvlog.args(["-d", define]);
    }
    for file in &compile_files {
        xvlog.arg(file);
    }

    let xvlog_out = xvlog.output().map_err(|e| format!("xsim_elab xvlog: {e}"))?;
    let xvlog_text = combine_output(&xvlog_out);
    if !xvlog_out.status.success() {
        return Ok(ToolResult::json(&json!({
            "phase": "compile",
            "success": false,
            "libraries_used": resolved_libraries,
            "workdir": workdir,
            "errors": collect_prefixed_lines(&xvlog_text, &["ERROR:"]),
            "warnings": collect_prefixed_lines(&xvlog_text, &["WARNING:"]),
            "output": xvlog_text,
        })));
    }

    let mut xelab = std::process::Command::new(XELAB);
    xelab.current_dir(&workdir).args(["--nolog", "--relax", "-mt", "off"]);
    for lib in xsim_search_libraries(&libraries) {
        xelab.args(["-L", &lib]);
    }
    for unit in &elaborate_tops {
        xelab.arg(unit);
    }
    xelab.args(["-s", snapshot]);

    let xelab_out = xelab.output().map_err(|e| format!("xsim_elab xelab: {e}"))?;
    let xelab_text = combine_output(&xelab_out);

    Ok(ToolResult::json(&json!({
        "phase": "elaborate",
        "success": xelab_out.status.success(),
        "libraries_used": resolved_libraries,
        "top_units": elaborate_tops,
        "workdir": workdir,
        "errors": collect_prefixed_lines(&xelab_text, &["ERROR:"]),
        "warnings": collect_prefixed_lines(&xelab_text, &["WARNING:"]),
        "output": xelab_text,
    })))
}

// ── cocotb_run ────────────────────────────────────────────────────────────────

/// Run cocotb 2.x tests via pytest in the given directory.
/// cocotb 2.0 uses pytest natively; Makefile fallback also supported.
pub fn cocotb_run(args: &Value) -> Result<ToolResult, String> {
    let dir         = args["dir"].as_str().ok_or("cocotb_run: 'dir' required")?;
    let simulator   = args["simulator"].as_str().unwrap_or("icarus");
    let test_module = args["module"].as_str().unwrap_or("");
    let timeout     = args["timeout_secs"].as_u64().unwrap_or(120);

    let dir_path = PathBuf::from(dir);
    if !dir_path.exists() {
        return Ok(ToolResult::error(format!("cocotb_run: dir not found: {dir}")));
    }

    // cocotb 2.x: prefer pytest if available, else fall back to make
    let use_pytest = std::process::Command::new("python3")
        .args(["-m", "pytest", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let start    = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(timeout);

    let mut cmd = if use_pytest {
        let mut c = std::process::Command::new("python3");
        c.args(["-m", "pytest", "-v", "--tb=short"]);
        if !test_module.is_empty() { c.arg(test_module); }
        c.current_dir(&dir_path)
         .env("SIM", simulator)
         .env("COCOTB_REDUCED_LOG_FMT", "1")
         .stdout(std::process::Stdio::piped())
         .stderr(std::process::Stdio::piped());
        c
    } else {
        let mut c = std::process::Command::new("make");
        c.current_dir(&dir_path)
         .env("SIM", simulator)
         .env("COCOTB_REDUCED_LOG_FMT", "1");
        if !test_module.is_empty() { c.env("MODULE", test_module); }
        c.stdout(std::process::Stdio::piped())
         .stderr(std::process::Stdio::piped());
        c
    };

    let mut child = cmd.spawn().map_err(|e| format!("cocotb_run: {e}"))?;

    loop {
        match child.try_wait().map_err(|e| format!("try_wait: {e}"))? {
            Some(_) => break,
            None => {
                if std::time::Instant::now() > deadline {
                    child.kill().ok();
                    return Ok(ToolResult::error(format!("cocotb_run: timed out after {timeout}s")));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    let out = child.wait_with_output().map_err(|e| format!("wait_with_output: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let combined = format!("{stdout}\n{stderr}");

    let (passed, failed) = if use_pytest {
        parse_pytest_output(&combined)
    } else {
        parse_cocotb_make_output(&combined)
    };

    Ok(ToolResult::json(&json!({
        "success":     out.status.success(),
        "runner":      if use_pytest { "pytest" } else { "make" },
        "simulator":   simulator,
        "passed":      passed,
        "failed":      failed,
        "duration_ms": duration_ms,
        "output":      combined,
    })))
}

fn parse_pytest_output(output: &str) -> (Vec<Value>, Vec<Value>) {
    let mut passed = Vec::new();
    let mut failed = Vec::new();
    for line in output.lines() {
        let t = line.trim();
        if t.ends_with(" PASSED") {
            let name = t.trim_end_matches(" PASSED").trim();
            passed.push(json!({ "name": name }));
        } else if t.ends_with(" FAILED") {
            let name = t.trim_end_matches(" FAILED").trim();
            failed.push(json!({ "name": name }));
        }
    }
    (passed, failed)
}

fn parse_cocotb_make_output(output: &str) -> (Vec<Value>, Vec<Value>) {
    let mut passed = Vec::new();
    let mut failed = Vec::new();
    for line in output.lines() {
        // cocotb make output: "                 test_name PASS" / "FAIL"
        let t = line.trim();
        if t.ends_with("PASS") {
            if let Some(name) = t.split_whitespace().rev().nth(1) {
                passed.push(json!({ "name": name }));
            }
        } else if t.ends_with("FAIL") {
            if let Some(name) = t.split_whitespace().rev().nth(1) {
                failed.push(json!({ "name": name }));
            }
        }
    }
    (passed, failed)
}

// ── vivado_tcl ────────────────────────────────────────────────────────────────

/// Run Vivado in batch mode with a Tcl script or inline command.
/// This is the universal EDA escape hatch: synthesis, impl, bitstream, queries.
pub fn vivado_tcl(args: &Value) -> Result<ToolResult, String> {
    let script_file = args["script"].as_str();
    let inline_cmd  = args["cmd"].as_str();
    let timeout     = args["timeout_secs"].as_u64().unwrap_or(600);

    if script_file.is_none() && inline_cmd.is_none() {
        return Err("vivado_tcl: provide 'script' (file path) or 'cmd' (inline Tcl)".into());
    }

    // Write inline cmd to a temp file
    let tmp_path = format!("/tmp/ferrite_vivado_{}.tcl", std::process::id());
    let script_path: String = if let Some(path) = script_file {
        path.to_owned()
    } else {
        std::fs::write(&tmp_path, inline_cmd.unwrap())
            .map_err(|e| format!("vivado_tcl: write tmp: {e}"))?;
        tmp_path.clone()
    };

    let start    = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(timeout);

    let mut child = std::process::Command::new(VIVADO)
        .args(["-mode", "batch", "-nolog", "-nojournal", "-source", &script_path])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("vivado_tcl: {e}"))?;

    loop {
        match child.try_wait().map_err(|e| format!("try_wait: {e}"))? {
            Some(_) => break,
            None => {
                if std::time::Instant::now() > deadline {
                    child.kill().ok();
                    if script_file.is_none() { let _ = std::fs::remove_file(&tmp_path); }
                    return Ok(ToolResult::error(format!("vivado_tcl: timed out after {timeout}s")));
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }

    if script_file.is_none() { let _ = std::fs::remove_file(&tmp_path); }

    let duration_ms = start.elapsed().as_millis() as u64;
    let out = child.wait_with_output().map_err(|e| format!("wait_with_output: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    let errors: Vec<&str> = stdout.lines()
        .filter(|l| l.starts_with("ERROR:") || l.starts_with("FATAL:"))
        .collect();
    let warnings: Vec<&str> = stdout.lines()
        .filter(|l| l.starts_with("WARNING:") || l.starts_with("CRITICAL WARNING:"))
        .collect();

    Ok(ToolResult::json(&json!({
        "success":       out.status.success() && errors.is_empty(),
        "duration_ms":   duration_ms,
        "error_count":   errors.len(),
        "warning_count": warnings.len(),
        "errors":        errors,
        "warnings":      warnings,
        "output":        stdout,
        "stderr":        stderr,
    })))
}

// ── fpga_boards ───────────────────────────────────────────────────────────────

/// List FPGA boards connected via JTAG, using Vivado hw_manager.
pub fn fpga_boards(_args: &Value) -> Result<ToolResult, String> {
    // refresh_hw_server is required — without it get_hw_targets returns empty
    // even when a board is physically connected and hw_server is running.
    let tcl = r#"
open_hw_manager
connect_hw_server -allow_non_jtag -url TCP:127.0.0.1:3121
refresh_hw_server
set targets [get_hw_targets]
foreach t $targets {
    puts "TARGET: $t"
    if {[catch {open_hw_target $t} open_err]} {
        puts "OPEN_ERROR: $t :: $open_err"
        continue
    }
    set devices [get_hw_devices]
    foreach d $devices {
        set part [get_property PART $d]
        set status "unknown"
        if {[catch {set status [get_property STATUS $d]}]} {
            # Some families expose STATE instead of STATUS on hw_device.
            catch {set status [get_property STATE $d]}
        }
        puts "DEVICE|||$d|||$part|||$status"
    }
    catch {close_hw_target}
}
disconnect_hw_server
close_hw_manager
"#;

    let tmp = format!("/tmp/ferrite_boards_{}.tcl", std::process::id());
    std::fs::write(&tmp, tcl).map_err(|e| format!("fpga_boards: write tmp: {e}"))?;

    let out = std::process::Command::new(VIVADO)
        .args(["-mode", "batch", "-nolog", "-nojournal", "-source", &tmp])
        .output()
        .map_err(|e| format!("fpga_boards: vivado: {e}"))?;
    let _ = std::fs::remove_file(&tmp);

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let boards = parse_hw_targets(&stdout);

    Ok(ToolResult::json(&json!({
        "success": out.status.success(),
        "found":   boards.len(),
        "boards":  boards,
    })))
}

fn parse_hw_targets(output: &str) -> Vec<Value> {
    let mut boards = Vec::new();
    let mut current_target = String::new();

    for line in output.lines() {
        if let Some(t) = line.strip_prefix("TARGET: ") {
            current_target = t.trim().to_owned();
        } else if let Some(rest) = line.trim().strip_prefix("DEVICE|||") {
            let parts: Vec<&str> = rest.split("|||").collect();
            let device = parts.get(0).copied().unwrap_or("").to_owned();
            let part   = parts.get(1).copied().unwrap_or("").to_owned();
            let status = parts.get(2).copied().unwrap_or("").to_owned();
            boards.push(json!({
                "target": current_target,
                "device": device,
                "part":   part,
                "status": status,
            }));
        } else if line.trim_start().starts_with("DEVICE: ") {
            // Backward-compat parser for older "DEVICE: ... PART: ... STATUS: ..." format.
            let tokens: Vec<&str> = line.trim().split_whitespace().collect();
            let device = tokens.get(1).copied().unwrap_or("").to_owned();
            let part   = tokens.get(3).copied().unwrap_or("").to_owned();
            let status = tokens.get(5).copied().unwrap_or("").to_owned();
            boards.push(json!({
                "target": current_target,
                "device": device,
                "part":   part,
                "status": status,
            }));
        }
    }
    boards
}

// ── fpga_program ──────────────────────────────────────────────────────────────

/// Program a bitstream (.bit) to a connected FPGA via Vivado hw_manager.
pub fn fpga_program(args: &Value) -> Result<ToolResult, String> {
    let bitfile = args["bitfile"].as_str().ok_or("fpga_program: 'bitfile' required")?;
    let target  = args["target"].as_str().unwrap_or("");
    let timeout = args["timeout_secs"].as_u64().unwrap_or(120);

    if !Path::new(bitfile).exists() {
        return Ok(ToolResult::error(format!("fpga_program: bitfile not found: {bitfile}")));
    }

    // Build the target-selection snippet.
    // refresh_hw_server is critical — without it get_hw_targets returns empty
    // even when a board is connected and hw_server is already running.
    let target_tcl = if target.is_empty() {
        "lindex [get_hw_targets] 0".to_owned()
    } else {
        format!("get_hw_targets {{{target}}}")
    };

    let tcl = format!(r#"
open_hw_manager
connect_hw_server -allow_non_jtag -url TCP:127.0.0.1:3121
refresh_hw_server
set t [{target_tcl}]
if {{$t eq ""}} {{ puts "ERROR: no JTAG target found"; exit 1 }}
open_hw_target $t
set dev [lindex [get_hw_devices] 0]
current_hw_device $dev
set_property PROGRAM.FILE {{{bitfile}}} $dev
program_hw_devices $dev
puts "PROGRAMMED: [get_property PART $dev] on $t"
close_hw_target
disconnect_hw_server
close_hw_manager
"#);

    let tmp = format!("/tmp/ferrite_prog_{}.tcl", std::process::id());
    std::fs::write(&tmp, &tcl).map_err(|e| format!("fpga_program: write tmp: {e}"))?;

    let start    = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(timeout);

    let mut child = std::process::Command::new(VIVADO)
        .args(["-mode", "batch", "-nolog", "-nojournal", "-source", &tmp])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("fpga_program: vivado: {e}"))?;

    // Drain stdout + stderr in background threads so the OS pipe buffer never
    // fills up and deadlocks the child process (Vivado can be verbose).
    use std::io::Read;
    use std::sync::{Arc, Mutex};

    let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));

    let stdout_t = {
        let buf = Arc::clone(&stdout_buf);
        let mut pipe = child.stdout.take().unwrap();
        std::thread::spawn(move || { let _ = pipe.read_to_end(&mut buf.lock().unwrap()); })
    };
    let stderr_t = {
        let buf = Arc::clone(&stderr_buf);
        let mut pipe = child.stderr.take().unwrap();
        std::thread::spawn(move || { let _ = pipe.read_to_end(&mut buf.lock().unwrap()); })
    };

    // Poll for completion with timeout.
    let timed_out = loop {
        match child.try_wait().map_err(|e| format!("try_wait: {e}"))? {
            Some(_) => break false,
            None => {
                if std::time::Instant::now() > deadline {
                    child.kill().ok();
                    break true;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    };

    let _ = stdout_t.join();
    let _ = stderr_t.join();
    let _ = std::fs::remove_file(&tmp);
    let duration_ms = start.elapsed().as_millis() as u64;

    if timed_out {
        return Ok(ToolResult::error(format!("fpga_program: timed out after {timeout}s")));
    }

    let stdout = String::from_utf8_lossy(&stdout_buf.lock().unwrap()).to_string();
    let stderr = String::from_utf8_lossy(&stderr_buf.lock().unwrap()).to_string();
    let exit_ok = child.wait().map(|s| s.success()).unwrap_or(false);
    let success = exit_ok && stdout.contains("PROGRAMMED:");

    // Extract the programmed device/target line for a clean summary.
    let programmed_line = stdout.lines()
        .find(|l| l.starts_with("PROGRAMMED:"))
        .unwrap_or("")
        .to_owned();

    Ok(ToolResult::json(&json!({
        "success":     success,
        "bitfile":     bitfile,
        "programmed":  programmed_line,
        "duration_ms": duration_ms,
        "output":      stdout,
        "stderr":      if stderr.is_empty() { Value::Null } else { Value::String(stderr) },
    })))
}

// ── synth_report ──────────────────────────────────────────────────────────────

/// Parse Vivado timing_summary.rpt and utilization.rpt into structured JSON.
/// Auto-locates report files under a project directory, or accepts explicit paths.
pub fn synth_report(args: &Value) -> Result<ToolResult, String> {
    let project_dir    = args["project_dir"].as_str();
    let timing_rpt     = args["timing_rpt"].as_str();
    let utilization_rpt = args["utilization_rpt"].as_str();

    // Resolve report paths
    let timing_path = if let Some(p) = timing_rpt {
        Some(PathBuf::from(p))
    } else if let Some(dir) = project_dir {
        find_report(dir, "timing_summary")
    } else {
        None
    };

    let util_path = if let Some(p) = utilization_rpt {
        Some(PathBuf::from(p))
    } else if let Some(dir) = project_dir {
        find_report(dir, "utilization")
    } else {
        None
    };

    if timing_path.is_none() && util_path.is_none() {
        return Err("synth_report: provide 'project_dir', 'timing_rpt', or 'utilization_rpt'".into());
    }

    let timing_result = timing_path.as_ref().map(|p| {
        std::fs::read_to_string(p)
            .map(|content| parse_timing_report(&content))
            .unwrap_or_else(|e| json!({ "error": format!("read timing rpt: {e}") }))
    });

    let util_result = util_path.as_ref().map(|p| {
        std::fs::read_to_string(p)
            .map(|content| parse_utilization_report(&content))
            .unwrap_or_else(|e| json!({ "error": format!("read utilization rpt: {e}") }))
    });

    Ok(ToolResult::json(&json!({
        "timing_report":     timing_result,
        "utilization_report": util_result,
        "timing_rpt_path":   timing_path.map(|p| p.display().to_string()),
        "util_rpt_path":     util_path.map(|p| p.display().to_string()),
    })))
}

/// Walk <project_dir>/**/runs/**/ looking for a .rpt file whose name contains `keyword`.
fn find_report(project_dir: &str, keyword: &str) -> Option<PathBuf> {
    let root = Path::new(project_dir);
    find_rpt_recursive(root, keyword)
}

fn find_rpt_recursive(dir: &Path, keyword: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<PathBuf> = None;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            if let Some(p) = find_rpt_recursive(&path, keyword) {
                // Prefer deeper/newer — take most-recently-modified
                match &best {
                    None => best = Some(p),
                    Some(prev) => {
                        let p_time = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
                        let prev_time = std::fs::metadata(prev).and_then(|m| m.modified()).ok();
                        if p_time > prev_time { best = Some(p); }
                    }
                }
            }
        } else if path.extension().map(|e| e == "rpt").unwrap_or(false) {
            let fname = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            if fname.contains(keyword) {
                match &best {
                    None => best = Some(path),
                    Some(prev) => {
                        let p_time = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                        let prev_time = std::fs::metadata(prev).and_then(|m| m.modified()).ok();
                        if p_time > prev_time { best = Some(path); }
                    }
                }
            }
        }
    }
    best
}

/// Parse a Vivado timing_summary.rpt. Extracts:
///   - WNS (worst negative slack), TNS, failing endpoints per path group
///   - Setup/hold summary table rows
fn parse_timing_report(content: &str) -> Value {
    let mut wns: Option<f64>  = None;
    let mut tns: Option<f64>  = None;
    let mut whs: Option<f64>  = None;
    let mut ths: Option<f64>  = None;
    let mut failing_endpoints: Option<i64> = None;
    let mut timing_met = false;
    let mut path_groups: Vec<Value> = Vec::new();

    // Parse the Design Timing Summary table
    // Line pattern: "  WNS(ns)  TNS(ns) TNS Failing Endpoints  ..."
    // Followed by data lines like: "  -2.345  -45.678           12  ..."
    let mut in_summary_data = false;

    for line in content.lines() {
        let t = line.trim();

        // "All user specified timing constraints are met." or similar
        if t.contains("All user specified timing constraints are met") {
            timing_met = true;
        }
        if t.contains("timing constraints are not met") || t.contains("WNS is negative") {
            timing_met = false;
        }

        // Header detection for the summary table
        if t.contains("WNS(ns)") && t.contains("TNS(ns)") {
            in_summary_data = true;
            continue;
        }
        // Separator rows
        if in_summary_data && (t.starts_with("---") || t.is_empty()) {
            if t.is_empty() && wns.is_some() { in_summary_data = false; }
            continue;
        }

        if in_summary_data {
            let cols: Vec<&str> = t.split_whitespace().collect();
            // WNS TNS TNS_failing_eps WHS THS THS_failing_eps WPWS TPWS ...
            if cols.len() >= 6 {
                if wns.is_none() {
                    wns = cols[0].parse().ok();
                    tns = cols[1].parse().ok();
                    failing_endpoints = cols[2].parse().ok();
                    whs = cols[3].parse().ok();
                    ths = cols[4].parse().ok();
                }
            }
        }

        // Path group lines: "| clk_BUFG     | -2.345 |   -45.68 |      12 | ..."
        if t.starts_with('|') && !t.contains("Path Group") && !t.contains("---") {
            let cols: Vec<&str> = t.split('|').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            if cols.len() >= 4 {
                let name = cols[0];
                if !name.is_empty() && !name.contains("WNS") {
                    let pg_wns: Option<f64> = cols.get(1).and_then(|s| s.parse().ok());
                    let pg_tns: Option<f64> = cols.get(2).and_then(|s| s.parse().ok());
                    let pg_fail: Option<i64> = cols.get(3).and_then(|s| s.parse().ok());
                    if pg_wns.is_some() || pg_tns.is_some() {
                        path_groups.push(json!({
                            "path_group":        name,
                            "wns_ns":            pg_wns,
                            "tns_ns":            pg_tns,
                            "failing_endpoints": pg_fail,
                        }));
                    }
                }
            }
        }
    }

    // Infer timing_met if not explicitly stated
    if wns.is_none() && tns.is_none() {
        timing_met = false;
    } else if wns.map(|v| v >= 0.0).unwrap_or(false) {
        timing_met = true;
    }

    json!({
        "timing_met":         timing_met,
        "wns_ns":             wns,
        "tns_ns":             tns,
        "failing_endpoints":  failing_endpoints.unwrap_or(0),
        "whs_ns":             whs,
        "ths_ns":             ths,
        "path_groups":        path_groups,
    })
}

/// Parse a Vivado utilization.rpt. Extracts:
///   - LUT count + total + utilization %
///   - FF (Flip-Flop) count + utilization %
///   - BRAM (36K + 18K) count + %
///   - DSP48 count + %
///   - URAM, CARRY, IO counts
fn parse_utilization_report(content: &str) -> Value {
    let mut sections: HashMap<&str, (i64, i64, f64)> = HashMap::new(); // name → (used, total, pct)

    // The Slice Logic table looks like:
    // | Site Type               | Used | Fixed | Prohibited | Available | Util% |
    // |-------------------------|------|-------|------------|-----------|-------|
    // | Slice LUTs*             | 1234 |     0 |          0 |     20800 |  5.94 |
    // | Slice Registers         | 2345 |     0 |          0 |     41600 |  5.64 |

    let mut in_table = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with("+-") || t.starts_with("|--") || t.starts_with("|Site") {
            in_table = true;
            continue;
        }
        if t.is_empty() || (t.starts_with('+') && !t.starts_with("+-")) {
            if in_table && t.is_empty() { in_table = false; }
            continue;
        }
        if !in_table || !t.starts_with('|') { continue; }

        let cols: Vec<&str> = t.split('|').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        if cols.len() < 5 { continue; }

        let name = cols[0];
        let used_str = cols[1].replace(',', "");
        let avail_str = cols.get(4).unwrap_or(&"0").replace(',', "");
        let pct_str  = cols.get(5).unwrap_or(&"0").replace(',', "");

        let used:  i64 = used_str.parse().unwrap_or(0);
        let avail: i64 = avail_str.parse().unwrap_or(0);
        let pct:   f64 = pct_str.parse().unwrap_or(0.0);

        // Map Vivado row names to canonical keys
        let key: Option<&str> = if name.contains("Slice LUT") || name.contains("LUT as Logic") {
            Some("lut")
        } else if name.contains("Slice Register") || name.contains("Register as Flip Flop") {
            Some("ff")
        } else if name.contains("Block RAM Tile") || name.contains("Block RAM") && !name.contains("18K") {
            Some("bram36")
        } else if name.contains("RAMB18") || name.contains("18K") {
            Some("bram18")
        } else if name.contains("DSP48") || name.contains("DSPs") {
            Some("dsp")
        } else if name.contains("URAM") {
            Some("uram")
        } else if name.contains("CARRY") {
            Some("carry")
        } else if name.contains("Bonded IOB") || (name.contains("IO ") && !name.contains("BUF")) {
            Some("io")
        } else {
            None
        };

        if let Some(k) = key {
            sections.entry(k).or_insert((used, avail, pct));
        }
    }

    let get = |k: &str| -> Value {
        if let Some((used, avail, pct)) = sections.get(k) {
            json!({ "used": used, "available": avail, "utilization_pct": pct })
        } else {
            json!(null)
        }
    };

    // Timing met field from utilization: not present, omit
    json!({
        "lut":    get("lut"),
        "ff":     get("ff"),
        "bram36": get("bram36"),
        "bram18": get("bram18"),
        "dsp":    get("dsp"),
        "uram":   get("uram"),
        "carry":  get("carry"),
        "io":     get("io"),
    })
}

// ── waveform_query ────────────────────────────────────────────────────────────

/// Parse a VCD waveform dump. Returns signal definitions and value-change events.
/// Filter by signal name substring; limit total events with max_events.
pub fn waveform_query(args: &Value) -> Result<ToolResult, String> {
    let vcd_path   = args["vcd_file"].as_str().ok_or("waveform_query: 'vcd_file' required")?;
    let signals    = collect_str_array(args, "signals", "waveform_query").unwrap_or_default();
    let max_events = args["max_events"].as_u64().unwrap_or(500) as usize;
    let time_start = args["time_start"].as_u64();
    let time_end   = args["time_end"].as_u64();

    let content = std::fs::read_to_string(vcd_path)
        .map_err(|e| format!("waveform_query: {vcd_path}: {e}"))?;

    let (timescale, var_list, events) =
        parse_vcd(&content, &signals, max_events, time_start, time_end);

    Ok(ToolResult::json(&json!({
        "vcd_file":    vcd_path,
        "timescale":   timescale,
        "signal_count": var_list.len(),
        "signals":     var_list,
        "event_count": events.len(),
        "events":      events,
    })))
}

fn parse_vcd(
    content: &str,
    filter_signals: &[String],
    max_events: usize,
    time_start: Option<u64>,
    time_end: Option<u64>,
) -> (String, Vec<Value>, Vec<Value>) {
    // id → {name, width}
    let mut vars: HashMap<String, (String, u32)> = HashMap::new();
    let mut events: Vec<Value> = Vec::new();
    let mut current_time: u64 = 0;
    let mut in_defs = true;
    let mut timescale = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }

        // $timescale 1ns $end
        if line.starts_with("$timescale") {
            timescale = line
                .replace("$timescale", "")
                .replace("$end", "")
                .trim()
                .to_owned();
            continue;
        }

        // $var wire 8 # data [7:0] $end
        if line.starts_with("$var") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // [0]=$var [1]=kind [2]=width [3]=id [4]=name [5+]=range/$end
            if parts.len() >= 5 {
                let width: u32 = parts[2].parse().unwrap_or(1);
                let id   = parts[3].to_owned();
                let name = parts[4].to_owned();
                let include = filter_signals.is_empty()
                    || filter_signals.iter().any(|s| name.contains(s.as_str()));
                if include {
                    vars.insert(id, (name, width));
                }
            }
            continue;
        }

        if line.contains("$enddefinitions") {
            in_defs = false;
            continue;
        }
        if in_defs { continue; }

        // Timestamp: #12345
        if let Some(ts_str) = line.strip_prefix('#') {
            if let Ok(ts) = ts_str.parse::<u64>() {
                current_time = ts;
            }
            continue;
        }

        if events.len() >= max_events { continue; }

        // Time filter
        if let Some(ts) = time_start { if current_time < ts { continue; } }
        if let Some(te) = time_end   { if current_time > te { continue; } }

        // Scalar change: "0!" / "1#" / "xA" / "zA"
        if line.len() >= 2 {
            let first = line.chars().next().unwrap_or(' ');
            if matches!(first, '0' | '1' | 'x' | 'z' | 'X' | 'Z') && !line.starts_with("b") {
                let val = &line[..1];
                let id  = &line[1..];
                if let Some((name, _)) = vars.get(id) {
                    events.push(json!({
                        "time":   current_time,
                        "signal": name,
                        "value":  val,
                    }));
                }
                continue;
            }
        }

        // Vector change: "b00101010 #" or "B..."
        if line.starts_with('b') || line.starts_with('B') {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let bits = &parts[0][1..];
                let id   = parts[1];
                if let Some((name, _)) = vars.get(id) {
                    let hex = bits_to_hex(bits);
                    events.push(json!({
                        "time":   current_time,
                        "signal": name,
                        "value":  format!("0x{hex}"),
                        "binary": bits,
                    }));
                }
            }
        }
    }

    let var_list: Vec<Value> = vars
        .into_iter()
        .map(|(id, (name, width))| json!({ "id": id, "name": name, "width": width }))
        .collect();

    (timescale, var_list, events)
}

/// Convert a binary string (possibly with x/z) to hex.
fn bits_to_hex(bits: &str) -> String {
    let pad = (4 - bits.len() % 4) % 4;
    let padded = format!("{}{}", "0".repeat(pad), bits);
    padded
        .as_bytes()
        .chunks(4)
        .map(|chunk| {
            let nibble = chunk.iter().enumerate().fold(0u8, |acc, (i, &b)| {
                acc | (if b == b'1' { 1 } else { 0 }) << (3 - i)
            });
            format!("{nibble:X}")
        })
        .collect()
}

// ── fpga_serial ───────────────────────────────────────────────────────────────

/// Low-level UART send/receive for FPGA communication.
/// send: hex string e.g. "52 01" or "5201". read_bytes: how many to read back.
pub fn fpga_serial(args: &Value) -> Result<ToolResult, String> {
    let port_arg   = args["port"].as_str().unwrap_or("").to_owned();
    let baud       = args["baud"].as_u64().unwrap_or(921600);
    let send_hex   = args["send"].as_str().unwrap_or("");
    let read_n     = args["read_bytes"].as_u64().unwrap_or(0) as usize;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(500);

    let port = if port_arg.is_empty() { find_uart_port()? } else { port_arg };
    let send_bytes = parse_hex_bytes(send_hex)?;
    let mut f = open_serial(&port, baud, timeout_ms)?;

    if !send_bytes.is_empty() {
        use std::io::Write;
        f.write_all(&send_bytes).map_err(|e| format!("fpga_serial: write: {e}"))?;
    }

    let received = if read_n > 0 {
        uart_read_n(&mut f, read_n, timeout_ms)?
    } else {
        Vec::new()
    };

    let recv_hex: Vec<String> = received.iter().map(|b| format!("{b:02X}")).collect();
    Ok(ToolResult::json(&json!({
        "port":           port,
        "baud":           baud,
        "sent_bytes":     send_bytes.len(),
        "received_count": received.len(),
        "received_hex":   recv_hex,
        "received_ascii": String::from_utf8_lossy(&received),
    })))
}

// ── fpga_tcfp_status ──────────────────────────────────────────────────────────

/// Read live status from the TCFP processor over UART.
/// Reads REG_STATUS (0x01) and REG_STEP_COUNT (0x02).
/// STATUS layout: bit0=busy, bit1=converged, bit2=done_sticky
pub fn fpga_tcfp_status(args: &Value) -> Result<ToolResult, String> {
    let port_arg = args["port"].as_str().unwrap_or("").to_owned();
    let baud     = args["baud"].as_u64().unwrap_or(921600);
    let port = if port_arg.is_empty() { find_uart_port()? } else { port_arg };
    let mut f = open_serial(&port, baud, 500)?;

    let status     = uart_read_reg(&mut f, 0x01)?;
    let step_count = uart_read_reg(&mut f, 0x02)?;

    Ok(ToolResult::json(&json!({
        "port":       port,
        "busy":       (status & 0x01) != 0,
        "converged":  (status & 0x02) != 0,
        "done":       (status & 0x04) != 0,
        "step_count": step_count & 0xFFFF,
        "raw_status": format!("0x{status:08X}"),
    })))
}

// ── fpga_tcfp_tile_read ───────────────────────────────────────────────────────

/// Read tile state vectors from the TCFP 2×2 array via UART 'Q' command.
/// tiles: [{row, col}] — defaults to all 4 tiles.
/// Returns phase/field/coupling/bias as raw int32 + float (Q16.16).
pub fn fpga_tcfp_tile_read(args: &Value) -> Result<ToolResult, String> {
    let port_arg = args["port"].as_str().unwrap_or("").to_owned();
    let baud     = args["baud"].as_u64().unwrap_or(921600);
    let port = if port_arg.is_empty() { find_uart_port()? } else { port_arg };

    let tile_addrs: Vec<(u8, u8)> = if let Some(arr) = args["tiles"].as_array() {
        arr.iter()
            .filter_map(|t| Some((t["row"].as_u64()? as u8, t["col"].as_u64()? as u8)))
            .collect()
    } else {
        vec![(0,0),(0,1),(1,0),(1,1)]   // default: all 4 tiles of 2×2
    };

    if tile_addrs.is_empty() {
        return Err("fpga_tcfp_tile_read: no tile addresses".into());
    }

    let count = tile_addrs.len() as u8;
    // addr encoding: {row[3:0], col[3:0]}
    let mut cmd = vec![0x51u8, count];
    cmd.extend(tile_addrs.iter().map(|(r, c)| (r << 4) | (c & 0x0F)));

    let mut f = open_serial(&port, baud, 1000)?;
    use std::io::Write;
    f.write_all(&cmd).map_err(|e| format!("fpga_tcfp_tile_read: write: {e}"))?;

    // Response: ACK(1) + count * 16 bytes (phase|field|coupling|bias, each 32-bit BE)
    let resp = uart_read_n(&mut f, 1 + count as usize * 16, 1000)?;

    if resp.first().copied().unwrap_or(0) != 0x06 {
        let ack = resp.first().copied().unwrap_or(0);
        return Ok(ToolResult::json(&json!({
            "success": false,
            "error":   format!("expected ACK 0x06, got 0x{ack:02X}"),
            "raw":     resp.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>(),
        })));
    }

    let payload = &resp[1..];
    let scale = 65536.0f64;   // Q16.16

    let tiles: Vec<serde_json::Value> = tile_addrs.iter().enumerate().map(|(i, (row, col))| {
        let base = i * 16;
        if base + 16 > payload.len() {
            return json!({ "row": row, "col": col, "error": "truncated" });
        }
        let phase    = i32::from_be_bytes(payload[base   ..base+ 4].try_into().unwrap());
        let field    = i32::from_be_bytes(payload[base+ 4..base+ 8].try_into().unwrap());
        let coupling = i32::from_be_bytes(payload[base+ 8..base+12].try_into().unwrap());
        let bias     = i32::from_be_bytes(payload[base+12..base+16].try_into().unwrap());
        json!({
            "row": row, "col": col,
            "addr": format!("0x{:02X}", (row << 4) | col),
            "phase":    { "raw": phase,    "f": phase    as f64 / scale },
            "field":    { "raw": field,    "f": field    as f64 / scale },
            "coupling": { "raw": coupling, "f": coupling as f64 / scale },
            "bias":     { "raw": bias,     "f": bias     as f64 / scale },
        })
    }).collect();

    Ok(ToolResult::json(&json!({
        "success":    true,
        "port":       port,
        "tile_count": tiles.len(),
        "tiles":      tiles,
    })))
}

// ── serial helpers ────────────────────────────────────────────────────────────

fn find_uart_port() -> Result<String, String> {
    let mut ports: Vec<String> = std::fs::read_dir("/dev")
        .map_err(|e| format!("find_uart_port: /dev: {e}"))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let s = n.to_string_lossy();
            s.starts_with("ttyUSB") || s.starts_with("ttyACM")
        })
        .map(|e| e.path().to_string_lossy().to_string())
        .collect();
    ports.sort();
    // Basys3: ttyUSB0=JTAG, ttyUSB1=UART — take last
    ports.last().cloned()
        .ok_or_else(|| "find_uart_port: no /dev/ttyUSB* found".to_owned())
}

fn open_serial(port: &str, baud: u64, timeout_ms: u64) -> Result<std::fs::File, String> {
    let vtime = ((timeout_ms / 100).max(1).min(255)).to_string();
    let out = std::process::Command::new("stty")
        .args(["-F", port, &baud.to_string(),
               "raw", "cs8", "-parenb", "-cstopb", "-echo", "min", "0", "time", &vtime])
        .output()
        .map_err(|e| format!("open_serial: stty: {e}"))?;
    if !out.status.success() {
        return Err(format!("open_serial: stty failed on {port}: {}",
            String::from_utf8_lossy(&out.stderr)));
    }
    std::fs::OpenOptions::new().read(true).write(true)
        .open(port)
        .map_err(|e| format!("open_serial: open {port}: {e}"))
}

fn uart_read_n(f: &mut std::fs::File, n: usize, timeout_ms: u64) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    let mut got = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while got < n && std::time::Instant::now() < deadline {
        match f.read(&mut buf[got..]) {
            Ok(0) | Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Ok(k) => got += k,
        }
    }
    buf.truncate(got);
    Ok(buf)
}

fn uart_read_reg(f: &mut std::fs::File, addr: u8) -> Result<u32, String> {
    use std::io::Write;
    f.write_all(&[0x52, addr]).map_err(|e| format!("uart_read_reg 0x{addr:02X}: {e}"))?;
    let bytes = uart_read_n(f, 4, 500)?;
    if bytes.len() < 4 {
        return Err(format!("uart_read_reg 0x{addr:02X}: timeout ({}/4 bytes)", bytes.len()));
    }
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if clean.len() % 2 != 0 {
        return Err(format!("parse_hex_bytes: odd nibble count in '{s}'"));
    }
    clean.as_bytes().chunks(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16)
            .map_err(|e| format!("parse_hex_bytes: {e}")))
        .collect()
}

fn collect_str_array(args: &Value, key: &str, tool: &str) -> Result<Vec<String>, String> {
    match &args[key] {
        Value::Array(arr) => Ok(arr.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()),
        Value::Null => Err(format!("{tool}: '{key}' array required")),
        _ => Err(format!("{tool}: '{key}' must be an array")),
    }
}

fn collect_optional_str_array(args: &Value, key: &str, tool: &str) -> Result<Vec<String>, String> {
    match &args[key] {
        Value::Array(arr) => Ok(arr.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()),
        Value::Null => Ok(Vec::new()),
        _ => Err(format!("{tool}: '{key}' must be an array")),
    }
}

fn apply_iverilog_libraries(
    cmd: &mut std::process::Command,
    libraries: &[String],
    tool: &str,
) -> Result<Vec<String>, String> {
    let mut resolved = Vec::new();

    for lib in libraries {
        match lib.as_str() {
            "xilinx_unisims" | "xilinx_unisims_7series" => {
                if !Path::new(XILINX_UNISIMS_DIR).exists() {
                    return Err(format!("{tool}: missing Xilinx unisims dir: {XILINX_UNISIMS_DIR}"));
                }
                if !Path::new(XILINX_GLBL_V).exists() {
                    return Err(format!("{tool}: missing Xilinx glbl.v: {XILINX_GLBL_V}"));
                }
                cmd.args(["-y", XILINX_UNISIMS_DIR]);
                cmd.arg(XILINX_GLBL_V);
                resolved.push(format!("{} (dir:{}, glbl:{})", lib, XILINX_UNISIMS_DIR, XILINX_GLBL_V));
            }
            "xilinx_xpm_memory" => {
                if !Path::new(XILINX_XPM_MEMORY_SV).exists() {
                    return Err(format!("{tool}: missing Xilinx XPM source: {XILINX_XPM_MEMORY_SV}"));
                }
                cmd.arg(XILINX_XPM_MEMORY_SV);
                resolved.push(format!("{} (file:{})", lib, XILINX_XPM_MEMORY_SV));
            }
            other => {
                return Err(format!(
                    "{tool}: unsupported library preset '{other}'. supported: xilinx_unisims, xilinx_unisims_7series, xilinx_xpm_memory"
                ));
            }
        }
    }

    Ok(resolved)
}

fn uses_xilinx_glbl(libraries: &[String]) -> bool {
    libraries.iter().any(|lib| matches!(lib.as_str(), "xilinx_unisims" | "xilinx_unisims_7series"))
}

fn xsim_compile_flags(standard: &str) -> Vec<&'static str> {
    let mut flags = vec!["--nolog", "--relax"];
    if matches!(standard, "2012" | "sv") {
        flags.push("--sv");
    }
    flags
}

fn xsim_compile_files(files: &[String], libraries: &[String]) -> Result<Vec<String>, String> {
    let mut compile_files = files.to_vec();
    if uses_xsim_glbl(libraries) {
        if !Path::new(XILINX_GLBL_V).exists() {
            return Err(format!("xsim_elab: missing Xilinx glbl.v: {XILINX_GLBL_V}"));
        }
        compile_files.push(XILINX_GLBL_V.to_owned());
    }
    Ok(compile_files)
}

fn resolve_xsim_libraries(libraries: &[String], tool: &str) -> Result<Vec<String>, String> {
    let mut resolved = Vec::new();

    for lib in libraries {
        match lib.as_str() {
            "xilinx_unisims" | "xilinx_unisims_7series" => {
                resolved.push(format!("{lib} (xelab -L unisims_ver, glbl:{XILINX_GLBL_V})"));
            }
            "xilinx_xpm_cdc" | "xilinx_xpm_fifo" | "xilinx_xpm_memory" => {
                let mut detail = format!("{lib} (xelab -L xpm");
                if lib == "xilinx_xpm_cdc" {
                    detail.push_str(&format!(", glbl:{XILINX_GLBL_V}"));
                }
                detail.push(')');
                resolved.push(detail);
            }
            other => {
                return Err(format!(
                    "{tool}: unsupported library preset '{other}'. supported: xilinx_unisims, xilinx_unisims_7series, xilinx_xpm_cdc, xilinx_xpm_fifo, xilinx_xpm_memory"
                ));
            }
        }
    }

    Ok(resolved)
}

fn xsim_search_libraries(libraries: &[String]) -> Vec<String> {
    let mut libs = Vec::new();

    for lib in libraries {
        let name = match lib.as_str() {
            "xilinx_unisims" | "xilinx_unisims_7series" => Some("unisims_ver"),
            "xilinx_xpm_cdc" | "xilinx_xpm_fifo" | "xilinx_xpm_memory" => Some("xpm"),
            _ => None,
        };
        if let Some(name) = name {
            if !libs.iter().any(|existing| existing == name) {
                libs.push(name.to_owned());
            }
        }
    }

    libs
}

fn xsim_elab_tops(top: &str, libraries: &[String]) -> Vec<String> {
    let mut tops = vec![top.to_owned()];
    if uses_xsim_glbl(libraries) {
        tops.push("glbl".to_owned());
    }
    tops
}

fn uses_xsim_glbl(libraries: &[String]) -> bool {
    libraries.iter().any(|lib| matches!(lib.as_str(), "xilinx_unisims" | "xilinx_unisims_7series" | "xilinx_xpm_cdc"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    fn xsim_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn parse_tool_json(result: ToolResult) -> Value {
        serde_json::from_str(&result.content[0].text).expect("tool result json")
    }

    #[test]
    fn verilog_lint_accepts_xilinx_unisims_for_mmcm_design() {
        let tmp = std::env::temp_dir().join(format!("ferrite_mcp_mmcm_{}_{}.sv", std::process::id(), 1));
        fs::write(&tmp, r#"
`default_nettype none
module mmcm_top(
    input  wire clk,
    input  wire rst,
    output wire out_clk,
    output wire locked
);
    wire clkfb;
    wire clkfb_bufg;
    wire clk_mmcm;

    MMCME2_BASE #(
        .CLKIN1_PERIOD(10.0),
        .DIVCLK_DIVIDE(1),
        .CLKFBOUT_MULT_F(10.0),
        .CLKOUT0_DIVIDE_F(20.0)
    ) u_mmcm (
        .CLKFBOUT(clkfb), .CLKFBOUTB(), .CLKOUT0(clk_mmcm), .CLKOUT0B(),
        .CLKOUT1(), .CLKOUT1B(), .CLKOUT2(), .CLKOUT2B(), .CLKOUT3(), .CLKOUT3B(),
        .CLKOUT4(), .CLKOUT5(), .CLKOUT6(), .LOCKED(locked),
        .CLKFBIN(clkfb_bufg), .CLKIN1(clk), .PWRDWN(1'b0), .RST(rst)
    );

    BUFG u_fb  (.I(clkfb),    .O(clkfb_bufg));
    BUFG u_out (.I(clk_mmcm), .O(out_clk));
endmodule
`default_nettype wire
"#).expect("write temp sv");

        let args = json!({
            "files": [tmp.to_string_lossy().to_string()],
            "top": "mmcm_top",
            "standard": "2012",
            "libraries": ["xilinx_unisims_7series"]
        });

        let result = verilog_lint(&args).expect("verilog_lint should return");
        let payload = parse_tool_json(result);
        let _ = fs::remove_file(&tmp);

        assert_eq!(payload["success"], Value::Bool(true), "payload={payload}");
        assert!(payload["error_count"].as_u64().unwrap_or(1) == 0, "payload={payload}");
        let libs = payload["libraries_used"].as_array().expect("libraries_used array");
        assert!(libs.iter().any(|v| v.as_str().unwrap_or("").contains("xilinx_unisims_7series")));
    }

    #[test]
    fn xsim_elab_accepts_xilinx_xpm_cdc_single() {
        let _guard = xsim_test_lock().lock().expect("xsim test lock");
        let tmp = std::env::temp_dir().join(format!("ferrite_mcp_xsim_xpm_cdc_{}_{}.sv", std::process::id(), 1));
        fs::write(&tmp, r#"
`default_nettype none
module xpm_cdc_top(
    input  wire src_clk,
    input  wire src_in,
    input  wire dest_clk,
    output wire dest_out
);
    xpm_cdc_single #(
        .DEST_SYNC_FF(2),
        .SRC_INPUT_REG(0)
    ) u_cdc (
        .src_clk(src_clk),
        .src_in(src_in),
        .dest_clk(dest_clk),
        .dest_out(dest_out)
    );
endmodule
`default_nettype wire
"#).expect("write temp sv");

        let args = json!({
            "files": [tmp.to_string_lossy().to_string()],
            "top": "xpm_cdc_top",
            "standard": "2012",
            "libraries": ["xilinx_xpm_cdc"]
        });

        let result = xsim_elab(&args).expect("xsim_elab should return");
        let payload = parse_tool_json(result);
        let _ = fs::remove_file(&tmp);

        assert_eq!(payload["success"], Value::Bool(true), "payload={payload}");
        let libs = payload["libraries_used"].as_array().expect("libraries_used array");
        assert!(libs.iter().any(|v| v.as_str().unwrap_or("").contains("xilinx_xpm_cdc")));
    }

    #[test]
    fn xsim_elab_accepts_xilinx_xpm_fifo_sync() {
        let _guard = xsim_test_lock().lock().expect("xsim test lock");
        let tmp = std::env::temp_dir().join(format!("ferrite_mcp_xsim_xpm_fifo_{}_{}.sv", std::process::id(), 1));
        fs::write(&tmp, r#"
`default_nettype none
module xpm_fifo_top(
    input  wire       clk,
    input  wire       rst,
    input  wire       wr_en,
    input  wire       rd_en,
    input  wire [7:0] din,
    output wire [7:0] dout,
    output wire       full,
    output wire       empty
);
    wire prog_full;
    wire [0:0] wr_data_count;
    wire overflow;
    wire wr_rst_busy;
    wire almost_full;
    wire wr_ack;
    wire prog_empty;
    wire [0:0] rd_data_count;
    wire underflow;
    wire rd_rst_busy;
    wire almost_empty;
    wire data_valid;
    wire sbiterr;
    wire dbiterr;

    xpm_fifo_sync #(
        .FIFO_WRITE_DEPTH(16),
        .WRITE_DATA_WIDTH(8),
        .READ_DATA_WIDTH(8),
        .WR_DATA_COUNT_WIDTH(1),
        .RD_DATA_COUNT_WIDTH(1)
    ) u_fifo (
        .sleep(1'b0),
        .rst(rst),
        .wr_clk(clk),
        .wr_en(wr_en),
        .din(din),
        .full(full),
        .prog_full(prog_full),
        .wr_data_count(wr_data_count),
        .overflow(overflow),
        .wr_rst_busy(wr_rst_busy),
        .almost_full(almost_full),
        .wr_ack(wr_ack),
        .rd_en(rd_en),
        .dout(dout),
        .empty(empty),
        .prog_empty(prog_empty),
        .rd_data_count(rd_data_count),
        .underflow(underflow),
        .rd_rst_busy(rd_rst_busy),
        .almost_empty(almost_empty),
        .data_valid(data_valid),
        .injectsbiterr(1'b0),
        .injectdbiterr(1'b0),
        .sbiterr(sbiterr),
        .dbiterr(dbiterr)
    );
endmodule
`default_nettype wire
"#).expect("write temp sv");

        let args = json!({
            "files": [tmp.to_string_lossy().to_string()],
            "top": "xpm_fifo_top",
            "standard": "2012",
            "libraries": ["xilinx_xpm_fifo"]
        });

        let result = xsim_elab(&args).expect("xsim_elab should return");
        let payload = parse_tool_json(result);
        let _ = fs::remove_file(&tmp);

        assert_eq!(payload["success"], Value::Bool(true), "payload={payload}");
        let libs = payload["libraries_used"].as_array().expect("libraries_used array");
        assert!(libs.iter().any(|v| v.as_str().unwrap_or("").contains("xilinx_xpm_fifo")));
    }
}
