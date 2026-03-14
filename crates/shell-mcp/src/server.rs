//! MCP stdio server loop.
//!
//! Reads newline-delimited JSON from stdin, writes responses to stdout.
//! Stderr is reserved for diagnostics — never written during normal MCP operation.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config::FerriteConfig;
use crate::job_store::JobStore;
use crate::pipeline::PipelineStore;
use crate::persist::Persistence;

use serde_json::{json, Value};

use crate::protocol::{ERR_INTERNAL, ERR_PARSE, InboundMessage, Response, ToolResult};
use crate::tools;

/// Default response cap. Override per-call with `max_chars=0` for unlimited.
const DEFAULT_MAX_CHARS: usize = 8192;

/// Apply keyword filter then size cap to every tool response.
///
/// - `filter`: if non-empty, keep only lines whose text contains it (case-insensitive).
/// - `max_chars`: hard cap on total text length; 0 = unlimited.
fn cap_and_filter(mut result: ToolResult, filter: &str, max_chars: usize) -> ToolResult {
    for item in &mut result.content {
        // ── keyword filter ───────────────────────────────────────────────────
        if !filter.is_empty() {
            let lower = filter.to_lowercase();
            let filtered: Vec<&str> = item.text
                .lines()
                .filter(|l| l.to_lowercase().contains(&lower))
                .collect();
            item.text = filtered.join("\n");
        }

        // ── size cap ─────────────────────────────────────────────────────────
        if max_chars > 0 && item.text.len() > max_chars {
            // Truncate at a char boundary
            let cut = item.text
                .char_indices()
                .map(|(i, _)| i)
                .nth(max_chars)
                .unwrap_or(max_chars);
            let omitted = item.text.len() - cut;
            item.text.truncate(cut);
            item.text.push_str(&format!("\n[truncated: {omitted}b omitted]"));
        }
    }
    result
}

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME:      &str = "ferrite";
const SERVER_VERSION:   &str = env!("CARGO_PKG_VERSION");

/// Shared mutable server state (working directory, session notes, config, etc.)
#[derive(Debug, Clone)]
pub struct ServerState {
    pub cwd:    PathBuf,
    pub notes:  Vec<String>,
    pub config: FerriteConfig,
}

impl Default for ServerState {
    fn default() -> Self {
        Self {
            cwd:    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            notes:  Vec::new(),
            config: FerriteConfig::load(),
        }
    }
}

#[derive(Debug)]
pub struct ServerMetrics {
    started_at: Instant,
    total_tool_calls: AtomicU64,
}

impl Default for ServerMetrics {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            total_tool_calls: AtomicU64::new(0),
        }
    }
}

impl ServerMetrics {
    pub fn record_tool_call(&self) -> u64 {
        self.total_tool_calls.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn total_tool_calls(&self) -> u64 {
        self.total_tool_calls.load(Ordering::Relaxed)
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

pub struct McpServer {
    pub state:     Arc<Mutex<ServerState>>,
    pub store:     Arc<JobStore>,
    pub pipelines: Arc<PipelineStore>,
    pub metrics:   Arc<ServerMetrics>,
}

impl McpServer {
    pub fn new() -> Self {
        let persistence = Arc::new(Persistence::new());
        let store       = Arc::new(JobStore::restore(Arc::clone(&persistence)));
        let pipelines   = Arc::new(PipelineStore::new(Arc::clone(&store)));
        Self {
            state:     Arc::new(Mutex::new(ServerState::default())),
            store,
            pipelines,
            metrics:   Arc::new(ServerMetrics::default()),
        }
    }

    /// Run the stdio event loop until stdin closes.
    pub fn run(&self) -> std::io::Result<()> {
        let stdin  = std::io::stdin();
        let stdout = std::io::stdout();
        let mut out = stdout.lock();

        for line in stdin.lock().lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }

            if let Some(resp) = self.handle_line(trimmed) {
                let s = serde_json::to_string(&resp)
                    .unwrap_or_else(|e| format!(r#"{{"jsonrpc":"2.0","error":{{"code":-32603,"message":"{e}"}}}}"#));
                writeln!(out, "{s}")?;
                out.flush()?;
            }

            if self.should_recycle() {
                break;
            }
        }

        self.store.persist_all();
        std::thread::sleep(std::time::Duration::from_millis(150));
        Ok(())
    }

    fn handle_line(&self, line: &str) -> Option<Response> {
        let msg: InboundMessage = match serde_json::from_str(line) {
            Ok(m)  => m,
            Err(e) => return Some(Response::err(Value::Null, ERR_PARSE, format!("parse error: {e}"))),
        };

        if msg.is_notification() {
            self.handle_notification(&msg.method);
            return None;
        }

        let id = msg.id.clone().unwrap_or(Value::Null);
        Some(match self.dispatch(&msg.method, msg.params.as_ref()) {
            Ok(v)  => Response::ok(id, v),
            Err(e) => Response::err(id, ERR_INTERNAL, e),
        })
    }

    fn handle_notification(&self, method: &str) {
        match method {
            "notifications/initialized" => {}
            _ => {}
        }
    }

    fn dispatch(&self, method: &str, params: Option<&Value>) -> Result<Value, String> {
        match method {
            "initialize"   => self.handle_initialize(),
            "tools/list"   => self.handle_tools_list(),
            "tools/call"   => self.handle_tools_call(params),
            other          => Err(format!("method not found: {other}")),
        }
    }

    fn handle_initialize(&self) -> Result<Value, String> {
        Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
        }))
    }

    fn handle_tools_list(&self) -> Result<Value, String> {
        Ok(json!({ "tools": tools::all_tool_definitions() }))
    }

    fn handle_tools_call(&self, params: Option<&Value>) -> Result<Value, String> {
        let params = params.ok_or("tools/call requires params")?;
        let name   = params["name"].as_str().ok_or("tools/call: missing 'name'")?;
        let args   = &params["arguments"];

        // Cross-cutting: keyword filter + size cap applied to every response.
        // Callers may override max_chars (0 = unlimited); filter is opt-in.
        let filter    = args["filter"].as_str().unwrap_or("");
        let max_chars = args["max_chars"].as_u64()
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_MAX_CHARS);

        self.metrics.record_tool_call();

        let decision = crate::authz::authorize(name, args);
        crate::authz::audit(&decision, name, args);
        if !decision.allowed {
            if name == "fercuda_runtime" {
                let action = args["action"].as_str()
                    .map(ToOwned::to_owned)
                    .or_else(|| args["op"].as_str().map(|op| match op {
                        "status" => "runtime.inspect".to_owned(),
                        "guide" => "runtime.guide".to_owned(),
                        "session_create" => "session.create".to_owned(),
                        "session_destroy" => "session.destroy".to_owned(),
                        "buffer_alloc" => "tensor.create".to_owned(),
                        "buffer_free" => "tensor.destroy".to_owned(),
                        "upload_f32" => "tensor.upload".to_owned(),
                        "download_f32" => "tensor.download".to_owned(),
                        "jit_compile" => "jit.program.compile".to_owned(),
                        "jit_release_program" => "jit.program.release".to_owned(),
                        "jit_get_kernel" => "jit.kernel.bind".to_owned(),
                        "jit_launch" => "jit.kernel.launch".to_owned(),
                        "jit_release_kernel" => "jit.kernel.release".to_owned(),
                        "jit_stats" => "jit.stats.get".to_owned(),
                        "submit_matmul" => "op.matmul.submit".to_owned(),
                        "submit_layer_norm" => "op.layer_norm.submit".to_owned(),
                        "job_status" => "job.status".to_owned(),
                        "job_wait" => "job.wait".to_owned(),
                        _ => "runtime.unknown".to_owned(),
                    }))
                    .unwrap_or_else(|| "runtime.unknown".to_owned());
                let result = ToolResult::json(&json!({
                    "ok": false,
                    "agent_api_version": "v1alpha1",
                    "action": action,
                    "op": args["op"].as_str().unwrap_or("unknown"),
                    "error": {
                        "code": "POLICY_DENIED",
                        "message": format!(
                            "authorization denied for {} as principal={} role={} reason={}",
                            decision.action, decision.principal, decision.role, decision.reason
                        ),
                        "details": {
                            "principal": decision.principal,
                            "role": decision.role,
                            "reason": decision.reason
                        }
                    }
                }));
                let result = cap_and_filter(result, filter, max_chars);
                return serde_json::to_value(&result).map_err(|e| e.to_string());
            }
            return Err(format!(
                "authorization denied for {} as principal={} role={} reason={}",
                decision.action, decision.principal, decision.role, decision.reason
            ));
        }

        crate::tools::git_guard::maybe_auto_checkpoint(name, args, &self.state)?;

        let result = self.call_tool(name, args)?;
        let result = cap_and_filter(result, filter, max_chars);
        serde_json::to_value(&result).map_err(|e| e.to_string())
    }

    fn should_recycle(&self) -> bool {
        let policy = RecyclePolicy::from_env();
        if policy.is_disabled() {
            return false;
        }

        let snapshot = HealthSnapshot::collect(&self.metrics, &self.state, &self.store);
        policy.should_recycle(&snapshot)
    }

    fn call_tool(&self, name: &str, args: &Value) -> Result<ToolResult, String> {
        match name {
            // Discovery
            "find_lib"       => tools::discovery::find_lib(args),
            "discover"       => tools::discovery::discover(args),
            // Hardware
            "gpu_info"       => tools::hardware::gpu_info(args),
            "cpu_info"       => tools::hardware::cpu_info(args),
            "occupancy_calc" => tools::hardware::occupancy_calc(args),
            "cuda_env_doctor" => tools::cuda::cuda_env_doctor(args, &self.state),
            "cuda_artifacts"  => tools::cuda::cuda_artifacts(args, &self.state),
            "cuda_triage"     => tools::cuda::cuda_triage(args, &self.state),
            "cuda_regression_run" => tools::cuda::cuda_regression_run(args, &self.state),
            "cuda_regression_report" => tools::cuda::cuda_regression_report(args, &self.state),
            // Code navigation
            "read_context"   => tools::code::read_context(args, &self.state),
            "grep_code"      => tools::code::grep_code(args, &self.state),
            // Filesystem
            "read_file"      => tools::filesystem::read_file(args, &self.state),
            "list_dir"       => tools::filesystem::list_dir(args, &self.state),
            "glob"           => tools::filesystem::glob_files(args, &self.state),
            "which"          => tools::filesystem::which_bin(args),
            // State
            "shell_state"    => tools::state::shell_state(args, &self.state),
            "set_cwd"        => tools::state::set_cwd(args, &self.state),
            "control_reconcile" => tools::control::control_reconcile(args, &self.state),
            "config_ux"      => tools::config_ux::config_ux(args),
            "ux_wizard"      => tools::ux_wizard::ux_wizard(args),
            "fercuda_runtime" => tools::fercuda::runtime(args),
            // Execution
            "exec"           => tools::execution::exec_cmd(args, &self.state),
            "build_check"    => tools::execution::build_check(args, &self.state),
            "task_run"       => tools::execution::task_run(args, &self.state),
            "launch"         => tools::execution::launch(args, &self.state),
            "close_observer" => tools::execution::close_observer(args),
            // Binary inspection
            "inspect_binary" => tools::binary::inspect_binary(args),
            // Rust tooling
            "cargo_tree"     => tools::rust_tools::cargo_tree(args, &self.state),
            "test_run"       => tools::rust_tools::test_run(args, &self.state),
            // GPU live state
            "gpu_live"       => tools::hardware::gpu_live(args),
            // Profiling
            "ptx_inspect"       => tools::profiling::ptx_inspect(args),
            "ncu_profile"       => tools::profiling::ncu_profile(args, &self.state),
            "compute_sanitizer" => tools::profiling::compute_sanitizer(args, &self.state),
            // Benchmark history
            "bench_history"  => tools::history::bench_history(args),
            // Incremental filesystem
            "changed_since"  => tools::filesystem::changed_since(args, &self.state),
            // HTTP
            "http_request"   => tools::http::http_request(args),
            // CPU profiling
            "flamegraph"     => tools::perf_tools::flamegraph(args),
            "perf_stat"      => tools::perf_tools::perf_stat(args),
            // Debugging
            "gdb_run"        => tools::debug::gdb_run(args),
            // ML
            "tensor_inspect"    => tools::ml::tensor_inspect(args),
            "checkpoint_list"   => tools::ml::checkpoint_list(args, &self.state),
            // Git
            "git_log"        => tools::git::git_log(args, &self.state),
            "git_diff"       => tools::git::git_diff(args, &self.state),
            "git_status"     => tools::git::git_status(args, &self.state),
            // Symbols
            "symbol_index"   => tools::symbols::symbol_index(args, &self.state),
            "find_symbol"    => tools::symbols::find_symbol(args, &self.state),
            // System
            "process_tree"   => tools::system::process_tree(args),
            "port_list"      => tools::system::port_list(args),
            "journal_query"  => tools::system::journal_query(args),
            // File ops
            "move_file"      => tools::filesystem::move_file(args),
            "mkdir"          => tools::filesystem::make_dir(args),
            "delete_file"    => tools::filesystem::delete_file(args),
            // Workspace
            "orient"         => tools::workspace::orient(args, &self.state, &self.store),
            "note"           => tools::workspace::note(args, &self.state),
            // EDA
            "verilog_lint"   => tools::eda::verilog_lint(args),
            "verilog_sim"    => tools::eda::verilog_sim(args),
            "xsim_elab"      => tools::eda::xsim_elab(args),
            "cocotb_run"     => tools::eda::cocotb_run(args),
            "vivado_tcl"     => tools::eda::vivado_tcl(args),
            "fpga_boards"         => tools::eda::fpga_boards(args),
            "fpga_program"        => tools::eda::fpga_program(args),
            "waveform_query"      => tools::eda::waveform_query(args),
            "synth_report"        => tools::eda::synth_report(args),
            "fpga_serial"         => tools::eda::fpga_serial(args),
            "fpga_tcfp_status"    => tools::eda::fpga_tcfp_status(args),
            "fpga_tcfp_tile_read" => tools::eda::fpga_tcfp_tile_read(args),
            // Background process orchestration
            "bg_spawn"         => tools::bg_spawn::bg_spawn(args, &self.store, &self.state),
            "bg_attach"        => tools::bg_spawn::bg_attach(args, &self.store),
            "bg_send"          => tools::bg_interact::bg_send(args, &self.store),
            "bg_status"        => tools::bg_query::bg_status(args, &self.store),
            "bg_wait"          => tools::bg_query::bg_wait(args, &self.store),
            "bg_tail"          => tools::bg_query::bg_tail(args, &self.store),
            "bg_list"          => tools::bg_query::bg_list(args, &self.store),
            "wait_for_pattern" => tools::bg_query::wait_for_pattern(args, &self.store),
            "wait_for_idle"    => tools::bg_query::wait_for_idle(args, &self.store),
            "output_summary"   => tools::bg_query::output_summary(args, &self.store),
            "bg_kill"          => tools::bg_control::bg_kill(args, &self.store),
            "pipeline_run"     => tools::bg_pipeline::pipeline_run(args, &self.pipelines, &self.state),
            "pipeline_status"  => tools::bg_pipeline::pipeline_status(args, &self.pipelines),
            "pipeline_cancel"  => tools::bg_pipeline::pipeline_cancel(args, &self.pipelines),
            "live_window"      => tools::bg_window::live_window(args, &self.store),
            // Project / chip awareness (Tier 1)
            "project_context"     => tools::project::project_context(args, &self.state),
            "chip_status"         => tools::project::chip_status(args),
            "chip_build_pipeline" => tools::project::chip_build_pipeline(args, &self.store),
            "rtl_regression_run"  => tools::project::rtl_regression_run(args),
            "rtl_regression_report" => tools::project::rtl_regression_report(args),
            "fpga_triage" => tools::project::fpga_triage(args),
            "fpga_artifacts" => tools::project::fpga_artifacts(args),
            "board_status"        => tools::project::board_status(args),
            "fpga_monitor"        => tools::project::fpga_monitor(args, &self.store),
            // Remote SSH (Tier 2)
            "remote_exec"   => tools::remote::remote_exec(args),
            "remote_build"  => tools::remote::remote_build(args, &self.store),
            "sync_project"  => tools::remote::sync_project(args),
            // Project creation
            "project_new"  => tools::git_new::project_new(args),
            // Git write
            "git_checkpoint" => tools::git_write::git_checkpoint(args, &self.state),
            "git_commit"   => tools::git_write::git_commit(args, &self.state),
            // Notifications
            "notify"            => tools::notify::notify(args),
            // tmux
            "tmux_ctl"          => tools::tmux::tmux_ctl(args),
            // Network / reachability
            "tailscale_status"  => tools::network::tailscale_status(args),
            // Session health
            "session_status"    => tools::session::session_status(args),
            "session_restart"   => tools::session::session_restart(args),
            // GitHub SSH
            "gh_clone"  => tools::github::gh_clone(args),
            "gh_sync"   => tools::github::gh_sync(args, &self.state),
            "gh_status" => tools::github::gh_status(args, &self.state),
            // Permissions / pre-validation
            "pre_validate"      => tools::permissions_tool::pre_validate(args),
            "permissions_setup" => tools::permissions_tool::permissions_setup(args),
            // PTY / interactive program driver
            "tty_exec"          => tools::tty_exec::tty_exec(args, &self.store, &self.state),
            // Server health
            "health"            => tools::health::health(args, &self.metrics, &self.state, &self.store),
            // Environment pre-flight
            "env_doctor"        => tools::env_doctor::env_doctor(args),
            _           => Err(format!("unknown tool: {name}")),
        }
    }
}

impl Default for McpServer {
    fn default() -> Self { Self::new() }
}

#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    pub uptime_secs: u64,
    pub total_tool_calls: u64,
    pub note_count: usize,
    pub note_bytes: usize,
    pub rss_bytes: Option<u64>,
    pub job_stats: crate::job_store::JobStoreStats,
}

impl HealthSnapshot {
    pub fn collect(
        metrics: &Arc<ServerMetrics>,
        state: &Arc<Mutex<ServerState>>,
        store: &Arc<JobStore>,
    ) -> Self {
        let (note_count, note_bytes) = {
            let guard = state.lock().unwrap();
            let bytes = guard.notes.iter().map(|n| n.len()).sum();
            (guard.notes.len(), bytes)
        };

        Self {
            uptime_secs: metrics.uptime_secs(),
            total_tool_calls: metrics.total_tool_calls(),
            note_count,
            note_bytes,
            rss_bytes: current_rss_bytes(),
            job_stats: store.stats(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RecyclePolicy {
    pub max_calls: Option<u64>,
    pub max_uptime_secs: Option<u64>,
    pub max_rss_bytes: Option<u64>,
}

impl RecyclePolicy {
    pub fn from_env() -> Self {
        Self {
            max_calls: std::env::var("FERRITE_MCP_MAX_CALLS").ok().and_then(|v| v.parse().ok()),
            max_uptime_secs: std::env::var("FERRITE_MCP_MAX_UPTIME_SECS").ok().and_then(|v| v.parse().ok()),
            max_rss_bytes: std::env::var("FERRITE_MCP_MAX_RSS_MB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(|mb| mb.saturating_mul(1024 * 1024)),
        }
    }

    pub fn is_disabled(&self) -> bool {
        self.max_calls.is_none() && self.max_uptime_secs.is_none() && self.max_rss_bytes.is_none()
    }

    pub fn should_recycle(&self, snapshot: &HealthSnapshot) -> bool {
        self.max_calls.map(|v| snapshot.total_tool_calls >= v).unwrap_or(false)
            || self.max_uptime_secs.map(|v| snapshot.uptime_secs >= v).unwrap_or(false)
            || self.max_rss_bytes
                .map(|v| snapshot.rss_bytes.map(|rss| rss >= v).unwrap_or(false))
                .unwrap_or(false)
    }
}

fn current_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
                return Some(kb.saturating_mul(1024));
            }
        }
        None
    }

    #[cfg(not(target_os = "linux"))]
    {
        let pid = std::process::id().to_string();
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid])
            .output()
            .ok()?;
        let kb = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().ok()?;
        Some(kb.saturating_mul(1024))
    }
}

#[cfg(test)]
mod tests {
    use super::{HealthSnapshot, RecyclePolicy};
    use crate::job_store::JobStoreStats;

    fn snapshot() -> HealthSnapshot {
        HealthSnapshot {
            uptime_secs: 10,
            total_tool_calls: 5,
            note_count: 0,
            note_bytes: 0,
            rss_bytes: Some(32 * 1024 * 1024),
            job_stats: JobStoreStats {
                total_jobs: 0,
                running_jobs: 0,
                attached_jobs: 0,
                done_jobs: 0,
                killed_jobs: 0,
                stdout_bytes: 0,
                stderr_bytes: 0,
                buffered_bytes: 0,
            },
        }
    }

    #[test]
    fn recycle_policy_disabled_when_no_thresholds() {
        let policy = RecyclePolicy {
            max_calls: None,
            max_uptime_secs: None,
            max_rss_bytes: None,
        };
        assert!(policy.is_disabled());
        assert!(!policy.should_recycle(&snapshot()));
    }

    #[test]
    fn recycle_policy_triggers_on_call_threshold() {
        let policy = RecyclePolicy {
            max_calls: Some(5),
            max_uptime_secs: None,
            max_rss_bytes: None,
        };
        assert!(policy.should_recycle(&snapshot()));
    }
}
