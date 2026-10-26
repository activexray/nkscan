//! A clock and a sleep for native and browser builds. In a browser `Instant::now` panics and a thread cannot block

use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;
#[cfg(target_arch = "wasm32")]
pub use web_time::Instant;

/// Wait `duration`. Natively this blocks the thread, and in a browser it yields to the event loop
pub async fn sleep(duration: Duration) {
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::sleep(duration);
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::sleep(duration).await;
}
