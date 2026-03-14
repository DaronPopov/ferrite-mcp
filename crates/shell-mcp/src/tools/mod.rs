pub mod bg_control;
pub mod bg_interact;
pub mod bg_pipeline;
pub mod bg_query;
pub mod bg_spawn;
pub mod bg_window;
pub mod binary;
pub mod code;
pub mod control;
pub mod config_ux;
pub mod debug;
pub mod discovery;
pub mod dynamic;
pub mod eda;
pub mod execution;
pub mod filesystem;
pub mod git;
pub mod git_new;
pub mod git_guard;
pub mod git_write;
pub mod github;
pub mod hardware;
pub mod health;
pub mod history;
pub mod http;
pub mod ml;
pub mod mobile_session;
pub mod network;
pub mod notify;
pub mod perf_tools;
pub mod profiling;
pub mod project;
pub mod remote;
pub mod rust_tools;
pub mod session;
pub mod state;
pub mod symbols;
pub mod system;
pub mod env_doctor;
pub mod fercuda;
pub mod permissions_tool;
pub mod tmux;
pub mod tty_exec;
pub mod ux_wizard;
pub mod workspace;

use crate::protocol::ToolDef;
use serde_json::json;

pub fn all_tool_definitions() -> Vec<ToolDef> {
    vec![
        // ── Discovery ─────────────────────────────────────────────────────────
        ToolDef {
            name: "find_lib",
            description: "Find a system library — pkg-config, ldconfig, env vars, common paths. Returns path, version, include dirs, link flags.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Library name (e.g. 'torch', 'cublas', 'opencv')" },
                    "version_hint": { "type": "string", "description": "Optional minimum version" }
                },
                "required": ["name"]
            }),
        },
        ToolDef {
            name: "discover",
            description: "Scan a category for all available toolchains, libraries, and SDKs.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "category": { "type": "string", "enum": ["cuda","rocm","ml","build-tools","all"] }
                },
                "required": ["category"]
            }),
        },

        // ── Hardware ──────────────────────────────────────────────────────────
        ToolDef {
            name: "gpu_info",
            description: "Full GPU device properties: compute capability, SM count, VRAM, \
                           warp size, shared memory, peak bandwidth, live utilization. \
                           Compiles a CUDA device query when nvcc is available. Cached per session.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "cpu_info",
            description: "CPU topology: model, core count, frequency, SIMD (AVX2/AVX-512/NEON/SVE), cache sizes.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "health",
            description: "Report ferrite MCP server health: uptime, RSS, tool-call count, note/job buffer pressure, and whether restart is recommended. Also exposes opt-in auto-recycle thresholds via FERRITE_MCP_MAX_CALLS / _UPTIME_SECS / _RSS_MB.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "occupancy_calc",
            description: "Calculate theoretical CUDA kernel occupancy for this GPU. \
                           Given threads_per_block, shared_mem_bytes, and optional registers_per_thread, \
                           returns active warps/blocks per SM, occupancy %, the limiting resource, \
                           and actionable tips. Auto-reads compute capability from the live GPU.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "threads_per_block":    { "type": "integer", "description": "Threads per CUDA block (1–1024)" },
                    "shared_mem_bytes":     { "type": "integer", "description": "Dynamic shared memory per block in bytes (0 if none)" },
                    "registers_per_thread": { "type": "integer", "description": "Registers per thread (0 to skip register limiter)" },
                    "compute_major":        { "type": "integer", "description": "Override compute capability major (e.g. 8)" },
                    "compute_minor":        { "type": "integer", "description": "Override compute capability minor (e.g. 6)" }
                },
                "required": ["threads_per_block"]
            }),
        },

        // ── Code navigation ───────────────────────────────────────────────────
        ToolDef {
            name: "read_context",
            description: "Read lines surrounding a specific location in a file. \
                           Called automatically after build_check errors to show the \
                           code at the error site without a separate Read round-trip.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file":   { "type": "string",  "description": "Absolute or relative file path" },
                    "line":   { "type": "integer", "description": "Target line number (1-indexed)" },
                    "radius": { "type": "integer", "description": "Lines above and below to include (default 10)" }
                },
                "required": ["file", "line"]
            }),
        },
        ToolDef {
            name: "grep_code",
            description: "Regex search across a file tree. Returns matches with file, line, column, \
                           content, and surrounding context. Uses ripgrep when available, pure-Rust fallback otherwise. \
                           Prefer this over the built-in Grep tool for dev work — no permission prompts and \
                           returns structured JSON instead of raw text. Use max_chars= to cap response size (0 = unlimited).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern":          { "type": "string",  "description": "Regex pattern (e.g. 'cudaMalloc', '__global__\\s+void\\s+\\w+')" },
                    "path":             { "type": "string",  "description": "Root directory to search (default: cwd)" },
                    "glob":             { "type": "string",  "description": "File glob filter (default: **/*.{rs,cu,c,cpp,h,cuh,py,toml})" },
                    "max_results":      { "type": "integer", "description": "Result cap (default 50)" },
                    "context_lines":    { "type": "integer", "description": "Context lines above/below each match (default 2)" },
                    "case_insensitive": { "type": "boolean", "description": "Case-insensitive match (default false)" },
                    "max_chars":        { "type": "integer", "description": "Max response chars (default 1024 ≈ 256 tokens; 0 = unlimited)" }
                },
                "required": ["pattern"]
            }),
        },

        // ── Filesystem ────────────────────────────────────────────────────────
        ToolDef {
            name: "read_file",
            description: "Read a file's contents. Optionally slice to a line range. \
                           Returns both a plain content string and a numbered lines array. \
                           Capped at 2000 lines per call — use start_line/end_line to paginate large files. \
                           Use filter= to keep only matching lines. Use max_chars= to cap response size (0 = unlimited). \
                           Prefer this over the built-in Read tool when you need pagination, filtering, or line-ranged reads.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":       { "type": "string",  "description": "Absolute or relative file path" },
                    "start_line": { "type": "integer", "description": "First line to return (1-indexed, default 1)" },
                    "end_line":   { "type": "integer", "description": "Last line to return (inclusive, default: end of file)" },
                    "filter":     { "type": "string",  "description": "Keep only lines containing this string (case-insensitive)" },
                    "max_chars":  { "type": "integer", "description": "Max response chars (default 1024 ≈ 256 tokens; 0 = unlimited)" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "list_dir",
            description: "List a directory's contents with type, size, and modified time. \
                           Supports recursive depth up to 5. Skips target/, node_modules/, .git/ automatically.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":        { "type": "string",  "description": "Directory to list (default: cwd)" },
                    "depth":       { "type": "integer", "description": "Recursion depth 1–5 (default 1)" },
                    "show_hidden": { "type": "boolean", "description": "Include dotfiles (default false)" }
                }
            }),
        },
        ToolDef {
            name: "glob",
            description: "Find files matching a glob pattern. Supports ** recursion.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern":     { "type": "string",  "description": "Glob pattern (e.g. 'src/**/*.cu')" },
                    "cwd":         { "type": "string",  "description": "Base directory for relative patterns" },
                    "max_results": { "type": "integer", "description": "Result cap (default 200)" }
                },
                "required": ["pattern"]
            }),
        },
        ToolDef {
            name: "which",
            description: "Look up a binary in PATH and probe its version.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Binary name (e.g. 'nvcc', 'cargo')" }
                },
                "required": ["name"]
            }),
        },
        // ── State ─────────────────────────────────────────────────────────────
        ToolDef {
            name: "shell_state",
            description: "Return ferrite's cwd, PATH, and dev-relevant environment variables.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "set_cwd",
            description: "Change ferrite's working directory. Persists across calls.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute or ~ path" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "control_reconcile",
            description: concat!(
                "Desired-state control loop scaffold for a self-maintaining stack. ",
                "Use op=set_desired|get_desired|tick|status|clear. ",
                "tick computes desired-vs-actual actions and emits execution intents. ",
                "Pass 'actual' to reconcile against observed runtime state."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": ["set_desired","get_desired","tick","status","clear"] },
                    "desired": {
                        "type": "object",
                        "description": "DesiredState v0 object. Required for set_desired; optional for tick."
                    },
                    "apply": {
                        "type": "boolean",
                        "description": "When op=tick: apply safe actions (default false)."
                    },
                    "enable_apply_runtime": {
                        "type": "boolean",
                        "description": "When op=tick and apply=true: permit direct execution bridge for supported actions."
                    }
                },
                "required": ["op"]
            }),
        },
        ToolDef {
            name: "config_ux",
            description: "Read and update ferrite config through MCP. op=list|get|set. Includes authz policy path introspection.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "op":   { "type": "string", "enum": ["list","get","set"], "description": "Operation (default list)" },
                    "key":  { "type": "string", "description": "Config key for get/set (e.g. terminal.mode)" },
                    "value":{ "type": "string", "description": "Value for set" }
                }
            }),
        },
        ToolDef {
            name: "ux_wizard",
            description: "Interactive question flow for staged config changes. Current workflow: fercuda_authz_limits. op=start|answer|status|apply|reset.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "op":         { "type": "string", "enum": ["start","answer","status","apply","reset"], "description": "Wizard operation" },
                    "workflow":   { "type": "string", "description": "Workflow id (default fercuda_authz_limits)" },
                    "question_id":{ "type": "string", "description": "Question id when op=answer" },
                    "value":      { "description": "Answer value when op=answer (string or integer)" }
                },
                "required": ["op"]
            }),
        },
        ToolDef {
            name: "fercuda_runtime",
            description: "Operate feRcuda through MCP. Preferred contract shape is {action, input, agent_api_version}; legacy {op,...} is still accepted. Supports runtime inspect, session lifecycle, tensor IO, JIT compile/bind/launch, fixed ops, and job control.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "agent_api_version": { "type": "string", "description": "Preferred value: v1alpha1" },
                    "action": { "type": "string", "description": "Canonical action name, e.g. session.create, tensor.upload, jit.kernel.launch" },
                    "input": {
                        "type": "object",
                        "description": "Structured payload for the selected action."
                    },
                    "op": {
                        "type": "string",
                        "description": "Legacy compatibility field. Prefer 'action' + 'input'."
                    }
                },
                "anyOf": [
                    { "required": ["action"] },
                    { "required": ["op"] }
                ]
            }),
        },

        // ── Execution ─────────────────────────────────────────────────────────
        ToolDef {
            name: "exec",
            description: "Run a shell command without permission prompts. Returns stdout, stderr, exit_code, \
                           duration_ms, timed_out. Uses /bin/sh -c so pipes, redirects, and compound commands work. \
                           Prefer this over the built-in Bash tool for builds, test runs, and any command that \
                           would otherwise prompt the user. Use filter= to keep only matching output lines. \
                           Use max_chars= to cap response (0 = unlimited).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cmd":          { "type": "string",  "description": "Shell command to run" },
                    "cwd":          { "type": "string",  "description": "Override working directory" },
                    "env":          { "type": "object",  "description": "Extra environment variables", "additionalProperties": { "type": "string" } },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 60)" },
                    "stdin":        { "type": "string",  "description": "Optional stdin" },
                    "filter":       { "type": "string",  "description": "Keep only output lines containing this string (case-insensitive)" },
                    "max_chars":    { "type": "integer", "description": "Max response chars (default 1024 ≈ 256 tokens; 0 = unlimited)" }
                },
                "required": ["cmd"]
            }),
        },
        ToolDef {
            name: "build_check",
            description: "Compile a file or project. Auto-detects cuda/rust/c/cpp. \
                           For CUDA: resolves correct -arch flags from the live GPU. \
                           For Rust: cargo check --message-format=json. \
                           Returns structured errors with file/line/col — no text parsing needed.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file":  { "type": "string", "description": "File or directory to compile (. for Rust workspace)" },
                    "type":  { "type": "string", "enum": ["auto","cuda","rust","c","cpp"] },
                    "flags": { "type": "array",  "items": { "type": "string" }, "description": "Extra compiler flags" }
                },
                "required": ["file"]
            }),
        },

        // ── Binary inspection ─────────────────────────────────────────────────
        ToolDef {
            name: "inspect_binary",
            description: "Inspect a compiled binary: dynamic dependencies (ldd), \
                           symbol table (nm -D), ELF header (readelf -h). \
                           Surfaces CUDA symbols and missing .so files automatically.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the compiled binary or .so" }
                },
                "required": ["path"]
            }),
        },

        // ── Rust tooling ──────────────────────────────────────────────────────
        ToolDef {
            name: "cargo_tree",
            description: "Parse a Cargo workspace: members, versions, features, dependencies. \
                           Replaces manually reading multiple Cargo.toml files. \
                           Set full=true for the complete resolved dependency graph.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string",  "description": "Path to workspace (default: cwd)" },
                    "full": { "type": "boolean", "description": "Include resolved dependency graph (slower)" }
                }
            }),
        },
        ToolDef {
            name: "test_run",
            description: "Run cargo tests and return structured results: passed/failed/ignored counts \
                           with test names. Optional filter narrows to matching test names.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "filter":       { "type": "string", "description": "Test name filter (substring match)" },
                    "package":      { "type": "string", "description": "Cargo package name (default: all)" },
                    "timeout_secs": { "type": "integer", "description": "Timeout (default 120s)" }
                }
            }),
        },

        // ── GPU Profiling ─────────────────────────────────────────────────────
        ToolDef {
            name: "gpu_live",
            description: "Live GPU state: utilization %, temperature, power draw vs limit, \
                           SM and memory clock speeds, VRAM used/total. \
                           Run before benchmarks to confirm the GPU is idle and at boost clocks. \
                           Returns ready_to_bench: true when GPU utilization < 5%.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "ptx_inspect",
            description: "Inspect PTX (virtual assembly) for a CUDA kernel. \
                           Compiles .cu source to PTX via nvcc --ptx, or dumps PTX from a binary via cuobjdump. \
                           Returns per-kernel register counts (b32/f32/b64/pred), shared memory bytes, \
                           global/shared memory op counts, and ptxas verbose stats (regs/smem/lmem).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":   { "type": "string", "description": "Path to .cu source or compiled binary" },
                    "kernel": { "type": "string", "description": "Filter by kernel name substring (default: all)" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "ncu_profile",
            description: "Profile CUDA kernels with Nsight Compute CLI (ncu). \
                           Returns per-kernel: achieved occupancy %, SM throughput %, DRAM throughput %, \
                           L1/L2 cache hit rates, stall reasons. \
                           Requires ncu in PATH and sufficient permissions (perf_event_paranoid). \
                           Note: profiling adds significant overhead — use on small N.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cmd":          { "type": "string",  "description": "Command to profile (e.g. './bench')" },
                    "kernel":       { "type": "string",  "description": "Kernel name filter (default: all)" },
                    "metrics":      { "type": "string",  "description": "Custom metric list (default: occupancy + bandwidth + cache)" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 120)" }
                },
                "required": ["cmd"]
            }),
        },
        ToolDef {
            name: "compute_sanitizer",
            description: "Run CUDA compute-sanitizer to detect memory errors, race conditions, \
                           uninitialized memory, or barrier errors. \
                           Returns structured errors with kernel name, thread location, file, and line number.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cmd":          { "type": "string", "description": "Command to sanitize (e.g. './bench')" },
                    "tool":         { "type": "string", "enum": ["memcheck","racecheck","initcheck","synccheck"],
                                      "description": "Sanitizer tool (default: memcheck)" },
                    "timeout_secs": { "type": "integer", "description": "Timeout (default 120s)" }
                },
                "required": ["cmd"]
            }),
        },

        // ── Benchmark history ─────────────────────────────────────────────────
        ToolDef {
            name: "bench_history",
            description: "Persistent benchmark result store across sessions. \
                           record: save a result. list: show last N records. \
                           query: filter by tag/kernel. compare: diff last two records for a tag. \
                           Stored at ~/.local/share/ferrite/bench_history.jsonl.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "op":     { "type": "string", "enum": ["record","list","query","compare"],
                                "description": "Operation" },
                    "tag":    { "type": "string",  "description": "Short label (e.g. 'saxpy', 'matmul_tile16')" },
                    "value":  { "type": "number",  "description": "Numeric result (record only)" },
                    "unit":   { "type": "string",  "description": "Unit string: 'GB/s', 'GFLOP/s', 'ms' (record only)" },
                    "kernel": { "type": "string",  "description": "Kernel name annotation (optional)" },
                    "N":      { "type": "integer", "description": "Problem size annotation (optional)" },
                    "config": { "type": "string",  "description": "Config description (optional)" },
                    "notes":  { "type": "string",  "description": "Free-form notes (optional)" },
                    "limit":  { "type": "integer", "description": "Max records to return (list/query, default 20)" }
                },
                "required": ["op"]
            }),
        },

        // ── Filesystem (incremental) ───────────────────────────────────────────
        ToolDef {
            name: "changed_since",
            description: "Find files modified after a given time. \
                           Provide since_secs (Unix timestamp) or since_relative ('10m', '2h', '1d', '30s'). \
                           Skips target/, node_modules/, hidden dirs. Returns list sorted newest-first.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":            { "type": "string",  "description": "Root directory to search (default: cwd)" },
                    "since_secs":      { "type": "integer", "description": "Unix timestamp threshold" },
                    "since_relative":  { "type": "string",  "description": "Relative time: '10m', '2h', '1d', '30s'" },
                    "max_results":     { "type": "integer", "description": "Result cap (default 100)" }
                }
            }),
        },

        // ── Workspace ─────────────────────────────────────────────────────────
        ToolDef {
            name: "orient",
            description: "Single-call situational awareness. Returns a compact `summary` string plus cwd, \
                           git state, recently changed SOURCE files (build artifacts suppressed), and shallow \
                           directory tree. Ports are opt-in via ports:true and filtered to named processes only — \
                           this keeps token cost low (~100-200 tokens vs 600+ before). Use at the start of a \
                           session or when context is lost.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":    { "type": "string",  "description": "Root path (default: cwd)" },
                    "root":    { "type": "string",  "description": "Root for project discovery scan (default: $HOME)" },
                    "since":   { "type": "string",  "description": "Recent-changes window (default '2h'). E.g. '30m', '1d'" },
                    "depth":   { "type": "integer", "description": "Directory tree depth (default 2)" },
                    "ports":   { "type": "boolean", "description": "Include listening ports filtered to named processes (default false)" },
                    "bg_jobs": { "type": "boolean", "description": "Include running + recent background jobs (default false)" },
                    "chips":   { "type": "boolean", "description": "Scan processor_lab chips — sim/bit/WNS status. Auto-detects lab path from cwd (default false)" },
                    "synth":   { "type": "boolean", "description": "Parse Vivado timing/utilization reports near cwd — WNS, LUT%, DSP% (default false)" },
                    "diff":    { "type": "boolean", "description": "Show compact git diff --stat for dirty repos (default false)" },
                    "hw":      { "type": "boolean", "description": "Include live GPU utilization + VRAM (default false)" }
                }
            }),
        },
        ToolDef {
            name: "note",
            description: "Session scratchpad — persist observations, decisions, or TODOs across tool calls \
                           within this session. Notes are stored in server memory (not disk) and reset when \
                           ferrite restarts. Use to avoid re-deriving context mid-session.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "op":      { "type": "string", "enum": ["read","append","clear"],
                                 "description": "read: return all notes; append: add a note; clear: wipe all" },
                    "content": { "type": "string", "description": "Note text (required for append)" }
                },
                "required": ["op"]
            }),
        },

        // ── EDA ───────────────────────────────────────────────────────────────
        ToolDef {
            name: "verilog_lint",
            description: "Lint Verilog/SystemVerilog files with iverilog -tnull. \
                           Returns structured diagnostics: [{severity, file, line, message}].",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "files":        { "type": "array",  "items": { "type": "string" }, "description": "Source files to lint" },
                    "top":          { "type": "string", "description": "Top-level module name (optional)" },
                    "include_dirs": { "type": "array",  "items": { "type": "string" }, "description": "Include search paths" },
                    "libraries":    { "type": "array",  "items": { "type": "string" }, "description": "Optional library presets: xilinx_unisims, xilinx_unisims_7series, xilinx_xpm_memory" },
                    "standard":     { "type": "string", "description": "Verilog standard: 2005, 2012 (default), sv" }
                },
                "required": ["files"]
            }),
        },
        ToolDef {
            name: "verilog_sim",
            description: "Compile Verilog with iverilog then simulate with vvp. \
                           Returns success, $finish status, assertion failures, stdout/stderr.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "files":        { "type": "array",   "items": { "type": "string" }, "description": "Source files (testbench + DUT)" },
                    "top":          { "type": "string",  "description": "Top module / testbench name (default: tb)" },
                    "include_dirs": { "type": "array",   "items": { "type": "string" }, "description": "Include paths" },
                    "libraries":    { "type": "array",   "items": { "type": "string" }, "description": "Optional library presets: xilinx_unisims, xilinx_unisims_7series, xilinx_xpm_memory" },
                    "vcd_out":      { "type": "string",  "description": "Optional VCD output path" },
                    "standard":     { "type": "string",  "description": "Verilog standard (default: 2012)" },
                    "timeout_secs": { "type": "integer", "description": "Simulation timeout (default 30s)" }
                },
                "required": ["files"]
            }),
        },
        ToolDef {
            name: "xsim_elab",
            description: "Run Vivado Simulator front-end checks via xvlog/xelab. \
                           Supports Xilinx precompiled libraries for UNISIM and XPM-heavy designs.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "files":        { "type": "array",   "items": { "type": "string" }, "description": "Source files to compile with xvlog" },
                    "top":          { "type": "string",  "description": "Top module to elaborate with xelab" },
                    "include_dirs": { "type": "array",   "items": { "type": "string" }, "description": "Include search paths passed via xvlog -i" },
                    "defines":      { "type": "array",   "items": { "type": "string" }, "description": "Verilog macro defines in NAME or NAME=VALUE form" },
                    "libraries":    { "type": "array",   "items": { "type": "string" }, "description": "Optional Xilinx library presets: xilinx_unisims, xilinx_unisims_7series, xilinx_xpm_cdc, xilinx_xpm_fifo, xilinx_xpm_memory" },
                    "standard":     { "type": "string",  "description": "Verilog standard hint: 2005, 2012 (default), sv" },
                    "snapshot":     { "type": "string",  "description": "Optional xelab snapshot name (default: ferrite_xsim)" }
                },
                "required": ["files", "top"]
            }),
        },
        ToolDef {
            name: "cocotb_run",
            description: "Run cocotb 2.x tests via pytest in a directory. \
                           Returns {passed, failed} test list with simulator and duration.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "dir":          { "type": "string",  "description": "Directory containing cocotb tests / Makefile" },
                    "simulator":    { "type": "string",  "description": "Simulator backend: icarus (default), verilator, ghdl" },
                    "module":       { "type": "string",  "description": "Python test module or pytest path filter" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 120)" }
                },
                "required": ["dir"]
            }),
        },
        ToolDef {
            name: "rtl_regression_run",
            description: "Run a chip-level RTL regression flow using ferrite's path resolution. \
                           Defaults to lint + sim and returns per-step results.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "chip":         { "type": "string",  "description": "Chip name under processor_lab/chips" },
                    "lab_path":     { "type": "string",  "description": "Path to processor_lab (default ~/processor_lab)" },
                    "board":        { "type": "string",  "description": "Board target (default basys3)" },
                    "steps":        { "type": "array",   "items": { "type": "string" }, "description": "Regression steps (default: [lint, sim])" },
                    "timeout_secs": { "type": "integer", "description": "Per-step timeout in seconds (default 300)" },
                    "dry_run":      { "type": "boolean", "description": "Show resolved commands without running them" }
                },
                "required": ["chip"]
            }),
        },
        ToolDef {
            name: "vivado_tcl",
            description: "Run Vivado 2025.2 in batch Tcl mode. \
                           Provide 'script' (file path) or 'cmd' (inline Tcl). \
                           Use for synthesis, implementation, bitstream generation, project queries. \
                           Returns {success, errors, warnings, output}.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "script":       { "type": "string",  "description": "Path to a .tcl script file" },
                    "cmd":          { "type": "string",  "description": "Inline Tcl command(s) to execute" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 600)" }
                }
            }),
        },
        ToolDef {
            name: "fpga_boards",
            description: "List FPGA boards connected via JTAG using Vivado hw_manager. \
                           Returns [{target, device, part, status}] for each found device.",
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "fpga_program",
            description: "Program a .bit bitstream to a connected FPGA via Vivado hw_manager. \
                           Uses the first available JTAG target unless 'target' is specified.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "bitfile":      { "type": "string",  "description": "Path to the .bit bitstream file" },
                    "target":       { "type": "string",  "description": "JTAG target name (default: first found)" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 120)" }
                },
                "required": ["bitfile"]
            }),
        },
        // ── Git ───────────────────────────────────────────────────────────────
        ToolDef {
            name: "git_log",
            description: "Structured git commit history. Returns [{hash, short, author, date, subject}]. \
                           Filter by author, since date, branch, or file path.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":   { "type": "string",  "description": "Repo path (default: cwd)" },
                    "limit":  { "type": "integer", "description": "Max commits to return (default 20)" },
                    "author": { "type": "string",  "description": "Filter by author name/email" },
                    "since":  { "type": "string",  "description": "Since date (e.g. '2 weeks ago', '2024-01-01')" },
                    "branch": { "type": "string",  "description": "Branch or ref to log (default HEAD)" },
                    "file":   { "type": "string",  "description": "Limit to commits touching this file" }
                }
            }),
        },
        ToolDef {
            name: "git_diff",
            description: "Uncommitted changes as structured file+hunk data. \
                           Returns [{path, additions, deletions, hunks: [{header, lines}]}]. \
                           Use staged=true for index vs HEAD.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":   { "type": "string",  "description": "Repo path (default: cwd)" },
                    "staged": { "type": "boolean", "description": "Diff staged (index) vs HEAD (default false = working tree)" },
                    "file":   { "type": "string",  "description": "Limit diff to a specific file" },
                    "commit": { "type": "string",  "description": "Diff against a specific commit or ref" }
                }
            }),
        },
        ToolDef {
            name: "git_status",
            description: "Working tree state: branch, ahead/behind, staged/unstaged/untracked files. \
                           Returns structured lists, not raw git output.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Repo path (default: cwd)" }
                }
            }),
        },

        // ── Symbols ───────────────────────────────────────────────────────────
        ToolDef {
            name: "symbol_index",
            description: "Walk Rust source files and index all symbols: fn, struct, enum, trait, impl, \
                           type, const, static, mod. Returns [{kind, name, file, line, public}]. \
                           Filter by kinds array. Use find_symbol for targeted lookup.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":  { "type": "string",  "description": "Root path to index (default: cwd)" },
                    "kinds": { "type": "array", "items": { "type": "string" },
                               "description": "Filter to these kinds: fn, struct, enum, trait, impl, type, const, static, mod" },
                    "limit": { "type": "integer", "description": "Max symbols to return (default 2000)" }
                }
            }),
        },
        ToolDef {
            name: "find_symbol",
            description: "Find a Rust symbol by name across source files. \
                           Returns [{kind, name, file, line, public}]. \
                           Substring match by default; use exact=true for exact name.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name":  { "type": "string",  "description": "Symbol name to find" },
                    "path":  { "type": "string",  "description": "Root path to search (default: cwd)" },
                    "kinds": { "type": "array", "items": { "type": "string" },
                               "description": "Limit to these kinds: fn, struct, enum, trait, impl, type, const, mod" },
                    "exact": { "type": "boolean", "description": "Exact name match (default false = substring)" }
                },
                "required": ["name"]
            }),
        },

        // ── System ────────────────────────────────────────────────────────────
        ToolDef {
            name: "process_tree",
            description: "List running processes from /proc. Returns [{pid, ppid, name, state, mem_kb, cmdline}]. \
                           Filter by process name substring.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "filter": { "type": "string",  "description": "Filter by process name substring (case-insensitive)" },
                    "limit":  { "type": "integer", "description": "Max processes to return (default 200)" }
                }
            }),
        },
        ToolDef {
            name: "port_list",
            description: "List listening TCP/UDP ports via ss. Returns [{proto, addr, port, pid, process}].",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "proto": { "type": "string", "enum": ["tcp", "udp", ""], "description": "Filter by protocol (default: both)" }
                }
            }),
        },
        ToolDef {
            name: "journal_query",
            description: "Query systemd journal via journalctl. Returns [{time, unit, pid, message}]. \
                           Filter by unit, time range, grep pattern.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "unit":  { "type": "string",  "description": "Systemd unit name (e.g. 'nginx.service')" },
                    "since": { "type": "string",  "description": "Time range (default '1h ago')" },
                    "grep":  { "type": "string",  "description": "Message grep pattern" },
                    "limit": { "type": "integer", "description": "Max entries (default 50)" },
                    "boot":  { "type": "boolean", "description": "Limit to current boot (default false)" }
                }
            }),
        },

        // ── File operations ───────────────────────────────────────────────────
        ToolDef {
            name: "move_file",
            description: "Move or rename a file/directory. With find_refs=true (default), scans .rs files \
                           for mod/use references to the moved file and returns them for update.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "src":       { "type": "string",  "description": "Source path" },
                    "dst":       { "type": "string",  "description": "Destination path" },
                    "find_refs": { "type": "boolean", "description": "Scan for Rust mod/use references (default true)" }
                },
                "required": ["src", "dst"]
            }),
        },
        ToolDef {
            name: "mkdir",
            description: "Create a directory. Creates intermediate parents by default.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":    { "type": "string",  "description": "Directory path to create" },
                    "parents": { "type": "boolean", "description": "Create parent dirs (default true)" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "delete_file",
            description: "Delete a single file. Refuses directories for safety — use exec('rm -rf') explicitly for those.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to delete" }
                },
                "required": ["path"]
            }),
        },

        // ── HTTP ──────────────────────────────────────────────────────────────
        ToolDef {
            name: "http_request",
            description: "Send an HTTP/HTTPS request via curl. Returns {ok, status, latency_ms, headers, body}. \
                           Body is parsed as JSON when possible. Use for probing running services and APIs.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url":               { "type": "string",  "description": "Request URL" },
                    "method":            { "type": "string",  "description": "HTTP method (default GET)" },
                    "body":              { "type": "string",  "description": "Request body (default Content-Type: application/json)" },
                    "headers":           { "type": "object",  "description": "Additional request headers as key:value pairs" },
                    "timeout_secs":      { "type": "integer", "description": "Request timeout (default 30s)" },
                    "follow_redirects":  { "type": "boolean", "description": "Follow HTTP redirects (default true)" },
                    "insecure":          { "type": "boolean", "description": "Skip TLS certificate verification (default false)" }
                },
                "required": ["url"]
            }),
        },

        // ── CPU profiling ─────────────────────────────────────────────────────
        ToolDef {
            name: "flamegraph",
            description: "Profile a command with Linux perf and generate a CPU flamegraph. \
                           Returns top hotspot functions. Generates SVG if inferno or flamegraph.pl is installed. \
                           May require: echo 0 | sudo tee /proc/sys/kernel/perf_event_paranoid",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cmd":          { "type": "string",  "description": "Command to profile (shell string)" },
                    "output":       { "type": "string",  "description": "SVG output path (default /tmp/ferrite_flamegraph.svg)" },
                    "freq":         { "type": "integer", "description": "Sampling frequency Hz (default 99)" },
                    "timeout_secs": { "type": "integer", "description": "Max profiling time (default 60s)" }
                },
                "required": ["cmd"]
            }),
        },
        ToolDef {
            name: "perf_stat",
            description: "Run a command under Linux perf stat. Returns CPU counters: cycles, instructions, \
                           cache-misses, branch-misses, IPC, task-clock. Structured output, no text parsing needed.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cmd":          { "type": "string", "description": "Command to measure (shell string)" },
                    "events":       { "type": "string", "description": "Comma-separated perf events (default: cycles,instructions,cache-misses,cache-references,branch-misses,task-clock)" },
                    "timeout_secs": { "type": "integer", "description": "Timeout (default 60s)" }
                },
                "required": ["cmd"]
            }),
        },

        // ── Debugging ─────────────────────────────────────────────────────────
        ToolDef {
            name: "gdb_run",
            description: "Run a binary under GDB in batch mode. Returns structured backtrace, locals, \
                           signal info, breakpoint hits. Supports core dump analysis and custom GDB commands.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "binary":       { "type": "string",  "description": "Path to the binary to debug" },
                    "args":         { "type": "string",  "description": "Arguments to pass to the binary" },
                    "core":         { "type": "string",  "description": "Core dump file for post-mortem analysis" },
                    "breakpoints":  { "type": "array", "items": { "type": "string" }, "description": "Breakpoint locations (e.g. 'main', 'file.c:42', 'ClassName::method')" },
                    "commands":     { "type": "array", "items": { "type": "string" }, "description": "Additional GDB commands to run (e.g. 'print var', 'x/10x $rsp')" },
                    "timeout_secs": { "type": "integer", "description": "Timeout (default 30s)" }
                },
                "required": ["binary"]
            }),
        },

        // ── ML data ───────────────────────────────────────────────────────────
        ToolDef {
            name: "tensor_inspect",
            description: "Inspect a PyTorch .pt/.pth file. Returns tensor shapes, dtypes, and statistics \
                           (min/max/mean/std) for each tensor. Requires python3 + torch.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":        { "type": "string",  "description": "Path to .pt/.pth file" },
                    "keys":        { "type": "array", "items": { "type": "string" }, "description": "Filter to specific top-level keys (default: all)" },
                    "max_tensors": { "type": "integer", "description": "Max tensors to inspect (default 50)" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "checkpoint_list",
            description: "Enumerate PyTorch checkpoint files (.pt/.pth/.ckpt/.bin/.safetensors) in a directory. \
                           Returns size, mtime, top-level keys, epoch/step if present. Newest-first.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":       { "type": "string",  "description": "Directory to search (default: cwd)" },
                    "inspect":    { "type": "boolean", "description": "Load each file to extract keys/metadata (default true)" },
                    "max_files":  { "type": "integer", "description": "Max files to return (default 20)" }
                }
            }),
        },

        ToolDef {
            name: "waveform_query",
            description: "Parse a VCD waveform file from simulation. \
                           Returns signal definitions and value-change events. \
                           Filter by signal name, time range, and max event count.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "vcd_file":   { "type": "string",  "description": "Path to the .vcd file" },
                    "signals":    { "type": "array",   "items": { "type": "string" }, "description": "Signal name substrings to include (default: all)" },
                    "time_start": { "type": "integer", "description": "Start time (in VCD timescale units)" },
                    "time_end":   { "type": "integer", "description": "End time (in VCD timescale units)" },
                    "max_events": { "type": "integer", "description": "Max events to return (default 500)" }
                },
                "required": ["vcd_file"]
            }),
        },

        ToolDef {
            name: "synth_report",
            description: "Parse Vivado synthesis/implementation report files and return structured \
                           timing and utilization data. Extracts WNS, TNS, failing endpoints from \
                           timing_summary.rpt and LUT%, FF%, BRAM, DSP counts from utilization.rpt. \
                           Pass a project directory and tether will auto-locate the report files, \
                           or specify explicit file paths.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project_dir":    { "type": "string", "description": "Vivado project directory — auto-locates .rpt files under runs/" },
                    "timing_rpt":     { "type": "string", "description": "Explicit path to timing_summary.rpt (overrides project_dir search)" },
                    "utilization_rpt":{ "type": "string", "description": "Explicit path to utilization.rpt (overrides project_dir search)" }
                }
            }),
        },

        // ── FPGA serial / TCFP observation ────────────────────────────────────
        ToolDef {
            name: "fpga_serial",
            description: "Low-level UART send/receive for FPGA communication. \
                           send: hex string (e.g. '52 01'). \
                           Auto-detects /dev/ttyUSB* or use port override.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "send":        { "type": "string",  "description": "Hex bytes to send, e.g. '52 01 FF'" },
                    "read_bytes":  { "type": "integer", "description": "Number of response bytes to read (0 = just send)" },
                    "port":        { "type": "string",  "description": "Serial port (default: auto-detect /dev/ttyUSB*)" },
                    "baud":        { "type": "integer", "description": "Baud rate (default: 921600)" },
                    "timeout_ms":  { "type": "integer", "description": "Read timeout ms (default: 500)" }
                }
            }),
        },
        ToolDef {
            name: "fpga_tcfp_status",
            description: "Read live status from the TCFP processor over UART. \
                           Returns busy/converged/done flags and step_count. \
                           Reads REG_STATUS (0x01) and REG_STEP_COUNT (0x02).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "port": { "type": "string",  "description": "Serial port (default: auto-detect)" },
                    "baud": { "type": "integer", "description": "Baud rate (default: 921600)" }
                }
            }),
        },
        ToolDef {
            name: "launch",
            description: "Fire-and-forget process launch. Spawns detached, returns PID immediately. \
                           Use for GUI apps (Vivado, waveform viewers, terminals) or long-running \
                           daemons where the output doesn't need to be reasoned about.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string", "description": "Shell command to launch detached" },
                    "cwd": { "type": "string", "description": "Working directory (default: ferrite cwd)" }
                },
                "required": ["cmd"]
            }),
        },
        ToolDef {
            name: "close_observer",
            description: "Close a terminal observer window opened by exec/build_check/task_run. \
                           Sends SIGTERM to the shell running inside the terminal, which kills \
                           the tail|awk pipeline and closes the window. \
                           Pass observer_pid_file from the exec result, or omit to close the most \
                           recently opened observer.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pid_file": { "type": "string", "description": "Path from exec result's observer_pid_file (omit to auto-find newest)" }
                }
            }),
        },
        ToolDef {
            name: "task_run",
            description: "Write a script to a tempfile and execute it atomically in one tool call. \
                           Use for multi-step hardware experiments, data sweeps, or any workflow \
                           that would otherwise require many round-trips. \
                           Returns {stdout, stderr, exit_code, duration_ms, success, timed_out}.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "script":       { "type": "string",  "description": "Script source code to execute" },
                    "interpreter":  { "type": "string",  "description": "Interpreter: python3 (default), python, bash, sh" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 120)" },
                    "cwd":          { "type": "string",  "description": "Working directory override (default: ferrite cwd)" }
                },
                "required": ["script"]
            }),
        },
        ToolDef {
            name: "fpga_tcfp_tile_read",
            description: "Read tile state vectors from the TCFP array via UART 'Q' command. \
                           Returns phase/field/coupling/bias as raw int32 + float (Q16.16) per tile. \
                           Defaults to all 4 tiles of the 2x2 array.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tiles": {
                        "type": "array",
                        "description": "Tiles to read: [{row, col}]. Default: all 4 tiles of 2x2.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "row": { "type": "integer" },
                                "col": { "type": "integer" }
                            }
                        }
                    },
                    "port": { "type": "string",  "description": "Serial port (default: auto-detect)" },
                    "baud": { "type": "integer", "description": "Baud rate (default: 921600)" }
                }
            }),
        },

        // ── Background process orchestration ──────────────────────────────────
        ToolDef {
            name: "bg_spawn",
            description: "Spawn a shell command in the background. Returns a job_id immediately — \
                the process runs while you continue other work. Output is buffered and persisted to \
                ~/.local/share/ferrite/logs/. Use bg_status to poll, bg_wait to block, \
                wait_for_pattern to watch for events, or live_window for a live terminal view.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cmd":   { "type": "string",  "description": "Shell command to run (passed to /bin/sh -c)" },
                    "cwd":   { "type": "string",  "description": "Working directory" },
                    "label": { "type": "string",  "description": "Human-readable label for bg_list (default: the command)" },
                    "env": {
                        "type": "object",
                        "description": "Extra environment variables",
                        "additionalProperties": { "type": "string" }
                    },
                    "pty": { "type": "boolean", "description": "Spawn in a pseudo-terminal (PTY). Required for interactive programs (Vivado, Python REPL, GDB). Enables bg_send for stdin. (default: false)" }
                },
                "required": ["cmd"]
            }),
        },
        ToolDef {
            name: "bg_attach",
            description: "Attach to an existing running process by PID. Tracks it via /proc so \
                bg_status can tell you when it finishes. Output is not captured (use bg_spawn for that).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pid":   { "type": "integer", "description": "PID of the running process" },
                    "label": { "type": "string",  "description": "Human-readable label" }
                },
                "required": ["pid"]
            }),
        },
        ToolDef {
            name: "bg_send",
            description: "Send text to the stdin of a PTY job (spawned with pty:true). \
                Use this to interact with Vivado, Python REPLs, GDB, or any interactive process. \
                Each call writes the text (and an optional trailing newline) to the process's terminal. \
                Use bg_status to read the response.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id":  { "type": "string",  "description": "Job ID of a PTY-spawned job" },
                    "text":    { "type": "string",  "description": "Text to send (e.g. a command string)" },
                    "newline": { "type": "boolean", "description": "Append a newline if text doesn't end with one (default: true)" }
                },
                "required": ["job_id", "text"]
            }),
        },
        ToolDef {
            name: "bg_status",
            description: "Poll a background job for new output since the last bg_status call \
                (cursor-based — each call advances the read cursor). Pass from_start: true to \
                read all output from the beginning.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id":     { "type": "string",  "description": "Job ID from bg_spawn or bg_attach" },
                    "from_start": { "type": "boolean", "description": "Return all output from byte 0 (default: false)" }
                },
                "required": ["job_id"]
            }),
        },
        ToolDef {
            name: "bg_wait",
            description: "Block until a background job completes or times out. Returns the full \
                stdout and stderr. Ideal for handing off a long task and picking up the result.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id":       { "type": "string",  "description": "Job ID to wait on" },
                    "timeout_secs": { "type": "integer", "description": "Max seconds to wait (default: 3600)" }
                },
                "required": ["job_id"]
            }),
        },
        ToolDef {
            name: "bg_tail",
            description: "Get the last N lines of a job's combined output log (stdout+stderr \
                interleaved). Safe to call at any time — does not advance the bg_status cursor.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string",  "description": "Job ID to read from" },
                    "lines":  { "type": "integer", "description": "Number of lines from the end (default: 50)" }
                },
                "required": ["job_id"]
            }),
        },
        ToolDef {
            name: "bg_list",
            description: "List all background jobs — running, done, and killed. \
                Shows job_id, pid, label, status, elapsed time, output byte counts. \
                Jobs persist across ferrite restarts.",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDef {
            name: "wait_for_pattern",
            description: "Block until a regex pattern appears in a job's output, or timeout. \
                Much more efficient than polling bg_status in a loop. Great for 'wait until \
                server is listening', 'wait until build hits an error', etc. \
                Checks stdout and stderr by default.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id":         { "type": "string",  "description": "Job ID to watch" },
                    "pattern":        { "type": "string",  "description": "Regex pattern to match against output lines" },
                    "timeout_secs":   { "type": "integer", "description": "Max seconds to wait (default: 60)" },
                    "from_cursor":    { "type": "boolean", "description": "Search only output newer than the bg_status cursor (default: false — searches from start)" },
                    "include_stderr": { "type": "boolean", "description": "Also search stderr (default: true)" }
                },
                "required": ["job_id", "pattern"]
            }),
        },
        ToolDef {
            name: "wait_for_idle",
            description: "Block until a job's output has been quiet for idle_secs seconds, \
                or the job ends, or timeout. Useful when a build/process 'pauses' to indicate \
                it's done with a phase — more reliable than a fixed sleep.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id":       { "type": "string",  "description": "Job ID to watch" },
                    "idle_secs":    { "type": "integer", "description": "Seconds of silence before returning (default: 2)" },
                    "timeout_secs": { "type": "integer", "description": "Max total seconds to wait (default: 300)" }
                },
                "required": ["job_id"]
            }),
        },
        ToolDef {
            name: "output_summary",
            description: "Summarise a job's output without returning the full log. \
                Returns: all error lines, all warning lines (capped), the first N lines (head), \
                and the last N lines (tail). Use this instead of bg_wait when the log could be \
                large — prevents context window blowout from verbose build tools or training logs.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id":        { "type": "string",  "description": "Job ID to summarise" },
                    "head_lines":    { "type": "integer", "description": "Lines to include from the start (default: 20)" },
                    "tail_lines":    { "type": "integer", "description": "Lines to include from the end (default: 30)" },
                    "error_pattern": { "type": "string",  "description": "Regex for error lines (default: error|fatal|panic|failed|traceback|exception)" },
                    "warn_pattern":  { "type": "string",  "description": "Regex for warning lines (default: warning|warn|deprecated)" }
                },
                "required": ["job_id"]
            }),
        },
        ToolDef {
            name: "bg_kill",
            description: "Kill a background job. Sends SIGTERM by default; SIGKILL for force-kill.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "Job ID to kill" },
                    "signal": { "type": "string", "description": "\"TERM\" (default) or \"KILL\"", "enum": ["TERM", "KILL"] }
                },
                "required": ["job_id"]
            }),
        },
        ToolDef {
            name: "pipeline_run",
            description: "Run a DAG of shell commands as a pipeline. Steps with no unmet \
                dependencies start immediately (in parallel where possible). Dependent steps \
                wait for their predecessors. Returns a pipeline_id immediately — use \
                pipeline_status to track progress.\n\nExample: build → [test, bench] → deploy",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "description": "Ordered list of pipeline steps",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id":         { "type": "string", "description": "Unique step identifier (e.g. 'build', 'test')" },
                                "cmd":        { "type": "string", "description": "Shell command for this step" },
                                "label":      { "type": "string", "description": "Human-readable label" },
                                "cwd":        { "type": "string", "description": "Working directory override for this step" },
                                "depends_on": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Step IDs that must succeed before this step runs"
                                }
                            },
                            "required": ["id", "cmd"]
                        }
                    },
                    "cwd":             { "type": "string",  "description": "Default working directory for all steps" },
                    "label":           { "type": "string",  "description": "Human-readable pipeline label" },
                    "stop_on_failure": { "type": "boolean", "description": "Cancel remaining steps if any step fails (default: true)" }
                },
                "required": ["steps"]
            }),
        },
        ToolDef {
            name: "pipeline_status",
            description: "Check the status of a running or completed pipeline. Returns per-step \
                status, exit codes, associated job IDs, and an overall summary.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pipeline_id": { "type": "string", "description": "Pipeline ID from pipeline_run" }
                },
                "required": ["pipeline_id"]
            }),
        },
        ToolDef {
            name: "pipeline_cancel",
            description: "Cancel a running pipeline. Kills all currently running step jobs \
                and marks pending steps as skipped.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pipeline_id": { "type": "string", "description": "Pipeline ID to cancel" }
                },
                "required": ["pipeline_id"]
            }),
        },
        ToolDef {
            name: "live_window",
            description: "Open a kitty terminal showing live output from a background job \
                (tail -f on the persistent log file). If no job_id is given, opens a ferrite \
                interactive shell so you can watch ferrite commands in real time.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "Job ID to stream (omit for ferrite shell)" },
                    "title":  { "type": "string", "description": "Window title override" }
                }
            }),
        },
        // ── Project / chip awareness (Tier 1) ────────────────────────────────
        ToolDef {
            name: "project_context",
            description: "Auto-detect workspace type from path. Walks up from path to find \
                known project roots (processor_lab, verilogchill, ferrite*, ferrite-mcp). \
                Returns project_name, project_type, root, context_hints, and active_targets \
                (chips with .bit or workspace crates).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Starting path (default: cwd)" }
                }
            }),
        },
        ToolDef {
            name: "chip_status",
            description: "Scan all chips in processor_lab. For each chip dir under chips/: \
                finds .bit files in build/, checks sim results (results.xml or .log), \
                and parses timing/utilization reports if present. Returns array of \
                [{chip, sim_ok, bit_built, bit_path, wns, lut_pct, last_built}].",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "lab_path": { "type": "string", "description": "Path to processor_lab (default: ~/processor_lab)" }
                }
            }),
        },
        ToolDef {
            name: "chip_build_pipeline",
            description: "Run a full RTL flow for one chip: lint → sim → synth → program → validate. \
                Synth runs as a background job (returns job_id). Other steps run inline. \
                Returns per-step results and overall success.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "chip":     { "type": "string", "description": "Chip name (required, e.g. 'rope_attn')" },
                    "lab_path": { "type": "string", "description": "Path to processor_lab (default: ~/processor_lab)" },
                    "board":    { "type": "string", "description": "Target board (default: basys3)" },
                    "steps": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["lint", "sim", "synth", "program", "validate"] },
                        "description": "Steps to run (default: [lint, sim, synth, program])"
                    },
                    "dry_run":  { "type": "boolean", "description": "Show commands without running (default: false)" }
                },
                "required": ["chip"]
            }),
        },
        ToolDef {
            name: "board_status",
            description: "Detect connected FPGA boards and serial ports. Queries Vivado \
                hw_manager for JTAG targets, scans /dev/ttyUSB* and /dev/ttyACM*. \
                Returns {jtag_boards[], serial_ports[], timestamp}.",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDef {
            name: "fpga_monitor",
            description: "Stream UART output from an FPGA as a background job. Auto-detects \
                /dev/ttyUSBx if port not specified. Returns {job_id, port, baud} — \
                use bg_tail/bg_status to read output, bg_kill to stop.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "port":  { "type": "string", "description": "Serial port (default: auto-detect /dev/ttyUSBx)" },
                    "baud":  { "type": "integer", "description": "Baud rate (default: 921600)" },
                    "label": { "type": "string", "description": "Label for the background job" }
                }
            }),
        },
        // ── Remote SSH tools (Tier 2) ─────────────────────────────────────────
        ToolDef {
            name: "remote_exec",
            description: "Run a command on a remote host via SSH. Uses BatchMode=yes \
                (key-based auth required). Returns {stdout, stderr, exit_code, duration_ms, \
                timed_out, host}.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "host":         { "type": "string", "description": "Remote hostname or IP (required)" },
                    "cmd":          { "type": "string", "description": "Command to run remotely (required)" },
                    "cwd":          { "type": "string", "description": "Remote working directory" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default: 60)" },
                    "env":          { "type": "object", "description": "Extra environment variables (key-value)" }
                },
                "required": ["host", "cmd"]
            }),
        },
        ToolDef {
            name: "remote_build",
            description: "Trigger a build on a remote machine as a background job. \
                Connects via SSH and runs the build command in ~/project. \
                Auto-detects build command from project type (Rust → cargo build --release, etc.). \
                Returns {job_id, host, project, cmd} — track with bg_status/bg_wait.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "host":      { "type": "string", "description": "Remote hostname or IP (required)" },
                    "project":   { "type": "string", "description": "Project directory name under ~/ (required)" },
                    "build_cmd": { "type": "string", "description": "Override build command (auto-detected if omitted)" },
                    "label":     { "type": "string", "description": "Background job label" }
                },
                "required": ["host", "project"]
            }),
        },
        ToolDef {
            name: "sync_project",
            description: "Rsync a project between local and a remote host. Smart excludes \
                (target/, *.bit, .cache/, *.runs/, node_modules/, etc.). \
                Returns {success, files_transferred, bytes_sent, duration_ms, dry_run}.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project":        { "type": "string", "description": "Project directory name under ~/ (required)" },
                    "host":           { "type": "string", "description": "Remote hostname or IP (required)" },
                    "direction":      { "type": "string", "enum": ["push", "pull"], "description": "push (local→remote) or pull (remote→local, default: push)" },
                    "dry_run":        { "type": "boolean", "description": "Show what would be transferred without doing it (default: false)" },
                    "extra_excludes": { "type": "array", "items": { "type": "string" }, "description": "Additional rsync --exclude patterns" }
                },
                "required": ["project", "host"]
            }),
        },
        // ── Project creation ──────────────────────────────────────────────────
        ToolDef {
            name: "project_new",
            description: "Create a new local git project with optional GitHub remote. \
                Initialises a directory, runs git init, scaffolds files by project_type \
                (bare, rust, rust-lib, rust-workspace, python, rtl), makes an initial commit, \
                and optionally creates a GitHub repo via REST API (needs GITHUB_TOKEN env var) \
                and pushes. Returns {root, files_created, ssh_url, next_steps}.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name":         { "type": "string",  "description": "Project / repo name (required)" },
                    "path":         { "type": "string",  "description": "Parent directory (default: ~)" },
                    "project_type": {
                        "type": "string",
                        "enum": ["bare", "rust", "rust-lib", "rust-workspace", "python", "rtl"],
                        "description": "Scaffold template (default: bare)"
                    },
                    "description":  { "type": "string",  "description": "Short description for README / GitHub" },
                    "github":       { "type": "boolean", "description": "Create GitHub repo and push (needs GITHUB_TOKEN, default: false)" },
                    "private":      { "type": "boolean", "description": "Make GitHub repo private (default: false)" }
                },
                "required": ["name"]
            }),
        },
        // ── GitHub SSH tools ──────────────────────────────────────────────────
        ToolDef {
            name: "gh_clone",
            description: "Clone a repo from DaronPopov GitHub via SSH (git@github.com:DaronPopov/<repo>.git). \
                Returns {success, local_path, repo, branch, stdout, stderr}.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo":    { "type": "string", "description": "Repository name (required, e.g. 'processor_lab')" },
                    "dest":    { "type": "string", "description": "Local destination path (default: ~/<repo>)" },
                    "branch":  { "type": "string", "description": "Branch to clone" },
                    "shallow": { "type": "boolean", "description": "Shallow clone --depth 1 (default: false)" }
                },
                "required": ["repo"]
            }),
        },
        ToolDef {
            name: "gh_sync",
            description: "Pull, push, or fetch for a local git repo. Defaults to pull from origin. \
                Returns {success, op, branch, stdout, stderr, fast_forward}.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":   { "type": "string", "description": "Local repo path (default: cwd)" },
                    "op":     { "type": "string", "enum": ["pull", "push", "fetch"], "description": "Operation (default: pull)" },
                    "branch": { "type": "string", "description": "Branch name (default: current branch)" },
                    "remote": { "type": "string", "description": "Remote name (default: origin)" }
                }
            }),
        },
        ToolDef {
            name: "gh_status",
            description: "Git status across all known project roots. Scans ~/processor_lab, \
                ~/verilogchill, ~/ferrite-os-clean, ~/cpp_importable_test, ~/rust_shell, \
                ~/aws_tool (or custom paths). Returns [{path, project, branch, ahead, behind, \
                dirty, last_commit}] for each git repo found.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Override scan list (default: well-known project roots)"
                    }
                }
            }),
        },
        // ── git write ─────────────────────────────────────────────────────────
        ToolDef {
            name: "git_checkpoint",
            description: "Create a structured git checkpoint in one call: optional stage + commit + push, with before/after repo state and commit provenance trailers.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path":    { "type": "string",  "description": "Repo root path (default: cwd)" },
                    "stage":   { "type": "boolean", "description": "Run git add before commit (default: true)" },
                    "add": {
                        "description": "Files to stage: \"all\", \"tracked\" (default), \"none\", a path string, or array of paths",
                        "oneOf": [
                            { "type": "string" },
                            { "type": "array", "items": { "type": "string" } }
                        ]
                    },
                    "commit":      { "type": "boolean", "description": "Create a commit from staged changes (default: true)" },
                    "message":     { "type": "string",  "description": "Commit message (default: generated checkpoint summary)" },
                    "allow_empty": { "type": "boolean", "description": "Allow empty commits (default: false)" },
                    "author":      { "type": "string",  "description": "Author override \"Name <email>\"" },
                    "push":        { "type": "boolean", "description": "Push after commit/status checkpoint (default: false)" },
                    "remote":      { "type": "string",  "description": "Remote name (default: origin)" },
                    "branch":      { "type": "string",  "description": "Branch to push (default: current)" },
                    "provenance": {
                        "type": "object",
                        "description": "Optional checkpoint provenance fields appended as commit trailers",
                        "properties": {
                            "agent":      { "type": "string", "description": "Agent name/id" },
                            "session_id": { "type": "string", "description": "Session identifier" },
                            "reason":     { "type": "string", "description": "Reason for this checkpoint" },
                            "tags":       { "type": "array", "items": { "type": "string" }, "description": "Short labels for later filtering" }
                        }
                    }
                }
            }),
        },
        ToolDef {
            name: "git_commit",
            description: "Stage files and commit in a git repo. Optionally push after commit. \
                'add' can be \"all\" (git add -A), \"tracked\" (git add -u, default), or an array \
                of specific file paths. Returns {committed, hash, staged_stat, push}.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string",  "description": "Commit message (required)" },
                    "path":    { "type": "string",  "description": "Repo root path (default: cwd)" },
                    "add": {
                        "description": "Files to stage: \"all\", \"tracked\" (default), a path string, or array of paths",
                        "oneOf": [
                            { "type": "string" },
                            { "type": "array", "items": { "type": "string" } }
                        ]
                    },
                    "push":   { "type": "boolean", "description": "Push after commit (default: false)" },
                    "remote": { "type": "string",  "description": "Remote name (default: origin)" },
                    "branch": { "type": "string",  "description": "Branch to push (default: current)" },
                    "author": { "type": "string",  "description": "Author override \"Name <email>\"" }
                },
                "required": ["message"]
            }),
        },
        // ── notifications ─────────────────────────────────────────────────────
        ToolDef {
            name: "notify",
            description: "Send a notification to the desktop (notify-send) and/or phone \
                (ntfy.sh push). For phone alerts: set NTFY_TOPIC env var to your ntfy topic \
                (free at ntfy.sh — subscribe via the ntfy app on your phone). \
                Returns {sent, desktop, phone}.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message":  { "type": "string", "description": "Notification body (required)" },
                    "title":    { "type": "string", "description": "Title (default: ferrite)" },
                    "topic":    { "type": "string", "description": "ntfy.sh topic override (default: $NTFY_TOPIC)" },
                    "urgency":  { "type": "string", "enum": ["low","normal","critical"], "description": "Desktop urgency (default: normal)" },
                    "priority": { "type": "string", "enum": ["min","low","default","high","urgent"], "description": "ntfy.sh priority (default: default)" },
                    "tags":     { "type": "array", "items": { "type": "string" }, "description": "ntfy.sh emoji tags e.g. [\"white_check_mark\"]" },
                    "icon":     { "type": "string", "description": "Desktop icon name (default: dialog-information)" },
                    "desktop":  { "type": "boolean", "description": "Send desktop notification (default: true)" },
                    "phone":    { "type": "boolean", "description": "Send phone notification (default: true if topic is set)" }
                },
                "required": ["message"]
            }),
        },
        // ── tmux ──────────────────────────────────────────────────────────────
        ToolDef {
            name: "tmux_ctl",
            description: "Create, query, and control tmux sessions. Use this to run long \
                commands that must survive connection drops (especially useful during remote \
                phone sessions). ops: new, list, send, capture, kill, has.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["new","list","send","capture","kill","has"],
                        "description": "Operation (required)"
                    },
                    "session": { "type": "string", "description": "Session name (default: main)" },
                    "window":  { "type": "string", "description": "Window name or index (optional)" },
                    "cmd":     { "type": "string", "description": "Command string — for op:new (start cmd) or op:send (keys to send)" },
                    "cwd":     { "type": "string", "description": "Working directory for new session (default: ~)" },
                    "enter":   { "type": "boolean", "description": "Append Enter after cmd in op:send (default: true)" },
                    "lines":   { "type": "integer", "description": "Lines of scrollback to capture in op:capture (default: 50)" }
                },
                "required": ["op"]
            }),
        },
        // ── network ───────────────────────────────────────────────────────────
        ToolDef {
            name: "tailscale_status",
            description: "Check Tailscale VPN status. Returns this machine's Tailscale IP, \
                name, whether it's reachable, and the list of online/offline peers. \
                Useful to confirm the PC is reachable from the phone before a remote session.",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        // ── session ───────────────────────────────────────────────────────────
        ToolDef {
            name: "session_status",
            description: "Report the state of the auto-started Claude remote session. \
                Shows: tmux session alive, saved remote-control URL and its age, \
                systemd unit state, ntfy config, and last 10 lines of the autostart log. \
                Call this first thing in any phone session to confirm everything is healthy.",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDef {
            name: "session_restart",
            description: "Kill the current Claude tmux session so the ferrite watchdog spawns a \
                fresh one — new session ID, new remote-control URL, new ntfy notification sent to your phone. \
                Use this when you want to hand off to a clean Claude session. \
                The current session will die ~1 s after the tool is called; \
                a new session will appear within ~30 s.",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDef {
            name: "session_handoff",
            description: "Migrate the current Claude conversation to a new tmux session so you can \
                continue from any terminal (local or remote SSH) without losing context. \
                Creates a detached tmux session running `claude --continue`, then kills this \
                session after a short delay. Returns the exact command to paste in the new terminal.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session_name": {
                        "type": "string",
                        "description": "Name for the new tmux session (default: 'claude-remote')"
                    },
                    "delay_secs": {
                        "type": "integer",
                        "description": "Seconds before killing the current session (default: 3, 0 = don't kill)"
                    },
                    "ssh_host": {
                        "type": "string",
                        "description": "Optional SSH hostname — if set, connect_cmd will include the ssh prefix"
                    }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "mobile_session",
            description: "Trigger the '/mobile-session' flow for Codex mobile attach. \
                Ensures the codex-remote-auth sidecar is up (optional auto-start), then requests \
                a magic-link login email and OTP flow for phone entry.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "email": {
                        "type": "string",
                        "description": "Email address to receive the sign-in link (defaults to allowed_email in config)"
                    },
                    "api_base": {
                        "type": "string",
                        "description": "Auth sidecar base URL (default: http://127.0.0.1:8787)"
                    },
                    "config_path": {
                        "type": "string",
                        "description": "Path to codex_remote_auth.toml (default: ~/.config/ferrite/codex_remote_auth.toml)"
                    },
                    "start_if_down": {
                        "type": "boolean",
                        "description": "Start sidecar automatically when health check fails (default: true)"
                    },
                    "start_cmd": {
                        "type": "string",
                        "description": "Override sidecar startup command"
                    }
                }
            }),
        },

        // ── Dynamic tool registration ─────────────────────────────────────────
        ToolDef {
            name: "tool_define",
            description: "Register a new tool at runtime without restarting. \
                The tool becomes immediately callable after this returns. \
                Sends notifications/tools/list_changed so the client re-fetches the tool list. \
                Tools are persisted to disk and reloaded on next startup.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name":        { "type": "string",  "description": "Tool name (no spaces, used as the call identifier)" },
                    "description": { "type": "string",  "description": "What this tool does — shown to the model" },
                    "params": {
                        "type": "object",
                        "description": "JSON Schema object for the tool's parameters (use {} for no params)",
                        "default": {}
                    },
                    "command":     { "type": "string",  "description": "Shell command. Use {param_name} placeholders; args also in $FERRITE_ARGS (JSON) and on stdin." },
                    "timeout_secs":{ "type": "integer", "description": "Execution timeout in seconds (default: 60)" }
                },
                "required": ["name", "description", "command"]
            }),
        },
        ToolDef {
            name: "tool_undefine",
            description: "Remove a previously registered dynamic tool. \
                Sends notifications/tools/list_changed so the client drops it from the list.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the tool to remove" }
                },
                "required": ["name"]
            }),
        },
        ToolDef {
            name: "tool_list_dynamic",
            description: "List all currently registered dynamic tools with their names, descriptions, and commands.",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },

        // ── Permissions / pre-validation ──────────────────────────────────────
        ToolDef {
            name: "pre_validate",
            description: "Analyze a shell command for interactive blockers (sudo password prompts, \
                           y/n confirmations, interactive TUI installers) before running it. \
                           Returns the auto-rewritten non-interactive form, injected env vars, \
                           and any unresolved blockers. Use this before exec for privileged or \
                           installer-style commands. exec also applies rewrites automatically, \
                           so this is mainly for transparency / go/no-go decisions.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cmd": {
                        "type": "string",
                        "description": "The shell command to analyze (same syntax as exec's 'cmd')"
                    }
                },
                "required": ["cmd"]
            }),
        },
        ToolDef {
            name: "permissions_setup",
            description: "Report the current sudoers pre-authorization status for ferrite. \
                           Shows whether /etc/sudoers.d/ferrite is installed (granting NOPASSWD \
                           for apt, systemctl, ufw, snap, chmod, etc.) and how to install it \
                           if missing. Pass show_entry=true to see the exact sudoers content. \
                           Pass install=true to attempt self-installation without a TTY — tries \
                           a cached sudo credential first, then falls back to a graphical Polkit \
                           (pkexec) dialog.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "show_entry": {
                        "type": "boolean",
                        "description": "Include the full sudoers entry text in the response (default false)"
                    },
                    "install": {
                        "type": "boolean",
                        "description": "Attempt to self-install /etc/sudoers.d/ferrite. Tries cached sudo \
                                        credential, then pkexec graphical prompt. Returns install_result: \
                                        'installed', 'already_active', or 'failed: <reason>' (default false)"
                    }
                }
            }),
        },

        // ── PTY / interactive program driver ──────────────────────────────────
        ToolDef {
            name: "tty_exec",
            description: "Execute a command in a PTY and automatically respond to interactive prompts. \
                           Handles programs that open /dev/tty directly (custom installers, license dialogs, \
                           y/n loops) that cannot be silenced by env vars alone. \
                           default_yes=true (the default) pre-loads a response table covering the most \
                           common confirmations. Supply a 'responses' map for custom prompts. \
                           exec applies non-interactive rewrites automatically — use tty_exec only \
                           when the program genuinely requires a TTY.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cmd": {
                        "type": "string",
                        "description": "Shell command to run in a PTY"
                    },
                    "responses": {
                        "type": "object",
                        "description": "Map of {prompt_substring → answer_to_send}. Keys matched case-insensitively. \
                                        Example: {\"overwrite?\": \"y\\n\", \"licence agreement\": \"yes\\n\"}"
                    },
                    "default_yes": {
                        "type": "boolean",
                        "description": "Pre-load built-in table of affirmative responses for common prompts (default true)"
                    },
                    "timeout_secs": {
                        "type": "number",
                        "description": "Kill command after this many seconds (default 120)"
                    },
                    "idle_done_secs": {
                        "type": "number",
                        "description": "Treat N seconds of silence as completion even without EOF (default 3)"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory (default: ferrite cwd)"
                    }
                },
                "required": ["cmd"]
            }),
        },

        // ── Environment pre-flight ─────────────────────────────────────────────
        ToolDef {
            name: "env_doctor",
            description: "Pre-flight environment check before complex workflows. Reports: \
                           which tools are in PATH (and their versions), missing binaries with install hints, \
                           free disk space on / and $HOME, network reachability to pypi/crates.io/npmjs/github, \
                           write access to key directories, and sudoers status. \
                           Call this at the start of any multi-step setup task to surface blockers before they bite.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "check_bins": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Additional binaries to check beyond the default list"
                    },
                    "check_network": {
                        "type": "boolean",
                        "description": "Probe package registries for reachability (default true; adds ~5s)"
                    },
                    "check_disk": {
                        "type": "boolean",
                        "description": "Check free disk space on / and $HOME (default true)"
                    },
                    "check_versions": {
                        "type": "boolean",
                        "description": "Query --version for each found binary (default true)"
                    }
                }
            }),
        },
    ]
}
