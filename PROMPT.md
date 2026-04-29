# Ferrite MCP Agent Prompt

Use Ferrite MCP as the local runtime for code-agent work. Ferrite is a stdio MCP
server that gives you deterministic filesystem access, command execution,
background jobs, git helpers, project inspection, profiling, hardware/EDA tools,
and session utilities.

## Operating Rules

- Prefer Ferrite tools over ad hoc shell commands when a structured tool exists.
- Inspect before mutating. Use `read_file`, `stat_file`, `grep_code`, `list_dir`,
  `glob`, `project_context`, `orient`, and `git_status` to understand the repo.
- For file edits, prefer deterministic edit tools:
  - `write_file` for new files.
  - `edit_file` for one exact unique replacement.
  - `sed_file` for regex substitutions.
  - `apply_patch` for a single-file unified diff.
  - `edit_transaction` for coordinated multi-file edits.
  - `replace_in_files` for broad mechanical replacements.
- Use `if_hash` from `stat_file` or `read_file` when editing an existing file.
  This prevents overwriting user changes made after you read the file.
- Do not use destructive git or filesystem operations unless explicitly asked.
- Treat user changes as authoritative. If a precondition fails, re-read and adapt.
- Keep changes scoped to the request. Do not refactor unrelated code.
- Run relevant verification after changes: `cargo check`, package tests,
  `./scripts/check-boundaries.sh`, or narrower commands as appropriate.

## Concurrency And Safety Model

Ferrite schedules tools by resource:

- Reads of the same file/repo/path can overlap.
- Writes to the same file/repo/path serialize.
- Writes wait for active reads on the same resource.
- Global state changes such as `set_cwd`, config changes, notes, and session
  restarts serialize against other tool calls.

You do not need to manually serialize normal tool calls, but you should still
avoid launching redundant work against the same file or repo.

## Command Execution

- Use `exec` for foreground commands that should return output.
- Use `timeout_secs` for commands that might hang.
- Use `bg_spawn`, `bg_status`, `bg_tail`, `bg_wait`, and `bg_kill` for long-running
  servers, watchers, or build loops.
- Use `tty_exec` only for interactive terminal programs.
- Use `pre_validate` before privileged or package-manager commands.
- Ferrite rewrites some commands to be non-interactive and sets safe environment
  defaults. Timed-out commands are killed as a process group on Unix.

## Git Workflow

- Start with `git_status`.
- Use `git_diff` to inspect current changes.
- Use `git_checkpoint` before risky mutation batches when useful.
- Use `git_commit` only when the user asks you to commit.
- Never discard user changes unless explicitly asked.

## Runtime Boundaries

Respect the repository boundaries in `ARCHITECTURE.md`:

- Core/parser/runtime crates must not depend on MCP, UI, or product tools.
- MCP tool code stays in `shell-mcp`.
- UI work should go through a dedicated app-facing layer, not direct imports from
  low-level runtime crates.
- Run `./scripts/check-boundaries.sh` before finishing boundary-sensitive work.

## Recommended Tool Patterns

Initial orientation:

```text
orient
git_status
list_dir
grep_code
read_file
```

Safe edit loop:

```text
stat_file -> get content_hash
read_file -> inspect target
edit_file/apply_patch/write_file with if_hash
git_diff
cargo check or relevant tests
```

Long-running dev server:

```text
bg_spawn
bg_status
bg_tail
bg_wait or wait_for_pattern
bg_kill when done
```

If a tool returns a structured error, use the code/detail fields instead of
guessing from prose.
