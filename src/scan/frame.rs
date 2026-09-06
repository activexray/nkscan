//! Scanning one frame: focus, meter, take the pass, and optionally clean it

use crate::{
    error::Error,
    protocol::{
        data::Rect,
        decode::{Image, Samples},
    },
    scan::{
        autoexpose::Exposures,
        boundaries::{self, Polarity},
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

/// What to do beyond where the frame is
#[derive(Debug, Clone, Copy, Default)]
pub struct Options<'a> {
    /// Reuse an exposure already decided; `None` meters this frame fresh
    pub exposures: Option<&'a Exposures>,
    /// Honored only where `exposures` is `None`
    pub lock_white_balance: bool,
    /// Run dust removal over the result in place
    pub clean: bool,
    /// Which way a picture reads against the film around it
    ///
    /// Needed where the unit positions the film by its perforation table, to
    /// find the frame in the pass and keep the film in front of it out of the
    /// metering. Ignored everywhere else
    pub polarity: Option<Polarity>,
}

/// What one frame's scan produced
pub struct Scanned {
    pub pass: Pass,
    /// Which lines of `pass` are the frame that was asked for
    ///
    /// A pass is longer than the frame where the unit positions the film
    /// itself. Where the frame's own edges are in the pass this is them
    /// exactly, however far off the detected frame was; a rectangle with no
    /// edges of its own is placed by where the frames that have them started
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
    // The pass is longer than the frame where the unit positions the film
    let over = framing::pass_rect(session.capabilities(), frame, session.gate_offset());

    framing::register(session, frame)?;
    session.focus_frame(over, Focus::default())?;

    let exposures = match options.exposures {
        Some(locked) => locked.clone(),
        None => {
            let lock = options.lock_white_balance;
            session.autoexpose_frame_with(over, recipe, lock, options.polarity, |pass, p| {
                on(Phase::Meter(pass), p)
            })?
        }
    };

    // Metering measured where a positioned pass starts its picture, so a
    // shorter rectangle can size its pass to the measurement now
    let over = framing::pass_rect(session.capabilities(), frame, session.gate_offset());
    let mut windows = recipe.windows(session.capabilities(), over)?;
    exposures.apply(&mut windows);

    let pass = session.scan_pass_with(&windows, SCAN_TIMEOUT, samples, |p| on(Phase::Scan, p))?;

    // Find the frame before to_full_scale: the finder reads each sample as a
    // fraction of the full scale the layout's bits describe, which the
    // scaling would no longer match
    let frame_lines = lines_of(session, &pass, samples, frame, options.polarity);
    samples.to_full_scale(pass.layout.bits_per_sample);

    let cleaned = options
        .clean
        .then(|| clean_frame(samples, &pass, session.capabilities().identity.model()))
        .transpose()?;

    Ok(Scanned {
        pass,
        frame_lines,
        exposures,
        cleaned,
    })
}

/// Which lines of the pass are the frame
///
/// Where the unit positions the film, the frame's own edges in the pass are
/// the exact answer, and where there are none - a frame never exposed, a
/// rectangle from inside a frame - the frame is where every positioned pass
/// has so far started its picture. Where neither is known the pass is the
/// frame, said out loud rather than cropped to a guess
fn lines_of(
    session: &mut Session,
    pass: &Pass,
    samples: &Samples,
    frame: Rect,
    polarity: Option<Polarity>,
) -> Range<usize> {
    let extent = frame.bottom.saturating_sub(frame.top);
    let pitch = pass.layout.line_pitch.max(1);
    let expected = (extent / pitch) as usize;

    if framing::Framing::choose(session.capabilities()) == framing::Framing::Perforation {
        if let Some(found) = polarity
            .zip(Image::new(&pass.layout, samples).ok())
            .and_then(|(pol, image)| boundaries::locate(&image, expected, pol))
        {
            session.note_gate_offset((found.start as u32) * pitch);
            debug!(?found, "the frame in the pass");
            return found.start..found.end.min(pass.cols);
        }
        if let Some(offset) = session.gate_offset() {
            let at = (offset / pitch) as usize;
            return at..(at + expected).min(pass.cols);
        }
        warn!("the frame's edges did not show in the pass, so the pass is the frame");
        return 0..pass.cols;
    }

    // The window addressed the film itself, so the pass is the frame
    0..expected.min(pass.cols)
}
