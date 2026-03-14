#![allow(dead_code)]

#[path = "remote.rs"]
mod remote;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    remote::run_enable(args.first().map(String::as_str))
}
