//! Scanning one frame: focus, meter, take the pass, and optionally clean it

use crate::{
    error::Error,
    protocol::{data::Rect, decode::Samples},
    scan::{
        autoexpose::Exposures,
        clean::clean_frame,
        focus::{Focus, Focused},
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
    /// How to focus before the pass
    pub focus: Focus,
}

/// What one frame's scan produced
pub struct Scanned {
    /// The pass over the rectangle that was asked for, which is that rectangle
    /// at both ends
    pub pass: Pass,
    /// What the frame was exposed at
    pub exposures: Exposures,
    /// Pixels dust removal rebuilt, where asked for
    pub cleaned: Option<usize>,
    /// The focus result
    pub focused: Focused,
    /// The lens position during the pass. `None` if the unit does not report it
    pub focus_position: Option<u16>,
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
    let (pass, exposures, focused, focus_position) = at_frame(session, frame, |session| {
        let (focused, focus_position) = focus_on(session, frame, options.focus)?;

        let exposures = match options.exposures {
            Some(locked) => locked.clone(),
            None => {
                let lock = options.lock_white_balance;
                session.autoexpose_frame_with(frame, recipe, lock, |pass, p| {
                    on(Phase::Meter(pass), p)
                })?
            }
        };

        let mut windows = recipe.windows(session.capabilities(), frame)?;
        exposures.apply(&mut windows);

        let pass =
            session.scan_pass_with(&windows, SCAN_TIMEOUT, samples, |p| on(Phase::Scan, p))?;
        Ok((pass, exposures, focused, focus_position))
    })?;
    samples.to_full_scale(pass.layout.bits_per_sample);

    let cleaned = options
        .clean
        .then(|| clean_frame(samples, &pass, session.capabilities().identity.model()))
        .transpose()?;

    Ok(Scanned {
        pass,
        exposures,
        cleaned,
        focused,
        focus_position,
    })
}

/// Meter `frame` without taking its pass
///
/// The exposures are the ones [`scan_frame_with`] meters for itself, so handing
/// them to a later scan through [`Options::exposures`] exposes that frame the
/// way this one would have been. `on` is told which metering pass is running,
/// counting from one, and can cancel it by returning `Break`
pub fn meter_frame_with(
    session: &mut Session,
    recipe: &Recipe,
    frame: Rect,
    lock_white_balance: bool,
    on: impl FnMut(usize, Progress) -> ControlFlow<()>,
) -> Result<Exposures, Error> {
    at_frame(session, frame, |session| {
        session.autoexpose_frame_with(frame, recipe, lock_white_balance, on)
    })
}

/// Focus on `frame` without metering or scanning it
///
/// Returns the focus result and the lens position. The position is `None` if
/// the unit does not report it. To scan at this focus, give the scan
/// [`Focus::Hold`]
pub fn focus_frame(
    session: &mut Session,
    frame: Rect,
    focus: Focus,
) -> Result<(Focused, Option<u16>), Error> {
    at_frame(session, frame, |session| focus_on(session, frame, focus))
}

/// Focus on `frame`, then read the lens position. The frame must already be
/// in the unit's frame table
fn focus_on(
    session: &mut Session,
    frame: Rect,
    focus: Focus,
) -> Result<(Focused, Option<u16>), Error> {
    let focused = session.focus_frame(frame, focus)?;
    let position = session.focus_position().ok();
    if let Some(position) = position {
        info!(position, "focused at");
    }
    Ok((focused, position))
}

/// Run `f` with `frame` reachable, and leave the unit's frame table as measured
fn at_frame<T>(
    session: &mut Session,
    frame: Rect,
    f: impl FnOnce(&mut Session) -> Result<T, Error>,
) -> Result<T, Error> {
    // Where the unit positions the film by its own frame table, the window's Y
    // selects a table entry and is an offset into that frame, not an address.
    // The rectangle must be in the table before the window can reach it
    let registered = framing::register(session, frame)?;

    let taken = f(session);

    // Put the measured table back, whether or not `f` got that far, so the
    // next rectangle of this session starts from it. Written and not
    // registered: `register` starts from the measured table, finds the entry it
    // wants already there, and leaves this pass's table in the unit
    if registered
        && let Some(measured) = session.frames_type2().cloned()
        && let Err(e) = session.set_boundaries_type2_for_pass(&measured)
    {
        warn!(
            %e,
            "could not put the measured frame table back, so a later rectangle can position against this one"
        );
    }

    taken
}
