//! Scanning one frame: focus, meter, take the pass, and optionally clean it

use crate::{
    error::Error,
    protocol::{data::Rect, decode::Samples},
    scan::{
        autoexpose::Exposures,
        clean::clean_frame,
        focus::Focus,
        framing,
        pass::{Pass, Progress},
        window::Recipe,
    },
    session::Session,
};
use std::{ops::ControlFlow, time::Duration};
use tracing::*;

/// Long enough for a full-resolution pass over the largest frame
const SCAN_TIMEOUT: Duration = Duration::from_secs(1800);

/// Which pass `scan_frame_with`'s progress belongs to
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Metering, and which pass of it, counting from one
    Meter(usize),
    /// The scan pass itself
    Scan,
}

/// What to do beyond taking the pass
#[derive(Debug, Clone, Copy, Default)]
pub struct Options<'a> {
    /// Reuse an exposure already decided; `None` meters this frame fresh
    pub exposures: Option<&'a Exposures>,
    /// Honored only where `exposures` is `None`
    pub lock_white_balance: bool,
    /// Run dust removal over the result in place
    pub clean: bool,
}

/// What one frame's scan produced
pub struct Scanned {
    /// The pass over the rectangle that was asked for
    ///
    /// The pass is that rectangle: the window is set to it, and on a unit that
    /// positions the film by its own table the rectangle is registered with the
    /// unit first so the window reaches it
    pub pass: Pass,
    /// What the frame was exposed at
    pub exposures: Exposures,
    /// Pixels dust removal rebuilt, where asked for
    pub cleaned: Option<usize>,
}

/// Focus, meter, and take the pass over `frame`
pub fn scan_frame(
    session: &mut Session,
    recipe: &Recipe,
    frame: Rect,
    options: Options,
    samples: &mut Samples,
) -> Result<Scanned, Error> {
    scan_frame_with(session, recipe, frame, options, samples, |_, _| {
        ControlFlow::Continue(())
    })
}

/// The same as [`scan_frame`], with `on` told which [`Phase`] is running and able to cancel it by returning `Break`
pub fn scan_frame_with(
    session: &mut Session,
    recipe: &Recipe,
    frame: Rect,
    options: Options,
    samples: &mut Samples,
    mut on: impl FnMut(Phase, Progress) -> ControlFlow<()>,
) -> Result<Scanned, Error> {
    // Where the unit positions the film by its own frame table, the window's Y
    // picks a table entry and reads as an offset into that frame rather than as
    // an address, so the rectangle has to be in the table before the window can
    // reach it. A table written for this pass comes back off the unit afterwards
    let registered = framing::register(session, frame)?;
    session.focus_frame(frame, Focus::default())?;

    let exposures = match options.exposures {
        Some(locked) => locked.clone(),
        None => {
            let lock = options.lock_white_balance;
            session
                .autoexpose_frame_with(frame, recipe, lock, |pass, p| on(Phase::Meter(pass), p))?
        }
    };

    let mut windows = recipe.windows(session.capabilities(), frame)?;
    exposures.apply(&mut windows);

    let pass = session.scan_pass_with(&windows, SCAN_TIMEOUT, samples, |p| on(Phase::Scan, p))?;

    // The table goes back to what it measured, so the next rectangle of this
    // session starts from the measured table rather than this one's place.
    // Written rather than registered: `register` starts from the measured
    // table, and would find the entry it wants already in it, so it would do
    // nothing and leave this pass's table in the unit
    if registered
        && let Some(measured) = session.frames_type2().cloned()
        && let Err(e) = session.set_boundaries_type2_for_pass(&measured)
    {
        warn!(
            %e,
            "could not put the measured frame table back, so a later rectangle may position against this one"
        );
    }

    samples.to_full_scale(pass.layout.bits_per_sample);

    let cleaned = options
        .clean
        .then(|| clean_frame(samples, &pass, session.capabilities().identity.model()))
        .transpose()?;

    Ok(Scanned {
        pass,
        exposures,
        cleaned,
    })
}
