use std::path::PathBuf;

use crate::{env::ShellEnv, types::ExitStatus};

/// Top-level mutable shell state passed through the runtime.
#[derive(Debug)]
pub struct ShellState {
    pub env: ShellEnv,
    pub cwd: PathBuf,
    pub last_status: ExitStatus,
    pub interactive: bool,
    pub opts: ShellOpts,
}

impl ShellState {
    pub fn new(interactive: bool) -> std::io::Result<Self> {
        Ok(Self {
            env: ShellEnv::from_process_env(),
            cwd: std::env::current_dir()?,
            last_status: ExitStatus::SUCCESS,
            interactive,
            opts: ShellOpts::default(),
        })
    }
}

/// Boolean shell options (set -e, set -x, etc.).
#[derive(Debug, Default, Clone)]
pub struct ShellOpts {
    /// Exit on error (set -e).
    pub errexit: bool,
    /// Treat unset variables as errors (set -u).
    pub nounset: bool,
    /// Print commands before executing (set -x).
    pub xtrace: bool,
    /// Disable globbing (set -f).
    pub noglob: bool,
}
