//! Minimal `fercuda-ffi` surface required by ferrite-mcp's `fercuda_runtime` tool.
//!
//! Default `cargo install` / `install.sh` use this crate so the workspace builds without a
//! sibling feRcuda tree. Session APIs return [`Error`] explaining that the native library
//! is not linked. To use the real runtime, change `crates/shell-mcp/Cargo.toml` so
//! `fercuda-ffi` points at `…/feRcuda/rust/fercuda-ffi`, then rebuild.

use std::fmt;

pub type BufferId = u64;
pub type JobId = u64;

pub const JIT_WILDCARD_U32: u32 = 0xFFFF_FFFF;
pub const JIT_WILDCARD_U64: u64 = 0xFFFF_FFFF_FFFF_FFFF;

#[repr(C)]
pub struct FerSessionOpaque {
    _private: [u8; 0],
}

#[repr(C)]
pub struct FerJitProgramOpaque {
    _private: [u8; 0],
}

#[repr(C)]
pub struct FerJitKernelOpaque {
    _private: [u8; 0],
}

pub type JitProgram = *mut FerJitProgramOpaque;
pub type JitKernel = *mut FerJitKernelOpaque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum StatusCode {
    Ok = 0,
    InvalidArgument = 1,
    NotFound = 2,
    InternalError = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum MemoryRegime {
    CustomPool = 0,
    CudaMalloc = 1,
    CudaManaged = 2,
    Auto = 0xFFFF_FFFF,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum JitBackend {
    Nvrtc = 0,
    Auto = 0xFFFF_FFFF,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum JitMode {
    Permissive = 0,
    Strict = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum JitSourceKind {
    Cuda = 0,
    Ptx = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum JitArgKind {
    Buffer = 0,
    ScalarI32 = 1,
    ScalarU32 = 2,
    ScalarI64 = 3,
    ScalarU64 = 4,
    ScalarF32 = 5,
    ScalarF64 = 6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum JitAccess {
    Read = 0,
    Write = 1,
    ReadWrite = 2,
}

#[derive(Debug, Clone, Copy)]
pub struct JitSource<'a> {
    pub kind: JitSourceKind,
    pub code: &'a str,
}

#[derive(Debug, Clone, Default)]
pub struct JitOptions {
    pub backend: Option<JitBackend>,
    pub mode: Option<JitMode>,
    pub arch: Option<String>,
    pub extra_nvrtc_opts: Option<String>,
    pub cache_dir: Option<String>,
    pub enable_disk_cache: bool,
}

#[derive(Debug, Clone)]
pub struct JitCompileResult {
    pub cache_hit: bool,
    pub backend_name: String,
    pub log: String,
}

#[derive(Debug, Clone)]
pub struct JitArgDesc {
    pub kind: JitArgKind,
    pub access: JitAccess,
    pub name: Option<String>,
    pub expected_dtype: u32,
    pub expected_rank: u32,
    pub expected_bytes: u64,
    pub expected_dims: [u32; 4],
}

#[derive(Debug, Clone, Copy)]
pub struct JitLaunchCfg {
    pub grid_x: u32,
    pub grid_y: u32,
    pub grid_z: u32,
    pub block_x: u32,
    pub block_y: u32,
    pub block_z: u32,
    pub shared_mem_bytes: u32,
    pub memory_regime: u32,
}

#[derive(Debug, Clone, Copy)]
pub enum JitArgValue {
    Buffer(BufferId),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F32(f32),
    F64(f64),
}

#[derive(Debug, Clone, Copy)]
pub struct JitStats {
    pub compile_count: u64,
    pub cache_hit_count: u64,
    pub launch_count: u64,
    pub compile_time_us: u64,
    pub launch_time_us: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct PoolConfig {
    pub mutable_bytes: u64,
    pub immutable_bytes: u64,
    pub cuda_reserve: u64,
    pub verbose: u8,
    pub memory_regime: u32,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            mutable_bytes: 512u64 << 20,
            immutable_bytes: 2u64 << 30,
            cuda_reserve: 256u64 << 20,
            verbose: 0,
            memory_regime: MemoryRegime::CustomPool as u32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum BufferDType {
    F32 = 0,
    F16 = 1,
    BF16 = 2,
    I8 = 3,
    U8 = 4,
    I16 = 5,
    U16 = 6,
    I32 = 7,
    U32 = 8,
    I64 = 9,
    U64 = 10,
    F64 = 11,
}

#[derive(Debug, Clone, Copy)]
pub struct BufferDesc {
    pub dtype: BufferDType,
    pub rank: u32,
    pub dims: [u32; 4],
    pub immutable: bool,
    pub tag: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct MatmulRequest {
    pub a: BufferId,
    pub b: BufferId,
    pub out: BufferId,
    pub memory_regime: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct LayerNormRequest {
    pub x: BufferId,
    pub out: BufferId,
    pub eps: f32,
    pub memory_regime: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct AffineF32Request {
    pub input: BufferId,
    pub output: BufferId,
    pub n: u32,
    pub alpha: f32,
    pub beta: f32,
    pub fusion_mask: u32,
    pub caps_mask: u32,
    pub memory_regime: u32,
}

#[derive(Debug, Clone)]
pub struct Error {
    pub code: StatusCode,
    pub message: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

fn stub_err() -> Error {
    Error {
        code: StatusCode::InternalError,
        message: "feRcuda native API is not linked in this ferrite-mcp build (stub fercuda-ffi). \
To enable fercuda_runtime against your GPU stack, point crates/shell-mcp/Cargo.toml \
dependency fercuda-ffi at …/feRcuda/rust/fercuda-ffi and rebuild."
            .to_owned(),
    }
}

pub struct Session {
    raw: *mut FerSessionOpaque,
}

// Matches the real feRcuda `Session`: owned C handle, safe to move across threads when
// feRcuda serializes access internally (MCP uses a global mutex around the store).
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

impl Session {
    pub fn new(_device: i32, _cfg: Option<PoolConfig>) -> Result<Self, Error> {
        Err(stub_err())
    }

    pub fn alloc_buffer(&self, _desc: BufferDesc) -> Result<BufferId, Error> {
        Err(stub_err())
    }

    pub fn free_buffer(&self, _id: BufferId) -> Result<(), Error> {
        Err(stub_err())
    }

    pub fn upload_f32(&self, _id: BufferId, _host: &[f32]) -> Result<(), Error> {
        Err(stub_err())
    }

    pub fn download_f32(&self, _id: BufferId, _host: &mut [f32]) -> Result<(), Error> {
        Err(stub_err())
    }

    pub fn upload_bytes(&self, _id: BufferId, _host: &[u8]) -> Result<(), Error> {
        Err(stub_err())
    }

    pub fn download_bytes(&self, _id: BufferId, _host: &mut [u8]) -> Result<(), Error> {
        Err(stub_err())
    }

    pub fn jit_compile(
        &self,
        _source: JitSource<'_>,
        _options: &JitOptions,
    ) -> Result<(JitProgram, JitCompileResult), Error> {
        Err(stub_err())
    }

    pub fn jit_release_program(&self, _program: JitProgram) -> Result<(), Error> {
        Err(stub_err())
    }

    pub fn jit_get_kernel(
        &self,
        _program: JitProgram,
        _kernel_name: &str,
        _descs: &[JitArgDesc],
    ) -> Result<JitKernel, Error> {
        Err(stub_err())
    }

    pub fn jit_release_kernel(&self, _kernel: JitKernel) -> Result<(), Error> {
        Err(stub_err())
    }

    pub fn jit_launch(
        &self,
        _kernel: JitKernel,
        _cfg: JitLaunchCfg,
        _args: &[JitArgValue],
    ) -> Result<JobId, Error> {
        Err(stub_err())
    }

    pub fn jit_get_stats(&self) -> Result<JitStats, Error> {
        Err(stub_err())
    }

    pub fn run_affine_f32(&self, _req: AffineF32Request) -> Result<JobId, Error> {
        Err(stub_err())
    }

    pub fn submit_matmul(&self, _req: MatmulRequest) -> Result<JobId, Error> {
        Err(stub_err())
    }

    pub fn submit_layer_norm(&self, _req: LayerNormRequest) -> Result<JobId, Error> {
        Err(stub_err())
    }

    pub fn job_status(&self, _job_id: JobId) -> Result<bool, Error> {
        Err(stub_err())
    }

    pub fn job_wait(&self, _job_id: JobId) -> Result<(), Error> {
        Err(stub_err())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.raw;
    }
}
