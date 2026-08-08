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
        /// AE exposure passes (in hardware)
        const AE = 1 << 5;
        /// White-balance preserving AE
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
        let (o1, o2) = (page.u8(8)?, page.u8(9)?);
        Ok(Self {
            page_length: page.u8(3)?,
            kind: ScanKind::from_bits_truncate(page.u8(4)?),
            mode: ScanMode::from_bits_truncate(page.u8(5)?),
            interleaving: ColorInterleaving::from_bits_truncate(page.u8(6)?),
            components: ColorComponents::from_bits_truncate(page.u8(7)?),
            order: [
                Component::from_nibble(o1 & 0x0F),
                Component::from_nibble(o1 >> 4),
                Component::from_nibble(o2 & 0x0F),
                Component::from_nibble(o2 >> 4),
            ],
            depth: BitDepth::from_bits_truncate(page.u8(10)?),
            setup_modes: page.u8(11)?,
            dic: page.u8(12)?,
            dic_len: page.u8(13)?,
            aic: AnalogControl::from_bits_truncate(page.u8(14)?),
            aic_len: page.u8(15)?,
            // Byte 16 is the parameter width. Both units say 4; anything else
            // would mean these offsets are wrong
            exposure: (page.be32(17)?..=page.be32(21)?).into(),
            filter_support: page.u8(25)?,
            matrix_support: page.u8(26)?,
            halftone_support: page.u8(27)?,
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
}
