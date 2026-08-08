//! What the byte stream of a scan looks like. Sections 2-10 and 2-11-3
//!
//! Image data carries no header and no length of its own, so [`Layout`] is the
//! only thing that says how much there is to read and how it is shaped.

use crate::{
    error::Error,
    protocol::{
        caps::{
            Capabilities,
            address::Transfer,
            set_window::{ColorInterleaving, ScanKind},
        },
        data::width_code,
        window::{Channel, Window, validate_set},
    },
};

/// The measurement unit divisor 2-10 treats as its second case. Any other is
/// the unit's maximum resolution, which is its first
const COARSE_DIVISOR: u16 = 1200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Output pixels along one scan line, 2-10's `W/P`
    pub pixels: u32,
    /// Output lines, 2-10's `L/P`
    pub lines: u32,
    /// Scanning pitch `P` along the scan line
    pub pitch: u32,
    /// The same down the frame. 2-10 derives both from the X resolution and
    /// calls Y ignored, but the hardware halves the lines for a half Y
    pub line_pitch: u32,
    /// What the unit will actually scan at, which is the optical resolution
    /// over the pitch and can be coarser than the window asked for
    pub dpi: u32,
    /// Bytes one sample occupies on the wire
    pub bytes_per_sample: u8,
    /// Valid bits in a sample, which can be fewer than the bytes carry
    pub bits_per_sample: u8,
    /// Window identifiers in the order SCAN was given them. 1, 2, 3 are R, G, B
    /// and 9 is infrared
    pub channels: Vec<u8>,
    /// Descriptor byte 44
    pub interleaving: ColorInterleaving,
    /// Times each line is read. More than 1 means the host owes an average
    pub readings_per_line: u8,
    /// Lines on the CCD, which is how many arrive at once under
    /// [`MULTILINE_SIMULTANEOUS`](ColorInterleaving::MULTILINE_SIMULTANEOUS)
    pub ccd_lines: u8,
    /// How far apart the CCD's lines land, in output lines. 2-11-5-3
    pub registration_gap: u32,
    /// The transfer length every READ has to be a whole number of. 1 means the
    /// unit constrains nothing
    pub granule: usize,
}

impl Layout {
    /// Describe a single-line 16-bit stream directly
    ///
    /// [`new`](Self::new) builds one from a scanner's capabilities. This builds
    /// one from bare dimensions, for decoding a file with no scanner attached.
    pub fn single_line(pixels: u32, lines: u32, channels: Vec<u8>) -> Self {
        Self {
            pixels,
            lines,
            pitch: 1,
            line_pitch: 1,
            dpi: 4000,
            bytes_per_sample: 2,
            bits_per_sample: 16,
            channels,
            interleaving: ColorInterleaving::LINE_WITHOUT_DISTANCE,
            readings_per_line: 1,
            ccd_lines: 1,
            registration_gap: 0,
            granule: 1,
        }
    }
}

/// A stream shape the unit's own numbers do not describe
fn bad(reason: String) -> Error {
    Error::Unsupported {
        op: "image layout",
        reason,
    }
}

/// How many sensor steps one output pixel spans on each axis, 2-10
///
/// 2-10 gives Y as ignored and both axes as taking X's pitch. They do not: a
/// 10000x1200 window at 666x333 comes back with exactly half the lines a square
/// 666 gives, so Y sets the line stepping and earns its own pitch.
///
/// Fractions are discarded, and the resulting pitch is snapped to the ladder
/// the unit publishes. A thumbnail runs off its own: 83 dpi against a 4000 dpi
/// sensor is pitch 48, which divides no line gap count, and it scans anyway
fn pitches(caps: &Capabilities, window: &Window) -> Result<(u32, u32), Error> {
    let optical = u32::from(caps.address.x_axis.optical_dpi);
    let asked = u32::from(window.resolution.0);
    if optical == 0 || asked == 0 {
        return Err(bad(format!(
            "cannot pitch {asked} dpi against an optical resolution of {optical}"
        )));
    }

    let snap = |raw: u32| match window.scanning_kind.contains(ScanKind::THUMBNAIL) {
        true => raw.max(1),
        false => caps.address.pitch_rule.snap(raw),
    };

    let optical_y = u32::from(caps.address.y_axis.optical_dpi).max(optical);
    let asked_y = match window.resolution.1 {
        0 => asked,
        y => u32::from(y),
    };
    Ok((snap(optical / asked), snap(optical_y / asked_y)))
}

/// The transfer length every READ has to be a whole number of, `Address` byte 4
///
/// Bit 1 makes it a line across every color, bit 2 one line. Neither set means
/// the unit constrains nothing, so any length will do
fn read_granule(caps: &Capabilities, line: usize, channels: usize) -> usize {
    let transfer = caps.address.transfer;
    if transfer.contains(Transfer::READ_LINE_COLS) {
        (line * channels).max(1)
    } else if transfer.contains(Transfer::READ_LINE) {
        line.max(1)
    } else {
        1
    }
}

impl Layout {
    /// Work out what a scan of `windows` will produce
    ///
    /// `divisor` is the measurement unit in force, from the mode page
    pub fn new(caps: &Capabilities, windows: &[Window], divisor: u16) -> Result<Self, Error> {
        // Every rule about the set itself, including that they agree on
        // everything shaping the stream
        validate_set(windows)?;
        let first = &windows[0];

        let optical = u32::from(caps.address.x_axis.optical_dpi);
        let (pitch, line_pitch) = pitches(caps, first)?;

        // A window coordinate is one sensor step only at the unit's maximum
        // resolution. At 1200 it is coarser, so the sizes scale to the sensor
        // before the pitch divides them
        let (pixels, lines) = if divisor == COARSE_DIVISOR {
            let scale = |v: u32, p: u32| {
                (u64::from(v) * u64::from(optical) / (u64::from(COARSE_DIVISOR) * u64::from(p)))
                    as u32
            };
            (scale(first.size.0, pitch), scale(first.size.1, line_pitch))
        } else {
            (first.size.0 / pitch, first.size.1 / line_pitch)
        };

        // 2-11-3: 14-bit data still transfers as two bytes
        let bytes_per_sample = first.bpp.div_ceil(8);
        if width_code(bytes_per_sample).is_none() {
            return Err(bad(format!(
                "{} bits a sample needs {bytes_per_sample} bytes, which 2-11-4 cannot encode",
                first.bpp
            )));
        }

        let channels: Vec<u8> = windows.iter().map(|w| w.id).collect();
        let line = pixels as usize * usize::from(bytes_per_sample);
        let granule = read_granule(caps, line, channels.len());

        Ok(Self {
            pixels,
            lines,
            pitch,
            line_pitch,
            dpi: optical / pitch,
            bytes_per_sample,
            bits_per_sample: first.bpp,
            channels,
            interleaving: first.color_interleaving,
            // Byte 40's high nibble is one less than the number of reads
            readings_per_line: first.multiple_reading.saturating_add(1),
            ccd_lines: caps.address.lines,
            // The gap is along the feed, so it divides by the feed's pitch. The
            // two pitches are equal except in a preview, which halves only Y
            registration_gap: u32::from(caps.address.line_gap) / line_pitch,
            granule,
        })
    }

    /// The data type qualifier's low byte for this sample width, per 2-11-4
    pub fn width_code(&self) -> u8 {
        width_code(self.bytes_per_sample).expect("checked when the layout was built")
    }

    /// Bytes in one line of every channel
    pub fn bytes_per_line(&self) -> u64 {
        u64::from(self.pixels) * u64::from(self.bytes_per_sample) * self.channels.len() as u64
    }

    /// How many bytes the whole scan will hand back
    ///
    /// The modes that raise a cooperative request are the ones this can be
    /// wrong for: the
    /// multi-line record reports its own byte and line counts, and a scan whose
    /// CCD lines need re-registering carries extra lines at the seams. Prefer
    /// the unit's numbers where it gives them
    pub fn total_bytes(&self) -> u64 {
        u64::from(self.pixels)
            * u64::from(self.lines)
            * u64::from(self.bytes_per_sample)
            * u64::from(self.readouts())
    }

    /// Readouts the unit emits per line
    ///
    /// Multi-sampling repeats the color channels only. Infrared is read once
    /// however many repeats are set, so channels times readings overstates it
    pub fn readouts(&self) -> u32 {
        let repeated = self
            .channels
            .iter()
            .filter(|id| Channel::from(**id).is_color())
            .count() as u32;
        let once = self.channels.len() as u32 - repeated;
        repeated * u32::from(self.readings_per_line) + once
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        caps::set_window::ScanKind,
        caps::{
            Page,
            address::{Address, PitchRule},
            identity::Identity,
            other::Features,
            set_window::SetWindowFunction,
        },
        window::{Composition, LENGTH},
    };

    /// An LS-9000 cut down to the fields a layout reads
    fn caps(transfer: u8, line_gap: u8, lines: u8) -> Capabilities {
        let mut p = vec![0u8; 91];
        p[1] = Address::PAGE_CODE;
        p[3] = 87;
        p[4] = transfer;
        // Frame rectangles, and a pitch rule of 2: divisors of the line gap
        p[16] = 0x42;
        p[18..20].copy_from_slice(&4000u16.to_be_bytes());
        p[20..22].copy_from_slice(&4000u16.to_be_bytes());
        p[22..24].copy_from_slice(&666u16.to_be_bytes());
        p[85] = line_gap;
        p[86] = lines;
        let address = Address::try_from(&Page::new(Address::PAGE_CODE, p).unwrap()).unwrap();

        let mut d = vec![0u8; 28];
        d[1] = SetWindowFunction::PAGE_CODE;
        d[3] = 24;
        let set_window =
            SetWindowFunction::try_from(&Page::new(SetWindowFunction::PAGE_CODE, d).unwrap())
                .unwrap();

        let mut e = vec![0u8; 39];
        e[1] = Features::PAGE_CODE;
        e[3] = 35;
        let features = Features::try_from(&Page::new(Features::PAGE_CODE, e).unwrap()).unwrap();

        let mut i = vec![0u8; 36];
        i[4] = 31;

        Capabilities {
            identity: Identity::parse(&i).unwrap(),
            address,
            features,
            set_window,
            ccd: None,
            frames: None,
        }
    }

    fn window(id: u8, dpi: u16, size: (u32, u32)) -> Window {
        let mut w = Window::try_from(&[0u8; LENGTH][..]).unwrap();
        w.id = id;
        w.resolution = (dpi, dpi);
        w.size = size;
        w.bpp = 16;
        w.color_interleaving = ColorInterleaving::LINE_WITHOUT_DISTANCE;
        w.composition = Composition::MultilevelBW;
        w
    }

    /// Three channels, so the composition has to be the three-plane one
    fn rgb(dpi: u16, size: (u32, u32)) -> Vec<Window> {
        [1, 2, 3]
            .iter()
            .map(|&id| {
                let mut w = window(id, dpi, size);
                w.composition = Composition::MultilevelRGB;
                w
            })
            .collect()
    }

    /// The one scan an LS-9000 has actually run: a 1200 x 1200 window at 666
    /// dpi, one channel, 16 bit, which read back 80000 bytes off the hardware
    #[test]
    fn the_first_real_scan_still_measures_80000_bytes() {
        let l = Layout::new(&caps(0x01, 12, 3), &[window(1, 666, (1200, 1200))], 4000).unwrap();

        assert_eq!(l.pitch, 6);
        assert_eq!((l.pixels, l.lines), (200, 200));
        assert_eq!(l.total_bytes(), 80000);
    }

    /// Table 2-10-5 in full, both columns. A gap of 12 has no pitch 5, so
    /// 1000 to 667 all scan at pitch 4 rather than at the bare ratio
    #[test]
    fn the_pitch_ladder_matches_table_2_10_5() {
        for (asked, dpi, pitch) in [
            (4000, 4000, 1),
            (2001, 4000, 1),
            (2000, 2000, 2),
            (1334, 2000, 2),
            (1333, 1333, 3),
            (1001, 1333, 3),
            (1000, 1000, 4),
            (800, 1000, 4),
            (667, 1000, 4),
            (666, 666, 6),
            (334, 666, 6),
            (333, 333, 12),
        ] {
            let l = Layout::new(&caps(0x01, 12, 3), &rgb(asked, (12000, 12000)), 4000).unwrap();
            assert_eq!((l.pitch, l.dpi), (pitch, dpi), "{asked} dpi");
            assert_eq!(l.pixels, 12000 / pitch, "{asked} dpi");
        }
    }

    /// 83 dpi against a 4000 dpi sensor is pitch 48, which divides no line gap
    /// count. A real thumbnail of 8964 x 34644 came back as 186 x 721, exactly
    /// what the unsnapped pitch gives, and 268212 bytes at one channel
    #[test]
    fn a_thumbnail_pitch_is_not_snapped_to_the_image_ladder() {
        let mut windows = rgb(83, (8964, 34644));
        windows.truncate(1);
        windows[0].composition = Composition::MultilevelBW;
        windows[0].scanning_kind = ScanKind::THUMBNAIL;

        let l = Layout::new(&caps(0x01, 12, 3), &windows, 4000).unwrap();
        assert_eq!(l.pitch, 48);
        assert_eq!((l.pixels, l.lines), (186, 721));
        assert_eq!(l.total_bytes(), 268212);

        // The same window as an image snaps to the ladder instead
        windows[0].scanning_kind = ScanKind::IMAGE;
        assert_eq!(
            Layout::new(&caps(0x01, 12, 3), &windows, 4000)
                .unwrap()
                .pitch,
            12
        );
    }

    /// 2-10 calls Y ignored. A 10000x1200 window at 666x333 came back as
    /// 1666x100 on hardware, half the lines a square 666 gives, so it is not
    #[test]
    fn a_half_y_resolution_halves_the_lines() {
        let square = Layout::new(&caps(0x01, 12, 3), &rgb(666, (10000, 1200)), 4000).unwrap();
        assert_eq!((square.pixels, square.lines), (1666, 200));
        assert_eq!(square.total_bytes(), 1999200);

        let mut half = rgb(666, (10000, 1200));
        for w in &mut half {
            w.resolution = (666, 333);
        }
        let half = Layout::new(&caps(0x01, 12, 3), &half, 4000).unwrap();
        assert_eq!((half.pixels, half.lines), (1666, 100));
        assert_eq!(half.total_bytes(), 999600);
        // X is untouched by it
        assert_eq!((half.pitch, half.line_pitch), (6, 12));
    }

    /// A gap of 1 makes every even pitch legal and nothing odd past 1
    #[test]
    fn the_one_plus_even_rule_drops_odd_pitches() {
        assert_eq!(PitchRule::OnePlusEven.snap(1), 1);
        assert_eq!(PitchRule::OnePlusEven.snap(2), 2);
        assert_eq!(PitchRule::OnePlusEven.snap(3), 2);
        assert_eq!(PitchRule::OnePlusEven.snap(7), 6);
        // Nothing to snap to when the unit reports no rule
        assert_eq!(PitchRule::Continuous.snap(7), 7);
    }

    /// 2-10 case 2: a 1200 divisor makes coordinates inches over 1200
    #[test]
    fn the_coarse_divisor_scales_the_window_to_pixels() {
        let windows = rgb(4000, (1200, 2400));
        let fine = Layout::new(&caps(0x01, 12, 3), &windows, 4000).unwrap();
        let coarse = Layout::new(&caps(0x01, 12, 3), &windows, 1200).unwrap();

        assert_eq!((fine.pixels, fine.lines), (1200, 2400));
        assert_eq!((coarse.pixels, coarse.lines), (4000, 8000));
    }

    /// Address byte 4 gives the two constraints different units, and the LS-5000
    /// sets the wider one
    #[test]
    fn the_read_granule_follows_the_advertised_units() {
        let windows = rgb(4000, (10000, 13860));
        let line = 10000 * 2;
        let granule = |transfer| {
            Layout::new(&caps(transfer, 1, 2), &windows, 4000)
                .unwrap()
                .granule
        };

        // Bit 0 is microcode downloading, not a constraint on READ
        assert_eq!(granule(0x01), 1);
        assert_eq!(granule(0x03), line * 3);
        assert_eq!(granule(0x05), line);
    }

    #[test]
    fn multiple_reading_multiplies_the_byte_count() {
        let mut windows = rgb(4000, (10000, 13860));
        let single = Layout::new(&caps(0x01, 12, 3), &windows, 4000).unwrap();
        assert_eq!(single.readings_per_line, 1);
        assert_eq!(single.total_bytes(), 10000 * 2 * 3 * 13860);

        for w in &mut windows {
            w.multiple_reading = 15;
        }
        let sixteen = Layout::new(&caps(0x01, 12, 3), &windows, 4000).unwrap();
        assert_eq!(sixteen.readings_per_line, 16);
        assert_eq!(sixteen.total_bytes(), single.total_bytes() * 16);
    }

    /// 2-11-5-3 defines the gap as Line Gap Count over the pitch
    #[test]
    fn the_registration_gap_shrinks_with_the_pitch() {
        let gap = |dpi| {
            Layout::new(&caps(0x01, 12, 3), &rgb(dpi, (10000, 13860)), 4000)
                .unwrap()
                .registration_gap
        };
        assert_eq!(gap(4000), 12);
        assert_eq!(gap(2000), 6);
        assert_eq!(gap(1333), 4);

        // At 666x333 the feed pitch is 12 and the sensor pitch 6, so the CCD's
        // lines land one output line apart rather than two
        let mut preview = rgb(666, (10000, 13860));
        for w in &mut preview {
            w.resolution = (666, 333);
        }
        let preview = Layout::new(&caps(0x01, 12, 3), &preview, 4000).unwrap();
        assert_eq!((preview.pitch, preview.line_pitch), (6, 12));
        assert_eq!(preview.registration_gap, 1);
    }

    #[test]
    fn a_window_set_that_disagrees_has_no_layout() {
        let mut windows = rgb(4000, (10000, 13860));
        windows[2].size.0 = 9000;
        assert!(Layout::new(&caps(0x01, 12, 3), &windows, 4000).is_err());

        // Exposure is what a set is allowed to differ in
        let mut windows = rgb(4000, (10000, 13860));
        windows[2].exposure = 71125;
        assert!(Layout::new(&caps(0x01, 12, 3), &windows, 4000).is_ok());
    }

    #[test]
    fn an_empty_window_set_has_no_layout() {
        assert!(Layout::new(&caps(0x01, 12, 3), &[], 4000).is_err());
    }
}

#[cfg(test)]
mod readouts {
    use super::*;

    /// The 666x333 prescan of a 6x9 frame with infrared, as the captures
    /// deliver it: 1494 sensor pixels by 1098 stage positions
    fn prescan(readings: u8) -> Layout {
        Layout {
            pixels: 1494,
            lines: 1098,
            pitch: 6,
            line_pitch: 12,
            dpi: 666,
            bytes_per_sample: 2,
            bits_per_sample: 16,
            channels: vec![9, 1, 2, 3],
            interleaving: ColorInterleaving::MULTILINE_SIMULTANEOUS,
            readings_per_line: readings,
            ccd_lines: 3,
            registration_gap: 1,
            granule: 1,
        }
    }

    /// Byte counts summed off the wire in `singleline_ccd` and
    /// `16x_multisample`. Infrared is captured once whatever the repeat count,
    /// so multiplying every channel by it overstates the pass
    #[test]
    fn a_pass_is_as_long_as_the_captures_measured() {
        assert_eq!(prescan(1).total_bytes(), 13_123_296);
        assert_eq!(prescan(16).total_bytes(), 160_760_376);

        // The 6x4.5 prescan from 8x_multisampling, same shape and fewer lines
        let short = Layout {
            lines: 558,
            ..prescan(1)
        };
        assert_eq!(short.total_bytes(), 6_669_216);
    }

    /// Three visible channels repeat, infrared does not
    #[test]
    fn only_the_color_channels_repeat() {
        assert_eq!(prescan(1).readouts(), 4);
        assert_eq!(prescan(16).readouts(), 49);
        assert_eq!(
            Layout {
                channels: vec![1, 2, 3],
                ..prescan(16)
            }
            .readouts(),
            48
        );
    }
}
