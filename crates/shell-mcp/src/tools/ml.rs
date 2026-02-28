//! ML data layer — inspect PyTorch tensors and checkpoints without a REPL.
//!
//! Tools:
//!   tensor_inspect   — load .pt/.pth file, return shape/dtype/stats per tensor
//!   checkpoint_list  — enumerate checkpoint files in a directory with metadata

use std::path::{Path, PathBuf};
use serde_json::{json, Value};
use crate::protocol::ToolResult;

// ── tensor_inspect ────────────────────────────────────────────────────────────

/// Inspect a PyTorch .pt/.pth file via a generated Python script.
/// Returns tensor shapes, dtypes, and basic statistics (min/max/mean).
pub fn tensor_inspect(args: &Value) -> Result<ToolResult, String> {
    let path    = args["path"].as_str().ok_or("tensor_inspect: 'path' required")?;
    let keys    = collect_str_array(args, "keys");
    let max_tensors = args["max_tensors"].as_u64().unwrap_or(50) as usize;

    if !Path::new(path).exists() {
        return Ok(ToolResult::error(format!("tensor_inspect: file not found: {path}")));
    }

    let key_filter_py = if keys.is_empty() {
        "None".to_owned()
    } else {
        format!("{keys:?}")
    };

    let script = format!(r#"
import sys, json
try:
    import torch
except ImportError:
    print(json.dumps({{"error": "torch not installed. Run: pip install torch"}}))
    sys.exit(0)

try:
    data = torch.load('{path}', map_location='cpu', weights_only=False)
except Exception as e:
    print(json.dumps({{"error": str(e)}}))
    sys.exit(0)

key_filter = {key_filter_py}
max_tensors = {max_tensors}
tensor_count = 0

def stats(t):
    if t.numel() == 0:
        return None
    f = t.float()
    return {{
        "min":  round(float(f.min()), 6),
        "max":  round(float(f.max()), 6),
        "mean": round(float(f.mean()), 6),
        "std":  round(float(f.std()), 6) if t.numel() > 1 else 0.0,
    }}

def inspect(obj, path="", depth=0):
    global tensor_count
    if depth > 6 or tensor_count >= max_tensors:
        return {{"truncated": True}}
    if isinstance(obj, torch.Tensor):
        tensor_count += 1
        return {{
            "type":  "tensor",
            "shape": list(obj.shape),
            "dtype": str(obj.dtype).replace("torch.", ""),
            "numel": obj.numel(),
            "stats": stats(obj),
        }}
    elif isinstance(obj, dict):
        result = {{}}
        for k, v in obj.items():
            if key_filter is not None and str(k) not in key_filter:
                continue
            result[str(k)] = inspect(v, f"{{path}}.{{k}}", depth + 1)
        return result
    elif isinstance(obj, (list, tuple)):
        if len(obj) == 0:
            return {{"type": type(obj).__name__, "len": 0}}
        sample = inspect(obj[0], f"{{path}}[0]", depth + 1)
        return {{"type": type(obj).__name__, "len": len(obj), "sample": sample}}
    elif isinstance(obj, (int, float, bool, str)):
        return {{"type": type(obj).__name__, "value": obj}}
    else:
        return {{"type": type(obj).__name__}}

result = inspect(data)
print(json.dumps({{"path": "{path}", "tensor_count": tensor_count, "data": result}}))
"#);

    run_python_script(&script, "tensor_inspect")
}

// ── checkpoint_list ───────────────────────────────────────────────────────────

/// Enumerate PyTorch checkpoint files (.pt/.pth/.ckpt) in a directory.
/// Returns basic metadata for each: size, mtime, and key listing if loadable.
pub fn checkpoint_list(args: &Value) -> Result<ToolResult, String> {
    let root = args["path"].as_str().unwrap_or(".");
    let inspect = args["inspect"].as_bool().unwrap_or(true);
    let max_files = args["max_files"].as_u64().unwrap_or(20) as usize;

    let root_path = PathBuf::from(root);
    if !root_path.exists() {
        return Ok(ToolResult::error(format!("checkpoint_list: path not found: {root}")));
    }

    // Collect checkpoint files
    let mut files: Vec<PathBuf> = Vec::new();
    collect_checkpoints(&root_path, &mut files, 0);
    files.sort_by(|a, b| {
        let ta = a.metadata().and_then(|m| m.modified()).ok();
        let tb = b.metadata().and_then(|m| m.modified()).ok();
        tb.cmp(&ta) // newest first
    });
    files.truncate(max_files);

    if files.is_empty() {
        return Ok(ToolResult::json(&json!({
            "path": root, "count": 0, "files": []
        })));
    }

    if !inspect {
        let listing: Vec<Value> = files.iter().map(|f| {
            let meta = f.metadata().ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta.and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            json!({ "path": f.display().to_string(), "size_bytes": size, "mtime": mtime })
        }).collect();
        return Ok(ToolResult::json(&json!({ "path": root, "count": listing.len(), "files": listing })));
    }

    // Build Python script to inspect all files at once
    let paths_py: Vec<String> = files.iter()
        .map(|f| format!("\"{}\"", f.display()))
        .collect();
    let paths_list = paths_py.join(", ");

    let script = format!(r#"
import sys, json, os
try:
    import torch
    has_torch = True
except ImportError:
    has_torch = False

from pathlib import Path

paths = [{paths_list}]
results = []

for p in paths:
    meta = {{
        "path":       p,
        "size_bytes": os.path.getsize(p),
        "mtime":      int(os.path.getmtime(p)),
        "ext":        Path(p).suffix,
    }}
    if has_torch:
        try:
            data = torch.load(p, map_location='cpu', weights_only=True)
            if isinstance(data, dict):
                keys = list(data.keys())[:20]
                meta["keys"] = [str(k) for k in keys]
                meta["key_count"] = len(data)
                # Find epoch/step if present
                for k in ['epoch', 'step', 'global_step', 'iteration']:
                    if k in data:
                        v = data[k]
                        meta[k] = int(v) if hasattr(v, '__int__') else str(v)
            elif isinstance(data, torch.Tensor):
                meta["tensor_shape"] = list(data.shape)
                meta["tensor_dtype"] = str(data.dtype).replace("torch.", "")
        except Exception as e:
            try:
                data = torch.load(p, map_location='cpu', weights_only=False)
                if isinstance(data, dict):
                    meta["keys"] = [str(k) for k in list(data.keys())[:20]]
                    meta["key_count"] = len(data)
            except Exception as e2:
                meta["load_error"] = str(e2)
    results.append(meta)

print(json.dumps({{"path": "{root}", "count": len(results), "files": results}}))
"#);

    run_python_script(&script, "checkpoint_list")
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn run_python_script(script: &str, tool: &str) -> Result<ToolResult, String> {
    let tmp = format!("/tmp/ferrite_{tool}_{}.py", std::process::id());
    std::fs::write(&tmp, script).map_err(|e| format!("{tool}: write tmp: {e}"))?;

    let out = std::process::Command::new("python3")
        .arg(&tmp)
        .output()
        .map_err(|e| format!("{tool}: python3: {e}"))?;
    let _ = std::fs::remove_file(&tmp);

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if !out.status.success() && stdout.trim().is_empty() {
        return Ok(ToolResult::error(format!("{tool}: {stderr}")));
    }

    let parsed: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|_| json!({ "raw": stdout.trim(), "stderr": stderr }));

    Ok(ToolResult::json(&parsed))
}

fn collect_checkpoints(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 6 { return; }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "target" || name == ".git" || name.starts_with('.') { continue; }
        if path.is_dir() {
            collect_checkpoints(&path, out, depth + 1);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if matches!(ext, "pt" | "pth" | "ckpt" | "bin" | "safetensors") {
                out.push(path);
            }
        }
    }
}

fn collect_str_array(args: &Value, key: &str) -> Vec<String> {
    match &args[key] {
        Value::Array(arr) => arr.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => vec![],
    }
}
