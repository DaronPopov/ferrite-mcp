# FPGA AI Automation Spec

## Goal

Turn `ferrite` from a strong collection of FPGA/EDA primitives into a project-aware, end-to-end automation layer for RTL development, formal verification, synthesis, board programming, and hardware validation.

Target outcome:

- an AI agent can inspect an RTL project
- discover how to lint, simulate, formally verify, synthesize, program, and validate it
- execute the flow with one high-level tool call
- classify failures and recommend or apply the next action

This spec is grounded in the existing `processor_lab` layout under `/home/daron/processor_lab`.

The MCP contract should remain agent-agnostic:

- plain stdio MCP transport
- tool discovery via `tools/list`
- explicit JSON arguments for workflow selection
- no dependence on Claude-only or Codex-only prompt conventions

## Current Baseline

The MCP already exposes strong primitives:

- RTL checks: `verilog_lint`, `verilog_sim`, `xsim_elab`, `cocotb_run`, `waveform_query`
- Vivado/FPGA: `vivado_tcl`, `synth_report`, `fpga_boards`, `fpga_program`, `board_status`, `fpga_serial`, `fpga_monitor`
- orchestration: `bg_spawn`, `bg_wait`, `pipeline_run`, `chip_status`, `chip_build_pipeline`
- remote/runtime: `tmux_ctl`, `session_status`, `remote_exec`, `remote_build`, `sync_project`
- AI/runtime: `fercuda_runtime`, `build_check`, `exec`, `task_run`

## Verified Findings From `processor_lab`

### Verified working

- `verilog_lint` succeeds on:
  - `chips/attn/ip/softmax/rtl/softmax_lut.sv`
  - `chips/tcfp/ip/tcfp_core/rtl/tcfp_state_core_matrix_net.sv`
- `cocotb_run` now handles Makefile-driven cocotb layouts and succeeds on:
  - `chips/attn/ip/softmax/sim`
    - 6 passing tests
  - `chips/tcfp/ip/tcfp_codebook/sim`
    - 4 passing tests
- `synth_report` parses existing reports for `chips/attn/build/basys3_n6`
  - WNS: `2.53 ns`
  - LUT: `3.36%`
  - DSP: `6.67%`
- `chip_status` detects 22 chips and existing sim artifacts across several designs
- `chip_build_pipeline` dry-run for `attn` now resolves the real project layout:
  - sim dir: `ip/softmax/sim`
  - Tcl: `top/basys3/tcl/build_n6_attn.tcl`
  - bitstream: `build/basys3_n6/attn_basys3_top_n6.bit`
- `rtl_regression_run` now works end-to-end on real chips:
  - `attn`: lint + sim pass
  - `tcfp`: lint surfaces a real syntax failure in `ip/tcfp_fabric/rtl/tcfp_fabric_multi_hetero.sv:137`

### Verified integration gaps

- there is still no first-class manifest file in the live chip directories, so pipeline resolution is partly heuristic today
- `rtl_regression_run` currently stops on the first failing stage and does not yet produce a richer aggregated report with artifact indexing
- `tcfp` currently fails regression due to a real RTL issue:
  - `chips/tcfp/ip/tcfp_fabric/rtl/tcfp_fabric_multi_hetero.sv:137`
  - `iverilog` reports `syntax error localparam list`
- formal tooling is only partially ready
  - `sby` exists: `/usr/local/bin/sby`
  - `yosys` is not currently detected in `PATH`
  - `chips/tcfp/formal` exists but is only a placeholder README today

## Main Missing Capabilities

### 1. Project manifest layer

There is no machine-readable per-project spec that tells the MCP:

- which RTL files belong to which unit
- which cocotb sim directories are valid
- which tops/boards/Tcl scripts should be used
- where bitstreams and reports land
- which hardware validations should run after programming
- which formal tasks exist

### 2. Formal verification integration

There is no MCP-native formal flow yet:

- no `formal_run`
- no `formal_status`
- no parsing of `sby` results
- no project-level registration of formal tasks

### 3. Regression orchestration

There is now an initial `rtl_regression_run`, but there is still no complete verification tool that can run:

- lint
- elaboration
- pure RTL sim
- cocotb suites
- formal tasks

and then summarize them as one regression result.

### 4. Artifact indexing

The MCP does not yet normalize artifacts across the flow:

- `results.xml`
- pytest/cocotb logs
- VCD waveforms
- formal logs/counterexamples
- timing and utilization reports
- bitstreams
- UART validation logs

### 5. Failure triage

There is no AI-oriented classification layer that says:

- this failed in cocotb discovery
- this failed because the manifest is wrong
- this failed in synthesis due to missing Tcl entrypoint
- this failed timing
- this failed hardware bring-up

### 6. Layout-aware pipeline execution

The current `chip_build_pipeline` is improved but still assumes too much when no manifest is present:

- Tcl script naming
- build output locations
- sim invocation style

That breaks on real chips even before deeper automation starts.

## Proposed Architecture

## A. Add `ferrite_fpga.toml`

Per project or per chip, add a manifest like:

```toml
[project]
name = "attn"
root = "/home/daron/processor_lab/chips/attn"
board = "basys3"

[[rtl_units]]
name = "softmax_lut"
files = ["ip/softmax/rtl/softmax_lut.sv"]
top = "softmax_lut"

[[cocotb]]
name = "softmax"
dir = "ip/softmax/sim"
mode = "makefile"
module = "test_softmax_lut"
sim = "icarus"

[[synth]]
name = "basys3_n6"
tcl = "top/basys3/tcl/build_n6_attn.tcl"
bitstream = "build/basys3_n6/attn_basys3_top_n6.bit"
timing_rpt = "build/basys3_n6/timing_summary.rpt"
util_rpt = "build/basys3_n6/utilization.rpt"

[[hardware_validation]]
name = "attn_host"
cmd = "python3 sw/host/attn_host.py --help"
```

This manifest becomes the source of truth for orchestration.

The workflow tools should also accept `manifest_path` as an override so non-`processor_lab` layouts can reuse the same contract.

## B. Add formal tools

Add MCP tools:

- `formal_run`
- `formal_list`
- `formal_status`

Requirements:

- support `.sby` tasks defined in the manifest
- parse `PASS`, `FAIL`, `UNKNOWN`, `TIMEOUT`
- return structured artifact paths
- detect missing `yosys`/solvers early

## C. Add regression tools

Add MCP tools:

- `rtl_regression_run`
- `rtl_regression_status`
- `rtl_regression_report`

Requirements:

- run selected suites or all suites from the manifest
- accept `manifest_path`, `sim_target`, and `synth_target` so generic MCP clients can select exact targets
- support modes:
  - `lint_only`
  - `sim_only`
  - `formal_only`
  - `full_verify`
- aggregate pass/fail by stage and target

## D. Fix cocotb execution model

Current gap: existing `processor_lab` sims are Makefile-based cocotb layouts.

Required behavior:

- if `sim/Makefile` exists and references `cocotb-config --makefiles`, run via `make`
- otherwise fall back to pytest-native cocotb invocation

Add or extend:

- `cocotb_run` should auto-detect `mode = makefile | pytest`
- preserve module filtering when possible
- parse `results.xml` and Makefile-driven simulator outputs

## E. Replace heuristic chip flow with manifest-backed flow

Either replace or wrap `chip_build_pipeline` with:

- `fpga_ai_flow`

Stages:

1. manifest load
2. lint
3. elaboration
4. cocotb / sim
5. formal
6. synth
7. timing/utilization parse
8. program
9. hardware validation
10. summarized next action

## F. Add artifact indexer

Add MCP tool:

- `fpga_artifacts`

Returns normalized records for:

- logs
- XML results
- VCDs
- formal outputs
- reports
- bitstreams
- UART traces

## G. Add failure triage

Add MCP tool:

- `fpga_triage`

It should map failures to categories:

- lint
- cocotb discovery
- cocotb runtime
- elaboration
- formal
- synthesis
- timing
- programming
- board I/O/runtime

And produce:

- likely root cause
- confidence
- recommended next tool/action

## Implementation Phases

### Phase 1: Make current verification tools work on `processor_lab`

Deliverables:

- manifest parser
- Makefile-aware `cocotb_run`
- manifest-backed synth/bitstream path resolution
- `chip_build_pipeline` updated or wrapped so `attn` runs correctly

Acceptance:

- `attn` cocotb suite is discoverable and runnable
- `chip_build_pipeline` resolves the actual `attn` Tcl and bitstream paths
- report parsing continues to work

### Phase 2: Formal + regression

Deliverables:

- `formal_run`
- `rtl_regression_run`
- structured regression report

Acceptance:

- can register at least one formal task under `chips/tcfp/formal`
- can run `lint + cocotb + formal` as one regression job

### Phase 3: End-to-end FPGA flow

Deliverables:

- `fpga_ai_flow`
- artifact indexer
- failure triage

Acceptance:

- one tool call can execute full verify -> synth -> program -> validate for a chip with a complete manifest

## Immediate Test Plan

### Test corpus

Primary real-world targets:

- `/home/daron/processor_lab/chips/attn`
- `/home/daron/processor_lab/chips/tcfp`

### Initial concrete tests

1. `attn` softmax cocotb
- target: `ip/softmax/sim`
- expected: Makefile-driven cocotb suite should run and report actual tests

2. `tcfp_codebook` cocotb
- target: `ip/tcfp_codebook/sim`
- expected: Makefile-driven suite should run instead of `0 tests collected`

3. `attn` synth flow resolution
- target: `top/basys3/tcl/build_n6_attn.tcl`
- expected: manifest-backed pipeline resolves correct Tcl and bitstream path

4. `attn` existing reports
- target: `build/basys3_n6`
- expected: timing/utilization parsing remains stable

## First Implementation Slice

The first slice should be:

1. add `ferrite_fpga.toml` support
2. update `cocotb_run` to support Makefile-based layouts
3. add a manifest-aware replacement path for `chip_build_pipeline`

This is the smallest slice that fixes real broken flows already observed in `processor_lab`.

## Progress

Implemented in this repo:

- Makefile-aware `cocotb_run`
- `results.xml` parsing for cocotb result enumeration
- manifest-capable path resolution for chip pipeline dry-runs
- initial `rtl_regression_run` wrapper tool
- example manifest template: `docs/ferrite_fpga.toml.example`
