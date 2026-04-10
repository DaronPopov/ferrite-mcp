//! CUDA workflow hardening tools.
//!
//! Tools:
//!   cuda_env_doctor — verify CUDA/GPU toolchain readiness
//!   cuda_artifacts  — inventory source/build/profile artifacts for a CUDA project
//!   cuda_triage     — classify CUDA failures and recommend next action/tool
//!   cuda_regression_run — run a compact CUDA validation flow
//!   cuda_regression_report — summarize CUDA validation with artifacts + triage

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::execution::run;
use crate::tools::state::resolve_or_cwd;
use crate::tools::walk::{self, FileType, WalkOptions};

pub fn cuda_env_doctor(
    args: &Value,
    state: &Arc<Mutex<ServerState>>,
) -> Result<ToolResult, String> {
    let base = resolve_or_cwd(state, args["path"].as_str())?;

    let nvcc = probe_bin("nvcc", &["--version"]);
    let nvidia_smi = probe_bin(
        "nvidia-smi",
        &[
            "--query-gpu=name,driver_version,memory.total",
            "--format=csv,noheader",
        ],
    );
    let ncu = probe_bin("ncu", &["--version"]);
    let compute_sanitizer = probe_bin("compute-sanitizer", &["--version"]);
    let cuobjdump = probe_bin("cuobjdump", &["--version"]);

    let ready =
        nvcc["found"].as_bool() == Some(true) && nvidia_smi["found"].as_bool() == Some(true);

    let gpu_ready = if nvidia_smi["found"].as_bool() == Some(true) {
        "ready"
    } else {
        "missing_driver_or_gpu"
    };

    Ok(ToolResult::json(&json!({
        "path": base.display().to_string(),
        "ready": ready,
        "gpu_ready": gpu_ready,
        "tools": {
            "nvcc": nvcc,
            "nvidia_smi": nvidia_smi,
            "ncu": ncu,
            "compute_sanitizer": compute_sanitizer,
            "cuobjdump": cuobjdump,
        },
        "recommended_next_action": if ready { "run_cuda_build_or_benchmark" } else { "install_or_fix_cuda_toolchain" },
    })))
}

pub fn cuda_artifacts(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let root = resolve_or_cwd(state, args["path"].as_str())?;
    let max_results = args["max_results"].as_u64().unwrap_or(20) as usize;

    let source_files = collect_files(&root, &["cu", "cuh", "ptx"], max_results);
    let build_outputs = collect_named_files(&root.join("build"), max_results);
    let profile_reports = collect_files(
        &root,
        &["ncu-rep", "nsys-rep", "qdrep", "sqlite", "json"],
        max_results,
    );
    let libraries = collect_matching_names(&root.join("build"), &[".so", ".a"], max_results);
    let binaries = collect_executables(&root.join("build"), max_results);

    let artifact_count = source_files.len()
        + build_outputs.len()
        + profile_reports.len()
        + libraries.len()
        + binaries.len();

    Ok(ToolResult::json(&json!({
        "path": root.display().to_string(),
        "artifact_count": artifact_count,
        "artifacts": {
            "sources": source_files,
            "build_outputs": build_outputs,
            "profile_reports": profile_reports,
            "libraries": libraries,
            "binaries": binaries,
        }
    })))
}

pub fn cuda_triage(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let phase = args["phase"].as_str().unwrap_or("runtime");

    let (stdout, stderr, exit_code, cmd, cwd) = if let Some(cmd) = args["cmd"].as_str() {
        let cwd = resolve_or_cwd(state, args["cwd"].as_str())?;
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(120);
        let raw = run(cmd, &cwd, &[], "", Duration::from_secs(timeout_secs));
        let stdout = raw["stdout"]
            .as_str()
            .or_else(|| raw["out"].as_str())
            .unwrap_or("")
            .to_owned();
        let stderr = raw["stderr"].as_str().unwrap_or("").to_owned();
        let exit_code = raw["exit_code"]
            .as_i64()
            .unwrap_or(if raw["ok"].as_bool() == Some(true) {
                0
            } else {
                -1
            });
        (
            stdout,
            stderr,
            exit_code,
            Some(cmd.to_owned()),
            Some(cwd.display().to_string()),
        )
    } else {
        (
            args["stdout"].as_str().unwrap_or("").to_owned(),
            args["stderr"].as_str().unwrap_or("").to_owned(),
            args["exit_code"].as_i64().unwrap_or(-1),
            None,
            None,
        )
    };

    let triage = classify_cuda_failure(phase, &stdout, &stderr, exit_code);

    Ok(ToolResult::json(&json!({
        "phase": phase,
        "cmd": cmd,
        "cwd": cwd,
        "exit_code": exit_code,
        "triage": triage,
    })))
}

pub fn cuda_regression_run(
    args: &Value,
    state: &Arc<Mutex<ServerState>>,
) -> Result<ToolResult, String> {
    let root = resolve_or_cwd(state, args["path"].as_str())?;
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);
    let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(120);
    let test_target = args["test_target"].as_str();
    let benchmark_cmd = args["benchmark_cmd"].as_str();
    let steps: Vec<&str> = args["steps"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_else(|| vec!["env", "test"]);

    let build_dir = root.join("build");
    let ctest_tests = discover_ctest_tests(&build_dir);

    if dry_run {
        let plan: Vec<Value> = steps
            .iter()
            .map(|step| {
                json!({
                    "step": step,
                    "cmd": cuda_step_cmd(step, &root, test_target, benchmark_cmd, &ctest_tests)
                })
            })
            .collect();
        return Ok(ToolResult::json(&json!({
            "path": root.display().to_string(),
            "dry_run": true,
            "plan": plan,
        })));
    }

    let mut step_results = Vec::new();
    let mut overall_success = true;
    for step in &steps {
        let cmd = cuda_step_cmd(step, &root, test_target, benchmark_cmd, &ctest_tests);
        if cmd.is_empty() {
            step_results.push(json!({ "step": step, "skipped": true, "reason": "unsupported or unresolved step" }));
            continue;
        }
        let raw = run(&cmd, &root, &[], "", Duration::from_secs(timeout_secs));
        let success = raw["success"]
            .as_bool()
            .or_else(|| raw["ok"].as_bool())
            .unwrap_or(false);
        if !success {
            overall_success = false;
        }
        step_results.push(json!({
            "step": step,
            "success": success,
            "cmd": cmd,
            "exit_code": raw["exit_code"].as_i64().unwrap_or(if success { 0 } else { -1 }),
            "stdout": raw["stdout"].as_str().or_else(|| raw["out"].as_str()).unwrap_or(""),
            "stderr": raw["stderr"].as_str().unwrap_or(""),
        }));
        if !success {
            break;
        }
    }

    Ok(ToolResult::json(&json!({
        "path": root.display().to_string(),
        "overall_success": overall_success,
        "steps": step_results,
        "ctest_tests": ctest_tests,
    })))
}

pub fn cuda_regression_report(
    args: &Value,
    state: &Arc<Mutex<ServerState>>,
) -> Result<ToolResult, String> {
    let root = resolve_or_cwd(state, args["path"].as_str())?;
    let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(120);
    let test_target = args["test_target"].as_str();
    let benchmark_cmd = args["benchmark_cmd"].as_str();
    let include_logs = args["include_logs"].as_bool().unwrap_or(false);
    let steps: Vec<&str> = args["steps"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_else(|| vec!["env", "test"]);

    let build_dir = root.join("build");
    let ctest_tests = discover_ctest_tests(&build_dir);

    let mut step_results = Vec::new();
    let mut overall_success = true;
    for step in &steps {
        let cmd = cuda_step_cmd(step, &root, test_target, benchmark_cmd, &ctest_tests);
        if cmd.is_empty() {
            step_results.push(json!({ "step": step, "skipped": true, "reason": "unsupported or unresolved step" }));
            continue;
        }
        let raw = run(&cmd, &root, &[], "", Duration::from_secs(timeout_secs));
        let stdout = raw["stdout"]
            .as_str()
            .or_else(|| raw["out"].as_str())
            .unwrap_or("")
            .to_owned();
        let stderr = raw["stderr"].as_str().unwrap_or("").to_owned();
        let exit_code = raw["exit_code"]
            .as_i64()
            .unwrap_or(if raw["ok"].as_bool() == Some(true) {
                0
            } else {
                -1
            });
        let success = raw["success"]
            .as_bool()
            .or_else(|| raw["ok"].as_bool())
            .unwrap_or(false);
        if !success {
            overall_success = false;
        }
        let triage = classify_cuda_failure(step, &stdout, &stderr, exit_code);
        let result = if include_logs {
            json!({
                "step": step,
                "success": success,
                "cmd": cmd,
                "exit_code": exit_code,
                "stdout": stdout,
                "stderr": stderr,
                "triage": triage,
            })
        } else {
            json!({
                "step": step,
                "success": success,
                "cmd": cmd,
                "exit_code": exit_code,
                "stdout_preview": truncate_preview(&stdout, 220),
                "stderr_preview": truncate_preview(&stderr, 220),
                "triage": triage,
            })
        };
        step_results.push(result);
        if !success {
            break;
        }
    }

    Ok(ToolResult::json(&json!({
        "path": root.display().to_string(),
        "overall_success": overall_success,
        "summary": cuda_regression_summary(&step_results),
        "artifacts": cuda_artifacts_json(&root, 12),
        "ctest_tests": ctest_tests,
        "steps": step_results,
    })))
}

fn probe_bin(name: &str, version_args: &[&str]) -> Value {
    let Some(path) = which_bin(name) else {
        return json!({ "found": false, "path": null, "version": null });
    };
    let output = Command::new(&path).args(version_args).output().ok();
    let version = output
        .as_ref()
        .map(|o| {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            text.lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_owned()
        })
        .unwrap_or_default();
    json!({
        "found": true,
        "path": path,
        "version": version,
    })
}

fn which_bin(name: &str) -> Option<String> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    path_var
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join(name))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .map(|p| p.display().to_string())
}

fn collect_files(root: &Path, exts: &[&str], max_results: usize) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, &mut |path| {
        if out.len() >= max_results {
            return;
        }
        if let Some(ext) = path.extension().and_then(|v| v.to_str()) {
            if exts.contains(&ext) {
                out.push(path.display().to_string());
            }
        }
    });
    out.sort();
    out
}

fn collect_matching_names(root: &Path, suffixes: &[&str], max_results: usize) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, &mut |path| {
        if out.len() >= max_results {
            return;
        }
        let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
        if suffixes.iter().any(|suffix| name.ends_with(suffix)) {
            out.push(path.display().to_string());
        }
    });
    out.sort();
    out
}

fn collect_named_files(root: &Path, max_results: usize) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, &mut |path| {
        if out.len() >= max_results {
            return;
        }
        if path.is_file() {
            out.push(path.display().to_string());
        }
    });
    out.sort();
    out
}

fn collect_executables(root: &Path, max_results: usize) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, &mut |path| {
        if out.len() >= max_results {
            return;
        }
        if let Ok(meta) = path.metadata() {
            use std::os::unix::fs::PermissionsExt;
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
                if !name.ends_with(".so") && !name.ends_with(".a") {
                    out.push(path.display().to_string());
                }
            }
        }
    });
    out.sort();
    out
}

/// Drive a callback over every file under `root`. Wraps the unified
/// `tools::walk` so artifact discovery uses the same gitignore-aware
/// walker as the rest of ferrite. Bound to files only — directories
/// pruned by ignore rules just don't appear.
fn walk(root: &Path, f: &mut impl FnMut(&Path)) {
    if !root.exists() {
        return;
    }
    let opts = WalkOptions {
        root: root.to_path_buf(),
        ..WalkOptions::default()
    };
    let Ok(iter) = walk::walk_sequential(&opts) else {
        return;
    };
    for entry in iter {
        if matches!(entry.file_type, FileType::File) {
            f(&entry.path);
        }
    }
}

fn classify_cuda_failure(phase: &str, stdout: &str, stderr: &str, exit_code: i64) -> Value {
    let combined = format!("{stdout}\n{stderr}").to_lowercase();
    let (status, severity, root_cause, confidence, next_action, tool) = if exit_code == 0 {
        (
            "green",
            "info",
            "none",
            "high",
            "continue",
            "cuda_artifacts",
        )
    } else if combined.contains("nvcc fatal") || combined.contains("error:") && phase == "build" {
        (
            "red",
            "high",
            "compile_error",
            "high",
            "inspect_build_error",
            "build_check",
        )
    } else if combined.contains("cannot find -l")
        || combined.contains("undefined reference")
        || combined.contains("cannot open shared object file")
    {
        (
            "red",
            "high",
            "link_or_library_error",
            "high",
            "inspect_library_resolution",
            "find_lib",
        )
    } else if combined.contains("illegal memory access") || combined.contains("out of bounds") {
        (
            "red",
            "high",
            "illegal_memory_access",
            "high",
            "run_compute_sanitizer",
            "compute_sanitizer",
        )
    } else if combined.contains("race") || combined.contains("barrier") {
        (
            "red",
            "high",
            "sync_or_race_error",
            "medium",
            "run_compute_sanitizer",
            "compute_sanitizer",
        )
    } else if combined.contains("err_nvgpuctrperm")
        || combined.contains("permission denied") && combined.contains("ncu")
    {
        (
            "yellow",
            "medium",
            "profiling_permission_error",
            "high",
            "fix_perf_permissions",
            "ncu_profile",
        )
    } else if combined.contains("no kernel image is available")
        || combined.contains("unsupported gpu architecture")
    {
        (
            "red",
            "high",
            "arch_mismatch",
            "high",
            "inspect_cuda_arch_flags",
            "build_check",
        )
    } else if combined.contains("timed out") {
        (
            "yellow",
            "medium",
            "runtime_timeout",
            "medium",
            "reduce_problem_size_or_profile",
            "cuda_triage",
        )
    } else {
        (
            "yellow",
            "medium",
            "unknown_cuda_failure",
            "low",
            "inspect_output",
            "cuda_triage",
        )
    };

    json!({
        "status": status,
        "severity": severity,
        "root_cause": root_cause,
        "confidence": confidence,
        "recommended_next_action": next_action,
        "recommended_tool": tool,
        "notes": extract_notes(stdout, stderr),
    })
}

fn cuda_step_cmd(
    step: &str,
    root: &Path,
    test_target: Option<&str>,
    benchmark_cmd: Option<&str>,
    ctest_tests: &[String],
) -> String {
    match step {
        "env" => "nvidia-smi --query-gpu=name,driver_version --format=csv,noheader".to_owned(),
        "test" => {
            if root.join("build/CTestTestfile.cmake").exists() {
                if let Some(target) = test_target {
                    format!(
                        "ctest --test-dir {} --output-on-failure -R '^{}$'",
                        root.join("build").display(),
                        target
                    )
                } else if let Some(first) = ctest_tests.first() {
                    format!(
                        "ctest --test-dir {} --output-on-failure -R '^{}$'",
                        root.join("build").display(),
                        first
                    )
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        }
        "benchmark" => benchmark_cmd.map(str::to_owned).unwrap_or_default(),
        _ => String::new(),
    }
}

fn discover_ctest_tests(build_dir: &Path) -> Vec<String> {
    let path = build_dir.join("CTestTestfile.cmake");
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut tests = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("add_test([=[") {
            if let Some(name) = rest.split("]=]").next() {
                tests.push(name.to_owned());
            }
        }
    }
    tests
}

fn cuda_artifacts_json(root: &Path, max_results: usize) -> Value {
    let source_files = collect_files(root, &["cu", "cuh", "ptx"], max_results);
    let build_outputs = collect_named_files(&root.join("build"), max_results);
    let profile_reports = collect_files(
        root,
        &["ncu-rep", "nsys-rep", "qdrep", "sqlite", "json"],
        max_results,
    );
    let libraries = collect_matching_names(&root.join("build"), &[".so", ".a"], max_results);
    let binaries = collect_executables(&root.join("build"), max_results);
    json!({
        "sources": source_files,
        "build_outputs": build_outputs,
        "profile_reports": profile_reports,
        "libraries": libraries,
        "binaries": binaries,
    })
}

fn cuda_regression_summary(step_results: &[Value]) -> Value {
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
                "root_cause": s["triage"]["root_cause"],
                "recommended_tool": s["triage"]["recommended_tool"],
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

fn truncate_preview(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let trimmed: String = s.chars().take(max_chars).collect();
    format!("{trimmed}...")
}

fn extract_notes(stdout: &str, stderr: &str) -> Vec<String> {
    let combined = format!("{stdout}\n{stderr}");
    let mut notes = Vec::new();
    for line in combined
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(4)
    {
        notes.push(line.to_owned());
    }
    if notes.is_empty() {
        notes.push("No diagnostic lines captured.".to_owned());
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::classify_cuda_failure;
    use std::fs;

    #[test]
    fn classify_cuda_failure_detects_illegal_memory_access() {
        let triage = classify_cuda_failure(
            "runtime",
            "",
            "CUDA error: an illegal memory access was encountered",
            1,
        );
        assert_eq!(triage["root_cause"], "illegal_memory_access");
        assert_eq!(triage["recommended_tool"], "compute_sanitizer");
    }

    #[test]
    fn classify_cuda_failure_detects_compile_error() {
        let triage = classify_cuda_failure(
            "build",
            "",
            "nvcc fatal   : Unsupported gpu architecture 'compute_99'",
            1,
        );
        assert_eq!(triage["root_cause"], "compile_error");
        assert_eq!(triage["recommended_tool"], "build_check");
    }

    #[test]
    fn discover_ctest_tests_parses_names() {
        let dir = std::env::temp_dir().join(format!("ferrite_cuda_ctest_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("CTestTestfile.cmake"),
            "add_test([=[test_one]=] \"/tmp/test_one\")\nadd_test([=[test_two]=] \"/tmp/test_two\")\n",
        ).unwrap();
        let tests = super::discover_ctest_tests(&dir);
        assert_eq!(tests, vec!["test_one".to_owned(), "test_two".to_owned()]);
        let _ = fs::remove_dir_all(&dir);
    }
}
