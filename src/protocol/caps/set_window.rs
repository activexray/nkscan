//! SET WINDOW function page, describes what SET WINDOW can do, code 0xD1

use super::{Error, Page};
use bitflags::bitflags;
use std::range::RangeInclusive;

#[derive(Debug, Clone)]
pub struct SetWindowFunction {
    /// Declared page length; the page is 4 + this. Byte 3
    pub page_length: u8,
    /// What kind of scanning can we do. Byte 4
    pub kind: ScanKind,
    /// What modes are supported for the scan. Byte 5.
    pub mode: ScanMode,
    /// Color interleaving (how we'll decode the image data stream). Byte 6.
    pub interleaving: ColorInterleaving,
    /// The composition of the color. Byte 7
    pub components: ColorComponents,
    /// Which component may occupy each position of a multi-color read. Bytes 8-9
    pub order: [Option<Component>; 4],
    /// Per-channel output bit depth. Byte 10
    pub depth: BitDepth,
    /// Number of setup modes supported. Byte 11
    pub setup_modes: u8,
    /// Digital image control support. We're not given the bitflags for this. Byte 12
    pub dic: u8,
    /// Length of additional information for digital image control. Byte 13
    pub dic_len: u8,
    /// Analog control support. Byte 14.
    pub aic: AnalogControl,
    /// Length of additional information for analog image control. Byte  15
    pub aic_len: u8,
    /// The first analog control's range. Both units put the exposure value
    /// here, in units of 10 ns, with byte 16 giving its width as 4. Bytes 16-24
    pub exposure: RangeInclusive<u32>,
    /// Byte 25
    pub filter_support: u8,
    /// Byte 26
    pub matrix_support: u8,
    /// Byte 27
    pub halftone_support: u8,
}

bitflags! {
    /// Byte 4, describes the kind of image scanning we can do
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct ScanKind: u8 {
        const IMAGE = 1 <<0;
        const THUMBNAIL = 1 << 1;
        /// Scanning for deciding exposure/gains/etc
        const SETUP_1 = 1 << 2;
        /// Sames as SETUP_1 but the low-density/high-density
        /// limit values are used instead of the maximum value and the minimum value.
        /// Called reserved by this page on the 9000, but named by its own 2-10
        /// byte 42 table, and set on hardware
        const SETUP_2 = 1 << 3;
        /// Scanning for creating a histogram of the data. Only the 5000's copy
        /// of this page names it; both 2-10 tables call it reserved. Set on a
        /// 9000 anyway
        const HISTOGRAM = 1 << 4;
        /// AE exposure passes (in hardware). No unit seen sets this: D1h byte 4
        /// is 03h on an LS-50 and 1Bh on an LS-9000, so both meter host-side
        const AE = 1 << 5;
        /// White-balance preserving AE. Unset everywhere [`AE`](Self::AE) is
        const AE_WB = 1 << 6;
    }
}

bitflags! {
    /// Byte 5, the mode of the scan
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct ScanMode: u8 {
        const HIGH_QUALITY =      1 << 0;
        const NORMAL_QUALITY =    1 << 1;
        const HIGH_SPEED =        1 << 2;
        // bit 3 reserved
        const MULTI_READING =     1 << 4;
        // bit 5 reserved
        const REVERSE_DIRECTION = 1 << 6;
    }
}

bitflags! {
    /// Byte 6, how the image data is color-interleaved
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct ColorInterleaving: u8 {
        const PIXEL_WITHOUT_DISTANCE = 1 << 0;
        const LINE_WITHOUT_DISTANCE =  1 << 1;
        const PLANE =                  1 << 2;
        // 3 reserved
        const PIXEL_WITH_DISTANCE =    1 << 4;
        const LINE_WITH_DISTANCE =     1 << 5;
        const MULTILINE_SIMULTANEOUS = 1 << 6;
    }
}

bitflags! {
    /// Byte 7, the color composition to be scanned
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct ColorComponents: u8 {
        const NEUTRAL_GRAY = 1 << 0;
        /// Old-school document scanner setting
        const DROPOUT =      1 << 1;
        /// Red-Green-Blue
        const RGB =          1 << 2;
        /// Cyan-Magenta-Yellow
        const CMY =          1 << 3;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    Red,       // Or Cyan
    Green,     // Or Magenta
    Blue,      // Or Yellow
    Other(u8), // Secret fourth thing (9 for IR)
}

bitflags! {
    /// Byte 10, bit-depth per channel
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct BitDepth: u8 {
        const BIT_1  = 1 << 0;
        const BIT_8  = 1 << 1;
        const BIT_10 = 1 << 2;
        const BIT_12 = 1 << 3;
        const BIT_14 = 1 << 4;
        const BIT_16 = 1 << 5;
    }
}

bitflags! {
    /// Byte 14, analog image control functions this unit supports
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct AnalogControl: u8 {
        const GAMMA          = 1 << 0;
        const EXPOSURE_TIME  = 1 << 1;
        const ANALOG_GAIN    = 1 << 2;
        const DIGITAL_GAIN   = 1 << 3;
        const ANALOG_SHIFT   = 1 << 4;
        const ANALOG_OFFSET  = 1 << 5;
        const EXPOSURE_VALUE = 1 << 6;
    }
}

impl Component {
    /// `None` for 0, which the spec defines as "all colors"
    fn from_nibble(n: u8) -> Option<Self> {
        match n {
            0 => None,
            1 => Some(Self::Red),
            2 => Some(Self::Green),
            3 => Some(Self::Blue),
            x => Some(Self::Other(x)),
        }
    }
}

impl SetWindowFunction {
    pub const PAGE_CODE: u8 = 0xD1;

    /// Whether `component` may be read in `position` (0-based)
    pub fn permits(&self, position: usize, component: Component) -> bool {
        self.order
            .get(position)
            .is_none_or(|slot| slot.is_none_or(|c| c == component))
    }
}

impl TryFrom<&Page> for SetWindowFunction {
    type Error = Error;

    fn try_from(page: &Page) -> Result<Self, Self::Error> {
        // Five extendable fields in a row, each saying where the next begins.
        // The byte numbers are where a unit that extends none of them puts
        // them, which is both of the ones we have
        let (kind, len) = page.flags(4)?; // byte 4
        let (mode, n) = page.flags(4 + len)?; // byte 5
        let (interleaving, n2) = page.flags(4 + len + n)?; // byte 6
        let (components, n3) = page.flags(4 + len + n + n2)?; // byte 7
        let order = 4 + len + n + n2 + n3; // byte 8
        let (o1, o2) = (page.u8(order)?, page.u8(order + 1)?);
        let (depth, n4) = page.flags(order + 2)?; // byte 10
        let rest = order + 2 + n4; // byte 11

        // The digital control's additional information sits between the two
        // supports, so the analog side starts wherever that ended
        let dic_len = page.u8(rest + 2)?;
        let (aic, n5) = page.flags(rest + 3 + usize::from(dic_len))?; // byte 14
        let analog = rest + 3 + usize::from(dic_len);
        let aic_len = page.u8(analog + n5)?;

        // Then the first control's parameter: its width, its minimum and its
        // maximum, the last two as wide as the first says. A unit offering no
        // analog control at all has no exposure to set
        let first = analog + n5 + 1;
        let exposure = match aic_len {
            0 => (0..=0).into(),
            _ => {
                let width = usize::from(page.u8(first)?);
                (page.be(first + 1, width)?..=page.be(first + 1 + width, width)?).into()
            }
        };
        let tail = first + usize::from(aic_len);

        Ok(Self {
            page_length: page.u8(3)?,
            kind: ScanKind::from_bits_truncate(kind as u8),
            mode: ScanMode::from_bits_truncate(mode as u8),
            interleaving: ColorInterleaving::from_bits_truncate(interleaving as u8),
            components: ColorComponents::from_bits_truncate(components as u8),
            order: [
                Component::from_nibble(o1 & 0x0F),
                Component::from_nibble(o1 >> 4),
                Component::from_nibble(o2 & 0x0F),
                Component::from_nibble(o2 >> 4),
            ],
            depth: BitDepth::from_bits_truncate(depth as u8),
            setup_modes: page.u8(rest)?,
            dic: page.u8(rest + 1)?,
            dic_len,
            aic: AnalogControl::from_bits_truncate(aic as u8),
            aic_len,
            exposure,
            filter_support: page.u8(tail)?,
            matrix_support: page.u8(tail + 1)?,
            halftone_support: page.u8(tail + 2)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read off a real LS-9000 ED
    const LS9000: &[u8] = &[
        0x06, 0xD1, 0x00, 0x18, 0x1B, 0x16, 0x42, 0x06, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x40,
        0x09, 0x04, 0x00, 0x00, 0x00, 0x01, 0x03, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00,
    ];

    /// 2-2-2-4's SA-21/SA-30 column for the LS-5000
    fn ls5000() -> Vec<u8> {
        let mut p = LS9000.to_vec();
        p[4] = 0x03;
        p[5] = 0x52;
        p[8] = 0x20;
        p[9] = 0x43;
        p
    }

    fn parse(bytes: &[u8]) -> SetWindowFunction {
        let page = Page::new(SetWindowFunction::PAGE_CODE, bytes.to_vec()).expect("page");
        SetWindowFunction::try_from(&page).expect("set window")
    }

    /// One has high speed, the other reverse direction, neither has both.
    /// The LS-9000's prose claims reverse direction; its bits disagree
    #[test]
    fn the_families_offer_different_scan_modes() {
        let nine = parse(LS9000).mode;
        let five = parse(&ls5000()).mode;

        assert!(nine.contains(ScanMode::HIGH_SPEED));
        assert!(!nine.contains(ScanMode::REVERSE_DIRECTION));
        assert!(five.contains(ScanMode::REVERSE_DIRECTION));
        assert!(!five.contains(ScanMode::HIGH_SPEED));
    }

    /// 0 in a nibble means "any color here". The LS-5000 pins three positions,
    /// and its fourth is the only place either spec admits to a channel past
    /// blue. That is an ordering code, not the window id captures use
    #[test]
    fn color_ordering_is_free_on_one_and_constrained_on_the_other() {
        assert_eq!(parse(LS9000).order, [None; 4]);

        let five = parse(&ls5000());
        assert_eq!(
            five.order,
            [
                None,
                Some(Component::Green),
                Some(Component::Blue),
                Some(Component::Other(4)),
            ]
        );
        assert!(five.permits(0, Component::Red));
        assert!(!five.permits(1, Component::Red));
    }

    /// 2-2-2-4 gives byte 4 as 03h and calls bits 3 and 4 reserved; hardware
    /// reports 1Bh. Setup Scan 2 is corroborated by byte 11, which the same
    /// spec documents and defines as effective only when that bit is set
    #[test]
    fn hardware_sets_scan_kinds_its_own_spec_calls_reserved() {
        let real = parse(LS9000);
        assert!(real.kind.contains(ScanKind::SETUP_2 | ScanKind::HISTOGRAM));
        assert_eq!(real.setup_modes, 0);
    }

    /// Every field before the analog control is extendable, so a unit that
    /// carries any of them on moves the whole tail
    #[test]
    fn an_extended_scanning_kind_moves_the_analog_side_along() {
        let mut p = LS9000.to_vec();
        p.insert(5, 0x00); // the byte the scanning kind extends into
        p[4] |= 0x80;
        p[3] += 1;

        let moved = parse(&p);
        let flat = parse(LS9000);
        assert_eq!(moved.kind, flat.kind);
        assert_eq!(moved.mode, flat.mode);
        assert_eq!(moved.depth, flat.depth);
        assert_eq!(moved.aic, flat.aic);
        assert_eq!(moved.exposure, flat.exposure);
    }

    /// A real LS-8000 ED, which reaches the same fields with a 14 bit depth
    #[test]
    fn the_walk_lands_on_a_second_unit() {
        const LS8000: &[u8] = &[
            0x06, 0xD1, 0x00, 0x17, 0x77, 0x16, 0x42, 0x46, 0x00, 0x00, 0x12, 0x00, 0x00, 0x00,
            0x40, 0x09, 0x04, 0x00, 0x00, 0x00, 0x01, 0x03, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00,
        ];
        let real = parse(LS8000);
        assert_eq!(real.aic, AnalogControl::EXPOSURE_VALUE);
        assert_eq!(real.aic_len, 9);
        assert_eq!(real.exposure, (1..=0x3FF_FFFF).into());
        assert_eq!(real.depth, BitDepth::BIT_8 | BitDepth::BIT_14);
    }

    /// Both known units leave the digital control empty, so the analog side
    /// happens to start at byte 14. A unit that carried some would push the
    /// whole tail along by the length it declared
    #[test]
    fn digital_control_information_moves_the_analog_side_along() {
        let mut p = LS9000.to_vec();
        let tail = p.split_off(14);
        p.extend([0xAA, 0xBB]); // two bytes of digital control information
        p.extend(tail);
        p[3] += 2;
        p[13] = 2;

        let moved = parse(&p);
        assert_eq!(moved.aic, parse(LS9000).aic);
        assert_eq!(moved.exposure, parse(LS9000).exposure);
        assert_eq!(moved.halftone_support, parse(LS9000).halftone_support);
    }

    /// The width of the first control's minimum and maximum is a field of its
    /// own, not the 4 bytes both units happen to use
    #[test]
    fn the_control_parameter_is_as_wide_as_the_unit_says() {
        let mut p = LS9000.to_vec();
        p[15] = 5; // a width, then a two byte minimum and maximum
        p[16] = 2;
        p[17..19].copy_from_slice(&64u16.to_be_bytes());
        p[19..21].copy_from_slice(&4096u16.to_be_bytes());
        p[21] = 0x11; // where filter, matrix and halftone now sit
        p[22] = 0x22;
        p[23] = 0x33;

        let narrow = parse(&p);
        assert_eq!(narrow.exposure, (64..=4096).into());
        assert_eq!(
            (
                narrow.filter_support,
                narrow.matrix_support,
                narrow.halftone_support
            ),
            (0x11, 0x22, 0x33)
        );
    }
}
