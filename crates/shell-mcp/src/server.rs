//! MCP stdio server loop.
//!
//! Reads newline-delimited JSON from stdin, writes responses to stdout.
//! Stderr is reserved for diagnostics — never written during normal MCP operation.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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

pub struct McpServer {
    pub state:     Arc<Mutex<ServerState>>,
    pub store:     Arc<JobStore>,
    pub pipelines: Arc<PipelineStore>,
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

        let decision = crate::authz::authorize(name, args);
        crate::authz::audit(&decision, name, args);
        if !decision.allowed {
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

    fn call_tool(&self, name: &str, args: &Value) -> Result<ToolResult, String> {
        match name {
            // Discovery
            "find_lib"       => tools::discovery::find_lib(args),
            "discover"       => tools::discovery::discover(args),
            // Hardware
            "gpu_info"       => tools::hardware::gpu_info(args),
            "cpu_info"       => tools::hardware::cpu_info(args),
            "occupancy_calc" => tools::hardware::occupancy_calc(args),
            // Code navigation
            "read_context"   => tools::code::read_context(args),
            "grep_code"      => tools::code::grep_code(args),
            // Filesystem
            "read_file"      => tools::filesystem::read_file(args),
            "list_dir"       => tools::filesystem::list_dir(args),
            "glob"           => tools::filesystem::glob_files(args),
            "which"          => tools::filesystem::which_bin(args),
            // State
            "shell_state"    => tools::state::shell_state(args, &self.state),
            "set_cwd"        => tools::state::set_cwd(args, &self.state),
            "control_reconcile" => tools::control::control_reconcile(args, &self.state),
            #[cfg(feature = "fercuda-runtime-apply")]
            "fercuda_runtime" => tools::fercuda::runtime(args),
            "config_ux"      => tools::config_ux::config_ux(args),
            "ux_wizard"      => tools::ux_wizard::ux_wizard(args),
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
            "changed_since"  => tools::filesystem::changed_since(args),
            // HTTP
            "http_request"   => tools::http::http_request(args),
            // CPU profiling
            "flamegraph"     => tools::perf_tools::flamegraph(args),
            "perf_stat"      => tools::perf_tools::perf_stat(args),
            // Debugging
            "gdb_run"        => tools::debug::gdb_run(args),
            // ML
            "tensor_inspect"    => tools::ml::tensor_inspect(args),
            "checkpoint_list"   => tools::ml::checkpoint_list(args),
            // Git
            "git_log"        => tools::git::git_log(args),
            "git_diff"       => tools::git::git_diff(args),
            "git_status"     => tools::git::git_status(args),
            // Symbols
            "symbol_index"   => tools::symbols::symbol_index(args),
            "find_symbol"    => tools::symbols::find_symbol(args),
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
            "bg_spawn"         => tools::bg_spawn::bg_spawn(args, &self.store),
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
            "pipeline_run"     => tools::bg_pipeline::pipeline_run(args, &self.pipelines),
            "pipeline_status"  => tools::bg_pipeline::pipeline_status(args, &self.pipelines),
            "pipeline_cancel"  => tools::bg_pipeline::pipeline_cancel(args, &self.pipelines),
            "live_window"      => tools::bg_window::live_window(args, &self.store),
            // Project / chip awareness (Tier 1)
            "project_context"     => tools::project::project_context(args),
            "chip_status"         => tools::project::chip_status(args),
            "chip_build_pipeline" => tools::project::chip_build_pipeline(args, &self.store),
            "board_status"        => tools::project::board_status(args),
            "fpga_monitor"        => tools::project::fpga_monitor(args, &self.store),
            // Remote SSH (Tier 2)
            "remote_exec"   => tools::remote::remote_exec(args),
            "remote_build"  => tools::remote::remote_build(args, &self.store),
            "sync_project"  => tools::remote::sync_project(args),
            // Project creation
            "project_new"  => tools::git_new::project_new(args),
            // Git write
            "git_checkpoint" => tools::git_write::git_checkpoint(args),
            "git_commit"   => tools::git_write::git_commit(args),
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
            "gh_sync"   => tools::github::gh_sync(args),
            "gh_status" => tools::github::gh_status(args),
            // Permissions / pre-validation
            "pre_validate"      => tools::permissions_tool::pre_validate(args),
            "permissions_setup" => tools::permissions_tool::permissions_setup(args),
            // PTY / interactive program driver
            "tty_exec"          => tools::tty_exec::tty_exec(args, &self.store),
            // Environment pre-flight
            "env_doctor"        => tools::env_doctor::env_doctor(args),
            _           => Err(format!("unknown tool: {name}")),
        }
    }
}

impl Default for McpServer {
    fn default() -> Self { Self::new() }
}
