# Ferrite Architecture Boundaries

This project is a local code-agent runtime. Maintainability depends on keeping
the runtime layers explicit: lower crates must not know about higher crates,
agent protocols, UI, or product-specific tooling.

## Dependency Direction

Allowed crate flow:

```text
shell-bin
  -> shell-mcp
  -> shell-tui
  -> shell-runtime
  -> shell-parser
  -> shell-lexer
  -> shell-hooks
  -> shell-core

third_party/warp-ui is vendored UI framework code and is not part of the core
runtime dependency chain yet.
```

The practical dependency rules are stricter than the drawing:

- `shell-core`: foundational types only. No filesystem/process/network/UI/MCP.
- `shell-lexer`: tokenization only. Depends on `shell-core`.
- `shell-parser`: AST construction only. Depends on `shell-core`, `shell-lexer`.
- `shell-hooks`: extension contracts only. Depends on `shell-core`.
- `shell-runtime`: shell execution and builtins. Depends on `shell-core`,
  `shell-parser`, `shell-hooks`.
- `shell-tui`: terminal interaction, prompt, completion, history. Depends on
  `shell-core`, `shell-hooks`.
- `shell-mcp`: MCP protocol, tool registry, authorization, persistence, job
  control, and machine integration tools. It may depend on `shell-core`, but it
  must not leak MCP concepts into lower crates.
- `shell-bin`: composition and CLI entrypoints only.
- `third_party/warp-ui`: isolated vendored UI framework. App-facing UI crates
  should be introduced separately before any runtime crate depends on Warp UI.

## No Feature Leakage

Feature leakage means a lower or general-purpose crate starts importing concerns
from a higher or narrower layer. Examples:

- Parser code invoking MCP tools or reading user config.
- Runtime builtins depending on UI widgets.
- `shell-core` gaining process execution, filesystem walking, or serde-heavy
  protocol shapes.
- Tool-specific hardware/Git/CUDA behavior leaking into generic filesystem or
  text-edit primitives.
- UI crates depending directly on MCP tool implementation modules instead of a
  typed application/service boundary.

When a feature needs data across a boundary, introduce a small trait or data
type at the lower layer and implement it in the higher layer. Do not import the
higher layer downward.

## MCP Tool Module Boundaries

Inside `shell-mcp`, tools are grouped by operational domain:

- `tools/fs`: filesystem, code search, symbols, deterministic mutations.
- `tools/process`: foreground, background, PTY, pipeline execution.
- `tools/git`: git, GitHub, checkpoint guard, project creation.
- `tools/sys`: host, network, environment, binary inspection.
- `tools/session`: workspace/session/project/remote utilities.
- `tools/hw`: CUDA, FPGA, EDA, hardware-specific operations.
- `tools/meta`: health, dynamic config, permissions, control-plane UX.
- `tools/perf`: profiling, benchmark history, debug/perf tools.

Shared logic should live in a clearly named neutral module only when at least two
domains genuinely need it. Do not put domain-specific behavior into neutral
helpers just to avoid a parameter.

## UI Integration Boundary

Warp UI should not be wired directly into `shell-mcp` or `shell-runtime`.
Introduce an app-facing crate when the UI work starts, for example:

```text
shell-app
  -> shell-mcp-client or shell-agent-service
  -> shell-tui / warpui
```

The UI layer should consume typed runtime/session state and emit typed commands.
It should not call individual MCP tool modules directly. That keeps Codex,
Claude Code, CLI, TUI, and future GUI frontends using the same runtime contract.

## Enforcement

Run:

```sh
./scripts/check-boundaries.sh
```

The script checks the current crate dependency boundary rules and rejects obvious
upward dependencies or premature Warp UI coupling.
