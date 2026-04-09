use shell_core::types::ExitStatus;

/// Events emitted by the shell runtime that hooks may observe or modify.
#[derive(Debug, Clone)]
pub enum HookEvent {
    /// Fired just before a command string is executed.
    PreExec {
        /// The raw command line string as typed by the user.
        cmdline: String,
    },

    /// Fired after a command finishes executing.
    PostExec { cmdline: String, status: ExitStatus },

    /// Fired when command lookup fails (command not found).
    CommandNotFound { name: String },

    /// Fired when any shell error occurs.
    OnError { message: String },

    /// Fired when the tab-completion system requests completions.
    ///
    /// Hooks may push additional entries into `completions`.
    Complete {
        /// The partial input up to the cursor.
        input: String,
        /// Cursor position (byte offset into `input`).
        cursor: usize,
        /// Completion candidates populated by the hook.
        completions: Vec<String>,
    },

    /// Fired on shell startup, after state is initialized.
    Startup,

    /// Fired just before the shell exits.
    Shutdown { status: ExitStatus },
}
