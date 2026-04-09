//! inspect_binary — nm + ldd + readelf in one call.
//!
//! After build_check succeeds, this answers:
//!   - Does the binary link the right .so files? (ldd)
//!   - Are the expected symbols present? (nm -D)
//!   - What architecture/type is this? (readelf -h)

use serde_json::{json, Value};

use crate::protocol::ToolResult;

pub fn inspect_binary(args: &Value) -> Result<ToolResult, String> {
    let path = args["path"]
        .as_str()
        .ok_or("inspect_binary: 'path' is required")?;

    if !std::path::Path::new(path).exists() {
        return Ok(ToolResult::error(format!(
            "inspect_binary: {path}: no such file"
        )));
    }

    let ldd = run_ldd(path);
    let symbols = run_nm(path);
    let elf_hdr = run_readelf(path);

    Ok(ToolResult::json(&json!({
        "path":    path,
        "elf":     elf_hdr,
        "dynamic_deps": ldd,
        "symbols": symbols,
    })))
}

// ── ldd ───────────────────────────────────────────────────────────────────────

fn run_ldd(path: &str) -> Value {
    let Ok(out) = std::process::Command::new("ldd").arg(path).output() else {
        return json!({"error": "ldd not available"});
    };

    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return json!({"error": msg.trim()});
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut deps: Vec<Value> = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Formats:
        //   libfoo.so.1 => /path/to/libfoo.so.1 (0xaddr)
        //   linux-vdso.so.1 (0xaddr)          ← virtual, no path
        //   not a dynamic executable
        if line.contains("not a dynamic") {
            continue;
        }

        if let Some(arrow) = line.find(" => ") {
            let lib = line[..arrow].trim();
            let rest = line[arrow + 4..].trim();

            let (resolved, found) = if rest.starts_with("not found") {
                (None, false)
            } else {
                // Strip (0xaddr) suffix
                let path = rest.split_whitespace().next().unwrap_or("").to_owned();
                (Some(path), true)
            };

            deps.push(json!({ "lib": lib, "path": resolved, "found": found }));
        } else {
            // Virtual / vdso entry
            let lib = line.split_whitespace().next().unwrap_or(line);
            deps.push(json!({ "lib": lib, "path": Value::Null, "found": true, "virtual": true }));
        }
    }

    let missing: Vec<&str> = deps
        .iter()
        .filter(|d| d["found"] == false)
        .filter_map(|d| d["lib"].as_str())
        .collect();

    json!({
        "deps": deps,
        "all_found": missing.is_empty(),
        "missing": missing,
    })
}

// ── nm ────────────────────────────────────────────────────────────────────────

fn run_nm(path: &str) -> Value {
    // -D: dynamic symbols only (most useful for shared libs and dynamically linked bins)
    // -C: demangle (Rust/C++ names)
    let out = std::process::Command::new("nm")
        .args(["-D", "-C", "--format=posix", path])
        .output();

    // If -D fails (e.g. static binary), fall back to all symbols
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => match std::process::Command::new("nm")
            .args(["-C", "--format=posix", path])
            .output()
        {
            Ok(o) => o,
            Err(e) => return json!({"error": format!("nm failed: {e}")}),
        },
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut defined: Vec<Value> = Vec::new();
    let mut undefined: Vec<Value> = Vec::new();

    for line in stdout.lines() {
        // posix format: name type [value] [size]
        let mut parts = line.splitn(4, ' ');
        let name = parts.next().unwrap_or("").to_owned();
        let stype = parts.next().unwrap_or("").to_owned();
        let _val = parts.next();
        let size = parts
            .next()
            .and_then(|s| u64::from_str_radix(s.trim(), 16).ok());

        if name.is_empty() {
            continue;
        }

        let entry = json!({ "name": name, "type": symbol_type_name(&stype), "size": size });

        if stype == "U" || stype == "u" {
            undefined.push(entry);
        } else {
            defined.push(entry);
        }
    }

    // Surface the most interesting symbols
    let cuda_syms: Vec<&Value> = defined
        .iter()
        .chain(undefined.iter())
        .filter(|s| {
            s["name"]
                .as_str()
                .map(|n| n.contains("cuda") || n.contains("CUDA"))
                .unwrap_or(false)
        })
        .collect();

    json!({
        "defined_count":   defined.len(),
        "undefined_count": undefined.len(),
        "cuda_symbols":    cuda_syms,
        "undefined":       undefined,
        "defined":         defined,
    })
}

fn symbol_type_name(t: &str) -> &'static str {
    match t {
        "T" | "t" => "text (code)",
        "D" | "d" => "data",
        "B" | "b" => "bss (uninit)",
        "R" | "r" => "read-only",
        "U" | "u" => "undefined (import)",
        "W" | "w" => "weak",
        "V" | "v" => "weak object",
        "A" => "absolute",
        "C" => "common",
        _ => "other",
    }
}

// ── readelf ───────────────────────────────────────────────────────────────────

fn run_readelf(path: &str) -> Value {
    let Ok(out) = std::process::Command::new("readelf")
        .args(["-h", path])
        .output()
    else {
        return json!({"error": "readelf not available"});
    };

    if !out.status.success() {
        return json!({"error": String::from_utf8_lossy(&out.stderr).trim().to_owned()});
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut fields = serde_json::Map::new();

    for line in stdout.lines() {
        if let Some(colon) = line.find(':') {
            let key = line[..colon]
                .trim()
                .to_lowercase()
                .replace(' ', "_")
                .replace('(', "")
                .replace(')', "");
            let val = line[colon + 1..].trim().to_owned();
            if !key.is_empty() && !val.is_empty() {
                fields.insert(key, Value::String(val));
            }
        }
    }

    Value::Object(fields)
}
