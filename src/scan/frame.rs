//! Scanning one frame: focus, meter, take the pass, and optionally clean it

use crate::{
    error::Error,
    protocol::{
        data::Rect,
        decode::{Image, Samples},
    },
    scan::{
        autoexpose::Exposures,
        boundaries::{self, Picture, Polarity},
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

    // A table written for this pass has to come back off the unit afterwards,
    // whether it was written for the rectangle as asked for or for the place
    // the metering pass then measured
    let mut registered = framing::register(session, frame)?;
    session.focus_frame(over, Focus::default())?;

    let exposures = match options.exposures {
        Some(locked) => locked.clone(),
        None => {
            let lock = options.lock_white_balance;
            let picture = options.polarity.map(|polarity| Picture {
                polarity,
                extent: frame.bottom.saturating_sub(frame.top),
            });
            session.autoexpose_frame_with(over, recipe, lock, picture, |pass, p| {
                on(Phase::Meter(pass), p)
            })?
        }
    };

    // Metering measured where the film starts its picture, which says whether
    // the range holds all of this frame
    let original = frame;
    let mut frame = frame;
    // Taken whether or not this rectangle is a whole frame: a reading that
    // belongs to one pass must not survive into the next
    let measured = session.take_picture_start();
    let corrected = framing::recentered(session.capabilities(), frame, measured);
    if let Some(rect) = corrected {
        // A correction is an improvement on a rectangle that already scans, so
        // one the unit will not take leaves the frame where it was rather than
        // failing the pass. The commonest reason is a top past wherever the
        // perforation table stopped counting, at the end of a strip
        match framing::register(session, rect) {
            Ok(wrote) => {
                registered |= wrote;
                debug!(
                    from = original.top,
                    to = rect.top,
                    "registered the frame where the metering pass measured it"
                );
                frame = rect;
            }
            Err(e) => warn!(
                %e,
                to = rect.top,
                "could not register the measured place, so the frame is scanned where it was"
            ),
        }
    }

    let over = framing::pass_rect(session.capabilities(), frame, session.gate_offset());
    let mut windows = recipe.windows(session.capabilities(), over)?;
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
            .map(|(pol, image)| boundaries::locate(&image, pol))
            .filter(|found| !found.is_empty())
        {
            // Only where the pass opened on film. A pass that opened inside
            // the picture starts its picture at column 0 whatever the offset
            // really was, and that would size every later pass short
            if found.start > 0 {
                session.note_gate_offset((found.start as u32) * pitch);
            }
            debug!(?found, "the picture in the pass");
            return found.start..found.end.min(pass.cols);
        }
        if let Some(offset) = session.gate_offset() {
            // The furthest start seen, which is what sizes a pass. It is the
            // wrong end of the readings to crop at, but the pass is handed
            // back whole and this only says where the frame is in it, so an
            // overestimate misreports the frame rather than cutting it
            let at = (offset / pitch) as usize;
            return at..(at + expected).min(pass.cols);
        }
        warn!("the frame's edges did not show in the pass, so the pass is the frame");
        return 0..pass.cols;
    }

    // The window addressed the film itself, so the pass is the frame
    0..expected.min(pass.cols)
}
