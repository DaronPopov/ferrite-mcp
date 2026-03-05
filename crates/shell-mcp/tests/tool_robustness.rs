//! Integration tests for MCP tool robustness: timeouts, non-repo paths, blocking binaries.

use serde_json::json;

use shell_mcp::tools::filesystem::which_bin;
use shell_mcp::tools::git::{git_log, git_status, git_diff};

fn result_json(r: &shell_mcp::protocol::ToolResult) -> serde_json::Value {
    serde_json::from_str(&r.content[0].text).unwrap_or(json!({}))
}

// ── which ────────────────────────────────────────────────────────────────────

#[test]
fn which_finds_git() {
    let result = which_bin(&json!({ "name": "git" })).unwrap();
    let v = result_json(&result);
    assert!(v["found"].as_bool().unwrap(), "git should be found on PATH");
    assert!(v["path"].as_str().unwrap().contains("git"));
}

#[test]
fn which_not_found() {
    let result = which_bin(&json!({ "name": "nonexistent_binary_xyz_123" })).unwrap();
    let v = result_json(&result);
    assert!(!v["found"].as_bool().unwrap());
}

#[test]
fn which_handles_blocking_binary() {
    use std::io::Write;
    let dir = std::env::temp_dir().join("ferrite_test_which");
    let _ = std::fs::create_dir_all(&dir);
    let fake_bin = dir.join("fake_hang_bin");
    {
        let mut f = std::fs::File::create(&fake_bin).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "sleep 999").unwrap();
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let orig_path = std::env::var("PATH").unwrap_or_default();
    unsafe { std::env::set_var("PATH", format!("{}:{}", dir.display(), orig_path)); }

    let start = std::time::Instant::now();
    let result = which_bin(&json!({ "name": "fake_hang_bin" })).unwrap();
    let elapsed = start.elapsed();

    unsafe { std::env::set_var("PATH", &orig_path); }
    let _ = std::fs::remove_dir_all(&dir);

    let v = result_json(&result);
    assert!(v["found"].as_bool().unwrap(), "blocking binary should still be found");
    assert!(v["version"].is_null(), "version should be null for blocking binary");
    assert!(elapsed.as_secs() < 10, "which must not hang; took {}s", elapsed.as_secs());
}

#[test]
fn which_handles_binary_that_leaks_stdio_fds() {
    use std::io::Write;
    let dir = std::env::temp_dir().join("ferrite_test_which_leaked_stdio");
    let _ = std::fs::create_dir_all(&dir);
    let fake_bin = dir.join("fake_leaky_bin");
    let pid_file = dir.join("fake_leaky_bin.pid");
    {
        let mut f = std::fs::File::create(&fake_bin).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "(sleep 999) &").unwrap();
        writeln!(f, "echo $! > '{}'", pid_file.display()).unwrap();
        writeln!(f, "echo 'fake-leaky-bin 1.0'").unwrap();
        writeln!(f, "exit 0").unwrap();
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let orig_path = std::env::var("PATH").unwrap_or_default();
    unsafe { std::env::set_var("PATH", format!("{}:{}", dir.display(), orig_path)); }

    let start = std::time::Instant::now();
    let result = which_bin(&json!({ "name": "fake_leaky_bin" })).unwrap();
    let elapsed = start.elapsed();

    unsafe { std::env::set_var("PATH", &orig_path); }
    if let Ok(pid) = std::fs::read_to_string(&pid_file) {
        let _ = std::process::Command::new("kill").arg(pid.trim()).status();
    }
    let _ = std::fs::remove_dir_all(&dir);

    let v = result_json(&result);
    assert!(v["found"].as_bool().unwrap(), "leaky binary should still be found");
    assert_eq!(v["version"].as_str(), Some("fake-leaky-bin 1.0"));
    assert!(elapsed.as_secs() < 10, "which must not hang; took {}s", elapsed.as_secs());
}

// ── git tools on non-repo path ──────────────────────────────────────────────

#[test]
fn git_status_non_repo_returns_error() {
    let result = git_status(&json!({ "path": "/tmp" }));
    match result {
        Err(e) => assert!(e.contains("not inside a git repository") || e.contains("timed out")),
        Ok(r) => assert!(r.is_error, "should be an error result for non-repo"),
    }
}

#[test]
fn git_log_non_repo_returns_error() {
    let result = git_log(&json!({ "path": "/tmp" }));
    match result {
        Err(e) => assert!(e.contains("not inside a git repository") || e.contains("timed out")),
        Ok(r) => assert!(r.is_error, "should be an error result for non-repo"),
    }
}

#[test]
fn git_diff_non_repo_returns_error() {
    let result = git_diff(&json!({ "path": "/tmp" }));
    match result {
        Err(e) => assert!(e.contains("not inside a git repository") || e.contains("timed out")),
        Ok(r) => assert!(r.is_error, "should be an error result for non-repo"),
    }
}

// ── git tools on a real repo ────────────────────────────────────────────────

#[test]
fn git_status_on_real_repo() {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if p.join(".git").exists() { break; }
        if !p.pop() { return; } // no git repo — skip
    }

    let result = git_status(&json!({ "path": p.to_str().unwrap() })).unwrap();
    let v = result_json(&result);
    assert!(!result.is_error, "git_status should succeed on a real repo");
    assert!(v["branch"].is_string(), "should have a branch field");
}
