#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

fail=0

check_no_dep() {
  local crate="$1"
  local dep="$2"
  local manifest="crates/$crate/Cargo.toml"
  if rg -n "^${dep}[[:space:]]*=" "$manifest" >/tmp/ferrite-boundary-hit.$$ 2>/dev/null; then
    echo "boundary violation: $crate must not depend on $dep"
    cat /tmp/ferrite-boundary-hit.$$
    fail=1
  fi
}

check_no_any_deps() {
  local crate="$1"
  shift
  local dep
  for dep in "$@"; do
    check_no_dep "$crate" "$dep"
  done
}

all_internal=(
  shell-core
  shell-lexer
  shell-parser
  shell-runtime
  shell-hooks
  shell-tui
  shell-mcp
  shell-bin
)

# Lower crates may not depend upward.
check_no_any_deps shell-core "${all_internal[@]/shell-core/}"
check_no_any_deps shell-lexer shell-parser shell-runtime shell-hooks shell-tui shell-mcp shell-bin
check_no_any_deps shell-parser shell-runtime shell-hooks shell-tui shell-mcp shell-bin
check_no_any_deps shell-hooks shell-lexer shell-parser shell-runtime shell-tui shell-mcp shell-bin
check_no_any_deps shell-runtime shell-tui shell-mcp shell-bin
check_no_any_deps shell-tui shell-runtime shell-mcp shell-bin
check_no_any_deps shell-mcp shell-lexer shell-parser shell-runtime shell-hooks shell-tui shell-bin

# Warp UI is intentionally vendored but not wired into runtime/MCP/TUI yet.
for crate in shell-core shell-lexer shell-parser shell-runtime shell-hooks shell-tui shell-mcp; do
  check_no_dep "$crate" "warpui"
  check_no_dep "$crate" "warpui_core"
done

rm -f /tmp/ferrite-boundary-hit.$$

if [[ "$fail" -ne 0 ]]; then
  echo "boundary check failed"
  exit 1
fi

echo "boundary check passed"
