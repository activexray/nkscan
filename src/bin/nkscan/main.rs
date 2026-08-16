use clap::Parser;
use cli::Cli;
use nkscan::device;
use std::io::{IsTerminal, stdout};
use tracing_subscriber::EnvFilter;

mod cli;
mod dump;
mod eject;
mod io;
mod mono;
mod scan;

// Legacy windows command prompt doesn't interpret ANSI escapes until a process opts in via SetConsoleMode.
// Without this, coloring anything on Windows prints raw escape codes instead of color.
#[cfg(target_os = "windows")]
fn enable_ansi_support() {
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE,
        SetConsoleMode,
    };
    // Safety: STD_OUTPUT_HANDLE is a well-known pseudo-handle, valid for the life of the
    // process, so GetStdHandle needs no cleanup. GetConsoleMode/SetConsoleMode take that
    // handle and an out-param/value of the right type; failure (handle redirected to a
    // file, not a console at all) is reported through the BOOL return, checked below, not
    // through UB. No pointers into Rust-managed memory escape this block.
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) != 0 {
            SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn enable_ansi_support() {}

fn main() -> anyhow::Result<()> {
    enable_ansi_support();

    let cli = Cli::parse();

    // Set up logging. RUST_LOG overrides everything, since it can target individual modules
    // Otherwise fall back to --log-level (default: info).
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(cli.log.to_string())),
        )
        .with_target(false)
        .with_ansi(stdout().is_terminal()) // Only emit color when stdout is a real terminal
        .init();

    // Perform the requested CLI action
    match cli.action {
        cli::Action::List => {
            let devs = device::list();
            println!("Attached scanners:");
            devs.iter().for_each(|x| println!("{x}"));
        }
        cli::Action::Scan(args) => scan::run(args)?,
        cli::Action::Dump(args) => dump::run(args)?,
        cli::Action::Eject(args) => eject::run(args)?,
    }

    // Donezo
    Ok(())
}
