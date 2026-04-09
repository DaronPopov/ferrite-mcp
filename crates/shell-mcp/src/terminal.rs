//! Shared terminal-launcher utility.
//!
//! Used by `bg_window` (live_window tool) and `execution` (terminal observer).
//!
//! Emulator preference:
//!   "auto"  → tries kitty, then xterm
//!   otherwise uses the named emulator directly

use std::path::Path;
use std::process::{Command, Stdio};

/// Launch a terminal window running `shell_cmd`.
///
/// * `title`      — window title string
/// * `shell_cmd`  — command passed to `/bin/sh -c`
/// * `emulator`   — "auto" | "kitty" | "xterm" | "gnome-terminal" | "alacritty"
///
/// Returns the terminal process PID.
pub fn launch_terminal(title: &str, shell_cmd: &str, emulator: &str) -> Result<u32, String> {
    let ordered: Vec<&str> = if emulator == "auto" {
        vec!["kitty", "xterm", "gnome-terminal", "alacritty"]
    } else {
        vec![emulator]
    };

    for name in &ordered {
        if let Some(bin) = which_bin(name) {
            return do_spawn(title, shell_cmd, name, &bin);
        }
    }

    Err(format!(
        "terminal: no suitable terminal found (tried {}). Install kitty or xterm.",
        ordered.join(", ")
    ))
}

fn do_spawn(title: &str, shell_cmd: &str, name: &str, bin: &str) -> Result<u32, String> {
    let args: Vec<&str> = match name {
        "kitty" => vec!["--title", title, "--", "/bin/sh", "-c", shell_cmd],
        "xterm" => vec!["-title", title, "-e", "/bin/sh", "-c", shell_cmd],
        "alacritty" => vec!["--title", title, "-e", "/bin/sh", "-c", shell_cmd],
        "gnome-terminal" => vec!["--title", title, "--", "/bin/sh", "-c", shell_cmd],
        _ => vec!["-e", "/bin/sh", "-c", shell_cmd],
    };

    spawn_detached(bin, &args)
}

pub fn spawn_detached(bin: &str, args: &[&str]) -> Result<u32, String> {
    let child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("terminal: failed to spawn '{bin}': {e}"))?;

    let pid = child.id();
    std::mem::drop(child); // detach
    Ok(pid)
}

pub fn which_bin(name: &str) -> Option<String> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    path_var
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| std::path::PathBuf::from(d).join(name))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .map(|p| p.display().to_string())
}

/// Build the shell command string that tails a log file with colorized awk output.
///
/// The awk script:
/// - Colorizes output by pattern (errors=red, warnings=yellow, progress=green, info=cyan)
/// - Includes EDA-aware patterns for Vivado synthesis, cocotb, cargo, gcc, and Python
/// - Watches for `FERRITE_DONE:{code}` sentinel and prints a success/failure banner
///
/// `log`       — path to the log file to tail
/// `keep_open` — if true, pauses with "press enter to close" after the banner
///
/// The shell script also writes `$$` to a `.pid` sidecar file so `close_observer`
/// can SIGTERM the shell and close the window programmatically.
///
/// # Why a FIFO?
///
/// `tail -f | awk` hangs after awk exits: tail is blocked in inotify_wait and
/// won't write again, so it never receives SIGPIPE and the pipeline never drains.
/// The fix: run tail into a named pipe (FIFO) in the background, then kill it
/// explicitly once awk returns. The EXIT trap ensures cleanup even on SIGTERM.
pub fn colorized_watch_cmd(log: &Path, keep_open: bool) -> String {
    // awk script — no single quotes inside (safe to shell-single-quote the whole program).
    // Patterns cover cargo, gcc, Python, cocotb, Vivado synthesis, and generic scripts.
    // ✓ / ✗ are literal UTF-8 bytes; all awk implementations pass them through unchanged.
    let awk = r#"BEGIN {
  OFS=""
  R="\033[31m"; Y="\033[33m"; G="\033[32m"; C="\033[36m"
  B="\033[1m";  X="\033[0m"
  printf "\033[2J\033[H"
}
/^FERRITE_DONE:/ {
  code = substr($0, 14)
  if (code+0 == 0) { printf "\n" B G "  ✓  success" X "\n\n" }
  else             { printf "\n" B R "  ✗  failed  (exit " code ")" X "\n\n" }
  exit
}
/[Ee]rror[: (\[]|^error$|ERROR[: ]|FAILED?[: ]|panic[: !]|fatal[: ]|CRITICAL WARNING|AssertionError|Timing requirements are NOT met|ASSERTION FAILED/ {
  print R $0 X; next
}
/[Ww]arning[: (\[]|WARN[: ]|deprecated/ {
  print Y $0 X; next
}
/^[[:space:]]*(Finished|Compiling|Linking|Building|Replaced|Running)[[:space:]]|PASSED|^\[100%\]|Synthesizing|Synthesized|Timing requirements are met|Place Design|Route Design|Opt Design|Power Opt Design|Write.*checkpoint|report_timing_summary/ {
  print G $0 X; next
}
/^[[:space:]]*(note|hint|INFO)[: ]|^[[:space:]]*WNS:|^[[:space:]]*TNS:|Phase [0-9]/ {
  print C $0 X; next
}
{ print }"#;

    let log_str = log.display();
    let pid_file = format!("{log_str}.pid");

    let pause = if keep_open {
        "\necho '  press enter to close'\nread -r"
    } else {
        ""
    };

    // On macOS, `tail -F` (capital F) follows renamed/rotated files — equivalent
    // to GNU coreutils `tail -f --retry` on Linux.
    #[cfg(target_os = "linux")]
    let tail_flags = "-f --retry";
    #[cfg(not(target_os = "linux"))]
    let tail_flags = "-F";

    // Shell script design notes:
    //
    // Problem: `tail -f | awk` hangs after awk exits. tail blocks in inotify_wait
    // for more log content; it never writes again, never gets SIGPIPE, so the
    // pipeline never terminates and the shell never reaches the pause/cleanup step.
    //
    // Fix: run tail into a named FIFO in the background, capture its PID,
    // then kill it explicitly once awk returns. The EXIT trap handles cleanup in
    // all cases: normal completion, SIGTERM from close_observer, SIGINT (Ctrl-C).
    //
    // Variable names use _C_ prefix to avoid colliding with user env vars.
    // tail -n +1  starts from the first line so bg jobs opened mid-run show full history.
    format!(
        "echo $$ > '{pid_file}'\n\
         _C_F=$(mktemp -u /tmp/ferrite_XXXXXX)\n\
         mkfifo \"$_C_F\"\n\
         tail -n +1 {tail_flags} '{log_str}' > \"$_C_F\" &\n\
         _C_T=$!\n\
         trap 'kill \"$_C_T\" 2>/dev/null; rm -f \"$_C_F\"' EXIT\n\
         awk '{awk}' < \"$_C_F\"\
         {pause}\n\
         rm -f '{pid_file}'",
        awk = awk,
        pause = pause,
        tail_flags = tail_flags,
    )
}
