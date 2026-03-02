# ferrite-mcp

An MCP server that gives Claude Code and OpenAI Codex deep access to your local machine — hardware, builds, EDA tools, GPU profiling, background jobs, autonomous permission handling, and a full remote-access system.

Built in Rust. 75 tools. Zero friction for trusted operations.

---

## Install

**Public repo (HTTPS):**
```sh
curl -fsSL https://raw.githubusercontent.com/DaronPopov/ferrite-mcp/main/install.sh | sh
```

**Private repo / SSH:**
```sh
git -C /tmp/ferrite-mcp pull 2>/dev/null || git clone git@github.com:DaronPopov/ferrite-mcp.git /tmp/ferrite-mcp
sh /tmp/ferrite-mcp/install.sh
```

Safe to re-run — skips steps already done.

The installer:
- Detects Rust/cargo, installs via rustup if missing
- Builds and installs `ferrite` from source
- Registers `ferrite` in `~/.claude.json` (Claude Code)
- Registers `ferrite` in `~/.codex/config.toml` (OpenAI Codex) if present
- Installs the Claude plugin (`/remote-start` skill)
- Installs `ferrite-autostart` to `~/.local/bin/`
- Writes `/etc/sudoers.d/ferrite` — one-time sudo prompt granting NOPASSWD for apt, systemctl, ufw, snap, chmod, tee, etc. so the agent never blocks on privileged ops

Restart your AI IDE after install to activate.

---

## Remote access setup

### 1. Tailscale
Install [Tailscale](https://tailscale.com) on your dev machine and phone for a stable private IP without port forwarding.

### 2. Notifications
Create `~/.config/ferrite/env`:
```sh
NTFY_TOPIC=your-private-topic-name   # ntfy.sh push to phone (free, install ntfy app)
FERRITE_SESSION=claude-remote        # tmux session name
```

For Gmail notifications, create `~/.config/ferrite/gmail.conf`:
```sh
GMAIL_USER=you@gmail.com
GMAIL_APP_PASSWORD=your-app-password
GMAIL_TO=you@gmail.com
```

### 3. Start the autostart daemon
```sh
ferrite-autostart           # run manually
# or start the service only when needed
systemctl --user start ferrite-session
```

`ferrite-autostart` spawns a Claude Code session in tmux on demand, captures the remote session URL, and pushes it to your phone.

### 4. Connect from anywhere
```
/remote-start
```
Returns your Tailscale IP, SSH command, tmux attach command, and live session URL.

---

## Manual MCP registration

**Claude Code** — add to `~/.claude.json`:
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

**OpenAI Codex** — add to `~/.codex/config.toml`:
```toml
[mcp_servers.ferrite]
command = "/home/you/.cargo/bin/ferrite"
args    = ["--mcp"]
```

---

## Tools (75 total)

| Category | Tools |
|---|---|
| **Filesystem** | `read_file`, `glob`, `grep_code`, `list_dir`, `move_file`, `mkdir`, `delete_file`, `changed_since` |
| **Execution** | `exec` (auto-rewrites commands non-interactive, auto-retries on lock/permission errors), `task_run`, `launch` |
| **PTY driver** | `tty_exec` — runs programs in a real PTY, auto-responds to prompts (`[Y/n]`, licence dialogs, pagers) |
| **Permissions** | `pre_validate`, `permissions_setup`, `env_doctor` — pre-flight checks, sudoers status, PATH/disk/network |
| **Build** | `build_check` — Rust/CUDA/C/C++ with structured errors (file, line, col) |
| **Hardware / GPU** | `gpu_info`, `gpu_live`, `cpu_info`, `occupancy_calc`, `ptx_inspect` |
| **Profiling** | `ncu_profile`, `compute_sanitizer`, `perf_stat`, `flamegraph` |
| **EDA / FPGA** | `vivado_tcl`, `synth_report`, `fpga_program`, `fpga_boards`, `board_status`, `verilog_lint`, `verilog_sim`, `cocotb_run`, `waveform_query`, `fpga_serial`, `fpga_monitor` |
| **Background jobs** | `bg_spawn` (PTY), `bg_send`, `bg_status`, `bg_wait`, `bg_tail`, `bg_list`, `bg_kill`, `wait_for_pattern`, `wait_for_idle`, `output_summary`, `pipeline_run`, `pipeline_status`, `pipeline_cancel`, `live_window` |
| **Git** | `git_log`, `git_diff`, `git_status`, `git_commit`, `gh_clone`, `gh_sync`, `gh_status` |
| **Remote / session** | `tailscale_status`, `tmux_ctl`, `remote_exec`, `remote_build`, `sync_project`, `session_status`, `session_restart`, `session_handoff`, `notify` |
| **ML** | `tensor_inspect`, `checkpoint_list` |
| **Project / discovery** | `orient`, `project_context`, `chip_status`, `chip_build_pipeline`, `find_lib`, `discover` |
| **Dynamic tools** | `tool_define`, `tool_undefine`, `tool_list_dynamic` — register tools at runtime without restart |

---

## Config

File: `~/.config/ferrite/config.toml`

| Key / Env var | Effect |
|---|---|
| `terminal.mode` / `FERRITE_TERMINAL_MODE` | `always` or `never` — open observer window on exec |
| `terminal.emulator` / `FERRITE_TERMINAL_EMULATOR` | `kitty`, `xterm`, `auto`, … |
| `paths.vivado` / `FERRITE_VIVADO_PATH` | Override Vivado bin directory |
| `NTFY_TOPIC` | ntfy.sh topic for phone push notifications |
| `FERRITE_SESSION` | tmux session name (default: `claude-remote`) |

---

## Platforms

Linux (primary). macOS supported for non-EDA tools.
