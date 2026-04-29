//! MCP stdio server loop.
//!
//! Reads newline-delimited JSON from stdin, writes responses to stdout.
//! Stderr is reserved for diagnostics — never written during normal MCP operation.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Instant;

use crate::config::FerriteConfig;
use crate::job_store::JobStore;
use crate::persist::Persistence;
use crate::pipeline::PipelineStore;

use serde_json::{json, Value};

use crate::protocol::{InboundMessage, Response, ToolResult, ERR_INTERNAL, ERR_PARSE};
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
            let filtered: Vec<&str> = item
                .text
                .lines()
                .filter(|l| l.to_lowercase().contains(&lower))
                .collect();
            item.text = filtered.join("\n");
        }

        // ── size cap ─────────────────────────────────────────────────────────
        if max_chars > 0 && item.text.len() > max_chars {
            // Truncate at a char boundary
            let cut = item
                .text
                .char_indices()
                .map(|(i, _)| i)
                .nth(max_chars)
                .unwrap_or(max_chars);
            let omitted = item.text.len() - cut;
            item.text.truncate(cut);
            item.text
                .push_str(&format!("\n[truncated: {omitted}b omitted]"));
        }
    }
    result
}

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "ferrite";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared mutable server state (working directory, session notes, config, etc.)
#[derive(Debug)]
pub struct ServerState {
    pub cwd: RwLock<PathBuf>,
    pub notes: Mutex<Vec<String>>,
    pub config: RwLock<FerriteConfig>,
}

impl Default for ServerState {
    fn default() -> Self {
        Self {
            cwd: RwLock::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))),
            notes: Mutex::new(Vec::new()),
            config: RwLock::new(FerriteConfig::load()),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolConcurrencyLane {
    ParallelRead,
    SerializedState,
    SerializedResource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionPolicy {
    pub lane: ToolConcurrencyLane,
    pub resource_keys: Vec<String>,
    pub reason: &'static str,
}

#[derive(Debug, Default)]
pub struct ExecutionScheduler {
    state_lock: RwLock<()>,
    resource_locks: Mutex<HashMap<String, Arc<RwLock<()>>>>,
}

impl ExecutionScheduler {
    fn resource_lock(&self, key: &str) -> Arc<RwLock<()>> {
        let mut locks = crate::tools::state::lock_mutex(&self.resource_locks, "resource locks");
        Arc::clone(
            locks
                .entry(key.to_owned())
                .or_insert_with(|| Arc::new(RwLock::new(()))),
        )
    }

    fn sorted_resource_keys(policy: &ToolExecutionPolicy) -> Vec<String> {
        let mut keys = policy.resource_keys.clone();
        keys.sort();
        keys.dedup();
        keys
    }

    fn run<T>(&self, policy: &ToolExecutionPolicy, f: impl FnOnce() -> T) -> T {
        match policy.lane {
            ToolConcurrencyLane::ParallelRead => {
                let _state = crate::tools::state::read_rwlock(&self.state_lock, "state scheduler");
                let keys = Self::sorted_resource_keys(policy);
                let locks: Vec<_> = keys.iter().map(|key| self.resource_lock(key)).collect();
                let _resources: Vec<_> = locks
                    .iter()
                    .zip(keys.iter())
                    .map(|(lock, key)| crate::tools::state::read_rwlock(lock, key))
                    .collect();
                f()
            }
            ToolConcurrencyLane::SerializedState => {
                let _state = crate::tools::state::write_rwlock(&self.state_lock, "state scheduler");
                f()
            }
            ToolConcurrencyLane::SerializedResource => {
                let _state = crate::tools::state::read_rwlock(&self.state_lock, "state scheduler");
                let keys = Self::sorted_resource_keys(policy);
                let keys = if keys.is_empty() {
                    vec!["resource:unknown".to_owned()]
                } else {
                    keys
                };
                let locks: Vec<_> = keys.iter().map(|key| self.resource_lock(key)).collect();
                let _resources: Vec<_> = locks
                    .iter()
                    .zip(keys.iter())
                    .map(|(lock, key)| crate::tools::state::write_rwlock(lock, key))
                    .collect();
                f()
            }
        }
    }
}

#[derive(Clone)]
pub struct McpServer {
    pub state: Arc<Mutex<ServerState>>,
    pub store: Arc<JobStore>,
    pub pipelines: Arc<PipelineStore>,
    pub metrics: Arc<ServerMetrics>,
    scheduler: Arc<ExecutionScheduler>,
}

enum RunEvent {
    Input(String),
    Response(Response),
    InputClosed,
    ReaderError(String),
}

impl McpServer {
    pub fn new() -> Self {
        let persistence = Arc::new(Persistence::new());
        let store = Arc::new(JobStore::restore(Arc::clone(&persistence)));
        let pipelines = Arc::new(PipelineStore::new(Arc::clone(&store)));
        Self {
            state: Arc::new(Mutex::new(ServerState::default())),
            store,
            pipelines,
            metrics: Arc::new(ServerMetrics::default()),
            scheduler: Arc::new(ExecutionScheduler::default()),
        }
    }

    /// Run the stdio event loop until stdin closes.
    pub fn run(&self) -> std::io::Result<()> {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let (tx, rx) = mpsc::channel::<RunEvent>();

        let reader_tx = tx.clone();
        thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                match line {
                    Ok(line) => {
                        if reader_tx.send(RunEvent::Input(line)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = reader_tx.send(RunEvent::ReaderError(e.to_string()));
                        return;
                    }
                }
            }
            let _ = reader_tx.send(RunEvent::InputClosed);
        });

        let mut input_closed = false;
        let mut in_flight = 0usize;

        while !input_closed || in_flight > 0 {
            let event = match rx.recv() {
                Ok(event) => event,
                Err(_) => break,
            };

            match event {
                RunEvent::Input(line) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    match self.parse_message(trimmed) {
                        Ok(msg) => {
                            if msg.is_notification() {
                                self.handle_notification(&msg.method);
                            } else {
                                in_flight += 1;
                                self.spawn_scheduled_request(msg, tx.clone());
                            }
                        }
                        Err(e) => {
                            let resp = Response::err(Value::Null, ERR_PARSE, e);
                            Self::write_response(&mut out, &resp)?;
                        }
                    }
                }
                RunEvent::Response(resp) => {
                    in_flight = in_flight.saturating_sub(1);
                    Self::write_response(&mut out, &resp)?;
                }
                RunEvent::InputClosed => {
                    input_closed = true;
                }
                RunEvent::ReaderError(e) => {
                    return Err(std::io::Error::other(e));
                }
            }

            if self.should_recycle() {
                break;
            }
        }

        self.store.persist_all();
        std::thread::sleep(std::time::Duration::from_millis(150));
        Ok(())
    }

    fn write_response(out: &mut impl Write, resp: &Response) -> std::io::Result<()> {
        let s = serde_json::to_string(resp).unwrap_or_else(|e| {
            format!(r#"{{"jsonrpc":"2.0","error":{{"code":-32603,"message":"{e}"}}}}"#)
        });
        writeln!(out, "{s}")?;
        out.flush()
    }

    fn parse_message(&self, line: &str) -> Result<InboundMessage, String> {
        serde_json::from_str(line).map_err(|e| format!("parse error: {e}"))
    }

    fn response_for_message(&self, msg: InboundMessage) -> Option<Response> {
        if msg.is_notification() {
            self.handle_notification(&msg.method);
            return None;
        }

        let id = msg.id.clone().unwrap_or(Value::Null);
        Some(match self.dispatch(&msg.method, msg.params.as_ref()) {
            Ok(v) => Response::ok(id, v),
            Err(e) => Response::err(id, ERR_INTERNAL, e),
        })
    }

    fn spawn_scheduled_request(&self, msg: InboundMessage, tx: Sender<RunEvent>) {
        let server = self.clone();
        thread::spawn(move || {
            if let Some(resp) = server.response_for_message(msg) {
                let _ = tx.send(RunEvent::Response(resp));
            }
        });
    }

    fn handle_notification(&self, method: &str) {
        if method == "notifications/initialized" {}
    }

    fn dispatch(&self, method: &str, params: Option<&Value>) -> Result<Value, String> {
        match method {
            "initialize" => self.handle_initialize(),
            "tools/list" => self.handle_tools_list(),
            "tools/call" => self.handle_tools_call(params),
            other => Err(format!("method not found: {other}")),
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
        let name = params["name"]
            .as_str()
            .ok_or("tools/call: missing 'name'")?;
        let args = &params["arguments"];
        let policy = self.tool_execution_policy(name, args);

        // Cross-cutting: keyword filter + size cap applied to every response.
        // Callers may override max_chars (0 = unlimited); filter is opt-in.
        let filter = self.response_filter(name, args);
        let max_chars = args["max_chars"]
            .as_u64()
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_MAX_CHARS);

        self.metrics.record_tool_call();

        let decision = crate::authz::authorize(name, args);
        crate::authz::audit(&decision, name, args);
        if !decision.allowed {
            return Err(format!(
                "authorization denied for {} as principal={} role={} reason={}",
                decision.action, decision.principal, decision.role, decision.reason
            ));
        }

        let result = self.scheduler.run(&policy, || {
            crate::tools::git_guard::maybe_auto_checkpoint(name, args, &self.state)?;
            self.call_tool(name, args)
        })?;
        let result = cap_and_filter(result, filter, max_chars);
        serde_json::to_value(&result).map_err(|e| e.to_string())
    }

    fn response_filter<'a>(&self, name: &str, args: &'a Value) -> &'a str {
        args["response_filter"]
            .as_str()
            .or_else(|| {
                if Self::tool_owns_filter_arg(name) {
                    None
                } else {
                    args["filter"].as_str()
                }
            })
            .unwrap_or("")
    }

    fn tool_owns_filter_arg(name: &str) -> bool {
        matches!(name, "test_run" | "process_tree")
    }

    fn tool_execution_policy(&self, name: &str, args: &Value) -> ToolExecutionPolicy {
        match name {
            "read_context" | "grep_code" | "list_dir" | "glob" | "changed_since"
            | "symbol_index" | "find_symbol" => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::ParallelRead,
                resource_keys: self
                    .path_resource_keys([args["path"].as_str().or_else(|| args["cwd"].as_str())]),
                reason: "read-only filesystem query scoped to a path",
            },
            "read_file" | "stat_file" | "read_bytes" | "hash_file" => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::ParallelRead,
                resource_keys: self.path_resource_keys([args["path"].as_str()]),
                reason: "read-only file query scoped to a path",
            },
            "diff_files" => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::ParallelRead,
                resource_keys: self.path_resource_keys([args["a"].as_str(), args["b"].as_str()]),
                reason: "read-only diff scoped to both input paths",
            },
            "git_log" | "git_diff" | "git_status" | "gh_status" => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::ParallelRead,
                resource_keys: self.repo_resource_keys([args["path"].as_str()]),
                reason: "read-only git query scoped to a repository",
            },

            // Pure read-only operations that can run concurrently once request-level
            // dispatch is parallelized.
            "find_lib"
            | "discover"
            | "gpu_info"
            | "cpu_info"
            | "occupancy_calc"
            | "which"
            | "inspect_binary"
            | "cargo_tree"
            | "gpu_live"
            | "ptx_inspect"
            | "bench_history"
            | "http_request"
            | "tensor_inspect"
            | "checkpoint_list"
            | "process_tree"
            | "port_list"
            | "journal_query"
            | "orient"
            | "waveform_query"
            | "synth_report"
            | "pipeline_status"
            | "chip_status"
            | "rtl_regression_report"
            | "fpga_triage"
            | "fpga_artifacts"
            | "board_status"
            | "pre_validate"
            | "health"
            | "env_doctor"
            | "shell_state"
            | "session_status"
            | "tailscale_status" => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::ParallelRead,
                resource_keys: Vec::new(),
                reason: "read-only tool with no shared mutable resource",
            },

            // Global session/process state mutations should remain serialized even
            // after parallel dispatch exists.
            "set_cwd" | "control_reconcile" | "config_ux" | "ux_wizard" | "note"
            | "project_new" | "notify" | "session_restart" => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::SerializedState,
                resource_keys: Vec::new(),
                reason: "mutates shared server or process-global state",
            },

            // Filesystem and repo mutations should serialize by target resource.
            "move_file" | "mkdir" | "delete_file" | "write_file" | "edit_file" | "sed_file"
            | "apply_patch" => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::SerializedResource,
                resource_keys: self.path_resource_keys([args["path"].as_str()]),
                reason: "mutates filesystem state under a target path",
            },
            // Multi-file mutations: serialize globally on the state lane
            // since we don't know all paths until we walk the args.
            "edit_transaction" | "replace_in_files" => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::SerializedState,
                resource_keys: Vec::new(),
                reason: "multi-file mutation; lock global to avoid overlap",
            },
            "gh_sync" | "git_checkpoint" | "git_commit" => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::SerializedResource,
                resource_keys: self.repo_resource_keys([args["path"].as_str()]),
                reason: "mutates a git repository that should serialize per repo",
            },

            // Background jobs, pipelines, and hardware endpoints keep their own
            // internal concurrency, but top-level mutations should serialize by ID
            // or endpoint to avoid conflicting control-plane actions.
            "bg_spawn" | "bg_attach" | "bg_list" => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::SerializedState,
                resource_keys: Vec::new(),
                reason: "mutates shared background job registry",
            },
            "bg_send" | "bg_status" | "bg_wait" | "bg_tail" | "bg_kill" | "wait_for_pattern"
            | "wait_for_idle" | "output_summary" | "live_window" => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::SerializedResource,
                resource_keys: Self::one_resource_key(
                    args["job_id"]
                        .as_str()
                        .map(|id| format!("job:{id}"))
                        .or_else(|| args["pid_file"].as_str().map(|p| format!("observer:{p}")))
                        .or_else(|| args["port"].as_str().map(|p| format!("uart:{p}")))
                        .or(Some("jobs:shared".to_owned())),
                ),
                reason: "targets a specific background job or observer handle",
            },
            "pipeline_run" | "pipeline_cancel" => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::SerializedResource,
                resource_keys: Self::one_resource_key(
                    args["pipeline_id"]
                        .as_str()
                        .map(|id| format!("pipeline:{id}"))
                        .or(Some("pipelines:shared".to_owned())),
                ),
                reason: "mutates pipeline orchestration state",
            },
            "fpga_program"
            | "fpga_serial"
            | "fpga_tcfp_status"
            | "fpga_tcfp_tile_read"
            | "fpga_monitor" => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::SerializedResource,
                resource_keys: Self::one_resource_key(
                    args["target"]
                        .as_str()
                        .map(|v| format!("fpga-target:{v}"))
                        .or_else(|| args["port"].as_str().map(|v| format!("uart:{v}")))
                        .or(Some("fpga:shared".to_owned())),
                ),
                reason: "talks to exclusive hardware control-plane resources",
            },

            // External commands often only snapshot cwd/config first, but until the
            // worker pool lands we keep them serialized by target path to avoid
            // overlapping repo/build-dir mutations.
            "exec"
            | "build_check"
            | "task_run"
            | "test_run"
            | "launch"
            | "close_observer"
            | "cuda_env_doctor"
            | "cuda_artifacts"
            | "cuda_triage"
            | "cuda_regression_run"
            | "cuda_regression_report"
            | "ncu_profile"
            | "compute_sanitizer"
            | "flamegraph"
            | "perf_stat"
            | "gdb_run"
            | "verilog_lint"
            | "verilog_sim"
            | "xsim_elab"
            | "cocotb_run"
            | "vivado_tcl"
            | "chip_build_pipeline"
            | "rtl_regression_run"
            | "remote_exec"
            | "remote_build"
            | "sync_project"
            | "tty_exec"
            | "tmux_ctl" => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::SerializedResource,
                resource_keys: self
                    .path_resource_keys([args["cwd"].as_str().or_else(|| args["path"].as_str())]),
                reason: "runs external commands that may mutate a working tree or tool state",
            },

            _ => ToolExecutionPolicy {
                lane: ToolConcurrencyLane::SerializedState,
                resource_keys: Vec::new(),
                reason: "unclassified tool defaults to conservative serialization",
            },
        }
    }

    fn one_resource_key(key: Option<String>) -> Vec<String> {
        key.into_iter().collect()
    }

    fn path_resource_key(&self, raw: Option<&str>) -> Option<String> {
        let path = match raw {
            Some(p) => {
                let expanded = crate::tools::project::expand_tilde(p);
                if expanded.is_absolute() {
                    expanded
                } else {
                    let cwd = crate::tools::state::read_cwd(&self.state);
                    cwd.join(expanded)
                }
            }
            None => crate::tools::state::read_cwd(&self.state),
        };
        Some(format!("path:{}", path.display()))
    }

    fn repo_resource_key(&self, raw: Option<&str>) -> Option<String> {
        self.path_resource_key(raw).map(|key| {
            let path = key.trim_start_matches("path:");
            format!("repo:{path}")
        })
    }

    fn path_resource_keys<'a>(
        &self,
        raws: impl IntoIterator<Item = Option<&'a str>>,
    ) -> Vec<String> {
        raws.into_iter()
            .filter_map(|raw| self.path_resource_key(raw))
            .collect()
    }

    fn repo_resource_keys<'a>(
        &self,
        raws: impl IntoIterator<Item = Option<&'a str>>,
    ) -> Vec<String> {
        raws.into_iter()
            .filter_map(|raw| self.repo_resource_key(raw))
            .collect()
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
            "find_lib" => tools::discovery::find_lib(args),
            "discover" => tools::discovery::discover(args),
            // Hardware
            "gpu_info" => tools::hardware::gpu_info(args),
            "cpu_info" => tools::hardware::cpu_info(args),
            "occupancy_calc" => tools::hardware::occupancy_calc(args),
            "cuda_env_doctor" => tools::cuda::cuda_env_doctor(args, &self.state),
            "cuda_artifacts" => tools::cuda::cuda_artifacts(args, &self.state),
            "cuda_triage" => tools::cuda::cuda_triage(args, &self.state),
            "cuda_regression_run" => tools::cuda::cuda_regression_run(args, &self.state),
            "cuda_regression_report" => tools::cuda::cuda_regression_report(args, &self.state),
            // Code navigation
            "read_context" => tools::code::read_context(args, &self.state),
            "grep_code" => tools::code::grep_code(args, &self.state),
            // Filesystem
            "read_file" => tools::filesystem::read_file(args, &self.state),
            "list_dir" => tools::filesystem::list_dir(args, &self.state),
            "glob" => tools::filesystem::glob_files(args, &self.state),
            "which" => tools::filesystem::which_bin(args),
            // State
            "shell_state" => tools::state::shell_state(args, &self.state),
            "set_cwd" => tools::state::set_cwd(args, &self.state),
            "control_reconcile" => tools::control::control_reconcile(args, &self.state),
            "config_ux" => tools::config_ux::config_ux(args),
            "ux_wizard" => tools::ux_wizard::ux_wizard(args),
            // Execution
            "exec" => tools::execution::exec_cmd(args, &self.state),
            "build_check" => tools::execution::build_check(args, &self.state),
            "task_run" => tools::execution::task_run(args, &self.state),
            "launch" => tools::execution::launch(args, &self.state),
            "close_observer" => tools::execution::close_observer(args),
            // Binary inspection
            "inspect_binary" => tools::binary::inspect_binary(args),
            // Rust tooling
            "cargo_tree" => tools::rust_tools::cargo_tree(args, &self.state),
            "test_run" => tools::rust_tools::test_run(args, &self.state),
            // GPU live state
            "gpu_live" => tools::hardware::gpu_live(args),
            // Profiling
            "ptx_inspect" => tools::profiling::ptx_inspect(args),
            "ncu_profile" => tools::profiling::ncu_profile(args, &self.state),
            "compute_sanitizer" => tools::profiling::compute_sanitizer(args, &self.state),
            // Benchmark history
            "bench_history" => tools::history::bench_history(args),
            // Incremental filesystem
            "changed_since" => tools::filesystem::changed_since(args, &self.state),
            // HTTP
            "http_request" => tools::http::http_request(args),
            // CPU profiling
            "flamegraph" => tools::perf_tools::flamegraph(args),
            "perf_stat" => tools::perf_tools::perf_stat(args),
            // Debugging
            "gdb_run" => tools::debug::gdb_run(args),
            // ML
            "tensor_inspect" => tools::ml::tensor_inspect(args),
            "checkpoint_list" => tools::ml::checkpoint_list(args, &self.state),
            // Git
            "git_log" => tools::git::git_log(args, &self.state),
            "git_diff" => tools::git::git_diff(args, &self.state),
            "git_status" => tools::git::git_status(args, &self.state),
            // Symbols
            "symbol_index" => tools::symbols::symbol_index(args, &self.state),
            "find_symbol" => tools::symbols::find_symbol(args, &self.state),
            // System
            "process_tree" => tools::system::process_tree(args),
            "port_list" => tools::system::port_list(args),
            "journal_query" => tools::system::journal_query(args),
            // File ops
            "move_file" => tools::filesystem::move_file(args),
            "mkdir" => tools::filesystem::make_dir(args),
            "delete_file" => tools::filesystem::delete_file(args),
            // Determinism-layer mutations & probes
            "write_file" => tools::mutate::write_file(args, &self.state),
            "edit_file" => tools::mutate::edit_file(args, &self.state),
            "sed_file" => tools::mutate::sed_file(args, &self.state),
            "apply_patch" => tools::mutate::apply_patch(args, &self.state),
            "stat_file" => tools::mutate::stat_file(args, &self.state),
            "read_bytes" => tools::mutate::read_bytes(args, &self.state),
            "diff_files" => tools::mutate::diff_files(args, &self.state),
            "hash_file" => tools::mutate::hash_file(args, &self.state),
            "edit_transaction" => tools::mutate::edit_transaction(args, &self.state),
            "replace_in_files" => tools::mutate::replace_in_files(args, &self.state),
            // Workspace
            "orient" => tools::workspace::orient(args, &self.state, &self.store),
            "note" => tools::workspace::note(args, &self.state),
            // EDA
            "verilog_lint" => tools::eda::verilog_lint(args),
            "verilog_sim" => tools::eda::verilog_sim(args),
            "xsim_elab" => tools::eda::xsim_elab(args),
            "cocotb_run" => tools::eda::cocotb_run(args),
            "vivado_tcl" => tools::eda::vivado_tcl(args),
            "fpga_boards" => tools::eda::fpga_boards(args),
            "fpga_program" => tools::eda::fpga_program(args),
            "waveform_query" => tools::eda::waveform_query(args),
            "synth_report" => tools::eda::synth_report(args),
            "fpga_serial" => tools::eda::fpga_serial(args),
            "fpga_tcfp_status" => tools::eda::fpga_tcfp_status(args),
            "fpga_tcfp_tile_read" => tools::eda::fpga_tcfp_tile_read(args),
            // Background process orchestration
            "bg_spawn" => tools::bg_spawn::bg_spawn(args, &self.store, &self.state),
            "bg_attach" => tools::bg_spawn::bg_attach(args, &self.store),
            "bg_send" => tools::bg_interact::bg_send(args, &self.store),
            "bg_status" => tools::bg_query::bg_status(args, &self.store),
            "bg_wait" => tools::bg_query::bg_wait(args, &self.store),
            "bg_tail" => tools::bg_query::bg_tail(args, &self.store),
            "bg_list" => tools::bg_query::bg_list(args, &self.store),
            "wait_for_pattern" => tools::bg_query::wait_for_pattern(args, &self.store),
            "wait_for_idle" => tools::bg_query::wait_for_idle(args, &self.store),
            "output_summary" => tools::bg_query::output_summary(args, &self.store),
            "bg_kill" => tools::bg_control::bg_kill(args, &self.store),
            "pipeline_run" => tools::bg_pipeline::pipeline_run(args, &self.pipelines, &self.state),
            "pipeline_status" => tools::bg_pipeline::pipeline_status(args, &self.pipelines),
            "pipeline_cancel" => tools::bg_pipeline::pipeline_cancel(args, &self.pipelines),
            "live_window" => tools::bg_window::live_window(args, &self.store),
            // Project / chip awareness (Tier 1)
            "project_context" => tools::project::project_context(args, &self.state),
            "chip_status" => tools::project::chip_status(args, &self.state),
            "chip_build_pipeline" => {
                tools::project::chip_build_pipeline(args, &self.store, &self.state)
            }
            "rtl_regression_run" => tools::project::rtl_regression_run(args, &self.state),
            "rtl_regression_report" => tools::project::rtl_regression_report(args, &self.state),
            "fpga_triage" => tools::project::fpga_triage(args, &self.state),
            "fpga_artifacts" => tools::project::fpga_artifacts(args, &self.state),
            "board_status" => tools::project::board_status(args),
            "fpga_monitor" => tools::project::fpga_monitor(args, &self.store),
            // Remote SSH (Tier 2)
            "remote_exec" => tools::remote::remote_exec(args),
            "remote_build" => tools::remote::remote_build(args, &self.store),
            "sync_project" => tools::remote::sync_project(args),
            // Project creation
            "project_new" => tools::git_new::project_new(args),
            // Git write
            "git_checkpoint" => tools::git_write::git_checkpoint(args, &self.state),
            "git_commit" => tools::git_write::git_commit(args, &self.state),
            // Notifications
            "notify" => tools::notify::notify(args),
            // tmux
            "tmux_ctl" => tools::tmux::tmux_ctl(args),
            // Network / reachability
            "tailscale_status" => tools::network::tailscale_status(args),
            // Session health
            "session_status" => tools::session::session_status(args),
            "session_restart" => tools::session::session_restart(args),
            // GitHub SSH
            "gh_clone" => tools::github::gh_clone(args),
            "gh_sync" => tools::github::gh_sync(args, &self.state),
            "gh_status" => tools::github::gh_status(args, &self.state),
            // Permissions / pre-validation
            "pre_validate" => tools::permissions_tool::pre_validate(args),
            "permissions_setup" => tools::permissions_tool::permissions_setup(args),
            // PTY / interactive program driver
            "tty_exec" => tools::tty_exec::tty_exec(args, &self.store, &self.state),
            // Server health
            "health" => tools::health::health(args, &self.metrics, &self.state, &self.store),
            // Environment pre-flight
            "env_doctor" => tools::env_doctor::env_doctor(args),
            _ => Err(format!("unknown tool: {name}")),
        }
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
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
            let notes = crate::tools::state::read_notes(state);
            let bytes = notes.iter().map(|n| n.len()).sum();
            (notes.len(), bytes)
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
            max_calls: std::env::var("FERRITE_MCP_MAX_CALLS")
                .ok()
                .and_then(|v| v.parse().ok()),
            max_uptime_secs: std::env::var("FERRITE_MCP_MAX_UPTIME_SECS")
                .ok()
                .and_then(|v| v.parse().ok()),
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
        self.max_calls
            .map(|v| snapshot.total_tool_calls >= v)
            .unwrap_or(false)
            || self
                .max_uptime_secs
                .map(|v| snapshot.uptime_secs >= v)
                .unwrap_or(false)
            || self
                .max_rss_bytes
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
        let kb = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u64>()
            .ok()?;
        Some(kb.saturating_mul(1024))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExecutionScheduler, HealthSnapshot, McpServer, RecyclePolicy, ToolConcurrencyLane,
        ToolExecutionPolicy,
    };
    use crate::job_store::JobStoreStats;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

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

    #[test]
    fn execution_policy_marks_read_only_tools_parallel() {
        let server = McpServer::new();
        let policy = server.tool_execution_policy("read_file", &json!({"path": "Cargo.toml"}));
        assert_eq!(policy.lane, ToolConcurrencyLane::ParallelRead);
        assert_eq!(
            policy.resource_keys,
            vec![format!(
                "path:{}",
                std::env::current_dir()
                    .unwrap()
                    .join("Cargo.toml")
                    .display()
            )]
        );
    }

    #[test]
    fn execution_policy_marks_git_reads_parallel_by_repo() {
        let server = McpServer::new();
        let policy = server.tool_execution_policy("git_status", &json!({"path": "/tmp/repo"}));
        assert_eq!(policy.lane, ToolConcurrencyLane::ParallelRead);
        assert_eq!(policy.resource_keys, vec!["repo:/tmp/repo".to_owned()]);
    }

    #[test]
    fn execution_policy_serializes_git_writes_by_repo() {
        let server = McpServer::new();
        let policy = server.tool_execution_policy("git_commit", &json!({"path": "/tmp/repo"}));
        assert_eq!(policy.lane, ToolConcurrencyLane::SerializedResource);
        assert_eq!(policy.resource_keys, vec!["repo:/tmp/repo".to_owned()]);
    }

    #[test]
    fn execution_policy_serializes_cwd_mutation_globally() {
        let server = McpServer::new();
        let policy = server.tool_execution_policy("set_cwd", &json!({"path": "/tmp"}));
        assert_eq!(policy.lane, ToolConcurrencyLane::SerializedState);
        assert!(policy.resource_keys.is_empty());
    }

    #[test]
    fn execution_policy_serializes_commands_that_may_mutate_or_run_code() {
        let server = McpServer::new();
        for tool in [
            "test_run",
            "ncu_profile",
            "compute_sanitizer",
            "flamegraph",
            "perf_stat",
            "gdb_run",
            "verilog_sim",
            "cocotb_run",
            "rtl_regression_run",
            "remote_exec",
        ] {
            let policy = server.tool_execution_policy(tool, &json!({"cwd": "/tmp/project"}));
            assert_eq!(
                policy.lane,
                ToolConcurrencyLane::SerializedResource,
                "{tool}"
            );
            assert_eq!(
                policy.resource_keys,
                vec!["path:/tmp/project".to_owned()],
                "{tool}"
            );
        }
    }

    #[test]
    fn execution_policy_tracks_both_diff_paths() {
        let server = McpServer::new();
        let policy = server
            .tool_execution_policy("diff_files", &json!({"a": "/tmp/a.txt", "b": "/tmp/b.txt"}));
        assert_eq!(policy.lane, ToolConcurrencyLane::ParallelRead);
        assert_eq!(
            policy.resource_keys,
            vec!["path:/tmp/a.txt".to_owned(), "path:/tmp/b.txt".to_owned()]
        );
    }

    #[test]
    fn scheduler_allows_different_resources_to_overlap() {
        let scheduler = Arc::new(ExecutionScheduler::default());
        let start = Instant::now();
        let mut handles = Vec::new();

        for key in ["path:/tmp/a", "path:/tmp/b"] {
            let scheduler = Arc::clone(&scheduler);
            let policy = ToolExecutionPolicy {
                lane: ToolConcurrencyLane::SerializedResource,
                resource_keys: vec![key.to_owned()],
                reason: "test",
            };
            handles.push(std::thread::spawn(move || {
                scheduler.run(&policy, || std::thread::sleep(Duration::from_millis(120)));
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
        assert!(start.elapsed() < Duration::from_millis(220));
    }

    #[test]
    fn scheduler_serializes_same_resource() {
        let scheduler = Arc::new(ExecutionScheduler::default());
        let start = Instant::now();
        let mut handles = Vec::new();

        for _ in 0..2 {
            let scheduler = Arc::clone(&scheduler);
            let policy = ToolExecutionPolicy {
                lane: ToolConcurrencyLane::SerializedResource,
                resource_keys: vec!["path:/tmp/shared".to_owned()],
                reason: "test",
            };
            handles.push(std::thread::spawn(move || {
                scheduler.run(&policy, || std::thread::sleep(Duration::from_millis(120)));
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
        assert!(start.elapsed() >= Duration::from_millis(220));
    }

    #[test]
    fn scheduler_allows_same_resource_reads_to_overlap() {
        let scheduler = Arc::new(ExecutionScheduler::default());
        let start = Instant::now();
        let mut handles = Vec::new();

        for _ in 0..2 {
            let scheduler = Arc::clone(&scheduler);
            let policy = ToolExecutionPolicy {
                lane: ToolConcurrencyLane::ParallelRead,
                resource_keys: vec!["path:/tmp/shared".to_owned()],
                reason: "test",
            };
            handles.push(std::thread::spawn(move || {
                scheduler.run(&policy, || std::thread::sleep(Duration::from_millis(120)));
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
        assert!(start.elapsed() < Duration::from_millis(220));
    }

    #[test]
    fn scheduler_blocks_same_resource_write_behind_read() {
        let scheduler = Arc::new(ExecutionScheduler::default());
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();

        let read_scheduler = Arc::clone(&scheduler);
        let reader = std::thread::spawn(move || {
            let policy = ToolExecutionPolicy {
                lane: ToolConcurrencyLane::ParallelRead,
                resource_keys: vec!["path:/tmp/shared".to_owned()],
                reason: "test",
            };
            read_scheduler.run(&policy, || {
                locked_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(150));
            });
        });

        locked_rx.recv().unwrap();
        let write_start = Instant::now();
        let write_policy = ToolExecutionPolicy {
            lane: ToolConcurrencyLane::SerializedResource,
            resource_keys: vec!["path:/tmp/shared".to_owned()],
            reason: "test",
        };
        scheduler.run(&write_policy, || {});

        reader.join().unwrap();
        assert!(write_start.elapsed() >= Duration::from_millis(120));
    }

    #[test]
    fn response_filter_does_not_steal_tool_filter_args() {
        let server = McpServer::new();
        let args = json!({"filter": "some_test"});
        assert_eq!(server.response_filter("test_run", &args), "");
    }

    #[test]
    fn response_filter_keeps_legacy_filter_for_other_tools() {
        let server = McpServer::new();
        let args = json!({"filter": "needle"});
        assert_eq!(server.response_filter("exec", &args), "needle");
    }

    #[test]
    fn response_filter_explicit_arg_overrides_tool_filter_arg() {
        let server = McpServer::new();
        let args = json!({"filter": "some_test", "response_filter": "needle"});
        assert_eq!(server.response_filter("test_run", &args), "needle");
    }
}
