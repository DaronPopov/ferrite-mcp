# cream

A machine intelligence layer for Claude Code. Built in Rust.

cream is an MCP (Model Context Protocol) server that gives Claude Code direct, pre-authorized access to your development machine — without permission prompts, without parsing raw shell output, without guessing at library paths or architecture flags. It sits between Claude and the system and answers questions that would otherwise require a terminal.

---

## The problem it solves

When Claude Code operates without machine context, it produces statistically average code. It guesses CUDA arch flags. It hardcodes library paths. It re-reads 2000-line files to find one error location. It has no memory between sessions. Every verification costs tokens and interrupts the user for permission.

cream inverts this. Before writing a line of code, Claude queries the machine. It finds the exact arch flags for your GPU, the real include paths for your libraries, the actual occupancy ceiling for your kernel parameters. Build errors arrive as structured `{file, line, col}` objects. The session starts with a single `orient` call instead of four separate queries. Notes survive across tool calls without crowding the context window.

The machine becomes the source of truth. Not documentation, not priors, not user confirmation dialogs.

---

## What it is

cream runs as a background MCP stdio server. Projects that register it give Claude access to **89 tools** across:

- **Situational awareness** — single-call session orientation
- **Hardware topology** — GPU/CPU architecture, SIMD capabilities, live utilization
- **Library discovery** — pkg-config, ldconfig, environment, filesystem scan
- **Execution** — shell commands, compilation, binary inspection, all without permission prompts
- **Background processes** — spawn, attach, stream, pipeline with DAG execution
- **Code navigation** — contextual reads, regex search, symbol indexing, git state
- **Git & GitHub** — structured diffs, commits, clone, push, cross-repo status
- **GPU profiling** — PTX inspection, Nsight Compute, compute-sanitizer
- **CPU profiling** — Linux perf flamegraphs, hardware counter stats
- **Debugging** — GDB batch mode with structured backtrace/locals output
- **ML data** — PyTorch checkpoint and tensor inspection without a REPL
- **EDA** — Verilog lint/sim, cocotb, Vivado Tcl, FPGA programming, waveform parsing
- **Remote development** — SSH exec, remote build, rsync project sync
- **Session management** — boot-time Claude launcher with Gmail/ntfy notifications, watchdog restart
- **System introspection** — process tree, open ports, systemd journal
- **Persistence** — benchmark history across sessions, in-session notes

cream never writes files. All file editing stays in Claude Code's Edit and Write tools.

---

## Install

```bash
# Build and install from workspace root
cargo install --path crates/shell-bin

# Register with Claude Code (project-scoped)
# .mcp.json is already at the workspace root — nothing to do

# Or register globally across all projects
claude mcp add --transport stdio cream -- ~/.cargo/bin/cream --mcp
```

The `.mcp.json` at the project root:

```json
{
  "mcpServers": {
    "cream": {
      "type": "stdio",
      "command": "${HOME}/.cargo/bin/cream",
      "args": ["--mcp"]
    }
  }
}
```

---

## Modes

```bash
cream --mcp       # MCP stdio server (used by Claude Code)
cream             # Interactive REPL shell
cream -c <cmd>    # Run a single command
```

---

## Remote session autostart

cream ships a boot-time launcher that starts Claude Code in a tmux session on login, enables remote control, captures the session URL, and emails it to you — so you can connect from your phone or any device without touching the PC.

### Setup

```bash
# Gmail config (app password required — not your account password)
cream-sendmail --setup

# Optional: ntfy.sh phone push
echo "NTFY_TOPIC=your-uuid-topic" >> ~/.config/cream/env

# Install the systemd user service
systemctl --user enable cream-session.service
systemctl --user start cream-session.service
```

### Flow on every boot

1. systemd starts `cream-autostart` after the graphical session
2. Kills any stale tmux session with the same name
3. Starts `claude --dangerously-skip-permissions --session-id <fresh-uuid>` in a detached tmux pane
4. Waits for Claude's interactive prompt
5. Sends `/remote-control` → captures the `claude.ai` session URL
6. Saves URL to `~/.local/share/cream/remote-session-url.txt`
7. Sends desktop notification + Gmail email + ntfy.sh push (whichever are configured)
8. Stays running as a watchdog — restarts the session if it dies

The `--session-id <fresh-uuid>` flag forces a brand-new session on every boot rather than resuming the previous conversation.

### Session restart from within Claude

The `session_restart` MCP tool lets Claude kill its own session so the watchdog spins up a clean one and sends a new email:

```
cream:session_restart()
→ { status: "killing", note: "new session + email in ~30s" }
```

### Check session health

```
cream:session_status()
→ {
    status: "ready",
    tmux_alive: true,
    remote_url: "https://claude.ai/code/session_...",
    url_age_secs: 142,
    systemd_state: "active",
    ntfy_configured: true,
    autostart_log: "... boot sequence complete ✓ ..."
  }
```

---

## Workflow examples

### Start every session with a single orient call

```
cream:orient()
→ {
    cwd: "/home/daron/project",
    git: { branch: "main", staged: [], unstaged: ["src/lib.rs"], untracked: [] },
    recent: [{ path: "src/lib.rs", modified: "3 minutes ago" }],
    tree: { "src/": [...], "Cargo.toml": ... },
    ports: [{ proto: "tcp", port: 8080, process: "node" }]
  }
```

Replaces calling git_status + changed_since + list_dir + port_list separately. One call, full picture.

---

### Hardware-tuned code from the start

```
cream:gpu_info()
→ RTX 3070, sm_86, 46 SMs, 48KB smem/block, 7.7GB VRAM, 448 GB/s

cream:occupancy_calc(threads_per_block=256, shared_mem_bytes=2048)
→ occupancy: 100%, limiting: registers, blocks_per_sm: 6

cream:discover("cuda")
→ CUDA 12.6 at /usr/local/cuda-12.6, nvcc 12.6 in PATH

cream:find_lib("cublas")
→ path: /usr/local/cuda-12.6/lib64/libcublas.so
  link_flags: -lcublas -L/usr/local/cuda-12.6/lib64
```

Arch flags, occupancy, library paths — all from the live machine. The kernel is written to what it will actually run on.

---

### Compile and iterate without leaving the conversation

```
cream:build_check("bench.cu", type="cuda")
→ errors: [{ file: "bench.cu", line: 42, col: 5, message: "use of undeclared identifier 'blockDim'" }]

cream:read_context("bench.cu", line=42, radius=10)
→ 20 lines centered on the error site

cream:exec("nvcc -O3 -arch=sm_86 bench.cu -o bench")
→ { exit_code: 0, duration_ms: 751 }

cream:exec("./bench")
→ saxpy: 396.7 GB/s
  matmul_tile16: 1721.5 GFLOP/s
```

No terminal switching. No permission prompts. Build errors are structured data, not stderr to parse.

---

### Background processes and pipelines

```
# Spawn a long build in the background
cream:bg_spawn(cmd="cargo build --release", label="release build")
→ { job_id: "j_abc123", pid: 9842 }

# Stream output as it arrives
cream:bg_status(job_id="j_abc123")
→ { running: true, new_output: "Compiling shell-mcp v0.3.0..." }

# Wait for a specific event
cream:wait_for_pattern(job_id="j_abc123", pattern="Finished|error")
→ { matched: "Finished release target", line: 142 }

# Run a DAG pipeline — build → test and bench in parallel → deploy
cream:pipeline_run(steps=[
  { id: "build", cmd: "cargo build --release" },
  { id: "test",  cmd: "cargo test",  depends_on: ["build"] },
  { id: "bench", cmd: "./bench",     depends_on: ["build"] },
  { id: "deploy",cmd: "./deploy.sh", depends_on: ["test", "bench"] }
])
→ { pipeline_id: "p_xyz", status: "running" }

# Interact with a PTY process (Vivado, GDB, Python REPL)
cream:bg_spawn(cmd="python3", pty=true)
→ { job_id: "j_py1" }
cream:bg_send(job_id="j_py1", text="import torch; print(torch.__version__)")
cream:bg_status(job_id="j_py1")
→ { new_output: "2.3.0+cu121" }
```

---

### Remote development

```
# Run a command on a remote build machine
cream:remote_exec(host="build-server", cmd="nvidia-smi")
→ { stdout: "Driver: 535.104 | CUDA 12.2 | RTX 4090", exit_code: 0 }

# Push project to remote and trigger build
cream:sync_project(project="myproject", host="build-server")
→ { files_transferred: 12, bytes_sent: 84210, duration_ms: 340 }

cream:remote_build(host="build-server", project="myproject")
→ { job_id: "j_rbld1" }   # background — track with bg_status
```

---

### Git write and GitHub

```
cream:git_commit(message="fix: session duplicate launch on boot", add="all", push=true)
→ { committed: true, hash: "a3f1b2c", push: { success: true } }

cream:gh_clone(repo="processor_lab")
→ { success: true, local_path: "/home/daron/processor_lab" }

cream:gh_status()
→ [
    { project: "creamMCP",      branch: "main", ahead: 0, dirty: false },
    { project: "processor_lab", branch: "main", ahead: 2, dirty: true  }
  ]
```

---

### Verify GPU state before benchmarking

```
cream:gpu_live()
→ util: 0%, temp: 53°C, power: 15W/220W, sm_clock: 210MHz, ready_to_bench: true
```

If the GPU is warm, busy, or power-throttled, benchmark results are unreliable. Check before launching.

---

### Inspect what the compiler generated

```
cream:ptx_inspect("bench.cu", kernel="matmul")
→ matmul_tile16: regs/thread=140, smem=2048B, ld.shared=32, st.shared=2, sync=2

cream:occupancy_calc(threads_per_block=256, shared_mem_bytes=2048, registers_per_thread=140)
→ occupancy: 66%, limiting: registers — tile_32 hurts, use tile_16
```

PTX inspection gives the real register count the compiler assigned. Feed that back into occupancy_calc for the actual ceiling, not a theoretical one.

---

### Profile and diagnose

```
cream:ncu_profile("./bench", kernel="saxpy")
→ achieved_occupancy_pct: 100
  dram_throughput_pct: 88
  l1_hit_rate_pct: 12
  stall_long_sb_pct: 3

cream:compute_sanitizer("./bench", tool="memcheck")
→ clean: true, error_count: 0

cream:perf_stat("./server --bench")
→ cycles: 4_821_003_442, instructions: 9_104_882_021, IPC: 1.89
  cache-misses: 2_104_221 (0.23%), branch-misses: 812_003

cream:flamegraph("./server --bench")
→ hotspots: [{ pct: 34.1, symbol: "json_parse" }, { pct: 18.7, symbol: "malloc" }]
  svg_path: /tmp/cream_flamegraph.svg
```

---

### EDA: Verilog, cocotb, Vivado, FPGA

```
cream:verilog_lint(files=["alu.v", "tb_alu.v"])
→ [{ severity: "error", file: "alu.v", line: 34, message: "port width mismatch" }]

cream:verilog_sim(files=["alu.v", "tb_alu.v"], top="tb_alu", vcd_out="/tmp/alu.vcd")
→ { success: true, finished: true, assertion_failures: 0 }

cream:cocotb_run("./tests/alu", simulator="icarus")
→ { passed: 5, failed: 0 }

cream:vivado_tcl(cmd="open_project proj.xpr; synth_design -top alu")
→ { success: true, errors: [], warnings: ["timing not met on path ..."] }

cream:synth_report(project_dir="./chips/rope_attn")
→ { wns: 0.412, tns: 0.0, failing_endpoints: 0, lut_pct: 38.2, ff_pct: 14.1, bram: 2, dsp: 0 }

cream:chip_build_pipeline(chip="rope_attn", steps=["lint","sim","synth","program"])
→ { lint: "pass", sim: "5/5 pass", synth: { job_id: "j_syn1" }, program: "pending" }

cream:fpga_boards()
→ [{ target: "xc7a35t_0", device: "basys3", part: "xc7a35tcpg236-1", status: "open" }]

cream:fpga_program(bitfile="./build/alu.bit")
→ { success: true, duration_ms: 3200 }
```

Vivado 2025.2 at `/opt/2025.2/Vivado/bin/vivado`. Uses Tcl batch mode, no GUI.

---

### Persist results and notes across sessions

```
cream:bench_history("record", tag="saxpy", value=396.7, unit="GB/s")
cream:bench_history("record", tag="saxpy", value=412.1, unit="GB/s")
cream:bench_history("compare", tag="saxpy")
→ previous: 396.7 GB/s → latest: 412.1 GB/s → +3.9% faster

cream:note("append", content="kernel register pressure is the bottleneck, not smem")
cream:note("read")
→ ["kernel register pressure is the bottleneck, not smem"]
```

bench_history is stored at `~/.local/share/cream/bench_history.jsonl` and survives restarts. notes live in server memory for the current session.

---

## Tool reference

### Workspace
| Tool | What it does |
|------|-------------|
| `orient` | Single-call session start: git state + recent changes + dir tree + ports |
| `note` | Session scratchpad — read / append / clear notes in server memory |
| `shell_state` | cream's cwd, PATH, dev-relevant environment variables |
| `set_cwd` | Change cream's working directory (persists across calls) |

### Hardware
| Tool | What it does |
|------|-------------|
| `gpu_info` | Compute cap, SM count, VRAM, warp size, shared mem, peak bandwidth. Cached. |
| `gpu_live` | Live util %, temperature, power, clocks, VRAM. `ready_to_bench` flag. |
| `cpu_info` | Model, cores, SIMD flags (AVX2/AVX-512/NEON/SVE), L1/L2/L3 cache sizes |
| `occupancy_calc` | Theoretical occupancy for given threads/smem/regs against live GPU arch |

### Discovery
| Tool | What it does |
|------|-------------|
| `find_lib` | Path, version, include dirs, link flags for any system library |
| `discover` | Full scan of cuda / rocm / ml / build-tools toolchains |
| `which` | Binary path + version probe |

### Execution
| Tool | What it does |
|------|-------------|
| `exec` | Run any shell command. stdout, stderr, exit_code, duration_ms. No prompts. |
| `build_check` | Structured errors `{file, line, col, message}` for CUDA / Rust / C / C++ |
| `task_run` | Write + execute a Python/bash script atomically. Multi-step experiments in one call. |
| `launch` | Fire-and-forget detached process (GUI apps, daemons). Returns PID immediately. |

### Background processes
| Tool | What it does |
|------|-------------|
| `bg_spawn` | Spawn a command in the background. Returns `job_id` immediately. PTY support for interactive programs. |
| `bg_attach` | Attach to an existing process by PID for lifecycle tracking |
| `bg_send` | Send stdin to a PTY job (Vivado, GDB, Python REPL, etc.) |
| `bg_status` | Poll new output since last call (cursor-based) |
| `bg_tail` | Last N lines of a job's output without advancing the cursor |
| `bg_wait` | Block until a job completes. Returns full output. |
| `bg_list` | All background jobs — running, done, killed |
| `bg_kill` | Kill a background job (SIGTERM or SIGKILL) |
| `wait_for_pattern` | Block until a regex matches in job output — more efficient than polling |
| `wait_for_idle` | Block until job output goes quiet for N seconds |
| `output_summary` | Errors + warnings + head + tail of a job's log without blowing the context window |
| `live_window` | Open a kitty terminal window streaming a job's output live |

### Pipelines
| Tool | What it does |
|------|-------------|
| `pipeline_run` | Run a DAG of shell commands. Steps with no unmet deps start immediately (parallel). |
| `pipeline_status` | Per-step status, exit codes, job IDs for a running/completed pipeline |
| `pipeline_cancel` | Cancel a running pipeline. Kills running steps, skips pending. |

### Code navigation
| Tool | What it does |
|------|-------------|
| `read_file` | File contents with pagination. filter= for line-level grep. |
| `read_context` | Window of N lines around a specific line number |
| `list_dir` | Directory tree with type/size/mtime, depth 1–5 |
| `glob` | Files matching a glob pattern |
| `grep_code` | Regex search with context lines, ripgrep-accelerated |
| `changed_since` | Files modified after a timestamp or relative duration |

### Symbols & Git (read)
| Tool | What it does |
|------|-------------|
| `symbol_index` | Index all Rust symbols (fn/struct/enum/trait/impl/…) across source files |
| `find_symbol` | Find a Rust symbol by name with file + line |
| `git_status` | Branch, ahead/behind, staged/unstaged/untracked — structured |
| `git_log` | Commit history with author, date, subject. Filter by author/file/since. |
| `git_diff` | Unified diff as structured `{path, additions, deletions, hunks}` |

### Git write & GitHub
| Tool | What it does |
|------|-------------|
| `git_commit` | Stage files and commit. Optionally push. Returns hash + push result. |
| `gh_clone` | Clone a repo from DaronPopov GitHub via SSH |
| `gh_sync` | Pull, push, or fetch for a local repo |
| `gh_status` | Git state across all known project roots in one call |
| `project_new` | Create a local git project with optional GitHub remote. Scaffolds bare/rust/python/rtl. |

### Filesystem operations
| Tool | What it does |
|------|-------------|
| `move_file` | Move/rename with optional reference scan for Rust mod/use |
| `mkdir` | Create directory with intermediate parents |
| `delete_file` | Delete a single file (refuses directories) |

### GPU profiling
| Tool | What it does |
|------|-------------|
| `ptx_inspect` | Per-kernel: register counts, smem bytes, global/shared op counts, ptxas -v stats |
| `ncu_profile` | Per-kernel: occupancy %, SM/DRAM throughput %, L1/L2 hit rates, stall reasons |
| `compute_sanitizer` | Memory errors with kernel, thread coordinates, file, line |

### CPU profiling
| Tool | What it does |
|------|-------------|
| `flamegraph` | perf record → hotspot list + optional SVG (inferno or flamegraph.pl) |
| `perf_stat` | Hardware counters: cycles, instructions, IPC, cache-misses, branch-misses |

### Debugging
| Tool | What it does |
|------|-------------|
| `gdb_run` | GDB batch mode: backtrace, locals, signal info. Live run or core dump. |

### ML
| Tool | What it does |
|------|-------------|
| `tensor_inspect` | PyTorch .pt/.pth — tensor shapes, dtypes, min/max/mean/std |
| `checkpoint_list` | Enumerate .pt/.pth/.ckpt/.bin/.safetensors with size, mtime, keys, epoch/step |

### EDA
| Tool | What it does |
|------|-------------|
| `verilog_lint` | iverilog -tnull → structured `{severity, file, line, message}` |
| `verilog_sim` | iverilog + vvp simulation → success, assertions, optional VCD |
| `cocotb_run` | cocotb 2.x pytest runner → `{passed, failed}` |
| `vivado_tcl` | Vivado 2025.2 batch Tcl — inline cmd or script file |
| `synth_report` | Parse timing_summary.rpt + utilization.rpt → WNS/TNS/LUT%/FF%/BRAM/DSP |
| `fpga_boards` | List JTAG-connected FPGA targets via hw_manager |
| `fpga_program` | Program a .bit bitstream via Vivado hw_manager |
| `board_status` | Detect connected FPGA boards + serial ports in one call |
| `fpga_monitor` | Stream UART output from FPGA as a background job |
| `fpga_serial` | Low-level UART send/receive (hex bytes) |
| `fpga_tcfp_status` | Read TCFP processor status (busy/converged/done/step_count) via UART |
| `fpga_tcfp_tile_read` | Read tile state vectors from TCFP array via UART |
| `waveform_query` | Parse VCD waveform — signal definitions and value-change timeline |
| `project_context` | Auto-detect workspace type, active chips, build targets |
| `chip_status` | Scan all chips: sim results, .bit built, timing WNS, LUT% |
| `chip_build_pipeline` | Full RTL flow for one chip: lint → sim → synth → program → validate |

### System
| Tool | What it does |
|------|-------------|
| `process_tree` | Running processes from /proc: pid, name, state, mem, cmdline |
| `port_list` | Listening TCP/UDP ports via ss: proto, port, pid, process |
| `journal_query` | systemd journal entries with unit, time, grep filter |

### Network & notifications
| Tool | What it does |
|------|-------------|
| `tailscale_status` | This machine's Tailscale IP + online/offline peer list |
| `http_request` | Send HTTP/HTTPS requests via curl. Returns `{status, latency_ms, headers, body}`. |
| `notify` | Desktop notification (notify-send) + phone push (ntfy.sh) |

### Remote development
| Tool | What it does |
|------|-------------|
| `remote_exec` | Run a command on a remote host via SSH (key auth). Returns stdout/stderr/exit_code. |
| `remote_build` | Trigger a build on a remote machine as a background job |
| `sync_project` | Rsync a project between local and remote. Smart excludes. Push or pull. |

### tmux
| Tool | What it does |
|------|-------------|
| `tmux_ctl` | Create, query, send, capture, kill tmux sessions. Keeps long commands alive across SSH drops. |

### Session
| Tool | What it does |
|------|-------------|
| `session_status` | Health check: tmux alive, remote URL + age, systemd state, autostart log tail |
| `session_restart` | Kill the current Claude session — watchdog spawns a fresh one + sends new email within ~30s |

### Rust tooling
| Tool | What it does |
|------|-------------|
| `cargo_tree` | Workspace members, versions, features, dependency graph |
| `test_run` | cargo test → structured `{passed, failed, ignored}` with names |
| `inspect_binary` | ldd dependencies, nm -D symbols, readelf ELF header |

### History
| Tool | What it does |
|------|-------------|
| `bench_history` | record / list / query / compare benchmark results. Persisted to disk. |

---

## Response filtering

Every tool accepts two cross-cutting parameters:

```
filter="keyword"    Keep only output lines containing this string (case-insensitive)
max_chars=N         Hard cap on response length. 0 = unlimited (default).
```

Useful when a tool returns more than you need:

```
cream:exec("cargo test 2>&1", filter="FAILED")
→ only the failing test lines

cream:grep_code("TODO", max_chars=2000)
→ first 2000 chars of matches
```

---

## Architecture

cream is a Cargo workspace with 8 crates:

```
crates/
├── shell-core      — foundation types, no OS calls
├── shell-lexer     — byte-level tokenizer
├── shell-parser    — recursive descent AST
├── shell-runtime   — executor, builtins, job control, signals
├── shell-hooks     — LLM integration seam (Hook trait + HookRegistry)
├── shell-tui       — readline, prompt, history, completion, cream theme
├── shell-mcp       — MCP stdio server (89 tools)
└── shell-bin       — entry point: cream / cream -c / cream --mcp
```

The MCP server (`shell-mcp`) is independent of the shell runtime. It speaks JSON-RPC 2.0 over stdin/stdout and can be embedded in any project without any shell functionality.

The `shell-hooks` crate is the LLM integration seam. Every command passes through `HookRegistry::dispatch(HookEvent)` before and after execution. Future integration — command suggestion, error explanation, completion — plugs in here without touching the shell core.

Background job state persists to `~/.local/share/cream/logs/` and `~/.local/share/cream/session.json` across cream restarts, so jobs survive MCP server restarts.

---

## Design principles

**cream executes. Claude reasons.**
cream is a pure execution surface. It faithfully runs what it is asked and returns structured data. It does not decide, suggest, or filter based on intent. That reasoning stays with Claude — which is where creativity lives.

**Verify reality, don't assume.**
Every path, version, and flag comes from what cream found on this machine. Not from documentation. Not from priors.

**Return exactly what the next decision needs.**
No prose, no padding, no explanatory text. Structured JSON that maps directly to code parameters and decisions.

**The machine is the source of truth.**
Hardware topology, library locations, compiler capabilities — queried once at the start, used throughout. Code shaped to the actual environment, not a generic target.

**Token efficiency compounds.**
`read_context` after a build error: 20 lines instead of 2000. `build_check` vs raw stderr: structured `{file,line,col}` vs text to parse. `orient` vs four separate queries: one call. Small savings per operation add up to qualitatively different sessions.
