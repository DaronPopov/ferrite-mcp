//! ferrite — entry point.
//!
//! Modes:
//!   ferrite              → interactive REPL
//!   ferrite --mcp        → MCP stdio server (for Claude Code integration)
//!   ferrite -c <cmd>     → execute a single command string and exit
//!   ferrite config set <key> <value>
//!   ferrite config get <key>
//!   ferrite config list
//!   ferrite install      → register MCP server in Claude Code and Codex configs
//!   ferrite uninstall    → remove MCP server from Claude Code and Codex configs
//!   ferrite status       → show config + MCP registration status for both clients
//!   ferrite up           → one-shot remote bootstrap for SSH/tmux access
//!   ferrite remote ...   → remote bootstrap helpers for SSH/tmux/MCP flows

mod remote;

use std::path::PathBuf;

use shell_core::state::ShellState;
use shell_parser::parse;
use shell_runtime::{executor::Executor, signals};
use shell_tui::readline::{Readline, ReadlineEvent};
use toml::value::Table;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("--mcp") => run_mcp(),
        Some("-c") => run_command(args.get(1).map(String::as_str).unwrap_or("")),
        Some("config") => run_config(&args[1..]),
        Some("install") => run_install(),
        Some("uninstall") => run_uninstall(),
        Some("status") => run_status(),
        Some("up") => remote::run_enable(None),
        Some("remote") => remote::run_remote(&args[1..]),
        _ => run_repl(),
    }
}

// ── MCP server mode ───────────────────────────────────────────────────────────

fn run_mcp() -> anyhow::Result<()> {
    let server = shell_mcp::McpServer::new();
    server
        .run()
        .map_err(|e| anyhow::anyhow!("MCP server error: {e}"))
}

// ── Single command mode ───────────────────────────────────────────────────────

fn run_command(cmd: &str) -> anyhow::Result<()> {
    let state = ShellState::new(false)?;
    let mut executor = Executor::new(state);

    match parse(cmd) {
        Ok(list) => {
            let status = executor.execute_list(&list);
            std::process::exit(status.code());
        }
        Err(e) => {
            eprintln!("ferrite: {e}");
            std::process::exit(2);
        }
    }
}

// ── Config subcommand ─────────────────────────────────────────────────────────

// args is the slice starting after "config", e.g. ["set", "terminal.mode", "always"]
fn run_config(args: &[String]) -> anyhow::Result<()> {
    let sub = args.first().map(String::as_str).unwrap_or("list");

    match sub {
        "set" => {
            let key = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: ferrite config set <key> <value>"))?;
            let val = args
                .get(2)
                .ok_or_else(|| anyhow::anyhow!("usage: ferrite config set <key> <value>"))?;
            let mut cfg = shell_mcp::FerriteConfig::load();
            cfg.set(key, val).map_err(|e| anyhow::anyhow!("{e}"))?;
            cfg.save().map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Set {key} = {val}");
        }
        "get" => {
            let key = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: ferrite config get <key>"))?;
            let cfg = shell_mcp::FerriteConfig::load();
            match cfg.get(key) {
                Some(v) => println!("{v}"),
                None => {
                    eprintln!("unknown key '{key}'");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            let cfg = shell_mcp::FerriteConfig::load();
            let config_path = shell_mcp::config::config_path();
            println!("Config file: {}", config_path.display());
            println!();
            for (k, v) in cfg.list() {
                println!("  {k} = {v}");
            }
        }
    }

    Ok(())
}

// ── Install / uninstall ───────────────────────────────────────────────────────

fn home_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
    PathBuf::from(home)
}

fn claude_json_path() -> PathBuf {
    home_dir().join(".claude.json")
}

fn codex_toml_path() -> PathBuf {
    home_dir().join(".codex/config.toml")
}

fn read_claude_json() -> anyhow::Result<serde_json::Value> {
    let path = claude_json_path();
    if !path.exists() {
        return Ok(serde_json::json!({ "mcpServers": {} }));
    }
    let text = std::fs::read_to_string(&path)?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    Ok(v)
}

fn write_claude_json(v: &serde_json::Value) -> anyhow::Result<()> {
    let path = claude_json_path();
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(v)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn read_codex_toml() -> anyhow::Result<toml::Value> {
    let path = codex_toml_path();
    if !path.exists() {
        return Ok(toml::Value::Table(Table::new()));
    }
    let text = std::fs::read_to_string(&path)?;
    let value: toml::Value = toml::from_str(&text)?;
    Ok(value)
}

fn write_codex_toml(v: &toml::Value) -> anyhow::Result<()> {
    let path = codex_toml_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    let text = toml::to_string_pretty(v)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn get_or_insert_table<'a>(table: &'a mut Table, key: &str) -> anyhow::Result<&'a mut Table> {
    let entry = table
        .entry(key.to_owned())
        .or_insert_with(|| toml::Value::Table(Table::new()));
    entry
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("expected '{key}' to be a TOML table"))
}

fn run_install() -> anyhow::Result<()> {
    let bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ferrite"));

    let mut json = read_claude_json()?;

    // Ensure mcpServers key exists
    if json.get("mcpServers").is_none() {
        json["mcpServers"] = serde_json::json!({});
    }

    json["mcpServers"]["ferrite"] = serde_json::json!({
        "type":    "stdio",
        "command": bin.display().to_string(),
        "args":    ["--mcp"],
        "env":     {}
    });

    write_claude_json(&json)?;

    let mut codex = read_codex_toml()?;
    let codex_root = codex
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("expected Codex config root to be a TOML table"))?;
    let mcp_servers = get_or_insert_table(codex_root, "mcp_servers")?;
    mcp_servers.insert(
        "ferrite".to_owned(),
        toml::Value::Table(Table::from_iter([
            (
                "command".to_owned(),
                toml::Value::String(bin.display().to_string()),
            ),
            (
                "args".to_owned(),
                toml::Value::Array(vec![toml::Value::String("--mcp".to_owned())]),
            ),
        ])),
    );
    write_codex_toml(&codex)?;

    println!("Registered ferrite MCP server at {}.", bin.display());
    println!("Updated:");
    println!("  Claude Code: {}", claude_json_path().display());
    println!("  OpenAI Codex: {}", codex_toml_path().display());
    println!("Restart clients to activate.");
    Ok(())
}

fn run_uninstall() -> anyhow::Result<()> {
    let mut json = read_claude_json()?;

    let claude_removed = json["mcpServers"]
        .as_object_mut()
        .map(|m| m.remove("ferrite").is_some())
        .unwrap_or(false);

    if claude_removed {
        write_claude_json(&json)?;
    }

    let mut codex = read_codex_toml()?;
    let codex_removed = codex
        .as_table_mut()
        .and_then(|root| root.get_mut("mcp_servers"))
        .and_then(toml::Value::as_table_mut)
        .map(|servers| servers.remove("ferrite").is_some())
        .unwrap_or(false);
    if codex_removed {
        write_codex_toml(&codex)?;
    }

    if claude_removed || codex_removed {
        println!("Removed ferrite MCP registration from:");
        if claude_removed {
            println!("  Claude Code: {}", claude_json_path().display());
        }
        if codex_removed {
            println!("  OpenAI Codex: {}", codex_toml_path().display());
        }
        println!("Restart clients to deactivate.");
    } else {
        println!("ferrite was not registered in Claude Code or OpenAI Codex.");
    }
    Ok(())
}

// ── Status ────────────────────────────────────────────────────────────────────

fn run_status() -> anyhow::Result<()> {
    let cfg = shell_mcp::FerriteConfig::load();
    let config_path = shell_mcp::config::config_path();

    println!("=== ferrite status ===");
    println!();
    println!(
        "Binary: {}",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".to_owned())
    );
    println!("Config: {}", config_path.display());
    println!();

    println!("[config]");
    for (k, v) in cfg.list() {
        println!("  {k} = {v}");
    }
    println!();

    let json = read_claude_json().unwrap_or_default();
    let claude_registered = json["mcpServers"]["ferrite"].is_object();
    if claude_registered {
        let cmd = json["mcpServers"]["ferrite"]["command"]
            .as_str()
            .unwrap_or("?");
        println!("[Claude Code] registered ✓  (command: {cmd})");
    } else {
        println!("[Claude Code] not registered  — run 'ferrite install' to register");
    }

    let codex = read_codex_toml().unwrap_or_else(|_| toml::Value::Table(Table::new()));
    let codex_cmd = codex
        .get("mcp_servers")
        .and_then(|v| v.get("ferrite"))
        .and_then(|v| v.get("command"))
        .and_then(toml::Value::as_str);
    if let Some(cmd) = codex_cmd {
        println!("[OpenAI Codex] registered ✓  (command: {cmd})");
    } else {
        println!("[OpenAI Codex] not registered  — run 'ferrite install' to register");
    }

    Ok(())
}

// ── Interactive REPL ──────────────────────────────────────────────────────────

fn run_repl() -> anyhow::Result<()> {
    let state = ShellState::new(true)?;
    let mut executor = Executor::new(state);

    signals::install_interactive_handlers()
        .map_err(|e| anyhow::anyhow!("signal setup failed: {e}"))?;

    let mut readline = Readline::new();

    loop {
        match readline.read_line(&mut executor.state, &mut executor.hooks)? {
            ReadlineEvent::Line(line) => {
                let line = line.trim().to_owned();
                if line.is_empty() {
                    continue;
                }
                match parse(&line) {
                    Ok(list) => {
                        executor.execute_list(&list);
                    }
                    Err(e) => {
                        eprintln!("ferrite: parse error: {e}");
                        executor.state.last_status = shell_core::types::ExitStatus(2);
                    }
                }
            }
            ReadlineEvent::Interrupted => {
                eprintln!();
                continue;
            }
            ReadlineEvent::Eof => {
                eprintln!("exit");
                break;
            }
        }
    }

    Ok(())
}
