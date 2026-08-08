//! "Address information", 2-2-2-3 in both specs
//! Probably the most complex page with all of the major details we will need

use super::{Error, Page};
use bitflags::bitflags;
use std::range::RangeInclusive;

#[derive(Debug, Clone)]
pub struct Address {
    /// Declared page length; the page is 4 + this. Byte 3
    pub page_length: u8,
    /// SCSI data transfer function bitflags. Byte 4
    pub transfer: Transfer,
    /// Window descriptor block length in bytes. Bytes 5,6
    pub window_descriptor_len: u16,
    /// Length of SET PARAMETER command in bytes. Bytes 7,8
    pub set_parameter_len: u16,
    /// SCSI buffer size in bytes, None being unlimited. Bytes 9,10
    pub scsi_buffer: Option<u16>,
    /// Image buffer size in KB. Bytes 11,12
    pub image_buffer_kb: u16,
    /// Number of units that can be attached simultaneously. Byte 13
    pub units_attachable: u8,
    /// ID of the attached adapter. None when none is attached, or one the unit does not recognize. Byte 14
    pub adapter_id: Option<u8>,
    /// ID of the attached holder, same convention. Byte 15
    pub holder_id: Option<u8>,
    /// Coordinate base information. Byte 16 bits 2-7
    pub coordinate_base: CoordinateBase,
    /// Pitch rule. Byte 16 bits 0-1, with the line gap from byte 85 folded in
    pub pitch_rule: PitchRule,
    /// The kind of addressing that is supported. Byte 17
    pub addressing_kind: AddressingKind,
    /// Details of the X-axis. Bytes 18-39
    pub x_axis: Axis,
    /// Details of the Y-axis. Bytes 40-61
    pub y_axis: Axis,
    /// "Another world" addresses (?? wtf nikon)
    /// We think this is travel along a strip beyond what one window covers.
    /// None here will mean this adapter has no such region. Bytes 62-69
    pub y_outside: Option<RangeInclusive<u32>>,
    /// Valid resolutions for the thumbnail scanning. Bytes 70-73
    pub thumbnail_resolution: RangeInclusive<u16>,
    /// Maximum number of frames that can be scanned. Byte 74
    ///
    /// Not a maximum: it reads 0 through most of a Nikon Scan session that
    /// writes four-rectangle tables, and 1 elsewhere in the same session. Do
    /// not size a frame table against it
    pub max_frames: u8,
    /// The number of frames that are currently set. Byte 75
    ///
    /// Moves with byte 74 rather than with the table, so it is no more a count
    /// of the loaded frames than that one is a maximum
    pub loaded_frames: u8,
    /// Range of valid focus addresses. Bytes 76-79
    pub focus_range: RangeInclusive<u16>,
    /// Maximum lamp warm-up time
    /// Bytes 80,81
    pub lamp_warmup_time: u16,
    /// A/D bit depth. Byte 82
    pub bit_depth: u8,
    /// Number of pixels in the CCD (maximum across colors). Bytes 83,84
    pub ccd_pixels: u16,
    /// Distance between CCD lines. Byte 85
    pub line_gap: u8,
    /// Number of lines on the CCD, normalized: 0 on the wire means 3. Byte 86
    pub lines: u8,
}

/// Details of each axis
/// byte 18 for X and byte 40 for Y
#[derive(Debug, Clone)]
pub struct Axis {
    /// Native, optical resolution in DPI
    pub optical_dpi: u16,
    /// DPI range settable
    pub dpi_range: RangeInclusive<u16>,
    /// Window descriptor offset address range
    pub address_range: RangeInclusive<u32>,
    /// Address offset for the first image
    pub address_offset: u32,
    /// Max window width in this axis
    pub boundary: u32,
}

impl Axis {
    /// Equal ends mean no arbitrary range on this axis: the caller has to use the full [`boundary`](Self::boundary) width
    pub fn croppable(&self) -> bool {
        self.address_range.start != self.address_range.last
    }
}

bitflags! {
    /// Byte 16 bits 2-7. Bits 0-1 are the resolution type and live in
    /// [`PitchRule`]; bit 5 is reserved and bit 7 is the extension bit
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CoordinateBase: u8 {
        /// The main-scanning origin is at the right end of the medium
        const X_ORIGIN_REVERSED = 1 << 2;
        /// The sub-scanning origin is at the bottom end of the medium
        const Y_ORIGIN_REVERSED = 1 << 3;
        /// Thumbnails are stored last frame to first, rather than first to last
        const THUMBNAIL_REVERSED = 1 << 4;
        /// The unit publishes frame rectangles
        /// This implies the "additional coordinate information" pages exist, of
        /// which `Frames` is the first
        const FRAME_RECTS = 1 << 6;
    }
}

/// Byte 16 bits 0-1 with the line gap count folded in
/// "Pitch" here is the optical_dpi / requested dpi (from GET WINDOW)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PitchRule {
    /// Any resolution in range
    Continuous, // 0
    /// Only exact pitches
    EachPitch, // 1
    /// Pitch must divide the line gap count
    DivisorsOf(u8), // 2
    /// Pitch must be 1 or even
    OnePlusEven, // 3
}

impl PitchRule {
    /// Snap `optical / asked` to a pitch the unit will really scan at
    ///
    /// 2-10 rounds a resolution off the ladder down to the next one it has and
    /// reports `01h-37h-00h`, so this is the pitch that sizes an image, not the
    /// ratio itself
    pub fn snap(self, pitch: u32) -> u32 {
        let pitch = pitch.max(1);
        match self {
            Self::Continuous | Self::EachPitch => pitch,
            // Largest divisor of the gap that is no finer than asked
            Self::DivisorsOf(gap) => (1..=pitch)
                .rev()
                .find(|p| u32::from(gap) % p == 0)
                .unwrap_or(1),
            Self::OnePlusEven if pitch == 1 => 1,
            Self::OnePlusEven => pitch & !1,
        }
    }
}

bitflags! {
    /// Byte 4. The LS-5000 and LS-9000 give bits 0 and 3 different meanings,
    /// but each zeroes what it does not use, so the union parses on both
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Transfer: u8 {
        /// LS-5000 calls this microcode downloading, LS-9000 calls it unused
        const MICROCODE      = 1 << 0;
        /// READ must be in units of [line bytes x colors]
        const READ_LINE_COLS = 1 << 1;
        /// READ must be in units of [line bytes]
        const READ_LINE      = 1 << 2;
        /// Thumbnail READ in units of [frame bytes x colors]
        const THUMB_FRAME_COLS = 1 << 3;
    }
}

bitflags! {
    /// Byte 17. What a SET WINDOW address refers to on this unit, and what the
    /// unit can be told to move
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AddressingKind: u8 {
        /// SET WINDOW addresses are positions on the medium itself
        const ADDR_MEDIUM = 1 << 0;
        /// SET WINDOW addresses are positions of the transport
        const ADDR_MECHANISM = 1 << 1;
        /// A scanning range may not span two or more frames
        const SINGLE_FRAME_ONLY = 1 << 2;
        /// The medium's position can be commanded
        const MEDIUM_MOVABLE = 1 << 4;
        /// The transport's position can be commanded
        const MECHANISM_MOVABLE = 1 << 5;
    }
}

impl Address {
    pub const PAGE_CODE: u8 = 0xC1;
}

impl TryFrom<&Page> for Address {
    type Error = Error;

    fn try_from(page: &Page) -> Result<Self, Self::Error> {
        let page_length = page.u8(3)?;
        let transfer = Transfer::from_bits_truncate(page.u8(4)?);
        let window_descriptor_len = page.be16(5)?;
        let set_parameter_len = page.be16(7)?;
        let scsi_buffer = page.opt_be16(9)?;
        let image_buffer_kb = page.be16(11)?;
        let units_attachable = page.u8(13)?;
        let adapter_id = page.opt_u8(14)?;
        let holder_id = page.opt_u8(15)?;
        let line_gap = page.u8(85)?;
        let b16 = page.u8(16)?;
        let coordinate_base = CoordinateBase::from_bits_truncate(b16);
        let pitch_rule = match b16 & 0b11 {
            0 => PitchRule::Continuous,
            1 => PitchRule::EachPitch,
            2 => PitchRule::DivisorsOf(line_gap),
            3 => PitchRule::OnePlusEven,
            _ => unreachable!("masked to two bits"),
        };
        let addressing_kind = AddressingKind::from_bits_truncate(page.u8(17)?);
        let x_axis = Axis {
            optical_dpi: page.be16(18)?,
            dpi_range: ((page.be16(22)?)..=(page.be16(20)?)).into(),
            address_range: ((page.be32(28)?)..=(page.be32(24)?)).into(),
            address_offset: page.be32(32)?,
            boundary: page.be32(36)?,
        };
        let y_axis = Axis {
            optical_dpi: page.be16(40)?,
            dpi_range: ((page.be16(44)?)..=(page.be16(42)?)).into(),
            address_range: ((page.be32(50)?)..=(page.be32(46)?)).into(),
            address_offset: page.be32(54)?,
            boundary: page.be32(58)?,
        };

        let y_outside_max = page.be32(62)?;
        let y_outside_min = page.be32(66)?;

        let y_outside = if y_outside_min == 0 && y_outside_max == 0 {
            None
        } else {
            Some(((y_outside_min)..=(y_outside_max)).into())
        };

        let thumbnail_resolution = ((page.be16(72)?)..=(page.be16(70)?)).into();
        let max_frames = page.u8(74)?;
        let loaded_frames = page.u8(75)?;
        let focus_range = ((page.be16(76)?)..=(page.be16(78)?)).into();
        let lamp_warmup_time = page.be16(80)?;
        let bit_depth = page.u8(82)?;
        let ccd_pixels = page.be16(83)?;
        let lines = match page.u8(86)? {
            0 => 3,
            n => n,
        };

        Ok(Self {
            page_length,
            transfer,
            window_descriptor_len,
            set_parameter_len,
            scsi_buffer,
            image_buffer_kb,
            units_attachable,
            adapter_id,
            holder_id,
            coordinate_base,
            pitch_rule,
            addressing_kind,
            x_axis,
            y_axis,
            y_outside,
            thumbnail_resolution,
            max_frames,
            loaded_frames,
            focus_range,
            lamp_warmup_time,
            bit_depth,
            ccd_pixels,
            line_gap,
            lines,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be16(p: &mut [u8], i: usize, v: u16) {
        p[i..i + 2].copy_from_slice(&v.to_be_bytes());
    }

    fn be32(p: &mut [u8], i: usize, v: u32) {
        p[i..i + 4].copy_from_slice(&v.to_be_bytes());
    }

    /// 2-2-2-3's bracketed values for the LS-9000. Holder-dependent fields have
    /// no bracketed value and are set to something plausible
    fn ls9000() -> Vec<u8> {
        let mut p = vec![0u8; 91];
        p[1] = Address::PAGE_CODE;
        p[3] = 87;
        p[4] = 0x01;
        be16(&mut p, 5, 58);
        p[16] = 0x42;
        p[17] = 0x12;
        be16(&mut p, 22, 666);
        be32(&mut p, 24, 9999);
        be32(&mut p, 36, 10000);
        be16(&mut p, 44, 333);
        be32(&mut p, 46, 9999);
        p[85] = 12;
        p[86] = 3;
        p
    }

    /// The same page on an LS-5000 with a strip adapter
    fn ls5000() -> Vec<u8> {
        let mut p = ls9000();
        p[3] = 83;
        p[4] = 0x03;
        be16(&mut p, 5, 61);
        p[16] = 0x03;
        p[17] = 0x22;
        be16(&mut p, 22, 90);
        be32(&mut p, 24, 0); // equal to the minimum: no X cropping
        be32(&mut p, 62, 5959);
        p[85] = 1;
        p[86] = 2;
        p
    }

    fn parse(bytes: &[u8]) -> Address {
        let page = Page::new(Address::PAGE_CODE, bytes.to_vec()).expect("page");
        Address::try_from(&page).expect("address")
    }

    /// One parser, and every difference falls out of an advertised field
    #[test]
    fn the_families_differ_only_in_what_they_advertise() {
        let nine = parse(&ls9000());
        let five = parse(&ls5000());

        assert_eq!(nine.pitch_rule, PitchRule::DivisorsOf(12));
        assert_eq!(five.pitch_rule, PitchRule::OnePlusEven);
        assert_eq!((nine.lines, five.lines), (3, 2));

        // The LS-5000 cannot crop in X, so a caller must take the full boundary
        assert!(nine.x_axis.croppable());
        assert!(!five.x_axis.croppable());

        // Only the LS-9000 publishes frame rectangles
        assert!(nine.coordinate_base.contains(CoordinateBase::FRAME_RECTS));
        assert!(!five.coordinate_base.contains(CoordinateBase::FRAME_RECTS));

        // Only the LS-5000 constrains READ to whole lines
        assert!(five.transfer.contains(Transfer::READ_LINE_COLS));
        assert!(!nine.transfer.contains(Transfer::READ_LINE_COLS));

        // Only strip adapters can travel past one window in Y
        assert_eq!(nine.y_outside, None);
        assert!(five.y_outside.is_some());
    }

    /// 2-2-2-3 byte 86: "When 0 is set ... '3 lines' is set"
    #[test]
    fn a_zero_ccd_line_count_means_three() {
        let mut p = ls9000();
        p[86] = 0;
        assert_eq!(parse(&p).lines, 3);
    }

    /// Byte 16 varies with what is loaded, not with the model: the IA-20 flips
    /// thumbnail order, and an FH3 inserted backwards flips both origins
    #[test]
    fn coordinate_base_tracks_the_loaded_adapter() {
        let mut p = ls5000();

        p[16] = 0x13;
        let ia20 = parse(&p).coordinate_base;
        assert!(ia20.contains(CoordinateBase::THUMBNAIL_REVERSED));

        p[16] = 0x0F;
        let reversed = parse(&p).coordinate_base;
        assert!(reversed.contains(CoordinateBase::X_ORIGIN_REVERSED));
        assert!(reversed.contains(CoordinateBase::Y_ORIGIN_REVERSED));
    }
}
