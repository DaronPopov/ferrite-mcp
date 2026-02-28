//! project_new — create a new local git project with optional GitHub remote.
//!
//! Steps:
//!   1. Create the project directory
//!   2. `git init --initial-branch=main`
//!   3. Write scaffold files based on `project_type`
//!   4. `git add -A && git commit -m "Initial commit"`
//!   5. (optional) Create GitHub repo via REST API and push
//!
//! GitHub creation requires GITHUB_TOKEN env var. If absent, the remote is
//! configured but the user must create the repo on GitHub manually first.

use std::fs;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use crate::protocol::ToolResult;
use crate::tools::execution::run;
use crate::tools::project::expand_tilde;

const GITHUB_USER: &str = "DaronPopov";

// ── entry point ───────────────────────────────────────────────────────────────

pub fn project_new(args: &Value) -> Result<ToolResult, String> {
    let name = args["name"].as_str().ok_or("project_new: 'name' is required")?;
    let parent = args["path"].as_str()
        .map(expand_tilde)
        .unwrap_or_else(|| expand_tilde("~"));
    let project_type = args["project_type"].as_str().unwrap_or("bare");
    let description  = args["description"].as_str().unwrap_or("");
    let with_github  = args["github"].as_bool().unwrap_or(false);
    let private      = args["private"].as_bool().unwrap_or(false);

    let root = parent.join(name);

    // ── 1. Create directory ────────────────────────────────────────────────────
    fs::create_dir_all(&root)
        .map_err(|e| format!("project_new: mkdir {}: {e}", root.display()))?;

    // ── 2. git init ────────────────────────────────────────────────────────────
    let init = run(
        "git init --initial-branch=main",
        &root, &[], "", Duration::from_secs(10),
    );
    if !init["success"].as_bool().unwrap_or(false) {
        return Err(format!("git init failed: {}", init["stderr"].as_str().unwrap_or("")));
    }

    // ── 3. Scaffold ────────────────────────────────────────────────────────────
    let files_created = scaffold(&root, name, description, project_type)?;

    // ── 4. Initial commit ──────────────────────────────────────────────────────
    let add = run("git add -A", &root, &[], "", Duration::from_secs(10));
    if !add["success"].as_bool().unwrap_or(false) {
        return Err(format!("git add failed: {}", add["stderr"].as_str().unwrap_or("")));
    }

    let commit_cmd = r#"git -c user.email="ferrite@local" -c user.name="ferrite" commit -m "Initial commit""#;
    let commit = run(commit_cmd, &root, &[], "", Duration::from_secs(10));
    if !commit["success"].as_bool().unwrap_or(false) {
        return Err(format!("git commit failed: {}", commit["stderr"].as_str().unwrap_or("")));
    }

    // ── 5. GitHub remote ───────────────────────────────────────────────────────
    let mut github_result = json!(null);
    let ssh_url = format!("git@github.com:{GITHUB_USER}/{name}.git");

    if with_github {
        github_result = create_github_repo(name, description, private, &root, &ssh_url);
    } else {
        // Pre-configure remote so the user only needs `git push -u origin main`
        let remote_cmd = format!("git remote add origin {ssh_url}");
        let _ = run(&remote_cmd, &root, &[], "", Duration::from_secs(5));
    }

    Ok(ToolResult::json(&json!({
        "success":       true,
        "name":          name,
        "root":          root.display().to_string(),
        "project_type":  project_type,
        "files_created": files_created,
        "ssh_url":       ssh_url,
        "github":        github_result,
        "next_steps":    next_steps(with_github, &github_result, name),
    })))
}

// ── scaffold ──────────────────────────────────────────────────────────────────

fn scaffold(root: &Path, name: &str, description: &str, ptype: &str) -> Result<Vec<String>, String> {
    let mut created = Vec::new();

    // Every project gets a .gitignore and README.md
    write_file(root, ".gitignore", gitignore_for(ptype), &mut created)?;
    write_file(root, "README.md",  readme(name, description, ptype), &mut created)?;

    match ptype {
        "rust" => {
            write_file(root, "Cargo.toml", rust_bin_cargo(name), &mut created)?;
            fs::create_dir_all(root.join("src"))
                .map_err(|e| format!("mkdir src: {e}"))?;
            write_file(root, "src/main.rs", "fn main() {\n    println!(\"Hello, world!\");\n}\n", &mut created)?;
        }

        "rust-lib" => {
            write_file(root, "Cargo.toml", rust_lib_cargo(name), &mut created)?;
            fs::create_dir_all(root.join("src"))
                .map_err(|e| format!("mkdir src: {e}"))?;
            write_file(root, "src/lib.rs", "pub fn hello() -> &'static str {\n    \"hello\"\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn it_works() {\n        assert_eq!(hello(), \"hello\");\n    }\n}\n", &mut created)?;
        }

        "rust-workspace" => {
            write_file(root, "Cargo.toml", rust_workspace_cargo(name), &mut created)?;
            fs::create_dir_all(root.join("crates"))
                .map_err(|e| format!("mkdir crates: {e}"))?;
        }

        "python" => {
            write_file(root, "requirements.txt", "", &mut created)?;
            write_file(root, "pyproject.toml", python_pyproject(name, description), &mut created)?;
            fs::create_dir_all(root.join("src"))
                .map_err(|e| format!("mkdir src: {e}"))?;
            write_file(root, &format!("src/{}.py", name.replace('-', "_")),
                "def main():\n    print(\"Hello, world!\")\n\nif __name__ == \"__main__\":\n    main()\n",
                &mut created)?;
        }

        "rtl" => {
            // processor_lab-style layout: ip/core/rtl/, ip/core/sim/, top/<board>/
            for dir in ["ip/core/rtl", "ip/core/sim", "top/basys3/rtl", "top/basys3/tcl", "top/basys3/xdc"] {
                fs::create_dir_all(root.join(dir))
                    .map_err(|e| format!("mkdir {dir}: {e}"))?;
            }
            let mod_name = name.replace('-', "_");
            write_file(root, &format!("ip/core/rtl/{mod_name}_core.sv"),
                &rtl_sv_stub(&mod_name), &mut created)?;
            write_file(root, &format!("top/basys3/rtl/{mod_name}_top.sv"),
                &rtl_top_stub(name, &mod_name), &mut created)?;
            write_file(root, &format!("top/basys3/tcl/build_{mod_name}.tcl"),
                &rtl_tcl_stub(name, &mod_name), &mut created)?;
            write_file(root, "ip/core/sim/test_core.py",
                &cocotb_test_stub(&mod_name), &mut created)?;
            write_file(root, "ip/core/sim/Makefile",
                &cocotb_makefile(&mod_name), &mut created)?;
        }

        _ => {} // "bare" — just .gitignore + README
    }

    Ok(created)
}

fn write_file(root: &Path, rel: &str, content: impl AsRef<[u8]>, created: &mut Vec<String>) -> Result<(), String> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(&path, content).map_err(|e| format!("write {rel}: {e}"))?;
    created.push(rel.to_owned());
    Ok(())
}

// ── GitHub API ────────────────────────────────────────────────────────────────

fn create_github_repo(name: &str, description: &str, private: bool, root: &Path, ssh_url: &str) -> Value {
    let token = match std::env::var("GITHUB_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => return json!({
            "created": false,
            "reason": "GITHUB_TOKEN not set — remote configured, create repo on GitHub then: git push -u origin main",
            "ssh_url": ssh_url,
        }),
    };

    let body = json!({
        "name": name,
        "description": description,
        "private": private,
        "auto_init": false,
    });

    let curl_cmd = format!(
        r#"curl -sf -X POST \
            -H "Authorization: Bearer {token}" \
            -H "Accept: application/vnd.github+json" \
            -H "X-GitHub-Api-Version: 2022-11-28" \
            https://api.github.com/user/repos \
            -d '{}'"#,
        body.to_string()
    );

    let cwd = std::env::temp_dir();
    let raw = run(&curl_cmd, &cwd, &[], "", Duration::from_secs(30));
    let api_success = raw["success"].as_bool().unwrap_or(false);

    if !api_success {
        return json!({
            "created": false,
            "reason": format!("GitHub API error: {}", raw["stderr"].as_str().unwrap_or("")),
        });
    }

    // Parse response to get html_url
    let api_resp: Value = serde_json::from_str(raw["stdout"].as_str().unwrap_or("{}"))
        .unwrap_or(json!({}));

    if api_resp["errors"].is_array() || api_resp["message"].is_string() {
        return json!({
            "created": false,
            "reason": api_resp["message"],
        });
    }

    // Configure remote and push
    let remote_cmd = format!("git remote add origin {ssh_url}");
    let _ = run(&remote_cmd, root, &[], "", Duration::from_secs(5));

    let push = run("git push -u origin main", root, &[], "", Duration::from_secs(60));
    let pushed = push["success"].as_bool().unwrap_or(false);

    json!({
        "created":   true,
        "pushed":    pushed,
        "html_url":  api_resp["html_url"],
        "ssh_url":   ssh_url,
        "private":   api_resp["private"],
        "push_stderr": push["stderr"].as_str().unwrap_or(""),
    })
}

// ── next steps helper ─────────────────────────────────────────────────────────

fn next_steps(with_github: bool, github_result: &Value, name: &str) -> Vec<String> {
    let mut steps = Vec::new();
    if !with_github || !github_result["created"].as_bool().unwrap_or(false) {
        steps.push(format!("Create repo on GitHub: https://github.com/new (name: {name})"));
        steps.push("git push -u origin main".to_owned());
    } else if !github_result["pushed"].as_bool().unwrap_or(false) {
        steps.push("git push -u origin main".to_owned());
    } else {
        steps.push(format!("Repo live: {}", github_result["html_url"].as_str().unwrap_or("")));
    }
    steps
}

// ── scaffold content ──────────────────────────────────────────────────────────

fn gitignore_for(ptype: &str) -> &'static str {
    match ptype {
        "rust" | "rust-lib" | "rust-workspace" => "\
/target/\n\
Cargo.lock\n\
**/*.rs.bk\n\
.env\n",
        "python" => "\
__pycache__/\n\
*.pyc\n\
*.pyo\n\
.venv/\n\
dist/\n\
*.egg-info/\n\
.env\n",
        "rtl" => "\
*.bit\n\
*.ltx\n\
*.log\n\
*.jou\n\
*.str\n\
*.runs/\n\
*.cache/\n\
*.hw/\n\
*.ip_user_files/\n\
*.sim/\n\
*.xpr\n\
__pycache__/\n\
*.pyc\n\
.Xil/\n",
        _ => "\
.env\n\
*.log\n\
.DS_Store\n",
    }
}

fn readme(name: &str, description: &str, ptype: &str) -> String {
    let desc = if description.is_empty() { name } else { description };
    let badge = match ptype {
        "rust" | "rust-lib" | "rust-workspace" => "![Rust](https://img.shields.io/badge/lang-Rust-orange)\n",
        "python" => "![Python](https://img.shields.io/badge/lang-Python-blue)\n",
        "rtl" => "![RTL](https://img.shields.io/badge/lang-SystemVerilog-purple)\n",
        _ => "",
    };
    format!("# {name}\n\n{badge}\n{desc}\n")
}

fn rust_bin_cargo(name: &str) -> String {
    format!("\
[package]\n\
name = \"{name}\"\n\
version = \"0.1.0\"\n\
edition = \"2021\"\n\
\n\
[[bin]]\n\
name = \"{name}\"\n\
path = \"src/main.rs\"\n\
\n\
[dependencies]\n")
}

fn rust_lib_cargo(name: &str) -> String {
    format!("\
[package]\n\
name = \"{name}\"\n\
version = \"0.1.0\"\n\
edition = \"2021\"\n\
\n\
[dependencies]\n")
}

fn rust_workspace_cargo(name: &str) -> String {
    format!("\
[workspace]\n\
name = \"{name}\"\n\
resolver = \"2\"\n\
members = [\n\
    # \"crates/my-crate\",\n\
]\n\
\n\
[workspace.dependencies]\n")
}

fn python_pyproject(name: &str, description: &str) -> String {
    format!("\
[project]\n\
name = \"{name}\"\n\
version = \"0.1.0\"\n\
description = \"{description}\"\n\
requires-python = \">=3.10\"\n\
dependencies = []\n")
}

fn rtl_sv_stub(mod_name: &str) -> String {
    format!("\
module {mod_name}_core (\n\
    input  logic clk,\n\
    input  logic rst_n\n\
);\n\
\n\
endmodule\n")
}

fn rtl_top_stub(name: &str, mod_name: &str) -> String {
    format!("\
// {name} top-level wrapper for Basys3\n\
module {mod_name}_top (\n\
    input  logic CLK100MHZ,\n\
    input  logic btnC\n\
);\n\
\n\
    {mod_name}_core u_core (\n\
        .clk   (CLK100MHZ),\n\
        .rst_n (~btnC)\n\
    );\n\
\n\
endmodule\n")
}

fn rtl_tcl_stub(name: &str, mod_name: &str) -> String {
    format!("\
# Build script for {name} on Basys3\n\
set project_name {mod_name}\n\
set board_part digilentinc.com:basys3:part0:1.1\n\
\n\
create_project -force $project_name ./build -part xc7a35tcpg236-1\n\
set_property board_part $board_part [current_project]\n\
\n\
add_files -scan_for_includes [glob ../rtl/*.sv]\n\
set_property top {{{mod_name}_top}} [current_fileset]\n\
\n\
add_files -fileset constrs_1 [glob ../xdc/*.xdc]\n\
\n\
launch_runs impl_1 -to_step write_bitstream -jobs 8\n\
wait_on_run impl_1\n\
puts \"Done: [get_property STATUS [get_runs impl_1]]\"\n")
}

fn cocotb_test_stub(mod_name: &str) -> String {
    format!("\
import cocotb\n\
from cocotb.clock import Clock\n\
from cocotb.triggers import RisingEdge\n\
\n\
@cocotb.test()\n\
async def test_basic(dut):\n\
    \"\"\"Basic smoke test for {mod_name}_core.\"\"\"\n\
    cocotb.start_soon(Clock(dut.clk, 10, units='ns').start())\n\
    dut.rst_n.value = 0\n\
    await RisingEdge(dut.clk)\n\
    await RisingEdge(dut.clk)\n\
    dut.rst_n.value = 1\n\
    await RisingEdge(dut.clk)\n\
    # TODO: add assertions\n")
}

fn cocotb_makefile(mod_name: &str) -> String {
    format!("\
SIM       ?= icarus\n\
TOPLEVEL_LANG ?= verilog\n\
VERILOG_SOURCES = $(shell find ../rtl -name '*.sv')\n\
TOPLEVEL  = {mod_name}_core\n\
MODULE    = test_core\n\
include $(shell cocotb-config --makefiles)/Makefile.sim\n")
}
