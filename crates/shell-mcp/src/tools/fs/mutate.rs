//! MCP tool wrappers for the determinism-layer mutation surface.
//!
//! These are the JSON-in / JSON-out adapters that bridge the
//! `tools::edit` primitives to the MCP protocol. Every function in
//! here is a thin translation layer: parse args → call an edit
//! primitive → serialise the receipt.
//!
//! Shared argument conventions:
//!
//! - `path`: required, resolved via session cwd / tilde expansion.
//! - `if_hash`: optional string — when present, becomes
//!   `Precondition::ExpectHash`. Without it, the default
//!   (`NotExists` for create, `Exists` for in-place edits) applies
//!   unless the caller passes `"unconditional": true`.
//! - `create_dirs`: optional bool (default false).
//! - `dry_run`: optional bool (default false).
//! - `mode`: optional unix mode for new files.
//!
//! Every successful response carries the receipt so downstream
//! git-guard / audit code has a token to reason about what changed.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::edit::{
    self, EditError, EditOp, Precondition, TransactionReceipt, WriteOptions, WriteReceipt,
};
use crate::tools::hash;
use crate::tools::state::{resolve_or_cwd, resolve_path};
use crate::tools::text::{self, TextError, TextOptions};
use crate::tools::walk::{self, FileType, WalkOptions};

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_precondition(args: &Value, default: Precondition) -> Precondition {
    if let Some(h) = args["if_hash"].as_str() {
        return Precondition::ExpectHash(h.to_owned());
    }
    if args["unconditional"].as_bool().unwrap_or(false) {
        return Precondition::Unconditional;
    }
    if args["if_exists"].as_bool().unwrap_or(false) {
        return Precondition::Exists;
    }
    if args["if_not_exists"].as_bool().unwrap_or(false) {
        return Precondition::NotExists;
    }
    default
}

fn parse_write_opts(args: &Value, default_precond: Precondition) -> WriteOptions {
    WriteOptions {
        precondition: parse_precondition(args, default_precond),
        create_dirs: args["create_dirs"].as_bool().unwrap_or(false),
        dry_run: args["dry_run"].as_bool().unwrap_or(false),
        mode: args["mode"].as_u64().map(|m| m as u32),
    }
}

fn receipt_to_json(r: &WriteReceipt) -> Value {
    json!({
        "path":       r.path.display().to_string(),
        "pre_hash":   r.pre_hash,
        "post_hash":  r.post_hash,
        "pre_bytes":  r.pre_bytes,
        "post_bytes": r.post_bytes,
        "delta":      r.post_bytes as i64 - r.pre_bytes as i64,
        "dry_run":    r.dry_run,
        "created":    r.created,
    })
}

fn edit_err_to_tool_result(tool: &str, e: EditError) -> ToolResult {
    // Structured error payload. Keeps the display form for humans and
    // tags with `code` so agents can branch on kind without parsing.
    let (code, detail) = match &e {
        EditError::Io { path, source } => (
            "io",
            json!({ "path": path.display().to_string(), "source": source.to_string() }),
        ),
        EditError::Text(TextError::Binary { path }) => {
            ("binary", json!({ "path": path.display().to_string() }))
        }
        EditError::Text(TextError::TooLarge { path, size, limit }) => (
            "too_large",
            json!({
                "path":  path.display().to_string(),
                "size":  size,
                "limit": limit,
            }),
        ),
        EditError::Text(TextError::InvalidEncoding { path, encoding }) => (
            "invalid_encoding",
            json!({ "path": path.display().to_string(), "encoding": encoding }),
        ),
        EditError::Text(other) => ("text", json!({ "message": other.to_string() })),
        EditError::PreconditionFailed {
            path,
            expected,
            actual,
        } => (
            "precondition_failed",
            json!({
                "path":     path.display().to_string(),
                "expected": expected,
                "actual":   actual,
            }),
        ),
        EditError::PatternNotFound { path, pattern } => (
            "pattern_not_found",
            json!({ "path": path.display().to_string(), "pattern": pattern }),
        ),
        EditError::PatternNotUnique {
            path,
            pattern,
            count,
        } => (
            "pattern_not_unique",
            json!({
                "path":    path.display().to_string(),
                "pattern": pattern,
                "count":   count,
            }),
        ),
        EditError::PatchRejected { path, reason } => (
            "patch_rejected",
            json!({ "path": path.display().to_string(), "reason": reason }),
        ),
        EditError::InvalidRegex { pattern, reason } => (
            "invalid_regex",
            json!({ "pattern": pattern, "reason": reason }),
        ),
    };
    ToolResult::json(&json!({
        "error":  true,
        "tool":   tool,
        "code":   code,
        "detail": detail,
        "message": e.to_string(),
    }))
}

// ── write_file ────────────────────────────────────────────────────────────────

pub fn write_file(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let path = args["path"]
        .as_str()
        .ok_or("write_file: 'path' is required")?;
    let content = args["content"]
        .as_str()
        .ok_or("write_file: 'content' is required")?;
    let target = resolve_path(state, path)?;

    // Default: refuse to clobber. Callers must opt in via if_hash,
    // if_exists, or unconditional=true.
    let opts = parse_write_opts(args, Precondition::NotExists);

    match edit::write_str(&target, content, &opts) {
        Ok(r) => Ok(ToolResult::json(&json!({
            "ok":      true,
            "tool":    "write_file",
            "receipt": receipt_to_json(&r),
        }))),
        Err(e) => Ok(edit_err_to_tool_result("write_file", e)),
    }
}

// ── edit_file ─────────────────────────────────────────────────────────────────

pub fn edit_file(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let path = args["path"]
        .as_str()
        .ok_or("edit_file: 'path' is required")?;
    let find = args["find"]
        .as_str()
        .ok_or("edit_file: 'find' is required")?;
    let replace = args["replace"]
        .as_str()
        .ok_or("edit_file: 'replace' is required")?;
    let target = resolve_path(state, path)?;

    // In-place edit — file must already exist. Callers should pass
    // if_hash for real CAS.
    let opts = parse_write_opts(args, Precondition::Exists);

    match edit::unique_replace(&target, find, replace, &opts) {
        Ok(r) => Ok(ToolResult::json(&json!({
            "ok":      true,
            "tool":    "edit_file",
            "receipt": receipt_to_json(&r),
        }))),
        Err(e) => Ok(edit_err_to_tool_result("edit_file", e)),
    }
}

// ── sed_file ──────────────────────────────────────────────────────────────────

pub fn sed_file(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let path = args["path"]
        .as_str()
        .ok_or("sed_file: 'path' is required")?;
    let pattern = args["pattern"]
        .as_str()
        .ok_or("sed_file: 'pattern' is required")?;
    let replacement = args["replacement"]
        .as_str()
        .ok_or("sed_file: 'replacement' is required")?;
    let target = resolve_path(state, path)?;

    let opts = parse_write_opts(args, Precondition::Exists);

    match edit::regex_substitute(&target, pattern, replacement, &opts) {
        Ok((r, count)) => Ok(ToolResult::json(&json!({
            "ok":            true,
            "tool":          "sed_file",
            "substitutions": count,
            "receipt":       receipt_to_json(&r),
        }))),
        Err(e) => Ok(edit_err_to_tool_result("sed_file", e)),
    }
}

// ── apply_patch ───────────────────────────────────────────────────────────────

pub fn apply_patch(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let path = args["path"]
        .as_str()
        .ok_or("apply_patch: 'path' is required")?;
    let diff = args["diff"]
        .as_str()
        .ok_or("apply_patch: 'diff' is required")?;
    let target = resolve_path(state, path)?;

    let opts = parse_write_opts(args, Precondition::Exists);

    match edit::apply_unified_diff(&target, diff, &opts) {
        Ok(r) => Ok(ToolResult::json(&json!({
            "ok":      true,
            "tool":    "apply_patch",
            "receipt": receipt_to_json(&r),
        }))),
        Err(e) => Ok(edit_err_to_tool_result("apply_patch", e)),
    }
}

// ── stat_file ─────────────────────────────────────────────────────────────────

pub fn stat_file(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let path = args["path"]
        .as_str()
        .ok_or("stat_file: 'path' is required")?;
    let target = resolve_path(state, path)?;

    let meta = match std::fs::symlink_metadata(&target) {
        Ok(m) => m,
        Err(e) => {
            return Ok(ToolResult::json(&json!({
                "exists":  false,
                "path":    target.display().to_string(),
                "error":   e.to_string(),
            })));
        }
    };

    let file_type = if meta.is_dir() {
        "dir"
    } else if meta.file_type().is_symlink() {
        "symlink"
    } else if meta.is_file() {
        "file"
    } else {
        "other"
    };

    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    // Only hash regular files.
    let content_hash = if meta.is_file() {
        hash::hash_file(&target).ok()
    } else {
        None
    };

    // Best-effort encoding probe for small text files. Binary files
    // return `None` instead of an error so stat_file stays total.
    let encoding = if meta.is_file() && meta.len() <= 256 * 1024 {
        match text::read_all(&target, &TextOptions::default()) {
            Ok((_, report)) => Some(report.encoding),
            Err(_) => None,
        }
    } else {
        None
    };

    #[cfg(unix)]
    let unix_mode = {
        use std::os::unix::fs::PermissionsExt;
        Some(meta.permissions().mode())
    };
    #[cfg(not(unix))]
    let unix_mode: Option<u32> = None;

    Ok(ToolResult::json(&json!({
        "exists":       true,
        "path":         target.display().to_string(),
        "type":         file_type,
        "size":         meta.len(),
        "mtime":        mtime,
        "mode":         unix_mode,
        "content_hash": content_hash,
        "encoding":     encoding,
    })))
}

// ── read_bytes ────────────────────────────────────────────────────────────────

pub fn read_bytes(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let path = args["path"]
        .as_str()
        .ok_or("read_bytes: 'path' is required")?;
    let start = args["start"].as_u64().unwrap_or(0);
    let end = args["end"]
        .as_u64()
        .ok_or("read_bytes: 'end' is required")?;
    let target = resolve_path(state, path)?;

    match text::read_slice_bytes(&target, start, end, &TextOptions::default()) {
        Ok((content, report)) => Ok(ToolResult::json(&json!({
            "path":       target.display().to_string(),
            "start":      start,
            "end":        end,
            "content":    content,
            "size_bytes": report.size_bytes,
            "encoding":   report.encoding,
            "lines":      report.line_count,
        }))),
        Err(e) => Ok(edit_err_to_tool_result("read_bytes", EditError::Text(e))),
    }
}

// ── diff_files ────────────────────────────────────────────────────────────────

pub fn diff_files(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let a = args["a"].as_str().ok_or("diff_files: 'a' is required")?;
    let b = args["b"].as_str().ok_or("diff_files: 'b' is required")?;
    let ctx = args["context_lines"].as_u64().unwrap_or(3) as usize;
    let path_a = resolve_path(state, a)?;
    let path_b = resolve_path(state, b)?;

    let (text_a, _) = match text::read_all(&path_a, &TextOptions::default()) {
        Ok(v) => v,
        Err(e) => return Ok(edit_err_to_tool_result("diff_files", EditError::Text(e))),
    };
    let (text_b, _) = match text::read_all(&path_b, &TextOptions::default()) {
        Ok(v) => v,
        Err(e) => return Ok(edit_err_to_tool_result("diff_files", EditError::Text(e))),
    };

    let diff = similar::TextDiff::from_lines(&text_a, &text_b);
    let unified = diff
        .unified_diff()
        .context_radius(ctx)
        .header(&path_a.display().to_string(), &path_b.display().to_string())
        .to_string();

    let identical = text_a == text_b;
    Ok(ToolResult::json(&json!({
        "a":         path_a.display().to_string(),
        "b":         path_b.display().to_string(),
        "identical": identical,
        "diff":      unified,
    })))
}

// ── hash_file (convenience) ───────────────────────────────────────────────────

pub fn hash_file(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let path = args["path"]
        .as_str()
        .ok_or("hash_file: 'path' is required")?;
    let target = resolve_path(state, path)?;
    match hash::hash_file(&target) {
        Ok(h) => Ok(ToolResult::json(&json!({
            "path": target.display().to_string(),
            "content_hash": h,
        }))),
        Err(e) => Ok(ToolResult::error(format!(
            "hash_file: {}: {e}",
            target.display()
        ))),
    }
}

// ── edit_transaction ──────────────────────────────────────────────────────────

fn parse_op_precondition(op: &Value) -> Precondition {
    if let Some(h) = op["if_hash"].as_str() {
        return Precondition::ExpectHash(h.to_owned());
    }
    if op["unconditional"].as_bool().unwrap_or(false) {
        return Precondition::Unconditional;
    }
    if op["if_exists"].as_bool().unwrap_or(false) {
        return Precondition::Exists;
    }
    if op["if_not_exists"].as_bool().unwrap_or(false) {
        return Precondition::NotExists;
    }
    // Sensible default depends on op kind: write/create wants NotExists
    // unless overridden, in-place edits want Exists. Caller can always
    // override with an explicit flag.
    match op["op"].as_str().unwrap_or("write") {
        "write" => Precondition::NotExists,
        _ => Precondition::Exists,
    }
}

fn parse_edit_op(state: &Arc<Mutex<ServerState>>, raw: &Value) -> Result<EditOp, String> {
    let kind = raw["op"]
        .as_str()
        .ok_or("edit_transaction: each op needs 'op' (write|edit|sed|patch)")?;
    let path_str = raw["path"]
        .as_str()
        .ok_or("edit_transaction: each op needs 'path'")?;
    let path = resolve_path(state, path_str)?;
    let precondition = parse_op_precondition(raw);

    match kind {
        "write" => {
            let content = raw["content"]
                .as_str()
                .ok_or("edit_transaction: write op needs 'content'")?
                .to_owned();
            Ok(EditOp::Write {
                path,
                content,
                precondition,
            })
        }
        "edit" | "replace" => {
            let find = raw["find"]
                .as_str()
                .ok_or("edit_transaction: edit op needs 'find'")?
                .to_owned();
            let replace = raw["replace"]
                .as_str()
                .ok_or("edit_transaction: edit op needs 'replace'")?
                .to_owned();
            Ok(EditOp::Replace {
                path,
                find,
                replace,
                precondition,
            })
        }
        "sed" | "regex" => {
            let pattern = raw["pattern"]
                .as_str()
                .ok_or("edit_transaction: sed op needs 'pattern'")?
                .to_owned();
            let replacement = raw["replacement"]
                .as_str()
                .ok_or("edit_transaction: sed op needs 'replacement'")?
                .to_owned();
            Ok(EditOp::RegexSubstitute {
                path,
                pattern,
                replacement,
                precondition,
            })
        }
        "patch" => {
            let diff = raw["diff"]
                .as_str()
                .ok_or("edit_transaction: patch op needs 'diff'")?
                .to_owned();
            Ok(EditOp::Patch {
                path,
                diff,
                precondition,
            })
        }
        other => Err(format!(
            "edit_transaction: unknown op kind '{other}' (want write|edit|sed|patch)"
        )),
    }
}

fn transaction_receipt_to_json(r: &TransactionReceipt) -> Value {
    json!({
        "ops":         r.receipts.iter().map(receipt_to_json).collect::<Vec<_>>(),
        "dry_run":     r.dry_run,
        "rolled_back": r.rolled_back,
        "failed_op":   r.failed_op,
    })
}

pub fn edit_transaction(
    args: &Value,
    state: &Arc<Mutex<ServerState>>,
) -> Result<ToolResult, String> {
    let raw_ops = args["ops"]
        .as_array()
        .ok_or("edit_transaction: 'ops' must be an array")?;
    if raw_ops.is_empty() {
        return Err("edit_transaction: 'ops' must not be empty".into());
    }
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);
    let create_dirs = args["create_dirs"].as_bool().unwrap_or(false);

    let mut ops: Vec<EditOp> = Vec::with_capacity(raw_ops.len());
    for raw in raw_ops {
        ops.push(parse_edit_op(state, raw)?);
    }

    match edit::execute_transaction(&ops, dry_run, create_dirs) {
        Ok(receipt) => Ok(ToolResult::json(&json!({
            "ok":          true,
            "tool":        "edit_transaction",
            "transaction": transaction_receipt_to_json(&receipt),
        }))),
        Err(e) => Ok(edit_err_to_tool_result("edit_transaction", e)),
    }
}

// ── replace_in_files ──────────────────────────────────────────────────────────

/// Walk a tree, find every file containing `find` exactly once, and
/// replace it. Files with zero matches are skipped (not an error).
/// Files with more than one match abort the whole transaction unless
/// `allow_multi: true` (then a regex_substitute is used per file).
pub fn replace_in_files(
    args: &Value,
    state: &Arc<Mutex<ServerState>>,
) -> Result<ToolResult, String> {
    let find = args["find"]
        .as_str()
        .ok_or("replace_in_files: 'find' is required")?;
    let replace = args["replace"]
        .as_str()
        .ok_or("replace_in_files: 'replace' is required")?;
    let root = resolve_or_cwd(state, args["path"].as_str())?;
    let glob = args["glob"].as_str().unwrap_or("**/*").to_owned();
    let allow_multi = args["allow_multi"].as_bool().unwrap_or(false);
    let dry_run = args["dry_run"].as_bool().unwrap_or(false);

    // Discover candidate files via the unified walker.
    let walk_opts = WalkOptions {
        root: root.clone(),
        include: vec![glob],
        respect_gitignore: true,
        respect_ferriteignore: true,
        ..WalkOptions::default()
    };
    let iter = walk::walk_sequential(&walk_opts)
        .map_err(|e| format!("replace_in_files: walker error: {e:?}"))?;

    // Filter to files containing the substring. Skip files we can't
    // read as text (binary, too-large, etc.).
    let mut ops: Vec<EditOp> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();

    for entry in iter.filter(|e| matches!(e.file_type, FileType::File)) {
        let (text_str, _) = match text::read_all(&entry.path, &TextOptions::default()) {
            Ok(v) => v,
            Err(e) => {
                skipped.push(json!({
                    "path": entry.path.display().to_string(),
                    "reason": e.to_string(),
                }));
                continue;
            }
        };
        let count = text_str.matches(find).count();
        if count == 0 {
            continue;
        }
        if count > 1 && !allow_multi {
            return Ok(ToolResult::json(&json!({
                "error": true,
                "tool":  "replace_in_files",
                "code":  "pattern_not_unique",
                "detail": {
                    "path":    entry.path.display().to_string(),
                    "pattern": find,
                    "count":   count,
                },
                "message": format!(
                    "{}: pattern matches {count} times (need exactly 1, or pass allow_multi=true)",
                    entry.path.display()
                ),
            })));
        }
        if allow_multi {
            ops.push(EditOp::RegexSubstitute {
                path: entry.path.clone(),
                pattern: regex::escape(find),
                replacement: regex_safe_replacement(replace),
                precondition: Precondition::Exists,
            });
        } else {
            ops.push(EditOp::Replace {
                path: entry.path.clone(),
                find: find.to_owned(),
                replace: replace.to_owned(),
                precondition: Precondition::Exists,
            });
        }
    }

    if ops.is_empty() {
        return Ok(ToolResult::json(&json!({
            "ok":      true,
            "tool":    "replace_in_files",
            "matched": 0,
            "ops":     [],
            "skipped": skipped,
        })));
    }

    match edit::execute_transaction(&ops, dry_run, false) {
        Ok(receipt) => Ok(ToolResult::json(&json!({
            "ok":          true,
            "tool":        "replace_in_files",
            "matched":     receipt.receipts.len(),
            "transaction": transaction_receipt_to_json(&receipt),
            "skipped":     skipped,
        }))),
        Err(e) => Ok(edit_err_to_tool_result("replace_in_files", e)),
    }
}

/// Escape `$` in a literal replacement string so the regex engine
/// doesn't interpret `$1`, `$name`, etc. when we use a regex
/// substitution to honour `allow_multi`.
fn regex_safe_replacement(s: &str) -> String {
    s.replace('$', "$$")
}

#[allow(dead_code)]
fn _keep_imports(state: &Arc<Mutex<ServerState>>) {
    let _ = resolve_or_cwd(state, None);
    let _: &dyn Fn(&Path) = &|_| ();
}
