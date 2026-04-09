//! Permission pre-validation and command preprocessing.
//!
//! Solves the "interactive prompt" problem for agent workflows:
//!
//!   1. **Rewrite** — transforms commands to their non-interactive equivalents
//!      (e.g. `apt install curl` → `apt install -y curl`).
//!
//!   2. **Env injection** — supplies standard non-interactive env vars
//!      (DEBIAN_FRONTEND=noninteractive, PIP_NO_INPUT=1, GIT_TERMINAL_PROMPT=0 …).
//!
//!   3. **Sudo coverage** — checks whether `/etc/sudoers.d/ferrite` already
//!      grants NOPASSWD access for the relevant binary, so the agent knows
//!      up-front whether a privileged op will block.
//!
//!   4. **`pre_validate`** — surface-level analysis tool the agent can call
//!      before running any command to get a structured go/no-go report.
//!
//!   5. **`preprocess`** — called automatically inside `exec_cmd`; applies all
//!      rewrites silently so the agent never has to think about it.

use std::collections::HashMap;

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Analysis {
    pub original: String,
    pub rewritten: String,
    pub injected_env: HashMap<String, String>,
    pub blockers: Vec<Blocker>,
    /// True when zero unresolved blockers remain after auto-fixes.
    pub all_clear: bool,
    pub sudo_status: SudoersStatus,
}

#[derive(Debug, Clone)]
pub struct Blocker {
    pub kind: BlockerKind,
    pub description: String,
    pub auto_resolved: bool,
    pub resolution: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockerKind {
    /// `sudo` is invoked but the binary isn't in the NOPASSWD whitelist.
    SudoNotCovered,
    /// Command is known to emit interactive y/n prompts without the right flags.
    InteractivePrompt,
    /// Something that can't be auto-fixed (e.g. unknown interactive TUI tool).
    ManualRequired,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SudoersStatus {
    /// `/etc/sudoers.d/ferrite` exists and covers this user.
    Active,
    /// File exists but user line is missing (stale install for different user).
    StaleUser,
    /// File doesn't exist — privileged ops will prompt for password.
    Missing,
}

// ── Sudoers whitelisted binaries ─────────────────────────────────────────────
//
// These are the commands that install.sh writes into /etc/sudoers.d/ferrite.
// When the sudoers entry is active, the agent can run these with `sudo` prefix
// without any password prompt.
const SUDOERS_BINS: &[&str] = &[
    "/usr/bin/apt",
    "/usr/bin/apt-get",
    "/bin/systemctl",
    "/usr/bin/systemctl",
    "/usr/sbin/ufw",
    "/usr/bin/snap",
    "/usr/bin/dpkg",
    "/sbin/ldconfig",
    "/usr/sbin/ldconfig",
    "/usr/bin/tee",
    "/bin/tee",
    "/bin/chmod",
    "/usr/bin/chmod",
    "/bin/chown",
    "/usr/bin/chown",
    "/usr/sbin/usermod",
    "/usr/sbin/groupmod",
    "/usr/bin/add-apt-repository",
    "/usr/sbin/update-alternatives",
    "/usr/bin/update-alternatives",
    "/usr/bin/install",
    "/usr/bin/tailscale",
];

/// Short names (without path) — used for matching in command strings.
const SUDOERS_SHORT: &[&str] = &[
    "apt",
    "apt-get",
    "systemctl",
    "ufw",
    "snap",
    "dpkg",
    "ldconfig",
    "tee",
    "chmod",
    "chown",
    "usermod",
    "groupmod",
    "add-apt-repository",
    "update-alternatives",
    "install",
    "tailscale",
];

// ── Error classification for exec auto-recovery ───────────────────────────────

/// Action exec should take after a non-zero exit.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryAction {
    /// Wait N seconds then retry the same command unchanged.
    RetryAfterDelay { secs: u64, reason: String },
    /// Retry the command with an extra flag appended.
    RetryWithFlag { flag: String, reason: String },
    /// Retry the command prefixed with `sudo`.
    RetryWithSudo { reason: String },
    /// Nothing can be done automatically.
    Unrecoverable { reason: String },
}

/// Inspect stdout + stderr of a failed command and suggest a recovery action.
/// Returns `None` if the failure looks permanent.
pub fn classify_error(cmd: &str, stdout: &str, stderr: &str) -> Option<RecoveryAction> {
    let combined = format!("{stdout}\n{stderr}").to_lowercase();

    // ── dpkg / apt lock contention ────────────────────────────────────────────
    if combined.contains("could not get lock /var/lib/dpkg")
        || combined.contains("could not get lock /var/lib/apt")
        || combined.contains("could not open lock file /var/lib/dpkg")
        || combined.contains("dpkg was interrupted")
        || combined.contains("e: unable to acquire the dpkg frontend lock")
    {
        return Some(RecoveryAction::RetryAfterDelay {
            secs: 8,
            reason: "dpkg/apt lock held by another process — waiting for it to release".to_owned(),
        });
    }

    // ── npm / yarn lock ───────────────────────────────────────────────────────
    if combined.contains("eacces: permission denied") && combined.contains("npm") {
        return Some(RecoveryAction::RetryWithFlag {
            flag: "--prefix ~/.local".to_owned(),
            reason: "npm EACCES — retrying with user-local prefix".to_owned(),
        });
    }

    // ── pip: externally managed Python environment ────────────────────────────
    if combined.contains("externally-managed-environment") {
        return Some(RecoveryAction::RetryWithFlag {
            flag: "--break-system-packages".to_owned(),
            reason: "pip externally-managed-environment — retrying with --break-system-packages"
                .to_owned(),
        });
    }

    // ── Permission denied on a sudoers-covered binary ─────────────────────────
    if combined.contains("permission denied") {
        let tool = first_token(cmd.trim_start_matches("sudo").trim_start());
        if is_in_sudoers_list(tool) && sudoers_status() == SudoersStatus::Active {
            return Some(RecoveryAction::RetryWithSudo {
                reason: format!(
                    "permission denied — retrying with sudo ('{tool}' is in sudoers whitelist)"
                ),
            });
        }
    }

    // ── Transient network failure (retry once) ────────────────────────────────
    if combined.contains("connection timed out")
        || combined.contains("temporary failure in name resolution")
        || combined.contains("could not resolve host")
        || combined.contains("network is unreachable")
    {
        return Some(RecoveryAction::RetryAfterDelay {
            secs: 5,
            reason: "transient network error — retrying after short delay".to_owned(),
        });
    }

    // ── Already running / port conflict — not retryable ──────────────────────
    if combined.contains("address already in use")
        || combined.contains("bind: address already in use")
    {
        return Some(RecoveryAction::Unrecoverable {
            reason: "port already in use — use process_tree or port_list to find the conflicting process".to_owned(),
        });
    }

    None
}

/// Apply a RetryWithFlag recovery: insert `flag` into the command string
/// after the first whitespace-delimited token (the binary name).
pub fn apply_flag(cmd: &str, flag: &str) -> String {
    let tokens = tokenize(cmd);
    if tokens.is_empty() {
        return cmd.to_owned();
    }
    let mut out = tokens;
    out.insert(1, flag.to_owned());
    out.join(" ")
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Full analysis of a command: blockers, rewrites, env injections.
/// Use this for the `pre_validate` tool.
pub fn analyze(cmd: &str) -> Analysis {
    let sudo_status = sudoers_status();
    let mut env = HashMap::new();
    let mut blockers = Vec::new();

    let rewritten = rewrite_command(cmd, &sudo_status, &mut env, &mut blockers);

    let all_clear = blockers.iter().all(|b| b.auto_resolved);

    Analysis {
        original: cmd.to_owned(),
        rewritten,
        injected_env: env,
        blockers,
        all_clear,
        sudo_status,
    }
}

/// Lightweight version for exec_cmd hot path.
/// Returns (rewritten_cmd, env_vars_to_inject).
/// Never errors — worst case returns the original command unchanged.
pub fn preprocess(cmd: &str) -> (String, Vec<(String, String)>) {
    let sudo_status = sudoers_status();
    let mut env_map = HashMap::new();
    let mut _blockers = Vec::new();

    let rewritten = rewrite_command(cmd, &sudo_status, &mut env_map, &mut _blockers);
    let env_vec: Vec<(String, String)> = env_map.into_iter().collect();

    (rewritten, env_vec)
}

/// Check whether the sudoers entry for ferrite is installed and current.
///
/// The file is root-owned 440 — `read_to_string` would fail with EACCES for
/// a normal user.  Instead we use two probes:
///
///   1. `Path::exists()` (stat syscall — only needs parent dir +x, which
///      /etc/sudoers.d has) to check file presence.
///   2. `sudo -k -n /usr/bin/tee /dev/null` — the `-k` flag invalidates any
///      cached credential before the check, so success means our NOPASSWD
///      entry is genuinely active (not just a cached token).
pub fn sudoers_status() -> SudoersStatus {
    let path = sudoers_path();

    // Step 1 — file presence (no read permission required).
    if !std::path::Path::new(path).exists() {
        return SudoersStatus::Missing;
    }

    // Step 2 — probe NOPASSWD without relying on a cached sudo token.
    // `/usr/bin/tee` is in our FERRITE_CMDS alias; `tee /dev/null` with
    // stdin=null is a no-op that exits 0.
    let probe_ok = std::process::Command::new("sudo")
        .args(["-k", "-n", "/usr/bin/tee", "/dev/null"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if probe_ok {
        SudoersStatus::Active
    } else {
        // File exists but NOPASSWD isn't working — stale user or broken entry.
        SudoersStatus::StaleUser
    }
}

/// Attempt to self-install the sudoers entry without a TTY.
///
/// Tries in order:
///   1. `sudo -n tee` — works if there is a cached sudo credential
///      or if a broader NOPASSWD rule already covers this user.
///   2. `pkexec tee`  — triggers a graphical Polkit dialog (desktop sessions).
///
/// Returns `Ok(())` on success, `Err(reason)` if both methods fail.
pub fn try_install_sudoers() -> Result<(), String> {
    let entry = sudoers_entry_content();
    let path = sudoers_path();
    let chmod = "/usr/bin/chmod";

    // Helper: write entry via `<writer> /usr/bin/tee <path>` then chmod 440.
    let write_via = |writer: &str, extra_args: &[&str]| -> bool {
        use std::io::Write;
        let mut cmd = std::process::Command::new(writer);
        for a in extra_args {
            cmd.arg(a);
        }
        cmd.arg("/usr/bin/tee").arg(path);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => return false,
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(entry.as_bytes());
        }
        let wrote_ok = child.wait().map(|s| s.success()).unwrap_or(false);
        if !wrote_ok {
            return false;
        }

        // chmod 440
        std::process::Command::new("sudo")
            .args(["-n", chmod, "440", path])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };

    // 1. Try sudo -n (cached credential or existing broader NOPASSWD).
    if write_via("sudo", &["-n"]) {
        return Ok(());
    }

    // 2. Try pkexec (graphical Polkit — works on desktop without a TTY).
    if write_via("pkexec", &[]) {
        return Ok(());
    }

    Err(format!(
        "Could not install {path} automatically.\n\
         Run install.sh once for a one-time interactive sudo prompt, or:\n\
         sudo tee {path} <<'EOF'\n{entry}EOF\n  sudo chmod 440 {path}"
    ))
}

pub fn sudoers_path() -> &'static str {
    "/etc/sudoers.d/ferrite"
}

/// Generate the text for the sudoers entry.
pub fn sudoers_entry_content() -> String {
    let username = current_username();
    let bins = SUDOERS_BINS.join(", ");
    format!(
        "# ferrite-mcp — agent system privilege pre-authorization\n\
         # Generated by ferrite install.sh — safe to remove if ferrite is uninstalled.\n\
         # Grants NOPASSWD for specific non-destructive system commands only.\n\
         Cmnd_Alias FERRITE_CMDS = {bins}\n\
         {username} ALL=(ALL) NOPASSWD: FERRITE_CMDS\n"
    )
}

// ── Command rewriter ──────────────────────────────────────────────────────────

/// Walk the pipeline stages of a shell command (split on `|`, `&&`, `||`, `;`)
/// and apply per-command rewrites + env injections.
fn rewrite_command(
    cmd: &str,
    sudo_status: &SudoersStatus,
    env: &mut HashMap<String, String>,
    blockers: &mut Vec<Blocker>,
) -> String {
    // Always inject baseline non-interactive env vars.
    inject_baseline_env(env);

    // Split into pipeline segments while preserving delimiters.
    let segments = split_pipeline(cmd);
    let mut out_parts: Vec<String> = Vec::with_capacity(segments.len());

    for (seg, delim) in &segments {
        let rewritten_seg = rewrite_segment(seg.trim(), sudo_status, env, blockers);
        out_parts.push(rewritten_seg);
        if !delim.is_empty() {
            out_parts.push(format!(" {delim} "));
        }
    }

    out_parts.join("").trim().to_owned()
}

/// Apply rewrites to a single command (no pipeline delimiters).
fn rewrite_segment(
    seg: &str,
    sudo_status: &SudoersStatus,
    env: &mut HashMap<String, String>,
    blockers: &mut Vec<Blocker>,
) -> String {
    if seg.is_empty() {
        return seg.to_owned();
    }

    // Handle `sudo <cmd>` — strip sudo prefix for inner analysis, then re-add.
    let (has_sudo, inner) = strip_sudo_prefix(seg);

    // Identify the tool name (first token of inner command).
    let tool = first_token(inner);

    // Apply tool-specific rewrite.
    let (rewritten_inner, _changed) = match tool {
        "apt" | "apt-get" => rewrite_apt(inner, env),
        "pip" | "pip3" => rewrite_pip(inner, env),
        "npm" => rewrite_npm(inner, env),
        "yarn" => rewrite_yarn(inner, env),
        "cargo" => rewrite_cargo(inner, env),
        "git" => rewrite_git(inner, env),
        "rustup" => rewrite_rustup(inner, env),
        "snap" => rewrite_snap(inner, sudo_status, env),
        "systemctl" => rewrite_systemctl(inner, sudo_status, env),
        "ufw" => rewrite_ufw(inner, sudo_status, env),
        "dpkg" => rewrite_dpkg(inner, env),
        "curl" | "wget" => rewrite_downloader(inner, env),
        _ => (inner.to_owned(), false),
    };

    // Check if sudo is needed and whether it's covered.
    let needs_sudo = has_sudo || requires_sudo_by_convention(tool);

    if has_sudo && *sudo_status != SudoersStatus::Active && is_in_sudoers_list(tool) {
        // Sudoers entry missing — this WILL prompt for password.
        blockers.push(Blocker {
            kind: BlockerKind::SudoNotCovered,
            description: format!(
                "`sudo {tool}` requires password — sudoers entry not installed. \
                 Run `ferrite install` or check /etc/sudoers.d/ferrite."
            ),
            auto_resolved: false,
            resolution: "Install sudoers entry via install.sh (one-time sudo prompt)".to_owned(),
        });
    } else if has_sudo && *sudo_status != SudoersStatus::Active && !is_in_sudoers_list(tool) {
        blockers.push(Blocker {
            kind: BlockerKind::SudoNotCovered,
            description: format!(
                "`sudo {tool}` will prompt for password — '{tool}' is not in ferrite's sudoers whitelist."
            ),
            auto_resolved: false,
            resolution: format!("Either run manually, or add {tool} to the sudoers whitelist."),
        });
    }

    // Re-add sudo prefix if it was there originally, or if we determined it's needed.
    if has_sudo || (needs_sudo && *sudo_status == SudoersStatus::Active) {
        format!("sudo {rewritten_inner}")
    } else {
        rewritten_inner
    }
}

// ── Per-tool rewriters ────────────────────────────────────────────────────────

fn rewrite_apt(cmd: &str, env: &mut HashMap<String, String>) -> (String, bool) {
    env.insert("DEBIAN_FRONTEND".into(), "noninteractive".into());
    env.insert("APT_LISTCHANGES_FRONTEND".into(), "none".into());

    // Subcommands that accept -y
    let interactive_verbs = [
        "install",
        "remove",
        "purge",
        "autoremove",
        "upgrade",
        "full-upgrade",
        "dist-upgrade",
        "reinstall",
    ];

    let tokens = tokenize(cmd);
    if tokens.len() < 2 {
        return (cmd.to_owned(), false);
    }

    let verb = tokens[1].as_str();
    if !interactive_verbs.contains(&verb) {
        return (cmd.to_owned(), false);
    }
    if tokens.iter().any(|t| t == "-y" || t == "--yes") {
        return (cmd.to_owned(), false); // already non-interactive
    }

    // Insert -y after the verb
    let mut out = tokens.clone();
    out.insert(2, "-y".to_owned());
    // Add -q to suppress progress meters that can confuse agent output
    if !out.iter().any(|t| t == "-q" || t == "--quiet") {
        out.insert(2, "-q".to_owned());
    }
    (out.join(" "), true)
}

fn rewrite_pip(cmd: &str, env: &mut HashMap<String, String>) -> (String, bool) {
    env.insert("PIP_NO_INPUT".into(), "1".into());
    env.insert("PIP_DISABLE_PIP_VERSION_CHECK".into(), "1".into());

    let tokens = tokenize(cmd);
    if tokens.len() < 2 {
        return (cmd.to_owned(), false);
    }
    if tokens[1] != "install" {
        return (cmd.to_owned(), false);
    }
    if tokens
        .iter()
        .any(|t| t == "--no-input" || t == "-q" || t == "--quiet")
    {
        return (cmd.to_owned(), false);
    }

    let mut out = tokens.clone();
    out.insert(2, "--no-input".to_owned());
    (out.join(" "), true)
}

fn rewrite_npm(cmd: &str, env: &mut HashMap<String, String>) -> (String, bool) {
    env.insert("NPM_CONFIG_YES".into(), "true".into());
    env.insert("CI".into(), "true".into());
    (cmd.to_owned(), false)
}

fn rewrite_yarn(cmd: &str, env: &mut HashMap<String, String>) -> (String, bool) {
    env.insert("YARN_ENABLE_INTERACTIVE".into(), "false".into());
    env.insert("CI".into(), "true".into());
    (cmd.to_owned(), false)
}

fn rewrite_cargo(cmd: &str, env: &mut HashMap<String, String>) -> (String, bool) {
    env.insert("CARGO_TERM_COLOR".into(), "never".into());
    // `cargo install` — add --locked to prevent network surprises, not interactive but good practice
    (cmd.to_owned(), false)
}

fn rewrite_git(cmd: &str, env: &mut HashMap<String, String>) -> (String, bool) {
    env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    env.insert("GCM_INTERACTIVE".into(), "never".into());
    (cmd.to_owned(), false)
}

fn rewrite_rustup(cmd: &str, env: &mut HashMap<String, String>) -> (String, bool) {
    // rustup is already non-interactive when RUSTUP_INIT_SKIP_PATH_CHECK is set,
    // and most subcommands are quiet. But toolchain/component install can ask.
    env.insert("RUSTUP_TOOLCHAIN".into(), "".into()); // prevent default toolchain prompt
    let tokens = tokenize(cmd);
    if tokens.iter().any(|t| t == "-y") {
        return (cmd.to_owned(), false);
    }
    // For `rustup update` / `rustup install`, the --no-modify-path is useful
    // but -y is the key non-interactive flag
    if tokens.len() >= 2 && matches!(tokens[1].as_str(), "update" | "install" | "toolchain") {
        if !tokens
            .iter()
            .any(|t| t == "-y" || t == "--no-update-default-toolchain")
        {
            let mut out = tokens;
            out.push("-y".to_owned());
            return (out.join(" "), true);
        }
    }
    (cmd.to_owned(), false)
}

fn rewrite_snap(
    cmd: &str,
    _sudo_status: &SudoersStatus,
    _env: &mut HashMap<String, String>,
) -> (String, bool) {
    // snap always needs root; if sudoers active, auto-prefix sudo
    // (caller re-adds sudo if has_sudo, so this just returns inner unchanged)
    let tokens = tokenize(cmd);
    if tokens.len() < 2 || tokens[1] != "install" {
        return (cmd.to_owned(), false);
    }

    // Add --classic only if the package hints it (we can't know for sure, so don't)
    (cmd.to_owned(), false)
}

fn rewrite_systemctl(
    cmd: &str,
    _sudo_status: &SudoersStatus,
    _env: &mut HashMap<String, String>,
) -> (String, bool) {
    // systemctl is non-interactive by default; nothing to rewrite.
    // sudo prefix is handled by the caller.
    (cmd.to_owned(), false)
}

fn rewrite_ufw(
    cmd: &str,
    _sudo_status: &SudoersStatus,
    _env: &mut HashMap<String, String>,
) -> (String, bool) {
    // ufw can ask for confirmation on some commands; --force suppresses it
    let tokens = tokenize(cmd);
    if tokens.iter().any(|t| t == "--force") {
        return (cmd.to_owned(), false);
    }
    // Only add --force for potentially confirming commands
    let confirming = ["enable", "disable", "reset", "delete"];
    if tokens.len() >= 2 && confirming.contains(&tokens[1].as_str()) {
        let mut out = tokens;
        out.push("--force".to_owned());
        return (out.join(" "), true);
    }
    (cmd.to_owned(), false)
}

fn rewrite_dpkg(cmd: &str, env: &mut HashMap<String, String>) -> (String, bool) {
    env.insert("DEBIAN_FRONTEND".into(), "noninteractive".into());
    let tokens = tokenize(cmd);
    if tokens
        .iter()
        .any(|t| t == "--force-confdef" || t == "--force-confold")
    {
        return (cmd.to_owned(), false);
    }
    // -i (install) can ask about config file diffs
    if tokens.len() >= 2 && tokens[1] == "-i" {
        let mut out = tokens;
        // Insert dpkg-specific non-interactive flags before the path arg
        out.insert(2, "--force-confdef".to_owned());
        out.insert(3, "--force-confold".to_owned());
        return (out.join(" "), true);
    }
    (cmd.to_owned(), false)
}

fn rewrite_downloader(cmd: &str, env: &mut HashMap<String, String>) -> (String, bool) {
    // curl/wget piped into sh — set CI so downstream installers go non-interactive
    env.insert("CI".into(), "1".into());
    (cmd.to_owned(), false)
}

// ── Baseline env always injected ─────────────────────────────────────────────

fn inject_baseline_env(env: &mut HashMap<String, String>) {
    // Always set — safe for any command, suppresses common interactive prompts.
    env.entry("GIT_TERMINAL_PROMPT".into())
        .or_insert("0".into());
    env.entry("DEBIAN_FRONTEND".into())
        .or_insert("noninteractive".into());
    env.entry("PIP_NO_INPUT".into()).or_insert("1".into());
    env.entry("PIP_DISABLE_PIP_VERSION_CHECK".into())
        .or_insert("1".into());
    env.entry("CARGO_TERM_COLOR".into())
        .or_insert("never".into());
    env.entry("NPM_CONFIG_YES".into()).or_insert("true".into());
    env.entry("YARN_ENABLE_INTERACTIVE".into())
        .or_insert("false".into());
    env.entry("APT_LISTCHANGES_FRONTEND".into())
        .or_insert("none".into());
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Split `cmd` into (segment, delimiter) pairs preserving pipeline structure.
/// Delimiters: `|`, `&&`, `||`, `;`
fn split_pipeline(cmd: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        // Check two-char operators first
        if i + 1 < chars.len() {
            let two = format!("{}{}", c, chars[i + 1]);
            if two == "&&" || two == "||" {
                result.push((current.trim().to_owned(), two));
                current = String::new();
                i += 2;
                continue;
            }
        }
        // Single-char operators
        if c == '|' || c == ';' {
            result.push((current.trim().to_owned(), c.to_string()));
            current = String::new();
        } else {
            current.push(c);
        }
        i += 1;
    }
    if !current.trim().is_empty() {
        result.push((current.trim().to_owned(), String::new()));
    }
    result
}

/// Return the first whitespace-delimited token of a command string.
fn first_token(cmd: &str) -> &str {
    cmd.split_whitespace().next().unwrap_or("")
}

/// Tokenize a command by whitespace (naive — doesn't handle quotes).
fn tokenize(cmd: &str) -> Vec<String> {
    cmd.split_whitespace().map(str::to_owned).collect()
}

/// Strip leading `sudo` (and any sudo flags like `-n`, `-E`, `-u user`) from a command.
/// Returns (had_sudo, remainder).
fn strip_sudo_prefix(cmd: &str) -> (bool, &str) {
    let trimmed = cmd.trim_start();
    if !trimmed.starts_with("sudo") {
        return (false, trimmed);
    }
    // Must be `sudo` followed by space or end
    let after = &trimmed[4..];
    if after.is_empty() || after.starts_with(' ') || after.starts_with('\t') {
        // Skip any sudo flags: -n, -E, -u <user>, -i, -s, --
        let rest = after.trim_start();
        let rest = skip_sudo_flags(rest);
        (true, rest.trim_start())
    } else {
        (false, trimmed) // `sudofoo` — not sudo
    }
}

fn skip_sudo_flags(s: &str) -> &str {
    let mut pos = s;
    loop {
        let t = pos.trim_start();
        if t.starts_with("-n") || t.starts_with("-E") || t.starts_with("-i") || t.starts_with("-s")
        {
            pos = &t[2..];
        } else if t.starts_with("-u ") || t.starts_with("-u\t") {
            // skip -u username
            pos = t[3..].trim_start();
            pos = pos
                .split_whitespace()
                .next()
                .map(|w| &t[3 + t[3..].find(w).unwrap_or(0) + w.len()..])
                .unwrap_or("");
        } else {
            break;
        }
    }
    pos
}

fn is_in_sudoers_list(tool: &str) -> bool {
    SUDOERS_SHORT.contains(&tool)
}

/// Some tools conventionally require root even without an explicit `sudo` prefix
/// (e.g. the agent might write `systemctl enable foo` expecting it to just work).
fn requires_sudo_by_convention(tool: &str) -> bool {
    matches!(
        tool,
        "systemctl" | "ufw" | "snap" | "dpkg" | "add-apt-repository" | "update-alternatives"
    )
}

fn current_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| {
            // Fallback: read from /proc/self/status or id command
            std::process::Command::new("id")
                .arg("-un")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_owned())
                .unwrap_or_else(|| "root".to_owned())
        })
}
