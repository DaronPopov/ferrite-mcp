//! Rust symbol indexing — semantic navigation without an LSP.
//!
//! Walks .rs source files and extracts: fn, struct, enum, trait, impl,
//! type alias, const, static, mod. Returns structured [{kind, name, file, line, public}].
//!
//! Tools:
//!   symbol_index — index all symbols in a workspace path
//!   find_symbol  — search for a symbol by name (and optional kind)

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use serde_json::{json, Value};
use regex::Regex;
use crate::protocol::ToolResult;
use crate::server::ServerState;
use crate::tools::state::resolve_or_cwd;

// ── symbol_index ──────────────────────────────────────────────────────────────

pub fn symbol_index(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let root = resolve_or_cwd(state, args["path"].as_str())?;
    let kinds = collect_kinds(args);
    let limit = args["limit"].as_u64().unwrap_or(2000) as usize;

    if !root.exists() {
        return Ok(ToolResult::error(format!("symbol_index: path not found: {}", root.display())));
    }

    let rs_files = collect_rs_files(&root);
    let mut symbols: Vec<Value> = Vec::new();

    for file in &rs_files {
        if symbols.len() >= limit { break; }
        let file_syms = extract_symbols(file, &kinds, limit - symbols.len());
        symbols.extend(file_syms);
    }

    Ok(ToolResult::json(&json!({
        "path":         root.display().to_string(),
        "files_scanned": rs_files.len(),
        "symbol_count": symbols.len(),
        "symbols":      symbols,
    })))
}

// ── find_symbol ───────────────────────────────────────────────────────────────

pub fn find_symbol(args: &Value, state: &Arc<Mutex<ServerState>>) -> Result<ToolResult, String> {
    let name  = args["name"].as_str().ok_or("find_symbol: 'name' required")?;
    let root = resolve_or_cwd(state, args["path"].as_str())?;
    let kinds = collect_kinds(args);
    let exact = args["exact"].as_bool().unwrap_or(false);

    if !root.exists() {
        return Ok(ToolResult::error(format!("find_symbol: path not found: {}", root.display())));
    }

    let rs_files = collect_rs_files(&root);
    let mut matches: Vec<Value> = Vec::new();

    for file in &rs_files {
        let syms = extract_symbols(file, &kinds, usize::MAX);
        for sym in syms {
            let sym_name = sym["name"].as_str().unwrap_or("");
            let hit = if exact {
                sym_name == name
            } else {
                sym_name.to_lowercase().contains(&name.to_lowercase())
            };
            if hit { matches.push(sym); }
        }
    }

    Ok(ToolResult::json(&json!({
        "query":   name,
        "exact":   exact,
        "count":   matches.len(),
        "matches": matches,
    })))
}

// ── extraction ────────────────────────────────────────────────────────────────

fn extract_symbols(file: &PathBuf, kinds: &[String], limit: usize) -> Vec<Value> {
    let Ok(content) = std::fs::read_to_string(file) else { return vec![] };
    let file_str = file.display().to_string();
    let mut results = Vec::new();

    // Compile regexes once per call — symbol_index is not a hot path
    let patterns: Vec<(&str, Regex)> = build_patterns();

    let mut in_block_comment = false;

    for (line_no, line) in content.lines().enumerate() {
        if results.len() >= limit { break; }

        // Track block comments (skip symbol extraction inside them)
        let stripped = line.trim();
        if stripped.starts_with("//") { continue; }
        if stripped.contains("/*") { in_block_comment = true; }
        if stripped.contains("*/") { in_block_comment = false; continue; }
        if in_block_comment { continue; }

        for (kind, re) in &patterns {
            if !kinds.is_empty() && !kinds.contains(&kind.to_string()) { continue; }

            if let Some(caps) = re.captures(line) {
                // Group 1 = pub keyword (if present), Group 2 = name
                let is_public = caps.get(1).map(|m| !m.as_str().is_empty()).unwrap_or(false);
                let name = match caps.get(2) {
                    Some(m) => m.as_str().to_owned(),
                    None    => continue,
                };

                // For impl, extract trait + type
                let mut entry = json!({
                    "kind":   kind,
                    "name":   name,
                    "file":   file_str,
                    "line":   line_no + 1,
                    "public": is_public,
                });

                // Enrich impl with trait_for info
                if *kind == "impl" {
                    if let Some(trait_cap) = caps.get(3) {
                        entry["trait_for"] = json!(trait_cap.as_str());
                    }
                }

                results.push(entry);
                break; // one symbol per line
            }
        }
    }

    results
}

fn build_patterns() -> Vec<(&'static str, Regex)> {
    // Each pattern: group 1 = pub?, group 2 = name
    // For impl: group 1 = ignored, group 2 = type name, group 3 = optional "TraitName for"
    let specs: &[(&str, &str)] = &[
        ("fn",     r"^\s*(pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)"),
        ("struct", r"^\s*(pub(?:\([^)]*\))?\s+)?struct\s+(\w+)"),
        ("enum",   r"^\s*(pub(?:\([^)]*\))?\s+)?enum\s+(\w+)"),
        ("trait",  r"^\s*(pub(?:\([^)]*\))?\s+)?trait\s+(\w+)"),
        ("type",   r"^\s*(pub(?:\([^)]*\))?\s+)?type\s+(\w+)"),
        ("const",  r"^\s*(pub(?:\([^)]*\))?\s+)?const\s+([A-Z_][A-Z0-9_]*)"),
        ("static", r"^\s*(pub(?:\([^)]*\))?\s+)?static\s+(?:mut\s+)?([A-Z_][A-Z0-9_]*)"),
        ("mod",    r"^\s*(pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*[;{]"),
        // impl: no pub prefix; group 2 = type being impl'd; group 3 = optional "TraitName for "
        ("impl",   r"^\s*impl(?:<[^>]*>)?\s+(?:(\w+(?:<[^>]*>)?)\s+for\s+)?(\w+)"),
    ];

    specs.iter()
        .filter_map(|(kind, pat)| {
            Regex::new(pat).ok().map(|re| (*kind, re))
        })
        .collect()
}

fn collect_kinds(args: &Value) -> Vec<String> {
    match &args["kinds"] {
        Value::Array(arr) => arr.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => vec![],
    }
}

fn collect_rs_files(root: &PathBuf) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rs_recursive(root, &mut files, 0);
    files
}

fn collect_rs_recursive(dir: &PathBuf, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 15 { return; }
    let Ok(entries) = std::fs::read_dir(dir) else { return };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        // Skip noisy dirs
        if path.is_dir() {
            if name != "target" && name != ".git" && name != "node_modules" {
                collect_rs_recursive(&path, out, depth + 1);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}
