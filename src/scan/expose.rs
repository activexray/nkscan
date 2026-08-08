//! How the exposures get decided, and the prescan window builder

use crate::{
    error::Error,
    protocol::{
        caps::{
            Capabilities,
            set_window::{AnalogControl, ScanKind, ScanMode},
        },
        window::{Flags, Window},
    },
    scan::meter::Metering,
};
use tracing::*;

/// How the exposures get decided
///
/// `SetWindowFunction` byte 4 says whether the unit will meter for itself. If
/// neither AE bit is set, we do it. There is no host-cooperation bit for this in
/// `Features` the way there is for autofocus, so the missing scan kind is the
/// only signal.
#[derive(Debug, Clone, Copy)]
pub enum Exposure {
    /// The unit meters itself. This is a scanning kind, so it goes in the
    /// window descriptor
    Unit(ScanKind),
    /// We take an ordinary pass and work the exposures out from it
    Host(Metering),
}

impl Exposure {
    /// Pick whichever mechanism this unit has
    pub fn choose(caps: &Capabilities, lock_white_balance: bool) -> Result<Self, Error> {
        let kinds = caps.set_window.kind;

        if lock_white_balance && kinds.contains(ScanKind::AE_WB) {
            return Ok(Self::Unit(ScanKind::AE_WB));
        }
        if !lock_white_balance && kinds.contains(ScanKind::AE) {
            return Ok(Self::Unit(ScanKind::AE));
        }

        // We meter by moving the exposure in the descriptor, so the unit has to
        // offer that as an analog control. `SetWindowFunction` byte 14
        let aic = caps.set_window.aic;
        if !aic.intersects(AnalogControl::EXPOSURE_VALUE | AnalogControl::EXPOSURE_TIME) {
            return Err(Error::Unsupported {
                op: "exposure",
                reason: format!(
                    "this unit runs no AE pass and offers no exposure control, only {aic:?}"
                ),
            });
        }

        Ok(Self::Host(Metering {
            lock_white_balance,
            ..Metering::default()
        }))
    }
}

/// The same windows, shrunk to something quick to take and read
///
/// Lowest resolution the unit offers, high speed if it has it, and no
/// multisampling. Anything past that only costs time, and multisampling or
/// multi-line reading would make the pass ask us for post-processing first.
pub fn prescan_windows(caps: &Capabilities, windows: &[Window]) -> Vec<Window> {
    let dpi = caps.address.x_axis.dpi_range.start;
    let fast = caps.set_window.mode.contains(ScanMode::HIGH_SPEED);

    windows
        .iter()
        .map(|w| {
            let mut w = w.clone();
            // A preview halves Y where a scan is square, and runs without the
            // averaging bit. The captures pair 666x333 with byte 41 = 01h and
            // high speed every time
            w.resolution = (dpi, dpi / 2);
            w.flags.remove(Flags::AVERAGING);
            if fast {
                w.scanning_mode = ScanMode::HIGH_SPEED;
            }
            // Nikon Scan meters off a single reading, and averaging costs a
            // pass what it saves nothing on. Both bytes have to say so
            w.multiple_reading = 0;
            w.scanning_mode.remove(ScanMode::MULTI_READING);
            w
        })
        .collect()
}

/// Whether a set has anything worth metering
///
/// Infrared measures obstructions, so a set of nothing else has no exposure to decide from a film's tones.
pub fn meterable(windows: &[Window]) -> bool {
    windows.iter().any(|w| w.channel().is_color())
}
