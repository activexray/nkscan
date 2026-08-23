//! Opening a session resiliently
//!
//! A unit an earlier command left mid-transaction sometimes stops answering
//! its bulk endpoints at all, which reads as the very first command timing
//! out before a session even exists. A USB reset is the software equivalent
//! of unplugging it - not guaranteed to clear a unit that is genuinely
//! wedged, but worth one try before asking the operator for a power cycle.

use nkscan::{
    device::{self, Device},
    error::Error,
    session::Session,
    transport,
};
use std::{thread::sleep, time::Duration};
use tracing::warn;

/// How long a reset USB device takes to reappear on the bus
const REENUMERATE: Duration = Duration::from_millis(1500);

/// [`Session::open`], retried once with a USB reset if the very first
/// command times out
pub fn open(device: &Device) -> anyhow::Result<Session> {
    match Session::open(device.open()?) {
        Err(Error::Transport(transport::Error::Timeout(_))) => {
            warn!("the scanner did not answer - resetting the USB connection and trying once more");
            device::reset(device)?;
            sleep(REENUMERATE);

            // A reset only asked the device to re-enumerate; nothing here
            // says it is back yet, so this looks it up again rather than
            // reusing the handle a reset just invalidated
            let device = device::list()
                .into_iter()
                .find(|d| d.attach == device.attach)
                .ok_or(Error::NotFound)?;
            Ok(Session::open(device.open()?)?)
        }
        other => Ok(other?),
    }
}
