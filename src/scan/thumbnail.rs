//! Scanning all the available film at once to generate a thumbnail
//!
//! `Address` byte 16 says whether the unit publishes frames at all, and
//! `Frames` says whether it knows where they end. A fixed-format mount does;
//! loose film reports a length of zero until something measures it.
//!
//! `Features` puts thumbnail in the host cooperation bits on both families, so
//! the unit hands us the pass and expects us to make sense of it.

use super::{framing, pass::Pass, strip, window};
use crate::{
    error::Error,
    protocol::{
        caps::{
            Capabilities,
            set_window::{ColorInterleaving, ScanKind, ScanMode},
        },
        data::{
            Boundary, BoundaryType2, FramePosition, PerfInformation, PerforationInformation, Rect,
        },
        decode::{Image, Samples},
        model::Model,
        window::{Flags, Window},
    },
};
use tracing::*;

/// The pitch of a 135 perforation in ten-thousandths of a millimeter, ISO 1007
const PERFORATION_MM_E4: u64 = 47_498;

/// Ten-thousandths of a millimeter in an inch, to turn that into addresses
const INCH_MM_E4: u64 = 254_000;

/// The film a perforation-framed strip must move to make a measurement worth
/// having: fewer perforations than this and the quarter a record is rounded to
/// is a large part of what is being measured
const MEASURED_QUARTERS: u64 = 16;

/// Stage addresses one thumbnail column spans
///
/// The pass asks for the thumbnail resolution, and a thumbnail pitch is the
/// optical resolution over what was asked, rounded down, so this is the pass's
/// own `line_pitch` without the pass in hand
pub(crate) fn line_pitch(caps: &Capabilities) -> u32 {
    let optical =
        u32::from(caps.address.y_axis.optical_dpi).max(u32::from(caps.address.x_axis.optical_dpi));
    match u32::from(caps.address.thumbnail_resolution.start) {
        0 => 1,
        asked => (optical / asked).max(1),
    }
}

/// How far the film moves between two thumbnail lines
///
/// Kept as the film it measured over the lines it took to move, because the
/// answer is not a whole number of addresses and a frame is over a hundred
/// lines of it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LinePitch {
    addresses: u64,
    lines: u64,
}

impl LinePitch {
    /// What the pass asked for, which is [`line_pitch`]
    pub(crate) fn computed(caps: &Capabilities) -> Self {
        Self {
            addresses: u64::from(line_pitch(caps)),
            lines: 1,
        }
    }

    /// What the film did, measured off the perforation table
    ///
    /// The unit reports a thumbnail resolution the film does not keep to: an
    /// LS-50 reports 97 dpi, so the pass asks for 41 addresses a line and the
    /// film moves about 41.8. Over the 137 lines of a 135 frame that is a
    /// millimeter, which is most of the room a frame has in the gate.
    ///
    /// 2-11-8's table is the ruler that settles it. Every record is one line's
    /// absolute position, counted in perforations and quarters of one, and a
    /// 135 perforation is 4.7498 mm. The count does not run the whole pass:
    /// the unit has nothing to count before the first perforation and stops at
    /// the last, so this measures between the first record that moved and the
    /// last one that did. A table that measures nothing, or a pitch the pass
    /// could not have asked for, gives `None` and the caller keeps the
    /// computed one
    pub(crate) fn measured(caps: &Capabilities, perfs: &PerfInformation) -> Option<Self> {
        let quarters =
            |p: &PerforationInformation| u64::from(p.perf_number) * 4 + u64::from(p.perf_decimal);

        let first = perfs.perfs.first()?;
        let last = perfs.perfs.last()?;
        // Past the flat head, and up to the flat tail
        let head = perfs
            .perfs
            .iter()
            .position(|p| quarters(p) != quarters(first))?;
        let tail = perfs
            .perfs
            .iter()
            .rposition(|p| quarters(p) != quarters(last))?
            + 1;

        let lines = (tail.checked_sub(head)?) as u64;
        let moved =
            quarters(perfs.perfs.get(tail)?).checked_sub(quarters(perfs.perfs.get(head)?))?;
        if lines == 0 || moved < MEASURED_QUARTERS {
            return None;
        }

        let optical = u64::from(
            caps.address
                .y_axis
                .optical_dpi
                .max(caps.address.x_axis.optical_dpi),
        );
        let addresses = moved * optical * PERFORATION_MM_E4 / (4 * INCH_MM_E4);

        let measured = Self { addresses, lines };
        // The pass moved the film once for every line it asked for, so a
        // measurement far from what it asked for is a table this cannot read
        let asked = u64::from(line_pitch(caps));
        let apart = measured.addresses.abs_diff(asked * lines);
        (apart * 4 <= asked * lines).then_some(measured)
    }

    /// The Y address of a thumbnail line
    pub(crate) fn address_of(&self, line: u32) -> u32 {
        (u64::from(line) * self.addresses / self.lines.max(1)) as u32
    }

    /// How many thumbnail lines a length of film spans
    ///
    /// A length rather than a position, so the axis origin does not come into
    /// it the way it does for [`Self::line_at`]
    pub(crate) fn columns(&self, addresses: u32) -> usize {
        let per = self.addresses.max(1);
        ((u64::from(addresses) * self.lines + per / 2) / per) as usize
    }

    /// The thumbnail line nearest a Y address
    pub(crate) fn line_at(&self, caps: &Capabilities, y: u32) -> usize {
        let origin = caps.address.y_axis.address_range.start;
        let film = u64::from(y.saturating_sub(origin)) * self.lines.max(1);
        let addresses = self.addresses.max(1);
        ((film + addresses / 2) / addresses) as usize
    }
}

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

    let Some(found) = strip::find(&image, (format / pitch) as usize) else {
        info!("nothing on the strip to frame");
        return Ok(Boundary::default());
    };

    // The window addresses the film here, so a frame is the format over the
    // middle of the picture. Not the picture: a camera gate is not the
    // rectangle the caller asked to scan
    let frames: Vec<Rect> = found
        .frames
        .iter()
        .map(|frame| origin + (frame.start + frame.len() / 2) as u32 * pitch)
        .map(|middle| middle.saturating_sub(format / 2).max(origin))
        .filter(|top| top + format <= end)
        .map(|top| Rect {
            top,
            left,
            bottom: top + format,
            right: left + width,
        })
        .collect();

    info!(
        frames = frames.len(),
        pitch = found.pitch as u32 * pitch,
        contrast = found.contrast,
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
) -> Result<(BoundaryType2, u32), Error> {
    // Whole readout blocks, and trimmed rather than refused where the format
    // is taller than the axis reaches
    let format = window::reachable_blocks(caps, length);
    framing::reachable(caps, format)?;

    let image = Image::new(&pass.layout, samples)?;

    // A thumbnail column is one line pitch of film, and the pass starts where
    // the Y axis does, so a column is an address. The pitch the pass asked for
    // is not the one the film kept to, and the table this unit just measured
    // says what it was
    let pitch = match LinePitch::measured(caps, perf_info) {
        Some(measured) => {
            debug!(
                per_thousand_lines = measured.address_of(1000),
                "measured the thumbnail line against the perforation table"
            );
            measured
        }
        None => {
            debug!("no perforation table to measure the thumbnail line against");
            LinePitch::computed(caps)
        }
    };
    let origin = caps.address.y_axis.address_range.start;
    let end = caps.address.y_axis.address_range.last;
    let range = caps.address.y_axis.boundary;

    let Some(found) = strip::find(&image, pitch.columns(format)) else {
        info!("nothing on the strip to frame");
        return Ok((BoundaryType2::default(), format));
    };

    // The table commonly falls short of the pass: the unit stops counting
    // perforations past the last one on the strip. That is not a fault on its
    // own, but a frame whose column falls past the end has no record to
    // register it with and drops below
    if perf_info.perfs.len() != image.cols {
        debug!(
            perfs = perf_info.perfs.len(),
            columns = image.cols,
            "the perforation table does not run the length of the thumbnail pass"
        );
    }

    // 2-11-9 moves the film so that an entry's top address is the first line
    // the pass reads. The record is how it gets there, so the two are one
    // reading of one place and move together. The pass is the whole range, so
    // the entry that puts the picture in the middle of it is half a range
    // ahead of the middle of the picture. The format never enters into it
    let frames: Vec<FramePosition> = found
        .frames
        .iter()
        .map(|frame| pitch.address_of((frame.start + frame.len() / 2) as u32))
        .filter_map(|middle| {
            let top = (origin + middle)
                .saturating_sub(range / 2)
                .max(origin)
                .min(end.saturating_sub(range));
            let col = pitch.line_at(caps, top);
            let perf = perf_info.at(col);
            debug!(col, top, ?perf, "the pass over a frame");
            match perf {
                Some(perf) => Some(FramePosition::new(top, perf)),
                None => {
                    // Past wherever the perforation table stopped: nothing to
                    // register this frame's stage position against
                    warn!(col, top, "no perforation reading for this frame, dropped");
                    None
                }
            }
        })
        .collect();

    info!(
        frames = frames.len(),
        pitch = pitch.address_of(found.pitch as u32),
        contrast = found.contrast,
        "measured the loaded strip"
    );
    for (n, frame) in frames.iter().enumerate() {
        debug!(frame = n + 1, ?frame, "frame position");
    }

    Ok((BoundaryType2 { frames }, format))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::window::tests::caps;

    /// A table like the one an LS-50 measures: a flat head with nothing
    /// counted yet, then `perforations` of film at `lines` thumbnail lines
    /// each, then a flat tail past the last perforation
    fn table(head: usize, lines: usize, perforations: usize, tail: usize) -> PerfInformation {
        let mut perfs = vec![PerforationInformation::default(); head];
        for line in 0..lines * perforations {
            let quarters = line * 4 / lines.max(1);
            perfs.push(PerforationInformation {
                perf_number: (quarters / 4) as u16,
                perf_decimal: (quarters % 4) as u8,
                ..Default::default()
            });
        }
        let last = perfs.last().cloned().unwrap_or_default();
        perfs.extend(std::iter::repeat_n(last, tail));
        PerfInformation { perfs }
    }

    /// 4.7498 mm a perforation over a 4000 dpi axis is 748 addresses, so a
    /// quarter every 4.5 lines is 41.6 addresses a line: the film's own pitch,
    /// not the 41 that 4000/97 computes
    #[test]
    fn the_perforation_table_measures_the_line() {
        let mut caps = caps();
        caps.address.thumbnail_resolution = (97u16..=97u16).into();
        assert_eq!(line_pitch(&caps), 41);

        // A perforation every 18 lines, 40 of them
        let measured = LinePitch::measured(&caps, &table(20, 18, 40, 30)).expect("a pitch");

        // 748 addresses of film every 18 lines, so 41.555 a line
        assert_eq!(measured.address_of(1000), 41_555);
        // A 135 frame is about 137 lines, where the computed pitch is a
        // millimeter short over the frame
        assert_eq!(
            measured.address_of(137) - LinePitch::computed(&caps).address_of(137),
            76
        );
        assert_eq!(measured.line_at(&caps, measured.address_of(137)), 137);
    }

    /// Nothing to measure against, or a table that says something the pass
    /// cannot have done, and the computed pitch stands
    #[test]
    fn an_unmeasurable_table_keeps_the_computed_pitch() {
        let mut caps = caps();
        caps.address.thumbnail_resolution = (97u16..=97u16).into();

        assert_eq!(
            LinePitch::measured(&caps, &PerfInformation::default()),
            None
        );
        // Flat: the unit counted nothing
        assert_eq!(LinePitch::measured(&caps, &table(40, 1, 0, 0)), None);
        // Three perforations is not enough film to divide by
        assert_eq!(LinePitch::measured(&caps, &table(10, 18, 3, 10)), None);
        // A perforation every other line is ten times what the pass asked for
        assert_eq!(LinePitch::measured(&caps, &table(10, 2, 40, 10)), None);
    }
}
