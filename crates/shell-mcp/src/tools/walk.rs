//! Unified filesystem walker.
//!
//! This is the single traversal layer used by every read/search/mutation
//! tool in ferrite. It replaces the five hand-rolled recursive walkers
//! that previously lived in `filesystem.rs`, `code.rs` (the rust-fallback
//! path), `changed_since`, and `find_rust_references`.
//!
//! Built on the `ignore` crate (ripgrep's building block) so we get
//! gitignore / global-ignore / `.ferriteignore` / hidden-file handling
//! and parallel walking for free.
//!
//! ## Filter model
//!
//! Two filter stages apply to every walk:
//!
//! 1. **Prune via `filter_entry`** — `exclude` globset patterns are
//!    evaluated against the *relative* path (relative to the walk root)
//!    for every entry. A matching directory is not descended into, a
//!    matching file is not yielded. This is where you cut out whole
//!    subtrees.
//!
//! 2. **Post-yield include filter** — `include` globset patterns are
//!    evaluated against the relative path of each yielded entry. If the
//!    include set is non-empty, only files whose relative path matches
//!    are yielded. Directories are always yielded regardless of include
//!    so that the walker can still descend and discover matches beneath
//!    them.
//!
//! ## Relative-path matching
//!
//! All glob matching runs against the **relative** path (root-stripped).
//! This means `src/*.rs` matches `<root>/src/foo.rs` as users expect,
//! without needing `**/` prefixes. For patterns that should match any
//! depth, use the standard `**/*.rs` form.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::{WalkBuilder, WalkState as IgnoreWalkState};

// ── Public types ─────────────────────────────────────────────────────────────

/// Options controlling a single walk. All fields have sensible defaults
/// via `WalkOptions::new(root)`.
#[derive(Debug, Clone)]
pub struct WalkOptions {
    pub root: PathBuf,
    /// `None` = unlimited. Depth 0 is the root itself.
    pub max_depth: Option<usize>,
    pub follow_links: bool,
    /// When true, hidden (dotfile) entries are included. Default: false.
    pub hidden: bool,
    /// When true, `.gitignore` / global gitignore / `.git/info/exclude`
    /// are honoured. Default: true.
    pub respect_gitignore: bool,
    /// When true, `.ferriteignore` files in the walked tree are honoured.
    /// Default: true.
    pub respect_ferriteignore: bool,
    /// Additional explicit ignore files (read as gitignore-style rules).
    pub custom_ignore_files: Vec<PathBuf>,
    /// Glob patterns a file must match to be yielded (evaluated against
    /// the relative path). Empty = include everything.
    pub include: Vec<String>,
    /// Glob patterns that prune whole subtrees (evaluated against the
    /// relative path). Empty = prune nothing.
    pub exclude: Vec<String>,
    /// Parallel walker thread count. `None` = ignore crate's default.
    pub threads: Option<usize>,
    /// Skip files strictly larger than this byte count. `None` = no limit.
    pub max_filesize: Option<u64>,
}

impl WalkOptions {
    pub fn new<P: Into<PathBuf>>(root: P) -> Self {
        Self {
            root: root.into(),
            ..Self::default()
        }
    }
}

impl Default for WalkOptions {
    /// Defaults: walk `.`, no depth limit, don't follow symlinks, skip
    /// hidden files, respect gitignore and ferriteignore, no include /
    /// exclude filters, no file-size cap.
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            max_depth: None,
            follow_links: false,
            hidden: false,
            respect_gitignore: true,
            respect_ferriteignore: true,
            custom_ignore_files: Vec::new(),
            include: Vec::new(),
            exclude: Vec::new(),
            threads: None,
            max_filesize: None,
        }
    }
}

/// One entry yielded by the walker.
#[derive(Debug, Clone)]
pub struct WalkEntry {
    /// Absolute (canonicalized when possible) path of the entry.
    pub path: PathBuf,
    /// Path relative to the walk root. Useful for user-visible output.
    pub relative: PathBuf,
    /// Depth below the root. The root itself has depth 0.
    pub depth: usize,
    pub file_type: FileType,
    /// File size in bytes. Zero for directories and entries whose
    /// metadata could not be read.
    pub size: u64,
    /// Last-modified time. Falls back to UNIX_EPOCH if unknown.
    pub mtime: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Dir,
    Symlink,
    Other,
}

impl FileType {
    pub fn is_file(self) -> bool {
        matches!(self, FileType::File)
    }
    pub fn is_dir(self) -> bool {
        matches!(self, FileType::Dir)
    }
    pub fn is_symlink(self) -> bool {
        matches!(self, FileType::Symlink)
    }
}

/// Parallel walker control signal — what should happen after this entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkState {
    /// Keep walking normally.
    Continue,
    /// Skip this entry's subtree (directories only; no-op for files).
    Skip,
    /// Stop the walk as soon as possible.
    Stop,
}

/// Errors that can occur while building or running a walk.
#[derive(Debug)]
pub enum WalkError {
    /// A glob pattern in `include` / `exclude` failed to compile.
    InvalidPattern { pattern: String, reason: String },
    /// The globset builder rejected the combined pattern set.
    GlobSet(String),
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkError::InvalidPattern { pattern, reason } => {
                write!(f, "invalid glob pattern {pattern:?}: {reason}")
            }
            WalkError::GlobSet(msg) => write!(f, "globset build error: {msg}"),
        }
    }
}

impl std::error::Error for WalkError {}

// ── Sequential walker ────────────────────────────────────────────────────────

/// Iterator returned by `walk_sequential`. Holds the underlying
/// `ignore::Walk`, the compiled include globset, and the canonical walk
/// root for relative-path computation.
pub struct WalkIter {
    inner: ignore::Walk,
    include: GlobSet,
    root: PathBuf,
}

impl Iterator for WalkIter {
    type Item = WalkEntry;

    fn next(&mut self) -> Option<WalkEntry> {
        loop {
            let dent = match self.inner.next()? {
                Ok(d) => d,
                // Walk errors (permission denied on a subdir, broken
                // symlink, etc.) are silently skipped — we never want
                // one bad entry to break the whole traversal.
                Err(_) => continue,
            };
            let entry = match to_walk_entry(&dent, &self.root) {
                Some(e) => e,
                None => continue,
            };
            // Directories always pass the include filter so the walker
            // can still descend into them.
            if !entry.file_type.is_dir()
                && !self.include.is_empty()
                && !self.include.is_match(&entry.relative)
            {
                continue;
            }
            return Some(entry);
        }
    }
}

/// Walk a filesystem tree sequentially, honouring all filters.
///
/// Returns an iterator that skips errored entries internally. Use
/// `walk_parallel` when you care about throughput on large trees.
pub fn walk_sequential(opts: &WalkOptions) -> Result<WalkIter, WalkError> {
    let (builder, include, root) = build_from_options(opts)?;
    Ok(WalkIter {
        inner: builder.build(),
        include,
        root,
    })
}

// ── Parallel walker ──────────────────────────────────────────────────────────

/// Walk a filesystem tree in parallel, invoking `on_entry` for every
/// entry that passes the filters. The callback runs on worker threads
/// and must be `Sync + Send + 'static`.
///
/// Returning `WalkState::Stop` from the callback aborts the walk as
/// soon as possible; `WalkState::Skip` prunes a directory's subtree;
/// `WalkState::Continue` proceeds normally.
pub fn walk_parallel<F>(opts: &WalkOptions, on_entry: F) -> Result<(), WalkError>
where
    F: Fn(WalkEntry) -> WalkState + Sync + Send + 'static,
{
    let (builder, include, root) = build_from_options(opts)?;
    let parallel = builder.build_parallel();
    let include = Arc::new(include);
    let root = Arc::new(root);
    let on_entry = Arc::new(on_entry);

    parallel.run(|| {
        let include = Arc::clone(&include);
        let root = Arc::clone(&root);
        let on_entry = Arc::clone(&on_entry);
        Box::new(move |result| {
            let dent = match result {
                Ok(d) => d,
                Err(_) => return IgnoreWalkState::Continue,
            };
            let Some(entry) = to_walk_entry(&dent, &root) else {
                return IgnoreWalkState::Continue;
            };
            if !entry.file_type.is_dir()
                && !include.is_empty()
                && !include.is_match(&entry.relative)
            {
                return IgnoreWalkState::Continue;
            }
            match on_entry(entry) {
                WalkState::Continue => IgnoreWalkState::Continue,
                WalkState::Skip => IgnoreWalkState::Skip,
                WalkState::Stop => IgnoreWalkState::Quit,
            }
        })
    });

    Ok(())
}

// ── Convenience collector ────────────────────────────────────────────────────

/// Parallel walk that collects every entry into a `Vec`. Entries are
/// returned in an unspecified order — sort if you need determinism.
pub fn walk_parallel_collect(opts: &WalkOptions) -> Result<Vec<WalkEntry>, WalkError> {
    let collected: Arc<Mutex<Vec<WalkEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let out = Arc::clone(&collected);
    walk_parallel(opts, move |entry| {
        if let Ok(mut guard) = out.lock() {
            guard.push(entry);
        }
        WalkState::Continue
    })?;
    Ok(collected.lock().map(|g| g.clone()).unwrap_or_default())
}

// ── Internals ────────────────────────────────────────────────────────────────

/// Build a `WalkBuilder` from a `WalkOptions`, compile the include set,
/// and derive the canonical root used for relative-path computation.
///
/// Exclude patterns are consumed here and installed as a `filter_entry`
/// predicate so they prune whole subtrees instead of merely filtering
/// yielded entries.
fn build_from_options(opts: &WalkOptions) -> Result<(WalkBuilder, GlobSet, PathBuf), WalkError> {
    // Canonicalize best-effort so DirEntry paths share a stable prefix
    // with the root we use for strip_prefix.
    let root = opts
        .root
        .canonicalize()
        .unwrap_or_else(|_| opts.root.clone());

    let include = build_globset(&opts.include)?;
    let exclude = build_globset(&opts.exclude)?;

    let mut b = WalkBuilder::new(&root);
    b.max_depth(opts.max_depth)
        .follow_links(opts.follow_links)
        // ignore::WalkBuilder::hidden(true) means "skip hidden"; our
        // `hidden: true` means "include hidden", so the flag is negated.
        .hidden(!opts.hidden)
        .max_filesize(opts.max_filesize);

    if !opts.respect_gitignore {
        b.git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .parents(false);
    }

    if opts.respect_ferriteignore {
        b.add_custom_ignore_filename(".ferriteignore");
    }

    for path in &opts.custom_ignore_files {
        // add_ignore returns an Option<Error> on failure — we
        // deliberately do not fail the walk for a missing/bad ignore
        // file, since per-project overrides are best-effort.
        let _ = b.add_ignore(path);
    }

    if let Some(n) = opts.threads {
        b.threads(n);
    }

    if !exclude.is_empty() {
        let exclude_for_filter = exclude.clone();
        let root_for_filter = root.clone();
        b.filter_entry(move |dent| {
            let rel = dent
                .path()
                .strip_prefix(&root_for_filter)
                .unwrap_or_else(|_| dent.path());
            !exclude_for_filter.is_match(rel)
        });
    }

    Ok((b, include, root))
}

/// Compile a vec of glob patterns into a `GlobSet`. Empty input yields
/// the empty set (matches nothing, has `is_empty() == true`).
fn build_globset(patterns: &[String]) -> Result<GlobSet, WalkError> {
    if patterns.is_empty() {
        return Ok(GlobSet::empty());
    }
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let glob = Glob::new(pat).map_err(|e| WalkError::InvalidPattern {
            pattern: pat.clone(),
            reason: e.to_string(),
        })?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| WalkError::GlobSet(e.to_string()))
}

/// Translate an `ignore::DirEntry` into our richer `WalkEntry`. Returns
/// `None` only for truly degenerate entries (no file type, which should
/// be unreachable for filesystem walks).
fn to_walk_entry(dent: &ignore::DirEntry, root: &Path) -> Option<WalkEntry> {
    let path = dent.path().to_path_buf();
    let relative = path
        .strip_prefix(root)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| path.clone());

    let ft = dent.file_type()?;
    let file_type = if ft.is_file() {
        FileType::File
    } else if ft.is_dir() {
        FileType::Dir
    } else if ft.is_symlink() {
        FileType::Symlink
    } else {
        FileType::Other
    };

    let meta = dent.metadata().ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH);

    Some(WalkEntry {
        path,
        relative,
        depth: dent.depth(),
        file_type,
        size,
        mtime,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    /// Create a fixture tree from `(relative_path, content)` tuples.
    /// Parent directories are created automatically.
    fn make_tree(root: &Path, files: &[(&str, &str)]) {
        for (rel, content) in files {
            let p = root.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let mut f = fs::File::create(&p).unwrap();
            f.write_all(content.as_bytes()).unwrap();
        }
    }

    /// Collect every yielded entry's relative path as a sorted vec of
    /// forward-slash strings, for deterministic assertion.
    fn collect_rel(opts: &WalkOptions) -> Vec<String> {
        let mut v: Vec<String> = walk_sequential(opts)
            .unwrap()
            .map(|e| e.relative.to_string_lossy().replace('\\', "/"))
            .collect();
        v.sort();
        v
    }

    fn collect_rel_parallel(opts: &WalkOptions) -> Vec<String> {
        let entries = walk_parallel_collect(opts).unwrap();
        let mut v: Vec<String> = entries
            .into_iter()
            .map(|e| e.relative.to_string_lossy().replace('\\', "/"))
            .collect();
        v.sort();
        v
    }

    // ── Fixture builders ─────────────────────────────────────────────────────

    fn basic_tree() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        make_tree(
            dir.path(),
            &[
                (".git/HEAD", "ref: refs/heads/main\n"),
                (".gitignore", "target\n*.log\n"),
                ("Cargo.toml", "[package]\nname = \"x\"\n"),
                ("src/main.rs", "fn main() {}\n"),
                ("src/lib.rs", "// lib\n"),
                ("src/bin.rs", "// bin\n"),
                ("target/debug/binary", "binary\n"),
                ("target/release/binary", "binary\n"),
                ("docs/readme.md", "# docs\n"),
                ("docs/.hidden.txt", "hidden\n"),
                ("build.log", "log line\n"),
            ],
        );
        dir
    }

    // ── Basic traversal + gitignore respect ──────────────────────────────────

    #[test]
    fn walks_tree_and_respects_gitignore_by_default() {
        let dir = basic_tree();
        let opts = WalkOptions::new(dir.path());
        let paths = collect_rel(&opts);

        // `target/` is gitignored, `build.log` matches `*.log`, and
        // `.git/` is always skipped. Hidden dotfiles are off by default.
        assert!(paths.iter().any(|p| p == "src/main.rs"));
        assert!(paths.iter().any(|p| p == "src/lib.rs"));
        assert!(paths.iter().any(|p| p == "Cargo.toml"));
        assert!(paths.iter().any(|p| p == "docs/readme.md"));
        assert!(!paths.iter().any(|p| p.starts_with("target")));
        assert!(!paths.iter().any(|p| p == "build.log"));
        assert!(!paths.iter().any(|p| p.starts_with(".git")));
        assert!(!paths.iter().any(|p| p.contains(".hidden.txt")));
    }

    #[test]
    fn disabling_gitignore_exposes_target() {
        let dir = basic_tree();
        let mut opts = WalkOptions::new(dir.path());
        opts.respect_gitignore = false;
        let paths = collect_rel(&opts);
        assert!(paths.iter().any(|p| p == "target/debug/binary"));
        assert!(paths.iter().any(|p| p == "build.log"));
    }

    #[test]
    fn hidden_flag_exposes_dotfiles() {
        let dir = basic_tree();
        let mut opts = WalkOptions::new(dir.path());
        opts.hidden = true;
        let paths = collect_rel(&opts);
        // docs/.hidden.txt is not gitignored so it should surface once
        // hidden files are allowed.
        assert!(paths.iter().any(|p| p == "docs/.hidden.txt"));
    }

    // ── .ferriteignore ───────────────────────────────────────────────────────

    #[test]
    fn respects_ferriteignore() {
        let dir = basic_tree();
        fs::write(dir.path().join(".ferriteignore"), "docs\n").unwrap();

        let opts = WalkOptions::new(dir.path());
        let paths = collect_rel(&opts);
        assert!(!paths.iter().any(|p| p.starts_with("docs")));
        assert!(paths.iter().any(|p| p == "src/main.rs"));
    }

    #[test]
    fn can_disable_ferriteignore() {
        let dir = basic_tree();
        fs::write(dir.path().join(".ferriteignore"), "docs\n").unwrap();

        let mut opts = WalkOptions::new(dir.path());
        opts.respect_ferriteignore = false;
        let paths = collect_rel(&opts);
        assert!(paths.iter().any(|p| p == "docs/readme.md"));
    }

    // ── max_depth ────────────────────────────────────────────────────────────

    #[test]
    fn max_depth_limits_traversal() {
        let dir = basic_tree();
        let mut opts = WalkOptions::new(dir.path());
        opts.max_depth = Some(1);
        let paths = collect_rel(&opts);

        // Depth 0 = root, depth 1 = immediate children. Nothing under
        // src/ or docs/ should appear.
        assert!(paths.iter().any(|p| p == "Cargo.toml"));
        assert!(paths.iter().any(|p| p == "src"));
        assert!(paths.iter().any(|p| p == "docs"));
        assert!(!paths.iter().any(|p| p == "src/main.rs"));
        assert!(!paths.iter().any(|p| p == "docs/readme.md"));
    }

    // ── Include / exclude globsets ───────────────────────────────────────────

    #[test]
    fn include_filters_to_rust_files() {
        let dir = basic_tree();
        let mut opts = WalkOptions::new(dir.path());
        opts.include = vec!["**/*.rs".into()];
        let paths: Vec<_> = collect_rel(&opts)
            .into_iter()
            // Directories still pass through the include filter; drop
            // them for the assertion so we only check file yields.
            .filter(|p| p.ends_with(".rs"))
            .collect();

        assert_eq!(
            paths,
            vec![
                "src/bin.rs".to_string(),
                "src/lib.rs".into(),
                "src/main.rs".into(),
            ]
        );
    }

    #[test]
    fn include_accepts_brace_expansion() {
        let dir = basic_tree();
        let mut opts = WalkOptions::new(dir.path());
        opts.include = vec!["**/*.{rs,toml}".into()];

        let mut files: Vec<_> = collect_rel(&opts)
            .into_iter()
            .filter(|p| p.ends_with(".rs") || p.ends_with(".toml"))
            .collect();
        files.sort();
        assert_eq!(
            files,
            vec![
                "Cargo.toml".to_string(),
                "src/bin.rs".into(),
                "src/lib.rs".into(),
                "src/main.rs".into(),
            ]
        );
    }

    #[test]
    fn exclude_prunes_subtree() {
        let dir = basic_tree();
        let mut opts = WalkOptions::new(dir.path());
        opts.exclude = vec!["docs/**".into(), "docs".into()];
        let paths = collect_rel(&opts);
        assert!(!paths.iter().any(|p| p.starts_with("docs")));
        assert!(paths.iter().any(|p| p == "src/main.rs"));
    }

    #[test]
    fn invalid_pattern_is_reported() {
        let dir = basic_tree();
        let mut opts = WalkOptions::new(dir.path());
        // Unbalanced `[` is rejected by globset.
        opts.include = vec!["src/[".into()];
        let err = walk_sequential(&opts).err().expect("must fail");
        match err {
            WalkError::InvalidPattern { pattern, .. } => assert_eq!(pattern, "src/["),
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    // ── max_filesize ─────────────────────────────────────────────────────────

    #[test]
    fn max_filesize_skips_large_files() {
        let dir = tempfile::tempdir().unwrap();
        make_tree(
            dir.path(),
            &[
                ("small.txt", "x"),
                // 2 KiB — larger than the limit we'll set below.
                ("big.bin", ""),
            ],
        );
        // Overwrite big.bin with 2 KiB of data.
        fs::write(dir.path().join("big.bin"), vec![0u8; 2048]).unwrap();

        let mut opts = WalkOptions::new(dir.path());
        opts.max_filesize = Some(1024);
        let paths = collect_rel(&opts);
        assert!(paths.iter().any(|p| p == "small.txt"));
        assert!(!paths.iter().any(|p| p == "big.bin"));
    }

    // ── Parallel vs sequential equivalence ───────────────────────────────────

    #[test]
    fn parallel_yields_same_set_as_sequential() {
        let dir = basic_tree();
        let opts = WalkOptions::new(dir.path());
        let seq = collect_rel(&opts);
        let par = collect_rel_parallel(&opts);
        assert_eq!(seq, par);
    }

    #[test]
    fn parallel_with_filters_matches_sequential() {
        let dir = basic_tree();
        let mut opts = WalkOptions::new(dir.path());
        opts.include = vec!["**/*.rs".into()];
        opts.exclude = vec!["docs".into(), "docs/**".into()];
        let seq: Vec<_> = collect_rel(&opts)
            .into_iter()
            .filter(|p| p.ends_with(".rs"))
            .collect();
        let par: Vec<_> = collect_rel_parallel(&opts)
            .into_iter()
            .filter(|p| p.ends_with(".rs"))
            .collect();
        assert_eq!(seq, par);
    }

    // ── WalkState::Stop ──────────────────────────────────────────────────────

    #[test]
    fn parallel_stop_aborts_walk() {
        let dir = basic_tree();
        let opts = WalkOptions::new(dir.path());

        use std::sync::atomic::{AtomicUsize, Ordering};
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        walk_parallel(&opts, move |_entry| {
            // Return Stop after the first entry — the walker should
            // wind down quickly even if a few more entries race through
            // on other threads.
            c.fetch_add(1, Ordering::Relaxed);
            WalkState::Stop
        })
        .unwrap();

        // At least one entry was processed; the walk terminated.
        assert!(count.load(Ordering::Relaxed) >= 1);
    }

    // ── Metadata ─────────────────────────────────────────────────────────────

    #[test]
    fn entries_carry_size_and_file_type() {
        let dir = basic_tree();
        let opts = WalkOptions::new(dir.path());
        let entries: Vec<WalkEntry> = walk_sequential(&opts).unwrap().collect();

        let cargo = entries
            .iter()
            .find(|e| e.relative == Path::new("Cargo.toml"))
            .expect("Cargo.toml missing");
        assert!(cargo.file_type.is_file());
        assert!(cargo.size > 0);

        let src = entries
            .iter()
            .find(|e| e.relative == Path::new("src"))
            .expect("src dir missing");
        assert!(src.file_type.is_dir());
    }

    // ── Root canonicalization ────────────────────────────────────────────────

    #[test]
    fn relative_paths_are_root_relative() {
        let dir = basic_tree();
        let opts = WalkOptions::new(dir.path());
        for entry in walk_sequential(&opts).unwrap() {
            // Every yielded entry should have a relative path that is
            // either empty (root) or does not start with the walk root.
            assert!(!entry.relative.starts_with(dir.path()));
        }
    }
}
