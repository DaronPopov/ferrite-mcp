//! Integration tests for MCP tool robustness: timeouts, non-repo paths, blocking binaries.

use serde_json::json;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::Duration;

use shell_mcp::job_store::JobStore;
use shell_mcp::persist::Persistence;
use shell_mcp::pipeline::PipelineStore;
use shell_mcp::server::ServerState;
use shell_mcp::tools::bg_pipeline::pipeline_run;
use shell_mcp::tools::bg_pipeline::pipeline_status;
use shell_mcp::tools::bg_query::bg_status;
use shell_mcp::tools::bg_spawn::bg_spawn;
use shell_mcp::tools::code::{grep_code, read_context};
use shell_mcp::tools::filesystem::which_bin;
use shell_mcp::tools::filesystem::{changed_since, glob_files, list_dir, read_file};
use shell_mcp::tools::git::{git_diff, git_log, git_status};
use shell_mcp::tools::github::gh_status;
use shell_mcp::tools::ml::checkpoint_list;
use shell_mcp::tools::project::project_context;
use shell_mcp::tools::state::shell_state;
use shell_mcp::tools::symbols::symbol_index;
use shell_mcp::tools::tty_exec::tty_exec;
use shell_mcp::tools::workspace::note;

fn result_json(r: &shell_mcp::protocol::ToolResult) -> serde_json::Value {
    serde_json::from_str(&r.content[0].text).unwrap_or(json!({}))
}

fn state_with_cwd(cwd: &std::path::Path) -> Arc<Mutex<ServerState>> {
    let mut server_state = ServerState::default();
    *server_state.cwd.get_mut().expect("test cwd lock") = cwd.to_path_buf();
    Arc::new(Mutex::new(server_state))
}

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env lock")
}

// ── which ────────────────────────────────────────────────────────────────────

#[test]
fn which_finds_git() {
    let _guard = lock_env();
    let result = which_bin(&json!({ "name": "git" })).unwrap();
    let v = result_json(&result);
    assert!(v["found"].as_bool().unwrap(), "git should be found on PATH");
    assert!(v["path"].as_str().unwrap().contains("git"));
}

#[test]
fn which_not_found() {
    let _guard = lock_env();
    let result = which_bin(&json!({ "name": "nonexistent_binary_xyz_123" })).unwrap();
    let v = result_json(&result);
    assert!(!v["found"].as_bool().unwrap());
}

#[test]
fn which_handles_blocking_binary() {
    let _guard = lock_env();
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
    unsafe {
        std::env::set_var("PATH", format!("{}:{}", dir.display(), orig_path));
    }

    let start = std::time::Instant::now();
    let result = which_bin(&json!({ "name": "fake_hang_bin" })).unwrap();
    let elapsed = start.elapsed();

    unsafe {
        std::env::set_var("PATH", &orig_path);
    }
    let _ = std::fs::remove_dir_all(&dir);

    let v = result_json(&result);
    assert!(
        v["found"].as_bool().unwrap(),
        "blocking binary should still be found"
    );
    assert!(
        v["version"].is_null(),
        "version should be null for blocking binary"
    );
    assert!(
        elapsed.as_secs() < 10,
        "which must not hang; took {}s",
        elapsed.as_secs()
    );
}

#[test]
fn which_handles_binary_that_leaks_stdio_fds() {
    let _guard = lock_env();
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
    unsafe {
        std::env::set_var("PATH", format!("{}:{}", dir.display(), orig_path));
    }

    let start = std::time::Instant::now();
    let result = which_bin(&json!({ "name": "fake_leaky_bin" })).unwrap();
    let elapsed = start.elapsed();

    unsafe {
        std::env::set_var("PATH", &orig_path);
    }
    if let Ok(pid) = std::fs::read_to_string(&pid_file) {
        let _ = std::process::Command::new("kill").arg(pid.trim()).status();
    }
    let _ = std::fs::remove_dir_all(&dir);

    let v = result_json(&result);
    assert!(
        v["found"].as_bool().unwrap(),
        "leaky binary should still be found"
    );
    assert_eq!(v["version"].as_str(), Some("fake-leaky-bin 1.0"));
    assert!(
        elapsed.as_secs() < 10,
        "which must not hang; took {}s",
        elapsed.as_secs()
    );
}

// ── git tools on non-repo path ──────────────────────────────────────────────

#[test]
fn git_status_non_repo_returns_error() {
    let state = Arc::new(Mutex::new(ServerState::default()));
    let result = git_status(&json!({ "path": "/tmp" }), &state);
    match result {
        Err(e) => assert!(e.contains("not inside a git repository") || e.contains("timed out")),
        Ok(r) => assert!(r.is_error, "should be an error result for non-repo"),
    }
}

#[test]
fn git_log_non_repo_returns_error() {
    let state = Arc::new(Mutex::new(ServerState::default()));
    let result = git_log(&json!({ "path": "/tmp" }), &state);
    match result {
        Err(e) => assert!(e.contains("not inside a git repository") || e.contains("timed out")),
        Ok(r) => assert!(r.is_error, "should be an error result for non-repo"),
    }
}

#[test]
fn git_diff_non_repo_returns_error() {
    let state = Arc::new(Mutex::new(ServerState::default()));
    let result = git_diff(&json!({ "path": "/tmp" }), &state);
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
        if p.join(".git").exists() {
            break;
        }
        if !p.pop() {
            return;
        } // no git repo — skip
    }

    let state = Arc::new(Mutex::new(ServerState::default()));
    let result = git_status(&json!({ "path": p.to_str().unwrap() }), &state).unwrap();
    let v = result_json(&result);
    assert!(!result.is_error, "git_status should succeed on a real repo");
    assert!(v["branch"].is_string(), "should have a branch field");
}

#[test]
fn git_status_defaults_to_server_cwd() {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        if p.join(".git").exists() {
            break;
        }
        if !p.pop() {
            return;
        }
    }

    let mut server_state = ServerState::default();
    *server_state.cwd.get_mut().expect("test cwd lock") = p;
    let state = Arc::new(Mutex::new(server_state));
    let result = git_status(&json!({}), &state).unwrap();
    let v = result_json(&result);
    assert!(
        !result.is_error,
        "git_status should honor server cwd when path is omitted"
    );
    assert!(v["branch"].is_string(), "should have a branch field");
}

#[test]
fn gh_status_discovers_repos_under_custom_root() {
    let root = std::env::temp_dir().join(format!("ferrite_gh_status_{}", std::process::id()));
    let repo = root.join("child_repo");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&repo).unwrap();
    std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&repo)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "ferrite"])
        .current_dir(&repo)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "ferrite@local"])
        .current_dir(&repo)
        .output()
        .unwrap();
    std::fs::write(repo.join("README.md"), "test\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "README.md"])
        .current_dir(&repo)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&repo)
        .output()
        .unwrap();

    let state = Arc::new(Mutex::new(ServerState::default()));
    let result = gh_status(&json!({ "paths": [root.display().to_string()] }), &state).unwrap();
    let v = result_json(&result);
    assert_eq!(
        v["count"].as_u64(),
        Some(1),
        "gh_status should discover nested repos under custom roots"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn poisoned_server_state_recovers_for_common_tools() {
    let state = state_with_cwd(std::env::temp_dir().as_path());

    let _ = std::panic::catch_unwind({
        let state = Arc::clone(&state);
        move || {
            let _guard = state.lock().unwrap();
            panic!("intentional poison");
        }
    });

    let shell = shell_state(&json!({}), &state).expect("shell_state should recover from poison");
    let shell_json = result_json(&shell);
    assert!(
        shell_json["cwd"].is_string(),
        "shell_state should still return cwd"
    );

    let note_result = note(&json!({ "op": "append", "content": "still alive" }), &state)
        .expect("note should recover from poison");
    let note_json = result_json(&note_result);
    assert_eq!(note_json["ok"].as_bool(), Some(true));
    assert_eq!(note_json["count"].as_u64(), Some(1));
}

#[test]
fn poisoned_job_mutexes_do_not_break_bg_status() {
    let root = std::env::temp_dir().join(format!("ferrite_poison_job_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let state = state_with_cwd(&root);
    let store = Arc::new(JobStore::new(Arc::new(Persistence::new())));

    let spawned = bg_spawn(&json!({ "cmd": "printf hello" }), &store, &state).unwrap();
    let spawned_json = result_json(&spawned);
    let job_id = spawned_json["job_id"].as_str().unwrap().to_string();
    let job = store.get(&job_id).expect("spawned job should exist");

    let _ = std::panic::catch_unwind({
        let job = Arc::clone(&job);
        move || {
            let _guard = job.status.lock().unwrap();
            panic!("intentional poison job status");
        }
    });
    let _ = std::panic::catch_unwind({
        let job = Arc::clone(&job);
        move || {
            let _guard = job.stdout_buf.lock().unwrap();
            panic!("intentional poison job stdout");
        }
    });

    let status = bg_status(&json!({ "job_id": job_id, "from_start": true }), &store)
        .expect("bg_status should recover from poisoned job locks");
    let status_json = result_json(&status);
    assert!(status_json["status"].is_string());
    assert!(status_json["total_stdout_bytes"].is_u64());
}

#[test]
fn poisoned_pipeline_mutexes_do_not_break_pipeline_status() {
    let root = std::env::temp_dir().join(format!("ferrite_poison_pipeline_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let state = state_with_cwd(&root);
    let store = Arc::new(JobStore::new(Arc::new(Persistence::new())));
    let pipelines = Arc::new(PipelineStore::new(Arc::clone(&store)));

    let started = pipeline_run(
        &json!({ "steps": [{ "id": "s1", "cmd": "printf ok" }] }),
        &pipelines,
        &state,
    )
    .unwrap();
    let started_json = result_json(&started);
    let pipeline_id = started_json["pipeline_id"].as_str().unwrap().to_string();
    let pipeline = pipelines.get(&pipeline_id).expect("pipeline should exist");
    let step = Arc::clone(&pipeline.steps[0]);

    let _ = std::panic::catch_unwind({
        let step = Arc::clone(&step);
        move || {
            let _guard = step.status.lock().unwrap();
            panic!("intentional poison step status");
        }
    });
    let _ = std::panic::catch_unwind({
        let step = Arc::clone(&step);
        move || {
            let _guard = step.job_id.lock().unwrap();
            panic!("intentional poison step job id");
        }
    });

    let status = pipeline_status(&json!({ "pipeline_id": pipeline_id }), &pipelines)
        .expect("pipeline_status should recover from poisoned pipeline locks");
    let status_json = result_json(&status);
    assert!(status_json["status"].is_string());
    assert_eq!(status_json["steps"].as_array().map(|v| v.len()), Some(1));
}

#[test]
fn core_path_tools_default_to_server_cwd() {
    let root = std::env::temp_dir().join(format!("ferrite_path_tools_{}", std::process::id()));
    let src = root.join("src");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(root.join("hello.txt"), "hello from session cwd\n").unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub struct SessionOnlySymbol;\npub fn session_only_symbol() {}\n",
    )
    .unwrap();
    std::fs::write(root.join("model.ckpt"), "fake checkpoint\n").unwrap();

    let state = state_with_cwd(&root);

    let file = result_json(&read_file(&json!({ "path": "hello.txt" }), &state).unwrap());
    assert_eq!(file["content"].as_str(), Some("hello from session cwd"));

    let listing = result_json(&list_dir(&json!({}), &state).unwrap());
    assert_eq!(listing["path"].as_str(), Some(root.to_str().unwrap()));
    assert!(listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["name"] == "hello.txt"));

    let glob = result_json(&glob_files(&json!({ "pattern": "*.txt" }), &state).unwrap());
    assert_eq!(glob["count"].as_u64(), Some(1));

    let changed = result_json(&changed_since(&json!({ "since_relative": "1h" }), &state).unwrap());
    assert!(changed["changed"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| { e["path"].as_str().unwrap_or("").ends_with("/hello.txt") }));

    let ctx =
        result_json(&read_context(&json!({ "file": "src/lib.rs", "line": 1 }), &state).unwrap());
    assert_eq!(ctx["file"].as_str(), Some("src/lib.rs"));
    assert!(ctx["lines"].as_array().unwrap().iter().any(|l| {
        l["content"]
            .as_str()
            .unwrap_or("")
            .contains("session_only_symbol")
    }));

    let grep =
        result_json(&grep_code(&json!({ "pattern": "session_only_symbol" }), &state).unwrap());
    assert_eq!(grep["count"].as_u64(), Some(1));

    let symbols = result_json(&symbol_index(&json!({}), &state).unwrap());
    assert_eq!(symbols["files_scanned"].as_u64(), Some(1));

    let checkpoints = result_json(&checkpoint_list(&json!({ "inspect": false }), &state).unwrap());
    assert_eq!(checkpoints["count"].as_u64(), Some(1));

    let project = result_json(&project_context(&json!({}), &state).unwrap());
    assert_eq!(project["root"].as_str(), Some(root.to_str().unwrap()));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn bg_tools_default_to_server_cwd() {
    let root = std::env::temp_dir().join(format!("ferrite_bg_tools_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let state = state_with_cwd(&root);

    let store = Arc::new(JobStore::new(Arc::new(Persistence::new())));
    let spawned = result_json(&bg_spawn(&json!({ "cmd": "pwd" }), &store, &state).unwrap());
    let job_id = spawned["job_id"].as_str().unwrap();
    let job = store.get(job_id).unwrap();
    assert_eq!(job.cwd, root);

    let tty = result_json(
        &tty_exec(&json!({ "cmd": "pwd", "timeout_secs": 5 }), &store, &state).unwrap(),
    );
    let expected_output = format!("{}\r\n", root.display());
    assert_eq!(tty["output"].as_str(), Some(expected_output.as_str()));

    let pipelines = Arc::new(PipelineStore::new(Arc::clone(&store)));
    let pipeline = result_json(
        &pipeline_run(
            &json!({ "steps": [{ "id": "s1", "cmd": "pwd" }] }),
            &pipelines,
            &state,
        )
        .unwrap(),
    );
    let pipeline_id = pipeline["pipeline_id"].as_str().unwrap();

    let mut step_job = None;
    for _ in 0..20 {
        if let Some(p) = pipelines.get(pipeline_id) {
            if let Some(step) = p.step_by_id("s1") {
                if let Some(job_id) = step.job_id.lock().unwrap().clone() {
                    step_job = Some(job_id);
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    let step_job = step_job.expect("pipeline step should spawn a job");
    let job = store.get(&step_job).unwrap();
    assert_eq!(job.cwd, root);

    let _ = std::fs::remove_dir_all(&root);
}
