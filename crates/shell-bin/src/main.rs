//! cream — entry point.
//!
//! Modes:
//!   cream          → interactive REPL
//!   cream --mcp    → MCP stdio server (for Claude Code integration)
//!   cream -c <cmd> → execute a single command string and exit

use shell_core::state::ShellState;
use shell_parser::parse;
use shell_runtime::{executor::Executor, signals};
use shell_tui::readline::{Readline, ReadlineEvent};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("--mcp") => run_mcp(),
        Some("-c")    => run_command(args.get(1).map(String::as_str).unwrap_or("")),
        _             => run_repl(),
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
            eprintln!("cream: {e}");
            std::process::exit(2);
        }
    }
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
                        eprintln!("cream: parse error: {e}");
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
