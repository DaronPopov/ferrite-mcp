# ferrite-mcp

A machine intelligence layer for Claude Code and OpenAI Codex. Built in Rust.

## Install

**Public repo (HTTPS):**
```sh
curl -fsSL https://raw.githubusercontent.com/DaronPopov/ferrite-mcp/main/install.sh | sh
```

**Private repo / SSH:**
```sh
git -C /tmp/ferrite-mcp pull 2>/dev/null || git clone git@github.com:DaronPopov/ferrite-mcp.git /tmp/ferrite-mcp; sh /tmp/ferrite-mcp/install.sh
  git clone git@github.com:DaronPopov/ferrite-mcp.git /tmp/ferrite-mcp && sh /tmp/ferrite-mcp/install.sh

```

Safe to re-run any time — clones if missing, pulls latest if already cloned, skips steps already done.

## What it does

- Detects Rust/cargo, installs via rustup if missing
- Builds and installs `ferrite` from source
- Registers `ferrite` in `~/.claude.json` (Claude Code)
- Registers `ferrite` in `~/.codex/config.toml` (OpenAI Codex) if codex is installed
- Restart your AI IDE after install to activate

## Manual registration

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

## Usage

```sh
ferrite status       # check registration and config
ferrite install      # re-register with Claude Code
ferrite uninstall    # remove from Claude Code
ferrite config list  # show current config
ferrite --mcp        # start MCP stdio server (Claude/Codex does this automatically)
```

## Config

Config file: `~/.config/ferrite/config.toml`

| Env var                    | Effect                          |
|----------------------------|---------------------------------|
| `FERRITE_TERMINAL_MODE`    | `always` or `never`             |
| `FERRITE_TERMINAL_EMULATOR`| `kitty`, `xterm`, `auto`, ...   |
| `FERRITE_VIVADO_PATH`      | Override Vivado bin directory   |

## Tools

71 tools across: filesystem, hardware/GPU, exec, build_check, EDA (Vivado/iverilog/cocotb), git, profiling (perf/ncu), ML, background process orchestration, pipelines, remote SSH, tmux, notifications, and more.

Works on Linux and macOS (Apple Silicon).
