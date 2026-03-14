# ferrite-mcp

> **Copyright Daron Popov. All rights reserved.**  \
> This source is viewable for reference only.  \
> No license is granted for use, copying, modification, redistribution, sublicensing, or commercial use without prior written permission.

An MCP server that gives Claude Code and OpenAI Codex deep access to your local machine — hardware, builds, EDA tools, GPU profiling, and background job orchestration.

Works on **macOS** and **Linux**.

---

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/DaronPopov/ferrite-mcp/main/install.sh | sh
```

Safe to re-run — skips steps already done. Restart your AI client after install to activate.

The installer:
- Detects Rust/cargo, installs via rustup if missing
- Builds and installs the `ferrite` binary from source
- Registers `ferrite` in `~/.claude.json` (Claude Code)
- Registers `ferrite` in `~/.codex/config.toml` (OpenAI Codex) if present

---

## Manual registration

**Claude Code** — `~/.claude.json`:
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

**OpenAI Codex** — `~/.codex/config.toml`:
```toml
[mcp_servers.ferrite]
command = "/home/you/.cargo/bin/ferrite"
args    = ["--mcp"]
```

---

## Tools

| Category | Tools |
|---|---|
| **Filesystem** | `read_file`, `glob`, `grep_code`, `list_dir`, `move_file`, `mkdir`, `delete_file`, `changed_since` |
| **Execution** | `exec`, `build_check`, `task_run`, `launch`, `tty_exec` |
| **Background jobs** | `bg_spawn`, `bg_send`, `bg_status`, `bg_wait`, `bg_tail`, `bg_list`, `bg_kill`, `wait_for_pattern`, `wait_for_idle`, `output_summary`, `pipeline_run`, `pipeline_status`, `pipeline_cancel`, `live_window` |
| **Hardware / GPU** | `gpu_info`, `gpu_live`, `cpu_info`, `health`, `occupancy_calc`, `ptx_inspect` |
| **Profiling** | `ncu_profile`, `compute_sanitizer`, `perf_stat`, `flamegraph` |
| **Git** | `git_log`, `git_diff`, `git_status`, `git_checkpoint`, `git_commit`, `gh_clone`, `gh_sync`, `gh_status` |
| **EDA / FPGA** | `vivado_tcl`, `synth_report`, `fpga_program`, `fpga_boards`, `board_status`, `verilog_lint`, `verilog_sim`, `cocotb_run`, `waveform_query`, `fpga_serial`, `fpga_monitor` |
| **ML** | `tensor_inspect`, `checkpoint_list` |
| **Project** | `orient`, `project_context`, `chip_status`, `chip_build_pipeline`, `find_lib`, `discover` |
| **System** | `process_tree`, `port_list`, `tmux_ctl`, `session_status`, `session_restart` |
| **Workspace** | `shell_state`, `set_cwd`, `note`, `symbol_index`, `find_symbol`, `control_reconcile` |
| **Dynamic tools** | `tool_define`, `tool_undefine`, `tool_list_dynamic` |
| **Pre-flight** | `pre_validate`, `permissions_setup`, `env_doctor` |

---

## Config

`~/.config/ferrite/config.toml`

| Key | Effect |
|---|---|
| `terminal.mode` | `always` or `never` — open observer window on exec |
| `terminal.emulator` | `kitty`, `xterm`, `auto`, … |
| `paths.vivado` | Override Vivado bin directory |
| `git.auto_checkpoint` | Auto-create checkpoint before write/build/deploy tools |
| `git.strict` | Block tool execution if auto-checkpoint fails |
| `git.before_write` | Enable checkpoint hook for write tools |
| `git.before_build` | Enable checkpoint hook for build/test tools |
| `git.before_deploy` | Enable checkpoint hook for deploy tools |
| `git.add_mode` | Auto-checkpoint staging mode: `tracked` or `all` |

## MCP recycle thresholds

Set these env vars on the `ferrite --mcp` process to make the server exit cleanly after a response once a threshold is exceeded. The client can then spawn a fresh process.

- `FERRITE_MCP_MAX_CALLS`
- `FERRITE_MCP_MAX_UPTIME_SECS`
- `FERRITE_MCP_MAX_RSS_MB`

Use the `health` tool to inspect current uptime, RSS, call count, job-buffer pressure, and whether restart is recommended.

---

## Remote SSH workflow

For private remote hardware development, keep `ferrite` on the Linux workstation and reach it over SSH.

```sh
ferrite remote doctor
ferrite remote up
ferrite remote login-shell
ferrite remote mcp-config <host> [user]
```

- `remote doctor` checks Tailscale, SSH, tmux, feRcuda, Vivado, and CUDA.
- `remote up` prepares a tmux-backed session and prints the exact SSH attach command.
- `remote login-shell` is intended for password-based SSH logins: it creates or reuses the tmux session and attaches immediately.
- `remote mcp-config` prints the Mac-side Codex MCP stanza for `ssh ... ferrite --mcp`.

For password-based access from outside your home network, prefer Tailscale plus SSH password auth over exposing port `22` directly to the public internet.
