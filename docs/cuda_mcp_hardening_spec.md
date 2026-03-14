# CUDA MCP Hardening Spec

## Goal

Extend `ferrite` with CUDA workflow tools that are:

- agent-agnostic
- compact by default
- useful on real CUDA projects like `/home/daron/feRcuda`
- parallel in shape to the hardened FPGA toolset

Target outcome:

- a generic MCP client can verify CUDA environment readiness
- inventory CUDA build/profile artifacts
- classify common CUDA failures
- choose the right next tool without scraping long logs

## Existing CUDA Baseline

Current MCP primitives already cover a lot:

- `gpu_info`
- `build_check`
- `ncu_profile`
- `compute_sanitizer`
- `ptx_inspect`
- `occupancy_calc`
- `fercuda_runtime`

These are strong low-level tools, but they do not yet provide a hardened workflow surface.

## New Hardened CUDA Tools

### 1. `cuda_env_doctor`

Purpose:

- verify that the machine is CUDA-ready
- report the exact toolchain state

Checks:

- `nvcc`
- `nvidia-smi`
- `ncu`
- `compute-sanitizer`
- `cuobjdump`
- visible GPU and driver

Output style:

- compact
- structured
- safe for any MCP client

### 2. `cuda_artifacts`

Purpose:

- provide a stable artifact inventory for CUDA projects

Targets:

- `.cu`, `.cuh`, `.ptx`
- profile outputs like `.ncu-rep`, `.nsys-rep`, `.qdrep`
- built libraries like `.so`, `.a`
- built executables under `build/`

Why:

- agents should not have to guess where outputs live
- inventory should be reusable by report/triage tools

### 3. `cuda_triage`

Purpose:

- classify CUDA failures into actionable buckets

Initial classes:

- `compile_error`
- `link_or_library_error`
- `illegal_memory_access`
- `sync_or_race_error`
- `profiling_permission_error`
- `arch_mismatch`
- `runtime_timeout`
- `unknown_cuda_failure`

Recommendations should point to existing tools like:

- `build_check`
- `find_lib`
- `compute_sanitizer`
- `ncu_profile`

## Real Validation Targets

Primary validation target:

- `/home/daron/feRcuda`

Observed useful assets there already:

- built libraries in `build/`
- built test binaries in `build/`
- CUDA source files in `src/`, `tests/`, and `examples/`
- a real NVIDIA GPU on the machine

## Phase 1 Definition Of Done

- `cuda_env_doctor` works on this machine
- `cuda_artifacts` inventories `/home/daron/feRcuda`
- `cuda_triage` classifies at least:
  - compile failure
  - illegal memory access
  - library/link failure
- unit tests cover at least two classification paths
- fresh MCP calls verify all three tools from a new `ferrite --mcp` process

## Follow-On CUDA Tools

Phase 2:

- `cuda_regression_run`
- `cuda_regression_report`
- `cuda_bench_report`
- `cuda_triage` integration with actual benchmark/profile outputs

That would make the CUDA side mirror the FPGA side:

- execution
- report
- triage
- artifacts
