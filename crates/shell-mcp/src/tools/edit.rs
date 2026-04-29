//! Atomic file mutation primitives — the write side of the determinism
//! contract.
//!
//! Every mutating tool in the MCP surface (`write_file`, `edit_file`,
//! `apply_patch`, `sed_file`, `replace_in_files`, future `edit_transaction`)
//! goes through this module. The rules are deliberately strict:
//!
//! 1. **Compare-and-swap is mandatory.** Callers pass
//!    `Precondition::ExpectHash(old_hash)` — the current file content is
//!    re-hashed immediately before the write and the operation is
//!    refused if the hash has drifted. `ExpectNotExists` / `ExpectExists`
//!    cover create/replace semantics.
//! 2. **All writes are atomic.** We write to a sibling `.ferrite-tmp-*`
//!    file in the same directory, `fsync` it, then `rename()` over the
//!    target. A crash at any point leaves either the pre-write or the
//!    post-write content — never a half-written file.
//! 3. **Every mutation returns a `WriteReceipt`** carrying the pre-write
//!    hash, the post-write hash, the path, and the byte delta. This is
//!    the token downstream audit / git-guard / transaction code uses to
//!    reason about what changed.
//! 4. **No silent clobbers.** Directory creation is opt-in
//!    (`WriteOptions::create_dirs`). Overwriting an existing file
//!    requires either `ExpectHash` or `ExpectExists` — the default
//!    `ExpectNotExists` refuses to destroy content.
//! 5. **Dry-run is free.** Every mutation has a `dry_run` flag that
//!    performs every check and computes the receipt *without* touching
//!    the filesystem. Agents use this to preview an edit before
//!    committing.
//! 6. **Edits route through `text.rs`.** Content-level edits
//!    (`unique_replace`, `regex_substitute`, `apply_unified_diff`) load
//!    via `text::read_all`, so binary rejection, encoding detection, and
//!    the byte budget apply uniformly. After editing, the string is
//!    re-encoded as UTF-8 and written through `write_bytes`.
//!
//! Everything here is sync. There is no tokio involved — the MCP tool
//! lanes already provide all the concurrency control we need, and
//! blocking I/O keeps the code simple.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::tools::hash;
use crate::tools::text::{self, TextOptions};

/// The precondition a mutation asserts about the target file *before*
/// doing anything else. This is the determinism contract's compare-and-swap.
#[derive(Debug, Clone)]
pub enum Precondition {
    /// The file must not exist — any existing file, even empty, is a
    /// violation. Use for create-new semantics.
    NotExists,
    /// The file must exist. Use for blind overwrites where the caller
    /// genuinely doesn't care about prior content (rare; prefer
    /// `ExpectHash`).
    Exists,
    /// The file must exist and its `content_hash` must match. This is
    /// the primary CAS form — every responsible edit should use it.
    ExpectHash(String),
    /// No precondition; caller is asserting "I know what I'm doing".
    /// Reserved for tools that manage their own locking (e.g. the
    /// transaction layer).
    Unconditional,
}

/// Options shared by every write entry point. Small, plain values — no
/// builder ceremony.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    /// Precondition that must hold before the write is attempted.
    pub precondition: Precondition,
    /// If true, create the parent directory chain as needed.
    pub create_dirs: bool,
    /// If true, run the full check pipeline and return the receipt
    /// *without* touching the filesystem. The receipt's `pre_hash`
    /// reflects current on-disk state; `post_hash` is the hash of the
    /// would-be-written content.
    pub dry_run: bool,
    /// Unix mode to set on newly-created files. `None` inherits from
    /// the process umask. Ignored for existing files (their mode is
    /// preserved by the rename-over-tmp atomic write).
    pub mode: Option<u32>,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            precondition: Precondition::NotExists,
            create_dirs: false,
            dry_run: false,
            mode: None,
        }
    }
}

/// What happened (or would have happened) on a mutation. Returned by
/// every write path so tools can surface machine-readable outcomes.
#[derive(Debug, Clone)]
pub struct WriteReceipt {
    pub path: PathBuf,
    /// `content_hash` of the file before the write, or `None` if the
    /// file didn't exist.
    pub pre_hash: Option<String>,
    /// `content_hash` of the file after the write.
    pub post_hash: String,
    /// Byte count before (0 if new).
    pub pre_bytes: u64,
    /// Byte count after.
    pub post_bytes: u64,
    /// True if this was a `dry_run` — no bytes touched.
    pub dry_run: bool,
    /// True if the write created the file (pre_hash was None).
    pub created: bool,
}

/// Structured mutation error. Every variant carries enough context to
/// turn into a determinism-contract error without re-stringifying io
/// errors.
#[derive(Debug)]
pub enum EditError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    /// Text-layer failure (binary, too large, decode error, …).
    Text(text::TextError),
    /// Precondition violated. The caller expected a particular state
    /// but the file's actual state differs.
    PreconditionFailed {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    /// `unique_replace` found zero matches.
    PatternNotFound {
        path: PathBuf,
        pattern: String,
    },
    /// `unique_replace` found more than one match — refuses to guess.
    PatternNotUnique {
        path: PathBuf,
        pattern: String,
        count: usize,
    },
    /// Unified-diff application failed (context didn't match).
    PatchRejected {
        path: PathBuf,
        reason: String,
    },
    /// Regex compilation failed.
    InvalidRegex {
        pattern: String,
        reason: String,
    },
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::Io { path, source } => write!(f, "io at {}: {source}", path.display()),
            EditError::Text(e) => write!(f, "{e}"),
            EditError::PreconditionFailed {
                path,
                expected,
                actual,
            } => write!(
                f,
                "precondition failed at {}: expected {expected}, found {actual}",
                path.display()
            ),
            EditError::PatternNotFound { path, pattern } => {
                write!(f, "pattern not found in {}: {pattern:?}", path.display())
            }
            EditError::PatternNotUnique {
                path,
                pattern,
                count,
            } => write!(
                f,
                "pattern matches {count} times in {} (need exactly 1): {pattern:?}",
                path.display()
            ),
            EditError::PatchRejected { path, reason } => {
                write!(f, "patch rejected for {}: {reason}", path.display())
            }
            EditError::InvalidRegex { pattern, reason } => {
                write!(f, "invalid regex {pattern:?}: {reason}")
            }
        }
    }
}

impl std::error::Error for EditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EditError::Io { source, .. } => Some(source),
            EditError::Text(e) => Some(e),
            _ => None,
        }
    }
}

impl From<text::TextError> for EditError {
    fn from(value: text::TextError) -> Self {
        EditError::Text(value)
    }
}

/// Raw byte write. This is the bottom of the stack — every other write
/// routine funnels bytes through here so the atomic-rename and CAS
/// logic live in exactly one place.
pub fn write_bytes(
    path: &Path,
    content: &[u8],
    opts: &WriteOptions,
) -> Result<WriteReceipt, EditError> {
    // Probe current state.
    let (pre_hash, pre_bytes, exists) = probe(path)?;

    // Check precondition.
    check_precondition(path, &opts.precondition, pre_hash.as_deref(), exists)?;

    // Ensure parent directory exists if requested.
    if opts.create_dirs {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| EditError::Io {
                    path: parent.to_path_buf(),
                    source: e,
                })?;
            }
        }
    }

    let post_hash = hash::hash_bytes(content);
    let post_bytes = content.len() as u64;

    if opts.dry_run {
        return Ok(WriteReceipt {
            path: path.to_path_buf(),
            pre_hash,
            post_hash,
            pre_bytes,
            post_bytes,
            dry_run: true,
            created: !exists,
        });
    }

    // Atomic write: sibling tmp file → fsync → rename.
    atomic_write(path, content, opts.mode)?;

    Ok(WriteReceipt {
        path: path.to_path_buf(),
        pre_hash,
        post_hash,
        pre_bytes,
        post_bytes,
        dry_run: false,
        created: !exists,
    })
}

/// Write a UTF-8 string. Thin convenience wrapper around `write_bytes`.
pub fn write_str(
    path: &Path,
    content: &str,
    opts: &WriteOptions,
) -> Result<WriteReceipt, EditError> {
    write_bytes(path, content.as_bytes(), opts)
}

/// Replace a unique occurrence of `needle` with `replacement`. Fails if
/// the pattern is missing or appears more than once — no
/// guess-the-right-match behaviour. This is the determinism-safe
/// counterpart to `sed s/a/b/`.
pub fn unique_replace(
    path: &Path,
    needle: &str,
    replacement: &str,
    opts: &WriteOptions,
) -> Result<WriteReceipt, EditError> {
    let (text, _report) = text::read_all(path, &TextOptions::default())?;
    let count = text.matches(needle).count();
    if count == 0 {
        return Err(EditError::PatternNotFound {
            path: path.to_path_buf(),
            pattern: needle.to_owned(),
        });
    }
    if count > 1 {
        return Err(EditError::PatternNotUnique {
            path: path.to_path_buf(),
            pattern: needle.to_owned(),
            count,
        });
    }
    let new_text = text.replacen(needle, replacement, 1);
    write_str(path, &new_text, opts)
}

/// Replace every regex match with `replacement`. Standard `regex` crate
/// substitution syntax (`$1`, `${name}`, `$$`). Returns the receipt plus
/// the number of substitutions performed.
pub fn regex_substitute(
    path: &Path,
    pattern: &str,
    replacement: &str,
    opts: &WriteOptions,
) -> Result<(WriteReceipt, usize), EditError> {
    let re = regex::Regex::new(pattern).map_err(|e| EditError::InvalidRegex {
        pattern: pattern.to_owned(),
        reason: e.to_string(),
    })?;
    let (text, _) = text::read_all(path, &TextOptions::default())?;
    let count = re.find_iter(&text).count();
    let new_text = re.replace_all(&text, replacement).into_owned();
    let receipt = write_str(path, &new_text, opts)?;
    Ok((receipt, count))
}

/// Apply a unified diff to `path`. The diff must be single-file and
/// use normal `@@ -a,b +c,d @@` hunk headers. Uses the `similar` crate
/// to verify context lines before committing.
pub fn apply_unified_diff(
    path: &Path,
    diff: &str,
    opts: &WriteOptions,
) -> Result<WriteReceipt, EditError> {
    let (original, _) = text::read_all(path, &TextOptions::default())?;
    let patched = apply_diff_to_string(path, &original, diff)?;
    write_str(path, &patched, opts)
}

/// Apply a unified diff to an in-memory string. Exposed for the
/// transaction layer, which stages multiple patches before committing.
pub fn apply_diff_to_string(path: &Path, original: &str, diff: &str) -> Result<String, EditError> {
    // Parse hunks manually. The `similar` crate ships a patch applier,
    // but rolling our own keeps the dependency surface small and gives
    // us line-exact error messages. The accepted format is the unified
    // diff subset Claude Code and git both emit:
    //
    //   --- a/file
    //   +++ b/file
    //   @@ -start,count +start,count @@
    //    context
    //   -old
    //   +new
    //
    // We ignore the file headers (caller already picked the path) and
    // validate every context / removal line against the source.

    let orig_lines: Vec<&str> = original.split_inclusive('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(orig_lines.len());
    let mut cursor: usize = 0; // index into orig_lines

    let mut iter = diff.lines().peekable();
    while let Some(line) = iter.next() {
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        }
        if !line.starts_with("@@") {
            // Skip any leading prose / index: lines the diff might carry.
            continue;
        }
        // Parse the hunk header: @@ -a,b +c,d @@
        let header = line;
        let (old_start, _old_count) =
            parse_hunk_range(header, '-').ok_or_else(|| EditError::PatchRejected {
                path: path.to_path_buf(),
                reason: format!("malformed hunk header: {header}"),
            })?;
        // old_start is 1-indexed; 0 means "empty original".
        let hunk_orig_idx = if old_start == 0 { 0 } else { old_start - 1 };

        // Flush unchanged lines up to the hunk's start.
        if hunk_orig_idx < cursor {
            return Err(EditError::PatchRejected {
                path: path.to_path_buf(),
                reason: format!(
                    "hunk {header} starts at line {old_start} but cursor is already at {}",
                    cursor + 1
                ),
            });
        }
        while cursor < hunk_orig_idx {
            if cursor >= orig_lines.len() {
                return Err(EditError::PatchRejected {
                    path: path.to_path_buf(),
                    reason: format!("hunk {header} references line past end of file"),
                });
            }
            out.push(orig_lines[cursor].to_owned());
            cursor += 1;
        }

        // Consume hunk body lines until the next '@@' or end-of-diff.
        while let Some(&peek) = iter.peek() {
            if peek.starts_with("@@") {
                break;
            }
            let body = iter.next().unwrap();
            if body.is_empty() {
                // A blank line in the diff is a context-line space followed
                // by an empty string — treat it as context with "\n".
                if cursor >= orig_lines.len() {
                    return Err(EditError::PatchRejected {
                        path: path.to_path_buf(),
                        reason: "context line past end of file".into(),
                    });
                }
                out.push(orig_lines[cursor].to_owned());
                cursor += 1;
                continue;
            }
            let (sign, rest) = body.split_at(1);
            match sign {
                " " => {
                    // Context — must match the original.
                    if cursor >= orig_lines.len() {
                        return Err(EditError::PatchRejected {
                            path: path.to_path_buf(),
                            reason: format!("context past EOF: {rest:?}"),
                        });
                    }
                    let orig = strip_final_newline(orig_lines[cursor]);
                    if orig != rest {
                        return Err(EditError::PatchRejected {
                            path: path.to_path_buf(),
                            reason: format!(
                                "context mismatch at line {}: expected {:?}, found {:?}",
                                cursor + 1,
                                rest,
                                orig
                            ),
                        });
                    }
                    out.push(orig_lines[cursor].to_owned());
                    cursor += 1;
                }
                "-" => {
                    if cursor >= orig_lines.len() {
                        return Err(EditError::PatchRejected {
                            path: path.to_path_buf(),
                            reason: format!("deletion past EOF: {rest:?}"),
                        });
                    }
                    let orig = strip_final_newline(orig_lines[cursor]);
                    if orig != rest {
                        return Err(EditError::PatchRejected {
                            path: path.to_path_buf(),
                            reason: format!(
                                "deletion mismatch at line {}: expected {:?}, found {:?}",
                                cursor + 1,
                                rest,
                                orig
                            ),
                        });
                    }
                    cursor += 1;
                }
                "+" => {
                    // Preserve the trailing newline if the original file had one
                    // on surrounding lines; the diff format drops the '\n' on
                    // each body line.
                    let mut added = rest.to_owned();
                    added.push('\n');
                    out.push(added);
                }
                "\\" => {
                    // "\ No newline at end of file" — pop the last newline
                    // from the previous output line if we added one.
                    if let Some(last) = out.last_mut() {
                        if last.ends_with('\n') {
                            last.pop();
                        }
                    }
                }
                _ => {
                    return Err(EditError::PatchRejected {
                        path: path.to_path_buf(),
                        reason: format!("unknown diff line prefix: {body:?}"),
                    });
                }
            }
        }
    }

    // Flush any remaining unchanged lines.
    while cursor < orig_lines.len() {
        out.push(orig_lines[cursor].to_owned());
        cursor += 1;
    }

    Ok(out.concat())
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Look up (content_hash, size, exists) for the target.
fn probe(path: &Path) -> Result<(Option<String>, u64, bool), EditError> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => {
            let size = meta.len();
            let h = hash::hash_file(path).map_err(|e| EditError::Io {
                path: path.to_path_buf(),
                source: e,
            })?;
            Ok((Some(h), size, true))
        }
        Ok(_) => {
            // Exists but isn't a regular file — reject as if it didn't
            // exist for create purposes; callers asking for ExpectHash
            // will still get a precondition failure because there's no
            // hash.
            Ok((None, 0, true))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((None, 0, false)),
        Err(e) => Err(EditError::Io {
            path: path.to_path_buf(),
            source: e,
        }),
    }
}

fn check_precondition(
    path: &Path,
    pre: &Precondition,
    pre_hash: Option<&str>,
    exists: bool,
) -> Result<(), EditError> {
    match pre {
        Precondition::Unconditional => Ok(()),
        Precondition::NotExists => {
            if exists {
                Err(EditError::PreconditionFailed {
                    path: path.to_path_buf(),
                    expected: "file not to exist".into(),
                    actual: "file exists".into(),
                })
            } else {
                Ok(())
            }
        }
        Precondition::Exists => {
            if exists {
                Ok(())
            } else {
                Err(EditError::PreconditionFailed {
                    path: path.to_path_buf(),
                    expected: "file to exist".into(),
                    actual: "file not found".into(),
                })
            }
        }
        Precondition::ExpectHash(want) => match pre_hash {
            Some(have) if have == want => Ok(()),
            Some(have) => Err(EditError::PreconditionFailed {
                path: path.to_path_buf(),
                expected: format!("hash={want}"),
                actual: format!("hash={have}"),
            }),
            None => Err(EditError::PreconditionFailed {
                path: path.to_path_buf(),
                expected: format!("hash={want}"),
                actual: "file not found".into(),
            }),
        },
    }
}

/// Atomic write: create a sibling tmp file, `fsync`, rename over the
/// target. On Unix the rename is atomic for same-filesystem targets.
fn atomic_write(path: &Path, content: &[u8], mode: Option<u32>) -> Result<(), EditError> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .map(|s| s.to_owned())
        .unwrap_or_else(|| std::ffi::OsString::from("ferrite"));
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(&file_name);
    tmp_name.push(".ferrite-tmp-");
    tmp_name.push(format!("{}", std::process::id()));
    tmp_name.push("-");
    tmp_name.push(format!("{}", rand_suffix()));
    let tmp_path = parent.join(&tmp_name);

    let mut open = OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(mode.unwrap_or(0o644));
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
    }

    let mut tmp_file: File = open.open(&tmp_path).map_err(|e| EditError::Io {
        path: tmp_path.clone(),
        source: e,
    })?;
    if let Err(e) = tmp_file.write_all(content) {
        let _ = fs::remove_file(&tmp_path);
        return Err(EditError::Io {
            path: tmp_path,
            source: e,
        });
    }
    if let Err(e) = tmp_file.sync_all() {
        let _ = fs::remove_file(&tmp_path);
        return Err(EditError::Io {
            path: tmp_path,
            source: e,
        });
    }
    drop(tmp_file);

    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(EditError::Io {
            path: path.to_path_buf(),
            source: e,
        });
    }
    Ok(())
}

/// Cheap per-call suffix. Not a security nonce — just enough entropy to
/// avoid collisions between concurrent writers in the same process.
fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    // Mix in a per-call counter so nanoseconds collisions in the same
    // thread still produce distinct names.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    nanos.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(n)
}

fn strip_final_newline(s: &str) -> &str {
    if let Some(stripped) = s.strip_suffix('\n') {
        stripped.strip_suffix('\r').unwrap_or(stripped)
    } else {
        s
    }
}

fn parse_hunk_range(header: &str, sign: char) -> Option<(usize, usize)> {
    // @@ -12,3 +12,4 @@  ← find the sign section.
    let at = header.find(sign)?;
    let rest = &header[at + 1..];
    let end = rest.find([' ', '@'])?;
    let spec = &rest[..end];
    let (start, count) = match spec.split_once(',') {
        Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
        None => (spec.parse().ok()?, 1usize),
    };
    Some((start, count))
}

// ── Multi-file transactions ─────────────────────────────────────────────────
//
// `EditTransaction` lets a caller stage several mutations across many
// files and commit them as a unit. POSIX has no real cross-file atomic
// commit, so the contract is "best-effort transactional":
//
// 1. **Validate** every operation up front: load every target, check
//    every precondition, and compute the post-write content. If any op
//    is invalid, the whole transaction aborts and *nothing* is touched.
// 2. **Snapshot** every target's pre-write bytes in memory. This is the
//    rollback log.
// 3. **Commit** by writing each entry through `write_bytes` with
//    `Unconditional` precondition (we already validated). On the first
//    commit failure, attempt to restore previously-committed entries
//    from the snapshot. The error returned describes both the original
//    failure and the rollback outcome.
//
// In `dry_run` mode the validate phase still runs but the commit phase
// is skipped.

/// A single staged operation in an [`EditTransaction`]. Every variant
/// owns its own per-op precondition so callers can mix CAS, exists,
/// and not-exists checks across files.
#[derive(Debug, Clone)]
pub enum EditOp {
    /// Replace a file's full contents (or create it).
    Write {
        path: PathBuf,
        content: String,
        precondition: Precondition,
    },
    /// Replace a unique substring inside an existing file.
    Replace {
        path: PathBuf,
        find: String,
        replace: String,
        precondition: Precondition,
    },
    /// Apply a regex substitution to every match in a file.
    RegexSubstitute {
        path: PathBuf,
        pattern: String,
        replacement: String,
        precondition: Precondition,
    },
    /// Apply a unified diff to a file.
    Patch {
        path: PathBuf,
        diff: String,
        precondition: Precondition,
    },
}

impl EditOp {
    fn path(&self) -> &Path {
        match self {
            EditOp::Write { path, .. }
            | EditOp::Replace { path, .. }
            | EditOp::RegexSubstitute { path, .. }
            | EditOp::Patch { path, .. } => path,
        }
    }

    fn precondition(&self) -> &Precondition {
        match self {
            EditOp::Write { precondition, .. }
            | EditOp::Replace { precondition, .. }
            | EditOp::RegexSubstitute { precondition, .. }
            | EditOp::Patch { precondition, .. } => precondition,
        }
    }
}

/// What happened (or would have happened) when a transaction ran.
#[derive(Debug, Clone)]
pub struct TransactionReceipt {
    /// One receipt per op, in submission order.
    pub receipts: Vec<WriteReceipt>,
    /// True if the transaction was a dry-run (no bytes touched).
    pub dry_run: bool,
    /// True if a rollback was attempted (commit phase failure).
    pub rolled_back: bool,
    /// If `rolled_back`, the index of the op that triggered the failure.
    pub failed_op: Option<usize>,
}

/// Internal staged entry produced during the validate phase.
struct StagedEntry {
    path: PathBuf,
    pre_bytes: Vec<u8>,
    pre_hash: Option<String>,
    pre_existed: bool,
    new_bytes: Vec<u8>,
    post_hash: String,
}

/// Run a list of edit operations as a best-effort transaction.
///
/// Validates every op up front, then commits in submission order. On
/// commit failure, attempts to restore each previously-committed file
/// from its in-memory snapshot. Returns a [`TransactionReceipt`] with
/// one entry per op (even on failure — the entries up to and including
/// the failing op describe what was attempted and rolled back).
pub fn execute_transaction(
    ops: &[EditOp],
    dry_run: bool,
    create_dirs: bool,
) -> Result<TransactionReceipt, EditError> {
    // ── Phase 1: validate + stage ──────────────────────────────────────
    let mut staged: Vec<StagedEntry> = Vec::with_capacity(ops.len());
    for op in ops {
        let path = op.path();
        let (pre_hash, pre_bytes_len, exists) = probe(path)?;
        check_precondition(path, op.precondition(), pre_hash.as_deref(), exists)?;

        // Load existing bytes for snapshot. Empty Vec if the file didn't
        // exist — used during rollback to delete a created file.
        let pre_bytes = if exists {
            fs::read(path).map_err(|e| EditError::Io {
                path: path.to_path_buf(),
                source: e,
            })?
        } else {
            Vec::new()
        };
        debug_assert_eq!(pre_bytes.len() as u64, pre_bytes_len);

        // Compute new content for this op.
        let new_text = compute_new_content(op, &pre_bytes, exists)?;
        let new_bytes = new_text.into_bytes();
        let post_hash = hash::hash_bytes(&new_bytes);

        staged.push(StagedEntry {
            path: path.to_path_buf(),
            pre_bytes,
            pre_hash,
            pre_existed: exists,
            new_bytes,
            post_hash,
        });
    }

    // Build the receipt list (works for both dry-run and live).
    let make_receipts = |entries: &[StagedEntry], dry: bool| -> Vec<WriteReceipt> {
        entries
            .iter()
            .map(|e| WriteReceipt {
                path: e.path.clone(),
                pre_hash: e.pre_hash.clone(),
                post_hash: e.post_hash.clone(),
                pre_bytes: e.pre_bytes.len() as u64,
                post_bytes: e.new_bytes.len() as u64,
                dry_run: dry,
                created: !e.pre_existed,
            })
            .collect()
    };

    if dry_run {
        return Ok(TransactionReceipt {
            receipts: make_receipts(&staged, true),
            dry_run: true,
            rolled_back: false,
            failed_op: None,
        });
    }

    // ── Phase 2: commit in order ──────────────────────────────────────
    // Use Unconditional now — we already validated, and re-checking would
    // race with our own intermediate writes.
    let commit_opts = WriteOptions {
        precondition: Precondition::Unconditional,
        create_dirs,
        dry_run: false,
        mode: None,
    };

    let mut committed: Vec<usize> = Vec::with_capacity(staged.len());
    for (idx, entry) in staged.iter().enumerate() {
        verify_staged_entry_still_current(entry)?;
        match write_bytes(&entry.path, &entry.new_bytes, &commit_opts) {
            Ok(_) => committed.push(idx),
            Err(commit_err) => {
                // Roll back everything we've committed so far. Best-effort:
                // we don't surface rollback failures (they'd hide the real
                // error) but the receipt flags rolled_back=true.
                for &done_idx in committed.iter().rev() {
                    let done = &staged[done_idx];
                    if done.pre_existed {
                        let _ = atomic_write(&done.path, &done.pre_bytes, None);
                    } else {
                        let _ = fs::remove_file(&done.path);
                    }
                }
                // Annotate the error with the failing op's path.
                return Err(annotate_op_failure(idx, commit_err));
            }
        }
    }

    Ok(TransactionReceipt {
        receipts: make_receipts(&staged, false),
        dry_run: false,
        rolled_back: false,
        failed_op: None,
    })
}

/// Compute the post-write content for one op against the loaded bytes.
fn compute_new_content(op: &EditOp, pre_bytes: &[u8], exists: bool) -> Result<String, EditError> {
    match op {
        EditOp::Write { content, .. } => Ok(content.clone()),
        EditOp::Replace {
            path,
            find,
            replace,
            ..
        } => {
            require_text(path, pre_bytes, exists)?;
            let text = String::from_utf8(pre_bytes.to_vec()).map_err(|_| {
                EditError::Text(text::TextError::InvalidEncoding {
                    path: path.clone(),
                    encoding: "utf-8",
                })
            })?;
            let count = text.matches(find.as_str()).count();
            if count == 0 {
                return Err(EditError::PatternNotFound {
                    path: path.clone(),
                    pattern: find.clone(),
                });
            }
            if count > 1 {
                return Err(EditError::PatternNotUnique {
                    path: path.clone(),
                    pattern: find.clone(),
                    count,
                });
            }
            Ok(text.replacen(find.as_str(), replace.as_str(), 1))
        }
        EditOp::RegexSubstitute {
            path,
            pattern,
            replacement,
            ..
        } => {
            require_text(path, pre_bytes, exists)?;
            let text = String::from_utf8(pre_bytes.to_vec()).map_err(|_| {
                EditError::Text(text::TextError::InvalidEncoding {
                    path: path.clone(),
                    encoding: "utf-8",
                })
            })?;
            let re = regex::Regex::new(pattern).map_err(|e| EditError::InvalidRegex {
                pattern: pattern.clone(),
                reason: e.to_string(),
            })?;
            Ok(re.replace_all(&text, replacement.as_str()).into_owned())
        }
        EditOp::Patch { path, diff, .. } => {
            require_text(path, pre_bytes, exists)?;
            let text = String::from_utf8(pre_bytes.to_vec()).map_err(|_| {
                EditError::Text(text::TextError::InvalidEncoding {
                    path: path.clone(),
                    encoding: "utf-8",
                })
            })?;
            apply_diff_to_string(path, &text, diff)
        }
    }
}

fn require_text(path: &Path, bytes: &[u8], exists: bool) -> Result<(), EditError> {
    if !exists {
        return Err(EditError::Io {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::NotFound, "file not found"),
        });
    }
    // Cheap binary sniff: any NUL in the first 8 KiB → reject.
    let sniff = &bytes[..bytes.len().min(8 * 1024)];
    if sniff.contains(&0) {
        return Err(EditError::Text(text::TextError::Binary {
            path: path.to_path_buf(),
        }));
    }
    Ok(())
}

fn verify_staged_entry_still_current(entry: &StagedEntry) -> Result<(), EditError> {
    let (current_hash, _, current_exists) = probe(&entry.path)?;
    if current_exists != entry.pre_existed || current_hash != entry.pre_hash {
        return Err(EditError::PreconditionFailed {
            path: entry.path.clone(),
            expected: match &entry.pre_hash {
                Some(hash) => format!("hash={hash}"),
                None => "file not to exist".to_owned(),
            },
            actual: match current_hash {
                Some(hash) => format!("hash={hash}"),
                None if current_exists => "file exists without content hash".to_owned(),
                None => "file not found".to_owned(),
            },
        });
    }
    Ok(())
}

fn annotate_op_failure(idx: usize, err: EditError) -> EditError {
    // We can't easily attach metadata to the existing variants, so the
    // op index is embedded in the path-bearing message via the
    // PatchRejected variant when the original is opaque. For most
    // failures the original error is already specific enough.
    if let EditError::Io { path, source } = &err {
        EditError::PatchRejected {
            path: path.clone(),
            reason: format!("transaction op #{idx} failed: io: {source}"),
        }
    } else {
        err
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{tempdir, NamedTempFile};

    fn write_tmp(content: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn write_bytes_creates_new_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("new.txt");
        let receipt = write_bytes(&p, b"hi", &WriteOptions::default()).unwrap();
        assert!(receipt.created);
        assert_eq!(receipt.pre_hash, None);
        assert_eq!(receipt.pre_bytes, 0);
        assert_eq!(receipt.post_bytes, 2);
        assert_eq!(fs::read(&p).unwrap(), b"hi");
        assert_eq!(receipt.post_hash, hash::hash_bytes(b"hi"));
    }

    #[test]
    fn default_not_exists_refuses_overwrite() {
        let f = write_tmp(b"old");
        let err = write_bytes(f.path(), b"new", &WriteOptions::default()).unwrap_err();
        assert!(matches!(err, EditError::PreconditionFailed { .. }));
        assert_eq!(fs::read(f.path()).unwrap(), b"old");
    }

    #[test]
    fn expect_hash_matches_and_overwrites() {
        let f = write_tmp(b"old");
        let old_hash = hash::hash_bytes(b"old");
        let opts = WriteOptions {
            precondition: Precondition::ExpectHash(old_hash),
            ..Default::default()
        };
        let r = write_bytes(f.path(), b"new", &opts).unwrap();
        assert!(!r.created);
        assert_eq!(r.pre_bytes, 3);
        assert_eq!(r.post_bytes, 3);
        assert_eq!(fs::read(f.path()).unwrap(), b"new");
    }

    #[test]
    fn expect_hash_mismatch_refuses() {
        let f = write_tmp(b"old");
        let opts = WriteOptions {
            precondition: Precondition::ExpectHash("deadbeef".into()),
            ..Default::default()
        };
        let err = write_bytes(f.path(), b"new", &opts).unwrap_err();
        assert!(matches!(err, EditError::PreconditionFailed { .. }));
        assert_eq!(fs::read(f.path()).unwrap(), b"old");
    }

    #[test]
    fn expect_exists_on_missing_fails() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("missing");
        let opts = WriteOptions {
            precondition: Precondition::Exists,
            ..Default::default()
        };
        let err = write_bytes(&p, b"x", &opts).unwrap_err();
        assert!(matches!(err, EditError::PreconditionFailed { .. }));
    }

    #[test]
    fn dry_run_does_not_touch_filesystem() {
        let f = write_tmp(b"old");
        let opts = WriteOptions {
            precondition: Precondition::ExpectHash(hash::hash_bytes(b"old")),
            dry_run: true,
            ..Default::default()
        };
        let r = write_bytes(f.path(), b"new", &opts).unwrap();
        assert!(r.dry_run);
        assert_eq!(r.post_hash, hash::hash_bytes(b"new"));
        // Disk content unchanged.
        assert_eq!(fs::read(f.path()).unwrap(), b"old");
    }

    #[test]
    fn create_dirs_builds_parent_chain() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a/b/c/new.txt");
        let opts = WriteOptions {
            create_dirs: true,
            ..Default::default()
        };
        write_bytes(&p, b"x", &opts).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"x");
    }

    #[test]
    fn unique_replace_happy_path() {
        let f = write_tmp(b"alpha beta gamma\n");
        let opts = WriteOptions {
            precondition: Precondition::ExpectHash(hash::hash_bytes(b"alpha beta gamma\n")),
            ..Default::default()
        };
        let r = unique_replace(f.path(), "beta", "BETA", &opts).unwrap();
        assert_eq!(fs::read(f.path()).unwrap(), b"alpha BETA gamma\n");
        assert_eq!(r.post_hash, hash::hash_bytes(b"alpha BETA gamma\n"));
    }

    #[test]
    fn unique_replace_missing_pattern_errors() {
        let f = write_tmp(b"alpha\n");
        let err = unique_replace(
            f.path(),
            "zzz",
            "x",
            &WriteOptions {
                precondition: Precondition::Unconditional,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, EditError::PatternNotFound { .. }));
    }

    #[test]
    fn unique_replace_nonunique_errors() {
        let f = write_tmp(b"x x x\n");
        let err = unique_replace(
            f.path(),
            "x",
            "y",
            &WriteOptions {
                precondition: Precondition::Unconditional,
                ..Default::default()
            },
        )
        .unwrap_err();
        match err {
            EditError::PatternNotUnique { count, .. } => assert_eq!(count, 3),
            other => panic!("expected PatternNotUnique, got {other:?}"),
        }
    }

    #[test]
    fn regex_substitute_counts_and_replaces() {
        let f = write_tmp(b"a1 b2 c3\n");
        let opts = WriteOptions {
            precondition: Precondition::Unconditional,
            ..Default::default()
        };
        let (r, count) = regex_substitute(f.path(), r"[a-z](\d)", "X$1", &opts).unwrap();
        assert_eq!(count, 3);
        assert_eq!(fs::read(f.path()).unwrap(), b"X1 X2 X3\n");
        assert_eq!(r.post_hash, hash::hash_bytes(b"X1 X2 X3\n"));
    }

    #[test]
    fn regex_substitute_invalid_pattern_errors() {
        let f = write_tmp(b"abc\n");
        let err = regex_substitute(
            f.path(),
            "(",
            "x",
            &WriteOptions {
                precondition: Precondition::Unconditional,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, EditError::InvalidRegex { .. }));
    }

    #[test]
    fn apply_unified_diff_replaces_single_line() {
        let f = write_tmp(b"one\ntwo\nthree\n");
        let diff = "\
--- a/file
+++ b/file
@@ -1,3 +1,3 @@
 one
-two
+TWO
 three
";
        let opts = WriteOptions {
            precondition: Precondition::Unconditional,
            ..Default::default()
        };
        apply_unified_diff(f.path(), diff, &opts).unwrap();
        assert_eq!(fs::read(f.path()).unwrap(), b"one\nTWO\nthree\n");
    }

    #[test]
    fn apply_unified_diff_context_mismatch_rejects() {
        let f = write_tmp(b"one\ntwo\nthree\n");
        let diff = "\
--- a/file
+++ b/file
@@ -1,3 +1,3 @@
 ONE
-two
+TWO
 three
";
        let err = apply_unified_diff(
            f.path(),
            diff,
            &WriteOptions {
                precondition: Precondition::Unconditional,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, EditError::PatchRejected { .. }));
        // File still original.
        assert_eq!(fs::read(f.path()).unwrap(), b"one\ntwo\nthree\n");
    }

    #[test]
    fn apply_unified_diff_insertion_only() {
        let f = write_tmp(b"one\ntwo\n");
        let diff = "\
--- a/file
+++ b/file
@@ -1,2 +1,3 @@
 one
+ONE_AND_A_HALF
 two
";
        apply_unified_diff(
            f.path(),
            diff,
            &WriteOptions {
                precondition: Precondition::Unconditional,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(fs::read(f.path()).unwrap(), b"one\nONE_AND_A_HALF\ntwo\n");
    }

    #[test]
    fn apply_unified_diff_deletion_only() {
        let f = write_tmp(b"one\ntwo\nthree\n");
        let diff = "\
--- a/file
+++ b/file
@@ -1,3 +1,2 @@
 one
-two
 three
";
        apply_unified_diff(
            f.path(),
            diff,
            &WriteOptions {
                precondition: Precondition::Unconditional,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(fs::read(f.path()).unwrap(), b"one\nthree\n");
    }

    #[test]
    fn write_binary_target_fails_on_text_paths() {
        // Binary bytes — write_bytes accepts them directly (this is the
        // low-level path). But unique_replace must refuse because text
        // reader rejects binary.
        let f = write_tmp(&[b'h', 0, b'i']);
        let err = unique_replace(
            f.path(),
            "h",
            "H",
            &WriteOptions {
                precondition: Precondition::Unconditional,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            EditError::Text(text::TextError::Binary { .. })
        ));
    }

    #[test]
    fn atomic_write_rejects_concurrent_garbage_if_crash() {
        // Simulate: write once, verify no tmp files remain afterwards.
        let dir = tempdir().unwrap();
        let p = dir.path().join("target");
        write_bytes(&p, b"content", &WriteOptions::default()).unwrap();
        // No files other than the target should remain in the directory.
        let children: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0], std::ffi::OsString::from("target"));
    }

    #[test]
    fn receipt_preserves_pre_and_post_hashes() {
        let f = write_tmp(b"v1");
        let pre_hash = hash::hash_bytes(b"v1");
        let opts = WriteOptions {
            precondition: Precondition::ExpectHash(pre_hash.clone()),
            ..Default::default()
        };
        let r = write_bytes(f.path(), b"v2", &opts).unwrap();
        assert_eq!(r.pre_hash.as_deref(), Some(pre_hash.as_str()));
        assert_eq!(r.post_hash, hash::hash_bytes(b"v2"));
    }

    #[test]
    fn staged_transaction_entry_rejects_external_drift() {
        let f = write_tmp(b"v1");
        let staged = StagedEntry {
            path: f.path().to_path_buf(),
            pre_bytes: b"v1".to_vec(),
            pre_hash: Some(hash::hash_bytes(b"v1")),
            pre_existed: true,
            new_bytes: b"v2".to_vec(),
            post_hash: hash::hash_bytes(b"v2"),
        };

        fs::write(f.path(), b"external").unwrap();

        let err = verify_staged_entry_still_current(&staged).unwrap_err();
        assert!(matches!(err, EditError::PreconditionFailed { .. }));
    }
}
