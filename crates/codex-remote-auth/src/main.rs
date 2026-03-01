use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::time::{sleep, Duration, Instant};
use tracing::{error, info, warn};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;
const SESSION_COOKIE: &str = "crx_session";

#[derive(Debug, Clone, Deserialize)]
struct Config {
    listen_addr: Option<String>,
    base_url: String,
    allowed_email: String,
    hmac_secret: String,
    admin_key: String,
    email_command: Option<String>,
    magic_link_ttl_secs: Option<u64>,
    otp_ttl_secs: Option<u64>,
    session_ttl_secs: Option<u64>,
    cookie_secure: Option<bool>,
    cookie_domain: Option<String>,
}

impl Config {
    fn listen_addr(&self) -> &str {
        self.listen_addr.as_deref().unwrap_or("127.0.0.1:8787")
    }
    fn magic_link_ttl_secs(&self) -> u64 {
        self.magic_link_ttl_secs.unwrap_or(600)
    }
    fn otp_ttl_secs(&self) -> u64 {
        self.otp_ttl_secs.unwrap_or(300)
    }
    fn session_ttl_secs(&self) -> u64 {
        self.session_ttl_secs.unwrap_or(8 * 60 * 60)
    }
    fn cookie_secure(&self) -> bool {
        self.cookie_secure.unwrap_or(true)
    }
    fn email_command(&self) -> &str {
        self.email_command
            .as_deref()
            .unwrap_or("/home/daron/.local/bin/cream-sendmail")
    }
}

#[derive(Debug, Clone)]
struct Session {
    email: String,
    expires_at: u64,
    auth_epoch: u64,
}

#[derive(Debug, Clone)]
struct PendingAuth {
    email: String,
    link_expires_at: u64,
    link_used: bool,
    otp_hash: Option<String>,
    otp_expires_at: Option<u64>,
    otp_attempts: u8,
}

#[derive(Debug, Default)]
struct Store {
    pending: HashMap<String, PendingAuth>,
    sessions: HashMap<String, Session>,
    auth_epoch: u64,
}

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    store: Arc<Mutex<Store>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MagicClaims {
    jti: String,
    email: String,
    exp: u64,
}

#[derive(Debug, Deserialize)]
struct RequestLinkPayload {
    email: String,
}

#[derive(Debug, Deserialize)]
struct RedeemQuery {
    token: String,
}

#[derive(Debug, Deserialize)]
struct VerifyForm {
    token: String,
    code: String,
}

#[derive(Debug, Deserialize)]
struct RevokePayload {
    admin_key: String,
}

#[derive(Debug, Deserialize)]
struct CodexSendPayload {
    text: String,
}

#[derive(Debug, Deserialize)]
struct CodexE2ePayload {
    prompt: Option<String>,
    timeout_ms: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "codex_remote_auth=info,info".to_string()),
        )
        .init();

    let cfg_path = config_path_from_args().unwrap_or_else(default_config_path);
    let cfg = load_config(&cfg_path)?;

    if cfg.hmac_secret.len() < 32 {
        anyhow::bail!("hmac_secret must be at least 32 characters");
    }
    if cfg.admin_key.len() < 24 {
        anyhow::bail!("admin_key must be at least 24 characters");
    }

    let addr: SocketAddr = cfg
        .listen_addr()
        .parse()
        .with_context(|| format!("invalid listen_addr: {}", cfg.listen_addr()))?;

    let state = AppState {
        cfg: Arc::new(cfg),
        store: Arc::new(Mutex::new(Store::default())),
    };

    let app = Router::new()
        .route("/", get(login_page))
        .route("/health", get(health))
        .route("/auth/request", post(auth_request))
        .route("/auth/redeem", get(auth_redeem))
        .route("/auth/verify-code", post(auth_verify_code))
        .route("/admin/revoke-all", post(admin_revoke_all))
        .route("/app", get(app_shell))
        .route("/codex", get(codex_shell))
        .route("/codex/new", get(codex_new))
        .route("/codex/read", get(codex_read))
        .route("/codex/send", post(codex_send))
        .route("/codex/e2e", post(codex_e2e))
        .with_state(state);

    info!("codex-remote-auth listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal received");
}

fn config_path_from_args() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--config" {
            return args.next().map(PathBuf::from);
        }
    }
    std::env::var_os("CRX_CONFIG").map(PathBuf::from)
}

fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config/ferrite/codex_remote_auth.toml")
}

fn load_config(path: &PathBuf) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading config: {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).context("failed parsing config TOML")?;
    Ok(cfg)
}

async fn health() -> impl IntoResponse {
    Json(json!({"ok": true}))
}

async fn login_page() -> impl IntoResponse {
    (
        StatusCode::OK,
        Html(
            r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
  <title>Codex Remote Login</title>
  <style>
    :root {
      --bg-a: #f3f4f8;
      --bg-b: #efece5;
      --ink: #111318;
      --muted: #5d6370;
      --line: #d9dde7;
      --card: rgba(255, 255, 255, 0.92);
      --accent: #116e63;
      --accent-strong: #0d5951;
      --ring: rgba(17, 110, 99, 0.32);
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      min-height: 100dvh;
      display: grid;
      place-items: center;
      padding: 24px 16px;
      color: var(--ink);
      font-family: "Space Grotesk", "IBM Plex Sans", "Segoe UI", sans-serif;
      background:
        radial-gradient(1300px 480px at 15% -10%, #d9e7ff 0%, transparent 50%),
        radial-gradient(900px 380px at 95% 8%, #dff3ea 0%, transparent 52%),
        linear-gradient(160deg, var(--bg-a), var(--bg-b));
    }
    .card {
      width: min(100%, 440px);
      border: 1px solid var(--line);
      border-radius: 20px;
      background: var(--card);
      backdrop-filter: blur(8px);
      box-shadow: 0 18px 60px rgba(17, 24, 39, 0.14);
      padding: clamp(18px, 4vw, 26px);
    }
    h1 {
      margin: 0 0 8px;
      font-size: clamp(26px, 4.8vw, 34px);
      line-height: 1.04;
      letter-spacing: -0.02em;
    }
    .muted {
      color: var(--muted);
      font-size: 14px;
      line-height: 1.45;
      margin: 0 0 16px;
    }
    label {
      display: block;
      font-size: 12px;
      letter-spacing: 0.12em;
      text-transform: uppercase;
      color: var(--muted);
      margin-bottom: 8px;
    }
    input {
      width: 100%;
      border: 1px solid var(--line);
      border-radius: 12px;
      padding: 12px 14px;
      min-height: 46px;
      font-size: 16px;
      margin-bottom: 12px;
      background: #fff;
      color: var(--ink);
      outline: none;
    }
    input:focus { border-color: var(--accent); box-shadow: 0 0 0 4px var(--ring); }
    button {
      width: 100%;
      border: 0;
      border-radius: 12px;
      min-height: 46px;
      padding: 12px;
      font-size: 15px;
      font-weight: 650;
      background: linear-gradient(135deg, var(--accent), var(--accent-strong));
      color: #fff;
      box-shadow: 0 10px 24px rgba(17, 110, 99, 0.28);
      cursor: pointer;
      transition: transform 140ms ease, box-shadow 140ms ease;
    }
    button:hover { transform: translateY(-1px); box-shadow: 0 14px 28px rgba(17, 110, 99, 0.32); }
    button:active { transform: translateY(0); }
    .ok {
      color: #0d5951;
      margin-top: 12px;
      padding: 10px 12px;
      border-radius: 10px;
      border: 1px solid #b8e0d8;
      background: #eef9f6;
      font-size: 14px;
      line-height: 1.4;
    }
  </style>
</head>
<body>
  <div class="card">
    <h1>Remote Session Login</h1>
    <p class="muted">Use your approved email to receive a secure sign-in link and one-time code.</p>
    <form id="f">
      <label for="email">Approved Email</label>
      <input id="email" type="email" autocomplete="email" required placeholder="you@example.com" />
      <button type="submit">Send Sign-In Link</button>
    </form>
    <div id="msg" class="ok" style="display:none">If this address is allowed, a sign-in link was sent.</div>
  </div>
  <script>
    const f = document.getElementById('f');
    const msg = document.getElementById('msg');
    f.addEventListener('submit', async (e) => {
      e.preventDefault();
      const email = document.getElementById('email').value.trim();
      try {
        await fetch('/auth/request', {
          method: 'POST',
          headers: {'Content-Type': 'application/json'},
          body: JSON.stringify({ email })
        });
      } catch (_) {}
      msg.style.display = 'block';
    });
  </script>
</body>
</html>"#
                .to_string(),
        ),
    )
}

async fn auth_request(
    State(state): State<AppState>,
    Json(payload): Json<RequestLinkPayload>,
) -> impl IntoResponse {
    let normalized = payload.email.trim().to_lowercase();
    let allowed = state.cfg.allowed_email.trim().to_lowercase();

    // Always return generic success to avoid account discovery.
    let generic = Json(json!({
        "ok": true,
        "message": "If this address is allowed, a sign-in link was sent."
    }));

    if normalized != allowed {
        warn!("auth_request for non-allowed address");
        return (StatusCode::OK, generic).into_response();
    }

    let now = now_secs();
    let jti = Uuid::new_v4().to_string();
    let claims = MagicClaims {
        jti: jti.clone(),
        email: allowed.clone(),
        exp: now + state.cfg.magic_link_ttl_secs(),
    };

    let token = match sign_token(&claims, &state.cfg.hmac_secret) {
        Ok(t) => t,
        Err(e) => {
            error!("failed to sign token: {e}");
            return (StatusCode::OK, generic).into_response();
        }
    };

    {
        let mut store = state.store.lock().expect("store poisoned");
        prune_expired(&mut store, now);
        store.pending.insert(
            jti.clone(),
            PendingAuth {
                email: allowed.clone(),
                link_expires_at: claims.exp,
                link_used: false,
                otp_hash: None,
                otp_expires_at: None,
                otp_attempts: 0,
            },
        );
    }

    let link = format!("{}/auth/redeem?token={}", state.cfg.base_url, token);
    let subject = "Codex Remote Login Link";
    let body = format!(
        "Use this sign-in link (expires in {} minutes):\n\n{}\n\nAfter opening the link, you must enter a one-time code sent in a second email.",
        state.cfg.magic_link_ttl_secs() / 60,
        link
    );

    if let Err(e) = send_email(&state.cfg, subject, &body, &allowed) {
        error!("email send failed: {e}");
    }

    (StatusCode::OK, generic).into_response()
}

async fn auth_redeem(
    State(state): State<AppState>,
    Query(query): Query<RedeemQuery>,
) -> impl IntoResponse {
    let claims = match verify_token(&query.token, &state.cfg.hmac_secret) {
        Ok(c) => c,
        Err(_) => {
            return (StatusCode::UNAUTHORIZED, Html(invalid_link_html())).into_response();
        }
    };

    let now = now_secs();
    if claims.exp < now {
        return (StatusCode::UNAUTHORIZED, Html(invalid_link_html())).into_response();
    }

    let otp_code = format!("{:06}", rand::thread_rng().gen_range(0..1_000_000));
    let otp_hash = hash_otp(&otp_code, &state.cfg.hmac_secret);

    let mut send_code = false;
    {
        let mut store = state.store.lock().expect("store poisoned");
        prune_expired(&mut store, now);
        if let Some(pending) = store.pending.get_mut(&claims.jti) {
            if pending.link_used || pending.link_expires_at < now || pending.email != claims.email {
                return (StatusCode::UNAUTHORIZED, Html(invalid_link_html())).into_response();
            }
            pending.link_used = true;
            pending.otp_hash = Some(otp_hash);
            pending.otp_expires_at = Some(now + state.cfg.otp_ttl_secs());
            pending.otp_attempts = 0;
            send_code = true;
        }
    }

    if !send_code {
        return (StatusCode::UNAUTHORIZED, Html(invalid_link_html())).into_response();
    }

    let subject = "Codex Remote Verification Code";
    let body = format!(
        "Your verification code is: {}\n\nIt expires in {} minutes.\nIf you did not request this, ignore this email.",
        otp_code,
        state.cfg.otp_ttl_secs() / 60
    );
    if let Err(e) = send_email(&state.cfg, subject, &body, &claims.email) {
        error!("email send failed: {e}");
    }

    (StatusCode::OK, Html(verify_html(&query.token))).into_response()
}

async fn auth_verify_code(
    State(state): State<AppState>,
    jar: CookieJar,
    Form(form): Form<VerifyForm>,
) -> impl IntoResponse {
    let claims = match verify_token(&form.token, &state.cfg.hmac_secret) {
        Ok(c) => c,
        Err(_) => {
            return (StatusCode::UNAUTHORIZED, jar, Html(invalid_link_html())).into_response()
        }
    };

    let now = now_secs();
    if claims.exp < now {
        return (StatusCode::UNAUTHORIZED, jar, Html(invalid_link_html())).into_response();
    }

    let incoming = form.code.trim();
    if incoming.len() != 6 || !incoming.chars().all(|c| c.is_ascii_digit()) {
        return (StatusCode::UNAUTHORIZED, jar, Html(invalid_code_html())).into_response();
    }

    let email = {
        let mut store = state.store.lock().expect("store poisoned");
        prune_expired(&mut store, now);

        let Some(pending) = store.pending.get_mut(&claims.jti) else {
            return (StatusCode::UNAUTHORIZED, jar, Html(invalid_link_html())).into_response();
        };

        if !pending.link_used || pending.email != claims.email {
            return (StatusCode::UNAUTHORIZED, jar, Html(invalid_link_html())).into_response();
        }

        let Some(otp_expires_at) = pending.otp_expires_at else {
            return (StatusCode::UNAUTHORIZED, jar, Html(invalid_code_html())).into_response();
        };
        if otp_expires_at < now {
            return (StatusCode::UNAUTHORIZED, jar, Html(invalid_code_html())).into_response();
        }

        if pending.otp_attempts >= 5 {
            return (StatusCode::TOO_MANY_REQUESTS, jar, Html(locked_html())).into_response();
        }

        let expected_hash = pending.otp_hash.clone().unwrap_or_default();
        let incoming_hash = hash_otp(incoming, &state.cfg.hmac_secret);

        if expected_hash != incoming_hash {
            pending.otp_attempts = pending.otp_attempts.saturating_add(1);
            return (StatusCode::UNAUTHORIZED, jar, Html(invalid_code_html())).into_response();
        }

        let email = pending.email.clone();
        store.pending.remove(&claims.jti);
        email
    };
    let sid = Uuid::new_v4().to_string();

    {
        let mut store = state.store.lock().expect("store poisoned");
        let auth_epoch = store.auth_epoch;
        store.sessions.insert(
            sid.clone(),
            Session {
                email,
                expires_at: now + state.cfg.session_ttl_secs(),
                auth_epoch,
            },
        );
    }

    let mut cookie = Cookie::new(SESSION_COOKIE, sid);
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Strict);
    cookie.set_secure(state.cfg.cookie_secure());
    cookie.set_path("/");
    if let Some(domain) = state.cfg.cookie_domain.as_deref() {
        if !domain.trim().is_empty() {
            cookie.set_domain(domain.trim().to_string());
        }
    }

    let jar = jar.add(cookie);
    (jar, Redirect::to("/app")).into_response()
}

async fn admin_revoke_all(
    State(state): State<AppState>,
    Json(payload): Json<RevokePayload>,
) -> impl IntoResponse {
    if payload.admin_key != state.cfg.admin_key {
        return (StatusCode::UNAUTHORIZED, Json(json!({"ok": false}))).into_response();
    }

    let mut store = state.store.lock().expect("store poisoned");
    store.auth_epoch = store.auth_epoch.saturating_add(1);
    store.sessions.clear();
    store.pending.clear();

    (
        StatusCode::OK,
        Json(json!({"ok": true, "auth_epoch": store.auth_epoch})),
    )
        .into_response()
}

async fn app_shell(State(state): State<AppState>, jar: CookieJar) -> impl IntoResponse {
    match authenticated_email(&state, &jar) {
        Some(email) => {
            let claude_url = read_session_url("remote-session-url.txt");
            let codex_url = read_session_url("codex-session-url.txt");
            let body = app_html(&email, claude_url.as_deref(), codex_url.as_deref());
            (StatusCode::OK, Html(body)).into_response()
        }
        None => (StatusCode::UNAUTHORIZED, Html(access_denied_html())).into_response(),
    }
}

fn codex_tmux_session() -> String {
    std::env::var("FERRITE_CODEX_SESSION").unwrap_or_else(|_| "codex-remote".to_string())
}

fn codex_bin() -> String {
    if let Ok(v) = std::env::var("CODEX_BIN") {
        return v;
    }
    let native = "/home/daron/.nvm/versions/node/v20.20.0/lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/codex/codex";
    if std::path::Path::new(native).exists() {
        native.to_string()
    } else {
        "/home/daron/.nvm/versions/node/v20.20.0/bin/codex".to_string()
    }
}

fn tmux_capture(session: &str, lines: &str) -> Option<String> {
    let out = Command::new("tmux")
        .args(["capture-pane", "-t", session, "-p", "-S", lines])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut iter = input.chars().peekable();
    while let Some(ch) = iter.next() {
        if ch == '\u{1b}' {
            if let Some('[') = iter.peek().copied() {
                let _ = iter.next();
                for c in iter.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        if ch == '\r' {
            continue;
        }
        out.push(ch);
    }
    out
}

fn clean_tmux_output(raw: &str) -> String {
    let normalized = strip_ansi(raw);
    let mut lines: Vec<&str> = normalized.lines().collect();
    if lines.len() > 220 {
        lines = lines.split_off(lines.len() - 220);
    }

    let mut cleaned: Vec<&str> = Vec::with_capacity(lines.len());
    for line in lines {
        let t = line.trim();
        let is_frame = t.starts_with('╭')
            || t.starts_with('╰')
            || t.starts_with('│')
            || t.starts_with('─')
            || t.starts_with('┌')
            || t.starts_with('┐')
            || t.starts_with('└')
            || t.starts_with('┘');
        if is_frame {
            continue;
        }
        cleaned.push(line);
    }

    cleaned.join("\n").trim().to_string()
}

fn tmux_has_session(session: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", session])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn tmux_send(session: &str, text: &str) -> bool {
    let ok1 = Command::new("tmux")
        .args(["send-keys", "-t", session, text])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let ok2 = Command::new("tmux")
        .args(["send-keys", "-t", session, "Enter"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    ok1 && ok2
}

fn tmux_kill_session(session: &str) -> bool {
    Command::new("tmux")
        .args(["kill-session", "-t", session])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn tmux_start_codex_session(session: &str) -> Result<(), String> {
    let cmd = format!("{}; exec $SHELL", codex_bin());
    let out = Command::new("tmux")
        .args(["new-session", "-d", "-s", session, "-x", "220", "-y", "50", "--", &cmd])
        .output()
        .map_err(|e| format!("failed to exec tmux: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

async fn codex_new(State(state): State<AppState>, jar: CookieJar) -> impl IntoResponse {
    if authenticated_email(&state, &jar).is_none() {
        return (StatusCode::UNAUTHORIZED, Html(access_denied_html())).into_response();
    }

    let session = codex_tmux_session();
    let _ = tmux_kill_session(&session);

    // Handle tmux race windows around kill/recreate and watchdog interference.
    let mut last_err = String::new();
    for _ in 0..4 {
        if tmux_has_session(&session) {
            let _ = tmux_kill_session(&session);
            sleep(Duration::from_millis(180)).await;
        }
        match tmux_start_codex_session(&session) {
            Ok(()) => break,
            Err(e) => {
                last_err = e;
                sleep(Duration::from_millis(220)).await;
            }
        }
    }

    for _ in 0..10 {
        if tmux_has_session(&session) {
            sleep(Duration::from_millis(350)).await;
            return Redirect::to("/codex").into_response();
        }
        sleep(Duration::from_millis(120)).await;
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Html(format!(
            "<h3>Failed to start fresh Codex session</h3><p>session: {}</p><p>tmux error: {}</p>",
            session,
            if last_err.is_empty() { "unknown" } else { &last_err }
        )),
    )
        .into_response()
}

async fn codex_shell(State(state): State<AppState>, jar: CookieJar) -> impl IntoResponse {
    if authenticated_email(&state, &jar).is_none() {
        return (StatusCode::UNAUTHORIZED, Html(access_denied_html())).into_response();
    }

    let session = codex_tmux_session();
    let boot = format!(
        r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
  <title>Codex Session</title>
  <style>
    :root {{
      --bg: #f2f4f8;
      --line: #d8dde8;
      --panel: rgba(255, 255, 255, 0.9);
      --ink: #121521;
      --muted: #5f6574;
      --accent: #0f6d63;
      --accent-strong: #0c5750;
      --shell: #0f1724;
      --shell-line: #243148;
      --shell-ink: #e6ecf7;
    }}
    * {{ box-sizing: border-box; }}
    body {{
      margin: 0;
      min-height: 100dvh;
      font-family: "IBM Plex Sans", "Space Grotesk", "Segoe UI", sans-serif;
      color: var(--ink);
      background:
        radial-gradient(1200px 420px at 0% -10%, #dce8ff 0%, transparent 55%),
        radial-gradient(900px 360px at 100% 0%, #dff3ea 0%, transparent 58%),
        linear-gradient(180deg, #f8fafc, var(--bg));
    }}
    .wrap {{
      max-width: 1040px;
      margin: 0 auto;
      min-height: 100dvh;
      display: grid;
      grid-template-rows: auto auto 1fr;
      gap: 10px;
      padding: clamp(10px, 2vw, 20px);
    }}
    .head {{
      border: 1px solid var(--line);
      border-radius: 14px;
      padding: 12px 14px;
      background: var(--panel);
      backdrop-filter: blur(8px);
    }}
    .title {{ margin: 0; font-size: clamp(22px, 3.4vw, 30px); letter-spacing: -0.02em; }}
    .sub {{ margin-top: 4px; color: var(--muted); font-size: 13px; }}
    .bar {{
      border: 1px solid var(--line);
      border-radius: 14px;
      background: var(--panel);
      backdrop-filter: blur(8px);
      padding: 10px;
      display: grid;
      grid-template-columns: 1fr auto auto auto;
      gap: 8px;
      align-items: center;
    }}
    input {{
      min-width: 0;
      width: 100%;
      padding: 12px 14px;
      min-height: 44px;
      border: 1px solid var(--line);
      border-radius: 10px;
      font-size: 15px;
      background: #fff;
      color: var(--ink);
      outline: none;
    }}
    input:focus {{ border-color: var(--accent); box-shadow: 0 0 0 4px rgba(15, 109, 99, 0.25); }}
    button {{
      min-height: 44px;
      border: 0;
      border-radius: 10px;
      padding: 0 14px;
      font-weight: 650;
      color: #fff;
      background: linear-gradient(135deg, var(--accent), var(--accent-strong));
      cursor: pointer;
      white-space: nowrap;
    }}
    .ghost {{
      background: #fff;
      color: var(--ink);
      border: 1px solid var(--line);
      box-shadow: none;
    }}
    .out {{
      margin: 0;
      border: 1px solid var(--line);
      border-radius: 14px;
      background: #ffffff;
      color: #111827;
      padding: 14px 14px 18px;
      min-height: 48vh;
      max-height: 74vh;
      overflow: auto;
      line-height: 1.45;
      font-size: 14px;
      font-family: "IBM Plex Sans", "Space Grotesk", "Segoe UI", sans-serif;
      white-space: pre-wrap;
      word-break: break-word;
    }}
    @media (max-width: 760px) {{
      .bar {{ grid-template-columns: 1fr 1fr 1fr; }}
      .bar input {{ grid-column: 1 / -1; }}
      .out {{ min-height: 52vh; max-height: 62vh; }}
    }}
  </style>
</head>
<body>
  <div class="wrap">
    <div class="head">
      <h1 class="title">Codex Session</h1>
      <div class="sub">tmux session: {session}</div>
    </div>
    <div class="bar">
      <input id="cmd" placeholder="Type command/input for Codex..." />
      <button id="send">Send</button>
      <button id="e2e" class="ghost">Run E2E</button>
      <button id="refresh" class="ghost">Refresh</button>
    </div>
    <div class="sub" id="diag">E2E idle.</div>
    <div id="out" class="out">Loading...</div>
  </div>
  <script>
    const out = document.getElementById('out');
    const cmd = document.getElementById('cmd');
    const send = document.getElementById('send');
    const e2eBtn = document.getElementById('e2e');
    const refreshBtn = document.getElementById('refresh');
    const diag = document.getElementById('diag');
    async function refresh() {{
      try {{
        const r = await fetch('/codex/read');
        const j = await r.json();
        out.textContent = j.output || '(no output)';
        out.scrollTop = out.scrollHeight;
      }} catch (_) {{}}
    }}
    async function submit() {{
      const text = cmd.value.trim();
      if (!text) return;
      cmd.value = '';
      try {{
        await fetch('/codex/send', {{
          method: 'POST',
          headers: {{'Content-Type': 'application/json'}},
          body: JSON.stringify({{ text }})
        }});
      }} catch (_) {{}}
      setTimeout(refresh, 300);
    }}
    async function runE2E() {{
      const prompt = cmd.value.trim();
      diag.textContent = 'Running E2E check...';
      e2eBtn.disabled = true;
      try {{
        const r = await fetch('/codex/e2e', {{
          method: 'POST',
          headers: {{'Content-Type': 'application/json'}},
          body: JSON.stringify({{ prompt, timeout_ms: 14000 }})
        }});
        const j = await r.json();
        diag.textContent = j.ok ? `E2E PASS: ${{j.summary}}` : `E2E FAIL: ${{j.summary}}`;
        if (j.output_tail) {{
          out.textContent = j.output_tail;
          out.scrollTop = out.scrollHeight;
        }} else {{
          refresh();
        }}
      }} catch (_) {{
        diag.textContent = 'E2E FAIL: request error';
      }} finally {{
        e2eBtn.disabled = false;
      }}
    }}
    send.addEventListener('click', submit);
    e2eBtn.addEventListener('click', runE2E);
    refreshBtn.addEventListener('click', refresh);
    cmd.addEventListener('keydown', (e) => {{ if (e.key === 'Enter') submit(); }});
    refresh();
    setInterval(refresh, 2000);
  </script>
</body>
</html>"#
    );
    (StatusCode::OK, Html(boot)).into_response()
}

async fn codex_read(State(state): State<AppState>, jar: CookieJar) -> impl IntoResponse {
    if authenticated_email(&state, &jar).is_none() {
        return (StatusCode::UNAUTHORIZED, Json(json!({"ok": false}))).into_response();
    }
    let session = codex_tmux_session();
    let output = tmux_capture(&session, "-240").unwrap_or_else(|| {
        format!(
            "Codex tmux session '{}' is not available yet. Please wait a few seconds.",
            session
        )
    });
    let cleaned = clean_tmux_output(&output);
    (StatusCode::OK, Json(json!({"ok": true, "output": cleaned}))).into_response()
}

async fn codex_send(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(payload): Json<CodexSendPayload>,
) -> impl IntoResponse {
    if authenticated_email(&state, &jar).is_none() {
        return (StatusCode::UNAUTHORIZED, Json(json!({"ok": false}))).into_response();
    }
    let session = codex_tmux_session();
    let ok = tmux_send(&session, payload.text.trim());
    (StatusCode::OK, Json(json!({"ok": ok}))).into_response()
}

async fn codex_e2e(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(payload): Json<CodexE2ePayload>,
) -> impl IntoResponse {
    if authenticated_email(&state, &jar).is_none() {
        return (StatusCode::UNAUTHORIZED, Json(json!({"ok": false, "summary": "unauthorized"})))
            .into_response();
    }

    let session = codex_tmux_session();
    if !tmux_has_session(&session) {
        return (
            StatusCode::OK,
            Json(json!({
                "ok": false,
                "summary": format!("tmux session '{}' not found", session),
                "session": session,
            })),
        )
            .into_response();
    }

    let timeout_ms = payload.timeout_ms.unwrap_or(12_000).clamp(1_000, 60_000);
    let marker = format!("CRX_E2E_{}", Uuid::new_v4().simple());
    let user_prompt = payload.prompt.unwrap_or_default();
    let prompt = user_prompt.trim();

    if !prompt.is_empty() && !tmux_send(&session, prompt) {
        return (
            StatusCode::OK,
            Json(json!({
                "ok": false,
                "summary": "failed to send prompt to tmux session",
                "session": session,
            })),
        )
            .into_response();
    }

    if !tmux_send(&session, &format!("echo {}", marker)) {
        return (
            StatusCode::OK,
            Json(json!({
                "ok": false,
                "summary": "failed to inject marker command into tmux session",
                "session": session,
            })),
        )
            .into_response();
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut last_output = String::new();
    while Instant::now() < deadline {
        let output = tmux_capture(&session, "-260").unwrap_or_default();
        if output.contains(&marker) {
            let cleaned = clean_tmux_output(&output);
            return (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "summary": format!("marker observed in session '{}' within {} ms", session, timeout_ms),
                    "session": session,
                    "marker": marker,
                    "output_tail": cleaned,
                })),
            )
                .into_response();
        }
        last_output = output;
        sleep(Duration::from_millis(300)).await;
    }

    let cleaned = clean_tmux_output(&last_output);
    (
        StatusCode::OK,
        Json(json!({
            "ok": false,
            "summary": format!("timeout waiting for marker in session '{}' after {} ms", session, timeout_ms),
            "session": session,
            "marker": marker,
            "output_tail": cleaned,
        })),
    )
        .into_response()
}

fn authenticated_email(state: &AppState, jar: &CookieJar) -> Option<String> {
    let sid = jar.get(SESSION_COOKIE)?.value().to_string();
    let now = now_secs();
    let mut store = state.store.lock().ok()?;
    prune_expired(&mut store, now);

    let session = store.sessions.get(&sid)?;
    if session.expires_at < now {
        store.sessions.remove(&sid);
        return None;
    }
    if session.auth_epoch != store.auth_epoch {
        store.sessions.remove(&sid);
        return None;
    }

    Some(session.email.clone())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn prune_expired(store: &mut Store, now: u64) {
    store.pending.retain(|_, p| {
        let otp_ok = p.otp_expires_at.map(|t| t >= now).unwrap_or(true);
        p.link_expires_at >= now && otp_ok
    });
    store
        .sessions
        .retain(|_, s| s.expires_at >= now && s.auth_epoch == store.auth_epoch);
}

fn hash_otp(code: &str, secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hasher.update(b":");
    hasher.update(code.as_bytes());
    let out = hasher.finalize();
    URL_SAFE_NO_PAD.encode(out)
}

fn sign_token(claims: &MagicClaims, secret: &str) -> Result<String> {
    let payload_json = serde_json::to_vec(claims)?;
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload_json);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).context("invalid hmac secret")?;
    mac.update(payload_b64.as_bytes());
    let sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(format!("{}.{}", payload_b64, sig))
}

fn verify_token(token: &str, secret: &str) -> Result<MagicClaims> {
    let mut parts = token.split('.');
    let payload_b64 = parts.next().context("bad token")?;
    let sig = parts.next().context("bad token")?;
    if parts.next().is_some() {
        anyhow::bail!("bad token format");
    }

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).context("invalid hmac secret")?;
    mac.update(payload_b64.as_bytes());
    let expected_sig = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    if expected_sig != sig {
        anyhow::bail!("signature mismatch");
    }

    let payload = URL_SAFE_NO_PAD.decode(payload_b64.as_bytes())?;
    let claims: MagicClaims = serde_json::from_slice(&payload)?;
    Ok(claims)
}

fn send_email(cfg: &Config, subject: &str, body: &str, to: &str) -> Result<()> {
    let output = Command::new(cfg.email_command())
        .arg(subject)
        .arg(body)
        .arg(to)
        .output()
        .with_context(|| format!("failed to execute email command: {}", cfg.email_command()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("email command failed: {stderr}");
    }

    Ok(())
}

fn verify_html(token: &str) -> String {
    format!(
        r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
  <title>Verify Access</title>
  <style>
    :root {{
      --bg-a: #f3f4f8;
      --bg-b: #efece5;
      --ink: #111318;
      --muted: #5d6370;
      --line: #d9dde7;
      --card: rgba(255, 255, 255, 0.92);
      --accent: #116e63;
      --accent-strong: #0d5951;
      --ring: rgba(17, 110, 99, 0.32);
    }}
    * {{ box-sizing: border-box; }}
    body {{
      margin: 0;
      min-height: 100dvh;
      display: grid;
      place-items: center;
      padding: 24px 16px;
      color: var(--ink);
      font-family: "Space Grotesk", "IBM Plex Sans", "Segoe UI", sans-serif;
      background:
        radial-gradient(1300px 480px at 15% -10%, #d9e7ff 0%, transparent 50%),
        radial-gradient(900px 380px at 95% 8%, #dff3ea 0%, transparent 52%),
        linear-gradient(160deg, var(--bg-a), var(--bg-b));
    }}
    .card {{
      width: min(100%, 440px);
      border: 1px solid var(--line);
      border-radius: 20px;
      background: var(--card);
      backdrop-filter: blur(8px);
      box-shadow: 0 18px 60px rgba(17, 24, 39, 0.14);
      padding: clamp(18px, 4vw, 26px);
    }}
    h1 {{
      margin: 0 0 8px;
      font-size: clamp(24px, 4.4vw, 32px);
      line-height: 1.06;
      letter-spacing: -0.02em;
    }}
    p {{ margin: 0 0 14px; color: var(--muted); line-height: 1.45; font-size: 14px; }}
    label {{
      display: block;
      font-size: 12px;
      letter-spacing: 0.12em;
      text-transform: uppercase;
      color: var(--muted);
      margin-bottom: 8px;
    }}
    input {{
      width: 100%;
      border: 1px solid var(--line);
      border-radius: 12px;
      padding: 12px 14px;
      min-height: 46px;
      font-size: 24px;
      line-height: 1;
      letter-spacing: 0.34em;
      text-align: center;
      font-weight: 600;
      background: #fff;
      color: var(--ink);
      outline: none;
    }}
    input:focus {{ border-color: var(--accent); box-shadow: 0 0 0 4px var(--ring); }}
    button {{
      margin-top: 12px;
      width: 100%;
      border: 0;
      border-radius: 12px;
      min-height: 46px;
      padding: 12px;
      font-size: 15px;
      font-weight: 650;
      background: linear-gradient(135deg, var(--accent), var(--accent-strong));
      color: #fff;
      box-shadow: 0 10px 24px rgba(17, 110, 99, 0.28);
      cursor: pointer;
    }}
    @media (max-width: 520px) {{
      input {{ font-size: 21px; letter-spacing: 0.26em; }}
    }}
  </style>
</head>
<body>
  <div class="card">
    <h1>Verify Access</h1>
    <p>A second email was sent with a 6-digit code.</p>
    <form method="post" action="/auth/verify-code">
      <input type="hidden" name="token" value="{token}" />
      <label for="code">One-Time Code</label>
      <input id="code" name="code" inputmode="numeric" pattern="[0-9]{{6}}" maxlength="6" placeholder="000000" required />
      <button type="submit">Enter Session</button>
    </form>
  </div>
</body>
</html>"#
    )
}

fn read_session_url(file_name: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join(".local/share/ferrite").join(file_name);
    let url = std::fs::read_to_string(path).ok()?.trim().to_owned();
    if url.starts_with("https://") {
        Some(url)
    } else {
        None
    }
}

fn app_html(email: &str, claude_url: Option<&str>, codex_url: Option<&str>) -> String {
    let chooser_block = if claude_url.is_none() && codex_url.is_none() {
        r#"<div class="msg assistant">
        Authenticated. Waiting for session URLs from ferrite services.
        <div style="margin-top: 8px;" class="sub">Refresh this page in a few seconds.</div>
      </div>"#
            .to_string()
    } else {
        let claude_card = if let Some(url) = claude_url {
            format!(
                r#"<div class="card">
          <div class="card-title">Claude Session</div>
          <div class="sub">Current remote web session link from ferrite-autostart.</div>
          <a class="open" href="{url}" target="_blank" rel="noopener">Open Claude</a>
        </div>"#
            )
        } else {
            r#"<div class="card disabled">
          <div class="card-title">Claude Session</div>
          <div class="sub">Not available yet.</div>
          <button disabled>Unavailable</button>
        </div>"#
                .to_string()
        };

        let codex_card = if let Some(url) = codex_url {
            format!(
                r#"<div class="card">
          <div class="card-title">Codex Session</div>
          <div class="sub">Always starts a fresh Codex tmux session before opening.</div>
          <a class="open" href="/codex/new">Open Fresh Codex</a>
          <div class="sub" style="margin-top: 8px;">Published URL: {url}</div>
        </div>"#
            )
        } else {
            r#"<div class="card disabled">
          <div class="card-title">Codex Session</div>
          <div class="sub">No Codex session URL published yet.</div>
          <button disabled>Unavailable</button>
        </div>"#
                .to_string()
        };

        format!(
            r#"<div class="msg assistant">
        Pick where to continue:
        <div class="chooser">
          {claude_card}
          {codex_card}
        </div>
      </div>"#
        )
    };

    format!(
        r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
  <title>Codex Remote</title>
  <style>
    :root {{
      --bg-a: #eef2fa;
      --bg-b: #ece8df;
      --panel: rgba(255, 255, 255, 0.92);
      --text: #12141d;
      --muted: #5e6474;
      --accent: #0f6d63;
      --accent-strong: #0d5851;
      --line: #d8dde7;
    }}
    * {{ box-sizing: border-box; }}
    body {{
      margin: 0;
      min-height: 100dvh;
      font-family: "Space Grotesk", "IBM Plex Sans", "Segoe UI", sans-serif;
      background:
        radial-gradient(1180px 420px at 8% -6%, #d7e6ff 0%, transparent 55%),
        radial-gradient(900px 380px at 94% 0%, #dff3ea 0%, transparent 58%),
        linear-gradient(168deg, var(--bg-a), var(--bg-b));
      color: var(--text);
    }}
    .wrap {{
      width: min(100%, 860px);
      margin: 0 auto;
      min-height: 100dvh;
      display: grid;
      grid-template-rows: auto 1fr auto;
      gap: 12px;
      padding: clamp(10px, 2.2vw, 18px);
    }}
    header {{
      border: 1px solid var(--line);
      border-radius: 16px;
      padding: 14px 16px;
      background: var(--panel);
      backdrop-filter: blur(8px);
      position: sticky;
      top: 8px;
      z-index: 3;
    }}
    .title {{
      font-weight: 700;
      letter-spacing: -0.02em;
      font-size: clamp(20px, 3.6vw, 30px);
      margin: 0;
    }}
    .sub {{ color: var(--muted); font-size: 13px; margin-top: 4px; }}
    main {{ overflow: auto; }}
    .msg {{
      background: var(--panel);
      border: 1px solid var(--line);
      border-radius: 16px;
      padding: 14px;
      max-width: 100%;
      backdrop-filter: blur(8px);
    }}
    .assistant {{ margin-right: auto; }}
    .chooser {{
      display: grid;
      grid-template-columns: repeat(2, minmax(0, 1fr));
      gap: 12px;
      margin-top: 12px;
    }}
    .card {{
      border: 1px solid var(--line);
      border-radius: 14px;
      padding: 14px;
      background: rgba(255, 255, 255, 0.88);
      box-shadow: 0 10px 24px rgba(17, 24, 39, 0.08);
    }}
    .card-title {{ font-weight: 700; margin-bottom: 6px; letter-spacing: -0.01em; }}
    .disabled {{ opacity: 0.74; filter: grayscale(0.12); }}
    .open {{
      display: inline-block;
      margin-top: 10px;
      background: linear-gradient(135deg, var(--accent), var(--accent-strong));
      color: #fff;
      text-decoration: none;
      padding: 10px 14px;
      border-radius: 10px;
      font-weight: 650;
    }}
    .composer {{
      border: 1px solid var(--line);
      border-radius: 14px;
      background: var(--panel);
      backdrop-filter: blur(8px);
      padding: 10px 12px calc(10px + env(safe-area-inset-bottom));
    }}
    textarea {{
      width: 100%;
      resize: none;
      border: 1px solid var(--line);
      border-radius: 10px;
      padding: 10px;
      font: inherit;
      color: var(--text);
      box-sizing: border-box;
      min-height: 44px;
      max-height: 120px;
      background: #fff;
    }}
    .row {{ display: grid; grid-template-columns: 1fr auto; gap: 8px; margin-top: 8px; align-items: center; }}
    button {{
      border: 0;
      background: linear-gradient(135deg, var(--accent), var(--accent-strong));
      color: #fff;
      border-radius: 10px;
      padding: 10px 12px;
      font-weight: 650;
      min-height: 42px;
    }}
    @media (max-width: 720px) {{
      .chooser {{ grid-template-columns: 1fr; }}
      .row {{ grid-template-columns: 1fr; }}
      .row .sub {{ margin-bottom: 2px; }}
      .open, button {{ width: 100%; text-align: center; }}
    }}
  </style>
</head>
<body>
  <div class="wrap">
    <header>
      <h1 class="title">Codex Remote</h1>
      <div class="sub">Authenticated as {email}</div>
    </header>
    <main id="log">
      {chooser_block}
    </main>
    <div class="composer">
      <textarea placeholder="Message..." disabled></textarea>
      <div class="row">
        <div class="sub">Input disabled in auth scaffold build</div>
        <button disabled>Send</button>
      </div>
    </div>
  </div>
</body>
</html>"#
    )
}

fn invalid_link_html() -> String {
    "<h3>Invalid or expired link</h3>".to_string()
}

fn invalid_code_html() -> String {
    "<h3>Invalid or expired code</h3>".to_string()
}

fn access_denied_html() -> String {
    "<h3>Access denied</h3><p>Use the email sign-in link.</p>".to_string()
}

fn locked_html() -> String {
    "<h3>Too many attempts</h3><p>Request a new email link.</p>".to_string()
}
