#!/usr/bin/env sh
# ferrite-mcp — one-line installer
#
# Installs the ferrite binary and registers it as an MCP server for:
#   • Claude Code  (~/.claude.json)
#   • OpenAI Codex (~/.codex/config.toml)
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/DaronPopov/ferrite-mcp/main/install.sh | sh

set -e

REPO="https://github.com/DaronPopov/ferrite-mcp"
BIN="ferrite"
CARGO_BIN="$HOME/.cargo/bin/$BIN"

# ── colour helpers ─────────────────────────────────────────────────────────────
grn() { printf '\033[32m%s\033[0m\n' "$*"; }
yel() { printf '\033[33m%s\033[0m\n' "$*"; }
bold(){ printf '\033[1m%s\033[0m\n'  "$*"; }

bold "ferrite-mcp installer"
echo ""

# ── 1. Ensure Rust / cargo ─────────────────────────────────────────────────────
if ! command -v cargo >/dev/null 2>&1; then
    yel "Rust not found — installing via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
    . "$HOME/.cargo/env"
fi

# ── 2. Build and install binary ────────────────────────────────────────────────
echo "Building ferrite (this takes ~30 s on first run)..."
cargo install --git "$REPO" --bin "$BIN" --locked --quiet
grn "  ✓ ferrite installed at $CARGO_BIN"

# Resolve actual path (respects PATH overrides)
FERRITE_BIN="$(command -v $BIN 2>/dev/null || echo "$CARGO_BIN")"

echo ""
echo "Registering MCP server..."

# ── 3. Claude Code — ~/.claude.json ───────────────────────────────────────────
register_claude() {
    CLAUDE_JSON="$HOME/.claude.json"

    if command -v python3 >/dev/null 2>&1; then
        python3 - "$CLAUDE_JSON" "$FERRITE_BIN" <<'PYEOF'
import json, sys, os

path     = sys.argv[1]
bin_path = sys.argv[2]

if os.path.exists(path):
    with open(path) as f:
        try:    d = json.load(f)
        except: d = {}
else:
    d = {}

d.setdefault("mcpServers", {})["ferrite"] = {
    "type":    "stdio",
    "command": bin_path,
    "args":    ["--mcp"],
    "env":     {}
}

tmp = path + ".tmp"
with open(tmp, "w") as f:
    json.dump(d, f, indent=2)
os.replace(tmp, path)
PYEOF
        grn "  ✓ Registered in ~/.claude.json"
    else
        yel "  python3 not found — add ferrite to ~/.claude.json manually:"
        yel '    "ferrite":{"type":"stdio","command":"'"$FERRITE_BIN"'","args":["--mcp"],"env":{}}'
    fi
}

# ── 4. OpenAI Codex — ~/.codex/config.toml ────────────────────────────────────
register_codex() {
    CODEX_DIR="$HOME/.codex"
    CODEX_CFG="$CODEX_DIR/config.toml"

    # Only register if codex is installed or its config dir exists
    if ! command -v codex >/dev/null 2>&1 && [ ! -d "$CODEX_DIR" ]; then
        return 0
    fi

    mkdir -p "$CODEX_DIR"

    if grep -q '\[mcp_servers\.ferrite\]' "$CODEX_CFG" 2>/dev/null; then
        echo "  ferrite already in ~/.codex/config.toml"
        return 0
    fi

    printf '\n[mcp_servers.ferrite]\ncommand = "%s"\nargs    = ["--mcp"]\n' \
        "$FERRITE_BIN" >> "$CODEX_CFG"
    grn "  ✓ Registered in ~/.codex/config.toml"
}

register_claude
register_codex

# ── Done ───────────────────────────────────────────────────────────────────────
echo ""
grn "Done!"
echo "  Binary : $FERRITE_BIN"
echo "  Verify : ferrite status"
echo "  Restart Claude Code / Codex to activate."
