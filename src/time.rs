//! A clock and a sleep that work in a browser as well as natively
//!
//! `std::time::Instant::now` panics on `wasm32-unknown-unknown`, and a browser
//! thread cannot block, so the session layer takes both from here

use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;
#[cfg(target_arch = "wasm32")]
pub use web_time::Instant;

/// Wait `duration` before going on
///
/// Natively this blocks the thread, which is what the native transports do
/// inside their futures as well, so a session future stays ready the first time
/// it is polled. In a browser it yields to the event loop
pub async fn sleep(duration: Duration) {
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::sleep(duration);
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::sleep(duration).await;
}
