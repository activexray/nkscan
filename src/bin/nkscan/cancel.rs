//! A Ctrl-c the rest of the program can check for and stop at its own next
//! safe point, instead of the default that kills wherever it happens to be.
//!
//! A stage move in flight has no checkpoint and always finishes; aborting
//! one mid-motion grinds the mechanism until a power cycle. A repeated
//! Ctrl-c does not force an exit for the same reason.

use std::{
    io::{IsTerminal, Write, stderr},
    sync::atomic::{AtomicBool, Ordering},
};

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Catch Ctrl-c and note it happened
pub fn install() {
    let _ = ctrlc::set_handler(|| {
        if !REQUESTED.swap(true, Ordering::SeqCst) {
            let mut err = stderr().lock();
            // The terminal has already echoed `^C` onto whatever line the bars
            // were drawing. Wind back over it so the notice replaces it rather
            // than hanging off the end of a half-drawn bar
            if err.is_terminal() {
                let _ = err.write_all(b"\r\x1b[2K");
            }
            let _ = writeln!(
                err,
                "stopping at the next safe point, this can take a moment"
            );
        }
    });
}

/// Whether the operator has asked to stop
pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}
