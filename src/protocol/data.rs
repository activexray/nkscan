//! READ and SEND data types. Section 2-11

use super::{caps::other::DataTypes, sense::Coop};
use crate::error::Error;
use bitflags::bitflags;

/// The header 2-11-6 puts in front of every type but `DataType::Image`
pub const HEADER: usize = 6;

/// One row of table 2-11-2, which both specs give identically for every code
/// they both define
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    /// Byte 2 of a READ or SEND
    pub code: u8,
    /// Bytes per element. `None` where the caller picks from 1, 2 or 4, or where
    /// neither spec documents one because neither unit implements the type.
    /// Not ours to invent: 2-11 answers common error 1 when the qualifier's low
    /// byte disagrees with this column
    pub width: Option<u8>,
    /// Elements, where 2-11-2 fixes a number rather than saying Variable
    pub count: Option<u32>,
    /// Whether the 6-byte data header precedes the valid data
    pub header: bool,
    /// The `Features` bit that says a unit will READ it, where the page has one
    pub read: Option<DataTypes>,
    /// The `Features` bit that says a unit will SEND it
    pub write: Option<DataTypes>,
}

/// Table 2-11-2 in full, so a type either unit implements can be named even
/// where ours does not. Support is never baked in here: `Features` decides it at
/// runtime through [`Row::read`] and [`Row::write`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Image,
    HalftoneMask,
    Lut,
    Histogram,
    MaxValue,
    Matrix,
    Filter,
    Shading,
    DarkVoltage,
    Magnetic,
    Cooperation,
    Boundary,
    AnalogGamma,
    AnalogGain,
    DigitalGain,
    WhiteBalanceExposure,
    /// The 9000 calls this Reserved while advertising it in `Features`
    Setup,
    Perforation,
    Boundary2,
    ShipmentWhiteBalance,
    CcdData,
    DriverVersion,
    LeakVolume,
    RamBuffer,
    EepromBuffer,
}

impl DataType {
    pub const fn row(self) -> Row {
        use DataTypes as D;
        /// Widths and counts for the rows neither spec fills in
        const NONE: (Option<u8>, Option<u32>) = (None, None);
        let (code, (width, count), header, read, write) = match self {
            // 1 or 2 bytes, so the caller picks, and always available
            Self::Image => (0x00, (None, None), false, None, None),
            Self::HalftoneMask => (
                0x02,
                NONE,
                true,
                Some(D::HALFTONE_READ),
                Some(D::HALFTONE_WRITE),
            ),
            Self::Lut => (
                0x03,
                (Some(2), Some(16384)),
                false,
                Some(D::GAMMA_READ),
                Some(D::GAMMA_WRITE),
            ),
            Self::Histogram => (0x80, NONE, true, Some(D::HISTOGRAM_READ), None),
            Self::MaxValue => (
                0x81,
                (Some(2), Some(1)),
                true,
                Some(D::MAX_VALUE_READ),
                None,
            ),
            Self::Matrix => (
                0x82,
                NONE,
                true,
                Some(D::MATRIX_READ),
                Some(D::MATRIX_WRITE),
            ),
            Self::Filter => (
                0x83,
                NONE,
                true,
                Some(D::FILTER_READ),
                Some(D::FILTER_WRITE),
            ),
            Self::Shading => (
                0x84,
                (Some(2), Some(47352)),
                true,
                Some(D::SHADING_READ),
                Some(D::SHADING_WRITE),
            ),
            Self::DarkVoltage => (
                0x85,
                NONE,
                true,
                Some(D::DARK_VOLTAGE_READ),
                Some(D::DARK_VOLTAGE_WRITE),
            ),
            Self::Magnetic => (
                0x86,
                NONE,
                true,
                Some(D::MAGNETIC_READ),
                Some(D::MAGNETIC_WRITE),
            ),
            // 18 elements on the 9000, Variable on the 5000, so left open
            Self::Cooperation => (0x87, (Some(1), None), true, Some(D::COOP_PARAMS_READ), None),
            Self::Boundary => (
                0x88,
                (Some(4), None),
                true,
                Some(D::BOUNDARY_READ),
                Some(D::BOUNDARY_WRITE),
            ),
            Self::AnalogGamma => (0x89, NONE, true, Some(D::ANALOG_GAMMA_READ), None),
            Self::AnalogGain => (
                0x8A,
                (Some(4), Some(2)),
                true,
                Some(D::ANALOG_GAIN_READ),
                None,
            ),
            Self::DigitalGain => (0x8B, NONE, true, Some(D::DIGITAL_GAIN_READ), None),
            Self::WhiteBalanceExposure => {
                (0x8C, (Some(4), Some(1)), true, Some(D::EXPOSURE_READ), None)
            }
            // 2-11-2 fixes no width, but Nikon Scan reads this with the
            // 1-byte code and a color qualifier 2-11-3 does not list either
            Self::Setup => (
                0x8D,
                (Some(1), None),
                true,
                Some(D::SETUP_READ),
                Some(D::SETUP_WRITE),
            ),
            Self::Perforation => (0x8E, (None, None), true, Some(D::PERFORATION_READ), None),
            Self::Boundary2 => (
                0x8F,
                (None, None),
                true,
                Some(D::BOUNDARY2_READ),
                Some(D::BOUNDARY2_WRITE),
            ),
            Self::ShipmentWhiteBalance => (0x90, NONE, true, Some(D::INITIAL_WB_READ), None),
            Self::CcdData => (0x91, (Some(2), None), true, Some(D::CCD_DATA_READ), None),
            Self::DriverVersion => (
                0x92,
                NONE,
                true,
                Some(D::DRIVER_VERSION_READ),
                Some(D::DRIVER_VERSION_WRITE),
            ),
            Self::LeakVolume => (0x93, (Some(2), Some(3)), true, Some(D::LEAK_READ), None),
            // Both buffers are always there, so Features has no bit for them
            Self::RamBuffer => (0xE0, (None, None), true, None, None),
            Self::EepromBuffer => (0xE1, (None, None), true, None, None),
        };
        Row {
            code,
            width,
            count,
            header,
            read,
            write,
        }
    }

    /// Whether the qualifier's upper byte names a channel, per 2-11-3
    pub const fn per_color(self) -> bool {
        matches!(
            self,
            Self::Lut
                | Self::Histogram
                | Self::MaxValue
                | Self::Shading
                | Self::DarkVoltage
                | Self::WhiteBalanceExposure
                // Undocumented in 2-11-3, but the captures read it per color
                | Self::Setup
        )
    }

    /// What one element holds. Width alone does not say: the two 4-byte types
    /// differ, and analog gain is IEEE-754, so `3F800000` reads back as 1.0
    pub const fn scalar(self) -> Scalar {
        match self {
            Self::AnalogGain => Scalar::F32,
            Self::Boundary | Self::WhiteBalanceExposure => Scalar::U32,
            _ => match self.row().width {
                Some(1) => Scalar::U8,
                Some(4) => Scalar::U32,
                _ => Scalar::U16,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scalar {
    U8,
    U16,
    U32,
    F32,
}

/// Valid data, split into elements
///
/// Leak volume arrives as [`Words`](Self::Words) scaled by a million, and
/// boundary information as [`Longs`](Self::Longs) whose first is a length and a
/// frame count rather than a coordinate. Neither is unpacked here
#[derive(Debug, Clone, PartialEq)]
pub enum Values {
    Bytes(Vec<u8>),
    Words(Vec<u16>),
    Longs(Vec<u32>),
    Floats(Vec<f32>),
}

impl Values {
    /// Split `bytes` into elements, dropping any tail too short to fill one
    pub fn decode(scalar: Scalar, bytes: &[u8]) -> Self {
        fn each<const N: usize, T>(bytes: &[u8], f: impl Fn([u8; N]) -> T) -> Vec<T> {
            bytes
                .chunks_exact(N)
                .map(|c| f(c.try_into().expect("chunks_exact")))
                .collect()
        }
        match scalar {
            Scalar::U8 => Self::Bytes(bytes.to_vec()),
            Scalar::U16 => Self::Words(each(bytes, u16::from_be_bytes)),
            Scalar::U32 => Self::Longs(each(bytes, u32::from_be_bytes)),
            Scalar::F32 => Self::Floats(each(bytes, f32::from_be_bytes)),
        }
    }
}

/// What the qualifier's low byte calls an element of `width` bytes, per 2-11-4
pub const fn width_code(width: u8) -> Option<u8> {
    Some(match width {
        1 => 0x00,
        2 => 0x01,
        4 => 0x03,
        _ => return None,
    })
}

/// One frame's rectangle as `DataType::Boundary` carries it, 2-11-6
///
/// Sub-scanning is Y and main-scanning is X, and this record puts them in that
/// order, the reverse of `Frames`, which leads with the left edge. Inclusive of
/// the lower right, so a 13860 line frame ends at 13859
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    /// Bytes 4-7, upper left in the sub-scanning direction
    pub top: u32,
    /// Bytes 8-11, upper left in the main-scanning direction
    pub left: u32,
    /// Bytes 12-15, lower right in the sub-scanning direction
    pub bottom: u32,
    /// Bytes 16-19, lower right in the main-scanning direction
    pub right: u32,
}

/// Boundary information, 2-11-6, `DataType::Boundary`
///
/// Where each frame sits. After a thumbnail of strip film the host works these
/// out and sends them, which is what gives the unit frame lengths it could not
/// measure for itself. Addresses are inches times the maximum resolution, the
/// same unit a window origin uses at pitch 1
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Boundary {
    pub frames: Vec<Rect>,
}

impl Boundary {
    /// Bytes before the first rectangle
    const HEAD: usize = 4;
    /// Bytes each rectangle occupies
    const RECT: usize = 16;

    /// The frame an address falls in, which is what autofocus resolves against
    pub fn at(&self, x: u32, y: u32) -> Option<Rect> {
        self.frames
            .iter()
            .copied()
            .find(|f| (f.left..f.right).contains(&x) && (f.top..f.bottom).contains(&y))
    }

    /// The frame a rectangle sits wholly inside, which is what the stage
    /// resolves a window against
    pub fn holding(&self, r: Rect) -> Option<Rect> {
        self.frames.iter().copied().find(|f| {
            f.left <= r.left && r.right <= f.right && f.top <= r.top && r.bottom <= f.bottom
        })
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let head: &[u8; Self::HEAD] = b.get(..Self::HEAD)?.try_into().ok()?;
        let count = usize::from(head[2]);
        let be32 = |s: &[u8], i: usize| u32::from_be_bytes([s[i], s[i + 1], s[i + 2], s[i + 3]]);

        let mut frames = Vec::with_capacity(count);
        for n in 0..count {
            let at = Self::HEAD + n * Self::RECT;
            let r = b.get(at..at + Self::RECT)?;
            frames.push(Rect {
                top: be32(r, 0),
                left: be32(r, 4),
                bottom: be32(r, 8),
                right: be32(r, 12),
            });
        }
        Some(Self { frames })
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        // The count is one byte (2-11-6 byte 2), so more than 255 frames cannot
        // be encoded. A silent `as u8` truncation sent a garbage table that the
        // transport refused with EINVAL
        if self.frames.len() > u8::MAX as usize {
            return Err(Error::Unsupported {
                op: "boundary",
                reason: format!(
                    "{} frames cannot fit the one-byte count field",
                    self.frames.len()
                ),
            });
        }
        let mut out = Vec::with_capacity(Self::HEAD + self.frames.len() * Self::RECT);
        // 2-11-6 gives bytes 0,1 as the parameter length "n-1", which for one
        // frame is 18. The unit's own record says 20, the whole thing including
        // these two bytes, and refuses 18 with common error 2. Match what it emits
        let length = (Self::HEAD + self.frames.len() * Self::RECT) as u16;
        out.extend_from_slice(&length.to_be_bytes());
        out.push(self.frames.len() as u8);
        out.push(0);
        for r in &self.frames {
            for v in [r.top, r.left, r.bottom, r.right] {
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
        Ok(out)
    }
}

/// What the unit remembers about one image, from 2-11-7
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Image {
    /// Which image this is
    pub index: u8,
    /// Exposure after this image's prescan
    pub exposure: u32,
    /// White balance exposure from the same prescan
    pub white_balance: u32,
    /// Darkest level the prescan found
    pub min: u16,
    /// Brightest level the prescan found
    pub max: u16,
}

/// Setup information, 2-11-7, `DataType::Setup`
///
/// The unit's own record of what it measured: the film base and, per image,
/// what a prescan decided. It survives across sessions, and `Features`'s
/// `SETUP_WRITE` means a driver can put its own numbers back
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    /// Byte 2, 0 on both units
    pub format: u8,
    /// Bytes 3,4. The film base level
    pub base_level: u16,
    /// Bytes 5-8, the exposure the base level was decided at
    pub base_exposure: u32,
    /// Bytes 9-12, the white balance exposure at that same measurement
    pub base_white_balance: u32,
    /// Byte 13 onwards, 13 bytes each
    pub images: Vec<Image>,
}

impl Setup {
    /// Bytes before the first image entry
    const HEAD: usize = 14;
    /// Bytes each image entry occupies
    const IMAGE: usize = 13;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let head: &[u8; Self::HEAD] = b.get(..Self::HEAD)?.try_into().ok()?;
        let be16 = |s: &[u8], i: usize| u16::from_be_bytes([s[i], s[i + 1]]);
        let be32 = |s: &[u8], i: usize| u32::from_be_bytes([s[i], s[i + 1], s[i + 2], s[i + 3]]);

        let count = usize::from(head[13]);
        let mut images = Vec::with_capacity(count);
        for n in 0..count {
            let at = Self::HEAD + n * Self::IMAGE;
            let e = b.get(at..at + Self::IMAGE)?;
            images.push(Image {
                index: e[0],
                exposure: be32(e, 1),
                white_balance: be32(e, 5),
                min: be16(e, 9),
                max: be16(e, 11),
            });
        }

        Some(Self {
            format: head[2],
            base_level: be16(head, 3),
            base_exposure: be32(head, 5),
            base_white_balance: be32(head, 9),
            images,
        })
    }
}

/// What EXECUTE can be told to do, table 2-15-3
///
/// `Features` bytes 20-35 say which of these a unit has, so nothing here is assumed
/// to exist. Only the ones either spec names are spelled out
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// The same as a power-on
    Initialize,
    ReturnToOrigin,
    /// The first setting value is 0 off, 1 on
    AutoAf,
    /// Likewise
    AutoCalibration,
    /// Takes an address in both setting values
    AutoFocus,
    /// The same plus a channel
    ColorAutoFocus,
    SetupShading,
    /// Moves the scan block in the AF direction, first value the position
    FocusMove,
    /// Ejects
    Unload,
    /// A code neither spec names
    Other(u8),
}

impl Op {
    pub const fn code(self) -> u8 {
        match self {
            Self::Initialize => 0x80,
            Self::ReturnToOrigin => 0x81,
            Self::AutoAf => 0x91,
            Self::AutoCalibration => 0x92,
            Self::AutoFocus => 0xA0,
            Self::ColorAutoFocus => 0xA1,
            Self::SetupShading => 0xB0,
            Self::FocusMove => 0xC1,
            Self::Unload => 0xD0,
            Self::Other(code) => code,
        }
    }
}

impl From<u8> for Op {
    fn from(code: u8) -> Self {
        match code {
            0x80 => Self::Initialize,
            0x81 => Self::ReturnToOrigin,
            0x91 => Self::AutoAf,
            0x92 => Self::AutoCalibration,
            0xA0 => Self::AutoFocus,
            0xA1 => Self::ColorAutoFocus,
            0xB0 => Self::SetupShading,
            0xC1 => Self::FocusMove,
            0xD0 => Self::Unload,
            x => Self::Other(x),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
/// The operation parameter SET PARAMETER carries, table 2-15-2
///
/// The table runs to 13 bytes and `Address` claims 15, but Nikon Scan sends 9 and it
/// works, leaving off the speed, torque and driving method. What the two setting
/// values mean depends on the operation: for AF they are the address to focus
/// on, for a focus move the first is the position
pub struct Operation {
    /// Which channel, where the operation takes one
    pub color: u8,
    pub first: u32,
    pub second: u32,
}

impl Operation {
    /// What Nikon Scan sends and reads back, against the 13 the table runs to
    pub const LENGTH: usize = 9;

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let b: &[u8; Self::LENGTH] = b.get(..Self::LENGTH)?.try_into().ok()?;
        let be32 = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Some(Self {
            color: b[0],
            first: be32(1),
            second: be32(5),
        })
    }

    pub fn to_bytes(&self) -> [u8; 9] {
        let mut b = [0u8; 9];
        b[0] = self.color;
        b[1..5].copy_from_slice(&self.first.to_be_bytes());
        b[5..9].copy_from_slice(&self.second.to_be_bytes());
        b
    }
}

/// The geometry block tables 2-11-5-1, -2 and -3 share
///
/// Each job fills in only the fields it needs and leaves the rest zero
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Geometry {
    /// Bytes 5,6
    pub bytes_per_line: u16,
    /// Bytes 7,8. Scanning lines times frames
    pub entire_lines: u16,
    /// Byte 9
    pub bits_per_color: u8,
    /// Bytes 10,11
    pub lines_per_image: u16,
    /// Byte 12. Exposures per line, which is what averaging needs
    pub readings_per_line: u8,
    /// Bytes 13,14. Line Gap Count over the scanning pitch, filled in only by
    /// the multi-line record
    pub registration_gap: u16,
}

impl Geometry {
    /// The shortest record carrying one of these, so the gap has to be there
    const LENGTH: usize = 15;

    fn from_bytes(b: &[u8]) -> Option<Self> {
        let b: &[u8; Self::LENGTH] = b.get(..Self::LENGTH)?.try_into().ok()?;
        let be16 = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
        Some(Self {
            bytes_per_line: be16(5),
            entire_lines: be16(7),
            bits_per_color: b[9],
            lines_per_image: be16(10),
            readings_per_line: b[12],
            registration_gap: be16(13),
        })
    }
}

/// Padding the host has to strip, per the LS-5000's table 2-11-5-3
///
/// Shares no byte past 4 with [`Geometry`], and runs 27 bytes rather than 18
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Truncation {
    /// Bytes 5,6. Says which of the counts below is attached at all
    pub position: Position,
    /// Bytes 7,8 then 9,10, measured from each color's own origin
    pub per_color: Edges,
    /// Bytes 11,12 then 13,14, measured from the origin of all colors
    pub all_colors: Edges,
    /// Bytes 19,20 then 21,22, counted in lines rather than bytes
    pub lines: Edges,
    /// Bytes 23,24 then 25,26, measured from one frame's origin
    pub frame: Edges,
}

/// A count of padding at each end of an axis, in the record's own unit
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Edges {
    /// The first-pixel or first-line side
    pub first: u16,
    /// The last-pixel or last-line side
    pub last: u16,
}

bitflags! {
    /// Bytes 5 and 6 of the truncation record, assembled as `byte5 | byte6 << 8`
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct Position: u16 {
        const COLOR_FIRST = 1 << 0;
        const COLOR_LAST  = 1 << 1;
        const ALL_FIRST   = 1 << 2;
        const ALL_LAST    = 1 << 3;
        const LINE_FIRST  = 1 << 6;
        const LINE_LAST   = 1 << 7;
        const FRAME_FIRST = 1 << 8;
        const FRAME_LAST  = 1 << 9;
    }
}

impl Truncation {
    /// Table 2-11-5-3 runs to byte 26
    const LENGTH: usize = 27;

    fn from_bytes(b: &[u8]) -> Option<Self> {
        let b: &[u8; Self::LENGTH] = b.get(..Self::LENGTH)?.try_into().ok()?;
        let be16 = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
        let edges = |i: usize| Edges {
            first: be16(i),
            last: be16(i + 2),
        };
        Some(Self {
            position: Position::from_bits_truncate(u16::from(b[5]) | u16::from(b[6]) << 8),
            per_color: edges(7),
            all_colors: edges(11),
            lines: edges(19),
            frame: edges(23),
        })
    }
}

/// What the scanner wants doing, and the numbers to do it with
///
/// 2-11-5, read back as `DataType::Cooperation` once a SCAN says a job is
/// pending. Byte 0 picks
/// the layout of everything after byte 4, and the four layouts share nothing
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CooperativeAction {
    /// Thumbnail (1), averaging (2) and multi-line registration (4)
    Geometry(Coop, Geometry),
    /// Truncation (6), which only the 5000 family documents
    Truncate(Truncation),
    /// CCD data (7). Bytes 5-12, one measurement type per color in the order
    /// R, G, B, neutral gray, C, M, Y, K
    CcdData([u8; 8]),
    /// A job neither spec describes, kept whole so it can be reported
    Unknown(u8, Vec<u8>),
}

impl CooperativeAction {
    /// The 1 x 18 of 2-11-2, which truncation exceeds. A floor, not the answer:
    /// the data header's own length is what sizes a read
    pub const LENGTH: usize = 18;

    /// Byte 0, in the same namespace as the `09h-80h` ASCQ that announced it.
    /// The two specs give the same job different 4th sense bytes, so dispatch
    /// on this rather than on the sense
    pub fn kind(&self) -> Coop {
        match self {
            Self::Geometry(kind, _) => *kind,
            Self::Truncate(_) => Coop::Truncate,
            Self::CcdData(_) => Coop::CcdData,
            Self::Unknown(code, _) => Coop::from(*code),
        }
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let unknown = || Some(Self::Unknown(b[0], b.to_vec()));
        let kind = Coop::from(*b.first()?);
        match kind {
            Coop::Thumbnail | Coop::Averaging | Coop::MultiLineRegistration => {
                match Geometry::from_bytes(b) {
                    Some(g) => Some(Self::Geometry(kind, g)),
                    None => unknown(),
                }
            }
            Coop::Truncate => match Truncation::from_bytes(b) {
                Some(t) => Some(Self::Truncate(t)),
                None => unknown(),
            },
            Coop::CcdData => match b.get(5..13).and_then(|s| s.try_into().ok()) {
                Some(types) => Some(Self::CcdData(types)),
                None => unknown(),
            },
            Coop::Unknown(_) => unknown(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Table 2-11-6
pub struct Header {
    /// Byte 0, echoing the type asked for
    pub code: u8,
    /// Byte 1. Valid bits per element, which can be fewer than the bytes carry:
    /// 14-bit data arrives in 2 bytes and reports 14
    pub bits: u8,
    /// Bytes 2-5. What the unit holds, and it is *not* cut down to match a short
    /// transfer length, so one short read tells us how much to ask for
    pub length: u32,
}

impl Header {
    /// Read the header and return the rest of the slice
    pub fn from_bytes(b: &[u8]) -> Option<(Self, &[u8])> {
        let head = b.get(..HEADER)?;
        Some((
            Self {
                code: head[0],
                bits: head[1],
                length: u32::from_be_bytes([head[2], head[3], head[4], head[5]]),
            },
            &b[HEADER..],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2-11-5-3's multi-line record: 4000 dpi, three colors, and a gap of 12
    #[test]
    fn the_multi_line_record_reads_as_geometry() {
        let mut b = [0u8; CooperativeAction::LENGTH];
        b[0] = 0x04;
        b[1..5].copy_from_slice(&[0x09, 0x80, 0x04, 0x01]);
        b[5..7].copy_from_slice(&60000u16.to_be_bytes());
        b[7..9].copy_from_slice(&13860u16.to_be_bytes());
        b[9] = 16;
        b[13..15].copy_from_slice(&12u16.to_be_bytes());

        let CooperativeAction::Geometry(kind, g) = CooperativeAction::from_bytes(&b).unwrap()
        else {
            panic!("not a geometry record");
        };
        assert_eq!(kind, Coop::MultiLineRegistration);
        assert_eq!(g.bytes_per_line, 60000);
        assert_eq!(g.entire_lines, 13860);
        assert_eq!(g.bits_per_color, 16);
        assert_eq!(g.registration_gap, 12);
        // 2-11-5-3 pins these to zero, and averaging is the record that fills them
        assert_eq!((g.lines_per_image, g.readings_per_line), (0, 0));
    }

    /// Byte 12 is all averaging fills in
    #[test]
    fn the_averaging_record_carries_only_the_reading_count() {
        let mut b = [0u8; CooperativeAction::LENGTH];
        b[0] = 0x02;
        b[12] = 16;

        let CooperativeAction::Geometry(kind, g) = CooperativeAction::from_bytes(&b).unwrap()
        else {
            panic!("not a geometry record");
        };
        assert_eq!(kind, Coop::Averaging);
        assert_eq!(g.readings_per_line, 16);
    }

    /// Type 7 redefines bytes 5-12 as one CCD measurement type per color, so
    /// reading them as a byte count would be nonsense
    #[test]
    fn the_ccd_record_is_per_color_types_not_geometry() {
        let mut b = [0u8; CooperativeAction::LENGTH];
        b[0] = 0x07;
        b[5..13].copy_from_slice(&[1, 2, 3, 0, 0, 0, 0, 0]);

        assert_eq!(
            CooperativeAction::from_bytes(&b).unwrap(),
            CooperativeAction::CcdData([1, 2, 3, 0, 0, 0, 0, 0])
        );
    }

    /// Truncation runs to byte 26, past the 18 bytes 2-11-2 gives the type
    #[test]
    fn the_truncation_record_is_longer_than_the_others() {
        let mut b = [0u8; 27];
        b[0] = 0x06;
        b[5] = 0b0000_0011; // padding at both ends of each color's line
        b[7..9].copy_from_slice(&8u16.to_be_bytes());
        b[9..11].copy_from_slice(&4u16.to_be_bytes());
        b[19..21].copy_from_slice(&2u16.to_be_bytes());
        b[25..27].copy_from_slice(&6u16.to_be_bytes());

        let CooperativeAction::Truncate(t) = CooperativeAction::from_bytes(&b).unwrap() else {
            panic!("not a truncation record");
        };
        assert_eq!(t.position, Position::COLOR_FIRST | Position::COLOR_LAST);
        assert_eq!(t.per_color, Edges { first: 8, last: 4 });
        assert_eq!(t.lines, Edges { first: 2, last: 0 });
        assert_eq!(t.frame, Edges { first: 0, last: 6 });

        // 18 bytes is all 2-11-2 promises, and it is not enough for this one
        assert!(matches!(
            CooperativeAction::from_bytes(&b[..CooperativeAction::LENGTH]),
            Some(CooperativeAction::Unknown(0x06, _))
        ));
    }

    /// The setup record an LS-9000 returned for one loaded image, payload only
    #[test]
    fn setup_information_decodes_a_retained_image() {
        let b = [
            0x00, 0x18, 0x00, 0x71, 0xF9, 0x00, 0x04, 0xFC, 0x62, 0x00, 0x04, 0xFC, 0x62, 0x01,
            0x01, 0x00, 0x04, 0xF0, 0x00, 0x00, 0x04, 0xF0, 0x00, 0x12, 0x34, 0x56, 0x78,
        ];
        let setup = Setup::from_bytes(&b).unwrap();

        assert_eq!(setup.format, 0);
        assert_eq!(setup.base_level, 29177);
        assert_eq!(setup.base_exposure, 326754);
        assert_eq!(setup.base_white_balance, 326754);
        assert_eq!(
            setup.images,
            vec![Image {
                index: 1,
                exposure: 0x0004F000,
                white_balance: 0x0004F000,
                min: 0x1234,
                max: 0x5678,
            }]
        );
    }

    /// The count in byte 13 is what says how much follows
    #[test]
    fn a_record_shorter_than_its_image_count_is_refused() {
        let mut b = vec![0u8; 27];
        b[13] = 2;
        assert!(Setup::from_bytes(&b).is_none());
    }

    /// The 20 bytes an LS-9000 returned with one frame loaded
    #[test]
    fn boundary_information_round_trips() {
        let b = Boundary {
            frames: vec![Rect {
                top: 0,
                left: 0,
                bottom: 13859,
                right: 9999,
            }],
        };
        let bytes = b.to_bytes().unwrap();
        assert_eq!(bytes.len(), 20);
        // What the unit itself returns: the whole length, then the frame count
        assert_eq!(&bytes[..4], &[0x00, 0x14, 0x01, 0x00]);
        assert_eq!(Boundary::from_bytes(&bytes), Some(b));
    }

    /// A job neither spec describes is reported rather than mis-parsed
    #[test]
    fn an_unknown_job_keeps_its_bytes() {
        let b = [0x0Au8; CooperativeAction::LENGTH];
        let action = CooperativeAction::from_bytes(&b).unwrap();
        assert_eq!(action.kind(), Coop::Unknown(0x0A));
        assert!(matches!(action, CooperativeAction::Unknown(0x0A, _)));
    }
}
