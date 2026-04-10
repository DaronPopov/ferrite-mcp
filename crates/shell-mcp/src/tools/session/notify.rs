//! notify — send notifications to desktop and/or phone via ntfy.sh.
//!
//! Desktop: notify-send (libnotify)
//! Phone:   POST to ntfy.sh/<topic>  (set NTFY_TOPIC env var or pass topic param)
//!          ntfy.sh is free, open-source, no account needed.
//!          Subscribe on phone: ntfy.sh app → subscribe to your topic.
//!
//! Parameters:
//!   message   (required) — notification body
//!   title     — title (default: "ferrite")
//!   topic     — ntfy.sh topic override (default: $NTFY_TOPIC env var)
//!   urgency   — low | normal | critical  (desktop only, default: normal)
//!   icon      — icon name for notify-send (default: "dialog-information")
//!   tags      — ntfy.sh tags array e.g. ["white_check_mark"] for phone emoji
//!   priority  — ntfy.sh priority: min|low|default|high|urgent (default: default)
//!   desktop   — send desktop notification (default: true)
//!   phone     — send phone notification via ntfy.sh (default: true if topic set)

use serde_json::{json, Value};
use std::time::Duration;

use crate::protocol::ToolResult;
use crate::tools::execution::run;

pub fn notify(args: &Value) -> Result<ToolResult, String> {
    let message = args["message"]
        .as_str()
        .ok_or("notify: 'message' is required")?;
    let title = args["title"].as_str().unwrap_or("ferrite");
    let urgency = args["urgency"].as_str().unwrap_or("normal");
    let icon = args["icon"].as_str().unwrap_or("dialog-information");
    let priority = args["priority"].as_str().unwrap_or("default");
    let desktop = args["desktop"].as_bool().unwrap_or(true);

    // Resolve ntfy topic
    let topic = args["topic"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| std::env::var("NTFY_TOPIC").ok().filter(|t| !t.is_empty()));
    let phone = args["phone"].as_bool().unwrap_or(topic.is_some());

    let tags: Vec<&str> = args["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let mut desktop_result = json!(null);
    let mut phone_result = json!(null);

    // ── Desktop notification ──────────────────────────────────────────────────
    if desktop {
        // Escape for shell
        let safe_title = title.replace('"', "\\\"");
        let safe_message = message.replace('"', "\\\"");

        let cmd = format!(r#"notify-send -u {urgency} -i {icon} "{safe_title}" "{safe_message}""#);
        let raw = run(&cmd, &cwd, &[], "", Duration::from_secs(5));
        let ok = raw["success"].as_bool().unwrap_or(false);

        desktop_result = json!({
            "sent":   ok,
            "reason": if ok { json!(null) } else { json!(raw["stderr"].as_str().unwrap_or("notify-send not available")) },
        });
    }

    // ── Phone notification via ntfy.sh ────────────────────────────────────────
    if phone {
        match &topic {
            None => {
                phone_result = json!({
                    "sent":   false,
                    "reason": "no ntfy topic — set NTFY_TOPIC env var or pass 'topic' param",
                });
            }
            Some(t) => {
                let tags_header = if tags.is_empty() {
                    String::new()
                } else {
                    format!(" -H 'Tags: {}'", tags.join(","))
                };

                let safe_msg = message.replace('\'', "'\\''");
                let safe_title = title.replace('\'', "'\\''");

                let cmd = format!(
                    "curl -sf \
                     -H 'Title: {safe_title}' \
                     -H 'Priority: {priority}'{tags_header} \
                     -d '{safe_msg}' \
                     https://ntfy.sh/{t}"
                );

                let raw = run(&cmd, &cwd, &[], "", Duration::from_secs(15));
                let ok = raw["success"].as_bool().unwrap_or(false);

                phone_result = json!({
                    "sent":  ok,
                    "topic": t,
                    "url":   format!("https://ntfy.sh/{t}"),
                    "reason": if ok { json!(null) } else {
                        json!(raw["stderr"].as_str().unwrap_or("curl failed"))
                    },
                });
            }
        }
    }

    let any_sent = desktop_result["sent"].as_bool().unwrap_or(false)
        || phone_result["sent"].as_bool().unwrap_or(false);

    Ok(ToolResult::json(&json!({
        "sent":    any_sent,
        "title":   title,
        "message": message,
        "desktop": desktop_result,
        "phone":   phone_result,
    })))
}
