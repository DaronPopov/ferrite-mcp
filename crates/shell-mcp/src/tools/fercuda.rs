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
use fercuda_ffi::{BufferDesc, BufferDType, LayerNormRequest, MatmulRequest, PoolConfig, Session};

struct SessionEntry {
    owner: String,
    limits: RuntimeLimits,
    session: Session,
    buffers: HashMap<u64, u64>, // buffer_id -> bytes
    allocated_bytes: u64,
    active_jobs: HashSet<u64>,
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

fn store() -> &'static SessionStore {
    STORE.get_or_init(SessionStore::new)
}

fn as_u64(args: &Value, key: &str) -> Result<u64, String> {
    args[key]
        .as_u64()
        .ok_or_else(|| format!("fercuda_runtime: '{key}' is required"))
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
    let op = args["op"]
        .as_str()
        .ok_or("fercuda_runtime: 'op' is required")?;
    let principal = authz::principal();

    match op {
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

            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": op,
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
            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": op,
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
                    "Use tool_define to attach new algorithm adapters as dynamic tools."
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
            };

            let session =
                Session::new(device, Some(cfg)).map_err(|e| format!("fercuda session_create failed: {e}"))?;
            let entry = SessionEntry {
                owner: principal.clone(),
                limits,
                session,
                buffers: HashMap::new(),
                allocated_bytes: 0,
                active_jobs: HashSet::new(),
            };
            let session_id = store().insert(entry)?;
            Ok(ToolResult::json(&json!({
                "ok": true,
                "op": op,
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
                None => Ok(ToolResult::json(&json!({
                    "ok": false,
                    "op": op,
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
                    let removed = guard.remove(&sid).is_some();
                    Ok(ToolResult::json(&json!({
                        "ok": removed,
                        "op": op,
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
                Ok(ToolResult::json(&json!({
                    "ok": true,
                    "op": op,
                    "session_id": sid,
                    "buffer_id": bid,
                    "rank": rank,
                    "dims": &dims[..rank as usize],
                    "bytes": bytes,
                    "allocated_bytes": entry.allocated_bytes,
                })))
            })
        }
        "buffer_free" => {
            let sid = as_u64(args, "session_id")?;
            let bid = as_u64(args, "buffer_id")?;
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
                Ok(ToolResult::json(&json!({
                    "ok": true,
                    "op": op,
                    "session_id": sid,
                    "buffer_id": bid,
                    "freed_bytes": bytes,
                    "allocated_bytes": entry.allocated_bytes,
                })))
            })
        }
        "upload_f32" => {
            let sid = as_u64(args, "session_id")?;
            let bid = as_u64(args, "buffer_id")?;
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
                Ok(ToolResult::json(&json!({
                    "ok": true,
                    "op": op,
                    "session_id": sid,
                    "buffer_id": bid,
                    "count": host.len(),
                })))
            })
        }
        "download_f32" => {
            let sid = as_u64(args, "session_id")?;
            let bid = as_u64(args, "buffer_id")?;
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
                Ok(ToolResult::json(&json!({
                    "ok": true,
                    "op": op,
                    "session_id": sid,
                    "buffer_id": bid,
                    "count": count,
                    "data": host,
                })))
            })
        }
        "submit_matmul" => {
            let sid = as_u64(args, "session_id")?;
            let a = as_u64(args, "a")?;
            let b = as_u64(args, "b")?;
            let out = as_u64(args, "out")?;
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
                    .submit_matmul(MatmulRequest { a, b, out })
                    .map_err(|e| format!("fercuda submit_matmul failed: {e}"))?;
                entry.active_jobs.insert(job);
                Ok(ToolResult::json(&json!({
                    "ok": true,
                    "op": op,
                    "session_id": sid,
                    "job_id": job,
                    "active_jobs": entry.active_jobs.len(),
                })))
            })
        }
        "submit_layer_norm" => {
            let sid = as_u64(args, "session_id")?;
            let x = as_u64(args, "x")?;
            let out = as_u64(args, "out")?;
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
                    .submit_layer_norm(LayerNormRequest { x, out, eps })
                    .map_err(|e| format!("fercuda submit_layer_norm failed: {e}"))?;
                entry.active_jobs.insert(job);
                Ok(ToolResult::json(&json!({
                    "ok": true,
                    "op": op,
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
                Ok(ToolResult::json(&json!({
                    "ok": true,
                    "op": op,
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
                Ok(ToolResult::json(&json!({
                    "ok": true,
                    "op": op,
                    "session_id": sid,
                    "job_id": job,
                    "active_jobs": entry.active_jobs.len(),
                })))
            })
        }
        _ => Err(format!("fercuda_runtime: unknown op '{op}'")),
    }
}
