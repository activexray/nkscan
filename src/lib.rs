//! A cross-platform driver for Nikon (Coolscan) film scanners
//!
//! Calls that talk to the unit are async but block natively. Use `pollster::block_on`, or `spawn_blocking` under tokio
//!
//! ```no_run
//! use nkscan::{device, session::Session};
//!
//! # pollster::block_on(async {
//! let devices = device::list().await;
//! let device = devices.first().expect("no scanner found");
//! let mut session = Session::open(device.open().await?).await?;
//!
//! session.stage().await?; // homes the mechanism once a holder is loaded
//! let caps = session.capabilities();
//! # Ok::<(), nkscan::error::Error>(())
//! # });
//! ```

pub mod device;
pub mod dust;
pub mod error;
#[cfg(feature = "python")]
pub mod python;
#[cfg(feature = "python")]
pub use python::stub_info as python_stub_info;
pub mod protocol;
pub mod scan;
pub mod session;
pub mod time;
pub mod transport;
