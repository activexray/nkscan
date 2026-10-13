//! Scanning all the available film at once to generate a thumbnail
//!
//! `Address` byte 16 says whether the unit publishes frames at all, and
//! `Frames` says whether it knows where they end. A fixed-format mount does;
//! loose film reports a length of zero until something measures it.
//!
//! `Features` puts thumbnail in the host cooperation bits on both families, so
//! the unit hands us the pass and expects us to make sense of it.

use super::{boundary, framing, pass::Pass, profile::Film, strip, window};
use crate::{
    error::Error,
    protocol::{
        caps::{
            Capabilities,
            film::FilmFormat,
            set_window::{ColorInterleaving, ScanKind, ScanMode},
        },
        data::{Boundary, Rect},
        decode::{Image, Samples},
        window::{Flags, Window},
    },
};
use tracing::*;

/// Stage addresses one thumbnail column spans
///
/// The optical resolution over the resolution the pass asks for, rounded down,
/// which is the pass's own `line_pitch` computed without the pass
pub(crate) fn line_pitch(caps: &Capabilities) -> u32 {
    let optical =
        u32::from(caps.address.y_axis.optical_dpi).max(u32::from(caps.address.x_axis.optical_dpi));
    match u32::from(caps.address.thumbnail_resolution.start) {
        0 => 1,
        asked => (optical / asked).max(1),
    }
}

/// The Y addresses between two thumbnail lines
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinePitch(u32);

impl LinePitch {
    /// The pitch the pass asks for
    pub fn computed(caps: &Capabilities) -> Self {
        Self(line_pitch(caps))
    }

    /// The pitch `pass` took
    pub(crate) fn of(pass: &Pass) -> Self {
        Self(pass.layout.line_pitch.max(1))
    }

    /// Stage addresses one thumbnail column spans
    pub fn addresses_per_column(&self) -> f64 {
        f64::from(self.0)
    }

    /// The Y address of a thumbnail line, from the axis origin
    pub fn address_of(&self, line: u32) -> u32 {
        line * self.0
    }

    /// The number of thumbnail lines in a length of film, rounded
    pub fn columns(&self, addresses: u32) -> usize {
        let per = self.0.max(1);
        ((addresses + per / 2) / per) as usize
    }
}

/// The top of a frame of `format` centered on the middle of its picture
///
/// The format is the rectangle the caller scans. A camera gate is not that
/// rectangle, so the format is put over the middle of the picture and the
/// difference goes to both ends. `None` where the axis is shorter than the
/// format
fn top_of(caps: &Capabilities, middle: u32, format: u32) -> Option<u32> {
    let origin = caps.address.y_axis.address_range.start;
    let end = caps.address.y_axis.address_range.last;
    let last = end.checked_sub(format)?;
    Some(
        middle
            .saturating_sub(format / 2)
            .clamp(origin.min(last), last),
    )
}

/// Nikon Scan's thumbnail exposure factors on the start-up exposure, by film and holder or adapter
///
/// `CNkThumbSourceData::SetDataInfo` (LS5000.md3 Mac `1012b430`, LS9000.md3 `10017a60`).
/// `None` if Nikon Scan takes no thumbnail.
pub fn exposure_factors(caps: &Capabilities, film: Film) -> Option<[f64; 3]> {
    const POSITIVE: [f64; 3] = [1.186, 1.21, 1.259];
    const NEGATIVE: [f64; 3] = [1.231, 2.844, 3.839];
    const DENSE_NEGATIVE: [f64; 3] = [1.259, 3.162, 5.012];
    const MONO: [f64; 3] = [1.301, 1.528, 1.411];
    let row = match caps.address.holder_id {
        Some(holder) => match (holder, film) {
            (_, Film::Kodachrome) => [1.246, 1.259, 1.246],
            (0x13, _) => [1.0; 3],
            (0x12..=0x18, Film::Positive) => POSITIVE,
            (0x12..=0x18, Film::MonochromeNegative) => MONO,
            (0x12 | 0x14 | 0x15, Film::Negative) => DENSE_NEGATIVE,
            (0x16..=0x18, Film::Negative) => NEGATIVE,
            _ => return None,
        },
        None => {
            let strip = match caps.adapter.as_deref()? {
                "Strip" | "6Strip" | "36Strip" => true,
                "240" => false,
                _ => return None,
            };
            match (film, strip) {
                (Film::Kodachrome, _) => [1.24; 3],
                (Film::Positive, _) => POSITIVE,
                (Film::Negative, true) => NEGATIVE,
                (Film::MonochromeNegative, true) => MONO,
                (Film::Negative | Film::MonochromeNegative, false) => DENSE_NEGATIVE,
            }
        }
    };
    Some(row)
}

/// Nikon Scan's film base density off a thumbnail (`GetBaseDensity`, LS5000.md3 Mac `10139370`)
///
/// `ratio` is the start-up exposure over the thumbnail exposure. `None` if the thumbnail is short.
pub(crate) fn base_density(image: &Image, ratio: [f64; 3]) -> Option<[f64; 3]> {
    const LINES: std::ops::Range<usize> = 0x4b..0xe1;
    const RANGE: [(f64, f64); 3] = [(0.121, 0.526), (0.459, 0.969), (0.654, 1.137)];
    if image.cols < 0xe2 || image.colors.len() < 3 {
        return None;
    }
    let full = f64::from(super::meter::ceiling(image.bits));
    let brightest: [f64; 3] = std::array::from_fn(|c| {
        LINES
            .map(|x| {
                let sum: u64 = (0..image.rows)
                    .map(|y| u64::from(image.colors[c][y * image.stride + x]))
                    .sum();
                sum as f64 / image.rows as f64
            })
            .filter(|&average| average < 0.9 * full)
            .fold(0.0, f64::max)
    });
    if brightest.contains(&0.0) {
        return None;
    }
    let density = std::array::from_fn(|c| {
        let d = (full / (brightest[c] * ratio[c])).log10();
        let (lo, hi) = RANGE[c];
        if (lo..=hi).contains(&d) {
            d
        } else {
            super::meter::DEFAULT_BASE_DENSITY[c]
        }
    });
    debug!(?density, "film base density");
    Some(density)
}

/// Whether this unit and adapter will thumbnail at all
///
/// Support follows the adapter rather than the model, so this is re-decided
/// whenever the adapter changes
pub fn available(caps: &Capabilities) -> bool {
    caps.set_window.kind.contains(ScanKind::THUMBNAIL)
        && caps.address.thumbnail_resolution.start > 0
}

/// The top of each frame that [`strip::find`] finds, in Y addresses; `None` if it finds none
fn tops(
    caps: &Capabilities,
    image: &Image,
    pitch: LinePitch,
    format: u32,
) -> Option<(Vec<u32>, strip::Strip)> {
    let found = strip::find(image, pitch.columns(format))?;
    let origin = caps.address.y_axis.address_range.start;
    let tops = found
        .frames
        .iter()
        .map(|frame| origin + pitch.address_of((frame.start + frame.len() / 2) as u32))
        .filter_map(|middle| top_of(caps, middle, format))
        .collect();
    Some((tops, found))
}

/// The frame table (2-11-6), the line pitch, and the [`strip::find`] contrast, from a thumbnail
///
/// A 120 format uses Nikon Scan's search ([`boundary::rectangle`]). Other formats use [`strip::find`].
pub fn frames(
    caps: &Capabilities,
    pass: &Pass,
    samples: &Samples,
    format: FilmFormat,
    film: Film,
) -> Result<(Boundary, LinePitch, Option<f32>), Error> {
    let y = &caps.address.y_axis;
    let length = window::reachable_blocks(caps, format.height_dots(y.optical_dpi));
    framing::reachable(caps, length)?;
    let image = pass.image(samples)?;
    let pitch = LinePitch::of(pass);
    let (left, width) = opening(caps);
    let last = y.address_range.last.saturating_sub(length);

    let (tops, contrast): (Vec<u32>, _) = if format.is_120() {
        let found = boundary::rectangle::frames(&image, film, format, caps.address.holder_id)
            .unwrap_or_default();
        let tops = found
            .iter()
            .map(|&(s, _)| (y.address_range.start + pitch.address_of(s.max(0) as u32)).min(last))
            .collect();
        (tops, None)
    } else {
        match tops(caps, &image, pitch, length) {
            Some((tops, found)) => (tops, Some(found.contrast)),
            None => (Vec::new(), None),
        }
    };

    let frames: Vec<Rect> = tops
        .into_iter()
        .map(|top| Rect {
            top,
            left,
            bottom: top + length,
            right: left + width,
        })
        .collect();
    info!(frames = frames.len(), "measured the loaded strip");
    for (n, rect) in frames.iter().enumerate() {
        debug!(frame = n + 1, ?rect, "frame rect");
    }
    Ok((Boundary { frames }, pitch, contrast))
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
        // Nikon Scan's flags for all units and films
        w.flags = Flags::AVERAGING | Flags::POSITIVE;
        w.color_interleaving = ColorInterleaving::LINE_WITHOUT_DISTANCE;
    }
    Ok(windows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::window::tests::caps;

    /// Answers INQUIRY with the pages of a capture, and refuses other pages
    struct Replay(std::collections::HashMap<String, Vec<u8>>);

    impl crate::transport::Transport for Replay {
        fn max_transfer(&self) -> usize {
            65536
        }

        fn execute(
            &mut self,
            cdb: &[u8],
            data: crate::transport::Data,
            _: std::time::Duration,
        ) -> Result<crate::transport::Completion, crate::transport::Error> {
            use crate::transport::{Completion, Data, Sense, Status};
            let key = match cdb[1] & 1 {
                0 => "std".to_owned(),
                _ => format!("{:02x}", cdb[2]),
            };
            Ok(match (self.0.get(&key), data) {
                (Some(page), Data::In(buf)) => {
                    let n = page.len().min(buf.len());
                    buf[..n].copy_from_slice(&page[..n]);
                    Completion {
                        status: Status::Good,
                        sense: None,
                        transferred: n,
                    }
                }
                _ => Completion {
                    status: Status::CheckCondition,
                    sense: Some(Sense {
                        key: 5,
                        asc: 0x24,
                        ascq: 0,
                        tsc: None,
                        ili: false,
                        information: None,
                        raw: Vec::new(),
                    }),
                    transferred: 0,
                },
            })
        }
    }

    /// Compares our thumbnail windows with Nikon Scan's; `start` is the unit's 8Ch exposures
    fn windows_match_nikon(unit: &str, film: Film, start: [u32; 3]) {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/thumbnail_window")
            .join(unit);
        let pages = std::fs::read_dir(&dir)
            .expect("fixture")
            .filter_map(|e| {
                let name = e.ok()?.file_name().into_string().ok()?;
                let code = name.strip_prefix("page_")?.strip_suffix(".bin")?.to_owned();
                Some((code, std::fs::read(dir.join(&name)).ok()?))
            })
            .collect();
        let caps = crate::session::probe::capabilities(&mut Replay(pages)).expect("capabilities");
        let factors = exposure_factors(&caps, film).expect("factors");
        let mut wrong = Vec::new();
        for (c, mut w) in windows(&caps).expect("windows").into_iter().enumerate() {
            w.exposure = crate::scan::meter::scale(start[c], factors[c]);
            let nikon = std::fs::read(dir.join(format!("window_{}.bin", w.id))).expect("window");
            let ours = w.to_bytes();
            if ours[..] != nikon[..] {
                wrong.push(format!(
                    "window {}\n ours  {:02x?}\n nikon {:02x?}",
                    w.id, ours, nikon
                ));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn an_ls50_thumbnail_window_is_nikons() {
        windows_match_nikon("ls50", Film::Negative, [48130, 51796, 35362]);
    }

    #[test]
    fn an_ls9000_thumbnail_window_is_nikons() {
        windows_match_nikon("ls9000", Film::Positive, [326754, 234030, 192099]);
    }

    /// Nikon Scan's thumbnail exposures in two captures, from the start-up exposures
    #[test]
    fn a_thumbnail_is_exposed_as_nikon_exposes_it() {
        let exposed = |caps: &Capabilities, film, start: [u32; 3]| {
            let f = exposure_factors(caps, film).expect("factors");
            [0, 1, 2].map(|c| crate::scan::meter::scale(start[c], f[c]))
        };
        let mut ls50 = caps();
        ls50.address.holder_id = None;
        ls50.adapter = Some("6Strip".into());
        assert_eq!(
            exposed(&ls50, Film::Negative, [47971, 51820, 35423]),
            [59052, 147376, 135988]
        );
        let mut ls9000 = caps();
        ls9000.address.holder_id = Some(0x17);
        assert_eq!(
            exposed(&ls9000, Film::Positive, [326754, 234030, 192099]),
            [387530, 283176, 241852]
        );
        // Nikon Scan takes no thumbnail through a mount
        ls50.adapter = Some("Mount".into());
        assert_eq!(exposure_factors(&ls50, Film::Negative), None);
    }
}
