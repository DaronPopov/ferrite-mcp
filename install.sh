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

# ── 5. Claude plugin (commands) ───────────────────────────────────────────────
PLUGIN_SRC="$SCRIPT_DIR/plugin"
PLUGIN_VERSION="1.0.0"
PLUGIN_CACHE="$HOME/.claude/plugins/cache/ferrite/ferrite-mcp/$PLUGIN_VERSION"
PLUGIN_KEY="ferrite-mcp@ferrite"

install_plugin() {
    if [ ! -d "$PLUGIN_SRC" ]; then
        echo "skip"
        return
    fi
    if ! command -v python3 >/dev/null 2>&1; then
        echo "nopy"
        return
    fi

    mkdir -p "$PLUGIN_CACHE"
    cp -r "$PLUGIN_SRC/." "$PLUGIN_CACHE/"

    # Register in installed_plugins.json and enable in settings.json
    python3 - "$HOME/.claude/plugins/installed_plugins.json" \
               "$HOME/.claude/settings.json" \
               "$PLUGIN_CACHE" \
               "$PLUGIN_KEY" \
               "$PLUGIN_VERSION" <<'PYEOF'
import json, sys, os, time

plugins_path = sys.argv[1]
settings_path = sys.argv[2]
install_path  = sys.argv[3]
key           = sys.argv[4]
version       = sys.argv[5]

# --- installed_plugins.json ---
if os.path.exists(plugins_path):
    with open(plugins_path) as f:
        try:    p = json.load(f)
        except: p = {}
else:
    p = {}
p.setdefault("version", 2)
p.setdefault("plugins", {})

entry = {
    "scope": "user",
    "installPath": install_path,
    "version": version,
    "installedAt": "2026-01-01T00:00:00.000Z",
    "lastUpdated": "2026-01-01T00:00:00.000Z",
}
existing = p["plugins"].get(key, [])
if not any(e.get("installPath") == install_path for e in existing):
    p["plugins"][key] = [entry]
    tmp = plugins_path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(p, f, indent=2)
    os.replace(tmp, plugins_path)

# --- settings.json ---
if os.path.exists(settings_path):
    with open(settings_path) as f:
        try:    s = json.load(f)
        except: s = {}
else:
    s = {}
s.setdefault("enabledPlugins", {})
if s["enabledPlugins"].get(key) is True:
    print("already")
    sys.exit(0)
s["enabledPlugins"][key] = True
tmp = settings_path + ".tmp"
with open(tmp, "w") as f:
    json.dump(s, f, indent=2)
os.replace(tmp, settings_path)
print("registered")
PYEOF
}

PLUGIN_STATUS="$(install_plugin 2>/dev/null || echo "error")"
case "$PLUGIN_STATUS" in
    already)     grn "Claude plugin  — already enabled (ferrite-mcp@ferrite)" ;;
    registered)  grn "Claude plugin  — installed → $PLUGIN_CACHE" ;;
    skip)        inf "Claude plugin  — plugin/ dir not found (skipped)" ;;
    nopy)        yel "Claude plugin  — python3 not found, skipped" ;;
    *)           yel "Claude plugin  — skipped (see above)" ;;
esac

# ── 6. Sudoers pre-authorization (NOPASSWD for agent-safe privileged ops) ─────
#
#  Writes /etc/sudoers.d/ferrite so the ferrite agent can run apt, systemctl,
#  ufw, snap, dpkg, chmod/chown, tee, etc. without any password prompt.
#  This is the ONLY interactive sudo call in the entire installer.
#
setup_sudoers() {
    SUDOERS_FILE="/etc/sudoers.d/ferrite"
    FERRITE_USER="$(id -un)"

    # Already current?
    if [ -f "$SUDOERS_FILE" ] && grep -q "^${FERRITE_USER} ALL=(ALL) NOPASSWD" "$SUDOERS_FILE" 2>/dev/null; then
        echo "already"
        return
    fi

    if ! command -v sudo >/dev/null 2>&1; then
        echo "nosudo"
        return
    fi

    # Check that we can sudo at all (may already have a cached token)
    if ! sudo -n true 2>/dev/null; then
        # Need one interactive prompt — warn the user clearly
        printf '\n\033[33m  › One-time sudo prompt to install /etc/sudoers.d/ferrite\033[0m\n'
        printf '    This grants NOPASSWD for: apt, systemctl, ufw, snap, dpkg, chmod, chown, tee\n'
        printf '    It can be removed at any time: sudo rm /etc/sudoers.d/ferrite\n\n'
    fi

    # Generate and validate the entry before writing
    ENTRY="# ferrite-mcp — agent system privilege pre-authorization
# Generated by install.sh — safe to remove if ferrite is uninstalled.
# Grants NOPASSWD for specific non-destructive system commands only.
Cmnd_Alias FERRITE_CMDS = /usr/bin/apt, /usr/bin/apt-get, /bin/systemctl, /usr/bin/systemctl, /usr/sbin/ufw, /usr/bin/snap, /usr/bin/dpkg, /sbin/ldconfig, /usr/sbin/ldconfig, /usr/bin/tee, /bin/tee, /bin/chmod, /usr/bin/chmod, /bin/chown, /usr/bin/chown, /usr/sbin/usermod, /usr/sbin/groupmod, /usr/bin/add-apt-repository, /usr/sbin/update-alternatives, /usr/bin/update-alternatives, /usr/bin/install
${FERRITE_USER} ALL=(ALL) NOPASSWD: FERRITE_CMDS
"

    # Write to a temp file, validate with visudo, then install
    TMP_SUDOERS="$(mktemp)"
    printf '%s' "$ENTRY" > "$TMP_SUDOERS"

    if ! sudo visudo -c -f "$TMP_SUDOERS" >/dev/null 2>&1; then
        rm -f "$TMP_SUDOERS"
        echo "invalid"
        return
    fi

    sudo cp "$TMP_SUDOERS" "$SUDOERS_FILE"
    sudo chmod 440 "$SUDOERS_FILE"
    rm -f "$TMP_SUDOERS"
    echo "registered"
}

SUDOERS_STATUS="$(setup_sudoers 2>/dev/null || echo "error")"
case "$SUDOERS_STATUS" in
    already)     grn "Sudoers         — already active (/etc/sudoers.d/ferrite)" ;;
    registered)  grn "Sudoers         — installed (/etc/sudoers.d/ferrite)" ;;
    nosudo)      yel "Sudoers         — sudo not found; privileged commands may prompt for password" ;;
    invalid)     yel "Sudoers         — visudo validation failed; entry NOT installed (safe)" ;;
    *)           yel "Sudoers         — skipped (run manually or re-run install.sh)" ;;
esac

# ── 7. ferrite-autostart script ───────────────────────────────────────────────
AUTOSTART_DST="$HOME/.local/bin/ferrite-autostart"
AUTOSTART_SRC="$SCRIPT_DIR/scripts/ferrite-autostart"


if [ -f "$AUTOSTART_SRC" ]; then
    mkdir -p "$HOME/.local/bin"
    cp "$AUTOSTART_SRC" "$AUTOSTART_DST"
    chmod +x "$AUTOSTART_DST"
    grn "ferrite-autostart installed → $AUTOSTART_DST"
    inf "Config  : ~/.config/cream/gmail.conf (GMAIL_USER, GMAIL_APP_PASSWORD, GMAIL_TO)"
    inf "Run     : ferrite-autostart  (or enable systemd: see README)"
else
    yel "ferrite-autostart — scripts/ferrite-autostart not found (skipped)"
fi

# ── Done ───────────────────────────────────────────────────────────────────────
echo ""
bold "All done."
inf "Binary  : $FERRITE_BIN"
inf "Autostart: $AUTOSTART_DST"
inf "Plugin  : /remote-control available after Claude Code restart"
inf "Verify  : ferrite status"
inf "Restart Claude Code / Codex to activate any new registrations."
