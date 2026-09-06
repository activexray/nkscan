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
use std::{
    ops::{ControlFlow, Range},
    time::Duration,
};

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

/// What to do beyond where the frame is
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
    pub pass: Pass,
    /// Which lines of `pass` are the frame that was asked for
    ///
    /// A pass is longer than the frame if the unit positions the film itself.
    /// This gives the position of the frame in the pass, from
    /// [`framing::pass_rect`]. It uses the fixed offset of the unit and not a
    /// measurement of this film. To get the frame exactly, find its edges in
    /// the pass
    pub frame_lines: Range<usize>,
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
    // The pass is longer than the frame. Focus and metering use the frame
    let over = framing::pass_rect(session.capabilities(), frame);
    let picture = framing::picture_rect(session.capabilities(), frame);
    let mut windows = recipe.windows(session.capabilities(), over)?;

    framing::register(session, frame)?;
    session.focus_frame(picture, Focus::default())?;

    let exposures = match options.exposures {
        Some(locked) => locked.clone(),
        None => {
            let lock = options.lock_white_balance;
            session
                .autoexpose_frame_with(picture, recipe, lock, |pass, p| on(Phase::Meter(pass), p))?
        }
    };
    exposures.apply(&mut windows);

    let pass = session.scan_pass_with(&windows, SCAN_TIMEOUT, samples, |p| on(Phase::Scan, p))?;
    samples.to_full_scale(pass.layout.bits_per_sample);

    let cleaned = options
        .clean
        .then(|| clean_frame(samples, &pass, session.capabilities().identity.model()))
        .transpose()?;

    let frame_lines = {
        let pitch = pass.layout.line_pitch.max(1);
        let at = ((picture.top - over.top) / pitch) as usize;
        let lines = (frame.bottom.saturating_sub(frame.top) / pitch) as usize;
        at..(at + lines).min(pass.cols)
    };

    Ok(Scanned {
        pass,
        frame_lines,
        exposures,
        cleaned,
    })
}
