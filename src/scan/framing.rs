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
        data::{Boundary, BoundaryType2, FramePosition, FrameTable, Op, PerfInformation, Rect},
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

/// The discovered frame table, however that happened.
/// If this required a thumbnail pass, this holds that thumbnail.
pub struct Discovery {
    pub table: FrameTable,
    pub frames: Vec<Rect>,
    pub thumbnail: Option<Pass>,
    /// Present only where `table` came from a thumbnail pass, so there is a
    /// picture and a geometry to review it against. `None` for `Published`/
    /// `Address`, which measure nothing themselves
    pub review: Option<ReviewContext>,
}

/// What a caller needs to turn an operator-picked column back into a frame,
/// the same way [`discover_with`] turned a detected one into one
pub struct ReviewContext {
    pub geometry: thumbnail::Geometry,
    /// The column each of `Discovery::frames` was actually found at, same
    /// order and length
    pub columns: Vec<usize>,
    /// `Some` only for [`Framing::Perforation`], whose columns need a real
    /// reading to be addressable at all
    pub perfs: Option<PerfInformation>,
}

/// Rebuild the boundary table from operator-picked frames and push it to the
/// device, so autofocus and the stage resolve future frames against what the
/// operator chose rather than what detection guessed
///
/// `frames` is `(column, width)` pairs, in thumbnail columns, replacing
/// `discovery.review`'s own one for one - a caller that wants some left alone
/// passes their already-detected column and `review.geometry.width()` back
/// unchanged. Width is a per-frame choice only on the `Boundary` mechanism;
/// `BoundaryType2`'s own frames carry no length of their own to vary, so
/// there every frame scans at the strip's one shared width regardless of what
/// its pair asks for, and a width is only ever used there to say whether a
/// column is still on the axis. A pair with nothing to build a frame from is
/// dropped rather than refused outright, the same as a detected one would be
pub fn commit_frames(
    session: &mut Session,
    discovery: &Discovery,
    frames: &[(usize, u32)],
) -> Result<Vec<Rect>, Error> {
    let Some(review) = &discovery.review else {
        return Err(Error::Unsupported {
            op: "commit frames",
            reason: "this unit's framing mechanism was never reviewed against a thumbnail".into(),
        });
    };
    match &discovery.table {
        FrameTable::Boundary(_) => {
            let rects: Vec<Rect> = frames
                .iter()
                .filter_map(|&(c, w)| review.geometry.rect(c, w))
                .collect();
            session.set_boundaries(&Boundary { frames: rects.clone() })?;
            Ok(rects)
        }
        FrameTable::BoundaryType2(_) => {
            let perfs = review
                .perfs
                .as_ref()
                .expect("a Perforation review always carries its perforation table");
            // The table itself carries no length - it is only ever the stage
            // registration a `FramePosition` seeks against - so each frame's
            // own width still reaches the rect it scans at, kept alongside
            // the position rather than dropped once the table is built
            let positions: Vec<(FramePosition, u32)> = frames
                .iter()
                .filter_map(|&(c, w)| review.geometry.frame_position(c, w, perfs).map(|f| (f, w)))
                .collect();
            let table = BoundaryType2 {
                frames: positions.iter().map(|&(f, _)| f).collect(),
            };
            session.set_boundaries_type2(&table)?;

            let x_start = session.capabilities().address.x_axis.address_range.start;
            let x_boundary = session.capabilities().address.x_axis.boundary;
            Ok(positions
                .into_iter()
                .map(|(f, w)| f.rect(x_start, x_boundary, review.geometry.dots(w)))
                .collect())
        }
    }
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
                review: None,
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

            let (measured, geometry, columns) =
                thumbnail::frames(session.capabilities(), &pass, samples, length, polarity)?;
            session.set_boundaries(&measured)?;
            let found = measured.frames.clone();
            info!(frames = found.len(), "detected frames");
            Ok(Discovery {
                table: FrameTable::Boundary(measured),
                frames: found,
                thumbnail: Some(pass),
                review: Some(ReviewContext {
                    geometry,
                    columns,
                    perfs: None,
                }),
            })
        }
        Framing::Address => {
            let boundary = frames(session.capabilities())?;
            let found = boundary.frames.clone();
            Ok(Discovery {
                table: FrameTable::Boundary(boundary),
                frames: found,
                thumbnail: None,
                review: None,
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
            let (measured, geometry, columns) = thumbnail::frames_type2(
                session.capabilities(),
                &pass,
                samples,
                &perfs,
                length,
                polarity,
            )?;
            session.set_boundaries_type2(&measured)?;

            let x_start = session.capabilities().address.x_axis.address_range.start;
            let x_boundary = session.capabilities().address.x_axis.boundary;
            let length = geometry.dots(geometry.width());
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
                review: Some(ReviewContext {
                    geometry,
                    columns,
                    perfs: Some(perfs),
                }),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::window::tests::caps;

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
