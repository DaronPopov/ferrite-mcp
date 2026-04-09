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

use serde::Deserialize;
use serde_json::{json, Value};

use crate::job_store::JobStore;
use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::execution::run;
use crate::tools::state::resolve_or_cwd;

// ── project_context ───────────────────────────────────────────────────────────

pub fn project_context(
    args: &Value,
    state: &Arc<Mutex<ServerState>>,
) -> Result<ToolResult, String> {
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
        if dir
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n == "verilogchill")
            .unwrap_or(false)
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
        "rtl_lab" => vec![
            "Use chip_status to see all chips",
            "Use chip_build_pipeline to run RTL flow",
            "Use board_status to detect hardware",
        ],
        "mcp_server" => vec![
            "cargo build --release -p shell-mcp",
            "cp target/release/ferrite ~/.cargo/bin/ferrite",
        ],
        "rtl_tcfp" => vec!["Use vivado_tcl for synthesis", "Use fpga_program to flash"],
        "cuda_runtime" => vec![
            "Use build_check for CUDA compilation",
            "Use ncu_profile to benchmark",
        ],
        "rust_project" => vec![
            "Use cargo_tree to inspect workspace",
            "Use test_run for tests",
        ],
        _ => vec![],
    }
}

// ── chip_status ───────────────────────────────────────────────────────────────

pub fn chip_status(args: &Value) -> Result<ToolResult, String> {
    let lab_path = args["lab_path"]
        .as_str()
        .map(expand_tilde)
        .unwrap_or_else(|| expand_tilde("~/processor_lab"));

    let chips_dir = lab_path.join("chips");
    if !chips_dir.exists() {
        return Err(format!(
            "chip_status: chips dir not found at {}",
            chips_dir.display()
        ));
    }

    let mut chips = Vec::new();
    let entries =
        std::fs::read_dir(&chips_dir).map_err(|e| format!("chip_status: read_dir: {e}"))?;

    for entry in entries.flatten() {
        let chip_path = entry.path();
        if !chip_path.is_dir() {
            continue;
        }
        let chip = chip_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();

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
        a["chip"]
            .as_str()
            .unwrap_or("")
            .cmp(b["chip"].as_str().unwrap_or(""))
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
                let mtime = p
                    .metadata()
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
    if !ip_dir.exists() {
        return None;
    }

    let Ok(ip_entries) = std::fs::read_dir(&ip_dir) else {
        return None;
    };
    for ip_entry in ip_entries.flatten() {
        let sim_dir = ip_entry.path().join("sim");
        if !sim_dir.is_dir() {
            continue;
        }

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
    if !build_dir.exists() {
        return (None, None);
    }

    // Look for timing_summary.rpt
    let mut wns: Option<f64> = None;
    let mut lut_pct: Option<f64> = None;

    if let Ok(entries) = std::fs::read_dir(build_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            if name.contains("timing_summary") && name.ends_with(".rpt") {
                if let Ok(content) = std::fs::read_to_string(&p) {
                    // Parse WNS from "WNS(ns)  TNS(ns)..." table
                    for line in content.lines() {
                        let line = line.trim();
                        if line.starts_with("WNS") {
                            continue;
                        }
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
    let chip = args["chip"]
        .as_str()
        .ok_or("chip_build_pipeline: 'chip' is required")?;
    let lab_path = args["lab_path"]
        .as_str()
        .map(expand_tilde)
        .unwrap_or_else(|| expand_tilde("~/processor_lab"));
    let requested_board = args["board"].as_str().unwrap_or("basys3");
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);
    let sim_target = args["sim_target"].as_str();
    let synth_target = args["synth_target"].as_str();

    let steps: Vec<&str> = args["steps"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_else(|| vec!["lint", "sim", "synth", "program"]);

    let chip_path = lab_path.join("chips").join(chip);
    if !chip_path.exists() {
        return Err(format!(
            "chip_build_pipeline: chip '{}' not found at {}",
            chip,
            chip_path.display()
        ));
    }
    let manifest = load_fpga_manifest_with_override(&chip_path, args["manifest_path"].as_str());
    let board = manifest_board_or_default(manifest.as_ref(), requested_board);

    if dry_run {
        let plan: Vec<Value> = steps.iter().map(|s| {
            json!({ "step": s, "cmd": build_step_cmd_resolved(chip, &chip_path, &board, s, manifest.as_ref(), sim_target, synth_target) })
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
        let cmd = build_step_cmd_resolved(
            chip,
            &chip_path,
            &board,
            step,
            manifest.as_ref(),
            sim_target,
            synth_target,
        );
        if cmd.is_empty() {
            step_results
                .push(json!({ "step": step, "skipped": true, "reason": "unsupported step" }));
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
            let exec = normalize_exec_result(&raw);
            let success = exec.success;
            if !success {
                overall_success = false;
            }
            json!({
                "step": step,
                "success": success,
                "cmd": cmd,
                "stdout": exec.stdout,
                "stderr": exec.stderr,
                "duration_ms": exec.duration_ms,
                "exit_code": exec.exit_code,
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

pub fn rtl_regression_run(args: &Value) -> Result<ToolResult, String> {
    let chip = args["chip"]
        .as_str()
        .ok_or("rtl_regression_run: 'chip' is required")?;
    let lab_path = args["lab_path"]
        .as_str()
        .map(expand_tilde)
        .unwrap_or_else(|| expand_tilde("~/processor_lab"));
    let requested_board = args["board"].as_str().unwrap_or("basys3");
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);
    let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(300);
    let sim_target = args["sim_target"].as_str();
    let synth_target = args["synth_target"].as_str();
    let steps: Vec<&str> = args["steps"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_else(|| vec!["lint", "sim"]);

    let chip_path = lab_path.join("chips").join(chip);
    if !chip_path.exists() {
        return Err(format!(
            "rtl_regression_run: chip '{}' not found at {}",
            chip,
            chip_path.display()
        ));
    }
    let manifest = load_fpga_manifest_with_override(&chip_path, args["manifest_path"].as_str());
    let board = manifest_board_or_default(manifest.as_ref(), requested_board);

    if dry_run {
        let plan: Vec<Value> = steps.iter().map(|s| {
            json!({ "step": s, "cmd": build_step_cmd_resolved(chip, &chip_path, &board, s, manifest.as_ref(), sim_target, synth_target) })
        }).collect();
        return Ok(ToolResult::json(&json!({
            "chip": chip,
            "board": board,
            "dry_run": true,
            "plan": plan,
        })));
    }

    let (step_results, overall_success) = execute_regression_steps(
        chip,
        &chip_path,
        &board,
        manifest.as_ref(),
        sim_target,
        synth_target,
        &steps,
        timeout_secs,
    );

    Ok(ToolResult::json(&json!({
        "chip": chip,
        "board": board,
        "overall_success": overall_success,
        "steps": step_results,
    })))
}

pub fn rtl_regression_report(args: &Value) -> Result<ToolResult, String> {
    let chip = args["chip"]
        .as_str()
        .ok_or("rtl_regression_report: 'chip' is required")?;
    let lab_path = args["lab_path"]
        .as_str()
        .map(expand_tilde)
        .unwrap_or_else(|| expand_tilde("~/processor_lab"));
    let requested_board = args["board"].as_str().unwrap_or("basys3");
    let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(300);
    let include_logs = args["include_logs"].as_bool().unwrap_or(false);
    let sim_target = args["sim_target"].as_str();
    let synth_target = args["synth_target"].as_str();
    let steps: Vec<&str> = args["steps"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_else(|| vec!["lint", "sim"]);

    let chip_path = lab_path.join("chips").join(chip);
    if !chip_path.exists() {
        return Err(format!(
            "rtl_regression_report: chip '{}' not found at {}",
            chip,
            chip_path.display()
        ));
    }
    let manifest = load_fpga_manifest_with_override(&chip_path, args["manifest_path"].as_str());
    let board = manifest_board_or_default(manifest.as_ref(), requested_board);

    let (step_results, overall_success) = execute_regression_steps(
        chip,
        &chip_path,
        &board,
        manifest.as_ref(),
        sim_target,
        synth_target,
        &steps,
        timeout_secs,
    );
    let step_results = if include_logs {
        step_results
    } else {
        step_results.into_iter().map(compact_step_result).collect()
    };
    let summary = summarize_regression(&step_results);
    let artifacts = regression_artifacts(&chip_path, manifest.as_ref(), sim_target, synth_target);

    Ok(ToolResult::json(&json!({
        "chip": chip,
        "board": board,
        "overall_success": overall_success,
        "summary": summary,
        "artifacts": artifacts,
        "steps": step_results,
    })))
}

pub fn fpga_triage(args: &Value) -> Result<ToolResult, String> {
    let chip = args["chip"]
        .as_str()
        .ok_or("fpga_triage: 'chip' is required")?;
    let lab_path = args["lab_path"]
        .as_str()
        .map(expand_tilde)
        .unwrap_or_else(|| expand_tilde("~/processor_lab"));
    let requested_board = args["board"].as_str().unwrap_or("basys3");
    let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(300);
    let sim_target = args["sim_target"].as_str();
    let synth_target = args["synth_target"].as_str();
    let steps: Vec<&str> = args["steps"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_else(|| vec!["lint", "sim"]);

    let chip_path = lab_path.join("chips").join(chip);
    if !chip_path.exists() {
        return Err(format!(
            "fpga_triage: chip '{}' not found at {}",
            chip,
            chip_path.display()
        ));
    }
    let manifest = load_fpga_manifest_with_override(&chip_path, args["manifest_path"].as_str());
    let board = manifest_board_or_default(manifest.as_ref(), requested_board);

    let (step_results, overall_success) = execute_regression_steps(
        chip,
        &chip_path,
        &board,
        manifest.as_ref(),
        sim_target,
        synth_target,
        &steps,
        timeout_secs,
    );
    let summary = summarize_regression(&step_results);
    let triage = build_triage(&step_results, manifest.as_ref(), sim_target, synth_target);
    let artifacts = regression_artifacts(&chip_path, manifest.as_ref(), sim_target, synth_target);

    Ok(ToolResult::json(&json!({
        "chip": chip,
        "board": board,
        "overall_success": overall_success,
        "summary": summary,
        "triage": triage,
        "artifacts": artifacts,
    })))
}

pub fn fpga_artifacts(args: &Value) -> Result<ToolResult, String> {
    let chip = args["chip"]
        .as_str()
        .ok_or("fpga_artifacts: 'chip' is required")?;
    let lab_path = args["lab_path"]
        .as_str()
        .map(expand_tilde)
        .unwrap_or_else(|| expand_tilde("~/processor_lab"));
    let sim_target = args["sim_target"].as_str();
    let synth_target = args["synth_target"].as_str();

    let chip_path = lab_path.join("chips").join(chip);
    if !chip_path.exists() {
        return Err(format!(
            "fpga_artifacts: chip '{}' not found at {}",
            chip,
            chip_path.display()
        ));
    }
    let manifest = load_fpga_manifest_with_override(&chip_path, args["manifest_path"].as_str());
    let artifacts = regression_artifacts(&chip_path, manifest.as_ref(), sim_target, synth_target);

    Ok(ToolResult::json(&json!({
        "chip": chip,
        "artifact_count": count_artifacts(&artifacts),
        "artifacts": artifacts,
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

fn build_step_cmd_resolved(
    chip: &str,
    chip_path: &Path,
    board: &str,
    step: &str,
    manifest: Option<&FpgaManifest>,
    sim_target: Option<&str>,
    synth_target: Option<&str>,
) -> String {
    match step {
        "lint" => build_step_cmd(chip, chip_path, board, step),
        "sim" => build_sim_cmd(chip_path, manifest, sim_target),
        "synth" => build_synth_cmd(chip, chip_path, board, manifest, synth_target),
        "program" => build_program_cmd(chip, chip_path, manifest, synth_target),
        "validate" => build_sim_cmd(chip_path, manifest, sim_target),
        _ => String::new(),
    }
}

#[derive(Debug, Deserialize)]
struct FpgaManifest {
    project: Option<ProjectEntry>,
    #[serde(default)]
    cocotb: Vec<CocotbEntry>,
    #[serde(default)]
    synth: Vec<SynthEntry>,
}

#[derive(Debug, Deserialize)]
struct ProjectEntry {
    board: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CocotbEntry {
    name: Option<String>,
    dir: String,
    mode: Option<String>,
    module: Option<String>,
    sim: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SynthEntry {
    name: Option<String>,
    tcl: String,
    bitstream: Option<String>,
}

fn load_fpga_manifest(chip_path: &Path) -> Option<FpgaManifest> {
    let text = std::fs::read_to_string(chip_path.join("ferrite_fpga.toml")).ok()?;
    toml::from_str(&text).ok()
}

fn load_fpga_manifest_with_override(
    chip_path: &Path,
    manifest_path: Option<&str>,
) -> Option<FpgaManifest> {
    if let Some(path) = manifest_path {
        let text = std::fs::read_to_string(expand_tilde(path)).ok()?;
        return toml::from_str(&text).ok();
    }
    load_fpga_manifest(chip_path)
}

fn manifest_board_or_default(manifest: Option<&FpgaManifest>, board: &str) -> String {
    manifest
        .and_then(|m| m.project.as_ref())
        .and_then(|p| p.board.clone())
        .unwrap_or_else(|| board.to_owned())
}

fn selected_cocotb_entries<'a>(
    manifest: Option<&'a FpgaManifest>,
    sim_target: Option<&str>,
) -> Vec<&'a CocotbEntry> {
    let Some(manifest) = manifest else {
        return Vec::new();
    };
    let mut entries: Vec<&CocotbEntry> = manifest.cocotb.iter().collect();
    if let Some(target) = sim_target {
        entries.retain(|entry| entry.name.as_deref() == Some(target));
    }
    entries
}

fn selected_synth_entry<'a>(
    manifest: Option<&'a FpgaManifest>,
    synth_target: Option<&str>,
) -> Option<&'a SynthEntry> {
    let manifest = manifest?;
    if let Some(target) = synth_target {
        manifest
            .synth
            .iter()
            .find(|entry| entry.name.as_deref() == Some(target))
    } else {
        manifest.synth.first()
    }
}

fn cocotb_entry_cmd(chip_path: &Path, entry: &CocotbEntry) -> Option<String> {
    let dir = chip_path.join(&entry.dir);
    if !dir.is_dir() {
        return None;
    }
    let sim_dir = shell_single_quote(&dir.display().to_string());
    let sim = entry.sim.as_deref().unwrap_or("icarus");
    let mode = entry.mode.as_deref().unwrap_or("auto");
    let module_env = entry
        .module
        .as_ref()
        .map(|module| format!(" MODULE={}", shell_single_quote(module)))
        .unwrap_or_default();
    let cmd = match mode {
        "makefile" => format!(
            "(cd {sim_dir} && SIM={}{} make 2>&1)",
            shell_single_quote(sim),
            module_env
        ),
        "pytest" => {
            if let Some(module) = entry.module.as_ref() {
                format!(
                    "(cd {sim_dir} && SIM={} python3 -m pytest -x -q {} 2>&1)",
                    shell_single_quote(sim),
                    shell_single_quote(module)
                )
            } else {
                format!(
                    "(cd {sim_dir} && SIM={} python3 -m pytest -x -q 2>&1)",
                    shell_single_quote(sim)
                )
            }
        }
        _ => format!(
            "if [ -f {sim_dir}/Makefile ]; then (cd {sim_dir} && SIM={}{} make 2>&1); else (cd {sim_dir} && SIM={} python3 -m pytest -x -q{} 2>&1); fi",
            shell_single_quote(sim),
            module_env,
            shell_single_quote(sim),
            entry.module.as_ref().map(|module| format!(" {}", shell_single_quote(module))).unwrap_or_default()
        ),
    };
    Some(cmd)
}

fn build_sim_cmd(
    chip_path: &Path,
    manifest: Option<&FpgaManifest>,
    sim_target: Option<&str>,
) -> String {
    let manifest_entries = selected_cocotb_entries(manifest, sim_target);
    let sim_cmds = if !manifest_entries.is_empty() {
        manifest_entries
            .into_iter()
            .filter_map(|entry| cocotb_entry_cmd(chip_path, entry))
            .collect::<Vec<_>>()
    } else {
        let sim_dirs = discover_sim_dirs(chip_path);
        sim_dirs
            .iter()
            .map(|dir| {
                let quoted = shell_single_quote(&dir.display().to_string());
                format!(
                    "if [ -f {quoted}/Makefile ]; then (cd {quoted} && make SIM=icarus 2>&1); else (cd {quoted} && python3 -m pytest -x -q 2>&1); fi"
                )
            })
            .collect::<Vec<_>>()
    };

    if sim_cmds.is_empty() {
        return String::new();
    }

    sim_cmds.join(" && ")
}

fn build_synth_cmd(
    chip: &str,
    chip_path: &Path,
    board: &str,
    manifest: Option<&FpgaManifest>,
    synth_target: Option<&str>,
) -> String {
    let vivado = "/opt/2025.2/Vivado/bin/vivado";
    let board_name = manifest_board_or_default(manifest, board);
    let tcl = selected_synth_entry(manifest, synth_target)
        .map(|entry| chip_path.join(&entry.tcl))
        .filter(|path| path.exists())
        .or_else(|| discover_synth_tcl(chip, chip_path, &board_name));
    match tcl {
        Some(tcl) => format!("{vivado} -mode batch -source {}", tcl.display()),
        None => String::new(),
    }
}

fn build_program_cmd(
    chip: &str,
    chip_path: &Path,
    manifest: Option<&FpgaManifest>,
    synth_target: Option<&str>,
) -> String {
    let bit = selected_synth_entry(manifest, synth_target)
        .and_then(|entry| entry.bitstream.as_ref())
        .map(|bit| chip_path.join(bit))
        .filter(|path| path.exists())
        .or_else(|| discover_bitstream(chip, chip_path));
    match bit {
        Some(bit) => format!(
            "/opt/2025.2/Vivado/bin/vivado -mode batch -source /dev/stdin <<'EOF'\nopen_hw_manager\nconnect_hw_server\nopen_hw_target\nset_property PROGRAM.FILE {} [current_hw_device]\nprogram_hw_devices\nEOF",
            bit.display()
        ),
        None => String::new(),
    }
}

fn discover_sim_dirs(chip_path: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let Ok(entries) = std::fs::read_dir(chip_path.join("ip")) else {
        return dirs;
    };
    for entry in entries.flatten() {
        let sim = entry.path().join("sim");
        if sim.is_dir() {
            dirs.push(sim);
        }
    }
    dirs
}

fn discover_synth_tcl(chip: &str, chip_path: &Path, board: &str) -> Option<PathBuf> {
    let mut fallback = None;
    let Ok(entries) = std::fs::read_dir(chip_path.join("top").join(board).join("tcl")) else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or_default();
        if !name.ends_with(".tcl") {
            continue;
        }
        if name.starts_with("build") && name.contains(chip) {
            return Some(path);
        }
        if fallback.is_none() && name.starts_with("build") {
            fallback = Some(path);
        }
    }
    fallback
}

fn discover_bitstream(chip: &str, chip_path: &Path) -> Option<PathBuf> {
    let mut stack = vec![chip_path.join("build")];
    let mut fallback = None;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|v| v.to_str()) != Some("bit") {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or_default();
            if name.contains(chip) {
                return Some(path);
            }
            if fallback.is_none() {
                fallback = Some(path);
            }
        }
    }
    fallback
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn execute_regression_steps(
    chip: &str,
    chip_path: &Path,
    board: &str,
    manifest: Option<&FpgaManifest>,
    sim_target: Option<&str>,
    synth_target: Option<&str>,
    steps: &[&str],
    timeout_secs: u64,
) -> (Vec<Value>, bool) {
    let mut step_results = Vec::new();
    let mut overall_success = true;
    for step in steps {
        let cmd = build_step_cmd_resolved(
            chip,
            chip_path,
            board,
            step,
            manifest,
            sim_target,
            synth_target,
        );
        if cmd.is_empty() {
            step_results.push(json!({ "step": step, "skipped": true, "reason": "unsupported or unresolved step" }));
            continue;
        }

        let raw = run(&cmd, chip_path, &[], "", Duration::from_secs(timeout_secs));
        let exec = normalize_exec_result(&raw);
        let success = exec.success;
        if !success {
            overall_success = false;
        }
        step_results.push(json!({
            "step": step,
            "success": success,
            "cmd": cmd,
            "stdout": exec.stdout,
            "stderr": exec.stderr,
            "duration_ms": exec.duration_ms,
            "exit_code": exec.exit_code,
            "failure_kind": classify_regression_failure(step, &exec),
        }));
        if !success {
            break;
        }
    }
    (step_results, overall_success)
}

struct ExecResult {
    success: bool,
    stdout: Value,
    stderr: Value,
    duration_ms: Value,
    exit_code: Value,
}

fn normalize_exec_result(raw: &Value) -> ExecResult {
    let success = raw["success"]
        .as_bool()
        .or_else(|| raw["ok"].as_bool())
        .unwrap_or(false);
    let stdout = if raw.get("stdout").is_some() {
        raw["stdout"].clone()
    } else {
        raw["out"].clone()
    };
    let stderr = raw
        .get("stderr")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let duration_ms = if raw.get("duration_ms").is_some() {
        raw["duration_ms"].clone()
    } else {
        raw["ms"].clone()
    };
    let exit_code =
        raw.get("exit_code").cloned().unwrap_or_else(
            || {
                if success {
                    json!(0)
                } else {
                    Value::Null
                }
            },
        );
    ExecResult {
        success,
        stdout,
        stderr,
        duration_ms,
        exit_code,
    }
}

fn classify_regression_failure(step: &str, exec: &ExecResult) -> &'static str {
    if exec.success {
        return "none";
    }
    let stdout = exec.stdout.as_str().unwrap_or("");
    let stderr = exec.stderr.as_str().unwrap_or("");
    let combined = format!("{stdout}\n{stderr}").to_lowercase();
    match step {
        "lint" => {
            if combined.contains("syntax error") {
                "lint_syntax"
            } else {
                "lint_error"
            }
        }
        "sim" | "validate" => {
            if combined.contains("timed out") {
                "sim_timeout"
            } else if combined.contains("failed") || combined.contains("assert") {
                "sim_failure"
            } else {
                "sim_error"
            }
        }
        "synth" => "synth_error",
        "program" => "program_error",
        _ => "unknown",
    }
}

fn summarize_regression(step_results: &[Value]) -> Value {
    let total_steps = step_results.len();
    let passed_steps = step_results
        .iter()
        .filter(|s| s["success"].as_bool() == Some(true))
        .count();
    let skipped_steps = step_results
        .iter()
        .filter(|s| s["skipped"].as_bool() == Some(true))
        .count();
    let first_failure = step_results
        .iter()
        .find(|s| s["success"].as_bool() == Some(false))
        .map(|s| {
            json!({
                "step": s["step"],
                "failure_kind": s["failure_kind"],
                "exit_code": s["exit_code"],
            })
        });
    json!({
        "total_steps": total_steps,
        "passed_steps": passed_steps,
        "skipped_steps": skipped_steps,
        "failed_steps": total_steps.saturating_sub(passed_steps + skipped_steps),
        "first_failure": first_failure,
    })
}

fn compact_step_result(step: Value) -> Value {
    let stdout = step["stdout"].as_str().unwrap_or("");
    let stderr = step["stderr"].as_str().unwrap_or("");
    let mut compact = step.clone();
    if let Some(obj) = compact.as_object_mut() {
        obj.remove("stdout");
        obj.remove("stderr");
        obj.insert(
            "stdout_preview".to_owned(),
            json!(truncate_preview(stdout, 280)),
        );
        obj.insert(
            "stderr_preview".to_owned(),
            json!(truncate_preview(stderr, 280)),
        );
    }
    compact
}

fn truncate_preview(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let trimmed: String = s.chars().take(max_chars).collect();
    format!("{trimmed}...")
}

fn regression_artifacts(
    chip_path: &Path,
    manifest: Option<&FpgaManifest>,
    sim_target: Option<&str>,
    synth_target: Option<&str>,
) -> Value {
    let sim_artifacts: Vec<Value> = if !selected_cocotb_entries(manifest, sim_target).is_empty() {
        selected_cocotb_entries(manifest, sim_target)
            .into_iter()
            .map(|entry| {
                let dir = chip_path.join(&entry.dir);
                json!({
                    "kind": "sim_dir",
                    "name": entry.name.clone().unwrap_or_else(|| entry.dir.clone()),
                    "path": dir.display().to_string(),
                    "results_xml": dir.join("results.xml").display().to_string(),
                    "results_xml_exists": dir.join("results.xml").exists(),
                    "vcd_files": discover_vcd_files(&dir),
                })
            })
            .collect()
    } else {
        discover_sim_dirs(chip_path)
            .into_iter()
            .map(|dir| {
                json!({
                    "kind": "sim_dir",
                    "name": dir.file_name().and_then(|n| n.to_str()).unwrap_or("sim"),
                    "path": dir.display().to_string(),
                    "results_xml": dir.join("results.xml").display().to_string(),
                    "results_xml_exists": dir.join("results.xml").exists(),
                    "vcd_files": discover_vcd_files(&dir),
                })
            })
            .collect()
    };

    let synth_artifact = selected_synth_entry(manifest, synth_target).map(|entry| {
        let tcl = chip_path.join(&entry.tcl);
        let bitstream = entry.bitstream.as_ref().map(|b| chip_path.join(b));
        json!({
            "kind": "synth_target",
            "name": entry.name.clone().unwrap_or_else(|| "default".to_owned()),
            "tcl": tcl.display().to_string(),
            "tcl_exists": tcl.exists(),
            "bitstream": bitstream.as_ref().map(|b| b.display().to_string()),
            "bitstream_exists": bitstream.as_ref().map(|b| b.exists()).unwrap_or(false),
        })
    });

    json!({
        "sim": sim_artifacts,
        "synth": synth_artifact,
    })
}

fn discover_vcd_files(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|v| v.to_str()) == Some("vcd") {
            files.push(path.display().to_string());
        }
    }
    files.sort();
    files
}

fn count_artifacts(artifacts: &Value) -> usize {
    let sim_count = artifacts["sim"].as_array().map(|a| a.len()).unwrap_or(0);
    let synth_count = usize::from(!artifacts["synth"].is_null());
    sim_count + synth_count
}

fn build_triage(
    step_results: &[Value],
    manifest: Option<&FpgaManifest>,
    sim_target: Option<&str>,
    synth_target: Option<&str>,
) -> Value {
    let Some(failure) = step_results
        .iter()
        .find(|s| s["success"].as_bool() == Some(false))
    else {
        return json!({
            "status": "green",
            "severity": "info",
            "root_cause": "none",
            "confidence": "high",
            "recommended_next_action": "advance_to_next_stage",
            "recommended_tool": "chip_build_pipeline",
            "notes": ["Regression passed for all executed steps."],
        });
    };

    let step = failure["step"].as_str().unwrap_or("unknown");
    let kind = failure["failure_kind"].as_str().unwrap_or("unknown");
    let stdout = failure["stdout"].as_str().unwrap_or("");
    let stderr = failure["stderr"].as_str().unwrap_or("");
    let combined = format!("{stdout}\n{stderr}");

    let (severity, root_cause, confidence, next_action, tool, notes): (
        &str,
        &str,
        &str,
        &str,
        &str,
        Vec<String>,
    ) = match kind {
        "lint_syntax" => (
            "high",
            "rtl_syntax_error",
            "high",
            "open_reported_file_and_fix_syntax",
            "read_context",
            extract_location_notes(&combined),
        ),
        "lint_error" => (
            "high",
            "rtl_lint_error",
            "medium",
            "inspect_lint_output_and_fix_rtl",
            "read_context",
            extract_location_notes(&combined),
        ),
        "sim_failure" => (
            "high",
            "simulation_failure",
            "medium",
            "inspect_test_output_then_query_waveform_or_testbench",
            "waveform_query",
            vec![format!(
                "Simulation stage '{step}' reported a failing test or assertion."
            )],
        ),
        "sim_timeout" => (
            "medium",
            "simulation_timeout",
            "medium",
            "inspect_testbench_for_deadlock_or_increase_timeout",
            "cocotb_run",
            vec![format!(
                "Simulation stage '{step}' timed out before completion."
            )],
        ),
        "synth_error" => (
            "high",
            "synthesis_failure",
            "medium",
            "inspect_vivado_output_and_tcl_entrypoint",
            "vivado_tcl",
            vec![manifest_synth_note(manifest, synth_target)],
        ),
        "program_error" => (
            "high",
            "board_programming_failure",
            "medium",
            "check_board_status_and_programming_target",
            "board_status",
            vec![manifest_synth_note(manifest, synth_target)],
        ),
        _ => (
            "medium",
            "unknown_failure",
            "low",
            "inspect_step_output",
            "rtl_regression_report",
            vec![format!("Unhandled failure kind '{kind}' in step '{step}'.")],
        ),
    };

    json!({
        "status": "red",
        "severity": severity,
        "root_cause": root_cause,
        "confidence": confidence,
        "failed_step": step,
        "failed_kind": kind,
        "recommended_next_action": next_action,
        "recommended_tool": tool,
        "selected_targets": {
            "sim_target": sim_target,
            "synth_target": synth_target,
        },
        "notes": notes,
    })
}

fn extract_location_notes(output: &str) -> Vec<String> {
    let mut notes = Vec::new();
    for line in output.lines().take(4) {
        if line.contains(':') {
            notes.push(line.to_owned());
        }
    }
    if notes.is_empty() {
        notes.push("No explicit file:line location found in tool output.".to_owned());
    }
    notes
}

fn manifest_synth_note(manifest: Option<&FpgaManifest>, synth_target: Option<&str>) -> String {
    if let Some(entry) = selected_synth_entry(manifest, synth_target) {
        return format!(
            "Selected synth target '{}' with Tcl '{}'.",
            entry.name.as_deref().unwrap_or("default"),
            entry.tcl
        );
    }
    "No explicit synth target selected; using discovered project layout.".to_owned()
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
        .args([
            "-mode",
            "batch",
            "-nojournal",
            "-nolog",
            "-source",
            &tcl_file.display().to_string(),
        ])
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
            if Path::new(&p).exists() {
                Some(p)
            } else {
                None
            }
        });
        found.ok_or("fpga_monitor: no /dev/ttyUSBx found; specify 'port' explicitly")?
    };

    let label = args["label"]
        .as_str()
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

#[cfg(test)]
mod tests {
    use super::{
        build_sim_cmd, count_artifacts, normalize_exec_result, CocotbEntry, FpgaManifest,
        ProjectEntry, SynthEntry,
    };
    use serde_json::json;
    use std::fs;

    #[test]
    fn normalize_exec_result_handles_compact_success_shape() {
        let exec = normalize_exec_result(&json!({
            "ok": true,
            "ms": 12,
            "out": "clean success"
        }));
        assert!(exec.success);
        assert_eq!(exec.stdout, json!("clean success"));
        assert_eq!(exec.stderr, json!(""));
        assert_eq!(exec.duration_ms, json!(12));
        assert_eq!(exec.exit_code, json!(0));
    }

    #[test]
    fn normalize_exec_result_handles_verbose_failure_shape() {
        let exec = normalize_exec_result(&json!({
            "success": false,
            "exit_code": 2,
            "duration_ms": 34,
            "stdout": "",
            "stderr": "failed"
        }));
        assert!(!exec.success);
        assert_eq!(exec.stdout, json!(""));
        assert_eq!(exec.stderr, json!("failed"));
        assert_eq!(exec.duration_ms, json!(34));
        assert_eq!(exec.exit_code, json!(2));
    }

    #[test]
    fn build_sim_cmd_honors_manifest_mode_and_module() {
        let root =
            std::env::temp_dir().join(format!("ferrite_project_test_{}", std::process::id()));
        let sim_dir = root.join("ip/softmax/sim");
        fs::create_dir_all(&sim_dir).unwrap();
        fs::write(sim_dir.join("Makefile"), "all:\n\t@true\n").unwrap();

        let manifest = FpgaManifest {
            project: Some(ProjectEntry {
                board: Some("basys3".to_owned()),
            }),
            cocotb: vec![CocotbEntry {
                name: Some("softmax".to_owned()),
                dir: "ip/softmax/sim".to_owned(),
                mode: Some("makefile".to_owned()),
                module: Some("test_softmax_lut".to_owned()),
                sim: Some("icarus".to_owned()),
            }],
            synth: vec![SynthEntry {
                name: Some("basys3_n6".to_owned()),
                tcl: "top/basys3/tcl/build_n6_attn.tcl".to_owned(),
                bitstream: Some("build/basys3_n6/attn_basys3_top_n6.bit".to_owned()),
            }],
        };

        let cmd = build_sim_cmd(&root, Some(&manifest), Some("softmax"));
        assert!(cmd.contains("make 2>&1"));
        assert!(cmd.contains("MODULE='test_softmax_lut'"));
        assert!(cmd.contains("SIM='icarus'"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn count_artifacts_counts_sim_and_synth_entries() {
        let artifacts = json!({
            "sim": [{ "kind": "sim_dir" }, { "kind": "sim_dir" }],
            "synth": { "kind": "synth_target" }
        });
        assert_eq!(count_artifacts(&artifacts), 3);
    }
}
