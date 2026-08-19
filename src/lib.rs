//! A cross-platform driver for Nikon (Coolscan) film scanners
//!
//! ```no_run
//! use nkscan::{device, session::Session};
//!
//! let devices = device::list();
//! let device = devices.first().expect("no scanner found");
//! let mut session = Session::open(device.open()?)?;
//!
//! session.stage()?; // homes the mechanism once a holder is loaded
//! let caps = session.capabilities();
//! # Ok::<(), nkscan::error::Error>(())
//! ```

pub mod device;
pub mod dust;
pub mod error;
#[cfg(feature = "python")]
pub mod python;
pub mod protocol;
pub mod scan;
pub mod session;
pub mod transport;
