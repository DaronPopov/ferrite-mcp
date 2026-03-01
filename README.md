# ferrite-mcp

An MCP server that gives Claude Code and OpenAI Codex deep access to your local machine — hardware, builds, EDA tools, GPU profiling, background jobs, and a full remote-access system so you can control your dev machine from your phone or any terminal.

Built in Rust. 71 tools. Zero permission prompts for trusted operations.

---

## What it enables

### Local dev
Claude can directly run builds, read GPU state, flash FPGAs, profile CUDA kernels, run Verilog simulations, launch background jobs, and pipe structured results back — no manual copy-paste. Because ferrite runs as a trusted MCP server, it bypasses Claude Code's per-command permission prompts for local operations.

### Remote access
The biggest addition is a zero-touch remote session system. When your machine boots, `ferrite-autostart` automatically:

1. Spawns a Claude Code session inside a detached `tmux` window
2. Enables remote control and captures the `claude.ai` session URL
3. Sends you the link via desktop notification + Gmail + ntfy.sh push to your phone
4. Watches the session — restarts it automatically if it dies

From your phone or any SSH terminal, run `/remote-start` inside Claude to get your connection info instantly:

```
REMOTE ACCESS — your-machine
Tailscale IP : 100.x.x.x
SSH          : ssh <user>@100.x.x.x
Attach tmux  : tmux attach -t claude-remote
Session URL  : https://claude.ai/...
```

One tap on the link and you're in a live Claude session on your machine, from anywhere.

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

Safe to re-run — pulls latest if already cloned, skips steps already done.

The installer:
- Detects Rust/cargo, installs via rustup if missing
- Builds and installs `ferrite` from source
- Registers `ferrite` in `~/.claude.json` (Claude Code)
- Registers `ferrite` in `~/.codex/config.toml` (OpenAI Codex) if present
- Installs `ferrite-autostart` to `~/.local/bin/`

Restart your AI IDE after install to activate.

---

## Remote access setup

### 1. Tailscale
Install [Tailscale](https://tailscale.com) on your dev machine and phone. This gives you a stable private IP that works across networks without port forwarding.

### 2. Notifications
Create `~/.config/ferrite/env`:

```sh
# ntfy.sh push to phone (free, no account — install ntfy app and subscribe to this topic)
NTFY_TOPIC=your-private-topic-name

# tmux session name (default: claude-remote)
FERRITE_SESSION=claude-remote
```

For Gmail notifications, create `~/.config/cream/gmail.conf`:
```sh
GMAIL_USER=you@gmail.com
GMAIL_APP_PASSWORD=your-app-password   # Google account → Security → App passwords
GMAIL_TO=you@gmail.com
```

### 3. Start the autostart daemon
```sh
# Run manually
ferrite-autostart

# Or enable as a systemd user service
systemctl --user enable --now ferrite-autostart
```

### 4. Check status from inside Claude
```
/remote-start
```

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

## Usage

```sh
ferrite status       # check registration and config
ferrite install      # re-register with Claude Code
ferrite uninstall    # remove from Claude Code
ferrite config list  # show current config
ferrite --mcp        # start MCP stdio server (Claude/Codex does this automatically)
```

---

## Tools (71 total)

### Filesystem & code
`read_file`, `glob`, `grep_code`, `list_dir`, `move_file`, `mkdir`, `delete_file`, `changed_since`

### Execution
`exec`, `task_run` — run shell commands and scripts; structured stdout/stderr/exit_code, no permission prompts

### Build & compile
`build_check` — Rust/CUDA/C/C++ with structured errors (file, line, col)

### Hardware & GPU
`gpu_info`, `gpu_live`, `cpu_info`, `occupancy_calc`, `ptx_inspect` — GPU specs, live utilization, CUDA kernel analysis

### Profiling
`ncu_profile`, `compute_sanitizer`, `perf_stat`, `flamegraph` — Nsight Compute, memory sanitizer, Linux perf

### EDA (FPGA/RTL)
`vivado_tcl`, `synth_report`, `fpga_program`, `fpga_boards`, `board_status` — Vivado synthesis, timing/utilization reports, bitstream flash
`verilog_lint`, `verilog_sim`, `cocotb_run` — Icarus Verilog, cocotb
`waveform_query` — parse VCD files
`fpga_serial`, `fpga_monitor` — UART communication with FPGA

### Background jobs (16 tools)
`bg_spawn`, `bg_send`, `bg_status`, `bg_wait`, `bg_tail`, `bg_list`, `bg_kill` — long-running jobs with PTY support
`wait_for_pattern`, `wait_for_idle`, `output_summary` — smart polling
`pipeline_run`, `pipeline_status`, `pipeline_cancel` — parallel DAG pipelines
`live_window` — stream job output to a terminal window

### Git
`git_log`, `git_diff`, `git_status`, `git_commit`, `gh_clone`, `gh_sync`, `gh_status`

### Remote & session
`tailscale_status`, `tmux_ctl`, `remote_exec`, `remote_build`, `sync_project`
`session_status`, `session_restart`, `session_handoff` — Claude session lifecycle
`notify` — desktop + phone notifications (ntfy.sh)

### ML & data
`tensor_inspect`, `checkpoint_list` — PyTorch checkpoint inspection

### Discovery & project
`orient`, `project_context`, `chip_status`, `chip_build_pipeline`

### Dynamic tools
`tool_define`, `tool_undefine`, `tool_list_dynamic` — register new tools at runtime without restart

---

## Config

Config file: `~/.config/ferrite/config.toml`

| Env var | Effect |
|---|---|
| `FERRITE_TERMINAL_MODE` | `always` or `never` |
| `FERRITE_TERMINAL_EMULATOR` | `kitty`, `xterm`, `auto`, ... |
| `FERRITE_VIVADO_PATH` | Override Vivado bin directory |
| `NTFY_TOPIC` | ntfy.sh topic for phone notifications |
| `FERRITE_SESSION` | tmux session name (default: `claude-remote`) |
| `CODEX_BIN` | Path to codex binary (auto-detected from PATH if not set) |

---

## codex-remote-auth (optional)

`crates/codex-remote-auth/` is a standalone Axum HTTP server that adds HMAC-signed magic-link + OTP authentication in front of your remote session — useful if you want a browser-based auth gate in addition to Tailscale.

Copy `crates/codex-remote-auth/config.example.toml` to `~/.config/ferrite/codex-remote-auth.toml` and fill in your values. `email_command` is optional — omit it to disable email delivery.

---

## Platforms

Linux (primary). macOS supported for non-EDA tools (Apple Silicon tested).
