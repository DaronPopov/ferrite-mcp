//! Command history — in-memory ring buffer with optional file persistence.

use std::path::PathBuf;

const DEFAULT_CAPACITY: usize = 10_000;

pub struct History {
    entries: Vec<String>,
    /// Index used during up-arrow navigation (None = not navigating).
    cursor: Option<usize>,
    capacity: usize,
    path: Option<PathBuf>,
}

impl History {
    pub fn new() -> Self {
        Self {
            entries: Vec::with_capacity(256),
            cursor: None,
            capacity: DEFAULT_CAPACITY,
            path: default_history_path(),
        }
    }

    /// Load history from the file (best-effort — silently ignored on error).
    pub fn load(&mut self) {
        if let Some(path) = &self.path {
            if let Ok(contents) = std::fs::read_to_string(path) {
                for line in contents.lines() {
                    if !line.is_empty() {
                        self.push_no_save(line.to_owned());
                    }
                }
            }
        }
    }

    /// Append a line and persist it.
    pub fn push(&mut self, line: String) {
        if line.trim().is_empty() {
            return;
        }
        // De-duplicate consecutive identical entries.
        if self.entries.last().map(String::as_str) == Some(&line) {
            return;
        }
        self.push_no_save(line.clone());
        self.append_to_file(&line);
        self.cursor = None;
    }

    pub fn prev(&mut self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        let idx = match self.cursor {
            None => self.entries.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.cursor = Some(idx);
        Some(&self.entries[idx])
    }

    pub fn next(&mut self) -> Option<&str> {
        match self.cursor {
            None => None,
            Some(i) if i + 1 >= self.entries.len() => {
                self.cursor = None;
                Some("")
            }
            Some(i) => {
                self.cursor = Some(i + 1);
                Some(&self.entries[i + 1])
            }
        }
    }

    pub fn reset_cursor(&mut self) {
        self.cursor = None;
    }

    fn push_no_save(&mut self, line: String) {
        if self.entries.len() >= self.capacity {
            self.entries.remove(0);
        }
        self.entries.push(line);
    }

    fn append_to_file(&self, line: &str) {
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(f, "{line}");
            }
        }
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

fn default_history_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".local/share/ferrite/history"))
}
