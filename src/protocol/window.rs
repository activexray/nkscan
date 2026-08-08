//! Data around the GET/SET WINDOW commands. Section 2-10

use super::caps::set_window::{BitDepth, ColorInterleaving, ScanKind, ScanMode};
use crate::{error::Error, protocol::caps::Capabilities};
use bitflags::bitflags;
use tracing::*;

/// How many bytes one descriptor occupies, per table 2-10-3
pub const LENGTH: usize = 50;

/// A scanning channel
///
/// One namespace for two fields: a window identifier in 2-10-4 and a data type
/// qualifier in 2-11-3, which agree on the codes
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Channel {
    /// 0. 2-11-3 calls this the G component, and 2-7 will only scan it alone
    Default,
    Red,
    Green,
    Blue,
    /// 4. 2-10-4 names it and neither unit supports it
    NeutralGray,
    /// 9, in neither spec. Measures obstructions rather than color
    Infrared,
    /// A code neither spec names
    Other(u8),
}

impl Channel {
    pub const fn id(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::Red => 1,
            Self::Green => 2,
            Self::Blue => 3,
            Self::NeutralGray => 4,
            Self::Infrared => 9,
            Self::Other(id) => id,
        }
    }

    /// Whether this carries color rather than measuring what is in the way
    ///
    /// 2-10-6's composition counts these and not infrared
    pub const fn is_color(self) -> bool {
        !matches!(self, Self::Infrared)
    }

    /// Where this sits in an R, G, B ordered reading, for the data types whose
    /// qualifier names a component
    pub const fn visible_index(self) -> Option<usize> {
        match self {
            Self::Red => Some(0),
            // 2-11-3: the default qualifier is the green component
            Self::Green | Self::Default => Some(1),
            Self::Blue => Some(2),
            _ => None,
        }
    }
}

impl From<u8> for Channel {
    fn from(id: u8) -> Self {
        match id {
            0 => Self::Default,
            1 => Self::Red,
            2 => Self::Green,
            3 => Self::Blue,
            4 => Self::NeutralGray,
            9 => Self::Infrared,
            x => Self::Other(x),
        }
    }
}

/// Byte 26 of a descriptor vs byte 10 of `SetWindowFunction`, deepest first so a search for what a unit offers finds the best of them
const DEPTHS: [(u8, BitDepth); 6] = [
    (16, BitDepth::BIT_16),
    (14, BitDepth::BIT_14),
    (12, BitDepth::BIT_12),
    (10, BitDepth::BIT_10),
    (8, BitDepth::BIT_8),
    (1, BitDepth::BIT_1),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// Window identifier, 0,1,2,3,9 (Default 0 is the same as green 2). Byte 0.
    /// 2-10-4 stops at 4 (neutral gray, unsupported); 9 is infrared, which only
    /// the hardware admits to
    pub id: u8,
    /// Byte 1 bit 0
    pub auto: bool,
    /// Resolution in DPI (X/Y). Bytes 2-5
    ///
    /// 2-10 byte 4-5 says Y is ignored and GET WINDOW answers it with X. It is
    /// not: a 10000x1200 window at 666x333 returns 100 lines where a square 666
    /// returns 200, so Y sets the stepping down the frame. Nikon Scan halves it
    /// for every preview pass.
    ///
    /// An X the unit cannot do is rounded to `optical / round(optical / asked)`
    /// and reported with `01h-37h-00h`
    pub resolution: (u16, u16),
    /// Window origin (upper left) of X and Y. Bytes 6-13
    ///
    /// The wire unit is inches times the measurement unit divisor, so at the
    /// unit's maximum resolution these are pixels
    pub origin: (u32, u32),
    /// Size of the window, in the same units as the origin.
    /// Bounded by `Address`'s boundary, which the power-on descriptor exceeds. Bytes 14-21
    pub size: (u32, u32),
    /// Brightness. Byte 22
    pub brightness: u8,
    /// Threshold. Byte 23
    pub threshold: u8,
    /// Contrast. Byte 24
    pub contrast: u8,
    /// Image composition. Byte 25
    pub composition: Composition,
    /// Pixel composition (bit depth). Byte 26
    pub bpp: u8,
    /// Halftone pattern. Bytes 27-28
    pub halftone_pattern: u16,
    /// Reverse in bit 7, padding type in the low bits. Byte 29
    pub padding_type: u8,
    /// Bit ordering: Byte 30-31
    pub bit_ordering: u16,
    /// Compression type. Byte 32
    pub compression_type: u8,
    /// Compression argument. Byte 33
    pub compression_argument: u8,
    /// One less than the number of times each line is read, so 0 is a single ordinary pass. Byte 40 high nibble
    pub multiple_reading: u8,
    /// What order to read this window's color in: 0 asks for the unit's own ordering, R=1, G=2, B=3. Byte 40 low nibble
    ///
    /// Across a window set this must be all-zero or all-nonzero with no repeats, or SCAN answers `05h-2Ch-02h`
    pub color_ordering: u8,
    /// Byte 41
    pub flags: Flags,
    /// Setup mode. Byte 41 bits 3-1
    ///
    /// Only meaningful when `SetWindowFunction` advertises [`ScanKind::SETUP_2`], which caps it with the setup-mode count in byte 11 of that page
    pub setup_mode: u8,
    /// Byte 42. `SetWindowFunction` reports which of these the unit will do, so a selection is checkable against it with `contains`
    pub scanning_kind: ScanKind,
    /// Byte 43, likewise checkable against `SetWindowFunction`
    pub scanning_mode: ScanMode,
    /// Byte 44, likewise checkable against `SetWindowFunction`
    pub color_interleaving: ColorInterleaving,
    /// Target output value for auto exposure. Byte 45, default 255. Sending 0 sets 255, and GET WINDOW then reports 255
    pub ae_value: u8,
    /// Integration time in units of 10 ns, up to `3FFFFFFh`. Bytes 46-49
    pub exposure: u32,
}

bitflags! {
    /// Byte 41
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Flags: u8 {
        /// Average along the scan line. This is binning across the sensor bar,
        /// not repeated reads of a line, which is `multiple_reading`
        const AVERAGING = 1 << 7;
        const MATRIX    = 1 << 6;
        const FILTER    = 1 << 5;
        // bits 3-1 are the setup mode, kept as a field
        /// Set for positive film, unset for negative
        const POSITIVE  = 1 << 0;
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Composition {
    BilevelBW,
    DitheredBW,
    MultilevelBW,
    BilevelRGB,
    DitheredRGB,
    MultilevelRGB,
    Unknown(u8),
}

impl From<u8> for Composition {
    fn from(value: u8) -> Self {
        match value {
            0x00 => Self::BilevelBW,
            0x01 => Self::DitheredBW,
            0x02 => Self::MultilevelBW,
            0x03 => Self::BilevelRGB,
            0x04 => Self::DitheredRGB,
            0x05 => Self::MultilevelRGB,
            x => Self::Unknown(x),
        }
    }
}

impl From<Composition> for u8 {
    fn from(value: Composition) -> Self {
        match value {
            Composition::BilevelBW => 0x00,
            Composition::DitheredBW => 0x01,
            Composition::MultilevelBW => 0x02,
            Composition::BilevelRGB => 0x03,
            Composition::DitheredRGB => 0x04,
            Composition::MultilevelRGB => 0x05,
            Composition::Unknown(x) => x,
        }
    }
}

/// A descriptor shorter than [`LENGTH`]
#[derive(Debug, thiserror::Error)]
#[error("window descriptor was {got} bytes, need {LENGTH}")]
pub struct TooShort {
    pub got: usize,
}

impl TryFrom<&[u8]> for Window {
    type Error = TooShort;

    /// Takes at least [`LENGTH`] bytes and ignores any beyond it, since a unit
    /// may report a stride larger than the fields it defines
    fn try_from(d: &[u8]) -> Result<Self, TooShort> {
        let d: &[u8; LENGTH] = d
            .get(..LENGTH)
            .and_then(|d| d.try_into().ok())
            .ok_or(TooShort { got: d.len() })?;
        let be16 = |i: usize| u16::from_be_bytes([d[i], d[i + 1]]);
        let be32 = |i: usize| u32::from_be_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);

        Ok(Self {
            id: d[0],
            auto: d[1] & 1 != 0,
            resolution: (be16(2), be16(4)),
            origin: (be32(6), be32(10)),
            size: (be32(14), be32(18)),
            brightness: d[22],
            threshold: d[23],
            contrast: d[24],
            composition: d[25].into(),
            bpp: d[26],
            halftone_pattern: be16(27),
            padding_type: d[29],
            bit_ordering: be16(30),
            compression_type: d[32],
            compression_argument: d[33],
            multiple_reading: d[40] >> 4,
            color_ordering: d[40] & 0x0F,
            flags: Flags::from_bits_truncate(d[41]),
            setup_mode: (d[41] >> 1) & 0b111,
            scanning_kind: ScanKind::from_bits_truncate(d[42]),
            scanning_mode: ScanMode::from_bits_truncate(d[43]),
            color_interleaving: ColorInterleaving::from_bits_truncate(d[44]),
            ae_value: d[45],
            exposure: be32(46),
        })
    }
}

/// The deepest per-channel depth this unit offers, from `SetWindowFunction` byte 10
///
/// `None` where the page advertises nothing, which would leave a descriptor with
/// no legal value for byte 26
pub fn deepest_depth(offered: BitDepth) -> Option<u8> {
    DEPTHS
        .iter()
        .find(|(_, bit)| offered.contains(*bit))
        .map(|(bits, _)| *bits)
}

/// How many planes a composition puts in the stream, per 2-10-6
///
/// Only the two multi-level codes are supported, and [`Window::validate`]
/// refuses the rest
fn planes(composition: Composition) -> Option<usize> {
    match composition {
        Composition::MultilevelBW => Some(1),
        Composition::MultilevelRGB => Some(3),
        _ => None,
    }
}

/// Check the rules that span a whole window set rather than one descriptor
///
/// These are what SCAN refuses rather than SET WINDOW: a descriptor is legal on
/// its own and only the combination is not. Each descriptor is checked by
/// [`Window::validate`] when it is set
pub fn validate_set(windows: &[Window]) -> Result<(), Error> {
    let bad = |op: &'static str, reason: String| Error::Unsupported { op, reason };

    let Some((first, rest)) = windows.split_first() else {
        return Err(bad("window set", "a scan needs at least one window".into()));
    };

    // 2-7: "The default color is valid when only the default color is read"
    if windows.len() > 1 && windows.iter().any(|w| w.channel() == Channel::Default) {
        return Err(bad(
            "window set",
            "the default color cannot be scanned alongside another".into(),
        ));
    }

    // 2-7 calls a disagreement here an invalid combination of windows. A set
    // carries one descriptor per channel so each can hold its own exposure
    let common = |w: &Window| {
        (
            w.resolution.0,
            w.size,
            w.origin,
            w.bpp,
            w.composition,
            w.color_interleaving,
            w.scanning_kind,
            w.scanning_mode,
            w.multiple_reading,
            w.flags,
        )
    };
    if let Some(odd) = rest.iter().find(|w| common(w) != common(first)) {
        return Err(bad(
            "window set",
            format!(
                "window {} differs from window {} in a parameter common to the set",
                odd.id, first.id
            ),
        ));
    }

    // 2-10 byte 40: the read positions are all zero, or all nonzero and
    // distinct. SCAN answers anything else with 05h-2Ch-02h
    let orders: Vec<u8> = windows.iter().map(|w| w.color_ordering).collect();
    if !orders.iter().all(|&o| o == 0) {
        if let Some(w) = windows.iter().find(|w| w.color_ordering == 0) {
            return Err(bad(
                "color ordering",
                format!(
                    "window {} leaves the order to the unit while the rest of the set pins it",
                    w.id
                ),
            ));
        }
        for (n, &order) in orders.iter().enumerate() {
            if orders[..n].contains(&order) {
                return Err(bad(
                    "color ordering",
                    format!("read position {order} is claimed twice"),
                ));
            }
        }
    }

    // 2-10-6's composition says how many color planes the stream carries. It
    // has to match the visible channels in the set. Infrared is not one of
    // them: Nikon Scan scans 09 01 02 03 with the RGB code, four windows to
    // three planes. Getting this wrong gets common error 2, 05h-26h, which no
    // section documents
    let planes = planes(first.composition).ok_or_else(|| {
        bad(
            "image composition",
            format!(
                "{:?} puts no known number of planes in the stream",
                first.composition
            ),
        )
    })?;
    let visible = windows.iter().filter(|w| w.channel().is_color()).count();
    if planes != visible {
        return Err(bad(
            "image composition",
            format!(
                "{:?} carries {planes} plane(s) and this set scans {visible} color channel(s)",
                first.composition
            ),
        ));
    }

    Ok(())
}

/// A descriptor field this unit will not take
fn bad(op: &'static str, reason: String) -> Error {
    Error::Unsupported { op, reason }
}

impl Window {
    /// Which channel this window reads, byte 0
    pub fn channel(&self) -> Channel {
        Channel::from(self.id)
    }

    /// Check this descriptor against what the unit says it will accept
    ///
    /// Per-window rules only. The rules spanning a whole set are
    /// [`validate_set`], since a descriptor can be legal alone and not in company
    ///
    /// A resolution off the unit's ladder is deliberately not refused: 2-10 says
    /// the unit rounds it and reports `01h-37h-00h`, so it is an adjustment
    /// rather than a rejection
    pub fn validate(&self, caps: &Capabilities) -> Result<(), Error> {
        self.check_channel()?;
        self.check_resolution(caps)?;
        self.check_geometry(caps)?;
        self.check_sensor(caps)?;
        self.check_modes(caps)?;
        self.check_pixel(caps)?;
        self.check_setup_mode(caps)?;
        self.check_multiple_reading(caps)?;
        self.check_color_ordering()?;
        self.check_exposure(caps)?;
        self.check_fixed_fields()
    }

    /// Byte 0 names a channel this unit scans, 2-10-4
    fn check_channel(&self) -> Result<(), Error> {
        if matches!(self.channel(), Channel::NeutralGray | Channel::Other(_)) {
            return Err(bad(
                "window identifier",
                format!(
                    "{} is not a scanning color: 2-10-4 defines 0 to 3, and 9 is infrared",
                    self.id
                ),
            ));
        }
        Ok(())
    }

    /// Bytes 2-5 are on a ladder this unit offers
    ///
    /// SET WINDOW ignores Y, so only X is worth bounding. A thumbnail runs off
    /// its own ladder, `Address` bytes 70-73, well below the image one
    fn check_resolution(&self, caps: &Capabilities) -> Result<(), Error> {
        let x = &caps.address.x_axis;
        let dpi = self.resolution.0;
        let range = match self.scanning_kind.contains(ScanKind::THUMBNAIL) {
            true => caps.address.thumbnail_resolution,
            false => x.dpi_range,
        };
        if !range.contains(&dpi) {
            return Err(bad(
                "resolution",
                format!(
                    "{dpi} dpi is outside the {} to {} this unit offers for {}",
                    range.start,
                    range.last,
                    match self.scanning_kind.contains(ScanKind::THUMBNAIL) {
                        true => "thumbnails",
                        false => "images",
                    }
                ),
            ));
        }
        Ok(())
    }

    /// Bytes 6-21 land on the medium, 2-2-2-3
    fn check_geometry(&self, caps: &Capabilities) -> Result<(), Error> {
        let (x, y) = (&caps.address.x_axis, &caps.address.y_axis);

        for (axis, name, origin, size) in [
            (x, 'X', self.origin.0, self.size.0),
            (y, 'Y', self.origin.1, self.size.1),
        ] {
            if !axis.address_range.contains(&origin) {
                return Err(bad(
                    "window origin",
                    format!(
                        "{name} {origin} is outside {} to {}",
                        axis.address_range.start, axis.address_range.last
                    ),
                ));
            }
            if size == 0 {
                return Err(bad("window size", format!("{name} is empty")));
            }
            // The boundary is the adapter's opening, not a limit. The unit's
            // own power-on descriptors exceed it
            if size > axis.boundary {
                warn!(
                    %name, size, opening = axis.boundary,
                    "window reaches past the adapter's opening"
                );
            }
            // 2-2-2-3: an axis with no address range has to be read whole
            if !axis.croppable() && size != axis.boundary {
                return Err(bad(
                    "window size",
                    format!(
                        "{name} cannot be cropped, so it has to be exactly {}",
                        axis.boundary
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The main-scanning axis against the sensor itself
    ///
    /// The one width that really is a limit: everything else 2-2-2-3 reports is
    /// the loaded adapter's opening, which a window may exceed
    fn check_sensor(&self, caps: &Capabilities) -> Result<(), Error> {
        let ccd = u32::from(caps.address.ccd_pixels);
        if self.origin.0 + self.size.0 > ccd {
            return Err(bad(
                "window size",
                format!(
                    "X {} from {} runs past the {ccd} pixel sensor",
                    self.size.0, self.origin.0
                ),
            ));
        }

        // Multi-line reads walk the bar in Line Gap Count blocks. A width that
        // is not a whole number of them splits a block and mis-orders columns
        let block = u32::from(caps.address.line_gap);
        if self
            .color_interleaving
            .contains(ColorInterleaving::MULTILINE_SIMULTANEOUS)
            && block != 0
            && !self.size.0.is_multiple_of(block)
        {
            warn!(
                width = self.size.0,
                block, "width is not a whole number of line-gap blocks"
            );
        }
        Ok(())
    }

    /// Bytes 42-44 are each a subset of what `SetWindowFunction` advertises
    ///
    /// Comparing raw bits keeps one loop over three unrelated flag types
    fn check_modes(&self, caps: &Capabilities) -> Result<(), Error> {
        let f = &caps.set_window;
        for (chosen, offered, op) in [
            (self.scanning_kind.bits(), f.kind.bits(), "scanning kind"),
            (self.scanning_mode.bits(), f.mode.bits(), "scanning mode"),
            (
                self.color_interleaving.bits(),
                f.interleaving.bits(),
                "color interleaving",
            ),
        ] {
            if chosen == 0 || chosen & !offered != 0 {
                return Err(bad(
                    op,
                    format!("asked for {chosen:#04x} of the {offered:#04x} this unit offers"),
                ));
            }
        }
        Ok(())
    }

    /// Byte 26 is a depth this unit offers and byte 25 a composition 2-10-6
    /// marks supported
    fn check_pixel(&self, caps: &Capabilities) -> Result<(), Error> {
        let f = &caps.set_window;
        let Some((_, depth)) = DEPTHS.iter().find(|(n, _)| *n == self.bpp) else {
            return Err(bad(
                "pixel composition",
                format!("{} bits is not a depth 2-2-2-4 defines", self.bpp),
            ));
        };
        if !f.depth.contains(*depth) {
            return Err(bad(
                "pixel composition",
                format!("this unit does not offer {} bits", self.bpp),
            ));
        }

        // 2-10-6 marks only the two multi-level codes supported, in both specs
        if !matches!(
            self.composition,
            Composition::MultilevelBW | Composition::MultilevelRGB
        ) {
            return Err(bad(
                "image composition",
                format!(
                    "2-10-6 supports neither {:?} nor anything else it lists past the two multi-level codes",
                    self.composition
                ),
            ));
        }
        Ok(())
    }

    /// Byte 41 bits 3-1 only mean something where setup scanning 2 is offered
    fn check_setup_mode(&self, caps: &Capabilities) -> Result<(), Error> {
        let f = &caps.set_window;
        if self.setup_mode != 0 {
            if !f.kind.contains(ScanKind::SETUP_2) {
                return Err(bad(
                    "setup mode",
                    "this unit does not offer setup scanning 2, which is what makes byte 41 bits 3-1 mean anything".into(),
                ));
            }
            if self.setup_mode > f.setup_modes {
                return Err(bad(
                    "setup mode",
                    format!(
                        "{} is past the {} this unit offers",
                        self.setup_mode, f.setup_modes
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Byte 40's high nibble is a repeat count this unit will honor
    fn check_multiple_reading(&self, caps: &Capabilities) -> Result<(), Error> {
        let f = &caps.set_window;
        if self.multiple_reading != 0 {
            if self.multiple_reading > 0x0F {
                return Err(bad(
                    "multiple reading",
                    format!(
                        "{} does not fit the nibble byte 40 gives it",
                        self.multiple_reading
                    ),
                ));
            }
            if !f.mode.contains(ScanMode::MULTI_READING) {
                return Err(bad(
                    "multiple reading",
                    "this unit reads each line once".into(),
                ));
            }
            // Byte 43 carries the mode bit in every capture that sets byte 40,
            // so a count on its own is half a request
            if !self.scanning_mode.contains(ScanMode::MULTI_READING) {
                return Err(bad(
                    "multiple reading",
                    format!(
                        "{} readings needs {:?} in the scanning mode, which is {:?}",
                        self.multiple_reading + 1,
                        ScanMode::MULTI_READING,
                        self.scanning_mode
                    ),
                ));
            }
        }
        Ok(())
    }

    /// A read position, 0 meaning the unit's own order
    ///
    /// `SetWindowFunction` bytes 8-9 also pin which component may sit at each
    /// position, but nothing states how a window identifier maps to a
    /// component, so that half is left to SCAN
    fn check_color_ordering(&self) -> Result<(), Error> {
        if self.color_ordering > 3 {
            return Err(bad(
                "color ordering",
                format!("{} is not a read position", self.color_ordering),
            ));
        }
        Ok(())
    }

    /// Bytes 46-49 are in the range `SetWindowFunction` reports
    ///
    /// 0 hands the choice to the unit, which then reports what it picked
    fn check_exposure(&self, caps: &Capabilities) -> Result<(), Error> {
        let f = &caps.set_window;
        if self.exposure != 0 && !f.exposure.contains(&self.exposure) {
            return Err(bad(
                "exposure",
                format!(
                    "{} is outside the {} to {} ten-nanosecond units this unit offers",
                    self.exposure, f.exposure.start, f.exposure.last
                ),
            ));
        }
        Ok(())
    }

    /// The fields 2-10 pins to 0 on both units
    fn check_fixed_fields(&self) -> Result<(), Error> {
        for (value, op) in [
            (u16::from(self.padding_type), "padding type"),
            (u16::from(self.compression_type), "compression type"),
            (u16::from(self.compression_argument), "compression argument"),
            (self.bit_ordering, "bit ordering"),
        ] {
            if value != 0 {
                return Err(bad(op, format!("2-10 defines this as 0, not {value}")));
            }
        }
        Ok(())
    }

    /// Write one descriptor
    pub fn to_bytes(&self) -> [u8; LENGTH] {
        let mut d = [0u8; LENGTH];
        let be16 = |d: &mut [u8; LENGTH], i: usize, v: u16| {
            d[i..i + 2].copy_from_slice(&v.to_be_bytes());
        };

        d[0] = self.id;
        d[1] = u8::from(self.auto);
        be16(&mut d, 2, self.resolution.0);
        be16(&mut d, 4, self.resolution.1);
        d[6..10].copy_from_slice(&self.origin.0.to_be_bytes());
        d[10..14].copy_from_slice(&self.origin.1.to_be_bytes());
        d[14..18].copy_from_slice(&self.size.0.to_be_bytes());
        d[18..22].copy_from_slice(&self.size.1.to_be_bytes());
        d[22] = self.brightness;
        d[23] = self.threshold;
        d[24] = self.contrast;
        d[25] = self.composition.into();
        d[26] = self.bpp;
        be16(&mut d, 27, self.halftone_pattern);
        d[29] = self.padding_type;
        be16(&mut d, 30, self.bit_ordering);
        d[32] = self.compression_type;
        d[33] = self.compression_argument;
        // 34-39 reserved
        d[40] = (self.multiple_reading << 4) | (self.color_ordering & 0x0F);
        d[41] = self.flags.bits() | ((self.setup_mode & 0b111) << 1);
        d[42] = self.scanning_kind.bits();
        d[43] = self.scanning_mode.bits();
        d[44] = self.color_interleaving.bits();
        d[45] = self.ae_value;
        d[46..50].copy_from_slice(&self.exposure.to_be_bytes());
        d
    }
}

/// Both headers are eight bytes, though they do not agree on what is in them
pub const HEADER: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Header tacked on to the front of an incoming GET WINDOW payload
/// Table 2-10-2
pub struct GetWindowHeader {
    /// Bytes 0,1. Counts what follows it, so the whole reply is two more
    pub data_length: u16,
    /// Bytes 6,7. A unit may report a stride longer than 2-10-3 defines
    pub descriptor_length: u16,
}

impl GetWindowHeader {
    /// Read the header and return the rest of the slice
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, &[u8]), TooShort> {
        if bytes.len() < HEADER {
            return Err(TooShort { got: bytes.len() });
        }
        let data_length = u16::from_be_bytes([bytes[0], bytes[1]]);
        let descriptor_length = u16::from_be_bytes([bytes[6], bytes[7]]);
        Ok((
            Self {
                data_length,
                descriptor_length,
            },
            &bytes[HEADER..],
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Header tacked on to the front of an outgoing SET WINDOW payload
/// Table 2-9-2
pub struct SetWindowHeader {
    /// Bytes 6,7. Sending 50 against a unit claiming more is fine: 2-9 note 3
    /// leaves the rest unchanged, and Nikon Scan sends 50
    pub descriptor_length: u16,
}

impl SetWindowHeader {
    /// Pack to bytes to send
    pub fn to_bytes(&self) -> [u8; HEADER] {
        let mut bytes = [0u8; HEADER];
        bytes[6..8].copy_from_slice(&self.descriptor_length.to_be_bytes());
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The five descriptors an LS-9000 reports at power-on, header stripped.
    /// Real bytes: full frame, 4000 dpi, per-channel exposures, and an
    /// infrared window whose identifier appears in neither spec
    const LS9000: &[u8] = &[
        // id 0, default color
        0x00, 0x00, 0x0F, 0xA0, 0x0F, 0xA0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x27, 0x10, 0x00, 0x00, 0x36, 0x24, 0x00, 0x00, 0x00, 0x02, 0x10, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x02, 0x02,
        0xFF, 0x00, 0x00, 0xC6, 0x9A, // id 1, R
        0x01, 0x00, 0x0F, 0xA0, 0x0F, 0xA0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x27, 0x10, 0x00, 0x00, 0x36, 0x24, 0x00, 0x00, 0x00, 0x02, 0x10, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x02, 0x02,
        0xFF, 0x00, 0x01, 0x15, 0xD5, // id 9, infrared
        0x09, 0x00, 0x0F, 0xA0, 0x0F, 0xA0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x27, 0x10, 0x00, 0x00, 0x36, 0x24, 0x00, 0x00, 0x00, 0x02, 0x10, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x02, 0x02,
        0xFF, 0x00, 0x01, 0x6B, 0x4C,
    ];

    fn nth(n: usize) -> Window {
        Window::try_from(&LS9000[n * LENGTH..]).expect("descriptor")
    }

    /// Three descriptors Nikon Scan actually sent, lifted out of the capture
    /// corpus with the 8-byte SET WINDOW header stripped. The first two are the
    /// same 4000 dpi scan in the two sensor modes; the third is a 16x prescan
    mod captured {
        /// `full_session_cold_start`, normal CCD mode
        pub const MULTI_LINE: &[u8] = &[
            0x01, 0x00, 0x0F, 0xA0, 0x0F, 0xA0, 0x00, 0x00, 0x02, 0x06, 0x00, 0x00, 0x29, 0x10,
            0x00, 0x00, 0x23, 0x04, 0x00, 0x00, 0x1A, 0x28, 0x00, 0x00, 0x00, 0x05, 0x10, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x81,
            0x01, 0x02, 0x40, 0xFF, 0x00, 0x07, 0xAB, 0xDD,
        ];

        /// `singleline_ccd`, the mode Nikon Scan calls Super Fine
        pub const SINGLE_LINE: &[u8] = &[
            0x09, 0x00, 0x0F, 0xA0, 0x0F, 0xA0, 0x00, 0x00, 0x02, 0x06, 0x00, 0x00, 0x48, 0xF0,
            0x00, 0x00, 0x23, 0x04, 0x00, 0x00, 0x33, 0x78, 0x00, 0x00, 0x00, 0x05, 0x10, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x81,
            0x01, 0x02, 0x02, 0xFF, 0x00, 0x08, 0x39, 0xDE,
        ];

        /// `16x_multisample`, the 666 dpi pass before the scan
        pub const PRESCAN_16X: &[u8] = &[
            0x01, 0x00, 0x02, 0x9A, 0x01, 0x4D, 0x00, 0x00, 0x02, 0x06, 0x00, 0x00, 0x0E, 0xA0,
            0x00, 0x00, 0x23, 0x04, 0x00, 0x00, 0x33, 0x78, 0x00, 0x00, 0x00, 0x05, 0x10, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x01,
            0x01, 0x14, 0x40, 0xFF, 0x00, 0x07, 0x55, 0xAB,
        ];
    }

    /// The sensor mode is byte 44 and nothing else: two scans at the same
    /// resolution, same quality, same averaging, differing in that one field
    #[test]
    fn the_two_sensor_modes_differ_only_in_the_interleaving() {
        let multi = Window::try_from(captured::MULTI_LINE).expect("descriptor");
        let single = Window::try_from(captured::SINGLE_LINE).expect("descriptor");

        for w in [&multi, &single] {
            assert_eq!(w.resolution, (4000, 4000));
            assert_eq!(w.flags, Flags::AVERAGING | Flags::POSITIVE);
            assert_eq!(w.scanning_kind, ScanKind::IMAGE);
            assert_eq!(w.scanning_mode, ScanMode::NORMAL_QUALITY);
            assert_eq!(w.multiple_reading, 0);
            // Every window is one color, yet Nikon Scan asks for the RGB code
            assert_eq!(w.composition, Composition::MultilevelRGB);
        }

        assert_eq!(
            multi.color_interleaving,
            ColorInterleaving::MULTILINE_SIMULTANEOUS
        );
        assert_eq!(
            single.color_interleaving,
            ColorInterleaving::LINE_WITHOUT_DISTANCE
        );
    }

    /// Multisampling is a count in byte 40 and a mode bit in byte 43, together
    #[test]
    fn sixteen_times_is_fifteen_extra_reads_and_a_mode_bit() {
        let w = Window::try_from(captured::PRESCAN_16X).expect("descriptor");
        assert_eq!(w.multiple_reading, 15);
        assert_eq!(
            w.scanning_mode,
            ScanMode::HIGH_SPEED | ScanMode::MULTI_READING
        );
        // High speed is what unsets averaging, and it is the preview that asks
        // for high speed. Resolution never selects it on its own
        assert_eq!(w.flags, Flags::POSITIVE);
        // Y resolution is sent as something else entirely, and ignored
        assert_eq!(w.resolution, (666, 333));
    }

    #[test]
    fn a_power_on_descriptor_reads_the_whole_frame_at_full_resolution() {
        let w = nth(0);
        assert_eq!(w.resolution, (4000, 4000));
        assert_eq!(w.origin, (0, 0));
        // 10000 x 13860 at 4000 dpi is the 6x9 frame, matching Address's y boundary
        assert_eq!(w.size, (10000, 13860));
        assert_eq!(w.bpp, 16);
        assert_eq!(w.composition, Composition::MultilevelBW);
        assert_eq!(w.ae_value, 255);
        // The same flags SetWindowFunction advertises, so a window can be
        // checked against it
        assert_eq!(w.scanning_kind, ScanKind::IMAGE);
        assert_eq!(w.scanning_mode, ScanMode::NORMAL_QUALITY);
        assert_eq!(
            w.color_interleaving,
            ColorInterleaving::LINE_WITHOUT_DISTANCE
        );
        // Byte 41 is 01h at power-on: positive film, no averaging
        assert!(w.flags.contains(Flags::POSITIVE));
        assert!(!w.flags.contains(Flags::AVERAGING));
    }

    /// Exposures are per channel, and infrared needs nearly twice green's
    #[test]
    fn each_window_carries_its_own_exposure() {
        assert_eq!((nth(0).id, nth(0).exposure), (0, 50842));
        assert_eq!((nth(1).id, nth(1).exposure), (1, 71125));
        assert_eq!((nth(2).id, nth(2).exposure), (9, 93004));
    }

    /// Whatever we send has to survive the trip, or SET WINDOW will not match
    /// what GET WINDOW reported
    #[test]
    fn descriptors_round_trip_byte_for_byte() {
        for n in 0..3 {
            let bytes = &LS9000[n * LENGTH..(n + 1) * LENGTH];
            assert_eq!(nth(n).to_bytes(), bytes, "descriptor {n}");
        }
    }

    #[test]
    fn a_short_descriptor_is_refused() {
        assert!(Window::try_from(&LS9000[..LENGTH - 1]).is_err());
    }

    use crate::protocol::caps::{
        Page, address::Address, identity::Identity, other::Features, set_window::SetWindowFunction,
    };

    /// Capabilities from an Address page, with the other pages left minimal
    fn caps_from(page: Vec<u8>) -> Capabilities {
        let address = Address::try_from(&Page::new(Address::PAGE_CODE, page).unwrap()).unwrap();

        let mut d = vec![0u8; 28];
        d[1] = SetWindowFunction::PAGE_CODE;
        d[3] = 24;
        d[4] = 0x1B; // image, thumbnail, setup 2, histogram
        d[5] = 0x16; // normal quality, high speed, multi reading
        d[6] = 0x42; // line ordering and multi-line
        d[10] = 0x20; // 16 bit
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

    fn set(ids: &[u8], composition: Composition) -> Vec<Window> {
        ids.iter()
            .map(|&id| {
                let mut w = Window::try_from(&[0u8; LENGTH][..]).unwrap();
                w.id = id;
                w.composition = composition;
                w
            })
            .collect()
    }

    /// What the hardware refused with common error 2: three channels declared
    /// as a one-plane composition
    #[test]
    fn the_composition_has_to_carry_one_plane_per_channel() {
        assert!(validate_set(&set(&[1], Composition::MultilevelBW)).is_ok());
        assert!(validate_set(&set(&[1, 2, 3], Composition::MultilevelRGB)).is_ok());
        assert!(validate_set(&set(&[1, 2, 3], Composition::MultilevelBW)).is_err());
        assert!(validate_set(&set(&[1], Composition::MultilevelRGB)).is_err());
    }

    /// Infrared is not a color plane. Nikon Scan sends four windows against the
    /// three-plane code whenever Digital ICE is on
    #[test]
    fn infrared_does_not_count_towards_the_planes() {
        assert!(
            validate_set(&set(
                &[Channel::Infrared.id(), 1, 2, 3],
                Composition::MultilevelRGB
            ))
            .is_ok()
        );
        assert!(
            validate_set(&set(
                &[Channel::Infrared.id(), 1],
                Composition::MultilevelBW
            ))
            .is_ok()
        );
        assert!(
            validate_set(&set(
                &[Channel::Infrared.id(), 1, 2],
                Composition::MultilevelRGB
            ))
            .is_err()
        );
    }

    /// Address publishes a separate resolution range for thumbnails, far below the
    /// image ladder, and a thumbnail has to be checked against that one
    #[test]
    fn a_thumbnail_is_checked_against_the_thumbnail_ladder() {
        let mut p = vec![0u8; 91];
        p[1] = Address::PAGE_CODE;
        p[3] = 87;
        p[16] = 0x42;
        p[18..20].copy_from_slice(&4000u16.to_be_bytes());
        p[20..22].copy_from_slice(&4000u16.to_be_bytes());
        p[22..24].copy_from_slice(&666u16.to_be_bytes());
        p[24..28].copy_from_slice(&8963u32.to_be_bytes()); // X address max
        p[36..40].copy_from_slice(&8964u32.to_be_bytes()); // X boundary
        p[46..50].copy_from_slice(&34644u32.to_be_bytes()); // Y address max
        p[58..62].copy_from_slice(&13176u32.to_be_bytes()); // Y boundary
        p[70..72].copy_from_slice(&83u16.to_be_bytes());
        p[72..74].copy_from_slice(&83u16.to_be_bytes());
        p[83..85].copy_from_slice(&10000u16.to_be_bytes()); // CCD pixels
        p[85] = 12;
        p[86] = 3;
        let caps = caps_from(p);

        let mut w = Window::try_from(&[0u8; LENGTH][..]).unwrap();
        w.id = 1;
        w.composition = Composition::MultilevelBW;
        w.bpp = 16;
        w.size = (1000, 1000);
        w.scanning_mode = ScanMode::NORMAL_QUALITY;
        w.color_interleaving = ColorInterleaving::LINE_WITHOUT_DISTANCE;

        // 83 dpi is off the image ladder but is the only thumbnail resolution
        w.scanning_kind = ScanKind::IMAGE;
        w.resolution = (83, 83);
        assert!(w.validate(&caps).is_err());

        w.scanning_kind = ScanKind::THUMBNAIL;
        assert!(w.validate(&caps).is_ok());

        // And an image resolution is not a thumbnail one
        w.resolution = (4000, 4000);
        assert!(w.validate(&caps).is_err());
    }

    /// 2-7: "The default color is valid when only the default color is read"
    #[test]
    fn the_default_color_scans_alone_or_not_at_all() {
        assert!(validate_set(&set(&[Channel::Default.id()], Composition::MultilevelBW)).is_ok());
        assert!(
            validate_set(&set(
                &[Channel::Default.id(), 1, 3],
                Composition::MultilevelRGB
            ))
            .is_err()
        );
    }

    /// 2-10 byte 40, which SCAN answers with 05h-2Ch-02h
    #[test]
    fn a_window_set_orders_every_color_or_none() {
        let ordered = |orders: &[u8]| {
            let mut windows = set(&[1, 2, 3], Composition::MultilevelRGB);
            for (w, &o) in windows.iter_mut().zip(orders) {
                w.color_ordering = o;
            }
            validate_set(&windows)
        };
        assert!(ordered(&[0, 0, 0]).is_ok());
        assert!(ordered(&[1, 2, 3]).is_ok());
        assert!(ordered(&[1, 0, 3]).is_err());
        assert!(ordered(&[1, 2, 2]).is_err());
    }

    /// 2-7 calls a disagreement an invalid combination of windows, but the
    /// per-channel exposure is exactly what a set is meant to differ in
    #[test]
    fn a_set_agrees_on_everything_but_its_exposures() {
        let mut windows = set(&[1, 2, 3], Composition::MultilevelRGB);
        windows[2].exposure = 71125;
        assert!(validate_set(&windows).is_ok());

        windows[2].size.0 = 9000;
        assert!(validate_set(&windows).is_err());
    }

    #[test]
    fn an_empty_window_set_is_refused() {
        assert!(validate_set(&[]).is_err());
    }
}
