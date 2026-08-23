//! A Ctrl-c the rest of the program can check for and stop at its own next
//! safe point, rather than the default that kills wherever it happens to be
//!
//! That matters here specifically: aborting a stage move mid-motion grinds
//! the mechanism until a power cycle, and Rust's default SIGINT handling
//! skips `Drop` entirely, so `Session`'s own cleanup never runs either. A
//! move already in flight has nothing to check this against and always runs
//! to completion, which is what keeps this safe rather than just quieter.

use std::sync::atomic::{AtomicBool, Ordering};
use tracing::warn;

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Catch Ctrl-c once. A second one forces an immediate exit, for whatever
/// has no checkpoint of its own to reach - a blocked stdin read, or a
/// single device call already past any budget worth waiting out
pub fn install() {
    // Nothing to fall back to if this fails beyond the platform default, and
    // that default (kill wherever it happens to be) is the thing this exists
    // to avoid - not worth refusing to start the program over
    let _ = ctrlc::set_handler(|| {
        if REQUESTED.swap(true, Ordering::SeqCst) {
            warn!("Ctrl-c again: stopping now, whatever the unit was doing");
            std::process::exit(130);
        }
    });
}

/// Whether the operator has asked to stop
pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}
