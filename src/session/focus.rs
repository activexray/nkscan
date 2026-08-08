//! Focusing, which is EXECUTE operations `Op::AutoAf`, `Op::AutoFocus`,
//! `Op::ColorAutoFocus` and `Op::FocusMove`. Section 2-15-4

use super::Session;
use crate::{
    error::Error,
    protocol::{
        caps::{address::Axis, other::HostCooperation},
        data::{Op, Operation},
        sense::{Failure, Fault},
        window::Window,
    },
    scan::focus::{Focus, Focused},
};
use std::time::Duration;
use tracing::*;

/// A focus move drives the lens alone and settles in seconds
const FOCUS_TIMEOUT: Duration = Duration::from_secs(60);

/// Autofocus takes an address on the medium, and the sub-scanning half of that
/// is the feed, so reaching it can move the stage
const AUTOFOCUS_TIMEOUT: Duration = Duration::from_secs(180);

/// [`Session::execute`] checks this too, but checking before the arguments means a unit that cannot do the thing says so, rather than faulting a coordinate
fn offers(session: &Session, operation: Op) -> Result<(), Error> {
    if session.capabilities().features.execute.supports(operation) {
        return Ok(());
    }
    Err(Error::Unsupported {
        op: "execute operation",
        reason: format!("this unit does not offer {operation:?}"),
    })
}

impl Session {
    /// Focus on a point of the medium
    ///
    /// The address is the one a window origin uses, whatever 2-15 means by
    /// calling it an address on the medium: the captures focus a window at top
    /// 10512 length 6696 at 13860, its center.
    ///
    /// Some addresses inside the range are still answered instantly with out of
    /// focus, having moved nothing. What bounds that is not yet known, so this
    /// only checks the range the axis reports.
    ///
    /// `color` picks the channel, which needs the unit to offer
    /// `Op::ColorAutoFocus`; `None` uses `Op::AutoFocus` and lets it choose.
    pub fn autofocus(&mut self, x: u32, y: u32, color: Option<u8>) -> Result<(), Error> {
        let operation = match color.is_some() {
            true => Op::ColorAutoFocus,
            false => Op::AutoFocus,
        };
        offers(self, operation)?;

        // A unit that sets this expects the initiator to do the focusing, which is a different job to asking the unit to do it
        // I think all of our scanners have hardware AF
        let coop = self.capabilities().features.cooperation;
        if coop.contains(HostCooperation::AUTOFOCUS) {
            return Err(Error::Unsupported {
                op: "autofocus",
                reason: "this unit leaves focusing to the driver".into(),
            });
        }

        let caps = self.capabilities();
        for (axis, name, value) in [
            (&caps.address.x_axis, 'X', x),
            (&caps.address.y_axis, 'Y', y),
        ] {
            if !axis.address_range.contains(&value) {
                return Err(Error::Unsupported {
                    op: "autofocus address",
                    reason: format!(
                        "{name} {value} is outside {} to {}",
                        axis.address_range.start, axis.address_range.last
                    ),
                });
            }
        }

        // The unit resolves the address against its frame table, and answers one
        // that lands in no frame with out of focus, which a real search failure
        // is indistinguishable from
        if let Some(frames) = self.frames()
            && !frames.frames.is_empty()
            && frames.at(x, y).is_none()
        {
            return Err(Error::Unsupported {
                op: "autofocus address",
                reason: format!("({x}, {y}) is in none of the {:?}", frames.frames),
            });
        }

        self.execute(
            operation,
            Operation {
                color: color.unwrap_or(0),
                first: x,
                second: y,
            },
            AUTOFOCUS_TIMEOUT,
        )
    }

    /// Move the scan block to an absolute focus position
    pub fn focus_to(&mut self, position: u16) -> Result<(), Error> {
        offers(self, Op::FocusMove)?;
        let range = self.capabilities().address.focus_range;
        if !range.contains(&position) {
            return Err(Error::Unsupported {
                op: "focus position",
                reason: format!("{position} is outside {} to {}", range.start, range.last),
            });
        }

        self.execute(
            Op::FocusMove,
            Operation {
                first: u32::from(position),
                ..Operation::default()
            },
            FOCUS_TIMEOUT,
        )
    }

    /// Let the unit focus itself when it decides it needs to
    pub fn set_auto_focus(&mut self, on: bool) -> Result<(), Error> {
        offers(self, Op::AutoAf)?;
        self.execute(
            Op::AutoAf,
            Operation {
                first: u32::from(on),
                ..Operation::default()
            },
            FOCUS_TIMEOUT,
        )
    }

    /// Focus for a scan of `windows`, per `focus`
    ///
    /// The address is worked out from the first window. A set has to agree on
    /// geometry, so any of them would do.
    pub fn focus_with(&mut self, focus: Focus, windows: &[Window]) -> Result<Focused, Error> {
        let Some(window) = windows.first() else {
            return Ok(Focused::Skipped);
        };

        match focus {
            Focus::Hold => Ok(Focused::Skipped),
            Focus::At(position) => self.focus_to(position).map(|()| Focused::Yes),
            Focus::Auto { at, color } => {
                let caps = self.capabilities();
                let point = |axis: &Axis, origin: u32, size: u32, fraction: f32| {
                    let offset = (size as f32 * fraction.clamp(0.0, 1.0)) as u32;
                    origin
                        .saturating_add(offset)
                        .clamp(axis.address_range.start, axis.address_range.last)
                };
                let x = point(&caps.address.x_axis, window.origin.0, window.size.0, at.0);
                let y = point(&caps.address.y_axis, window.origin.1, window.size.1, at.1);

                debug!(x, y, "focusing");
                let outcome = match self.autofocus(x, y, color) {
                    Ok(()) => Ok(Focused::Yes),
                    Err(Error::Device(fault))
                        if matches!(*fault, Fault::Reported(Failure::OutOfFocus, _)) =>
                    {
                        warn!(x, y, "autofocus did not reach focus");
                        Ok(Focused::NotReached)
                    }
                    Err(e) => Err(e),
                };

                if let Ok(params) = self.get_parameter(Op::FocusMove) {
                    info!(position = params.first, "focused at");
                }
                outcome
            }
        }
    }
}
