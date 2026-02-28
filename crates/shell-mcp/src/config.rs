//! ferrite user configuration.
//!
//! Config file: ~/.config/ferrite/config.toml
//!
//! Env-var overrides (applied after file load):
//!   FERRITE_TERMINAL_MODE      → terminal.mode
//!   FERRITE_TERMINAL_EMULATOR  → terminal.emulator
//!   FERRITE_VIVADO_PATH        → paths.vivado

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── Sub-structs ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalConfig {
    /// "auto" | "kitty" | "xterm" | "gnome-terminal" | "alacritty"
    pub emulator:  String,
    /// "always" | "never"
    pub mode:      String,
    /// Keep window open after command finishes (read -r pause)
    pub keep_open: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            emulator:  "auto".to_owned(),
            mode:      "never".to_owned(),
            keep_open: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PathsConfig {
    /// Override Vivado bin dir, e.g. /opt/2025.2/Vivado/bin
    pub vivado: String,
}

// ── Top-level config ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FerriteConfig {
    pub terminal: TerminalConfig,
    pub paths:    PathsConfig,
}

impl FerriteConfig {
    /// Load from ~/.config/ferrite/config.toml, then apply env-var overrides.
    /// Missing file → default config (no error).
    pub fn load() -> Self {
        let mut cfg = Self::load_file().unwrap_or_default();
        cfg.apply_env();
        cfg
    }

    fn load_file() -> Option<Self> {
        let path = config_path();
        let text = std::fs::read_to_string(&path).ok()?;
        toml::from_str(&text).ok()
    }

    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("FERRITE_TERMINAL_MODE") {
            self.terminal.mode = v;
        }
        if let Ok(v) = std::env::var("FERRITE_TERMINAL_EMULATOR") {
            self.terminal.emulator = v;
        }
        if let Ok(v) = std::env::var("FERRITE_VIVADO_PATH") {
            self.paths.vivado = v;
        }
    }

    /// Persist current config to ~/.config/ferrite/config.toml.
    pub fn save(&self) -> Result<(), String> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("config: create dir: {e}"))?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| format!("config: serialize: {e}"))?;
        std::fs::write(&path, text)
            .map_err(|e| format!("config: write {}: {e}", path.display()))
    }

    /// Set a value using dot-notation key, e.g. "terminal.mode".
    pub fn set(&mut self, key: &str, val: &str) -> Result<(), String> {
        match key {
            "terminal.emulator"  => self.terminal.emulator  = val.to_owned(),
            "terminal.mode"      => {
                if !matches!(val, "always" | "never") {
                    return Err(format!("terminal.mode must be 'always' or 'never', got '{val}'"));
                }
                self.terminal.mode = val.to_owned();
            }
            "terminal.keep_open" => {
                self.terminal.keep_open = val.parse::<bool>()
                    .map_err(|_| format!("terminal.keep_open must be true or false, got '{val}'"))?;
            }
            "paths.vivado" => self.paths.vivado = val.to_owned(),
            other => return Err(format!("unknown config key '{other}'; valid keys: terminal.emulator, terminal.mode, terminal.keep_open, paths.vivado")),
        }
        Ok(())
    }

    /// Get a value by dot-notation key.
    pub fn get(&self, key: &str) -> Option<String> {
        match key {
            "terminal.emulator"  => Some(self.terminal.emulator.clone()),
            "terminal.mode"      => Some(self.terminal.mode.clone()),
            "terminal.keep_open" => Some(self.terminal.keep_open.to_string()),
            "paths.vivado"       => Some(self.paths.vivado.clone()),
            _                    => None,
        }
    }

    /// Return all key-value pairs.
    pub fn list(&self) -> Vec<(String, String)> {
        vec![
            ("terminal.emulator".to_owned(),  self.terminal.emulator.clone()),
            ("terminal.mode".to_owned(),      self.terminal.mode.clone()),
            ("terminal.keep_open".to_owned(), self.terminal.keep_open.to_string()),
            ("paths.vivado".to_owned(),       self.paths.vivado.clone()),
        ]
    }

    /// Returns true if the terminal observer should open a window for executions.
    pub fn terminal_always(&self) -> bool {
        self.terminal.mode == "always"
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
            PathBuf::from(home).join(".config")
        });
    base.join("ferrite").join("config.toml")
}
