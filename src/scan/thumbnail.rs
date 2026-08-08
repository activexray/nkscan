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
};
use crate::{
    error::Error,
    protocol::{
        caps::{
            Capabilities,
            set_window::{ColorComponents, ColorInterleaving, ScanKind, ScanMode},
        },
        data::{Boundary, Rect},
        decode::Image,
        window::{Channel, Composition, LENGTH, Window, deepest_depth},
    },
};
use tracing::*;

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
/// `polarity` of `None` is worked out from the strip.
pub fn frames(
    caps: &Capabilities,
    pass: &Pass,
    samples: &[u16],
    length: u32,
    polarity: Option<Polarity>,
) -> Result<Boundary, Error> {
    framing::reachable(caps, length)?;
    let image = Image::new(&pass.layout, samples)?;

    // A thumbnail column is one line pitch of film, and the pass starts where
    // the Y axis does, so a column is an address
    let pitch = pass.layout.line_pitch.max(1);
    let origin = caps.address.y_axis.address_range.start;
    let end = caps.address.y_axis.address_range.last;
    let (left, width) = opening(caps);

    let found = boundaries::detect(&image, (length / pitch) as usize, polarity);
    // Nothing caps the count here. `Address` byte 74 calls itself the maximum
    // and does not behave like one: it reads 0 through most of a Nikon Scan
    // session that writes four-rectangle tables
    let frames: Vec<Rect> = found
        .frames
        .iter()
        .map(|frame| origin + frame.col as u32 * pitch)
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
        polarity = ?found.polarity,
        pitch = found.pitch as u32 * pitch,
        "measured the loaded strip"
    );
    Ok(Boundary { frames })
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

    let bpp = deepest_depth(caps.set_window.depth)
        .ok_or_else(|| unsupported("this unit advertises no pixel depth".into()))?;

    // Line ordering owes the host nothing, where the three-line mode owes it
    // registration. Take it when offered rather than assuming it is
    let offered = caps.set_window.interleaving;
    if !offered.contains(ColorInterleaving::LINE_WITHOUT_DISTANCE) {
        return Err(unsupported(format!(
            "a thumbnail needs line ordering and this unit offers {offered:?}"
        )));
    }

    // 2-10-6 has one code for a one-plane output and one for three
    let channels: &[Channel] = match caps.set_window.components.contains(ColorComponents::RGB) {
        true => &[Channel::Red, Channel::Green, Channel::Blue],
        false => &[Channel::Default],
    };
    let composition = match channels.len() {
        1 => Composition::MultilevelBW,
        _ => Composition::MultilevelRGB,
    };

    let (left, width) = opening(caps);

    Ok(channels
        .iter()
        .map(|channel| {
            let mut w =
                Window::try_from(&[0u8; LENGTH][..]).expect("a zeroed descriptor is long enough");
            w.id = channel.id();
            w.composition = composition;
            w.resolution = (
                caps.address.thumbnail_resolution.start,
                caps.address.thumbnail_resolution.start,
            );
            // Y starts at the axis rather than the first frame, so the leading
            // edge of the film is in the pass and can be found
            w.origin = (left, y.address_range.start);
            w.size = (width, y.address_range.last);
            w.bpp = bpp;
            w.scanning_kind = ScanKind::THUMBNAIL;
            w.scanning_mode = ScanMode::NORMAL_QUALITY;
            w.color_interleaving = ColorInterleaving::LINE_WITHOUT_DISTANCE;
            // 2-10 byte 45: the default, and what the unit reports back for a 0
            w.ae_value = 255;
            w
        })
        .collect())
}
