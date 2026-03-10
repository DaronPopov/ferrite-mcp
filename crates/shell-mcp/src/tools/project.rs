//! Project/chip awareness tools (Tier 1).
//!
//! Tools:
//!   project_context      — auto-detect workspace type from path
//!   chip_status          — scan all chips in processor_lab
//!   chip_build_pipeline  — full RTL flow for one chip
//!   board_status         — detect connected boards + serial ports
//!   fpga_monitor         — stream UART output as background job

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use crate::job_store::JobStore;
use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::execution::run;
use crate::tools::state::resolve_or_cwd;

// ── project_context ───────────────────────────────────────────────────────────

pub fn project_context(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let start = resolve_or_cwd(state, args["path"].as_str())?;

    // Walk up to find project root
    let mut dir = start.clone();
    loop {
        if let Some((name, ptype)) = detect_project_at(&dir) {
            let active_targets = collect_active_targets(&dir, &ptype);
            let context_hints = context_hints_for(&ptype);
            return Ok(ToolResult::json(&json!({
                "project_name": name,
                "project_type": ptype,
                "root":         dir.display().to_string(),
                "context_hints": context_hints,
                "active_targets": active_targets,
            })));
        }
        match dir.parent() {
            Some(p) => dir = p.to_owned(),
            None => break,
        }
    }

    Ok(ToolResult::json(&json!({
        "project_name": null,
        "project_type": "unknown",
        "root":         start.display().to_string(),
        "context_hints": [],
        "active_targets": [],
        "note": "No known project root detected in path hierarchy",
    })))
}

fn detect_project_at(dir: &Path) -> Option<(String, String)> {
    let name = dir.file_name()?.to_string_lossy().into_owned();

    // Exact-name matches first
    if name == "processor_lab" {
        return Some((name, "rtl_lab".to_owned()));
    }
    if name == "ferrite-mcp" {
        return Some((name, "mcp_server".to_owned()));
    }

    // Prefix matches
    if name.starts_with("ferrite") {
        return Some((name, "cuda_runtime".to_owned()));
    }

    // Parent-name matches (e.g. we're inside verilogchill/ferrite-mcp)
    if dir.join("Cargo.toml").exists() {
        // Check if this is a known Rust project type
        if dir.parent().and_then(|p| p.file_name())
            .map(|n| n == "verilogchill").unwrap_or(false)
        {
            return Some((name, "rtl_tcfp".to_owned()));
        }
        return Some((name, "rust_project".to_owned()));
    }

    // Contains chips/ directory — it's an RTL lab
    if dir.join("chips").exists() && dir.join("chips").is_dir() {
        return Some((name, "rtl_lab".to_owned()));
    }

    None
}

fn collect_active_targets(root: &Path, ptype: &str) -> Vec<Value> {
    let mut targets = Vec::new();

    if ptype == "rtl_lab" {
        let chips_dir = root.join("chips");
        if let Ok(entries) = std::fs::read_dir(&chips_dir) {
            for entry in entries.flatten() {
                let chip = entry.file_name().to_string_lossy().into_owned();
                // Look for .bit files in build/
                let build_dir = entry.path().join("build");
                if let Ok(builds) = std::fs::read_dir(&build_dir) {
                    for b in builds.flatten() {
                        if b.path().extension().map(|e| e == "bit").unwrap_or(false) {
                            targets.push(json!({
                                "chip": chip,
                                "bit": b.path().display().to_string(),
                            }));
                            break;
                        }
                    }
                }
            }
        }
    } else if ptype == "rust_project" || ptype == "mcp_server" || ptype == "rtl_tcfp" {
        // List workspace crates from Cargo.toml
        if let Ok(cargo_src) = std::fs::read_to_string(root.join("Cargo.toml")) {
            for line in cargo_src.lines() {
                let line = line.trim();
                if line.starts_with("\"crates/") || line.starts_with("'crates/") {
                    let name = line.trim_matches(|c: char| c == '"' || c == '\'' || c == ',');
                    targets.push(json!({ "crate": name }));
                }
            }
        }
    }

    targets
}

fn context_hints_for(ptype: &str) -> Vec<&'static str> {
    match ptype {
        "rtl_lab"      => vec!["Use chip_status to see all chips", "Use chip_build_pipeline to run RTL flow", "Use board_status to detect hardware"],
        "mcp_server"   => vec!["cargo build --release -p shell-mcp", "cp target/release/ferrite ~/.cargo/bin/ferrite"],
        "rtl_tcfp"     => vec!["Use vivado_tcl for synthesis", "Use fpga_program to flash"],
        "cuda_runtime" => vec!["Use build_check for CUDA compilation", "Use ncu_profile to benchmark"],
        "rust_project" => vec!["Use cargo_tree to inspect workspace", "Use test_run for tests"],
        _              => vec![],
    }
}

// ── chip_status ───────────────────────────────────────────────────────────────

pub fn chip_status(args: &Value) -> Result<ToolResult, String> {
    let lab_path = args["lab_path"].as_str()
        .map(expand_tilde)
        .unwrap_or_else(|| expand_tilde("~/processor_lab"));

    let chips_dir = lab_path.join("chips");
    if !chips_dir.exists() {
        return Err(format!("chip_status: chips dir not found at {}", chips_dir.display()));
    }

    let mut chips = Vec::new();
    let entries = std::fs::read_dir(&chips_dir)
        .map_err(|e| format!("chip_status: read_dir: {e}"))?;

    for entry in entries.flatten() {
        let chip_path = entry.path();
        if !chip_path.is_dir() { continue; }
        let chip = chip_path.file_name().unwrap_or_default().to_string_lossy().into_owned();

        // Find .bit file
        let build_dir = chip_path.join("build");
        let (bit_built, bit_path, last_built) = find_bit_file(&build_dir);

        // Check sim results
        let sim_ok = check_sim_results(&chip_path);

        // Parse synth report if exists
        let (wns, lut_pct) = parse_synth_report_quick(&build_dir);

        chips.push(json!({
            "chip":       chip,
            "sim_ok":     sim_ok,
            "bit_built":  bit_built,
            "bit_path":   bit_path,
            "wns":        wns,
            "lut_pct":    lut_pct,
            "last_built": last_built,
        }));
    }

    // Sort chips alphabetically
    chips.sort_by(|a, b| {
        a["chip"].as_str().unwrap_or("").cmp(b["chip"].as_str().unwrap_or(""))
    });

    Ok(ToolResult::json(&json!({
        "lab_path": lab_path.display().to_string(),
        "chip_count": chips.len(),
        "chips": chips,
    })))
}

fn find_bit_file(build_dir: &Path) -> (bool, Option<String>, Option<String>) {
    if !build_dir.exists() {
        return (false, None, None);
    }
    if let Ok(entries) = std::fs::read_dir(build_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().map(|e| e == "bit").unwrap_or(false) {
                let path_str = p.display().to_string();
                let mtime = p.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs().to_string());
                return (true, Some(path_str), mtime);
            }
        }
    }
    (false, None, None)
}

fn check_sim_results(chip_path: &Path) -> Option<bool> {
    // Walk ip/*/sim/ looking for results.xml or recent .log files
    let ip_dir = chip_path.join("ip");
    if !ip_dir.exists() { return None; }

    let Ok(ip_entries) = std::fs::read_dir(&ip_dir) else { return None; };
    for ip_entry in ip_entries.flatten() {
        let sim_dir = ip_entry.path().join("sim");
        if !sim_dir.is_dir() { continue; }

        // Check results.xml
        let results_xml = sim_dir.join("results.xml");
        if results_xml.exists() {
            if let Ok(content) = std::fs::read_to_string(&results_xml) {
                let failures = content.contains("failure") || content.contains("error");
                return Some(!failures);
            }
        }

        // Check for recent .log files
        if let Ok(log_entries) = std::fs::read_dir(&sim_dir) {
            for log in log_entries.flatten() {
                if log.path().extension().map(|e| e == "log").unwrap_or(false) {
                    if let Ok(content) = std::fs::read_to_string(log.path()) {
                        // cocotb: look for PASSED/FAILED summary
                        if content.contains("PASSED") && !content.contains("FAILED") {
                            return Some(true);
                        } else if content.contains("FAILED") {
                            return Some(false);
                        }
                    }
                }
            }
        }
    }
    None
}

fn parse_synth_report_quick(build_dir: &Path) -> (Option<f64>, Option<f64>) {
    if !build_dir.exists() { return (None, None); }

    // Look for timing_summary.rpt
    let mut wns: Option<f64> = None;
    let mut lut_pct: Option<f64> = None;

    if let Ok(entries) = std::fs::read_dir(build_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();

            if name.contains("timing_summary") && name.ends_with(".rpt") {
                if let Ok(content) = std::fs::read_to_string(&p) {
                    // Parse WNS from "WNS(ns)  TNS(ns)..." table
                    for line in content.lines() {
                        let line = line.trim();
                        if line.starts_with("WNS") { continue; }
                        // Try to parse as a data row — first token should be a float
                        let first = line.split_whitespace().next().unwrap_or("");
                        if let Ok(v) = first.parse::<f64>() {
                            wns = Some(v);
                            break;
                        }
                    }
                }
            }

            if name.contains("utilization") && name.ends_with(".rpt") {
                if let Ok(content) = std::fs::read_to_string(&p) {
                    // Look for LUT row: "| Slice LUTs*  |   <n>  |   0  | <total> | <pct> |"
                    for line in content.lines() {
                        if line.contains("Slice LUT") || line.contains("LUT as Logic") {
                            let parts: Vec<&str> = line.split('|').collect();
                            // parts[5] is typically the percentage
                            if parts.len() >= 6 {
                                let pct_str = parts[5].trim().trim_end_matches('%');
                                if let Ok(v) = pct_str.parse::<f64>() {
                                    lut_pct = Some(v);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    (wns, lut_pct)
}

// ── chip_build_pipeline ───────────────────────────────────────────────────────

pub fn chip_build_pipeline(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let chip = args["chip"].as_str().ok_or("chip_build_pipeline: 'chip' is required")?;
    let lab_path = args["lab_path"].as_str()
        .map(expand_tilde)
        .unwrap_or_else(|| expand_tilde("~/processor_lab"));
    let board = args["board"].as_str().unwrap_or("basys3");
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);

    let steps: Vec<&str> = args["steps"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_else(|| vec!["lint", "sim", "synth", "program"]);

    let chip_path = lab_path.join("chips").join(chip);
    if !chip_path.exists() {
        return Err(format!("chip_build_pipeline: chip '{}' not found at {}", chip, chip_path.display()));
    }

    if dry_run {
        let plan: Vec<Value> = steps.iter().map(|s| {
            json!({ "step": s, "cmd": build_step_cmd(chip, &chip_path, board, s) })
        }).collect();
        return Ok(ToolResult::json(&json!({
            "chip": chip,
            "board": board,
            "dry_run": true,
            "plan": plan,
        })));
    }

    let mut step_results = Vec::new();
    let mut overall_success = true;

    for step in &steps {
        let cmd = build_step_cmd(chip, &chip_path, board, step);
        if cmd.is_empty() {
            step_results.push(json!({ "step": step, "skipped": true, "reason": "unsupported step" }));
            continue;
        }

        let result = if *step == "synth" {
            // Synth runs asynchronously via bg job
            let label = format!("{}_{}_synth", chip, board);
            let job = store.spawn(&cmd, chip_path.clone(), Some(&label), vec![])?;
            json!({
                "step": step,
                "bg_job_id": job.job_id,
                "cmd": cmd,
                "note": "Synthesis running in background — use bg_wait to block or bg_status to poll",
            })
        } else {
            let cwd = chip_path.clone();
            let raw = run(&cmd, &cwd, &[], "", Duration::from_secs(300));
            let success = raw["success"].as_bool().unwrap_or(false);
            if !success { overall_success = false; }
            json!({
                "step": step,
                "success": success,
                "cmd": cmd,
                "stdout": raw["stdout"],
                "stderr": raw["stderr"],
                "duration_ms": raw["duration_ms"],
            })
        };

        let was_success = result["success"].as_bool().unwrap_or(true);
        step_results.push(result);

        // Stop on failure (except synth which is async)
        if !was_success && overall_success == false {
            break;
        }
    }

    Ok(ToolResult::json(&json!({
        "chip":            chip,
        "board":           board,
        "overall_success": overall_success,
        "steps":           step_results,
    })))
}

fn build_step_cmd(chip: &str, chip_path: &Path, board: &str, step: &str) -> String {
    match step {
        "lint" => {
            // iverilog -tnull on all .sv/.v in ip/*/rtl/
            let rtl_glob = chip_path.join("ip").join("*").join("rtl");
            format!(
                "find {} -name '*.sv' -o -name '*.v' | xargs iverilog -tnull -g2012 2>&1",
                rtl_glob.display()
            )
        }
        "sim" => {
            // cocotb via pytest in each ip/*/sim/
            let sim_base = chip_path.join("ip");
            format!(
                "for sim_dir in {}/*/sim; do [ -d \"$sim_dir\" ] && (cd \"$sim_dir\" && python -m pytest -x -q 2>&1); done",
                sim_base.display()
            )
        }
        "synth" => {
            // Vivado batch TCL
            let tcl = chip_path
                .join("top")
                .join(board)
                .join("tcl")
                .join(format!("build_{chip}.tcl"));
            let vivado = "/opt/2025.2/Vivado/bin/vivado";
            format!("{vivado} -mode batch -source {}", tcl.display())
        }
        "program" => {
            // Find .bit and program
            let build_dir = chip_path.join("build");
            format!(
                "ls {}/{}*.bit 2>/dev/null | head -1 | xargs -I{{}} /opt/2025.2/Vivado/bin/vivado -mode batch -source /dev/stdin <<'EOF'\nopen_hw_manager\nconnect_hw_server\nopen_hw_target\nset_property PROGRAM.FILE {{}} [current_hw_device]\nprogram_hw_devices\nEOF",
                build_dir.display(),
                chip
            )
        }
        "validate" => {
            // Run cocotb again post-program as smoke test
            let sim_base = chip_path.join("ip");
            format!(
                "for sim_dir in {}/*/sim; do [ -d \"$sim_dir\" ] && (cd \"$sim_dir\" && python -m pytest -x -q 2>&1); done",
                sim_base.display()
            )
        }
        _ => String::new(),
    }
}

// ── board_status ──────────────────────────────────────────────────────────────

pub fn board_status(args: &Value) -> Result<ToolResult, String> {
    let _ = args; // no required params

    // JTAG boards via Vivado hw_manager
    let jtag_boards = probe_jtag_boards();

    // Serial ports
    let serial_ports = probe_serial_ports();

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    Ok(ToolResult::json(&json!({
        "jtag_boards":   jtag_boards,
        "serial_ports":  serial_ports,
        "timestamp":     ts,
    })))
}

fn probe_jtag_boards() -> Vec<Value> {
    let tcl = r#"
open_hw_manager
connect_hw_server -allow_non_jtag -url TCP:127.0.0.1:3121
refresh_hw_server
set targets [get_hw_targets]
foreach t $targets {
    if {[catch {open_hw_target $t} open_err]} {
        continue
    }
    foreach d [get_hw_devices] {
        set part [get_property PART $d]
        set status "unknown"
        if {[catch {set status [get_property STATUS $d]}]} {
            # Some families expose STATE instead of STATUS on hw_device.
            catch {set status [get_property STATE $d]}
        }
        puts "JTAG_TARGET|||$t|||$d|||$part|||$status"
    }
    catch {close_hw_target}
}
disconnect_hw_server
close_hw_manager
"#;

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let tcl_file = std::env::temp_dir().join(format!("ferrite_board_status_{ts}.tcl"));
    let _ = std::fs::write(&tcl_file, tcl);

    let out = std::process::Command::new("/opt/2025.2/Vivado/bin/vivado")
        .args(["-mode", "batch", "-nojournal", "-nolog", "-source", &tcl_file.display().to_string()])
        .output();

    let _ = std::fs::remove_file(&tcl_file);

    let mut boards = Vec::new();
    if let Ok(o) = out {
        let stdout = String::from_utf8_lossy(&o.stdout);
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix("JTAG_TARGET:") {
                let parts: Vec<&str> = rest.splitn(4, ':').collect();
                if parts.len() >= 4 {
                    boards.push(json!({
                        "target": parts[0],
                        "device": parts[1],
                        "part":   parts[2],
                        "status": parts[3],
                    }));
                }
            } else if let Some(rest) = line.strip_prefix("JTAG_TARGET|||") {
                let parts: Vec<&str> = rest.split("|||").collect();
                if parts.len() >= 4 {
                    boards.push(json!({
                        "target": parts[0],
                        "device": parts[1],
                        "part":   parts[2],
                        "status": parts[3],
                    }));
                }
            }
        }
    }
    boards
}

fn probe_serial_ports() -> Vec<Value> {
    let mut ports = Vec::new();
    // Scan /dev/ttyUSB* and /dev/ttyACM*
    for pattern in ["/dev/ttyUSB", "/dev/ttyACM"] {
        for i in 0..8 {
            let path = format!("{pattern}{i}");
            if Path::new(&path).exists() {
                ports.push(json!({ "port": path, "available": true }));
            }
        }
    }
    ports
}

// ── fpga_monitor ──────────────────────────────────────────────────────────────

pub fn fpga_monitor(args: &Value, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let baud = args["baud"].as_u64().unwrap_or(921600);

    // Auto-detect port if not supplied
    let port = if let Some(p) = args["port"].as_str() {
        p.to_owned()
    } else {
        // Find first available /dev/ttyUSBx
        let found = (0..8).find_map(|i| {
            let p = format!("/dev/ttyUSB{i}");
            if Path::new(&p).exists() { Some(p) } else { None }
        });
        found.ok_or("fpga_monitor: no /dev/ttyUSBx found; specify 'port' explicitly")?
    };

    let label = args["label"].as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("fpga_monitor_{}", port.trim_start_matches("/dev/")));

    // Use Python3 serial reader for proper baud handling
    let cmd = format!(
        "python3 -c \"\
import serial, sys, time\n\
s = serial.Serial('{port}', {baud}, timeout=1)\n\
print(f'Monitoring {port} @ {baud} baud', flush=True)\n\
while True:\n\
    try:\n\
        data = s.read(256)\n\
        if data:\n\
            sys.stdout.buffer.write(data)\n\
            sys.stdout.flush()\n\
    except Exception as e:\n\
        print(f'Error: {{e}}', flush=True)\n\
        time.sleep(0.5)\n\
\""
    );

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let job = store.spawn(&cmd, cwd, Some(&label), vec![])?;

    Ok(ToolResult::json(&json!({
        "job_id": job.job_id,
        "port":   port,
        "baud":   baud,
        "label":  label,
        "note":   "Use bg_tail/bg_status to read output, bg_kill to stop",
    })))
}

// ── helpers ───────────────────────────────────────────────────────────────────

pub fn expand_tilde(path: &str) -> PathBuf {
    if path.starts_with("~/") || path == "~" {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
        PathBuf::from(path.replacen('~', &home, 1))
    } else {
        PathBuf::from(path)
    }
}
