//! cargo_tree — Rust workspace structure + dependency graph.
//!
//! Uses `cargo metadata` (JSON output) to return the full picture of a
//! workspace: members, versions, features, and resolved dep trees.
//! This replaces manually reading multiple Cargo.toml files.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::state::read_cwd;
use std::sync::{Arc, Mutex};

pub fn cargo_tree(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let state_cwd = read_cwd(state);

    let root_arg = args["path"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or(state_cwd);
    let full_deps = args["full"].as_bool().unwrap_or(false);

    // Find the workspace root from the given path
    let manifest = find_manifest(&root_arg).ok_or_else(|| {
        format!(
            "cargo_tree: no Cargo.toml found at or above {}",
            root_arg.display()
        )
    })?;

    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("metadata")
        .arg("--format-version")
        .arg("1")
        .arg("--manifest-path")
        .arg(&manifest);

    if !full_deps {
        cmd.arg("--no-deps");
    }

    let out = cmd
        .output()
        .map_err(|e| format!("cargo metadata failed: {e}"))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Ok(ToolResult::error(format!("cargo metadata error:\n{err}")));
    }

    let meta: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("failed to parse cargo metadata: {e}"))?;

    let workspace_root = meta["workspace_root"].as_str().unwrap_or("").to_owned();
    let workspace_members: Vec<&str> = meta["workspace_members"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    // Build a clean summary per package
    let packages: Vec<Value> = meta["packages"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter(|pkg| {
            // Only include workspace members (filter out deps when --no-deps is on anyway)
            let id = pkg["id"].as_str().unwrap_or("");
            workspace_members.iter().any(|m| *m == id)
        })
        .map(|pkg| summarise_package(pkg, full_deps, &meta))
        .collect();

    let _workspace_default_members = meta["workspace_default_members"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    Ok(ToolResult::json(&json!({
        "workspace_root": workspace_root,
        "is_workspace": workspace_members.len() > 1,
        "member_count": packages.len(),
        "packages": packages,
        "rust_edition": packages.first().and_then(|p| p["edition"].as_str()),
    })))
}

fn summarise_package(pkg: &Value, include_deps: bool, meta: &Value) -> Value {
    let name = pkg["name"].as_str().unwrap_or("");
    let version = pkg["version"].as_str().unwrap_or("");
    let edition = pkg["edition"].as_str().unwrap_or("");
    let manifest_path = pkg["manifest_path"].as_str().unwrap_or("");

    // Feature flags
    let features: Vec<&str> = pkg["features"]
        .as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();

    // Direct dependencies
    let deps: Vec<Value> = pkg["dependencies"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|d| {
            let dep_name = d["name"].as_str().unwrap_or("");
            let req = d["req"].as_str().unwrap_or("*");
            let kind = d["kind"].as_str().unwrap_or("normal");
            let feats: Vec<&str> = d["features"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();

            // Look up resolved version if full metadata available
            let resolved_ver = if include_deps {
                find_resolved_version(meta, dep_name)
            } else {
                None
            };

            json!({
                "name":     dep_name,
                "req":      req,
                "kind":     kind,
                "features": feats,
                "resolved": resolved_ver,
            })
        })
        .collect();

    // Targets (lib, bins, tests)
    let targets: Vec<Value> = pkg["targets"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|t| {
            json!({
                "name": t["name"],
                "kind": t["kind"],
                "src":  t["src_path"],
            })
        })
        .collect();

    json!({
        "name":          name,
        "version":       version,
        "edition":       edition,
        "manifest_path": manifest_path,
        "features":      features,
        "dependencies":  deps,
        "dep_count":     deps.len(),
        "targets":       targets,
    })
}

fn find_resolved_version(meta: &Value, name: &str) -> Option<String> {
    meta["packages"]
        .as_array()?
        .iter()
        .find(|p| p["name"].as_str() == Some(name))
        .and_then(|p| p["version"].as_str())
        .map(str::to_owned)
}

// ── test_run ──────────────────────────────────────────────────────────────────

/// Run `cargo test` and return structured {passed, failed, ignored, duration_ms}.
/// Optional filter narrows to matching test names.
pub fn test_run(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let filter = args["filter"].as_str().unwrap_or("");
    let package = args["package"].as_str();
    let timeout = args["timeout_secs"].as_u64().unwrap_or(120);

    let cwd = read_cwd(state);

    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("test");

    if let Some(pkg) = package {
        cmd.args(["--package", pkg]);
    }
    if !filter.is_empty() {
        cmd.arg(filter);
    }

    // -- separates cargo args from test binary args
    cmd.args(["--", "--test-output=immediate"]);

    cmd.current_dir(&cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(timeout);

    let mut child = cmd.spawn().map_err(|e| format!("cargo test: {e}"))?;

    loop {
        match child.try_wait().map_err(|e| format!("try_wait: {e}"))? {
            Some(_) => break,
            None => {
                if std::time::Instant::now() > deadline {
                    child.kill().ok();
                    return Ok(ToolResult::error(format!(
                        "cargo test timed out after {timeout}s"
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

    let (passed, failed, ignored) = parse_test_output(&combined);
    let success = out.status.success();

    Ok(ToolResult::json(&json!({
        "success":     success,
        "passed":      passed,
        "failed":      failed,
        "ignored":     ignored,
        "duration_ms": duration_ms,
        "filter":      filter,
        "output":      combined,
    })))
}

fn parse_test_output(output: &str) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let mut passed = Vec::new();
    let mut failed = Vec::new();
    let mut ignored = Vec::new();

    // Format: "test module::name ... ok" or "... FAILED" or "... ignored"
    for line in output.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("test ") {
            continue;
        }

        // Skip "test result: ..." summary line
        if trimmed.starts_with("test result:") {
            continue;
        }

        if let Some(name_part) = trimmed.strip_prefix("test ") {
            if let Some((name, outcome)) = name_part.rsplit_once(" ... ") {
                let name = name.trim().to_owned();
                match outcome.trim() {
                    "ok" => passed.push(json!({ "name": name })),
                    "FAILED" => failed.push(json!({ "name": name })),
                    "ignored" => ignored.push(json!({ "name": name })),
                    _ => {}
                }
            }
        }
    }

    (passed, failed, ignored)
}

fn find_manifest(from: &Path) -> Option<PathBuf> {
    // If it's already a Cargo.toml, use it
    if from.file_name().map(|n| n == "Cargo.toml").unwrap_or(false) {
        return Some(from.to_owned());
    }
    // Walk up to find one
    let mut dir = if from.is_file() { from.parent()? } else { from };
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
}
