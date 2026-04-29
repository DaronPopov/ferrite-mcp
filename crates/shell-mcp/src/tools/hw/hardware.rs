//! gpu_info and cpu_info tool implementations.
//!
//! gpu_info strategy — Linux:
//!
//! 1. Compile and run a minimal CUDA deviceQuery program via nvcc   ← most accurate
//! 2. Parse nvidia-smi --query-gpu output                           ← no CUDA SDK needed
//! 3. Return "no GPU detected"
//!
//! gpu_info strategy — macOS:
//! Returns GPU name from system_profiler; utilization unavailable without sudo.
//!
//! cpu_info strategy — Linux:
//!
//! 1. Parse /proc/cpuinfo
//! 2. Parse /sys for cache topology
//!
//! cpu_info strategy — macOS:
//! Uses sysctl to query hw.* and machdep.cpu.*

use serde_json::{json, Value};

use crate::protocol::ToolResult;

// ── GPU info — Linux ──────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
use std::sync::OnceLock;

/// CUDA C source for a minimal device query — compiled once per session.
#[cfg(target_os = "linux")]
const GPU_QUERY_SRC: &str = r#"
#include <stdio.h>
#include <cuda_runtime.h>
int main(void) {
    int n = 0;
    if (cudaGetDeviceCount(&n) != cudaSuccess || n == 0) return 1;
    printf("[");
    for (int i = 0; i < n; i++) {
        cudaDeviceProp p;
        cudaGetDeviceProperties(&p, i);
        double bw = 2.0 * p.memoryClockRate * (p.memoryBusWidth / 8.0) / 1.0e6;
        if (i) printf(",");
        printf("{");
        printf("\"index\":%d,",                   i);
        printf("\"name\":\"%s\",",                p.name);
        printf("\"compute_major\":%d,",           p.major);
        printf("\"compute_minor\":%d,",           p.minor);
        printf("\"multiprocessors\":%d,",         p.multiProcessorCount);
        printf("\"warp_size\":%d,",               p.warpSize);
        printf("\"max_threads_per_block\":%d,",   p.maxThreadsPerBlock);
        printf("\"max_threads_per_sm\":%d,",      p.maxThreadsPerMultiProcessor);
        printf("\"shared_mem_per_block_kb\":%zu,",p.sharedMemPerBlock / 1024);
        printf("\"l2_cache_kb\":%d,",             p.l2CacheSize / 1024);
        printf("\"total_vram_mb\":%zu,",          p.totalGlobalMem / (1024*1024));
        printf("\"memory_clock_mhz\":%d,",        p.memoryClockRate / 1000);
        printf("\"memory_bus_bits\":%d,",         p.memoryBusWidth);
        printf("\"peak_bandwidth_gbps\":%.1f,",   bw);
        printf("\"registers_per_block\":%d,",     p.regsPerBlock);
        printf("\"concurrent_kernels\":%s,",      p.concurrentKernels ? "true":"false");
        printf("\"unified_addressing\":%s,",      p.unifiedAddressing  ? "true":"false");
        printf("\"ecc_enabled\":%s",              p.ECCEnabled         ? "true":"false");
        printf("}");
    }
    printf("]\n");
    return 0;
}
"#;

/// Cached result — GPU topology doesn't change during a session.
#[cfg(target_os = "linux")]
static GPU_CACHE: OnceLock<Value> = OnceLock::new();

#[cfg(target_os = "linux")]
pub fn gpu_info(_args: &Value) -> Result<ToolResult, String> {
    let data = GPU_CACHE.get_or_init(collect_gpu_info);
    Ok(ToolResult::json(data))
}

#[cfg(target_os = "linux")]
fn collect_gpu_info() -> Value {
    // Strategy 1: compile + run CUDA query
    if let Some(devices) = try_cuda_device_query() {
        let smi = nvidia_smi_extra();
        return json!({
            "source": "cuda_runtime",
            "devices": devices,
            "driver_info": smi,
        });
    }

    // Strategy 2: nvidia-smi only (no CUDA SDK needed)
    if let Some(smi) = nvidia_smi_full() {
        return json!({
            "source": "nvidia-smi",
            "devices": smi,
            "note": "nvcc not available — compile-time device properties unavailable",
        });
    }

    json!({ "source": "none", "devices": [], "note": "No NVIDIA GPU or driver detected" })
}

#[cfg(target_os = "linux")]
fn try_cuda_device_query() -> Option<Value> {
    // Need nvcc
    let nvcc = which_bin("nvcc")?;

    let dir = std::env::temp_dir();
    let src = dir.join("ferrite_gpuq.cu");
    let bin = dir.join("ferrite_gpuq");

    std::fs::write(&src, GPU_QUERY_SRC).ok()?;

    // Find CUDA include dir
    let cuda_include = cuda_include_dir();

    let mut cmd = std::process::Command::new(&nvcc);
    cmd.arg("-o").arg(&bin).arg(&src);
    if let Some(inc) = &cuda_include {
        cmd.arg(format!("-I{inc}"));
    }
    // Suppress nvcc progress output
    cmd.arg("-w");

    let compile = cmd.output().ok()?;
    if !compile.status.success() {
        return None;
    }

    let run = std::process::Command::new(&bin).output().ok()?;
    std::fs::remove_file(&src).ok();
    std::fs::remove_file(&bin).ok();

    if !run.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&run.stdout);
    serde_json::from_str(stdout.trim()).ok()
}

#[cfg(target_os = "linux")]
fn nvidia_smi_full() -> Option<Value> {
    let fields = "index,name,driver_version,memory.total,memory.free,\
                  utilization.gpu,temperature.gpu,compute_cap,pci.bus_id";
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu", fields, "--format=csv,noheader,nounits"])
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let mut devices = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let parts: Vec<&str> = line.split(", ").collect();
        if parts.len() < 9 {
            continue;
        }
        devices.push(json!({
            "index":          parts[0].trim().parse::<u32>().unwrap_or(0),
            "name":           parts[1].trim(),
            "driver_version": parts[2].trim(),
            "vram_total_mb":  parts[3].trim().parse::<u32>().ok(),
            "vram_free_mb":   parts[4].trim().parse::<u32>().ok(),
            "util_pct":       parts[5].trim().parse::<u32>().ok(),
            "temp_c":         parts[6].trim().parse::<u32>().ok(),
            "compute_cap":    parts[7].trim(),
            "pci_bus":        parts[8].trim(),
        }));
    }

    if devices.is_empty() {
        None
    } else {
        Some(Value::Array(devices))
    }
}

#[cfg(target_os = "linux")]
fn nvidia_smi_extra() -> Value {
    // Real-time stats to augment the compile-time data
    let fields = "index,driver_version,memory.used,memory.free,\
                  utilization.gpu,utilization.memory,temperature.gpu,power.draw";
    let Ok(out) = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu", fields, "--format=csv,noheader,nounits"])
        .output()
    else {
        return Value::Null;
    };

    let mut stats = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let p: Vec<&str> = line.split(", ").collect();
        if p.len() < 8 {
            continue;
        }
        stats.push(json!({
            "index":          p[0].trim().parse::<u32>().unwrap_or(0),
            "driver_version": p[1].trim(),
            "vram_used_mb":   p[2].trim().parse::<u32>().ok(),
            "vram_free_mb":   p[3].trim().parse::<u32>().ok(),
            "gpu_util_pct":   p[4].trim().parse::<u32>().ok(),
            "mem_util_pct":   p[5].trim().parse::<u32>().ok(),
            "temp_c":         p[6].trim().parse::<u32>().ok(),
            "power_w":        p[7].trim().parse::<f32>().ok(),
        }));
    }
    Value::Array(stats)
}

#[cfg(target_os = "linux")]
fn cuda_include_dir() -> Option<String> {
    for var in ["CUDA_HOME", "CUDA_PATH", "CUDA_ROOT"] {
        if let Ok(v) = std::env::var(var) {
            let inc = format!("{v}/include");
            if std::path::Path::new(&inc).exists() {
                return Some(inc);
            }
        }
    }
    for candidate in ["/usr/local/cuda/include", "/usr/include/cuda"] {
        if std::path::Path::new(candidate).exists() {
            return Some(candidate.to_owned());
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn which_bin(name: &str) -> Option<String> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    path_var
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| std::path::PathBuf::from(d).join(name))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
        .map(|p| p.display().to_string())
}

// ── GPU info — macOS ──────────────────────────────────────────────────────────

/// Return Apple GPU name from system_profiler.
#[cfg(target_os = "macos")]
fn apple_gpu_name() -> Option<String> {
    let out = std::process::Command::new("system_profiler")
        .args(["SPDisplaysDataType"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.contains("Chipset Model"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| s.trim().to_owned())
}

#[cfg(target_os = "macos")]
pub fn gpu_info(_args: &Value) -> Result<ToolResult, String> {
    let name = apple_gpu_name().unwrap_or_else(|| "Apple GPU".to_owned());
    Ok(ToolResult::json(&json!({
        "source":  "system_profiler",
        "devices": [{ "name": name }],
    })))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn gpu_info(_args: &Value) -> Result<ToolResult, String> {
    Ok(ToolResult::json(&json!({
        "source": "unsupported",
        "devices": [],
        "note": "gpu_info currently supports Linux NVIDIA GPUs and macOS display devices",
    })))
}

// ── CPU info ──────────────────────────────────────────────────────────────────

pub fn cpu_info(_args: &Value) -> Result<ToolResult, String> {
    let info = collect_cpu_info();
    Ok(ToolResult::json(&info))
}

#[cfg(target_os = "linux")]
fn collect_cpu_info() -> Value {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();

    let model = cpuinfo
        .lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());

    let logical_cores = cpuinfo
        .lines()
        .filter(|l| l.starts_with("processor"))
        .count();

    let sockets = cpuinfo
        .lines()
        .filter(|l| l.starts_with("physical id"))
        .filter_map(|l| {
            l.split(':')
                .nth(1)
                .and_then(|s| s.trim().parse::<usize>().ok())
        })
        .max()
        .map(|m| m + 1)
        .unwrap_or(1);

    let physical_cores = linux_physical_core_count(&cpuinfo).unwrap_or_else(|| {
        cpuinfo
            .lines()
            .find(|l| l.starts_with("cpu cores"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|s| s.trim().parse::<usize>().ok())
            .map(|cores_per_socket| cores_per_socket.saturating_mul(sockets))
            .unwrap_or(logical_cores)
    });

    // SIMD flags
    let flags_line = cpuinfo
        .lines()
        .find(|l| l.starts_with("flags") || l.starts_with("Features"))
        .and_then(|l| l.split(':').nth(1))
        .unwrap_or("")
        .to_owned();

    let simd = extract_simd_flags(&flags_line);

    // Cache sizes from /sys
    let caches = read_cache_topology();

    // MHz
    let freq_mhz = cpuinfo
        .lines()
        .find(|l| l.starts_with("cpu MHz"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|s| s.trim().parse::<f64>().ok());

    json!({
        "model": model,
        "sockets": sockets,
        "physical_cores": physical_cores,
        "logical_cores": logical_cores,
        "freq_mhz": freq_mhz,
        "simd": simd,
        "caches": caches,
    })
}

#[cfg(target_os = "linux")]
fn linux_physical_core_count(cpuinfo: &str) -> Option<usize> {
    use std::collections::HashSet;

    let mut cores = HashSet::new();
    let mut physical_id: Option<String> = None;
    let mut core_id: Option<String> = None;
    let mut saw_topology = false;

    for line in cpuinfo.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if let Some(core) = core_id.take() {
                let socket = physical_id.take().unwrap_or_else(|| "0".to_owned());
                cores.insert((socket, core));
            }
            physical_id = None;
            continue;
        }

        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            if key == "physical id" {
                physical_id = Some(value.to_owned());
                saw_topology = true;
            } else if key == "core id" {
                core_id = Some(value.to_owned());
                saw_topology = true;
            }
        }
    }

    saw_topology
        .then_some(cores.len())
        .filter(|count| *count > 0)
}

#[cfg(target_os = "linux")]
fn extract_simd_flags(flags: &str) -> Value {
    let flag_set: std::collections::HashSet<&str> = flags.split_whitespace().collect();
    json!({
        "sse4_2":  flag_set.contains("sse4_2"),
        "avx":     flag_set.contains("avx"),
        "avx2":    flag_set.contains("avx2"),
        "avx512f": flag_set.contains("avx512f"),
        "avx512bw":flag_set.contains("avx512bw"),
        "avx512cd":flag_set.contains("avx512cd"),
        "avx512vl":flag_set.contains("avx512vl"),
        "fma":     flag_set.contains("fma"),
        "neon":    flag_set.contains("neon") || flag_set.contains("asimd"),
        "sve":     flag_set.contains("sve"),
        "sve2":    flag_set.contains("sve2"),
    })
}

#[cfg(target_os = "linux")]
fn read_cache_topology() -> Value {
    let mut caches = Vec::new();
    let base = std::path::Path::new("/sys/devices/system/cpu/cpu0/cache");
    if !base.exists() {
        return Value::Array(caches);
    }

    if let Ok(entries) = std::fs::read_dir(base) {
        let mut indices: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        indices.sort_by_key(|e| e.file_name());

        for entry in indices {
            let p = entry.path();
            let level = read_sysfs(&p, "level").and_then(|s| s.parse::<u32>().ok());
            let ctype = read_sysfs(&p, "type");
            let size = read_sysfs(&p, "size");
            let ways = read_sysfs(&p, "ways_of_associativity").and_then(|s| s.parse::<u32>().ok());
            if level.is_some() {
                caches.push(json!({ "level": level, "type": ctype, "size": size, "ways": ways }));
            }
        }
    }
    Value::Array(caches)
}

#[cfg(target_os = "linux")]
fn read_sysfs(base: &std::path::Path, file: &str) -> Option<String> {
    std::fs::read_to_string(base.join(file))
        .ok()
        .map(|s| s.trim().to_owned())
}

// ── CPU info — macOS ──────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn collect_cpu_info() -> Value {
    let model = sysctl_str("machdep.cpu.brand_string")
        .or_else(|| sysctl_str("hw.model"))
        .unwrap_or_else(|| "unknown".to_owned());
    let logical = sysctl_u64("hw.logicalcpu").unwrap_or(0) as usize;
    let physical = sysctl_u64("hw.physicalcpu").unwrap_or(0) as usize;

    let mut caches = vec![];
    if let Some(s) = sysctl_u64("hw.l1dcachesize") {
        caches.push(json!({"level": 1, "type": "Data",    "size": s}));
    }
    if let Some(s) = sysctl_u64("hw.l2cachesize") {
        caches.push(json!({"level": 2, "type": "Unified", "size": s}));
    }
    if let Some(s) = sysctl_u64("hw.l3cachesize") {
        caches.push(json!({"level": 3, "type": "Unified", "size": s}));
    }

    let simd = macos_simd_flags();

    json!({
        "model":          model,
        "sockets":        1,
        "physical_cores": physical,
        "logical_cores":  logical,
        "freq_mhz":       null,
        "simd":           simd,
        "caches":         caches,
    })
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn macos_simd_flags() -> Value {
    json!({
        "sse4_2": false,
        "avx": false,
        "avx2": false,
        "avx512f": false,
        "avx512bw": false,
        "avx512cd": false,
        "avx512vl": false,
        "fma": false,
        "neon": true,
        "sve": false,
        "sve2": false,
    })
}

#[cfg(all(target_os = "macos", any(target_arch = "x86", target_arch = "x86_64")))]
fn macos_simd_flags() -> Value {
    let features = sysctl_str("machdep.cpu.features").unwrap_or_default();
    let leaf7 = sysctl_str("machdep.cpu.leaf7_features").unwrap_or_default();
    let flags = format!("{} {}", features.to_lowercase(), leaf7.to_lowercase());
    json!({
        "sse4_2": flags.contains("sse4.2"),
        "avx": flags.split_whitespace().any(|f| f == "avx1.0" || f == "avx"),
        "avx2": flags.split_whitespace().any(|f| f == "avx2"),
        "avx512f": flags.split_whitespace().any(|f| f == "avx512f"),
        "avx512bw": flags.split_whitespace().any(|f| f == "avx512bw"),
        "avx512cd": flags.split_whitespace().any(|f| f == "avx512cd"),
        "avx512vl": flags.split_whitespace().any(|f| f == "avx512vl"),
        "fma": flags.split_whitespace().any(|f| f == "fma"),
        "neon": false,
        "sve": false,
        "sve2": false,
    })
}

#[cfg(all(
    target_os = "macos",
    not(any(target_arch = "aarch64", target_arch = "x86", target_arch = "x86_64"))
))]
fn macos_simd_flags() -> Value {
    json!({
        "sse4_2": false,
        "avx": false,
        "avx2": false,
        "avx512f": false,
        "avx512bw": false,
        "avx512cd": false,
        "avx512vl": false,
        "fma": false,
        "neon": false,
        "sve": false,
        "sve2": false,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn collect_cpu_info() -> Value {
    json!({
        "model": "unknown",
        "sockets": null,
        "physical_cores": std::thread::available_parallelism().map(|n| n.get()).ok(),
        "logical_cores": std::thread::available_parallelism().map(|n| n.get()).ok(),
        "freq_mhz": null,
        "simd": {},
        "caches": [],
        "note": "Detailed cpu_info currently supports Linux and macOS",
    })
}

#[cfg(target_os = "macos")]
fn sysctl_str(name: &str) -> Option<String> {
    std::process::Command::new("sysctl")
        .args(["-n", name])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &str) -> Option<u64> {
    sysctl_str(name)?.parse().ok()
}

// ── occupancy_calc ────────────────────────────────────────────────────────────

/// Architecture limits for theoretical occupancy calculation.
/// Source: CUDA Occupancy Calculator + CUDA C Programming Guide.
struct ArchLimits {
    max_warps_per_sm: u32,
    max_blocks_per_sm: u32,
    max_regs_per_sm: u32,
    /// Default shared memory per SM in bytes (without dynamic config)
    max_smem_per_sm: u32,
    /// Register allocation granularity (warps) — reserved for future fine-grained calc
    #[allow(dead_code)]
    reg_alloc_unit: u32,
}

fn arch_limits(major: u32, minor: u32) -> ArchLimits {
    match (major, minor) {
        // Maxwell
        (5, _) => ArchLimits {
            max_warps_per_sm: 64,
            max_blocks_per_sm: 32,
            max_regs_per_sm: 65536,
            max_smem_per_sm: 96 * 1024,
            reg_alloc_unit: 4,
        },
        // Pascal
        (6, 0) => ArchLimits {
            max_warps_per_sm: 64,
            max_blocks_per_sm: 32,
            max_regs_per_sm: 65536,
            max_smem_per_sm: 64 * 1024,
            reg_alloc_unit: 4,
        },
        (6, _) => ArchLimits {
            max_warps_per_sm: 32,
            max_blocks_per_sm: 32,
            max_regs_per_sm: 65536,
            max_smem_per_sm: 48 * 1024,
            reg_alloc_unit: 4,
        },
        // Volta
        (7, 0) => ArchLimits {
            max_warps_per_sm: 64,
            max_blocks_per_sm: 32,
            max_regs_per_sm: 65536,
            max_smem_per_sm: 96 * 1024,
            reg_alloc_unit: 4,
        },
        // Turing
        (7, 5) => ArchLimits {
            max_warps_per_sm: 32,
            max_blocks_per_sm: 16,
            max_regs_per_sm: 65536,
            max_smem_per_sm: 64 * 1024,
            reg_alloc_unit: 4,
        },
        // Ampere A100
        (8, 0) => ArchLimits {
            max_warps_per_sm: 64,
            max_blocks_per_sm: 32,
            max_regs_per_sm: 65536,
            max_smem_per_sm: 164 * 1024,
            reg_alloc_unit: 4,
        },
        // Ada Lovelace RTX 40xx (sm_89) — more blocks per SM than sm_86
        (8, 9) => ArchLimits {
            max_warps_per_sm: 48,
            max_blocks_per_sm: 24,
            max_regs_per_sm: 65536,
            max_smem_per_sm: 100 * 1024,
            reg_alloc_unit: 4,
        },
        // Ampere RTX 30xx (sm_86, sm_87)
        (8, _) => ArchLimits {
            max_warps_per_sm: 48,
            max_blocks_per_sm: 16,
            max_regs_per_sm: 65536,
            max_smem_per_sm: 100 * 1024,
            reg_alloc_unit: 4,
        },
        // Hopper
        (9, _) => ArchLimits {
            max_warps_per_sm: 64,
            max_blocks_per_sm: 32,
            max_regs_per_sm: 65536,
            max_smem_per_sm: 228 * 1024,
            reg_alloc_unit: 4,
        },
        // Safe default for unknown future arches
        _ => ArchLimits {
            max_warps_per_sm: 48,
            max_blocks_per_sm: 16,
            max_regs_per_sm: 65536,
            max_smem_per_sm: 48 * 1024,
            reg_alloc_unit: 4,
        },
    }
}

pub fn occupancy_calc(args: &Value) -> Result<ToolResult, String> {
    let threads_per_block = args["threads_per_block"]
        .as_u64()
        .ok_or("occupancy_calc: 'threads_per_block' is required")?
        as u32;
    let shared_mem_bytes = args["shared_mem_bytes"].as_u64().unwrap_or(0) as u32;
    let regs_per_thread = args["registers_per_thread"].as_u64().unwrap_or(0) as u32;

    if threads_per_block == 0 || threads_per_block > 1024 {
        return Ok(ToolResult::error("threads_per_block must be 1–1024"));
    }

    // Get compute capability from nvidia-smi (or use provided override)
    let (major, minor) = if let (Some(maj), Some(min)) = (
        args["compute_major"].as_u64(),
        args["compute_minor"].as_u64(),
    ) {
        (maj as u32, min as u32)
    } else {
        live_compute_cap().unwrap_or((8, 6)) // default to sm_86 (this machine)
    };

    let arch = arch_limits(major, minor);
    let warp_size: u32 = 32;

    // Warps per block (always round up to warp boundary)
    let warps_per_block = threads_per_block.div_ceil(warp_size);

    // ── Limiter 1: thread count ───────────────────────────────────────────────
    let blocks_by_threads = (arch.max_warps_per_sm / warps_per_block).min(arch.max_blocks_per_sm);
    let active_warps_by_threads = blocks_by_threads * warps_per_block;

    // ── Limiter 2: shared memory ──────────────────────────────────────────────
    let (blocks_by_smem, active_warps_by_smem) = if shared_mem_bytes == 0 {
        (arch.max_blocks_per_sm, arch.max_warps_per_sm)
    } else {
        let b = (arch.max_smem_per_sm / shared_mem_bytes).min(arch.max_blocks_per_sm);
        (b, b * warps_per_block)
    };

    // ── Limiter 3: registers ──────────────────────────────────────────────────
    let (blocks_by_regs, active_warps_by_regs) = if regs_per_thread == 0 {
        (arch.max_blocks_per_sm, arch.max_warps_per_sm)
    } else {
        // Registers are allocated per warp, rounded up to reg_alloc_unit warps
        let regs_per_warp_raw = regs_per_thread * warp_size;
        // Round to nearest 256-register boundary (common granularity)
        let regs_per_warp = regs_per_warp_raw.div_ceil(256) * 256;
        let b =
            (arch.max_regs_per_sm / (regs_per_warp * warps_per_block)).min(arch.max_blocks_per_sm);
        (b, b * warps_per_block)
    };

    // ── Determine bottleneck ──────────────────────────────────────────────────
    let active_warps = active_warps_by_threads
        .min(active_warps_by_smem)
        .min(active_warps_by_regs);

    let active_blocks = blocks_by_threads.min(blocks_by_smem).min(blocks_by_regs);

    let occupancy_pct = (active_warps as f64 / arch.max_warps_per_sm as f64 * 100.0) as u32;

    // Only report a resource as a limiter if it is actually constrained
    // (i.e. the user specified it AND it ties for the minimum active warps).
    let mut limiters = Vec::new();
    if active_warps_by_threads == active_warps {
        limiters.push("thread_count");
    }
    if shared_mem_bytes > 0 && active_warps_by_smem == active_warps {
        limiters.push("shared_memory");
    }
    if regs_per_thread > 0 && active_warps_by_regs == active_warps {
        limiters.push("registers");
    }
    if limiters.is_empty() {
        limiters.push("thread_count");
    } // always at least one

    // ── Recommendations ───────────────────────────────────────────────────────
    let mut tips = Vec::new();
    if !threads_per_block.is_multiple_of(warp_size) {
        tips.push(format!(
            "threads_per_block ({threads_per_block}) is not a multiple of warp size ({warp_size}) — \
             {} threads wasted per block",
            warp_size - (threads_per_block % warp_size)
        ));
    }
    if occupancy_pct < 50 {
        tips.push(
            "Occupancy below 50% — consider reducing shared memory or register usage, \
                   or increasing threads_per_block"
                .to_owned(),
        );
    }
    if shared_mem_bytes > 0 && blocks_by_smem < blocks_by_threads {
        let max_smem_for_full = arch.max_smem_per_sm / blocks_by_threads;
        tips.push(format!(
            "Shared memory is the bottleneck. Reduce to ≤{} bytes/block for full occupancy",
            max_smem_for_full
        ));
    }

    Ok(ToolResult::json(&json!({
        "compute_capability": format!("sm_{major}{minor}"),
        "threads_per_block":  threads_per_block,
        "warps_per_block":    warps_per_block,
        "shared_mem_bytes":   shared_mem_bytes,
        "registers_per_thread": regs_per_thread,

        "theoretical_occupancy_pct": occupancy_pct,
        "active_warps_per_sm":       active_warps,
        "active_blocks_per_sm":      active_blocks,
        "max_warps_per_sm":          arch.max_warps_per_sm,
        "max_blocks_per_sm":         arch.max_blocks_per_sm,

        "limiters": limiters,
        "by_threads":       { "blocks": blocks_by_threads, "active_warps": active_warps_by_threads },
        "by_shared_memory": { "blocks": blocks_by_smem,    "active_warps": active_warps_by_smem },
        "by_registers":     { "blocks": blocks_by_regs,    "active_warps": active_warps_by_regs },

        "tips": tips,
    })))
}

// ── gpu_live — Linux ──────────────────────────────────────────────────────────

/// Live GPU state — utilization, temperature, clocks, power, VRAM.
/// Not cached: run before benchmarks to verify the GPU is idle and at boost clocks.
#[cfg(target_os = "linux")]
pub fn gpu_live(_args: &Value) -> Result<ToolResult, String> {
    let fields = "index,name,\
                  utilization.gpu,utilization.memory,\
                  temperature.gpu,\
                  power.draw,power.limit,\
                  clocks.current.sm,clocks.current.memory,\
                  memory.used,memory.total";

    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu", fields, "--format=csv,noheader,nounits"])
        .output()
        .map_err(|e| format!("gpu_live: nvidia-smi: {e}"))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Ok(ToolResult::error(format!("nvidia-smi failed: {err}")));
    }

    let mut gpus: Vec<Value> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let p: Vec<&str> = line.split(", ").collect();
        if p.len() < 11 {
            continue;
        }

        let power_draw = p[5].trim().parse::<f32>().ok();
        let power_limit = p[6].trim().parse::<f32>().ok();
        let power_pct = match (power_draw, power_limit) {
            (Some(d), Some(l)) if l > 0.0 => Some((d / l * 100.0) as u32),
            _ => None,
        };

        gpus.push(json!({
            "index":          p[0].trim().parse::<u32>().unwrap_or(0),
            "name":           p[1].trim(),
            "gpu_util_pct":   p[2].trim().parse::<u32>().ok(),
            "mem_util_pct":   p[3].trim().parse::<u32>().ok(),
            "temp_c":         p[4].trim().parse::<u32>().ok(),
            "power_draw_w":   power_draw,
            "power_limit_w":  power_limit,
            "power_pct":      power_pct,
            "sm_clock_mhz":   p[7].trim().parse::<u32>().ok(),
            "mem_clock_mhz":  p[8].trim().parse::<u32>().ok(),
            "vram_used_mb":   p[9].trim().parse::<u32>().ok(),
            "vram_total_mb":  p[10].trim().parse::<u32>().ok(),
        }));
    }

    let n = gpus.len();
    let ready = gpus
        .iter()
        .all(|g| g["gpu_util_pct"].as_u64().unwrap_or(100) < 5);

    Ok(ToolResult::json(&json!({
        "gpus":           gpus,
        "count":          n,
        "ready_to_bench": ready,
        "note": if ready {
            "GPU idle — benchmark results will be reliable"
        } else {
            "GPU busy — benchmark results may be skewed"
        },
    })))
}

#[cfg(target_os = "linux")]
fn live_compute_cap() -> Option<(u32, u32)> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=compute_cap", "--format=csv,noheader"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let cap = s.lines().next()?.trim();
    let (maj, min) = cap.split_once('.')?;
    Some((maj.parse().ok()?, min.parse().ok()?))
}

// ── gpu_live — macOS ──────────────────────────────────────────────────────────

/// Apple Silicon GPU stub — utilization data requires sudo/Instruments.
#[cfg(not(target_os = "linux"))]
pub fn gpu_live(_args: &Value) -> Result<ToolResult, String> {
    let name = apple_gpu_name().unwrap_or_else(|| "Apple GPU".to_owned());
    Ok(ToolResult::json(&json!({
        "gpus":   [{ "index": 0, "name": name }],
        "count":  1,
        "ready_to_bench": true,
        "note": "Apple Silicon GPU — utilization data unavailable without sudo. \
                 Use Instruments or Activity Monitor for GPU metrics.",
    })))
}

/// Fallback live_compute_cap on macOS — always returns None (no CUDA).
#[cfg(not(target_os = "linux"))]
fn live_compute_cap() -> Option<(u32, u32)> {
    None
}
