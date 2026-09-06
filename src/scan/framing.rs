//! Where the frames are, and how this unit expects us to find out
//!
//! Four mechanisms, picked from what the unit and the loaded adapter advertise.

use super::{
    boundaries::Polarity,
    pass::{Pass, Progress},
    thumbnail,
};
use crate::{
    error::Error,
    protocol::{
        caps::{Capabilities, address::CoordinateBase, film::FilmFormat, other::DataTypes},
        data::{Boundary, FramePosition, FrameTable, Op, PerforationInformation, Rect},
        decode::Samples,
    },
    session::Session,
};
use std::ops::ControlFlow;
use tracing::*;

/// How a scan comes to know where each frame sits
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// `Frames` already carries every frame. A masked adapter knows its own geometry
    Published,
    /// `Frames` carries rectangles with no length.
    /// 2-11-6: after a thumbnail of strip film the host works the boundaries out and sends them as `DataType::Boundary`.
    /// `thumbnail::frames` is what works them out
    Thumbnail,
    /// No rectangles at all. 135 film seeks by counting perforations: read `DataType::Perforation` and write `DataType::Boundary2` back
    Perforation,
    /// No mechanism is offered, so the address page's own geometry is what says
    /// where the frames are
    Address,
}

impl Framing {
    /// Pick the mechanism this unit and adapter use
    pub fn choose(caps: &Capabilities) -> Self {
        let rects = caps
            .address
            .coordinate_base
            .contains(CoordinateBase::FRAME_RECTS);

        if rects {
            // Lengths present means there is nothing left to measure
            if caps.frames.as_ref().is_some_and(|f| f.measured()) {
                return Self::Published;
            }
            if thumbnail::available(caps) {
                return Self::Thumbnail;
            }
            return Self::Address;
        }

        // Perforations are the other way a frame gets found, and only the families that read DataType::Perforation can do it
        if caps
            .features
            .data_types
            .contains(DataTypes::PERFORATION_READ)
        {
            return Self::Perforation;
        }
        Self::Address
    }
}

/// Refuse a frame the stage cannot step to
///
/// Past `Address`'s Y boundary the stage target comes out behind the home stop,
/// and the mechanism grinds there until a power cycle. A length of zero is not a
/// frame and would tile nothing
pub(crate) fn reachable(caps: &Capabilities, extent: u32) -> Result<(), Error> {
    if extent == 0 {
        return Err(Error::Unsupported {
            op: "frame table",
            reason: "a frame of length 0 is not a frame".into(),
        });
    }
    let limit = caps.address.y_axis.boundary;
    match extent > limit {
        true => Err(Error::Unsupported {
            op: "frame table",
            reason: format!(
                "a frame of {extent} is past the {limit} boundary and would stall the stage"
            ),
        }),
        false => Ok(()),
    }
}

/// The frame table a masked holder publishes, 2-11-6
///
/// Both the stage and autofocus resolve against the `88h` table, and until the
/// host writes one the unit answers with a single whole-sensor rect that makes
/// every frame-kind SET WINDOW drive the holder out and back. A masked holder
/// publishes its frames with lengths in C8h, and this emits one rect per image
/// so the host can write them back as the `88h` table.
///
/// Strip holders publish no lengths, and this returns empty: the host has not
/// measured yet, and the table they need comes from `thumbnail::frames` after a
/// whole-strip pass. A rect longer than the Y boundary stalls the stage, so a
/// placeholder is not something we can invent here
pub fn table(caps: &Capabilities) -> Result<Boundary, Error> {
    let Some(published) = caps.frames.as_ref() else {
        return Ok(Boundary::default());
    };
    let mut frames = Vec::new();

    for image in &published.images {
        // A published length is the adapter's own geometry. Without one, the
        // host owes a measurement and the table comes from `thumbnail::frames`
        let Some(extent) = image.length else {
            continue;
        };
        // A scan window has to be whole readout blocks and has to sit inside
        // the frame this table gives, so the table carries the rounding
        let extent = super::window::whole_blocks(caps, extent);
        reachable(caps, extent)?;
        frames.push(Rect {
            top: image.top,
            left: image.left,
            bottom: image.top + extent,
            right: image.left + image.width,
        });
    }
    Ok(Boundary { frames })
}

/// How many frames the addressable Y range holds, at one boundary each
///
/// More than one only where the window address is a position on the medium
/// rather than the gate, 2-2-2-2 byte 17 bit 0
pub fn frames_on_medium(caps: &Capabilities) -> u32 {
    let y = &caps.address.y_axis;
    let span = y.address_range.last.saturating_sub(y.address_range.start) + 1;
    match y.boundary {
        0 => 1,
        boundary => (span / boundary).max(1),
    }
}

/// Whether the unit fetches the next medium itself
///
/// A supply of single-frame media is a stack it works through. Anything longer
/// is one medium and the operator swaps it
pub fn self_feeding(caps: &Capabilities) -> bool {
    caps.features.execute.supports(Op::Load) && frames_on_medium(caps) == 1
}

/// The frames a [`Framing::Address`] unit never describes on its own: one rect
/// per frame the Y range holds, the whole opening wide
pub fn frames(caps: &Capabilities) -> Result<Boundary, Error> {
    let (x, y) = (&caps.address.x_axis, &caps.address.y_axis);
    let extent = super::window::reachable_blocks(caps, y.boundary);
    reachable(caps, extent)?;

    let count = frames_on_medium(caps);
    debug!(
        count,
        pitch = y.boundary,
        extent,
        "framed from the address page"
    );
    let frames = (0..count)
        .map(|n| {
            let top = y.address_range.start + n * y.boundary;
            Rect {
                top,
                left: x.address_range.start,
                bottom: top + extent,
                right: x.address_range.start + x.boundary,
            }
        })
        .collect();
    Ok(Boundary { frames })
}

/// The rectangle for a pass that includes all of `frame`
///
/// Where a unit positions the film by its perforation table, the window does
/// not say where the frame appears: the film is latched by the record at the
/// frame's own line, and the frame appears part-way into the pass with film
/// in front of it, so a pass the length of the frame ends that much film
/// short of the frame's end. Nikon Scan asks for the whole scannable range
/// for every frame, and a rectangle more than half the range is a whole
/// frame, since one frame is all the range takes.
///
/// A shorter rectangle - a half frame, a crop - adds the measured offset
/// instead, or the whole slack of the range until the offset has been
/// measured. Everywhere else the window addresses the film itself and the
/// frame is already where the window says
pub fn pass_rect(caps: &Capabilities, frame: Rect, offset: Option<u32>) -> Rect {
    if Framing::choose(caps) != Framing::Perforation {
        return frame;
    }
    let boundary = caps.address.y_axis.boundary;
    let extent = frame.bottom.saturating_sub(frame.top);
    let length = match extent > boundary / 2 {
        true => boundary,
        false => extent + offset.unwrap_or(boundary - extent),
    };
    Rect {
        bottom: frame.top + length.min(boundary),
        ..frame
    }
}

/// Register `frame` with the unit before a pass over it, where the unit
/// positions the film by its table rather than by the window
///
/// The unit reads a rectangle that is not in the table as an offset into the
/// frame that contains its top, and gives black data after the end of that
/// frame. An entry with the perforation record of the rectangle's own thumbnail
/// line moves the film to the rectangle instead. The unit accepts a table only
/// as a whole, so this starts from the table the session knows, and reads the
/// unit's table if the session knows none. A top already in the table is left
/// alone.
///
/// The session keeps the measured table, not this one. The next rectangle must
/// start from the measured table, because an entry is replaced and not added
pub fn register(session: &mut Session, frame: Rect) -> Result<(), Error> {
    if Framing::choose(session.capabilities()) != Framing::Perforation {
        return Ok(());
    }
    let mut table = match session.frames_type2() {
        Some(table) => table.clone(),
        // Nothing measured this strip, so ask the unit what it is holding
        None => match session.boundaries_type2() {
            Ok(table) => table,
            Err(e) => {
                debug!(%e, "no frame table to register against");
                return Ok(());
            }
        },
    };
    if table.frames.is_empty() || table.frames.iter().any(|f| f.top == frame.top) {
        return Ok(());
    }

    let (line, perf) = record(session, frame.top)?;
    table.register(FramePosition::new(frame.top, &perf));
    debug!(
        top = frame.top,
        line,
        ?perf,
        "registered a frame off the table"
    );
    session.set_boundaries_type2_for_pass(&table)
}

/// The perforation record that puts the top of a frame at `top`
///
/// The record read at the frame's own line. Do not calculate one: the pattern
/// phases have different lengths, 24, 28, 12, 28 and 14 pulses in one
/// perforation of a measured strip, so arithmetic on a record uses a length
/// that only the pass measures. Nikon Scan also reads the record from the
/// table
fn record(session: &mut Session, top: u32) -> Result<(usize, PerforationInformation), Error> {
    let line = thumbnail::line_at(session.capabilities(), top);
    let perfs = session.read_perforations()?;
    let perf = perfs.at(line).ok_or_else(|| Error::Unsupported {
        op: "frame registration",
        reason: format!(
            "a frame at {top} needs the perforation reading of thumbnail line {line}, and the last pass measured {}",
            perfs.perfs.len()
        ),
    })?;
    Ok((line, perf.clone()))
}

/// The discovered frame table, however that happened.
/// If this required a thumbnail pass, this holds that thumbnail.
pub struct Discovery {
    pub table: FrameTable,
    pub frames: Vec<Rect>,
    pub thumbnail: Option<Pass>,
}

/// Find every frame on whatever is loaded, driving whatever pass the chosen mechanism needs
pub fn discover(
    session: &mut Session,
    format: Option<FilmFormat>,
    polarity: Polarity,
    samples: &mut Samples,
) -> Result<Discovery, Error> {
    discover_with(session, format, polarity, samples, |_| {
        ControlFlow::Continue(())
    })
}

/// The same as [`discover`], letting `on` cancel a thumbnail pass by returning `Break`
///
/// `format` is only needed by [`Framing::Thumbnail`] and [`Framing::Perforation`], and even
/// there only where [`FilmFormat::resolve`] cannot work it out from the loaded holder.
/// `samples` is scratch, left holding the thumbnail where [`Discovery::thumbnail`] is `Some`
pub fn discover_with(
    session: &mut Session,
    format: Option<FilmFormat>,
    polarity: Polarity,
    samples: &mut Samples,
    on: impl FnMut(Progress) -> ControlFlow<()>,
) -> Result<Discovery, Error> {
    // Only the two mechanisms below need it, but both need it before they move
    let need_format = |session: &Session| FilmFormat::resolve(format, session.capabilities());

    let mechanism = Framing::choose(session.capabilities());
    debug!(?mechanism, "frame discovery");

    match mechanism {
        Framing::Published => {
            let boundary = table(session.capabilities())?;
            let found = boundary.frames.clone();
            info!(frames = found.len(), "published frames");
            Ok(Discovery {
                table: FrameTable::Boundary(boundary),
                frames: found,
                thumbnail: None,
            })
        }
        Framing::Thumbnail => {
            let format = need_format(session)?;
            let pass = session.scan_thumbnail_with(samples, on)?;
            debug!(
                rows = pass.rows,
                cols = pass.cols,
                complete = pass.complete,
                "thumbnail"
            );
            let optical_dpi = session.capabilities().address.y_axis.optical_dpi;
            let length = format.height_dots(optical_dpi);
            info!(?format, length, "frame length");

            let measured =
                thumbnail::frames(session.capabilities(), &pass, samples, length, polarity)?;
            // Nothing measured is nothing to tell the unit, and it refuses an
            // empty record. The caller gets the pass either way, which is the
            // only evidence of why the strip measured empty
            if !measured.frames.is_empty() {
                session.set_boundaries(&measured)?;
            }
            let found = measured.frames.clone();
            info!(frames = found.len(), "detected frames");
            Ok(Discovery {
                table: FrameTable::Boundary(measured),
                frames: found,
                thumbnail: Some(pass),
            })
        }
        Framing::Address => {
            let boundary = frames(session.capabilities())?;
            let found = boundary.frames.clone();
            Ok(Discovery {
                table: FrameTable::Boundary(boundary),
                frames: found,
                thumbnail: None,
            })
        }
        Framing::Perforation => {
            let format = need_format(session)?;
            // Discard whatever a previous strip left behind
            let _ = session.read_perforations()?;
            let _ = session.read_boundaries_type2();

            let pass = session.scan_thumbnail_with(samples, on)?;
            debug!(
                rows = pass.rows,
                cols = pass.cols,
                complete = pass.complete,
                "thumbnail"
            );
            let optical_dpi = session.capabilities().address.y_axis.optical_dpi;
            let length = format.height_dots(optical_dpi);
            info!(?format, length, "frame length");

            let perfs = session.read_perforations()?;
            let (measured, length) = thumbnail::frames_type2(
                session.capabilities(),
                &pass,
                samples,
                &perfs,
                length,
                polarity,
            )?;
            if !measured.frames.is_empty() {
                session.set_boundaries_type2(&measured)?;
            }

            let x_start = session.capabilities().address.x_axis.address_range.start;
            let x_boundary = session.capabilities().address.x_axis.boundary;
            let found = measured
                .frames
                .iter()
                .map(|f| f.rect(x_start, x_boundary, length))
                .collect::<Vec<_>>();
            info!(frames = found.len(), "detected frames");
            Ok(Discovery {
                table: FrameTable::BoundaryType2(measured),
                frames: found,
                thumbnail: Some(pass),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{protocol::caps::other::DataTypes, scan::window::tests::caps};

    /// A pass over a frame the unit positions is the whole range, which is
    /// what Nikon Scan asks for. The other mechanisms address the film with
    /// the window and the frame is its own pass
    #[test]
    fn a_perforation_framed_pass_is_the_whole_range() {
        let frame = Rect {
            top: 2236,
            left: 518,
            bottom: 2236 + 11964,
            right: 518 + 8964,
        };
        let plain = caps();
        assert_eq!(pass_rect(&plain, frame, Some(166)), frame);

        let mut perforated = caps();
        perforated.features.data_types |= DataTypes::PERFORATION_READ;
        assert_eq!(Framing::choose(&perforated), Framing::Perforation);
        let boundary = perforated.address.y_axis.boundary;
        assert!(
            frame.bottom - frame.top > boundary / 2,
            "the fixture has to make this a whole frame"
        );

        let over = pass_rect(&perforated, frame, Some(166));
        assert_eq!(over.top, frame.top);
        assert_eq!(over.bottom - over.top, boundary);
    }

    /// A rectangle shorter than half the range - a half frame, a crop - adds
    /// the measured offset, and before anything is measured the whole slack of
    /// the range, which is the most the offset can be
    #[test]
    fn a_shorter_rect_adds_the_offset() {
        let mut perforated = caps();
        perforated.features.data_types |= DataTypes::PERFORATION_READ;
        let boundary = perforated.address.y_axis.boundary;
        let half = boundary / 4;

        let frame = Rect {
            top: 2236,
            left: 518,
            bottom: 2236 + half,
            right: 518 + 8964,
        };
        let measured = pass_rect(&perforated, frame, Some(166));
        assert_eq!(measured.bottom - measured.top, half + 166);

        let unmeasured = pass_rect(&perforated, frame, None);
        assert_eq!(unmeasured.bottom - unmeasured.top, boundary);
    }

    /// A cartridge addresses the film itself, so its range runs the length of
    /// the roll at one frame per boundary. These are an IA-20's
    #[test]
    fn a_medium_longer_than_the_gate_is_framed_along_its_range() {
        let mut caps = caps();
        assert_eq!(frames(&caps).expect("frames").frames.len(), 1);

        caps.address.y_axis.address_range = (0..=111324).into();
        caps.address.y_axis.boundary = 4453;
        caps.address.line_gap = 0;

        let found = frames(&caps).expect("frames").frames;
        assert_eq!(found.len(), 25);
        assert_eq!(found[0].bottom - found[0].top, 4453);
        assert_eq!(found[24].top, 24 * 4453);
    }
}
