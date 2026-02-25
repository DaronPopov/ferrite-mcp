//! find_lib and discover tool implementations.
//!
//! Discovery strategy for each tool:
//!   1. Environment variables  (e.g. CUDA_HOME, LIBTORCH)
//!   2. pkg-config
//!   3. ldconfig cache
//!   4. Well-known filesystem paths
//!   5. PATH lookup for associated binaries

use serde_json::{json, Value};

use crate::protocol::ToolResult;

// ── find_lib ──────────────────────────────────────────────────────────────────

pub fn find_lib(args: &Value) -> Result<ToolResult, String> {
    let name = args["name"].as_str().ok_or("find_lib: 'name' is required")?;
    let _version_hint = args["version_hint"].as_str();

    let result = probe_library(name);
    Ok(ToolResult::json(&result))
}

fn probe_library(name: &str) -> Value {
    // 1. pkg-config
    if let Some(v) = try_pkg_config(name) {
        return v;
    }

    // 2. Env-var hints  (e.g. LIBTORCH=/opt/libtorch)
    if let Some(v) = try_env_hint(name) {
        return v;
    }

    // 3. ldconfig search
    if let Some(path) = try_ldconfig(name) {
        return json!({
            "found": true,
            "name": name,
            "path": path,
            "source": "ldconfig",
        });
    }

    // 4. Common filesystem paths
    if let Some(path) = try_filesystem_paths(name) {
        return json!({
            "found": true,
            "name": name,
            "path": path,
            "source": "filesystem",
        });
    }

    json!({ "found": false, "name": name })
}

fn try_pkg_config(name: &str) -> Option<Value> {
    // Try the name as-is, then with common prefixes/suffixes
    for candidate in &[name.to_owned(), format!("lib{name}"), name.to_lowercase()] {
        if let Ok(out) = std::process::Command::new("pkg-config")
            .args(["--modversion", "--libs", "--cflags", candidate])
            .output()
        {
            if out.status.success() {
                let version  = String::from_utf8_lossy(&out.stdout).trim().lines().next()
                    .unwrap_or("").to_owned();

                if let (Ok(libs), Ok(cflags)) = (
                    std::process::Command::new("pkg-config").args(["--libs",   candidate]).output(),
                    std::process::Command::new("pkg-config").args(["--cflags", candidate]).output(),
                ) {
                    let link_flags   = String::from_utf8_lossy(&libs.stdout).trim().to_owned();
                    let include_dirs: Vec<String> = String::from_utf8_lossy(&cflags.stdout)
                        .split_whitespace()
                        .filter(|f| f.starts_with("-I"))
                        .map(|f| f[2..].to_owned())
                        .collect();

                    return Some(json!({
                        "found": true,
                        "name": name,
                        "version": version,
                        "link_flags": link_flags,
                        "include_dirs": include_dirs,
                        "pkg_config_name": candidate,
                        "source": "pkg-config",
                    }));
                }
            }
        }
    }
    None
}

fn try_env_hint(name: &str) -> Option<Value> {
    let upper = name.to_uppercase().replace('-', "_");
    let candidates = [
        upper.clone(),
        format!("{upper}_DIR"),
        format!("{upper}_HOME"),
        format!("{upper}_PATH"),
        format!("{upper}_ROOT"),
    ];

    for key in &candidates {
        if let Ok(val) = std::env::var(key) {
            let path = std::path::Path::new(&val);
            if path.exists() {
                let include = path.join("include");
                let lib     = path.join("lib");
                return Some(json!({
                    "found": true,
                    "name": name,
                    "path": val,
                    "include_dirs": if include.exists() { vec![include.display().to_string()] } else { vec![] },
                    "lib_dir": if lib.exists() { Some(lib.display().to_string()) } else { None },
                    "source": format!("env:{key}"),
                }));
            }
        }
    }
    None
}

fn try_ldconfig(name: &str) -> Option<String> {
    let out = std::process::Command::new("ldconfig")
        .arg("-p")
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if lower.contains(&format!("lib{}", name.to_lowercase()))
            || lower.contains(&name.to_lowercase())
        {
            // ldconfig -p lines look like: "\tlib<name>.so.X => /path/to/lib<name>.so.X"
            if let Some(pos) = line.rfind("=>") {
                let path = line[pos + 2..].trim().to_owned();
                if !path.is_empty() {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn try_filesystem_paths(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    let candidates = [
        format!("/usr/lib/lib{lower}.so"),
        format!("/usr/local/lib/lib{lower}.so"),
        format!("/usr/lib/x86_64-linux-gnu/lib{lower}.so"),
        format!("/usr/lib/aarch64-linux-gnu/lib{lower}.so"),
        format!("/opt/{lower}/lib/lib{lower}.so"),
        format!("/usr/lib/lib{lower}.a"),
        format!("/usr/local/lib/lib{lower}.a"),
    ];
    candidates.into_iter().find(|p| std::path::Path::new(p).exists())
}

// ── discover ──────────────────────────────────────────────────────────────────

pub fn discover(args: &Value) -> Result<ToolResult, String> {
    let category = args["category"].as_str().ok_or("discover: 'category' is required")?;

    let result = match category {
        "cuda"        => discover_cuda(),
        "rocm"        => discover_rocm(),
        "ml"          => discover_ml(),
        "build-tools" => discover_build_tools(),
        "all" => {
            json!({
                "cuda":        discover_cuda(),
                "rocm":        discover_rocm(),
                "ml":          discover_ml(),
                "build_tools": discover_build_tools(),
            })
        }
        other => return Ok(ToolResult::error(format!("unknown category: {other}"))),
    };

    Ok(ToolResult::json(&result))
}

fn discover_cuda() -> Value {
    // Find CUDA installation root
    let cuda_root = find_cuda_root();

    let nvcc = which_path("nvcc")
        .or_else(|| cuda_root.as_ref().map(|r| format!("{r}/bin/nvcc")))
        .filter(|p| std::path::Path::new(p).exists());

    let nvcc_version = nvcc.as_ref().and_then(|p| run_version_flag(p, "--version"));

    let cudnn = find_cudnn(cuda_root.as_deref());
    let nccl  = find_nccl(cuda_root.as_deref());

    let libcuda = try_ldconfig("cuda").or_else(|| {
        ["/usr/lib/x86_64-linux-gnu/libcuda.so",
         "/usr/lib/aarch64-linux-gnu/libcuda.so"]
            .iter().find(|p| std::path::Path::new(p).exists())
            .map(|s| s.to_string())
    });

    let gpu_info = run_cmd("nvidia-smi", &["--query-gpu=name,driver_version,memory.total",
                                            "--format=csv,noheader"]);

    json!({
        "cuda_root": cuda_root,
        "nvcc": nvcc,
        "nvcc_version": nvcc_version,
        "libcuda": libcuda,
        "cudnn": cudnn,
        "nccl": nccl,
        "gpus": gpu_info,
    })
}

fn find_cuda_root() -> Option<String> {
    // Env vars first
    for var in ["CUDA_HOME", "CUDA_PATH", "CUDA_ROOT"] {
        if let Ok(v) = std::env::var(var) {
            if std::path::Path::new(&v).exists() {
                return Some(v);
            }
        }
    }
    // Standard install path
    if std::path::Path::new("/usr/local/cuda").exists() {
        return Some("/usr/local/cuda".to_owned());
    }
    // Versioned paths: /usr/local/cuda-12.x
    if let Ok(entries) = std::fs::read_dir("/usr/local") {
        let mut versioned: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("cuda-"))
            .map(|n| format!("/usr/local/{n}"))
            .collect();
        versioned.sort();
        if let Some(last) = versioned.last() {
            return Some(last.clone());
        }
    }
    None
}

fn find_cudnn(cuda_root: Option<&str>) -> Value {
    let search_roots: Vec<String> = [
        cuda_root.map(|r| r.to_owned()),
        Some("/usr".to_owned()),
        Some("/usr/local".to_owned()),
    ]
    .into_iter()
    .flatten()
    .collect();

    for root in &search_roots {
        let header = format!("{root}/include/cudnn.h");
        let header_v8 = format!("{root}/include/cudnn_version.h");

        let found_header = [&header, &header_v8]
            .iter()
            .find(|p| std::path::Path::new(p).exists())
            .map(|p| p.to_string());

        if let Some(h) = found_header {
            let version = extract_define_from_header(&h, "CUDNN_MAJOR")
                .zip(extract_define_from_header(&h, "CUDNN_MINOR"))
                .zip(extract_define_from_header(&h, "CUDNN_PATCHLEVEL"))
                .map(|((maj, min), patch)| format!("{maj}.{min}.{patch}"));

            return json!({ "found": true, "header": h, "version": version });
        }
    }
    json!({ "found": false })
}

fn find_nccl(cuda_root: Option<&str>) -> Value {
    let search_roots: Vec<String> = [
        cuda_root.map(|r| r.to_owned()),
        Some("/usr".to_owned()),
        Some("/usr/local".to_owned()),
    ]
    .into_iter()
    .flatten()
    .collect();

    for root in &search_roots {
        let header = format!("{root}/include/nccl.h");
        if std::path::Path::new(&header).exists() {
            let version = extract_define_from_header(&header, "NCCL_MAJOR")
                .zip(extract_define_from_header(&header, "NCCL_MINOR"))
                .zip(extract_define_from_header(&header, "NCCL_PATCH"))
                .map(|((maj, min), patch)| format!("{maj}.{min}.{patch}"));
            return json!({ "found": true, "header": header, "version": version });
        }
    }
    json!({ "found": false })
}

fn discover_rocm() -> Value {
    let rocm_root = ["/opt/rocm", "/usr/local/rocm"]
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|p| p.to_string())
        .or_else(|| std::env::var("ROCM_PATH").ok());

    let hipcc = which_path("hipcc").or_else(|| {
        rocm_root.as_ref().map(|r| format!("{r}/bin/hipcc"))
            .filter(|p| std::path::Path::new(p).exists())
    });

    let version = rocm_root.as_ref().and_then(|r| {
        std::fs::read_to_string(format!("{r}/.info/version")).ok()
            .map(|s| s.trim().to_owned())
    });

    json!({
        "rocm_root": rocm_root,
        "hipcc": hipcc,
        "version": version,
    })
}

fn discover_ml() -> Value {
    // LibTorch
    let libtorch = {
        let root = std::env::var("LIBTORCH")
            .or_else(|_| std::env::var("TORCH_LIB"))
            .ok()
            .filter(|p| std::path::Path::new(p).exists())
            .or_else(|| {
                // Common Python-installed locations
                let candidates = [
                    "/usr/local/lib/python3.*/dist-packages/torch",
                    "/usr/lib/python3/dist-packages/torch",
                    "/opt/conda/lib/python3.*/site-packages/torch",
                ];
                candidates.iter().find_map(|pattern| {
                    glob::glob(pattern).ok()?.find_map(|e| {
                        e.ok().filter(|p| p.exists()).map(|p| p.display().to_string())
                    })
                })
            });

        let version = root.as_ref().and_then(|r| {
            std::fs::read_to_string(format!("{r}/version.txt")).ok()
                .map(|s| s.trim().to_owned())
        });

        json!({ "found": root.is_some(), "path": root, "version": version })
    };

    // ONNX Runtime
    let ort = {
        let path = std::env::var("ORT_DYLIB_PATH")
            .ok()
            .filter(|p| std::path::Path::new(p).exists())
            .or_else(|| try_ldconfig("onnxruntime"));
        json!({ "found": path.is_some(), "path": path })
    };

    // Python (check for ml-relevant packages)
    let python = which_path("python3").map(|p| {
        let version = run_version_flag(&p, "--version");
        json!({ "path": p, "version": version })
    });

    json!({
        "libtorch": libtorch,
        "onnxruntime": ort,
        "python3": python,
    })
}

fn discover_build_tools() -> Value {
    let tools = [
        ("gcc",     &["--version"][..]),
        ("g++",     &["--version"]),
        ("clang",   &["--version"]),
        ("clang++", &["--version"]),
        ("cmake",   &["--version"]),
        ("ninja",   &["--version"]),
        ("make",    &["--version"]),
        ("cargo",   &["--version"]),
        ("rustc",   &["--version"]),
        ("nvcc",    &["--version"]),
        ("python3", &["--version"]),
        ("pip3",    &["--version"]),
        ("meson",   &["--version"]),
        ("bazel",   &["--version"]),
    ];

    let mut result = serde_json::Map::new();
    for (name, version_args) in tools {
        if let Some(path) = which_path(name) {
            let version = std::process::Command::new(&path)
                .args(version_args)
                .output()
                .ok()
                .and_then(|o| {
                    let text = String::from_utf8_lossy(&o.stdout).to_string()
                        + &String::from_utf8_lossy(&o.stderr);
                    text.lines().find(|l| !l.trim().is_empty()).map(|l| l.trim().to_owned())
                });
            result.insert(name.to_owned(), json!({ "found": true, "path": path, "version": version }));
        } else {
            result.insert(name.to_owned(), json!({ "found": false }));
        }
    }
    Value::Object(result)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn which_path(name: &str) -> Option<String> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    path_var
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|dir| std::path::PathBuf::from(dir).join(name))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
        })
        .map(|p| p.display().to_string())
}

fn run_version_flag(bin: &str, flag: &str) -> Option<String> {
    std::process::Command::new(bin)
        .arg(flag)
        .output()
        .ok()
        .and_then(|o| {
            let text = String::from_utf8_lossy(&o.stdout).to_string()
                + &String::from_utf8_lossy(&o.stderr);
            text.lines().find(|l| !l.trim().is_empty()).map(|l| l.trim().to_owned())
        })
}

fn run_cmd(bin: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(bin)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Parse a `#define NAME VALUE` line from a C header.
fn extract_define_from_header(path: &str, define: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with("#define") {
            let parts: Vec<&str> = t.split_whitespace().collect();
            if parts.get(1) == Some(&define) {
                if let Some(val) = parts.get(2) {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}
