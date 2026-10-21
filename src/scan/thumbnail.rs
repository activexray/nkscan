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
            address::Axis,
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

    /// The feed address of each of `cols` columns
    pub(crate) fn column_addresses(&self, caps: &Capabilities, cols: usize) -> Vec<u32> {
        let origin = caps.address.y_axis.address_range.start;
        (0..cols as u32)
            .map(|i| origin + self.address_of(i))
            .collect()
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
/// `CNkThumbSourceData::SetDataInfo` (LS5000.md3 Mac `1012b430`, LS9000.md3 `10017910`).
/// `None` if Nikon Scan takes no thumbnail.
///
/// LS4000.md3 (`10011dc0`) and LS8000.md3 (`100161d0`) have other positive and
/// Kodachrome rows. We use the rows of the newer modules on all units. A
/// factor only sets the brightness of the thumbnail, because
/// [`framing`](super::framing) divides it out of the base density.
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
            // FH-869M is code 8400h in Nikon Scan, which takes the dense row
            (0x12 | 0x14..=0x16, Film::Negative) => DENSE_NEGATIVE,
            (0x17 | 0x18, Film::Negative) => NEGATIVE,
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
/// The same function is in LS4000.md3 (`100794a0`) and LS5000.md3 (`10088810`).
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

/// The origin and size of Nikon Scan's thumbnail window
///
/// Nikon Scan selects the window by holder or adapter:
/// - FH-869S and FH-869G: from the left of the first image, as wide as the
///   opening, to the Y maximum address. LS9000.md3 `1008d6b0`, LS8000.md3
///   `1008a4d0`. These are the holders that take Boundary Information after
///   the thumbnail (LS9000 2-11-6).
/// - All other holders: the full addressable area, one pass for all frames
///   and strips. LS9000.md3 `1008dd50`, LS8000.md3 `10088560`.
/// - IA-20: the X boundary wide, and one Y boundary for each loaded frame.
///   LS5000.md3 `10083ba0`. This is the Y maximum address plus one (LS5000
///   2-2-2-3, note 3).
/// - Strip adapters: the X boundary wide, to the Y maximum address.
///   `CSAThumbManager::CreateRect`, LS5000.md3 Mac `10047270`.
fn area(caps: &Capabilities) -> ((u32, u32), (u32, u32)) {
    let (x, y) = (&caps.address.x_axis, &caps.address.y_axis);
    let full = |axis: &Axis| axis.address_range.last - axis.address_range.start + 1;
    let top = y.address_range.start;
    match (caps.address.holder_id, caps.adapter.as_deref()) {
        (Some(holder), _) if !matches!(holder, 0x17 | 0x18) => {
            ((x.address_range.start, top), (full(x), full(y)))
        }
        (None, Some("240")) => {
            let frames = u32::from(caps.address.loaded_frames);
            (
                (x.address_range.start, top),
                (x.boundary, frames * y.boundary),
            )
        }
        _ => {
            let (left, width) = opening(caps);
            ((left, top), (width, y.address_range.last))
        }
    }
}

/// The X address of each row of a thumbnail
///
/// The CCD reads out opposite to increasing X, so the first row has the
/// highest address (see `protocol::decode`).
pub(crate) fn row_addresses(caps: &Capabilities, pass: &Pass) -> Vec<u32> {
    addresses_of_rows(caps, pass.rows, pass.layout.pitch)
}

/// The X address of each of `rows` rows, `pitch` addresses apart
fn addresses_of_rows(caps: &Capabilities, rows: usize, pitch: u32) -> Vec<u32> {
    let ((left, _), _) = area(caps);
    let pitch = pitch.max(1);
    (0..rows as u32).rev().map(|r| left + r * pitch).collect()
}

/// Windows over everything the adapter can reach, one per channel
pub(crate) fn windows(caps: &Capabilities) -> Result<Vec<Window>, Error> {
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

    let (origin, size) = area(caps);
    let mut windows = window::blank(caps, &window::color_channels(caps))?;
    for w in &mut windows {
        w.resolution = (
            caps.address.thumbnail_resolution.start,
            caps.address.thumbnail_resolution.start,
        );
        // Y starts at the axis rather than the first frame, so the leading
        // edge of the film is in the pass and can be found
        w.origin = origin;
        w.size = size;
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
    fn an_ls4000_roll_thumbnail_window_is_nikons() {
        windows_match_nikon("ls4000", Film::Negative, [46805, 36481, 29701]);
    }

    #[test]
    fn an_ls9000_thumbnail_window_is_nikons() {
        windows_match_nikon("ls9000", Film::Positive, [326754, 234030, 192099]);
    }

    /// The test capabilities with the C1, C8 and D1 pages of an LS-9000 with an FH-835S
    fn fh835s() -> Capabilities {
        use crate::protocol::caps::{
            Page, address::Address, frames::Frames, set_window::SetWindowFunction,
        };
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/thumbnail_window/ls9000_fh835s");
        let page = |code: u8| {
            let bytes = std::fs::read(dir.join(format!("page_{code:02x}.bin"))).expect("fixture");
            Page::new(code, bytes).expect("page")
        };
        let mut caps = caps();
        caps.address = Address::try_from(&page(Address::PAGE_CODE)).expect("address");
        caps.frames = Some(Frames::try_from(&page(Frames::PAGE_CODE)).expect("frames"));
        caps.set_window =
            SetWindowFunction::try_from(&page(SetWindowFunction::PAGE_CODE)).expect("set window");
        caps
    }

    /// One pass over the full addressable area, so the second strip is in the thumbnail
    #[test]
    fn an_fh835s_thumbnail_covers_both_strips() {
        let caps = fh835s();
        assert_eq!(caps.address.holder_id, Some(0x15));
        let (origin, size) = area(&caps);
        assert_eq!((origin, size), ((0, 0), (10000, 36220)));
        let frames = &caps.frames.as_ref().expect("frames").images;
        assert_eq!(frames.len(), 12);
        assert!(frames.iter().all(|f| f.left + f.width <= size.0));
        for w in windows(&caps).expect("windows") {
            w.validate(&caps).expect("a window the unit takes");
        }
    }

    /// The first row has the highest X address, and the second strip is in the rows
    #[test]
    fn an_fh835s_thumbnail_row_has_an_x_address() {
        let caps = fh835s();
        // 83 dpi from 4000 is pitch 48, and 10000 addresses are 208 rows
        let rows = addresses_of_rows(&caps, 208, 48);
        assert_eq!((rows[0], rows[207]), (207 * 48, 0));
        let second = caps.frames.as_ref().expect("frames").images[6];
        assert_eq!(second.left, 5834);
        assert!(
            rows.iter()
                .any(|&x| (second.left..second.left + second.width).contains(&x))
        );
    }

    /// One Y boundary for each loaded frame, which is one more than the Y maximum address
    #[test]
    fn an_ia20_thumbnail_reaches_past_the_last_address() {
        let mut caps = caps();
        caps.address.holder_id = None;
        caps.adapter = Some("240".into());
        caps.address.loaded_frames = 25;
        caps.address.y_axis.boundary = 4453;
        caps.address.y_axis.address_range = (0..=25 * 4453 - 1).into();
        let (_, size) = area(&caps);
        assert_eq!(size, (caps.address.x_axis.boundary, 25 * 4453));
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
