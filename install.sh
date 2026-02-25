#!/usr/bin/env bash
# cream MCP installer
# Builds cream from source and registers it as an MCP server for Claude Code.
set -e

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HOME/.cargo/bin/cream"

echo "cream MCP installer"
echo "==================="

# --- build -----------------------------------------------------------
echo "[1/3] building cream (release)..."
cd "$REPO_DIR"
cargo build --release -p shell-bin
cp target/release/cream "$BIN"
echo "      installed to $BIN"

# --- project-scoped MCP (this repo) ----------------------------------
echo "[2/3] writing .mcp.json for this project..."
cat > "$REPO_DIR/.mcp.json" <<EOF
{
  "mcpServers": {
    "cream": {
      "type": "stdio",
      "command": "${HOME}/.cargo/bin/cream",
      "args": ["--mcp"],
      "env": {}
    }
  }
}
EOF
echo "      .mcp.json written"

# --- global MCP registration -----------------------------------------
echo "[3/3] registering cream globally with Claude Code..."
if command -v claude &>/dev/null; then
    claude mcp add --transport stdio cream -- "$BIN" --mcp 2>/dev/null && \
        echo "      registered via 'claude mcp add'" || \
        echo "      (already registered or claude mcp add unavailable — skipping)"
else
    echo "      'claude' CLI not found — skipping global registration"
    echo "      To register manually, run:"
    echo "        claude mcp add --transport stdio cream -- $BIN --mcp"
fi

echo ""
echo "done. verify with: cream --version"
echo "      or open Claude Code in this directory and check /mcp"
