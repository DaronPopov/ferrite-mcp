//! bench_history — persistent benchmark result store.
//!
//! Stores results as newline-delimited JSON at ~/.local/share/ferrite/bench_history.jsonl
//! Operations: record | list | query | compare
//!
//! Schema per record:
//!   { ts, tag, value, unit, kernel?, N?, config?, notes? }

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};

use crate::protocol::ToolResult;

pub fn bench_history(args: &Value) -> Result<ToolResult, String> {
    let op = args["op"]
        .as_str()
        .ok_or("bench_history: 'op' required — record | list | query | compare")?;

    match op {
        "record" => record(args),
        "list" => list(args),
        "query" => query(args),
        "compare" => compare(args),
        other => Err(format!(
            "bench_history: unknown op '{other}'. Use record|list|query|compare"
        )),
    }
}

// ── record ────────────────────────────────────────────────────────────────────

fn record(args: &Value) -> Result<ToolResult, String> {
    let tag = args["tag"]
        .as_str()
        .ok_or("bench_history record: 'tag' required")?;
    let value = args["value"]
        .as_f64()
        .ok_or("bench_history record: 'value' required (f64)")?;
    let unit = args["unit"]
        .as_str()
        .ok_or("bench_history record: 'unit' required (e.g. 'GB/s', 'GFLOP/s', 'ms')")?;

    let ts = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let record = json!({
        "ts":     ts,
        "tag":    tag,
        "value":  value,
        "unit":   unit,
        "kernel": args["kernel"].as_str(),
        "N":      args["N"].as_u64(),
        "config": args["config"].as_str(),
        "notes":  args["notes"].as_str(),
    });

    let path = history_path()?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("bench_history: open {}: {e}", path.display()))?;

    writeln!(file, "{}", serde_json::to_string(&record).unwrap())
        .map_err(|e| format!("bench_history: write: {e}"))?;

    Ok(ToolResult::json(&json!({
        "recorded": true,
        "record":   record,
        "path":     path.display().to_string(),
    })))
}

// ── list ─────────────────────────────────────────────────────────────────────

fn list(args: &Value) -> Result<ToolResult, String> {
    let limit = args["limit"].as_u64().unwrap_or(20) as usize;
    let records = load_all()?;
    let total = records.len();
    let slice: Vec<&Value> = records.iter().rev().take(limit).collect();

    Ok(ToolResult::json(&json!({
        "total":   total,
        "showing": slice.len(),
        "records": slice,
    })))
}

// ── query ─────────────────────────────────────────────────────────────────────

fn query(args: &Value) -> Result<ToolResult, String> {
    let tag_filter = args["tag"].as_str();
    let kernel_filter = args["kernel"].as_str();
    let limit = args["limit"].as_u64().unwrap_or(50) as usize;

    let records = load_all()?;
    let filtered: Vec<&Value> = records
        .iter()
        .filter(|r| {
            let tag_ok = tag_filter
                .map(|t| r["tag"].as_str().map(|rt| rt.contains(t)).unwrap_or(false))
                .unwrap_or(true);
            let kern_ok = kernel_filter
                .map(|k| {
                    r["kernel"]
                        .as_str()
                        .map(|rk| rk.contains(k))
                        .unwrap_or(false)
                })
                .unwrap_or(true);
            tag_ok && kern_ok
        })
        .rev()
        .take(limit)
        .collect();

    Ok(ToolResult::json(&json!({
        "count":   filtered.len(),
        "records": filtered,
    })))
}

// ── compare ───────────────────────────────────────────────────────────────────

fn compare(args: &Value) -> Result<ToolResult, String> {
    let tag = args["tag"]
        .as_str()
        .ok_or("bench_history compare: 'tag' required")?;
    let records = load_all()?;

    let tag_records: Vec<&Value> = records
        .iter()
        .filter(|r| r["tag"].as_str() == Some(tag))
        .collect();

    if tag_records.len() < 2 {
        return Ok(ToolResult::json(&json!({
            "tag":     tag,
            "message": "Need at least 2 records for comparison",
            "count":   tag_records.len(),
        })));
    }

    let latest = tag_records[tag_records.len() - 1];
    let prev = tag_records[tag_records.len() - 2];

    let v1 = prev["value"].as_f64().unwrap_or(0.0);
    let v2 = latest["value"].as_f64().unwrap_or(0.0);
    let delta_pct = if v1 != 0.0 {
        (v2 - v1) / v1 * 100.0
    } else {
        0.0
    };
    let improved = delta_pct > 0.0;

    Ok(ToolResult::json(&json!({
        "tag":        tag,
        "previous":   prev,
        "latest":     latest,
        "delta_pct":  (delta_pct * 100.0).round() / 100.0,
        "improved":   improved,
        "direction":  if improved { "faster" } else if delta_pct < 0.0 { "slower" } else { "unchanged" },
        "total_records": tag_records.len(),
    })))
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn history_path() -> Result<PathBuf, String> {
    let base = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));

    let dir = base.join(".local/share/ferrite");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("bench_history: create dir {}: {e}", dir.display()))?;

    Ok(dir.join("bench_history.jsonl"))
}

fn load_all() -> Result<Vec<Value>, String> {
    let path = history_path()?;

    if !path.exists() {
        return Ok(vec![]);
    }

    let file = std::fs::File::open(&path).map_err(|e| format!("bench_history: open: {e}"))?;

    let records: Vec<Value> = std::io::BufReader::new(file)
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect();

    Ok(records)
}
