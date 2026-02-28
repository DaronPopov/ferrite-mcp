#!/usr/bin/env sh
# ferrite-mcp installer — idempotent, safe to re-run any time
#
#   First install   → builds + registers everywhere
#   Re-run          → skips what's already done, updates binary if changed
#   New tool added  → picks up Claude Code or Codex registration automatically
#
# Public repo (HTTPS):
#   curl -fsSL https://raw.githubusercontent.com/DaronPopov/ferrite-mcp/main/install.sh | sh
#
# Private repo / SSH:
#   git clone git@github.com:DaronPopov/ferrite-mcp.git /tmp/ferrite-mcp && sh /tmp/ferrite-mcp/install.sh

set -e

REPO="https://github.com/DaronPopov/ferrite-mcp"
BIN="ferrite"
CARGO_BIN="$HOME/.cargo/bin/$BIN"

# Detect if we're running from a local clone — use --path instead of --git
SCRIPT_DIR="$(cd "$(dirname "$0")" 2>/dev/null && pwd || echo "")"
if [ -f "$SCRIPT_DIR/crates/shell-bin/Cargo.toml" ]; then
    LOCAL_PATH="$SCRIPT_DIR"
else
    LOCAL_PATH=""
fi

grn() { printf '\033[32m  ✓ %s\033[0m\n' "$*"; }
yel() { printf '\033[33m  ~ %s\033[0m\n' "$*"; }
inf() { printf '  · %s\n' "$*"; }
bold(){ printf '\033[1m%s\033[0m\n' "$*"; }

bold "ferrite-mcp setup"
echo ""

# ── 1. Rust / cargo ────────────────────────────────────────────────────────────
if command -v cargo >/dev/null 2>&1; then
    grn "Rust already installed ($(cargo --version 2>/dev/null | cut -d' ' -f2))"
else
    yel "Rust not found — installing via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
    . "$HOME/.cargo/env"
    grn "Rust installed"
fi

# ── 2. ferrite binary ──────────────────────────────────────────────────────────
ALREADY_INSTALLED=0
command -v "$BIN" >/dev/null 2>&1 && ALREADY_INSTALLED=1

if [ -n "$LOCAL_PATH" ]; then
    inf "Building from local clone ($LOCAL_PATH) ..."
    cargo install --path "$LOCAL_PATH/crates/shell-bin" || true
else
    inf "Building from $REPO ..."
    cargo install --git "$REPO" --bin "$BIN" --locked || true
fi

if ! command -v "$BIN" >/dev/null 2>&1; then
    printf '\033[31m  ✗ build failed — ferrite binary not found after install\033[0m\n'
    exit 1
elif [ "$ALREADY_INSTALLED" = "1" ]; then
    grn "ferrite updated"
else
    grn "ferrite installed"
fi

FERRITE_BIN="$(command -v $BIN 2>/dev/null || echo "$CARGO_BIN")"

echo ""
bold "Registering MCP server..."

# ── 3. Claude Code — ~/.claude.json ───────────────────────────────────────────
register_claude() {
    CLAUDE_JSON="$HOME/.claude.json"

    if ! command -v python3 >/dev/null 2>&1; then
        yel "python3 not found — add to ~/.claude.json manually"
        inf '"ferrite":{"type":"stdio","command":"'"$FERRITE_BIN"'","args":["--mcp"],"env":{}}'
        return
    fi

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

existing = d.get("mcpServers", {}).get("ferrite", {})
if existing.get("command") == bin_path and existing.get("args") == ["--mcp"]:
    print("already")
    sys.exit(0)

d.setdefault("mcpServers", {})["ferrite"] = {
    "type": "stdio", "command": bin_path, "args": ["--mcp"], "env": {}
}
tmp = path + ".tmp"
with open(tmp, "w") as f:
    json.dump(d, f, indent=2)
os.replace(tmp, path)
print("registered")
PYEOF
}

CLAUDE_STATUS="$(register_claude 2>/dev/null || echo "error")"
case "$CLAUDE_STATUS" in
    already)     grn "Claude Code — already registered" ;;
    registered)  grn "Claude Code — registered" ;;
    *)           yel "Claude Code — skipped (see above)" ;;
esac

# ── 4. OpenAI Codex — ~/.codex/config.toml ────────────────────────────────────
register_codex() {
    CODEX_DIR="$HOME/.codex"
    CODEX_CFG="$CODEX_DIR/config.toml"

    if ! command -v codex >/dev/null 2>&1 && [ ! -d "$CODEX_DIR" ]; then
        echo "skip"
        return
    fi

    mkdir -p "$CODEX_DIR"

    if grep -q '\[mcp_servers\.ferrite\]' "$CODEX_CFG" 2>/dev/null; then
        # Check if the command path is current
        if grep -A2 '\[mcp_servers\.ferrite\]' "$CODEX_CFG" | grep -q "$(printf '%s' "$FERRITE_BIN" | sed 's/[[\.*^$()+?{}|]/\\&/g')"; then
            echo "already"
        else
            # Path changed (e.g. after rustup change) — update in place
            if command -v python3 >/dev/null 2>&1; then
                python3 - "$CODEX_CFG" "$FERRITE_BIN" <<'PYEOF'
import sys, re
path, bin_path = sys.argv[1], sys.argv[2]
with open(path) as f:
    text = f.read()
text = re.sub(
    r'(\[mcp_servers\.ferrite\][^\[]*command\s*=\s*")[^"]*(")',
    r'\g<1>' + bin_path + r'\g<2>',
    text, flags=re.DOTALL
)
with open(path, "w") as f:
    f.write(text)
PYEOF
                echo "updated"
            else
                echo "already"
            fi
        fi
        return
    fi

    printf '\n[mcp_servers.ferrite]\ncommand = "%s"\nargs    = ["--mcp"]\n' \
        "$FERRITE_BIN" >> "$CODEX_CFG"
    echo "registered"
}

CODEX_STATUS="$(register_codex)"
case "$CODEX_STATUS" in
    already)     grn "OpenAI Codex  — already registered" ;;
    registered)  grn "OpenAI Codex  — registered" ;;
    updated)     grn "OpenAI Codex  — updated command path" ;;
    skip)        inf "OpenAI Codex  — not detected (skipped)" ;;
esac

# ── Done ───────────────────────────────────────────────────────────────────────
echo ""
bold "All done."
inf "Binary  : $FERRITE_BIN"
inf "Verify  : ferrite status"
inf "Restart Claude Code / Codex to activate any new registrations."
