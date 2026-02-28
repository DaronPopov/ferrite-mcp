//! ferrite — entry point.
//!
//! Modes:
//!   ferrite              → interactive REPL
//!   ferrite --mcp        → MCP stdio server (for Claude Code integration)
//!   ferrite -c <cmd>     → execute a single command string and exit
//!   ferrite config set <key> <value>
//!   ferrite config get <key>
//!   ferrite config list
//!   ferrite install      → register MCP server in ~/.claude.json
//!   ferrite uninstall    → remove MCP server from ~/.claude.json
//!   ferrite status       → show config + MCP registration status

use shell_core::state::ShellState;
use shell_parser::parse;
use shell_runtime::{executor::Executor, signals};
use shell_tui::readline::{Readline, ReadlineEvent};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("--mcp")     => run_mcp(),
        Some("-c")        => run_command(args.get(1).map(String::as_str).unwrap_or("")),
        Some("config")    => run_config(&args[1..]),
        Some("install")   => run_install(),
        Some("uninstall") => run_uninstall(),
        Some("status")    => run_status(),
        _                 => run_repl(),
    }
}

// ── MCP server mode ───────────────────────────────────────────────────────────

fn run_mcp() -> anyhow::Result<()> {
    let server = shell_mcp::McpServer::new();
    server.run().map_err(|e| anyhow::anyhow!("MCP server error: {e}"))
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
            let key = args.get(1).ok_or_else(|| anyhow::anyhow!("usage: ferrite config set <key> <value>"))?;
            let val = args.get(2).ok_or_else(|| anyhow::anyhow!("usage: ferrite config set <key> <value>"))?;
            let mut cfg = shell_mcp::FerriteConfig::load();
            cfg.set(key, val).map_err(|e| anyhow::anyhow!("{e}"))?;
            cfg.save().map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Set {key} = {val}");
        }
        "get" => {
            let key = args.get(1).ok_or_else(|| anyhow::anyhow!("usage: ferrite config get <key>"))?;
            let cfg = shell_mcp::FerriteConfig::load();
            match cfg.get(key) {
                Some(v) => println!("{v}"),
                None    => {
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

fn claude_json_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
    std::path::PathBuf::from(home).join(".claude.json")
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
    let tmp  = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(v)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn run_install() -> anyhow::Result<()> {
    let bin = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from("ferrite"));

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
    println!("Registered ferrite MCP server at {}.", bin.display());
    println!("Restart Claude Code to activate.");
    Ok(())
}

fn run_uninstall() -> anyhow::Result<()> {
    let mut json = read_claude_json()?;

    let removed = json["mcpServers"]
        .as_object_mut()
        .map(|m| m.remove("ferrite").is_some())
        .unwrap_or(false);

    if removed {
        write_claude_json(&json)?;
        println!("Removed ferrite from MCP servers.");
        println!("Restart Claude Code to deactivate.");
    } else {
        println!("ferrite was not registered (nothing to remove).");
    }
    Ok(())
}

// ── Status ────────────────────────────────────────────────────────────────────

fn run_status() -> anyhow::Result<()> {
    let cfg = shell_mcp::FerriteConfig::load();
    let config_path = shell_mcp::config::config_path();

    println!("=== ferrite status ===");
    println!();
    println!("Binary: {}", std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "?".to_owned()));
    println!("Config: {}", config_path.display());
    println!();

    println!("[config]");
    for (k, v) in cfg.list() {
        println!("  {k} = {v}");
    }
    println!();

    // Check ~/.claude.json registration
    let json = read_claude_json().unwrap_or_default();
    let registered = json["mcpServers"]["ferrite"].is_object();
    if registered {
        let cmd = json["mcpServers"]["ferrite"]["command"].as_str().unwrap_or("?");
        println!("[MCP] registered ✓  (command: {cmd})");
    } else {
        println!("[MCP] not registered  — run 'ferrite install' to register");
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
                    Ok(list) => { executor.execute_list(&list); }
                    Err(e) => {
                        eprintln!("ferrite: parse error: {e}");
                        executor.state.last_status = shell_core::types::ExitStatus(2);
                    }
                }
            }
            ReadlineEvent::Interrupted => { eprintln!(); continue; }
            ReadlineEvent::Eof => { eprintln!("exit"); break; }
        }
    }

    Ok(())
}
