//! Scanning all the available film at once to generate a thumbnail
//!
//! `Address` byte 16 says whether the unit publishes frames at all, and
//! `Frames` says whether it knows where they end. A fixed-format mount does;
//! loose film reports a length of zero until something measures it.
//!
//! `Features` puts thumbnail in the host cooperation bits on both families, so
//! the unit hands us the pass and expects us to make sense of it.

use super::{
    boundaries::{self, Polarity},
    framing,
    pass::Pass,
    window,
};
use crate::{
    error::Error,
    protocol::{
        caps::{
            Capabilities,
            set_window::{ColorInterleaving, ScanKind, ScanMode},
        },
        data::{Boundary, BoundaryType2, FramePosition, PerfInformation, Rect},
        decode::{Image, Samples},
        model::Model,
        window::{Flags, Window},
    },
};
use tracing::*;

/// The most film to leave either side of a frame, as the format over this
const MARGIN: u32 = 50;

/// Whether this unit and adapter will thumbnail at all
///
/// Support follows the adapter rather than the model, so this is re-decided
/// whenever the adapter changes
pub fn available(caps: &Capabilities) -> bool {
    caps.set_window.kind.contains(ScanKind::THUMBNAIL)
        && caps.address.thumbnail_resolution.start > 0
}

/// The frame table a thumbnail measures, 2-11-6
///
/// `length` is the frame's extent along the feed, the film format, which
/// nothing advertises. Every rectangle comes out that long: the captures'
/// measured tables move the tops about and leave the heights at the format.
///
/// `polarity` is which way the loaded film reads, which the film type says.
pub fn frames(
    caps: &Capabilities,
    pass: &Pass,
    samples: &Samples,
    length: u32,
    polarity: Polarity,
) -> Result<Boundary, Error> {
    let format = window::reachable_blocks(caps, length);
    framing::reachable(caps, format)?;
    let image = Image::new(&pass.layout, samples)?;

    // A thumbnail column is one line pitch of film, and the pass starts where
    // the Y axis does, so a column is an address
    let pitch = pass.layout.line_pitch.max(1);
    let origin = caps.address.y_axis.address_range.start;
    let end = caps.address.y_axis.address_range.last;
    let (left, width) = opening(caps);

    let found = boundaries::detect(&image, (format / pitch) as usize, polarity);

    // The strip's own gate, where enough of it was found to say - a real one
    // is not always the nominal format
    let format = window::reachable_blocks(caps, found.length as u32 * pitch);
    framing::reachable(caps, format)?;

    // A frame cut to the format exactly would sometimes clip the picture, since
    // a column is a third of a millimeter. A little film either side takes that
    // slack off the gap instead, and leaves an edge to see the frame against
    let bleed = margin(format, found.pitch as u32 * pitch);
    let length = window::reachable_blocks(caps, format + 2 * window::whole_blocks(caps, bleed));
    framing::reachable(caps, length)?;
    // Whatever the axis would not take comes off the margin, not the format
    let margin = (length - format) / 2;

    let frames: Vec<Rect> = found
        .frames
        .iter()
        // Off the top as well, so the frame stays centered on what was found
        .map(|&col| {
            (origin + col as u32 * pitch)
                .saturating_sub(margin)
                .max(origin)
        })
        .filter(|top| top + length <= end)
        .map(|top| Rect {
            top,
            left,
            bottom: top + length,
            right: left + width,
        })
        .collect();

    info!(
        frames = frames.len(),
        ?polarity,
        pitch = found.pitch as u32 * pitch,
        "measured the loaded strip"
    );
    for (n, rect) in frames.iter().enumerate() {
        debug!(frame = n + 1, ?rect, "frame rect");
    }
    Ok(Boundary { frames })
}

pub fn frames_type2(
    caps: &Capabilities,
    pass: &Pass,
    samples: &Samples,
    perf_info: &PerfInformation,
    length: u32,
    polarity: Polarity,
) -> Result<(BoundaryType2, u32), Error> {
    // Whole readout blocks, and trimmed rather than refused where the format
    // is taller than the axis reaches
    let length = window::reachable_blocks(caps, length);
    framing::reachable(caps, length)?;

    let image = Image::new(&pass.layout, samples)?;

    // A thumbnail column is one line pitch of film, and the pass starts
    // where the Y axis does, so a column is an address.
    let pitch = pass.layout.line_pitch.max(1);
    let origin = caps.address.y_axis.address_range.start;
    let end = caps.address.y_axis.address_range.last;

    let found = boundaries::detect(&image, (length / pitch) as usize, polarity);

    // The strip's own gate, where enough of it was found to say - a real one
    // is not always the nominal format
    let length = window::reachable_blocks(caps, found.length as u32 * pitch);
    framing::reachable(caps, length)?;

    // A detected column indexes the perforation table directly. The table
    // commonly falls short of the pass - the unit stops counting perforations
    // past the last one on the strip, so bare leader or trailer beyond it goes
    // uncovered without that meaning anything is wrong - but a frame whose
    // column falls in that gap has no registration to send the unit for it
    // and drops below, so this is only context for that, not itself the fault
    if perf_info.perfs.len() != image.cols {
        debug!(
            perfs = perf_info.perfs.len(),
            columns = image.cols,
            "the perforation table does not run the length of the thumbnail pass"
        );
    }

    // The frame table the detected boundaries and the perforation data come to
    let frames: Vec<FramePosition> = found
        .frames
        .iter()
        .filter_map(|&col| {
            let top = origin + col as u32 * pitch;
            let perf = perf_info.at(col);
            debug!(col, top, ?perf, "detected column");
            if top + length > end {
                // A column with no reading is one the stage cannot be sent to
                return None;
            }
            match perf {
                Some(perf) => Some(FramePosition::new(top, perf)),
                None => {
                    // Past wherever the perforation table stopped: nothing to
                    // register this frame's stage position against, so unlike
                    // an unaddressable column this one is worth naming
                    warn!(col, top, "no perforation reading for this frame, dropped");
                    None
                }
            }
        })
        .collect();

    info!(
        frames = frames.len(),
        ?polarity,
        pitch = found.pitch as u32 * pitch,
        "measured the loaded strip"
    );
    for (n, frame) in frames.iter().enumerate() {
        debug!(frame = n + 1, ?frame, "frame position");
    }

    Ok((BoundaryType2 { frames }, length))
}

/// Film to leave either side of a frame, in whatever units the format is given
/// in
///
/// `wound` is how far the transport advanced between frames, 0 where nothing
/// measured it. Never over half the wind, or the margin carries the frame next
/// door; frames that overlap get none.
fn margin(format: u32, wound: u32) -> u32 {
    let most = format / MARGIN;
    match wound {
        // One frame on its own says nothing about the wind, and has the film to
        // itself either way
        0 => most,
        wound => most.min(wound.saturating_sub(format) / 2),
    }
}

/// Where the adapter's opening sits on the sensor, and how wide it is
///
/// The first published image is the opening: a frame narrower than that is a
/// crop, and cropping is not what a pass over the whole strip is for
fn opening(caps: &Capabilities) -> (u32, u32) {
    let x = &caps.address.x_axis;
    match caps.frames.as_ref().and_then(|f| f.images.first()) {
        Some(opening) => (opening.left, opening.width),
        None => (x.address_range.start, x.boundary),
    }
}

/// Windows over everything the adapter can reach, one per channel
pub(crate) fn windows(caps: &Capabilities) -> Result<Vec<Window>, Error> {
    let y = &caps.address.y_axis;
    let unsupported = |reason: String| Error::Unsupported {
        op: "thumbnail window",
        reason,
    };

    // Line ordering owes the host nothing, where the three-line mode owes it
    // registration. Take it when offered rather than assuming it is
    let offered = caps.set_window.interleaving;
    if !offered.contains(ColorInterleaving::LINE_WITHOUT_DISTANCE) {
        return Err(unsupported(format!(
            "a thumbnail needs line ordering and this unit offers {offered:?}"
        )));
    }

    let flags = match caps.identity.model() {
        Some(Model::Ls8000 | Model::Ls9000) => Flags::empty(),
        Some(_) => Flags::POSITIVE | Flags::AVERAGING,
        None => {
            return Err(Error::Unsupported {
                op: "thumbnail window",
                reason: "unrecognized model".into(),
            });
        }
    };

    let (left, width) = opening(caps);
    let mut windows = window::blank(caps, &window::color_channels(caps))?;
    for w in &mut windows {
        w.resolution = (
            caps.address.thumbnail_resolution.start,
            caps.address.thumbnail_resolution.start,
        );
        // Y starts at the axis rather than the first frame, so the leading
        // edge of the film is in the pass and can be found
        w.origin = (left, y.address_range.start);
        w.size = (width, y.address_range.last);
        w.scanning_kind = ScanKind::THUMBNAIL;
        w.scanning_mode = ScanMode::NORMAL_QUALITY;
        w.flags = flags;
        w.color_interleaving = ColorInterleaving::LINE_WITHOUT_DISTANCE;
    }
    Ok(windows)
}
