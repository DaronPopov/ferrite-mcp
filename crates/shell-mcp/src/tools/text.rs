//! Bounded-memory text I/O for the determinism contract.
//!
//! This module is the single source of truth for reading file *contents*
//! into the MCP tool surface. Every user-facing read tool (`read_file`,
//! `read_context`, `grep_code`'s rust fallback, etc.) goes through here so
//! behaviour is consistent and auditable.
//!
//! Design rules:
//!
//! 1. **Never load unbounded bytes.** Every entry point takes an explicit
//!    byte budget. Files larger than the budget are rejected with a
//!    structured error — callers opt in to streaming or slicing.
//! 2. **Binary detection is mandatory.** We sniff the first ~8 KiB for NUL
//!    bytes and a UTF-8/UTF-16 BOM before committing to a text decode.
//!    Binary files are never returned as `String` — callers must use the
//!    bytes path or ask for a stat-only read.
//! 3. **Encoding detection is explicit.** UTF-8 is the fast path; UTF-16
//!    LE/BE with BOM are transparently transcoded via `encoding_rs`; any
//!    other encoding (e.g. latin-1) is returned only if the caller passed
//!    `TextOptions::lossy = true`, in which case invalid sequences become
//!    U+FFFD.
//! 4. **Line endings are preserved.** We do not silently strip `\r`. Tools
//!    that need normalised text ask for it explicitly via
//!    `TextOptions::normalize_newlines`.
//! 5. **Line and byte slicing share one decoder.** `read_slice_lines` and
//!    `read_slice_bytes` both route through `read_all` once size has been
//!    validated, so the binary / encoding rules apply identically.
//!
//! The module is intentionally allocation-conservative: the hot path is a
//! single `Vec<u8>` read, one `String::from_utf8` (or `encoding_rs`
//! transcode), and direct slice returns. No per-line allocations on the
//! load path — splitting happens lazily when the caller asks for lines.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Default cap on bytes a single `read_all` call is allowed to load.
/// Mirrors Claude Code's 2 MB practical ceiling for in-context file reads.
pub const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Sniff window for binary/BOM detection. 8 KiB is enough to catch NULs in
/// almost every real binary format while remaining cheap to read.
const SNIFF_LEN: usize = 8 * 1024;

/// Structured error for the text layer. Every variant carries enough
/// context for an MCP tool to turn it into a determinism-contract error
/// without re-stringifying io errors by hand.
#[derive(Debug)]
pub enum TextError {
    /// The file did not exist, or the OS returned an error opening it.
    Io { path: PathBuf, source: io::Error },
    /// The file is larger than the caller's byte budget.
    TooLarge {
        path: PathBuf,
        size: u64,
        limit: u64,
    },
    /// The file's sniff window contained NUL bytes or a non-text BOM.
    Binary { path: PathBuf },
    /// The file is valid for the caller's requested encoding but the bytes
    /// could not be decoded (e.g. invalid UTF-8 without `lossy`).
    InvalidEncoding {
        path: PathBuf,
        encoding: &'static str,
    },
    /// The caller asked for a line range that falls outside the file.
    LineRangeOutOfBounds {
        path: PathBuf,
        requested_start: usize,
        requested_end: usize,
        available: usize,
    },
    /// The caller asked for a byte range that falls outside the file.
    ByteRangeOutOfBounds {
        path: PathBuf,
        requested_start: u64,
        requested_end: u64,
        available: u64,
    },
}

impl std::fmt::Display for TextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TextError::Io { path, source } => {
                write!(f, "io error at {}: {source}", path.display())
            }
            TextError::TooLarge { path, size, limit } => write!(
                f,
                "{} is {size} bytes, which exceeds the {limit}-byte read budget",
                path.display()
            ),
            TextError::Binary { path } => {
                write!(f, "{} looks binary (NUL bytes in sniff window)", path.display())
            }
            TextError::InvalidEncoding { path, encoding } => write!(
                f,
                "{} is not valid {encoding}",
                path.display()
            ),
            TextError::LineRangeOutOfBounds {
                path,
                requested_start,
                requested_end,
                available,
            } => write!(
                f,
                "{}: requested lines {requested_start}..{requested_end} but file has {available} lines",
                path.display()
            ),
            TextError::ByteRangeOutOfBounds {
                path,
                requested_start,
                requested_end,
                available,
            } => write!(
                f,
                "{}: requested bytes {requested_start}..{requested_end} but file is {available} bytes",
                path.display()
            ),
        }
    }
}

impl std::error::Error for TextError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TextError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Caller-facing options for every text read. These are small plain
/// values — no builder ceremony — so they can be inlined at tool
/// dispatch with a single struct literal.
#[derive(Debug, Clone, Copy)]
pub struct TextOptions {
    /// Maximum bytes we are willing to load from disk. Anything larger
    /// returns `TextError::TooLarge` with the actual size.
    pub max_bytes: u64,
    /// If true, transcode lossy-fallback encodings with U+FFFD replacements
    /// instead of failing with `InvalidEncoding`. Off by default —
    /// callers must opt in so determinism-sensitive reads stay strict.
    pub lossy: bool,
    /// If true, normalise `\r\n` and bare `\r` to `\n` in the returned
    /// string. Default is false — the raw bytes are preserved so edits
    /// round-trip.
    pub normalize_newlines: bool,
}

impl Default for TextOptions {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            lossy: false,
            normalize_newlines: false,
        }
    }
}

/// Report describing what was loaded. Returned alongside the decoded
/// contents so tools can surface metadata (encoding, size, line count)
/// without re-walking the bytes.
#[derive(Debug, Clone)]
pub struct TextReport {
    /// On-disk size in bytes (may differ from `contents.len()` after
    /// newline normalisation).
    pub size_bytes: u64,
    /// Name of the encoding actually used — always one of
    /// `"utf-8"`, `"utf-16le"`, `"utf-16be"`, or (with `lossy`) the
    /// label that `encoding_rs` reported.
    pub encoding: &'static str,
    /// Number of newline-terminated lines detected in the *returned*
    /// text. Trailing content without a final newline still counts as
    /// one line.
    pub line_count: usize,
    /// True if newline normalisation replaced `\r\n` or `\r` with `\n`.
    pub normalized_newlines: bool,
}

/// The canonical text-read primitive. Loads the entire file into memory,
/// subject to the caller's byte budget, and returns decoded text plus a
/// `TextReport`. Binary files are rejected.
pub fn read_all(path: &Path, opts: &TextOptions) -> Result<(String, TextReport), TextError> {
    let meta = std::fs::metadata(path).map_err(|e| TextError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let size = meta.len();
    if size > opts.max_bytes {
        return Err(TextError::TooLarge {
            path: path.to_path_buf(),
            size,
            limit: opts.max_bytes,
        });
    }
    let mut file = File::open(path).map_err(|e| TextError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mut buf = Vec::with_capacity(size as usize);
    file.read_to_end(&mut buf).map_err(|e| TextError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    decode_bytes(path, buf, size, opts)
}

/// Load a line-delimited slice of the file. `start_line` is 1-indexed
/// and inclusive; `end_line` is inclusive. Behaviour is equivalent to
/// `sed -n 'A,Bp'` but routes through the same decoder as `read_all`.
pub fn read_slice_lines(
    path: &Path,
    start_line: usize,
    end_line: usize,
    opts: &TextOptions,
) -> Result<(String, TextReport), TextError> {
    let (text, report) = read_all(path, opts)?;
    if start_line == 0 || end_line == 0 || start_line > end_line {
        return Err(TextError::LineRangeOutOfBounds {
            path: path.to_path_buf(),
            requested_start: start_line,
            requested_end: end_line,
            available: report.line_count,
        });
    }
    if start_line > report.line_count {
        return Err(TextError::LineRangeOutOfBounds {
            path: path.to_path_buf(),
            requested_start: start_line,
            requested_end: end_line,
            available: report.line_count,
        });
    }
    let clamped_end = end_line.min(report.line_count);
    // Preserve original line endings by splitting on '\n' and re-joining
    // with '\n' — works for both LF and CRLF because the '\r' stays on
    // the line. If the caller asked for normalisation, read_all already
    // rewrote them.
    let mut current = 0usize;
    let mut slice_start: Option<usize> = None;
    let mut slice_end: usize = text.len();
    let mut line_idx = 1usize;
    let bytes = text.as_bytes();
    while current <= bytes.len() {
        if line_idx == start_line && slice_start.is_none() {
            slice_start = Some(current);
        }
        if line_idx == clamped_end + 1 {
            slice_end = current;
            break;
        }
        // Advance to next '\n' or end.
        match bytes[current..].iter().position(|&b| b == b'\n') {
            Some(off) => {
                current += off + 1;
                line_idx += 1;
            }
            None => {
                // Past the last byte — if we were looking for the end of
                // the final line, clamp it.
                slice_end = bytes.len();
                break;
            }
        }
    }
    let start = slice_start.unwrap_or(0);
    let slice = &text[start..slice_end];
    let lines_out = count_lines(slice);
    let report = TextReport {
        line_count: lines_out,
        ..report
    };
    Ok((slice.to_owned(), report))
}

/// Load a byte-range slice. Half-open `[start, end)`. Fails if the range
/// is outside the file or if the file is larger than `max_bytes`.
pub fn read_slice_bytes(
    path: &Path,
    start: u64,
    end: u64,
    opts: &TextOptions,
) -> Result<(String, TextReport), TextError> {
    let (text, report) = read_all(path, opts)?;
    let size = text.len() as u64;
    if start > end || end > size {
        return Err(TextError::ByteRangeOutOfBounds {
            path: path.to_path_buf(),
            requested_start: start,
            requested_end: end,
            available: size,
        });
    }
    // Snap the slice to char boundaries so we never return invalid UTF-8.
    let mut s = start as usize;
    let mut e = end as usize;
    while s < text.len() && !text.is_char_boundary(s) {
        s += 1;
    }
    while e < text.len() && !text.is_char_boundary(e) {
        e += 1;
    }
    if e > text.len() {
        e = text.len();
    }
    let slice = &text[s..e];
    let lines_out = count_lines(slice);
    Ok((
        slice.to_owned(),
        TextReport {
            line_count: lines_out,
            ..report
        },
    ))
}

/// Shared decode path: takes an already-loaded byte buffer and runs the
/// binary sniff → encoding dispatch → optional normalisation pipeline.
fn decode_bytes(
    path: &Path,
    buf: Vec<u8>,
    size: u64,
    opts: &TextOptions,
) -> Result<(String, TextReport), TextError> {
    let (encoding, text) = match detect_encoding(&buf) {
        Encoding::Binary => {
            return Err(TextError::Binary {
                path: path.to_path_buf(),
            })
        }
        Encoding::Utf8 => match String::from_utf8(buf) {
            Ok(s) => ("utf-8", s),
            Err(e) => {
                if opts.lossy {
                    let replaced = String::from_utf8_lossy(e.as_bytes()).into_owned();
                    ("utf-8", replaced)
                } else {
                    return Err(TextError::InvalidEncoding {
                        path: path.to_path_buf(),
                        encoding: "utf-8",
                    });
                }
            }
        },
        Encoding::Utf16Le => {
            let (cow, _, had_errors) = encoding_rs::UTF_16LE.decode(&buf);
            if had_errors && !opts.lossy {
                return Err(TextError::InvalidEncoding {
                    path: path.to_path_buf(),
                    encoding: "utf-16le",
                });
            }
            ("utf-16le", cow.into_owned())
        }
        Encoding::Utf16Be => {
            let (cow, _, had_errors) = encoding_rs::UTF_16BE.decode(&buf);
            if had_errors && !opts.lossy {
                return Err(TextError::InvalidEncoding {
                    path: path.to_path_buf(),
                    encoding: "utf-16be",
                });
            }
            ("utf-16be", cow.into_owned())
        }
    };

    let (text, normalized) = if opts.normalize_newlines {
        (normalize_newlines(&text), true)
    } else {
        (text, false)
    };
    let line_count = count_lines(&text);
    Ok((
        text,
        TextReport {
            size_bytes: size,
            encoding,
            line_count,
            normalized_newlines: normalized,
        },
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Binary,
    Utf8,
    Utf16Le,
    Utf16Be,
}

/// Sniff the leading bytes to decide text vs binary and detect BOMs.
/// The rules are deliberately conservative: only explicit UTF-16 BOMs
/// escape the "looks binary if NUL present" check, because unmarked
/// UTF-16 is indistinguishable from a real binary with NUL bytes.
fn detect_encoding(buf: &[u8]) -> Encoding {
    if buf.starts_with(&[0xFF, 0xFE]) {
        return Encoding::Utf16Le;
    }
    if buf.starts_with(&[0xFE, 0xFF]) {
        return Encoding::Utf16Be;
    }
    // UTF-8 BOM is harmless — fall through to utf-8 path; `from_utf8`
    // will accept it as valid bytes and the caller can strip it if they
    // want to.
    let sniff = &buf[..buf.len().min(SNIFF_LEN)];
    if sniff.contains(&0) {
        return Encoding::Binary;
    }
    Encoding::Utf8
}

/// Rewrite `\r\n` and bare `\r` into `\n`. Preserves multi-byte UTF-8
/// sequences because it only looks at ASCII CR/LF bytes.
fn normalize_newlines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\r' {
            out.push('\n');
            // Skip the following '\n' if this was a CRLF pair.
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
        } else {
            // Push the whole UTF-8 char in one step for non-ASCII bytes.
            let ch_start = i;
            i += 1;
            while i < bytes.len() && (bytes[i] & 0xC0) == 0x80 {
                i += 1;
            }
            // Safety: slice is a valid char boundary because `s` is &str.
            out.push_str(&s[ch_start..i]);
        }
    }
    out
}

/// Count newline-terminated lines. Trailing content without a final
/// newline still counts as one line, matching `wc -l` + 1 semantics
/// that humans expect from "how many lines does this file have".
fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    let mut n = s.bytes().filter(|&b| b == b'\n').count();
    if !s.ends_with('\n') {
        n += 1;
    }
    n
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn tmp_with(bytes: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn reads_plain_utf8() {
        let f = tmp_with(b"hello\nworld\n");
        let (text, report) = read_all(f.path(), &TextOptions::default()).unwrap();
        assert_eq!(text, "hello\nworld\n");
        assert_eq!(report.encoding, "utf-8");
        assert_eq!(report.size_bytes, 12);
        assert_eq!(report.line_count, 2);
        assert!(!report.normalized_newlines);
    }

    #[test]
    fn preserves_crlf_by_default() {
        let f = tmp_with(b"a\r\nb\r\nc");
        let (text, report) = read_all(f.path(), &TextOptions::default()).unwrap();
        assert_eq!(text, "a\r\nb\r\nc");
        assert_eq!(report.line_count, 3); // a, b, c — trailing no-newline counts
        assert!(!report.normalized_newlines);
    }

    #[test]
    fn normalize_newlines_converts_crlf_and_cr() {
        let f = tmp_with(b"a\r\nb\rc\n");
        let opts = TextOptions {
            normalize_newlines: true,
            ..Default::default()
        };
        let (text, report) = read_all(f.path(), &opts).unwrap();
        assert_eq!(text, "a\nb\nc\n");
        assert!(report.normalized_newlines);
    }

    #[test]
    fn rejects_binary_by_nul_bytes() {
        let f = tmp_with(&[b'h', b'i', 0, b'\n']);
        let err = read_all(f.path(), &TextOptions::default()).unwrap_err();
        assert!(matches!(err, TextError::Binary { .. }));
    }

    #[test]
    fn too_large_rejected_with_actual_size() {
        let payload = vec![b'x'; 1024];
        let f = tmp_with(&payload);
        let opts = TextOptions {
            max_bytes: 512,
            ..Default::default()
        };
        let err = read_all(f.path(), &opts).unwrap_err();
        match err {
            TextError::TooLarge { size, limit, .. } => {
                assert_eq!(size, 1024);
                assert_eq!(limit, 512);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn invalid_utf8_strict_errors() {
        let f = tmp_with(&[b'a', 0xFF, 0xFE_u8.wrapping_add(1), b'b']);
        let err = read_all(f.path(), &TextOptions::default()).unwrap_err();
        assert!(matches!(
            err,
            TextError::InvalidEncoding {
                encoding: "utf-8",
                ..
            }
        ));
    }

    #[test]
    fn invalid_utf8_lossy_replaces() {
        let f = tmp_with(&[b'a', 0xFF, b'b']);
        let opts = TextOptions {
            lossy: true,
            ..Default::default()
        };
        let (text, report) = read_all(f.path(), &opts).unwrap();
        assert_eq!(report.encoding, "utf-8");
        assert!(text.contains('a'));
        assert!(text.contains('b'));
        assert!(text.contains('\u{FFFD}'));
    }

    #[test]
    fn utf16_le_bom_transcoded() {
        // UTF-16 LE BOM + "hi"
        let bytes: Vec<u8> = vec![0xFF, 0xFE, b'h', 0, b'i', 0];
        let f = tmp_with(&bytes);
        let (text, report) = read_all(f.path(), &TextOptions::default()).unwrap();
        assert_eq!(text, "hi");
        assert_eq!(report.encoding, "utf-16le");
    }

    #[test]
    fn utf16_be_bom_transcoded() {
        let bytes: Vec<u8> = vec![0xFE, 0xFF, 0, b'h', 0, b'i'];
        let f = tmp_with(&bytes);
        let (text, report) = read_all(f.path(), &TextOptions::default()).unwrap();
        assert_eq!(text, "hi");
        assert_eq!(report.encoding, "utf-16be");
    }

    #[test]
    fn slice_lines_returns_inclusive_range() {
        let f = tmp_with(b"one\ntwo\nthree\nfour\nfive\n");
        let (text, _) = read_slice_lines(f.path(), 2, 4, &TextOptions::default()).unwrap();
        assert_eq!(text, "two\nthree\nfour\n");
    }

    #[test]
    fn slice_lines_first_line_only() {
        let f = tmp_with(b"one\ntwo\nthree\n");
        let (text, _) = read_slice_lines(f.path(), 1, 1, &TextOptions::default()).unwrap();
        assert_eq!(text, "one\n");
    }

    #[test]
    fn slice_lines_clamps_end_beyond_file() {
        let f = tmp_with(b"one\ntwo\n");
        let (text, _) = read_slice_lines(f.path(), 1, 99, &TextOptions::default()).unwrap();
        assert_eq!(text, "one\ntwo\n");
    }

    #[test]
    fn slice_lines_start_out_of_bounds_errors() {
        let f = tmp_with(b"one\n");
        let err = read_slice_lines(f.path(), 5, 10, &TextOptions::default()).unwrap_err();
        assert!(matches!(err, TextError::LineRangeOutOfBounds { .. }));
    }

    #[test]
    fn slice_lines_zero_start_errors() {
        let f = tmp_with(b"one\n");
        let err = read_slice_lines(f.path(), 0, 1, &TextOptions::default()).unwrap_err();
        assert!(matches!(err, TextError::LineRangeOutOfBounds { .. }));
    }

    #[test]
    fn slice_bytes_returns_range() {
        let f = tmp_with(b"hello world");
        let (text, _) = read_slice_bytes(f.path(), 6, 11, &TextOptions::default()).unwrap();
        assert_eq!(text, "world");
    }

    #[test]
    fn slice_bytes_out_of_bounds_errors() {
        let f = tmp_with(b"abc");
        let err = read_slice_bytes(f.path(), 0, 99, &TextOptions::default()).unwrap_err();
        assert!(matches!(err, TextError::ByteRangeOutOfBounds { .. }));
    }

    #[test]
    fn slice_bytes_snaps_to_char_boundary() {
        // "héllo" — the 'é' is two bytes (0xC3 0xA9).
        let f = tmp_with("héllo".as_bytes());
        // Request bytes 1..3 — slicing mid-char would be invalid UTF-8,
        // so the decoder must snap.
        let (text, _) = read_slice_bytes(f.path(), 1, 3, &TextOptions::default()).unwrap();
        assert!(text.starts_with('é'));
    }

    #[test]
    fn missing_file_returns_io_error() {
        let err = read_all(
            Path::new("/nonexistent/ferrite/text/path"),
            &TextOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(err, TextError::Io { .. }));
    }

    #[test]
    fn empty_file_is_empty_text() {
        let f = tmp_with(b"");
        let (text, report) = read_all(f.path(), &TextOptions::default()).unwrap();
        assert_eq!(text, "");
        assert_eq!(report.line_count, 0);
        assert_eq!(report.size_bytes, 0);
    }

    #[test]
    fn count_lines_trailing_partial_counts() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("a"), 1);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb"), 2);
        assert_eq!(count_lines("a\nb\n"), 2);
    }

    #[test]
    fn normalize_handles_mixed_and_unicode() {
        assert_eq!(normalize_newlines("a\r\nb\rcé\n"), "a\nb\ncé\n");
    }
}
