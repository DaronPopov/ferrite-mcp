# ferrite-mcp

> **Copyright Daron Popov. All rights reserved.**  
> This source is viewable for reference only.  
> No license is granted for use, copying, modification, redistribution, sublicensing, or commercial use without prior written permission.

`ferrite-mcp` is a local stdio MCP server plus shell runtime. It is aimed at coding agents that need direct access to the machine they are running on: files, builds, git, background jobs, hardware/EDA tools, profiling, and project-specific automation.

It is not a cloud service and not a language-model wrapper. The core design is:

- `shell-mcp`: MCP server and tool surface
- `shell-bin`: `ferrite` CLI entrypoint
- `shell-runtime`, `shell-parser`, `shell-lexer`, `shell-core`, `shell-hooks`, `shell-tui`: local shell/runtime support

## Current State

The current system is a generic MCP server.

- No `fercuda` attachment remains.
- The deleted `ferrite-notify` crate is no longer part of the workspace.
- Request handling is selectively parallelized:
  - read-only MCP tool calls can run concurrently
  - stateful or conflicting operations remain serialized
  - stdout response emission stays single-owner and ordered by completion
- Shared server state is split internally:
  - `cwd` behind an `RwLock`
  - `config` behind an `RwLock`
  - session `notes` behind a separate `Mutex`

That means the server can overlap safe inspection/probe calls without allowing races in git, filesystem mutation, job control, or hardware control paths.

## Workspace

Current workspace members:

- `crates/shell-core`
- `crates/shell-lexer`
- `crates/shell-parser`
- `crates/shell-runtime`
- `crates/shell-hooks`
- `crates/shell-tui`
- `crates/shell-mcp`
- `crates/shell-bin`
- `crates/xtask`

The vendored Warp UI slice lives outside the main workspace at
`third_party/warp-ui`. The root workspace exposes `warpui` and `warpui_core` as
workspace dependencies, but the copied upstream crates keep their own workspace
so their pinned toolchain and support crates do not leak into the server build.

The crate boundaries are part of the design, not just folder organization. See
[ARCHITECTURE.md](ARCHITECTURE.md) and run `./scripts/check-boundaries.sh`
before moving functionality between layers.

For agent configuration, copy or reference [PROMPT.md](PROMPT.md) in Codex,
Claude Code, or other MCP-capable coding agents so they consistently use the
Ferrite toolset and editing model.

## Build And Verification

Use the Cargo xtask entrypoint for the full local check suite:

```sh
cargo run -p xtask -- check-all
```

Available checks:

- `check-main`: boundary checks, root workspace `cargo check`, and `shell-mcp`
  tests
- `check-ui`: vendored Warp UI `warpui` library check
- `check-macos`: root workspace checks for installed macOS Rust targets

The main Ferrite workspace is intended to build on Linux and macOS. The Warp UI
macOS target check requires Apple SDK headers and `xcrun`/`metal`, so it is
skipped automatically when run from Linux.

## Main Capabilities

The MCP surface is broad, but the tool families are straightforward:

- Filesystem and code inspection: `read_file`, `list_dir`, `glob`, `grep_code`, `read_context`, `changed_since`
- Command execution: `exec`, `build_check`, `task_run`, `launch`, `tty_exec`
- Background jobs and orchestration: `bg_spawn`, `bg_status`, `bg_wait`, `bg_tail`, `bg_list`, `bg_kill`, `pipeline_run`, `pipeline_status`, `pipeline_cancel`
- System and profiling: `process_tree`, `port_list`, `journal_query`, `gpu_info`, `gpu_live`, `cpu_info`, `perf_stat`, `flamegraph`, `ncu_profile`, `compute_sanitizer`, `ptx_inspect`
- Git and project automation: `git_status`, `git_diff`, `git_log`, `git_checkpoint`, `git_commit`, `gh_clone`, `gh_sync`, `gh_status`, `project_new`, `project_context`, `orient`
- FPGA / EDA: `verilog_lint`, `verilog_sim`, `xsim_elab`, `cocotb_run`, `vivado_tcl`, `synth_report`, `fpga_program`, `fpga_boards`, `board_status`, `waveform_query`, `rtl_regression_run`, `rtl_regression_report`, `fpga_triage`, `fpga_artifacts`
- Session / workspace utilities: `shell_state`, `set_cwd`, `note`, `symbol_index`, `find_symbol`, `control_reconcile`, `health`

## Concurrency Model

The server does not blindly run everything in parallel.

- `ParallelRead`: safe read-only requests may execute on worker threads.
- `SerializedState`: requests that mutate shared server/process state stay globally serialized.
- `SerializedResource`: requests that target a repo, file path, job, pipeline, or hardware endpoint are serialized per resource key.

Examples:

- `read_file`, `grep_code`, `health`, `process_tree`, `cargo_tree`: parallel-safe
- `set_cwd`, `note`, `config_ux`, `ux_wizard`: serialized global state
- `git_status`, `git_commit`, `move_file`, `bg_kill`, `fpga_program`: serialized by repo/path/job/hardware target

This is the core rule set the server uses today. It is intentionally conservative around mutation.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaronPopov/ferrite-mcp/main/install.sh | sh
```

The installer builds `ferrite` from source and registers it with supported MCP clients when their config files are present.

Manual registration example:

**Claude Code** `~/.claude.json`

```json
{
  "mcpServers": {
    "ferrite": {
      "type": "stdio",
      "command": "/home/you/.cargo/bin/ferrite",
      "args": ["--mcp"],
      "env": {}
    }
  }
}
```

**OpenAI Codex** `~/.codex/config.toml`

```toml
[mcp_servers.ferrite]
command = "/home/you/.cargo/bin/ferrite"
args = ["--mcp"]
```

## Config

Primary config file:

```text
~/.config/ferrite/config.toml
```

Common settings:

- `terminal.mode`: observer windows on command execution
- `terminal.emulator`: terminal backend selection
- `paths.vivado`: Vivado binary directory override
- `git.auto_checkpoint`: automatic pre-tool checkpoints
- `git.strict`: fail mutating calls when checkpointing fails
- `git.before_write`
- `git.before_build`
- `git.before_deploy`
- `git.add_mode`

## Runtime Health And Recycling

Optional environment variables on the `ferrite --mcp` process:

- `FERRITE_MCP_MAX_CALLS`
- `FERRITE_MCP_MAX_UPTIME_SECS`
- `FERRITE_MCP_MAX_RSS_MB`

When configured, the server exits cleanly after a response once a threshold is exceeded so the client can start a fresh process.

Use the `health` tool to inspect:

- uptime
- tool call count
- RSS
- note buffer size
- background job buffer pressure

## CLI

The main entrypoint is:

```sh
ferrite
```

Important modes:

- `ferrite --mcp`
- `ferrite -c <cmd>`
- `ferrite config list`
- `ferrite config get <key>`
- `ferrite config set <key> <value>`
- `ferrite install`
- `ferrite uninstall`
- `ferrite status`
