use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    let mut args = env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "check-all".to_owned());
    let result = match cmd.as_str() {
        "check-all" => check_all(),
        "check-main" => check_main(),
        "check-ui" => check_ui(),
        "check-macos" => check_macos(),
        _ => {
            eprintln!("unknown xtask command: {cmd}");
            eprintln!("usage: cargo run -p xtask -- [check-all|check-main|check-ui|check-macos]");
            Err(())
        }
    };

    if result.is_err() {
        std::process::exit(1);
    }
}

fn check_all() -> Result<(), ()> {
    check_main()?;
    check_ui()?;
    check_macos()?;
    Ok(())
}

fn check_main() -> Result<(), ()> {
    run(Path::new("."), "scripts/check-boundaries.sh", &[])?;
    run(Path::new("."), "cargo", &["check", "--workspace"])?;
    run(Path::new("."), "cargo", &["test", "-p", "shell-mcp"])?;
    Ok(())
}

fn check_ui() -> Result<(), ()> {
    let ui = Path::new("third_party/warp-ui");
    if !ui.join("Cargo.toml").exists() {
        println!("skip: third_party/warp-ui is not present");
        return Ok(());
    }
    run(ui, "cargo", &["check", "-p", "warpui", "--lib"])?;
    Ok(())
}

fn check_macos() -> Result<(), ()> {
    for target in ["x86_64-apple-darwin", "aarch64-apple-darwin"] {
        if !target_installed(target) {
            println!("skip: Rust target {target} is not installed");
            continue;
        }
        run_without_host_rustflags(
            Path::new("."),
            "cargo",
            &["check", "--workspace", "--target", target],
        )?;
    }

    let ui = Path::new("third_party/warp-ui");
    if ui.join("Cargo.toml").exists() {
        if macos_sdk_available() {
            for target in ["x86_64-apple-darwin", "aarch64-apple-darwin"] {
                if target_installed_for_warp(target) {
                    run_without_host_rustflags(
                        ui,
                        "cargo",
                        &["check", "-p", "warpui", "--lib", "--target", target],
                    )?;
                }
            }
        } else {
            println!("skip: Warp UI macOS target check requires Apple SDK headers and xcrun/metal");
        }
    }

    Ok(())
}

fn run(cwd: &Path, program: &str, args: &[&str]) -> Result<(), ()> {
    println!("$ {} {}", program, args.join(" "));
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| {
            eprintln!("failed to run {program}: {e}");
        })?;
    if status.success() {
        Ok(())
    } else {
        eprintln!("{program} exited with {status}");
        Err(())
    }
}

fn run_without_host_rustflags(cwd: &Path, program: &str, args: &[&str]) -> Result<(), ()> {
    println!("$ {} {}", program, args.join(" "));
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .status()
        .map_err(|e| {
            eprintln!("failed to run {program}: {e}");
        })?;
    if status.success() {
        Ok(())
    } else {
        eprintln!("{program} exited with {status}");
        Err(())
    }
}

fn target_installed(target: &str) -> bool {
    installed_targets(None).iter().any(|t| t == target)
}

fn target_installed_for_warp(target: &str) -> bool {
    installed_targets(Some("1.92.0"))
        .iter()
        .any(|t| t == target)
}

fn installed_targets(toolchain: Option<&str>) -> Vec<String> {
    let mut cmd = Command::new("rustup");
    if let Some(toolchain) = toolchain {
        cmd.arg(format!("+{toolchain}"));
    }
    let output = cmd.args(["target", "list", "--installed"]).output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn macos_sdk_available() -> bool {
    if env::consts::OS != "macos" {
        return false;
    }
    let sdk_path = Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_owned()));
    sdk_path
        .as_ref()
        .is_some_and(|path| path.join("usr/include/simd/simd.h").exists())
}
