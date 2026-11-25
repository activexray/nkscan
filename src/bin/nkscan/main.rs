use clap::Parser;
use cli::Cli;
use nkscan::{device, transport::Transport};
use std::io::{IsTerminal, stderr};
use tracing_subscriber::EnvFilter;

mod cache;
mod cancel;
mod cli;
mod dump;
mod eject;
mod io;
mod mono;
mod progress;
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

/// Open the `--device` unit, or the only one attached, and list what is attached if that fails
async fn open(location: Option<String>) -> anyhow::Result<(device::Device, Box<dyn Transport>)> {
    let selector = location.map_or(device::Selector::Only, device::Selector::Location);
    match selector.open().await {
        Ok(opened) => Ok(opened),
        Err(e) => {
            let list: Vec<_> = device::list()
                .await
                .iter()
                .map(ToString::to_string)
                .collect();
            Err(anyhow::anyhow!("{e}\n\nattached:\n  {}", list.join("\n  ")))
        }
    }
}

fn main() -> anyhow::Result<()> {
    enable_ansi_support();
    cancel::install();

    let cli = Cli::parse();

    // Set up logging. RUST_LOG overrides everything, since it can target individual modules
    // Otherwise fall back to --log-level (default: info). nusb logs one line per USB
    // transfer at debug, which drowns out everything else in a `--log debug` capture, so
    // the default keeps it at warn; RUST_LOG can still ask for it by name.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(format!("{},nusb=warn", cli.log))),
        )
        .with_target(false)
        // The bars and the log share stderr, so that is what decides both the
        // color and which stream a line has to be sequenced against
        .with_ansi(stderr().is_terminal())
        .with_writer(progress::Writer)
        .init();

    // Perform the requested CLI action
    match cli.action {
        cli::Action::List => {
            let devs = pollster::block_on(device::list());
            println!("Attached scanners:");
            devs.iter().for_each(|x| println!("{x}"));
        }
        cli::Action::Scan(args) => {
            let outcome = pollster::block_on(scan::run(args));
            // A pass that ended early still has its bar drawn
            progress::clear();
            outcome?
        }
        cli::Action::Dump(args) => pollster::block_on(dump::run(args))?,
        cli::Action::Eject(args) => pollster::block_on(eject::run(args))?,
    }

    // Donezo
    Ok(())
}
