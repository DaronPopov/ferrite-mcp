//! pipeline_run, pipeline_status, pipeline_cancel tool implementations.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::pipeline::{PipelineStore, StepDef};
use crate::protocol::ToolResult;

// ── pipeline_run ──────────────────────────────────────────────────────────────

pub fn pipeline_run(args: &Value, pipelines: &Arc<PipelineStore>) -> Result<ToolResult, String> {
    let steps_raw = args["steps"].as_array()
        .ok_or("pipeline_run: 'steps' must be a non-empty array")?;

    if steps_raw.is_empty() {
        return Err("pipeline_run: 'steps' array must not be empty".to_string());
    }

    let step_defs: Vec<StepDef> = steps_raw.iter().enumerate().map(|(i, s)| {
        let id  = s["id"].as_str()
            .unwrap_or(&format!("step{i}"))
            .to_string();
        let cmd = s["cmd"].as_str()
            .ok_or_else(|| format!("pipeline_run: step {i} missing 'cmd'"))?
            .to_string();
        let depends_on: Vec<String> = s["depends_on"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();

        Ok(StepDef {
            id,
            cmd,
            cwd:        s["cwd"].as_str().map(str::to_owned),
            label:      s["label"].as_str().map(str::to_owned),
            depends_on,
        })
    }).collect::<Result<Vec<_>, String>>()?;

    let default_cwd = args["cwd"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

    let label           = args["label"].as_str();
    let stop_on_failure = args["stop_on_failure"].as_bool().unwrap_or(true);

    let pipeline = pipelines.run(step_defs, default_cwd, label, stop_on_failure)?;

    let step_list: Vec<Value> = pipeline.steps.iter().map(|s| {
        json!({
            "id":         s.id,
            "label":      s.label,
            "cmd":        s.cmd,
            "depends_on": s.depends_on,
            "status":     "pending",
        })
    }).collect();

    Ok(ToolResult::json(&json!({
        "pipeline_id":       pipeline.pipeline_id,
        "label":             pipeline.label,
        "status":            "running",
        "stop_on_failure":   stop_on_failure,
        "steps":             step_list,
        "note": "Pipeline started. Use pipeline_status to track progress."
    })))
}

// ── pipeline_status ───────────────────────────────────────────────────────────

pub fn pipeline_status(args: &Value, pipelines: &Arc<PipelineStore>) -> Result<ToolResult, String> {
    let pipeline_id = args["pipeline_id"].as_str()
        .ok_or("pipeline_status: 'pipeline_id' is required")?;

    let pipeline = pipelines.get(pipeline_id)
        .ok_or_else(|| format!("pipeline_status: pipeline '{pipeline_id}' not found"))?;

    let p_status = pipeline.current_status();

    let steps: Vec<Value> = pipeline.steps.iter().map(|s| {
        let status    = s.status.lock().unwrap().clone();
        let job_id    = s.job_id.lock().unwrap().clone();
        json!({
            "id":         s.id,
            "label":      s.label,
            "cmd":        s.cmd,
            "depends_on": s.depends_on,
            "status":     status.label(),
            "exit_code":  status.exit_code(),
            "job_id":     job_id,
        })
    }).collect();

    let pending  = steps.iter().filter(|s| s["status"] == "pending").count();
    let running  = steps.iter().filter(|s| s["status"] == "running").count();
    let done     = steps.iter().filter(|s| s["status"] == "done").count();
    let failed   = steps.iter().filter(|s| s["status"] == "failed").count();
    let skipped  = steps.iter().filter(|s| s["status"] == "skipped").count();

    Ok(ToolResult::json(&json!({
        "pipeline_id":   pipeline.pipeline_id,
        "label":         pipeline.label,
        "status":        p_status.label(),
        "elapsed_secs":  (pipeline.elapsed_secs() * 10.0).round() / 10.0,
        "steps":         steps,
        "summary": {
            "total":   pipeline.steps.len(),
            "pending": pending,
            "running": running,
            "done":    done,
            "failed":  failed,
            "skipped": skipped,
        }
    })))
}

// ── pipeline_cancel ───────────────────────────────────────────────────────────

pub fn pipeline_cancel(args: &Value, pipelines: &Arc<PipelineStore>) -> Result<ToolResult, String> {
    let pipeline_id = args["pipeline_id"].as_str()
        .ok_or("pipeline_cancel: 'pipeline_id' is required")?;

    let cancelled_jobs = pipelines.cancel(pipeline_id)?;

    Ok(ToolResult::json(&json!({
        "ok":             true,
        "pipeline_id":    pipeline_id,
        "cancelled_jobs": cancelled_jobs,
        "message":        format!("Pipeline '{pipeline_id}' cancelled ({} jobs killed)", cancelled_jobs.len()),
    })))
}
