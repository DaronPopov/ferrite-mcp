//! Signal handling setup for the interactive shell.

use nix::sys::signal::{self, SigAction, SigHandler, SaFlags, SigSet, Signal};

/// Install signal handlers appropriate for an interactive shell session.
///
/// - SIGINT  → ignore (Ctrl-C; readline handles it for the foreground line)
/// - SIGQUIT → ignore
/// - SIGTSTP → ignore (Ctrl-Z; we handle job control ourselves)
/// - SIGTTIN → ignore
/// - SIGTTOU → ignore
/// - SIGCHLD → default (we use waitpid in the job table)
pub fn install_interactive_handlers() -> nix::Result<()> {
    let ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());

    unsafe {
        signal::sigaction(Signal::SIGINT,  &ignore)?;
        signal::sigaction(Signal::SIGQUIT, &ignore)?;
        signal::sigaction(Signal::SIGTSTP, &ignore)?;
        signal::sigaction(Signal::SIGTTIN, &ignore)?;
        signal::sigaction(Signal::SIGTTOU, &ignore)?;
        signal::sigaction(Signal::SIGCHLD, &default)?;
    }

    Ok(())
}

/// Restore default signal handlers (used before exec-ing a child process).
pub fn restore_default_handlers() -> nix::Result<()> {
    let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());

    unsafe {
        signal::sigaction(Signal::SIGINT,  &default)?;
        signal::sigaction(Signal::SIGQUIT, &default)?;
        signal::sigaction(Signal::SIGTSTP, &default)?;
        signal::sigaction(Signal::SIGTTIN, &default)?;
        signal::sigaction(Signal::SIGTTOU, &default)?;
    }

    Ok(())
}
