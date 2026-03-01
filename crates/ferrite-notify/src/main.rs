/// ferrite-notify — generic connection notifier.
///
/// Reads ~/.config/ferrite/notify.toml, gets Tailscale IP + Claude session URL,
/// then fans out to configured backends: smtp, ntfy, webhook.
///
/// Usage:
///   ferrite-notify              — send with current state
///   ferrite-notify --wait       — poll until Tailscale is up, then send
///   ferrite-notify --dry-run    — print what would be sent, no network calls
///   ferrite-notify --config /path/to/notify.toml

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};
use serde::Deserialize;

// ── Config schema ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct Config {
    #[serde(default)]
    smtp: Vec<SmtpConfig>,
    #[serde(default)]
    ntfy: Vec<NtfyConfig>,
    #[serde(default)]
    webhook: Vec<WebhookConfig>,
    #[serde(default)]
    machine: MachineConfig,
}

#[derive(Debug, Deserialize)]
struct SmtpConfig {
    /// SMTP host, e.g. "smtp.gmail.com"
    host: String,
    /// SMTP port, e.g. 587 (STARTTLS) or 465 (TLS)
    #[serde(default = "default_smtp_port")]
    port: u16,
    /// Login username
    user: String,
    /// Login password / app password
    password: String,
    /// From address, e.g. "ferrite <you@example.com>"
    #[serde(default)]
    from: String,
    /// List of recipient addresses
    to: Vec<String>,
    /// Use STARTTLS (port 587). Set false for implicit TLS (port 465).
    #[serde(default = "yes")]
    starttls: bool,
    /// Human label for logs
    #[serde(default)]
    label: String,
}

#[derive(Debug, Deserialize)]
struct NtfyConfig {
    /// Your ntfy topic (acts as a shared secret)
    topic: String,
    /// ntfy server base URL (default: https://ntfy.sh)
    #[serde(default = "ntfy_default_server")]
    server: String,
    /// Optional auth token for self-hosted ntfy
    #[serde(default)]
    token: String,
    #[serde(default)]
    label: String,
}

#[derive(Debug, Deserialize)]
struct WebhookConfig {
    /// HTTP endpoint to POST to (e.g. Slack/Discord/custom)
    url: String,
    /// Optional static headers (e.g. Authorization)
    #[serde(default)]
    headers: HashMap<String, String>,
    /// Body template — use {hostname}, {ip}, {ssh}, {session_url}
    /// Default: JSON with all fields
    #[serde(default)]
    body_template: String,
    #[serde(default = "content_type_json")]
    content_type: String,
    #[serde(default)]
    label: String,
}

#[derive(Debug, Deserialize, Default)]
struct MachineConfig {
    /// Override the displayed machine name (default: hostname)
    #[serde(default)]
    name: String,
}

fn default_smtp_port() -> u16 { 587 }
fn yes() -> bool { true }
fn ntfy_default_server() -> String { "https://ntfy.sh".to_string() }
fn content_type_json() -> String { "application/json".to_string() }

// ── Config loading ────────────────────────────────────────────────────────────

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/root"))
}

fn load_config(path: &PathBuf) -> Config {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
            eprintln!("WARN: failed to parse {}: {e}", path.display());
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

/// Backwards-compat: if no smtp entries in notify.toml, fall back to cream gmail.conf
fn maybe_load_legacy_gmail(cfg: &mut Config) {
    if !cfg.smtp.is_empty() {
        return;
    }
    let path = home().join(".config/cream/gmail.conf");
    let Ok(content) = std::fs::read_to_string(&path) else { return };
    let mut kv: HashMap<String, String> = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        if let Some((k, v)) = line.split_once('=') {
            kv.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let user = kv.get("GMAIL_USER").cloned().unwrap_or_default();
    let pass = kv.get("GMAIL_APP_PASSWORD").cloned().unwrap_or_default();
    let to   = kv.get("GMAIL_TO").cloned().unwrap_or_else(|| user.clone());
    if !user.is_empty() && !pass.is_empty() {
        eprintln!("INFO: using legacy ~/.config/cream/gmail.conf (migrate to ~/.config/ferrite/notify.toml)");
        cfg.smtp.push(SmtpConfig {
            host: "smtp.gmail.com".to_string(),
            port: 587,
            user: user.clone(),
            password: pass,
            from: format!("ferrite <{user}>"),
            to: vec![to],
            starttls: true,
            label: "gmail-legacy".to_string(),
        });
    }
}

// ── Tailscale ─────────────────────────────────────────────────────────────────

fn tailscale_ip() -> Option<String> {
    let out = Command::new("tailscale").args(["status", "--json"]).output().ok()?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    json["Self"]["TailscaleIPs"].as_array()?.iter().find_map(|v| {
        let s = v.as_str()?;
        if !s.contains(':') { Some(s.to_string()) } else { None }
    })
}

fn wait_tailscale(timeout_secs: u64) -> Option<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Some(ip) = tailscale_ip() { return Some(ip); }
        if std::time::Instant::now() >= deadline { return None; }
        eprintln!("[ferrite-notify] waiting for Tailscale…");
        sleep(Duration::from_secs(5));
    }
}

// ── System info ───────────────────────────────────────────────────────────────

fn run_trim(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd).args(args).output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

// ── Payload ───────────────────────────────────────────────────────────────────

struct Payload {
    hostname: String,
    whoami: String,
    ts_ip: Option<String>,
    session_url: String,
}

impl Payload {
    fn ts_display(&self) -> &str { self.ts_ip.as_deref().unwrap_or("not available") }
    fn ssh_cmd(&self) -> String {
        self.ts_ip.as_ref()
            .map(|ip| format!("ssh {}@{}", self.whoami, ip))
            .unwrap_or_else(|| "Tailscale not connected".to_string())
    }
    fn subject(&self) -> String {
        format!("🖥️ {} is online — {}", self.hostname, self.ts_display())
    }
    fn html(&self) -> String {
        let session_btn = if !self.session_url.is_empty() {
            format!(
                r#"<p style="margin-top:24px"><a href="{u}" style="background:#5046e4;color:white;padding:12px 24px;border-radius:6px;text-decoration:none;font-weight:bold">Open Claude Session →</a></p>"#,
                u = self.session_url
            )
        } else {
            r#"<p style="color:#999">No Claude session URL saved yet.</p>"#.to_string()
        };
        format!(
            r#"<html><body style="font-family:sans-serif;max-width:600px;margin:40px auto">
  <h2 style="color:#1a1a2e">🖥️ {host} is online</h2>
  <table style="border-collapse:collapse;width:100%;font-size:15px">
    <tr style="background:#f8f8f8">
      <td style="padding:10px 12px;color:#555;white-space:nowrap">Tailscale IP</td>
      <td style="padding:10px 12px;font-family:monospace">{ip}</td>
    </tr>
    <tr>
      <td style="padding:10px 12px;color:#555;white-space:nowrap">SSH</td>
      <td style="padding:10px 12px;font-family:monospace">{ssh}</td>
    </tr>
    <tr style="background:#f8f8f8">
      <td style="padding:10px 12px;color:#555;white-space:nowrap">Attach tmux</td>
      <td style="padding:10px 12px;font-family:monospace">tmux attach -t claude-remote</td>
    </tr>
  </table>
  {btn}
  <hr style="border:none;border-top:1px solid #eee;margin:24px 0">
  <p style="color:#999;font-size:11px">Sent by ferrite-notify</p>
</body></html>"#,
            host = self.hostname,
            ip   = self.ts_display(),
            ssh  = self.ssh_cmd(),
            btn  = session_btn,
        )
    }
    fn plain(&self) -> String {
        format!(
            "Host: {}\nTailscale IP: {}\nSSH: {}\ntmux: tmux attach -t claude-remote\nSession: {}",
            self.hostname, self.ts_display(), self.ssh_cmd(),
            if self.session_url.is_empty() { "(none)" } else { &self.session_url }
        )
    }
    fn json(&self) -> String {
        format!(
            r#"{{"hostname":"{h}","tailscale_ip":"{ip}","ssh":"{ssh}","session_url":"{url}","tmux":"tmux attach -t claude-remote"}}"#,
            h   = self.hostname,
            ip  = self.ts_display(),
            ssh = self.ssh_cmd(),
            url = self.session_url,
        )
    }
    fn render_template(&self, tmpl: &str) -> String {
        tmpl.replace("{hostname}", &self.hostname)
            .replace("{ip}",          self.ts_display())
            .replace("{ssh}",         &self.ssh_cmd())
            .replace("{session_url}", &self.session_url)
            .replace("{tmux}",        "tmux attach -t claude-remote")
    }
}

// ── Backends ──────────────────────────────────────────────────────────────────

fn send_smtp(smtp: &SmtpConfig, p: &Payload, dry_run: bool) -> Result<(), String> {
    let label = if smtp.label.is_empty() { &smtp.host } else { &smtp.label };
    let from_str = if smtp.from.is_empty() {
        format!("ferrite <{}>", smtp.user)
    } else {
        smtp.from.clone()
    };

    if dry_run {
        println!("  [smtp:{label}] → {:?} subject: {}", smtp.to, p.subject());
        return Ok(());
    }

    let from: lettre::message::Mailbox = from_str.parse().map_err(|e| format!("{e}"))?;
    let creds = Credentials::new(smtp.user.clone(), smtp.password.clone());

    for to_addr in &smtp.to {
        let to_mb: lettre::message::Mailbox = to_addr.parse().map_err(|e| format!("{e}"))?;
        let email = Message::builder()
            .from(from.clone())
            .to(to_mb)
            .subject(p.subject())
            .header(ContentType::TEXT_HTML)
            .body(p.html())
            .map_err(|e| format!("{e}"))?;

        let mailer = if smtp.starttls {
            SmtpTransport::starttls_relay(&smtp.host)
                .map_err(|e| format!("{e}"))?
                .port(smtp.port)
                .credentials(creds.clone())
                .build()
        } else {
            SmtpTransport::relay(&smtp.host)
                .map_err(|e| format!("{e}"))?
                .port(smtp.port)
                .credentials(creds.clone())
                .build()
        };

        mailer.send(&email).map_err(|e| format!("{e}"))?;
        println!("  ✓ smtp:{label} → {to_addr}");
    }
    Ok(())
}

fn send_ntfy(ntfy: &NtfyConfig, p: &Payload, dry_run: bool) -> Result<(), String> {
    let label = if ntfy.label.is_empty() { &ntfy.topic } else { &ntfy.label };
    let url = format!("{}/{}", ntfy.server.trim_end_matches('/'), ntfy.topic);

    if dry_run {
        println!("  [ntfy:{label}] → {url}");
        return Ok(());
    }

    let mut args = vec![
        "-sf".to_string(),
        "-H".to_string(), format!("Title: {}", p.subject()),
        "-H".to_string(), "Tags: tada,laptop".to_string(),
        "-H".to_string(), "Priority: high".to_string(),
    ];
    if !ntfy.token.is_empty() {
        args.push("-H".to_string());
        args.push(format!("Authorization: Bearer {}", ntfy.token));
    }
    args.push("-d".to_string());
    args.push(p.plain());
    args.push(url);

    let out = Command::new("curl").args(&args).output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    println!("  ✓ ntfy:{label}");
    Ok(())
}

fn send_webhook(wh: &WebhookConfig, p: &Payload, dry_run: bool) -> Result<(), String> {
    let label = if wh.label.is_empty() { &wh.url } else { &wh.label };
    let body = if wh.body_template.is_empty() {
        p.json()
    } else {
        p.render_template(&wh.body_template)
    };

    if dry_run {
        println!("  [webhook:{label}] body: {body}");
        return Ok(());
    }

    let mut args = vec![
        "-sf".to_string(),
        "-X".to_string(), "POST".to_string(),
        "-H".to_string(), format!("Content-Type: {}", wh.content_type),
    ];
    for (k, v) in &wh.headers {
        args.push("-H".to_string());
        args.push(format!("{k}: {v}"));
    }
    args.push("-d".to_string());
    args.push(body);
    args.push(wh.url.clone());

    let out = Command::new("curl").args(&args).output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string());
    }
    println!("  ✓ webhook:{label}");
    Ok(())
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let wait    = args.iter().any(|a| a == "--wait");
    let dry_run = args.iter().any(|a| a == "--dry-run");

    // Config path
    let config_path = args.windows(2)
        .find(|w| w[0] == "--config")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| home().join(".config/ferrite/notify.toml"));

    let mut cfg = load_config(&config_path);
    maybe_load_legacy_gmail(&mut cfg);

    let backend_count = cfg.smtp.len() + cfg.ntfy.len() + cfg.webhook.len();
    if backend_count == 0 {
        eprintln!(
            "ERROR: no notification backends configured.\n\
             Create ~/.config/ferrite/notify.toml — see the example:\n\
             https://github.com/DaronPopov/verilogchill (ferrite-mcp/crates/ferrite-notify/)"
        );
        std::process::exit(1);
    }

    // Gather info
    let ts_ip = if wait { wait_tailscale(300) } else { tailscale_ip() };
    let hostname = if cfg.machine.name.is_empty() {
        run_trim("hostname", &[])
    } else {
        cfg.machine.name.clone()
    };
    let whoami = run_trim("whoami", &[]);
    let url_path = home().join(".local/share/ferrite/remote-session-url.txt");
    let session_url = std::fs::read_to_string(&url_path).unwrap_or_default().trim().to_string();

    let payload = Payload { hostname, whoami, ts_ip, session_url };

    if dry_run {
        println!("=== DRY RUN ===");
        println!("Subject : {}", payload.subject());
        println!("Backends: {} smtp, {} ntfy, {} webhook",
            cfg.smtp.len(), cfg.ntfy.len(), cfg.webhook.len());
    }

    // Fan out
    let mut errors = 0usize;

    for smtp in &cfg.smtp {
        if let Err(e) = send_smtp(smtp, &payload, dry_run) {
            eprintln!("  ✗ smtp:{} — {e}", if smtp.label.is_empty() { &smtp.host } else { &smtp.label });
            errors += 1;
        }
    }
    for ntfy in &cfg.ntfy {
        if let Err(e) = send_ntfy(ntfy, &payload, dry_run) {
            eprintln!("  ✗ ntfy:{} — {e}", if ntfy.label.is_empty() { &ntfy.topic } else { &ntfy.label });
            errors += 1;
        }
    }
    for wh in &cfg.webhook {
        if let Err(e) = send_webhook(wh, &payload, dry_run) {
            eprintln!("  ✗ webhook:{} — {e}", if wh.label.is_empty() { &wh.url } else { &wh.label });
            errors += 1;
        }
    }

    if errors > 0 {
        std::process::exit(1);
    }
}
