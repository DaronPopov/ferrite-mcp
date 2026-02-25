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

cream runs as a background MCP stdio server. Projects that register it give Claude access to **53 tools** across:

- **Situational awareness** — single-call session orientation
- **Hardware topology** — GPU/CPU architecture, SIMD capabilities, live utilization
- **Library discovery** — pkg-config, ldconfig, environment, filesystem scan
- **Execution** — shell commands, compilation, binary inspection, all without permission prompts
- **Code navigation** — contextual reads, regex search, symbol indexing, git state
- **GPU profiling** — PTX inspection, Nsight Compute, compute-sanitizer
- **CPU profiling** — Linux perf flamegraphs, hardware counter stats
- **Debugging** — GDB batch mode with structured backtrace/locals output
- **ML data** — PyTorch checkpoint and tensor inspection without a REPL
- **EDA** — Verilog lint/sim, cocotb, Vivado Tcl, FPGA programming, waveform parsing
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

Nsight Compute gives GPU diagnosis. perf_stat gives CPU counters. flamegraph gives the call-tree view. Three different tools for three different questions.

---

### Navigate code without re-reading whole files

```
cream:grep_code("__global__", path=".", glob="**/*.cu")
→ all CUDA kernel definitions with context

cream:find_symbol("ServerState", kind="struct")
→ { file: "src/server.rs", line: 57 }

cream:symbol_index(path=".", kinds=["fn", "trait"])
→ all public functions and traits, with file/line

cream:changed_since(since_relative="30m")
→ src/lib.rs, target/debug/bench (modified 4m ago)
```

---

### Debug with GDB without a terminal

```
cream:gdb_run("./bench", breakpoints=["main"])
→ backtrace: [{ frame: 0, function: "main", file: "bench.cu", line: 12 }]
  locals: [{ name: "N", value: "1048576" }]

cream:gdb_run("./bench", core="/tmp/core.1234")
→ signal: SIGSEGV, signal_addr: 0x7fff00000000
  backtrace: [...]
```

Batch GDB with structured output. Works on live runs and core dumps.

---

### Inspect ML checkpoints without a REPL

```
cream:checkpoint_list("./checkpoints")
→ epoch_42.ckpt — 1.2GB, epoch=42, step=84000
  epoch_41.ckpt — 1.2GB, epoch=41, step=82000

cream:tensor_inspect("./checkpoints/epoch_42.ckpt", keys=["model"])
→ model.weight: shape=[768, 768], dtype=float32, min=-0.21, max=0.19, mean=0.0001
```

Generates a Python script, runs it, parses the JSON output, cleans up. torch not installed → useful error message.

---

### Git state without raw output

```
cream:git_status()
→ branch: "feature/orient", ahead: 2, behind: 0
  unstaged: [{ path: "src/server.rs", status: "M" }]

cream:git_diff(file="src/server.rs")
→ [{ path: "src/server.rs", additions: 12, deletions: 3,
     hunks: [{ header: "@@ -55,7 +55,12 @@", lines: [...] }] }]

cream:git_log(limit=5)
→ [{ short: "a3f1b2", author: "daron", subject: "add orient tool" }]
```

---

### EDA: Verilog, cocotb, Vivado, FPGA

```
cream:verilog_lint(files=["alu.v", "tb_alu.v"])
→ [{ severity: "error", file: "alu.v", line: 34, message: "port width mismatch" }]

cream:verilog_sim(files=["alu.v", "tb_alu.v"], top="tb_alu", vcd_out="/tmp/alu.vcd")
→ { success: true, finished: true, assertion_failures: 0 }

cream:waveform_query("/tmp/alu.vcd", signals=["alu_out"], time_end=1000)
→ signal timeline as value-change events

cream:cocotb_run("./tests/alu", simulator="icarus")
→ { passed: 5, failed: 0 }

cream:vivado_tcl(cmd="open_project proj.xpr; synth_design -top alu")
→ { success: true, errors: [], warnings: ["timing not met on path ..."] }

cream:fpga_boards()
→ [{ target: "xc7a35t_0", device: "digilent_arty", part: "xc7a35tcsg324-1", status: "open" }]

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

### Execution
| Tool | What it does |
|------|-------------|
| `exec` | Run any shell command. stdout, stderr, exit_code, duration_ms. No prompts. |
| `build_check` | Structured errors `{file, line, col, message}` for CUDA / Rust / C / C++ |

### Code navigation
| Tool | What it does |
|------|-------------|
| `read_file` | File contents with pagination. filter= for line-level grep. |
| `read_context` | Window of N lines around a specific line number |
| `list_dir` | Directory tree with type/size/mtime, depth 1–5 |
| `glob` | Files matching a glob pattern |
| `grep_code` | Regex search with context lines, ripgrep-accelerated |
| `changed_since` | Files modified after a timestamp or relative duration |
| `which` | Binary path + version probe |

### Symbols & Git
| Tool | What it does |
|------|-------------|
| `symbol_index` | Index all Rust symbols (fn/struct/enum/trait/impl/…) across source files |
| `find_symbol` | Find a Rust symbol by name with file + line |
| `git_status` | Branch, ahead/behind, staged/unstaged/untracked — structured |
| `git_log` | Commit history with author, date, subject |
| `git_diff` | Unified diff as structured `{path, additions, deletions, hunks}` |

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
| `fpga_boards` | List JTAG-connected FPGA targets via hw_manager |
| `fpga_program` | Program a .bit bitstream via Vivado hw_manager |
| `waveform_query` | Parse VCD waveform — signal definitions and value-change timeline |

### System
| Tool | What it does |
|------|-------------|
| `process_tree` | Running processes from /proc: pid, name, state, mem, cmdline |
| `port_list` | Listening TCP/UDP ports via ss: proto, port, pid, process |
| `journal_query` | systemd journal entries with unit, time, grep filter |

### Rust tooling
| Tool | What it does |
|------|-------------|
| `cargo_tree` | Workspace members, versions, features, dependency graph |
| `test_run` | cargo test → structured `{passed, failed, ignored}` with names |
| `inspect_binary` | ldd dependencies, nm -D symbols, readelf ELF header |

### Filesystem operations
| Tool | What it does |
|------|-------------|
| `move_file` | Move/rename with optional reference scan for Rust mod/use |
| `mkdir` | Create directory with intermediate parents |
| `delete_file` | Delete a single file (refuses directories) |

### State
| Tool | What it does |
|------|-------------|
| `shell_state` | cream's cwd, PATH, dev-relevant environment variables |
| `set_cwd` | Change cream's working directory (persists across calls) |

### History
| Tool | What it does |
|------|-------------|
| `bench_history` | record / list / query / compare benchmark results. Persisted to disk. |

### HTTP
| Tool | What it does |
|------|-------------|
| `http_request` | Send HTTP/HTTPS requests via curl. Returns `{status, latency_ms, headers, body}`. |

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
├── shell-mcp       — MCP stdio server (53 tools)
└── shell-bin       — entry point: cream / cream -c / cream --mcp
```

The MCP server (`shell-mcp`) is independent of the shell runtime. It speaks JSON-RPC 2.0 over stdin/stdout and can be embedded in any project without any shell functionality.

The `shell-hooks` crate is the LLM integration seam. Every command passes through `HookRegistry::dispatch(HookEvent)` before and after execution. Future integration — command suggestion, error explanation, completion — plugs in here without touching the shell core.

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
