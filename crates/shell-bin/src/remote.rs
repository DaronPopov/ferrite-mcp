use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

const DEFAULT_SESSION: &str = "main";

pub fn run_remote(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("enable") => run_enable(args.get(1).map(String::as_str)),
        Some("up") => run_up(args.get(1).map(String::as_str)),
        Some("doctor") => run_doctor(),
        Some("login-shell") => run_login_shell(args.get(1).map(String::as_str)),
        Some("install-login-hook") => run_install_login_hook(args.get(1).map(String::as_str)),
        Some("mcp-config") => run_mcp_config(args.get(1), args.get(2)),
        _ => {
            print_remote_usage();
            Ok(())
        }
    }
}

pub fn run_enable(session: Option<&str>) -> Result<()> {
    let session = session.unwrap_or(DEFAULT_SESSION);

    run_install_login_hook(Some(session))?;
    let tailscale_note = ensure_tailscale();
    let sshd_note = ensure_sshd();
    run_up(Some(session))?;

    println!();
    println!("FERRITE UP");
    println!("tailscale  : {tailscale_note}");
    println!("sshd       : {sshd_note}");
    println!("password   : use your Linux account password");
    println!(
        "next       : ssh {}@{}",
        current_user(),
        best_host_for_login()
    );
    println!("landing    : ferrite remote login-shell {session}");

    Ok(())
}

fn run_up(session: Option<&str>) -> Result<()> {
    let session = session.unwrap_or(DEFAULT_SESSION);
    let report = collect_report(session)?;

    if report.tmux.bin.is_none() {
        bail!("tmux not found in PATH; install tmux before using ferrite remote up");
    }

    if !report.tmux.session_exists {
        create_tmux_session(session, &report.workspace_root)?;
    }

    println!("REMOTE READY");
    println!("Host       : {}", report.best_host_display());
    if let Some(dns_name) = report.best_dns_display() {
        println!("MagicDNS   : {dns_name}");
    }
    println!("SSH        : {}", report.ssh_command());
    if let Some(ssh_dns) = report.ssh_dns_command() {
        println!("SSH DNS    : {ssh_dns}");
    }
    println!("tmux       : tmux attach -t {session}");
    println!("Workspace  : {}", report.workspace_root.display());
    println!("ferrite    : {}", report.ferrite_bin.display());
    println!("sshd       : {}", report.sshd.summary());
    println!("tailscale  : {}", report.tailscale.summary());
    println!(
        "tmux state : {}",
        if report.tmux.session_exists {
            "existing session reused"
        } else {
            "created"
        }
    );
    println!("Vivado     : {}", report.vivado.summary());
    println!("CUDA       : {}", report.cuda.summary());

    for warn in report.warning_lines() {
        println!("WARN: {warn}");
    }

    Ok(())
}

fn run_doctor() -> Result<()> {
    let report = collect_report(DEFAULT_SESSION)?;

    println!("REMOTE DOCTOR");
    println!("Host       : {}", report.best_host_display());
    if let Some(dns_name) = report.best_dns_display() {
        println!("MagicDNS   : {dns_name}");
    }
    println!("Workspace  : {}", report.workspace_root.display());
    println!("ferrite    : {}", report.ferrite_bin.display());
    println!();
    println!("[reachability]");
    println!("tailscale  : {}", report.tailscale.summary());
    println!("sshd       : {}", report.sshd.summary());
    println!();
    println!("[tooling]");
    println!("tmux       : {}", report.tmux.summary());
    println!("Vivado     : {}", report.vivado.summary());
    println!("CUDA       : {}", report.cuda.summary());

    let warnings = report.warning_lines();
    if warnings.is_empty() {
        println!();
        println!("doctor: OK");
    } else {
        println!();
        for warn in warnings {
            println!("WARN: {warn}");
        }
        println!("doctor: issues found");
    }

    Ok(())
}

fn run_login_shell(session: Option<&str>) -> Result<()> {
    let session = session.unwrap_or(DEFAULT_SESSION);
    let report = collect_report(session)?;

    if report.tmux.bin.is_none() {
        bail!("tmux not found in PATH; install tmux before using ferrite remote login-shell");
    }

    if !session_exists(session) {
        create_tmux_session(session, &report.workspace_root)?;
    }

    let status = Command::new("tmux")
        .args(["attach-session", "-t", session])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to attach tmux session")?;

    if !status.success() {
        bail!("tmux attach-session exited with status {status}");
    }

    Ok(())
}

fn run_mcp_config(host: Option<&String>, user: Option<&String>) -> Result<()> {
    let host = host.ok_or_else(|| anyhow!("usage: ferrite remote mcp-config <host> [user]"))?;
    let user = user
        .cloned()
        .or_else(|| env::var("USER").ok())
        .unwrap_or_else(|| "daron".to_owned());
    let ferrite_bin = env::current_exe().unwrap_or_else(|_| PathBuf::from("~/.cargo/bin/ferrite"));
    let remote_bin = ferrite_bin.display().to_string();

    println!("[mcp_servers.ferrite]");
    println!("command = \"ssh\"");
    println!(
        "args = [\"{}@{}\", \"/bin/bash\", \"-lc\", \"{} --mcp\"]",
        user, host, remote_bin
    );
    println!();
    println!("# If SSH still prompts for a password, use `ferrite remote login-shell` for manual remote work.");

    Ok(())
}

fn run_install_login_hook(session: Option<&str>) -> Result<()> {
    let session = session.unwrap_or(DEFAULT_SESSION);
    let bashrc = home_dir()?.join(".bashrc");
    let hook = login_hook_block(session);

    let existing = std::fs::read_to_string(&bashrc).unwrap_or_default();
    let updated = upsert_hook_block(&existing, &hook);

    if updated == existing {
        println!("login hook already installed in {}", bashrc.display());
        return Ok(());
    }

    std::fs::write(&bashrc, updated)
        .with_context(|| format!("failed to write {}", bashrc.display()))?;

    println!("installed login hook in {}", bashrc.display());
    println!("SSH password logins will auto-attach to tmux session `{session}` via ferrite");
    Ok(())
}

fn print_remote_usage() {
    println!("usage:");
    println!("  ferrite remote enable [session]");
    println!("  ferrite remote up [session]");
    println!("  ferrite remote doctor");
    println!("  ferrite remote login-shell [session]");
    println!("  ferrite remote install-login-hook [session]");
    println!("  ferrite remote mcp-config <host> [user]");
}

#[derive(Clone)]
struct Report {
    ferrite_bin: PathBuf,
    workspace_root: PathBuf,
    tailscale: TailscaleCheck,
    sshd: Check,
    tmux: TmuxCheck,
    vivado: Check,
    cuda: Check,
}

impl Report {
    fn best_host_display(&self) -> String {
        self.tailscale.ip.clone().unwrap_or_else(local_hostname)
    }

    fn best_dns_display(&self) -> Option<String> {
        self.tailscale.dns_name.clone()
    }

    fn ssh_command(&self) -> String {
        let user = env::var("USER").unwrap_or_else(|_| "daron".to_owned());
        format!("ssh {}@{}", user, self.best_host_display())
    }

    fn ssh_dns_command(&self) -> Option<String> {
        let user = env::var("USER").unwrap_or_else(|_| "daron".to_owned());
        self.best_dns_display()
            .map(|host| format!("ssh {}@{}", user, host))
    }

    fn warning_lines(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        for (label, check) in [
            ("tailscale", &self.tailscale.base),
            ("sshd", &self.sshd),
            ("tmux", &self.tmux.base),
            ("Vivado", &self.vivado),
            ("CUDA", &self.cuda),
        ] {
            if !check.ok {
                warnings.push(format!("{label}: {}", check.message));
            }
        }

        warnings
    }
}

#[derive(Clone)]
struct Check {
    ok: bool,
    message: String,
}

impl Check {
    fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
        }
    }

    fn warn(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
        }
    }

    fn summary(&self) -> &str {
        &self.message
    }
}

#[derive(Clone)]
struct TailscaleCheck {
    base: Check,
    ip: Option<String>,
    dns_name: Option<String>,
}

impl TailscaleCheck {
    fn summary(&self) -> &str {
        self.base.summary()
    }
}

#[derive(Clone)]
struct TmuxCheck {
    base: Check,
    bin: Option<PathBuf>,
    session_exists: bool,
}

impl TmuxCheck {
    fn summary(&self) -> &str {
        self.base.summary()
    }
}

fn collect_report(session: &str) -> Result<Report> {
    let ferrite_bin = resolve_ferrite_bin().context("failed to resolve ferrite binary path")?;
    let cwd = env::current_dir().context("failed to resolve current directory")?;
    let workspace_root = resolve_workspace_root(&cwd);

    Ok(Report {
        ferrite_bin,
        workspace_root,
        tailscale: check_tailscale(),
        sshd: check_sshd(),
        tmux: check_tmux(session),
        vivado: check_vivado(),
        cuda: check_cuda(),
    })
}

fn resolve_ferrite_bin() -> Result<PathBuf> {
    let current = env::current_exe().context("failed to resolve current executable")?;
    if current.file_name().and_then(|v| v.to_str()) == Some("ferrite") {
        return Ok(current);
    }

    if let Some(parent) = current.parent() {
        let sibling = parent.join("ferrite");
        if sibling.exists() {
            return Ok(sibling);
        }
    }

    if let Some(path) = find_bin("ferrite") {
        return Ok(path);
    }

    Ok(current)
}

fn resolve_workspace_root(cwd: &Path) -> PathBuf {
    if let Ok(root) = env::var("FERRITE_REMOTE_ROOT") {
        let path = PathBuf::from(root);
        if path.exists() {
            return path;
        }
    }

    if let Some(root) = git_root(cwd) {
        return root;
    }

    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home);
    }

    cwd.to_path_buf()
}

fn git_root(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

fn check_tailscale() -> TailscaleCheck {
    let Some(bin) = find_bin("tailscale") else {
        return TailscaleCheck {
            base: Check::warn("not installed"),
            ip: None,
            dns_name: None,
        };
    };

    let output = Command::new(bin).args(["status", "--json"]).output();
    let Ok(output) = output else {
        return TailscaleCheck {
            base: Check::warn("installed but status probe failed"),
            ip: None,
            dns_name: None,
        };
    };

    let body = String::from_utf8_lossy(&output.stdout);
    if let Ok(json) = serde_json::from_str::<Value>(&body) {
        let state = json
            .get("BackendState")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if state == "Running" {
            let ip = json
                .get("Self")
                .and_then(|v| v.get("TailscaleIPs"))
                .and_then(Value::as_array)
                .and_then(|v| v.first())
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let dns_name = json
                .get("Self")
                .and_then(|v| v.get("DNSName"))
                .and_then(Value::as_str)
                .map(|s| s.trim_end_matches('.').to_owned());
            let message = match &ip {
                Some(ip) => format!("running ({ip})"),
                None => "running".to_owned(),
            };
            return TailscaleCheck {
                base: Check::ok(message),
                ip,
                dns_name,
            };
        }
    }

    TailscaleCheck {
        base: Check::warn("installed but not running"),
        ip: None,
        dns_name: None,
    }
}

fn check_sshd() -> Check {
    if port_22_listening() {
        return Check::ok("listening on tcp/22");
    }

    for unit in ["ssh", "sshd"] {
        let output = Command::new("systemctl").args(["is-active", unit]).output();
        if let Ok(output) = output {
            if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "active"
            {
                return Check::ok(format!("service {unit} active"));
            }
        }
    }

    Check::warn("not listening on tcp/22 and no active ssh/sshd systemd unit detected")
}

fn port_22_listening() -> bool {
    let output = Command::new("ss").args(["-ltn"]).output();
    let Ok(output) = output else {
        return false;
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.contains(":22 ") || line.ends_with(":22"))
}

fn check_tmux(session: &str) -> TmuxCheck {
    let bin = find_bin("tmux");
    let Some(bin_path) = &bin else {
        return TmuxCheck {
            base: Check::warn("not installed"),
            bin,
            session_exists: false,
        };
    };

    let exists = Command::new(bin_path)
        .args(["has-session", "-t", session])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    let message = if exists {
        format!("installed; session `{session}` exists")
    } else {
        format!("installed; session `{session}` missing")
    };

    TmuxCheck {
        base: Check::ok(message),
        bin,
        session_exists: exists,
    }
}


fn check_vivado() -> Check {
    let cfg = shell_mcp::FerriteConfig::load();
    if !cfg.paths.vivado.is_empty() {
        let path = PathBuf::from(&cfg.paths.vivado);
        if path.join("vivado").exists() || path.join("vivado.bat").exists() {
            return Check::ok(format!("configured at {}", path.display()));
        }
        return Check::warn(format!(
            "configured path missing vivado binary: {}",
            path.display()
        ));
    }

    if let Some(path) = find_bin("vivado") {
        return Check::ok(format!("in PATH at {}", path.display()));
    }

    Check::warn("not found; set paths.vivado or add vivado to PATH")
}

fn check_cuda() -> Check {
    if let Some(path) = find_bin("nvcc") {
        return Check::ok(format!("nvcc in PATH at {}", path.display()));
    }

    let cuda_root = PathBuf::from("/usr/local/cuda");
    if cuda_root.exists() {
        return Check::ok(format!("CUDA root present at {}", cuda_root.display()));
    }

    Check::warn("nvcc not found and /usr/local/cuda missing")
}

fn find_bin(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn create_tmux_session(session: &str, workspace_root: &Path) -> Result<()> {
    let status = Command::new("tmux")
        .args(["new-session", "-d", "-s", session, "-c"])
        .arg(workspace_root)
        .arg("bash")
        .status()
        .context("failed to create tmux session")?;

    if !status.success() {
        bail!("tmux new-session exited with status {status}");
    }

    Ok(())
}

fn session_exists(session: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", session])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn local_hostname() -> String {
    if let Ok(output) = Command::new("hostname").output() {
        let host = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !host.is_empty() {
            return host;
        }
    }
    "unknown-host".to_owned()
}

fn ensure_tailscale() -> String {
    match check_tailscale() {
        TailscaleCheck {
            base: Check {
                ok: true, message, ..
            },
            ..
        } => message,
        _ => {
            let Some(bin) = find_bin("tailscale") else {
                return "not installed".to_owned();
            };

            if let Ok(status) = Command::new("sudo")
                .arg("-n")
                .arg(&bin)
                .args(["up", "--accept-routes"])
                .status()
            {
                if status.success() {
                    return check_tailscale().base.message;
                }
            }

            if let Ok(status) = Command::new(&bin).args(["up", "--accept-routes"]).status() {
                if status.success() {
                    return check_tailscale().base.message;
                }
            }

            "failed to start automatically".to_owned()
        }
    }
}

fn ensure_sshd() -> String {
    match check_sshd() {
        Check {
            ok: true, message, ..
        } => message,
        _ => {
            for unit in ["ssh", "sshd"] {
                if let Ok(status) = Command::new("sudo")
                    .args(["-n", "systemctl", "start", unit])
                    .status()
                {
                    if status.success() && check_sshd().ok {
                        return format!("started via systemctl ({unit})");
                    }
                }
            }

            "failed to start automatically".to_owned()
        }
    }
}

fn best_host_for_login() -> String {
    check_tailscale().ip.unwrap_or_else(local_hostname)
}

fn current_user() -> String {
    env::var("USER").unwrap_or_else(|_| "daron".to_owned())
}

fn home_dir() -> Result<PathBuf> {
    env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| anyhow!("HOME is not set"))
}

fn login_hook_block(session: &str) -> String {
    format!(
        "\n# >>> ferrite remote login >>>\nif [ -n \"$SSH_CONNECTION\" ] && [ -z \"$TMUX\" ] && [ \"${{FERRITE_REMOTE_AUTO_ATTACH:-1}}\" = \"1\" ]; then\n    if command -v ferrite >/dev/null 2>&1; then\n        exec ferrite remote login-shell {session}\n    fi\nfi\n# <<< ferrite remote login <<<\n"
    )
}

fn upsert_hook_block(existing: &str, hook: &str) -> String {
    const START: &str = "# >>> ferrite remote login >>>";
    const END: &str = "# <<< ferrite remote login <<<";

    if let Some(start) = existing.find(START) {
        if let Some(rel_end) = existing[start..].find(END) {
            let end = start + rel_end + END.len();
            let mut out = String::with_capacity(existing.len() + hook.len());
            out.push_str(&existing[..start]);
            out.push_str(hook);
            out.push_str(&existing[end..]);
            return out;
        }
    }

    let mut out = existing.to_owned();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(hook);
    out
}
