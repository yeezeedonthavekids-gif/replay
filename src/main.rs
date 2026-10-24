//! Replay: a lightweight instant-replay recorder for Windows.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod core;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("--selftest") => win::selftest::run(&args[1..]),
        _ => win::app::run(args.iter().any(|a| a == "--background"), args.iter().any(|a| a == "--clips")),
    };
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Replay is a Windows application. On other platforms only the unit tests run.");
}
