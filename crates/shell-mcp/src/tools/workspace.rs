//! Workspace-level orientation and session notes.
//!
//! Tools:
//!   orient — machine topology: project discovery, git state, recent SOURCE changes, ports
//!            ports are opt-in (ports:true) and filtered to named processes only
//!   note   — session scratchpad stored in ServerState (read/append/clear)

use std::sync::{Arc, Mutex};
use serde_json::{json, Value};

use crate::job_store::JobStore;
use crate::protocol::ToolResult;
use crate::server::ServerState;
use super::{filesystem, git, system, hardware, eda, project};

// ── orient ────────────────────────────────────────────────────────────────────

/// Single-call situational awareness / machine topology.
///
/// Returns:
///   - `projects`  — discovered git repos / workspaces under `root` (or `~/`)
///   - `git`       — git status of `path` (cwd)
///   - `recent`    — recently changed SOURCE files (build artifacts + SDK noise suppressed)
///   - `ports`     — opt-in (ports:true), named processes only
///   - `summary`   — one-line compact string for fast orientation
pub fn orient(args: &Value, state: &Arc<Mutex<ServerState>>, store: &Arc<JobStore>) -> Result<ToolResult, String> {
    let cwd = {
        let s = state.lock().unwrap();
        s.cwd.to_string_lossy().into_owned()
    };

    let path          = args["path"].as_str().unwrap_or(&cwd).to_owned();
    let recent_window = args["since"].as_str().unwrap_or("2h");
    let include_ports = args["ports"].as_bool().unwrap_or(false);

    // Root for project discovery — use home dir, not cwd, for broad machine view
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home".to_owned());
    let discover_root = args["root"].as_str().unwrap_or(&home).to_owned();

    // ── opt-in expansion flags ────────────────────────────────────────────────
    let include_bg_jobs = args["bg_jobs"].as_bool().unwrap_or(false);
    let include_chips   = args["chips"].as_bool().unwrap_or(false);
    let include_synth   = args["synth"].as_bool().unwrap_or(false);
    let include_diff    = args["diff"].as_bool().unwrap_or(false);
    let include_hw      = args["hw"].as_bool().unwrap_or(false);

    // ── project discovery ─────────────────────────────────────────────────────
    let projects = discover_projects(&discover_root);

    // ── git status of cwd (cap untracked to avoid blowup on dirty repos) ──────
    let git_val = match git::git_status(&json!({ "path": path }), state) {
        Ok(tr) => {
            let mut v = parse_tool_result_json(&tr);
            cap_array_field(&mut v, "untracked", 20);
            v
        }
        Err(_) => json!(null),
    };

    // ── recently changed SOURCE files (artifacts + SDK noise suppressed) ───────
    let (recent_val, artifacts_suppressed) = match filesystem::changed_since(&json!({
        "path":           home,
        "since_relative": recent_window,
        "max_results":    80,
    }), state) {
        Ok(tr) => slim_changed_filtered(parse_tool_result_json(&tr)),
        Err(_) => (json!({ "count": 0, "changed": [] }), 0usize),
    };

    // ── ports (opt-in, named processes only) ──────────────────────────────────
    let ports_val = if include_ports {
        match system::port_list(&json!({})) {
            Ok(tr) => filter_named_ports(parse_tool_result_json(&tr)),
            Err(_) => json!([]),
        }
    } else {
        json!(null)
    };

    // ── bg_jobs (opt-in) ──────────────────────────────────────────────────────
    let bg_jobs_val = if include_bg_jobs {
        orient_bg_jobs(store)
    } else {
        json!(null)
    };

    // ── chips (opt-in) ────────────────────────────────────────────────────────
    let chips_val = if include_chips {
        orient_chips(&path)
    } else {
        json!(null)
    };

    // ── synth report (opt-in) ─────────────────────────────────────────────────
    let synth_val = if include_synth {
        orient_synth(&path)
    } else {
        json!(null)
    };

    // ── git diff stat (opt-in) ────────────────────────────────────────────────
    let diff_val = if include_diff {
        orient_diff(&path)
    } else {
        json!(null)
    };

    // ── gpu live (opt-in) ─────────────────────────────────────────────────────
    let hw_val = if include_hw {
        match hardware::gpu_live(&json!({})) {
            Ok(tr) => parse_tool_result_json(&tr),
            Err(e) => json!({ "error": e }),
        }
    } else {
        json!(null)
    };

    // ── compact summary string ────────────────────────────────────────────────
    let mut summary = build_summary(&path, &git_val, &recent_val, artifacts_suppressed, &projects, &ports_val, include_ports);

    // Append active-job count when bg_jobs was requested
    if include_bg_jobs {
        if let Some(n) = bg_jobs_val["running"].as_u64() {
            if n > 0 {
                summary.push_str(&format!(" | {} bg running", n));
            }
        }
    }

    Ok(ToolResult::json(&json!({
        "summary":              summary,
        "cwd":                  cwd,
        "projects":             projects,
        "git":                  git_val,
        "recent":               recent_val,
        "artifacts_suppressed": artifacts_suppressed,
        "ports":                ports_val,
        "bg_jobs":              bg_jobs_val,
        "chips":                chips_val,
        "synth":                synth_val,
        "diff":                 diff_val,
        "hw":                   hw_val,
    })))
}

// ── note ──────────────────────────────────────────────────────────────────────

/// Session scratchpad.
///
/// op=read   — return all notes
/// op=append — add a new note line
/// op=clear  — delete all notes
pub fn note(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let op = args["op"].as_str().unwrap_or("read");

    match op {
        "read" => {
            let s = state.lock().unwrap();
            Ok(ToolResult::json(&json!({
                "count": s.notes.len(),
                "notes": s.notes,
            })))
        }
        "append" => {
            let content = args["content"].as_str()
                .ok_or("note append: 'content' required")?
                .to_owned();
            let mut s = state.lock().unwrap();
            s.notes.push(content.clone());
            Ok(ToolResult::json(&json!({
                "ok":    true,
                "count": s.notes.len(),
                "added": content,
            })))
        }
        "clear" => {
            let mut s = state.lock().unwrap();
            let prev = s.notes.len();
            s.notes.clear();
            Ok(ToolResult::json(&json!({
                "ok":      true,
                "cleared": prev,
            })))
        }
        other => Err(format!("note: unknown op '{}' — use read|append|clear", other)),
    }
}

// ── project discovery ─────────────────────────────────────────────────────────

/// Top-level directory names to skip during project discovery.
/// These are noisy (system, SDK, media) and not project roots.
const SKIP_DIRS: &[&str] = &[
    "nvidia", "nvidia_sdk", "Downloads", "Desktop", "Pictures",
    "Videos", "Music", "Templates", "Public", "snap", "flatpak",
    "go", "gems", "perl5",
];

/// Discover projects under `root`. Walks depth 1, and depth 2 for
/// dirs that look like workspace containers rather than projects themselves.
fn discover_projects(root: &str) -> Vec<Value> {
    let root_path = std::path::PathBuf::from(root);
    let Ok(entries) = std::fs::read_dir(&root_path) else { return vec![] };

    let mut projects = Vec::new();

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_dir() { continue; }

        let name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();

        // Skip hidden dirs and known noisy dirs
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }

        if let Some(proj) = classify_project(&path, &name) {
            projects.push(proj);
        } else {
            // Might be a workspace container (e.g. ~/verilogchill/) — scan one level deeper
            if let Ok(sub_entries) = std::fs::read_dir(&path) {
                for sub in sub_entries.filter_map(|e| e.ok()) {
                    let sub_path = sub.path();
                    if !sub_path.is_dir() { continue; }
                    let sub_name = sub_path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_owned();
                    if sub_name.starts_with('.') { continue; }
                    if let Some(proj) = classify_project(&sub_path, &sub_name) {
                        projects.push(proj);
                    }
                }
            }
        }
    }

    // Sort by path for stable output
    projects.sort_by(|a, b| {
        a["path"].as_str().unwrap_or("").cmp(b["path"].as_str().unwrap_or(""))
    });

    projects
}

/// Identify a project directory by its marker files. Returns None if no markers found.
fn classify_project(path: &std::path::Path, name: &str) -> Option<Value> {
    let has_git       = path.join(".git").exists();
    let has_cargo     = path.join("Cargo.toml").exists();
    let has_pyproject = path.join("pyproject.toml").exists() || path.join("setup.py").exists();
    let has_pkg_json  = path.join("package.json").exists();
    let has_rtl       = path.join("rtl").exists()
                     || path.join("chips").exists()
                     || path.join("ip").exists();
    let has_makefile  = path.join("Makefile").exists();

    // Need at least one meaningful marker
    if !has_git && !has_cargo && !has_pyproject && !has_pkg_json && !has_rtl {
        return None;
    }

    // Build kind tags
    let mut kinds: Vec<&str> = Vec::new();
    if has_rtl       { kinds.push("rtl"); }
    if has_cargo     { kinds.push("rust"); }
    if has_pyproject { kinds.push("python"); }
    if has_pkg_json  { kinds.push("node"); }
    if kinds.is_empty() { kinds.push("git"); }

    // Read git branch without shelling out
    let branch = if has_git {
        read_git_head(&path.join(".git").join("HEAD"))
    } else {
        None
    };

    // Count immediate children for size hint
    let child_count = std::fs::read_dir(path)
        .map(|d| d.filter_map(|e| e.ok()).count())
        .unwrap_or(0);

    Some(json!({
        "name":         name,
        "path":         path.display().to_string(),
        "kind":         kinds.join("+"),
        "git":          has_git,
        "branch":       branch,
        "has_makefile": has_makefile,
        "items":        child_count,
    }))
}

/// Read git branch from `.git/HEAD` without shelling out.
fn read_git_head(head_path: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(head_path).ok()?;
    let content = content.trim();
    if let Some(stripped) = content.strip_prefix("ref: refs/heads/") {
        Some(stripped.to_owned())
    } else {
        // Detached HEAD — return short hash
        Some(content.chars().take(8).collect())
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Extract JSON from a ToolResult's first content item, or return raw text as string value.
fn parse_tool_result_json(tr: &ToolResult) -> Value {
    tr.content.first()
        .and_then(|c| serde_json::from_str(&c.text).ok())
        .unwrap_or_else(|| {
            tr.content.first()
                .map(|c| Value::String(c.text.clone()))
                .unwrap_or(Value::Null)
        })
}

/// Truncate an array field inside a JSON object to `max` entries.
fn cap_array_field(v: &mut Value, key: &str, max: usize) {
    if let Some(arr) = v.get_mut(key).and_then(|a| a.as_array_mut()) {
        arr.truncate(max);
    }
}

// ── artifact / noise filtering ────────────────────────────────────────────────

/// Build artifact file extensions — generated files that are never source.
const ARTIFACT_EXTENSIONS: &[&str] = &[
    ".log", ".jou", ".bit", ".rpt", ".dcp", ".xpr",
    ".pb",  ".xml", ".vcd", ".wdb", ".ltx", ".mmi",
];

/// Path segments that indicate build output directories.
const ARTIFACT_PATH_SEGMENTS: &[&str] = &[
    "/build/", "/sim_build/", "/__pycache__/", "/.Xil/",
    "/runs/",  "/.cache/",    "/target/",
];

/// File name fragments that mark backup/log artifacts.
const ARTIFACT_NAME_SEGMENTS: &[&str] = &[
    ".backup.", "backup.log", "backup.jou",
];

/// Path segments that indicate SDK/system noise — rootfs trees, runtime dirs, etc.
const NOISE_PATH_SEGMENTS: &[&str] = &[
    "/rootfs/var/",
    "/rootfs/run/",
    "/rootfs/etc/",
    "/rootfs/tmp/",
    "/Linux_for_Tegra/rootfs/",
    "/nvidia_sdk/",
    "/nvidia/nvidia_sdk/",
    "/var/run/",
    "/var/lock/",
];

/// Returns true if the file path looks like a build artifact.
fn is_artifact(path: &str) -> bool {
    let lower = path.to_lowercase();
    ARTIFACT_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
        || ARTIFACT_PATH_SEGMENTS.iter().any(|seg| lower.contains(seg))
        || ARTIFACT_NAME_SEGMENTS.iter().any(|seg| lower.contains(seg))
}

/// Returns true if the file path is under a known SDK/system noise tree.
fn is_noise(path: &str) -> bool {
    NOISE_PATH_SEGMENTS.iter().any(|seg| path.contains(seg))
}

/// Slim and filter changed_since output — only SOURCE files, artifacts + noise suppressed.
/// Returns (filtered_value, suppressed_count).
fn slim_changed_filtered(v: Value) -> (Value, usize) {
    let all = match v["changed"].as_array() {
        Some(a) => a.clone(),
        None    => return (json!({ "count": 0, "changed": [] }), 0),
    };

    let mut source     = Vec::new();
    let mut suppressed = 0usize;

    for f in &all {
        let path = f["path"].as_str().unwrap_or("");
        if is_artifact(path) || is_noise(path) {
            suppressed += 1;
        } else {
            source.push(json!({
                "path":          f["path"],
                "modified_secs": f["modified_secs"],
            }));
        }
    }

    let out = json!({
        "since_secs": v["since_secs"],
        "count":      source.len(),
        "truncated":  v["truncated"],
        "changed":    source,
    });

    (out, suppressed)
}

/// Filter port list to only entries with a non-empty process name.
fn filter_named_ports(v: Value) -> Value {
    match v.as_array() {
        Some(ports) => {
            let named: Vec<Value> = ports.iter()
                .filter(|p| p["process"].as_str().map(|s| !s.is_empty()).unwrap_or(false))
                .cloned()
                .collect();
            json!(named)
        }
        None => {
            match v["ports"].as_array() {
                Some(arr) => {
                    let named: Vec<Value> = arr.iter()
                        .filter(|p| p["process"].as_str().map(|s| !s.is_empty()).unwrap_or(false))
                        .cloned()
                        .collect();
                    json!(named)
                }
                None => json!([]),
            }
        }
    }
}

/// Build a compact one-line summary for fast Claude orientation.
fn build_summary(
    path: &str,
    git: &Value,
    recent: &Value,
    artifacts_suppressed: usize,
    projects: &[Value],
    ports: &Value,
    include_ports: bool,
) -> String {
    let project = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);

    let git_summary = if git.is_null() {
        "not a git repo".to_owned()
    } else {
        let branch   = git["branch"].as_str().unwrap_or("?");
        let clean    = git["clean"].as_bool().unwrap_or(true);
        let ahead    = git["ahead"].as_u64().unwrap_or(0);
        let behind   = git["behind"].as_u64().unwrap_or(0);
        let staged   = git["staged"].as_array().map(|a| a.len()).unwrap_or(0);
        let unstaged = git["unstaged"].as_array().map(|a| a.len()).unwrap_or(0);

        let mut parts = vec![format!("branch: {}", branch)];
        if clean {
            parts.push("clean".to_owned());
        } else {
            if staged   > 0 { parts.push(format!("{} staged", staged)); }
            if unstaged > 0 { parts.push(format!("{} unstaged", unstaged)); }
        }
        if ahead  > 0 { parts.push(format!("+{} ahead", ahead)); }
        if behind > 0 { parts.push(format!("{} behind", behind)); }
        parts.join(", ")
    };

    let source_count = recent["count"].as_u64().unwrap_or(0);
    let recent_summary = if artifacts_suppressed > 0 {
        format!("{} source changes ({} suppressed)", source_count, artifacts_suppressed)
    } else {
        format!("{} source changes", source_count)
    };

    let proj_summary = format!("{} projects", projects.len());

    let ports_summary = if !include_ports {
        "ports: n/a (use ports:true)".to_owned()
    } else {
        match ports.as_array() {
            Some(arr) if arr.is_empty() => "no named ports".to_owned(),
            Some(arr) => format!("{} named ports", arr.len()),
            None      => "no named ports".to_owned(),
        }
    };

    format!("{} | git: {} | {} | {} | {}", project, git_summary, recent_summary, proj_summary, ports_summary)
}

// ── orient expansion helpers ──────────────────────────────────────────────────

/// bg_jobs: running + recently completed jobs (within 1 hour).
fn orient_bg_jobs(store: &Arc<JobStore>) -> Value {
    let jobs = store.all();
    let active: Vec<Value> = jobs.iter().filter_map(|job| {
        let status  = job.current_status();
        let elapsed = job.elapsed_secs();
        // Always include running; include done/killed from the last hour
        if status.label() == "running" || elapsed < 3600.0 {
            Some(json!({
                "job_id":       job.job_id,
                "label":        job.label,
                "status":       status.label(),
                "elapsed_secs": elapsed as u64,
                "exit_code":    status.exit_code(),
            }))
        } else {
            None
        }
    }).collect();
    let running = active.iter().filter(|j| j["status"] == "running").count();
    let shown   = active.len();
    json!({ "jobs": active, "running": running, "shown": shown })
}

/// chips: scan processor_lab — auto-detect from cwd or fall back to ~/processor_lab.
fn orient_chips(cwd: &str) -> Value {
    let lab_path = find_processor_lab(cwd).unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home".to_owned());
        format!("{}/processor_lab", home)
    });
    match project::chip_status(&json!({ "lab_path": lab_path })) {
        Ok(tr)  => parse_tool_result_json(&tr),
        Err(e)  => json!({ "error": e }),
    }
}

/// Walk up from cwd to find a directory named "processor_lab".
fn find_processor_lab(cwd: &str) -> Option<String> {
    let mut p = std::path::PathBuf::from(cwd);
    loop {
        if p.file_name().map(|n| n == "processor_lab").unwrap_or(false) {
            return Some(p.display().to_string());
        }
        if !p.pop() { break; }
    }
    // Fall back: check ~/processor_lab exists
    let home = std::env::var("HOME").ok()?;
    let lab  = std::path::PathBuf::from(&home).join("processor_lab");
    if lab.join("chips").exists() { Some(lab.display().to_string()) } else { None }
}

/// synth: parse Vivado timing/utilization reports found near cwd.
fn orient_synth(cwd: &str) -> Value {
    match eda::synth_report(&json!({ "project_dir": cwd })) {
        Ok(tr)  => parse_tool_result_json(&tr),
        Err(e)  => json!({ "error": e }),
    }
}

/// diff: compact `git diff --stat` for dirty repos (no full hunks).
fn orient_diff(cwd: &str) -> Value {
    let out = std::process::Command::new("git")
        .args(["-C", cwd, "diff", "--stat", "--no-color"])
        .output();
    match out {
        Ok(o) => {
            let stat = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            if stat.is_empty() {
                json!({ "stat": "", "note": "working tree clean" })
            } else {
                // Last line is the summary "N files changed, …" — rest are per-file lines
                let lines: Vec<&str> = stat.lines().collect();
                let summary_line = lines.last().map(|s| s.trim()).unwrap_or("").to_owned();
                json!({ "stat": stat, "summary": summary_line })
            }
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}
