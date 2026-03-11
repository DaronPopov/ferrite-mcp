//! fercuda_runtime — single MCP entrypoint for feRcuda runtime operations.
//!
//! Phase 2 hardening:
//! - Per-principal session ownership enforcement
//! - Per-role quota checks (bytes, concurrent jobs)

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::authz::{self, RuntimeLimits};
use crate::protocol::ToolResult;
use fercuda_ffi::{AffineF32Request, BufferDesc, BufferDType, JitAccess, JitArgDesc, JitArgKind, JitArgValue, JitBackend, JitLaunchCfg, JitMode, JitOptions, JitSource, JitSourceKind, LayerNormRequest, MatmulRequest, MemoryRegime, PoolConfig, Session, JIT_WILDCARD_U32, JIT_WILDCARD_U64};

struct SessionEntry {
    owner: String,
    limits: RuntimeLimits,
    session: Session,
    buffers: HashMap<u64, u64>, // buffer_id -> bytes
    programs: HashMap<u64, usize>,
    kernels: HashMap<u64, usize>,
    allocated_bytes: u64,
    active_jobs: HashSet<u64>,
    next_program_id: u64,
    next_kernel_id: u64,
    blobs: HashMap<String, Vec<u8>>,
}

struct SessionStore {
    next_id: AtomicU64,
    sessions: Mutex<HashMap<u64, SessionEntry>>,
}

impl SessionStore {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn insert(&self, entry: SessionEntry) -> Result<u64, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut guard = self
            .sessions
            .lock()
            .map_err(|e| format!("session lock poisoned: {e}"))?;
        guard.insert(id, entry);
        Ok(id)
    }
}

static STORE: OnceLock<SessionStore> = OnceLock::new();
const AGENT_API_VERSION: &str = "v1alpha1";

fn store() -> &'static SessionStore {
    STORE.get_or_init(SessionStore::new)
}

fn normalized_args(args: &Value) -> Result<(String, serde_json::Map<String, Value>), String> {
    let mut merged = match args.get("input").and_then(Value::as_object) {
        Some(obj) => obj.clone(),
        None => serde_json::Map::new(),
    };
    if let Some(obj) = args.as_object() {
        for (k, v) in obj {
            if matches!(k.as_str(), "input" | "action" | "agent_api_version") {
                continue;
            }
            merged.insert(k.clone(), v.clone());
        }
    }
    let action = args.get("action").and_then(Value::as_str);
    let op = merged.get("op").and_then(Value::as_str);
    let resolved = if let Some(action) = action {
        action_to_op(action)?.to_owned()
    } else if let Some(op) = op {
        op.to_owned()
    } else {
        return Err("fercuda_runtime: either 'action' or 'op' is required".to_owned());
    };
    merged.insert("op".to_owned(), Value::String(resolved.clone()));
    Ok((resolved, merged))
}

fn action_to_op(action: &str) -> Result<&'static str, String> {
    match action {
        "runtime.inspect" => Ok("status"),
        "runtime.guide" => Ok("guide"),
        "session.create" => Ok("session_create"),
        "session.destroy" => Ok("session_destroy"),
        "tensor.create" => Ok("buffer_alloc"),
        "tensor.destroy" => Ok("buffer_free"),
        "tensor.upload" => Ok("upload_f32"),
        "tensor.download" => Ok("download_f32"),
        "blob.put" => Ok("blob_put"),
        "blob.get" => Ok("blob_get"),
        "tensor.upload_bytes" => Ok("upload_bytes"),
        "tensor.download_bytes" => Ok("download_bytes"),
        "op.matmul.submit" => Ok("submit_matmul"),
        "op.layer_norm.submit" => Ok("submit_layer_norm"),
        "jit.intent.run" => Ok("jit_intent_run"),
        "job.status" => Ok("job_status"),
        "job.wait" => Ok("job_wait"),
        "jit.program.compile" => Ok("jit_compile"),
        "jit.program.release" => Ok("jit_release_program"),
        "jit.kernel.bind" => Ok("jit_get_kernel"),
        "jit.kernel.launch" => Ok("jit_launch"),
        "jit.kernel.release" => Ok("jit_release_kernel"),
        "jit.stats.get" => Ok("jit_stats"),
        other => Err(format!("fercuda_runtime: unknown action '{other}'")),
    }
}

fn canonical_action(op: &str) -> &'static str {
    match op {
        "status" => "runtime.inspect",
        "guide" => "runtime.guide",
        "session_create" => "session.create",
        "session_destroy" => "session.destroy",
        "buffer_alloc" => "tensor.create",
        "buffer_free" => "tensor.destroy",
        "upload_f32" => "tensor.upload",
        "download_f32" => "tensor.download",
        "blob_put" => "blob.put",
        "blob_get" => "blob.get",
        "upload_bytes" => "tensor.upload_bytes",
        "download_bytes" => "tensor.download_bytes",
        "submit_matmul" => "op.matmul.submit",
        "submit_layer_norm" => "op.layer_norm.submit",
        "jit_intent_run" => "jit.intent.run",
        "job_status" => "job.status",
        "job_wait" => "job.wait",
        "jit_compile" => "jit.program.compile",
        "jit_release_program" => "jit.program.release",
        "jit_get_kernel" => "jit.kernel.bind",
        "jit_launch" => "jit.kernel.launch",
        "jit_release_kernel" => "jit.kernel.release",
        "jit_stats" => "jit.stats.get",
        _ => "runtime.unknown",
    }
}

fn error_code_for(reason: &str) -> &'static str {
    let r = reason.to_ascii_lowercase();
    if r.contains("ownership violation") || r.contains("unknown ") || r.contains("not found") {
        "NOT_FOUND"
    } else if r.contains("policy") || r.contains("explicit_allow") || r.contains("explicit_deny") || r.contains("no_matching_allow_rule") {
        "POLICY_DENIED"
    } else if r.contains("exceeds") || r.contains("max_") || r.contains("resource") {
        "RESOURCE_EXHAUSTED"
    } else if r.contains("timed out") || r.contains("timeout") {
        "TIMEOUT"
    } else if r.contains("required") || r.contains("must be") || r.contains("unsupported") || r.contains("invalid") {
        "INVALID_ARGUMENT"
    } else {
        "INTERNAL"
    }
}

fn respond(op: &str, body: Value) -> ToolResult {
    let mut obj = match body {
        Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("result".to_owned(), other);
            map
        }
    };
    obj.entry("ok".to_owned()).or_insert(Value::Bool(true));
    obj.insert("op".to_owned(), Value::String(op.to_owned()));
    obj.insert("action".to_owned(), Value::String(canonical_action(op).to_owned()));
    obj.insert("agent_api_version".to_owned(), Value::String(AGENT_API_VERSION.to_owned()));
    ToolResult::json(&Value::Object(obj))
}

fn respond_error(op: &str, message: impl Into<String>) -> ToolResult {
    let message = message.into();
    ToolResult::json(&json!({
        "ok": false,
        "op": op,
        "action": canonical_action(op),
        "agent_api_version": AGENT_API_VERSION,
        "error": {
            "code": error_code_for(&message),
            "message": message,
            "details": {}
        }
    }))
}

fn as_u64(args: &Value, key: &str) -> Result<u64, String> {
    args[key]
        .as_u64()
        .ok_or_else(|| format!("fercuda_runtime: '{key}' is required"))
}

fn as_handle_u64(args: &Value, primary: &str, alias: &str) -> Result<u64, String> {
    args[primary]
        .as_u64()
        .or_else(|| args[alias].as_u64())
        .ok_or_else(|| format!("fercuda_runtime: '{primary}' or '{alias}' is required"))
}

fn as_u32(args: &Value, key: &str) -> Result<u32, String> {
    args[key]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| format!("fercuda_runtime: '{key}' must be a u32"))
}

fn as_f32(args: &Value, key: &str, default: f32) -> f32 {
    args[key].as_f64().map(|v| v as f32).unwrap_or(default)
}

fn with_session_mut<F>(sid: u64, principal: &str, f: F) -> Result<ToolResult, String>
where
    F: FnOnce(&mut SessionEntry) -> Result<ToolResult, String>,
{
    let mut guard = store()
        .sessions
        .lock()
        .map_err(|e| format!("session lock poisoned: {e}"))?;
    let entry = guard
        .get_mut(&sid)
        .ok_or_else(|| format!("fercuda_runtime: unknown session_id={sid}"))?;
    if entry.owner != principal {
        return Err(format!(
            "fercuda_runtime: session ownership violation session_id={} owner={} caller={}",
            sid, entry.owner, principal
        ));
    }
    f(entry)
}

fn rank_dims_and_bytes(args: &Value) -> Result<(u32, [u32; 4], u64), String> {
    let rank = as_u32(args, "rank")?;
    let dims_arr = args["dims"]
        .as_array()
        .ok_or("fercuda_runtime: 'dims' array is required")?;
    if dims_arr.len() != rank as usize {
        return Err("fercuda_runtime: dims length must match rank".to_owned());
    }
    if rank == 0 || rank > 4 {
        return Err("fercuda_runtime: rank must be in [1,4]".to_owned());
    }
    let mut dims = [0u32; 4];
    let mut elems: u64 = 1;
    for (i, d) in dims_arr.iter().enumerate() {
        let dv = d
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or("fercuda_runtime: dims must be u32 values")?;
        dims[i] = dv;
        elems = elems.saturating_mul(u64::from(dv));
    }
    let bytes = elems.saturating_mul(4); // f32 only for now
    Ok((rank, dims, bytes))
}

fn decode_hex_blob(hex: &str) -> Result<Vec<u8>, String> {
    let clean: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
    if clean.len() % 2 != 0 {
        return Err("fercuda_runtime: blob_hex must have even length".to_owned());
    }
    let mut out = Vec::with_capacity(clean.len() / 2);
    let bytes = clean.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let pair = std::str::from_utf8(&bytes[i..i + 2]).map_err(|_| "fercuda_runtime: blob_hex must be valid ascii")?;
        let b = u8::from_str_radix(pair, 16).map_err(|_| "fercuda_runtime: blob_hex contains non-hex characters")?;
        out.push(b);
    }
    Ok(out)
}

fn encode_hex_blob(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn parse_fusion_mask(value: Option<&Value>) -> Result<u32, String> {
    let Some(value) = value else { return Ok(0) };
    let Some(arr) = value.as_array() else { return Err("fercuda_runtime: intent.fusion must be an array".to_owned()) };
    let mut mask = 0u32;
    for item in arr {
        match item.as_str().ok_or("fercuda_runtime: intent.fusion entries must be strings")? {
            "relu" => mask |= 1u32 << 0,
            "none" => {}
            other => return Err(format!("fercuda_runtime: unsupported fusion '{other}'")),
        }
    }
    Ok(mask)
}

fn parse_caps_mask(value: Option<&Value>) -> Result<u32, String> {
    let Some(value) = value else { return Ok(0) };
    let Some(arr) = value.as_array() else { return Err("fercuda_runtime: intent.caps must be an array".to_owned()) };
    let mut mask = 0u32;
    for item in arr {
        match item.as_str().ok_or("fercuda_runtime: intent.caps entries must be strings")? {
            "tensor_cores" => mask |= 1u32 << 0,
            "coop_groups" => mask |= 1u32 << 1,
            "none" => {}
            other => return Err(format!("fercuda_runtime: unsupported cap '{other}'")),
        }
    }
    Ok(mask)
}

fn authz_policy_path() -> PathBuf {
    if let Ok(p) = std::env::var("FERRITE_AUTHZ_POLICY") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
            PathBuf::from(home).join(".config")
        });
    base.join("ferrite").join("authz_policy.toml")
}

fn install_paths() -> (PathBuf, PathBuf, PathBuf) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
    let base = PathBuf::from(home).join(".local");
    let so = base.join("lib").join("libfercuda_capi.so");
    let a = base.join("lib").join("libfercuda.a");
    let h = base.join("include").join("fercuda").join("c_api.h");
    (so, a, h)
}

pub fn runtime(args: &Value) -> Result<ToolResult, String> {
    let (op, merged) = normalized_args(args)?;
    let args = Value::Object(merged);
    let args = &args;
    let principal = authz::principal();

    let result = match op.as_str() {
        "status" => {
            let limits = authz::runtime_limits_for_principal(&principal);
            let decision = authz::authorize("fercuda_runtime", &json!({ "op": "session_create" }));
            let policy = authz_policy_path();
            let (capi_so, static_a, header) = install_paths();

            let guard = store()
                .sessions
                .lock()
                .map_err(|e| format!("session lock poisoned: {e}"))?;
            let mut owned_ids = Vec::new();
            let mut total_allocated_bytes = 0u64;
            let mut total_active_jobs = 0usize;
            for (sid, entry) in guard.iter() {
                if entry.owner == principal {
                    owned_ids.push(*sid);
                    total_allocated_bytes = total_allocated_bytes.saturating_add(entry.allocated_bytes);
                    total_active_jobs += entry.active_jobs.len();
                }
            }
            drop(guard);

            Ok(respond(&op, json!({
                "principal": principal,
                "install": {
                    "capi_so_path": capi_so.display().to_string(),
                    "capi_so_exists": capi_so.exists(),
                    "static_lib_path": static_a.display().to_string(),
                    "static_lib_exists": static_a.exists(),
                    "header_path": header.display().to_string(),
                    "header_exists": header.exists(),
                },
                "authz": {
                    "policy_path": policy.display().to_string(),
                    "policy_exists": policy.exists(),
                    "limits": limits,
                    "can_create_session_now": decision.allowed,
                    "create_session_reason": decision.reason,
                },
                "sessions": {
                    "owned_count": owned_ids.len(),
                    "owned_session_ids": owned_ids,
                    "total_allocated_bytes": total_allocated_bytes,
                    "total_active_jobs": total_active_jobs,
                },
                "quickstart": [
                    { "tool": "fercuda_runtime", "arguments": { "op": "status" } },
                    { "tool": "ux_wizard", "arguments": { "op": "start", "workflow": "fercuda_authz_limits" } },
                    { "tool": "fercuda_runtime", "arguments": { "op": "session_create", "device": 0 } }
                ]
            })))
        }
        "guide" => {
            Ok(respond(&op, json!({
                "name": "ferrite+feRcuda super-system",
                "single_entry_tool": "fercuda_runtime",
                "recommended_flow": [
                    {
                        "step": 1,
                        "label": "Inspect runtime readiness",
                        "call": { "tool": "fercuda_runtime", "arguments": { "op": "status" } }
                    },
                    {
                        "step": 2,
                        "label": "Configure secure limits",
                        "call": { "tool": "ux_wizard", "arguments": { "op": "start", "workflow": "fercuda_authz_limits" } }
                    },
                    {
                        "step": 3,
                        "label": "Create GPU session",
                        "call": { "tool": "fercuda_runtime", "arguments": { "op": "session_create", "device": 0 } }
                    },
                    {
                        "step": 4,
                        "label": "Attach kernels",
                        "call": { "tool": "fercuda_runtime", "arguments": { "op": "submit_matmul" } }
                    }
                ],
                "notes": [
                    "All mutating runtime ops are enforced by authz policy.",
                    "Preferred contract shape is {action, input, agent_api_version}; legacy {op,...} remains supported."
                ]
            })))
        }
        "session_create" => {
            let limits = authz::runtime_limits_for_principal(&principal).unwrap_or_default();
            let device = args["device"].as_i64().unwrap_or(0) as i32;
            let mutable_bytes = args["mutable_bytes"].as_u64().unwrap_or(512u64 << 20);
            let immutable_bytes = args["immutable_bytes"].as_u64().unwrap_or(2u64 << 30);
            let cuda_reserve = args["cuda_reserve"].as_u64().unwrap_or(256u64 << 20);

            if let Some(max) = limits.max_session_mutable_bytes {
                if mutable_bytes > max {
                    return Err(format!(
                        "fercuda_runtime: mutable_bytes={} exceeds limit={}",
                        mutable_bytes, max
                    ));
                }
            }
            if let Some(max) = limits.max_session_immutable_bytes {
                if immutable_bytes > max {
                    return Err(format!(
                        "fercuda_runtime: immutable_bytes={} exceeds limit={}",
                        immutable_bytes, max
                    ));
                }
            }

            let cfg = PoolConfig {
                mutable_bytes,
                immutable_bytes,
                cuda_reserve,
                verbose: if args["verbose"].as_bool().unwrap_or(false) {
                    1
                } else {
                    0
                },
                memory_regime: MemoryRegime::CustomPool as u32,
            };

            let session =
                Session::new(device, Some(cfg)).map_err(|e| format!("fercuda session_create failed: {e}"))?;
            let entry = SessionEntry {
                owner: principal.clone(),
                limits,
                session,
                buffers: HashMap::new(),
                programs: HashMap::new(),
                kernels: HashMap::new(),
                allocated_bytes: 0,
                active_jobs: HashSet::new(),
                next_program_id: 1,
                next_kernel_id: 1,
                blobs: HashMap::new(),
            };
            let session_id = store().insert(entry)?;
            Ok(respond(&op, json!({
                "session_id": session_id,
                "device": device,
                "owner": principal,
            })))
        }
        "session_destroy" => {
            let sid = as_u64(args, "session_id")?;
            let mut guard = store()
                .sessions
                .lock()
                .map_err(|e| format!("session lock poisoned: {e}"))?;
            let owner = guard.get(&sid).map(|e| e.owner.clone());
            match owner {
                None => Ok(respond(&op, json!({
                    "ok": false,
                    "session_id": sid,
                    "removed": false,
                }))),
                Some(o) => {
                    if o != principal {
                        return Err(format!(
                            "fercuda_runtime: session ownership violation session_id={} owner={} caller={}",
                            sid, o, principal
                        ));
                    }
                    let removed = if let Some(entry) = guard.remove(&sid) {
                        for (_, kernel) in entry.kernels {
                            let _ = entry.session.jit_release_kernel(kernel as _);
                        }
                        for (_, program) in entry.programs {
                            let _ = entry.session.jit_release_program(program as _);
                        }
                        true
                    } else {
                        false
                    };
                    Ok(respond(&op, json!({
                        "ok": removed,
                        "session_id": sid,
                        "removed": removed,
                    })))
                }
            }
        }
        "buffer_alloc" => {
            let sid = as_u64(args, "session_id")?;
            let (rank, dims, bytes) = rank_dims_and_bytes(args)?;
            let immutable = args["immutable"].as_bool().unwrap_or(false);
            let tag = args["tag"].as_u64().unwrap_or(0) as u32;

            with_session_mut(sid, &principal, |entry| {
                if let Some(max) = entry.limits.max_total_alloc_bytes {
                    if entry.allocated_bytes.saturating_add(bytes) > max {
                        return Err(format!(
                            "fercuda_runtime: buffer allocation exceeds max_total_alloc_bytes={} (requested={}, current={})",
                            max, bytes, entry.allocated_bytes
                        ));
                    }
                }
                let desc = BufferDesc {
                    dtype: BufferDType::F32,
                    rank,
                    dims,
                    immutable,
                    tag,
                };
                let bid = entry
                    .session
                    .alloc_buffer(desc)
                    .map_err(|e| format!("fercuda buffer_alloc failed: {e}"))?;
                entry.buffers.insert(bid, bytes);
                entry.allocated_bytes = entry.allocated_bytes.saturating_add(bytes);
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "tensor_id": bid,
                    "rank": rank,
                    "dims": &dims[..rank as usize],
                    "shape": &dims[..rank as usize],
                    "bytes": bytes,
                    "allocated_bytes": entry.allocated_bytes,
                })))
            })
        }
        "blob_put" => {
            let sid = as_u64(args, "session_id")?;
            let blob_id = args["blob_id"].as_str().ok_or("fercuda_runtime: 'blob_id' is required")?;
            let bytes = if let Some(hex) = args["blob_hex"].as_str() {
                decode_hex_blob(hex)?
            } else if let Some(arr) = args["blob_bytes"].as_array() {
                arr.iter()
                    .map(|v| v.as_u64().and_then(|x| u8::try_from(x).ok()).ok_or("fercuda_runtime: blob_bytes must be 0..255"))
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                return Err("fercuda_runtime: 'blob_hex' or 'blob_bytes' is required".to_owned());
            };
            with_session_mut(sid, &principal, |entry| {
                entry.blobs.insert(blob_id.to_owned(), bytes.clone());
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "blob_id": blob_id,
                    "bytes": bytes.len(),
                })))
            })
        }
        "blob_get" => {
            let sid = as_u64(args, "session_id")?;
            let blob_id = args["blob_id"].as_str().ok_or("fercuda_runtime: 'blob_id' is required")?;
            with_session_mut(sid, &principal, |entry| {
                let bytes = entry.blobs.get(blob_id).ok_or_else(|| format!("fercuda_runtime: unknown blob_id={blob_id}"))?;
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "blob_id": blob_id,
                    "bytes": bytes.len(),
                    "blob_hex": encode_hex_blob(bytes),
                })))
            })
        }
        "jit_compile" => {
            let sid = as_u64(args, "session_id")?;
            let source = args["source"]
                .as_str()
                .ok_or("fercuda_runtime: 'source' is required")?;
            let source_kind = match args["source_kind"].as_str().unwrap_or("cuda") {
                "cuda" => JitSourceKind::Cuda,
                "ptx" => JitSourceKind::Ptx,
                other => return Err(format!("fercuda_runtime: unsupported source_kind '{other}'")),
            };
            let backend = match args["backend"].as_str().unwrap_or("nvrtc") {
                "nvrtc" => Some(JitBackend::Nvrtc),
                "auto" => Some(JitBackend::Auto),
                other => return Err(format!("fercuda_runtime: unsupported backend '{other}'")),
            };
            let mode = match args["mode"].as_str().unwrap_or("strict") {
                "strict" => Some(JitMode::Strict),
                "permissive" => Some(JitMode::Permissive),
                other => return Err(format!("fercuda_runtime: unsupported mode '{other}'")),
            };
            let options = JitOptions {
                backend,
                mode,
                arch: args["arch"].as_str().map(ToOwned::to_owned),
                extra_nvrtc_opts: args["extra_nvrtc_opts"].as_str().map(ToOwned::to_owned),
                cache_dir: args["cache_dir"].as_str().map(ToOwned::to_owned),
                enable_disk_cache: args["enable_disk_cache"].as_bool().unwrap_or(false),
            };
            with_session_mut(sid, &principal, |entry| {
                let (program, result) = entry
                    .session
                    .jit_compile(JitSource { kind: source_kind, code: source }, &options)
                    .map_err(|e| format!("fercuda jit_compile failed: {e}"))?;
                let program_id = entry.next_program_id;
                entry.next_program_id += 1;
                entry.programs.insert(program_id, program as usize);
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "program_id": program_id,
                    "cache": { "hit": result.cache_hit },
                    "diagnostics": {
                        "backend_name": result.backend_name,
                        "log": result.log
                    }
                })))
            })
        }
        "jit_get_kernel" => {
            let sid = as_u64(args, "session_id")?;
            let program_id = as_u64(args, "program_id")?;
            let kernel_name = args["kernel_name"]
                .as_str()
                .ok_or("fercuda_runtime: 'kernel_name' is required")?;
            let sig = args["signature"]
                .as_array()
                .ok_or("fercuda_runtime: 'signature' array is required")?;
            let mut descs = Vec::with_capacity(sig.len());
            for item in sig {
                let kind = match item["kind"].as_str().ok_or("fercuda_runtime: signature.kind is required")? {
                    "buffer" => JitArgKind::Buffer,
                    "i32" => JitArgKind::ScalarI32,
                    "u32" => JitArgKind::ScalarU32,
                    "i64" => JitArgKind::ScalarI64,
                    "u64" => JitArgKind::ScalarU64,
                    "f32" => JitArgKind::ScalarF32,
                    "f64" => JitArgKind::ScalarF64,
                    other => return Err(format!("fercuda_runtime: unsupported signature kind '{other}'")),
                };
                let access = match item["access"].as_str().unwrap_or("read_write") {
                    "read" => JitAccess::Read,
                    "write" => JitAccess::Write,
                    "read_write" => JitAccess::ReadWrite,
                    other => return Err(format!("fercuda_runtime: unsupported signature access '{other}'")),
                };
                let mut expected_dims = [JIT_WILDCARD_U32; 4];
                if let Some(arr) = item["expected_dims"].as_array() {
                    if arr.len() > 4 {
                        return Err("fercuda_runtime: expected_dims length must be <= 4".to_owned());
                    }
                    for (i, v) in arr.iter().enumerate() {
                        expected_dims[i] = v.as_u64()
                            .and_then(|x| u32::try_from(x).ok())
                            .ok_or("fercuda_runtime: expected_dims values must be u32")?;
                    }
                }
                descs.push(JitArgDesc {
                    kind,
                    access,
                    name: item["name"].as_str().map(ToOwned::to_owned),
                    expected_dtype: item["expected_dtype"].as_u64().and_then(|x| u32::try_from(x).ok()).unwrap_or(JIT_WILDCARD_U32),
                    expected_rank: item["expected_rank"].as_u64().and_then(|x| u32::try_from(x).ok()).unwrap_or(JIT_WILDCARD_U32),
                    expected_bytes: item["expected_bytes"].as_u64().unwrap_or(JIT_WILDCARD_U64),
                    expected_dims,
                });
            }
            with_session_mut(sid, &principal, |entry| {
                let program = *entry
                    .programs
                    .get(&program_id)
                    .ok_or_else(|| format!("fercuda_runtime: unknown program_id={} for this session", program_id))? as _;
                let kernel = entry
                    .session
                    .jit_get_kernel(program, kernel_name, &descs)
                    .map_err(|e| format!("fercuda jit_get_kernel failed: {e}"))?;
                let kernel_id = entry.next_kernel_id;
                entry.next_kernel_id += 1;
                entry.kernels.insert(kernel_id, kernel as usize);
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "program_id": program_id,
                    "kernel_id": kernel_id,
                    "kernel_name": kernel_name,
                })))
            })
        }
        "jit_launch" => {
            let sid = as_u64(args, "session_id")?;
            let kernel_id = as_u64(args, "kernel_id")?;
            let grid = args["grid"].as_array().ok_or("fercuda_runtime: 'grid' array is required")?;
            let block = args["block"].as_array().ok_or("fercuda_runtime: 'block' array is required")?;
            if grid.len() != 3 || block.len() != 3 {
                return Err("fercuda_runtime: grid and block must be length 3".to_owned());
            }
            let launch = JitLaunchCfg {
                grid_x: grid[0].as_u64().and_then(|v| u32::try_from(v).ok()).ok_or("fercuda_runtime: grid[0] must be u32")?,
                grid_y: grid[1].as_u64().and_then(|v| u32::try_from(v).ok()).ok_or("fercuda_runtime: grid[1] must be u32")?,
                grid_z: grid[2].as_u64().and_then(|v| u32::try_from(v).ok()).ok_or("fercuda_runtime: grid[2] must be u32")?,
                block_x: block[0].as_u64().and_then(|v| u32::try_from(v).ok()).ok_or("fercuda_runtime: block[0] must be u32")?,
                block_y: block[1].as_u64().and_then(|v| u32::try_from(v).ok()).ok_or("fercuda_runtime: block[1] must be u32")?,
                block_z: block[2].as_u64().and_then(|v| u32::try_from(v).ok()).ok_or("fercuda_runtime: block[2] must be u32")?,
                shared_mem_bytes: args["shared_mem_bytes"].as_u64().and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
                memory_regime: args["memory_regime"].as_u64().and_then(|v| u32::try_from(v).ok()).unwrap_or(MemoryRegime::Auto as u32),
            };
            let raw_args = args["args"].as_array().ok_or("fercuda_runtime: 'args' array is required")?;
            with_session_mut(sid, &principal, |entry| {
                if let Some(max) = entry.limits.max_concurrent_jobs {
                    if entry.active_jobs.len() as u64 >= max {
                        return Err(format!("fercuda_runtime: max_concurrent_jobs={} exceeded", max));
                    }
                }
                let kernel = *entry
                    .kernels
                    .get(&kernel_id)
                    .ok_or_else(|| format!("fercuda_runtime: unknown kernel_id={} for this session", kernel_id))? as _;
                let mut launch_args = Vec::with_capacity(raw_args.len());
                for item in raw_args {
                    let kind = item["kind"].as_str().ok_or("fercuda_runtime: jit arg kind is required")?;
                    let arg = match kind {
                        "buffer" => {
                            let bid = item["buffer_id"].as_u64()
                                .or_else(|| item["tensor_id"].as_u64())
                                .ok_or("fercuda_runtime: buffer_id or tensor_id is required")?;
                            if !entry.buffers.contains_key(&bid) {
                                return Err(format!("fercuda_runtime: unknown buffer_id={} for this session", bid));
                            }
                            JitArgValue::Buffer(bid)
                        }
                        "i32" => JitArgValue::I32(item["value"].as_i64().and_then(|v| i32::try_from(v).ok()).ok_or("fercuda_runtime: i32 value required")?),
                        "u32" => JitArgValue::U32(item["value"].as_u64().and_then(|v| u32::try_from(v).ok()).ok_or("fercuda_runtime: u32 value required")?),
                        "i64" => JitArgValue::I64(item["value"].as_i64().ok_or("fercuda_runtime: i64 value required")?),
                        "u64" => JitArgValue::U64(item["value"].as_u64().ok_or("fercuda_runtime: u64 value required")?),
                        "f32" => JitArgValue::F32(item["value"].as_f64().map(|v| v as f32).ok_or("fercuda_runtime: f32 value required")?),
                        "f64" => JitArgValue::F64(item["value"].as_f64().ok_or("fercuda_runtime: f64 value required")?),
                        other => return Err(format!("fercuda_runtime: unsupported jit arg kind '{other}'")),
                    };
                    launch_args.push(arg);
                }
                let job = entry
                    .session
                    .jit_launch(kernel, launch, &launch_args)
                    .map_err(|e| format!("fercuda jit_launch failed: {e}"))?;
                entry.active_jobs.insert(job);
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "kernel_id": kernel_id,
                    "job_id": job,
                    "active_jobs": entry.active_jobs.len(),
                })))
            })
        }
        "jit_release_kernel" => {
            let sid = as_u64(args, "session_id")?;
            let kernel_id = as_u64(args, "kernel_id")?;
            with_session_mut(sid, &principal, |entry| {
                let kernel = entry
                    .kernels
                    .remove(&kernel_id)
                    .ok_or_else(|| format!("fercuda_runtime: unknown kernel_id={} for this session", kernel_id))? as _;
                entry
                    .session
                    .jit_release_kernel(kernel)
                    .map_err(|e| format!("fercuda jit_release_kernel failed: {e}"))?;
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "kernel_id": kernel_id,
                })))
            })
        }
        "jit_release_program" => {
            let sid = as_u64(args, "session_id")?;
            let program_id = as_u64(args, "program_id")?;
            with_session_mut(sid, &principal, |entry| {
                let program = entry
                    .programs
                    .remove(&program_id)
                    .ok_or_else(|| format!("fercuda_runtime: unknown program_id={} for this session", program_id))? as _;
                entry
                    .session
                    .jit_release_program(program)
                    .map_err(|e| format!("fercuda jit_release_program failed: {e}"))?;
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "program_id": program_id,
                })))
            })
        }
        "jit_stats" => {
            let sid = as_u64(args, "session_id")?;
            with_session_mut(sid, &principal, |entry| {
                let stats = entry
                    .session
                    .jit_get_stats()
                    .map_err(|e| format!("fercuda jit_get_stats failed: {e}"))?;
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "compile_count": stats.compile_count,
                    "cache_hit_count": stats.cache_hit_count,
                    "launch_count": stats.launch_count,
                    "compile_time_us": stats.compile_time_us,
                    "launch_time_us": stats.launch_time_us,
                })))
            })
        }
        "buffer_free" => {
            let sid = as_u64(args, "session_id")?;
            let bid = as_handle_u64(args, "buffer_id", "tensor_id")?;
            with_session_mut(sid, &principal, |entry| {
                let bytes = entry
                    .buffers
                    .remove(&bid)
                    .ok_or_else(|| format!("fercuda_runtime: unknown buffer_id={} for this session", bid))?;
                entry
                    .session
                    .free_buffer(bid)
                    .map_err(|e| format!("fercuda buffer_free failed: {e}"))?;
                entry.allocated_bytes = entry.allocated_bytes.saturating_sub(bytes);
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "tensor_id": bid,
                    "freed_bytes": bytes,
                    "allocated_bytes": entry.allocated_bytes,
                })))
            })
        }
        "upload_bytes" => {
            let sid = as_u64(args, "session_id")?;
            let bid = as_handle_u64(args, "buffer_id", "tensor_id")?;
            with_session_mut(sid, &principal, |entry| {
                if !entry.buffers.contains_key(&bid) {
                    return Err(format!("fercuda_runtime: unknown buffer_id={} for this session", bid));
                }
                let bytes = if let Some(blob_id) = args["blob_id"].as_str() {
                    entry.blobs.get(blob_id).cloned().ok_or_else(|| format!("fercuda_runtime: unknown blob_id={blob_id}"))?
                } else if let Some(hex) = args["blob_hex"].as_str() {
                    decode_hex_blob(hex)?
                } else if let Some(arr) = args["blob_bytes"].as_array() {
                    arr.iter()
                        .map(|v| v.as_u64().and_then(|x| u8::try_from(x).ok()).ok_or("fercuda_runtime: blob_bytes must be 0..255"))
                        .collect::<Result<Vec<_>, _>>()?
                } else {
                    return Err("fercuda_runtime: upload_bytes requires blob_id, blob_hex, or blob_bytes".to_owned());
                };
                entry.session.upload_bytes(bid, &bytes).map_err(|e| format!("fercuda upload_bytes failed: {e}"))?;
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "tensor_id": bid,
                    "bytes": bytes.len(),
                })))
            })
        }
        "download_bytes" => {
            let sid = as_u64(args, "session_id")?;
            let bid = as_handle_u64(args, "buffer_id", "tensor_id")?;
            let bytes_len = args["bytes"].as_u64()
                .and_then(|v| usize::try_from(v).ok())
                .or_else(|| args["count"].as_u64().and_then(|v| usize::try_from(v).ok()))
                .ok_or("fercuda_runtime: 'bytes' is required")?;
            let save_blob = args["blob_id"].as_str().map(ToOwned::to_owned);
            with_session_mut(sid, &principal, |entry| {
                if !entry.buffers.contains_key(&bid) {
                    return Err(format!("fercuda_runtime: unknown buffer_id={} for this session", bid));
                }
                let mut host = vec![0u8; bytes_len];
                entry.session.download_bytes(bid, &mut host).map_err(|e| format!("fercuda download_bytes failed: {e}"))?;
                if let Some(blob_id) = &save_blob {
                    entry.blobs.insert(blob_id.clone(), host.clone());
                }
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "tensor_id": bid,
                    "bytes": bytes_len,
                    "blob_id": save_blob,
                    "blob_hex": encode_hex_blob(&host),
                })))
            })
        }
        "upload_f32" => {
            let sid = as_u64(args, "session_id")?;
            let bid = as_handle_u64(args, "buffer_id", "tensor_id")?;
            let data = args["data"]
                .as_array()
                .ok_or("fercuda_runtime: 'data' array is required")?;
            let host: Vec<f32> = data
                .iter()
                .map(|v| {
                    v.as_f64()
                        .map(|x| x as f32)
                        .ok_or("fercuda_runtime: data must be numeric")
                })
                .collect::<Result<Vec<_>, _>>()?;

            with_session_mut(sid, &principal, |entry| {
                if !entry.buffers.contains_key(&bid) {
                    return Err(format!("fercuda_runtime: unknown buffer_id={} for this session", bid));
                }
                entry
                    .session
                    .upload_f32(bid, &host)
                    .map_err(|e| format!("fercuda upload_f32 failed: {e}"))?;
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "tensor_id": bid,
                    "count": host.len(),
                })))
            })
        }
        "download_f32" => {
            let sid = as_u64(args, "session_id")?;
            let bid = as_handle_u64(args, "buffer_id", "tensor_id")?;
            let count = args["count"]
                .as_u64()
                .and_then(|v| usize::try_from(v).ok())
                .ok_or("fercuda_runtime: 'count' is required")?;
            with_session_mut(sid, &principal, |entry| {
                if !entry.buffers.contains_key(&bid) {
                    return Err(format!("fercuda_runtime: unknown buffer_id={} for this session", bid));
                }
                let mut host = vec![0.0f32; count];
                entry
                    .session
                    .download_f32(bid, &mut host)
                    .map_err(|e| format!("fercuda download_f32 failed: {e}"))?;
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "tensor_id": bid,
                    "count": count,
                    "data": host,
                })))
            })
        }
        "jit_intent_run" => {
            let sid = as_u64(args, "session_id")?;
            let intent = args["intent"].as_object().ok_or("fercuda_runtime: 'intent' object is required")?;
            let op_name = intent.get("op").and_then(Value::as_str).unwrap_or("affine_f32");
            if op_name != "affine_f32" {
                return Err(format!("fercuda_runtime: unsupported intent.op '{op_name}'"));
            }
            let input = as_u64(args, "input").or_else(|_| as_u64(args, "input_tensor_id"))?;
            let output = as_u64(args, "output").or_else(|_| as_u64(args, "output_tensor_id"))?;
            let n = intent.get("n").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()).ok_or("fercuda_runtime: intent.n is required")?;
            let alpha = intent.get("alpha").and_then(Value::as_f64).map(|v| v as f32).unwrap_or(1.0);
            let beta = intent.get("beta").and_then(Value::as_f64).map(|v| v as f32).unwrap_or(0.0);
            let fusion_mask = parse_fusion_mask(intent.get("fusion"))?;
            let caps_mask = parse_caps_mask(intent.get("caps"))?;
            let memory_regime = intent.get("memory_regime").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()).unwrap_or(MemoryRegime::Auto as u32);
            with_session_mut(sid, &principal, |entry| {
                for bid in [input, output] {
                    if !entry.buffers.contains_key(&bid) {
                        return Err(format!("fercuda_runtime: unknown buffer_id={} for this session", bid));
                    }
                }
                if let Some(max) = entry.limits.max_concurrent_jobs {
                    if entry.active_jobs.len() as u64 >= max {
                        return Err(format!("fercuda_runtime: max_concurrent_jobs={} exceeded", max));
                    }
                }
                let job = entry.session.run_affine_f32(AffineF32Request {
                    input,
                    output,
                    n,
                    alpha,
                    beta,
                    fusion_mask,
                    caps_mask,
                    memory_regime,
                }).map_err(|e| format!("fercuda jit_intent_run failed: {e}"))?;
                entry.active_jobs.insert(job);
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "input_tensor_id": input,
                    "output_tensor_id": output,
                    "job_id": job,
                    "active_jobs": entry.active_jobs.len(),
                })))
            })
        }
        "submit_matmul" => {
            let sid = as_u64(args, "session_id")?;
            let a = as_u64(args, "a").or_else(|_| as_u64(args, "a_tensor_id"))?;
            let b = as_u64(args, "b").or_else(|_| as_u64(args, "b_tensor_id"))?;
            let out = as_u64(args, "out").or_else(|_| as_u64(args, "out_tensor_id"))?;
            with_session_mut(sid, &principal, |entry| {
                for bid in [a, b, out] {
                    if !entry.buffers.contains_key(&bid) {
                        return Err(format!("fercuda_runtime: unknown buffer_id={} for this session", bid));
                    }
                }
                if let Some(max) = entry.limits.max_concurrent_jobs {
                    if entry.active_jobs.len() as u64 >= max {
                        return Err(format!(
                            "fercuda_runtime: max_concurrent_jobs={} exceeded",
                            max
                        ));
                    }
                }
                let job = entry
                    .session
                    .submit_matmul(MatmulRequest { a, b, out, memory_regime: MemoryRegime::Auto as u32 })
                    .map_err(|e| format!("fercuda submit_matmul failed: {e}"))?;
                entry.active_jobs.insert(job);
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "job_id": job,
                    "active_jobs": entry.active_jobs.len(),
                })))
            })
        }
        "submit_layer_norm" => {
            let sid = as_u64(args, "session_id")?;
            let x = as_u64(args, "x").or_else(|_| as_u64(args, "x_tensor_id"))?;
            let out = as_u64(args, "out").or_else(|_| as_u64(args, "out_tensor_id"))?;
            let eps = as_f32(args, "eps", 1e-6f32);
            with_session_mut(sid, &principal, |entry| {
                for bid in [x, out] {
                    if !entry.buffers.contains_key(&bid) {
                        return Err(format!("fercuda_runtime: unknown buffer_id={} for this session", bid));
                    }
                }
                if let Some(max) = entry.limits.max_concurrent_jobs {
                    if entry.active_jobs.len() as u64 >= max {
                        return Err(format!(
                            "fercuda_runtime: max_concurrent_jobs={} exceeded",
                            max
                        ));
                    }
                }
                let job = entry
                    .session
                    .submit_layer_norm(LayerNormRequest { x, out, eps, memory_regime: MemoryRegime::Auto as u32 })
                    .map_err(|e| format!("fercuda submit_layer_norm failed: {e}"))?;
                entry.active_jobs.insert(job);
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "job_id": job,
                    "active_jobs": entry.active_jobs.len(),
                })))
            })
        }
        "job_status" => {
            let sid = as_u64(args, "session_id")?;
            let job = as_u64(args, "job_id")?;
            with_session_mut(sid, &principal, |entry| {
                let done = entry
                    .session
                    .job_status(job)
                    .map_err(|e| format!("fercuda job_status failed: {e}"))?;
                if done {
                    entry.active_jobs.remove(&job);
                }
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "job_id": job,
                    "done": done,
                    "active_jobs": entry.active_jobs.len(),
                })))
            })
        }
        "job_wait" => {
            let sid = as_u64(args, "session_id")?;
            let job = as_u64(args, "job_id")?;
            with_session_mut(sid, &principal, |entry| {
                entry
                    .session
                    .job_wait(job)
                    .map_err(|e| format!("fercuda job_wait failed: {e}"))?;
                entry.active_jobs.remove(&job);
                Ok(respond(&op, json!({
                    "session_id": sid,
                    "job_id": job,
                    "active_jobs": entry.active_jobs.len(),
                })))
            })
        }
        _ => Err(format!("fercuda_runtime: unknown op '{op}'")),
    };
    Ok(match result {
        Ok(v) => v,
        Err(e) => respond_error(&op, e),
    })
}
