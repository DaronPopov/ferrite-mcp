//! HTTP request tool — probe and debug running services.
//!
//! Tools:
//!   http_request — send HTTP/HTTPS request via curl, return structured response

use serde_json::{json, Value};
use crate::protocol::ToolResult;

// ── http_request ──────────────────────────────────────────────────────────────

pub fn http_request(args: &Value) -> Result<ToolResult, String> {
    let url     = args["url"].as_str().ok_or("http_request: 'url' required")?;
    let method  = args["method"].as_str().unwrap_or("GET").to_uppercase();
    let body    = args["body"].as_str().unwrap_or("");
    let timeout = args["timeout_secs"].as_u64().unwrap_or(30);
    let follow  = args["follow_redirects"].as_bool().unwrap_or(true);
    let insecure = args["insecure"].as_bool().unwrap_or(false);

    let header_tmp = format!("/tmp/cream_http_hdrs_{}.txt", std::process::id());

    // -s silent, -X method, -D header dump, -w stats appended to body output
    // We use a sentinel to split body from stats
    let stats_fmt = "%{http_code}\x1F%{time_total}\x1F%{size_download}\x1F%{content_type}";

    let mut cmd = std::process::Command::new("curl");
    cmd.arg("-s")
       .arg("-X").arg(&method)
       .arg("-D").arg(&header_tmp)
       .arg("-w").arg(format!("\x02CREAM_STATS\x03{stats_fmt}"))
       .arg("--max-time").arg(timeout.to_string())
       .arg("--connect-timeout").arg("10");

    if follow   { cmd.arg("-L"); }
    if insecure { cmd.arg("-k"); }

    // Request headers
    if let Some(obj) = args["headers"].as_object() {
        for (k, v) in obj {
            if let Some(val) = v.as_str() {
                cmd.args(["-H", &format!("{k}: {val}")]);
            }
        }
    }

    // Content-type default for POST with body
    if !body.is_empty() {
        cmd.args(["-d", body]);
        if args["headers"]["content-type"].is_null()
            && args["headers"]["Content-Type"].is_null()
        {
            cmd.args(["-H", "Content-Type: application/json"]);
        }
    }

    cmd.arg(url);

    let start = std::time::Instant::now();
    let out = cmd.output().map_err(|e| format!("http_request: curl: {e}"))?;
    let wall_ms = start.elapsed().as_millis() as u64;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();

    // Split body from appended stats
    let (body_raw, stats_raw) = stdout
        .split_once("\x02CREAM_STATS\x03")
        .unwrap_or((&stdout, ""));

    let stats: Vec<&str> = stats_raw.splitn(4, '\x1F').collect();
    let status: u16  = stats.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let curl_ms: f64 = stats.get(1).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0) * 1000.0;
    let size: u64    = stats.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let ctype        = stats.get(3).unwrap_or(&"").trim().to_owned();

    // Parse response headers from temp file
    let headers_raw = std::fs::read_to_string(&header_tmp).unwrap_or_default();
    let _ = std::fs::remove_file(&header_tmp);
    let resp_headers = parse_http_headers(&headers_raw);

    // Try to parse body as JSON for cleaner output
    let body_value: Value = serde_json::from_str(body_raw.trim())
        .unwrap_or_else(|_| Value::String(body_raw.trim_end().to_owned()));

    let ok = (200..300).contains(&status);

    Ok(ToolResult::json(&json!({
        "ok":           ok,
        "status":       status,
        "latency_ms":   curl_ms as u64,
        "wall_ms":      wall_ms,
        "size_bytes":   size,
        "content_type": ctype,
        "headers":      resp_headers,
        "body":         body_value,
    })))
}

fn parse_http_headers(raw: &str) -> Value {
    let mut map = serde_json::Map::new();
    for line in raw.lines() {
        // Skip status line and blank lines
        if line.starts_with("HTTP/") || line.trim().is_empty() { continue; }
        if let Some((key, val)) = line.split_once(':') {
            map.insert(
                key.trim().to_lowercase(),
                Value::String(val.trim().to_owned()),
            );
        }
    }
    Value::Object(map)
}
