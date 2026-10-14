//! The "other information" page, 0xE1

use super::{Error, Page};
use crate::protocol::data::Op;
use bitflags::bitflags;

/// The two flag fields are extendable, so each says where the next one starts
/// and the rest is found by walking rather than by indexing
#[derive(Debug, Clone)]
pub struct Features {
    /// Declared page length
    pub page_length: u8,
    /// What the host (this software) needs to do rather than the scanner
    pub cooperation: HostCooperation,
    /// What types are available for READ/SEND
    pub data_types: DataTypes,
    /// Bit depths for the various things
    pub depths: Depths,
    /// EXECUTE operation support
    pub execute: ExecuteOps,
    /// Other other additional information (jfc nikon)
    pub additional: u8,
    /// RAM buffer area
    pub volatile_buffer: u8,
    /// NV buffer area
    pub nonvolatile_buffer: u8,
}

bitflags! {
    /// The field's bytes, low one first. A bit set means *the initiator* does
    /// that work, not the scanner
    ///
    /// Five of these pair with the [`Coop`](crate::protocol::sense::Coop)
    /// handshakes: a bit set here is an `09h-80h` ASCQ that will arrive
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct HostCooperation: u16 {
        // Byte 4
        const THUMBNAIL           = 1 << 0;
        const AVERAGING           = 1 << 1;
        const REGISTRATION        = 1 << 2;
        const DARK_VOLTAGE        = 1 << 3;
        const SHADING_CALIBRATION = 1 << 4;
        const AUTOFOCUS           = 1 << 5;
        const SHADING_CORRECTION  = 1 << 6;
        // Byte 5. The LS-5000 words bit 0 "3 line" where the LS-9000 says
        // "multi line"; same bit, same meaning
        const MULTI_LINE          = 1 << 8;
        const PITCH_MAIN_SCAN     = 1 << 9;
        const TRUNCATED           = 1 << 10;
        const CCD_DATA            = 1 << 11;
        // Bit 7 of each byte is the extend bit, marking that the field carries
        // on into the next one. Structural, so truncated away rather than
        // listed. Bits 12-14 are reserved.
    }
}

bitflags! {
    /// Bytes 6-10, assembled as `byte6 | byte7 << 8 | .. | byte10 << 32`
    ///
    /// Which data types READ and SEND will carry
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct DataTypes: u64 {
        // Byte 6
        const HALFTONE_READ    = 1 << 0;
        const HALFTONE_WRITE   = 1 << 1;
        const GAMMA_READ       = 1 << 2;
        const GAMMA_WRITE      = 1 << 3;
        const HISTOGRAM_READ   = 1 << 4;
        const MAX_VALUE_READ   = 1 << 5;
        // Byte 7
        const MATRIX_READ      = 1 << 8;
        const MATRIX_WRITE     = 1 << 9;
        const FILTER_READ      = 1 << 10;
        const FILTER_WRITE     = 1 << 11;
        const SHADING_READ     = 1 << 12;
        const SHADING_WRITE    = 1 << 13;
        // Byte 8
        const DARK_VOLTAGE_READ  = 1 << 16;
        const DARK_VOLTAGE_WRITE = 1 << 17;
        const MAGNETIC_READ      = 1 << 18;
        const MAGNETIC_WRITE     = 1 << 19;
        const COOP_PARAMS_READ   = 1 << 20;
        const BOUNDARY_READ      = 1 << 21;
        const BOUNDARY_WRITE     = 1 << 22;
        // Byte 9
        const ANALOG_GAMMA_READ  = 1 << 24;
        const ANALOG_GAIN_READ   = 1 << 25;
        const DIGITAL_GAIN_READ  = 1 << 26;
        const EXPOSURE_READ      = 1 << 27;
        const SETUP_READ         = 1 << 28;
        const SETUP_WRITE        = 1 << 29;
        const PERFORATION_READ   = 1 << 30;
        // Byte 10
        const BOUNDARY2_READ       = 1 << 32;
        const BOUNDARY2_WRITE      = 1 << 33;
        const INITIAL_WB_READ      = 1 << 34;
        const CCD_DATA_READ        = 1 << 35;
        const DRIVER_VERSION_READ  = 1 << 36;
        const DRIVER_VERSION_WRITE = 1 << 37;
        const LEAK_READ            = 1 << 38;
    }
}

/// Bytes 11-19, each the number of bits in one datum of that kind
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Depths {
    /// Byte 11
    pub halftone_mask: u8,
    /// Byte 12, input side of a downloaded LUT
    pub lut_input: u8,
    /// Byte 13, output side of a downloaded LUT
    pub lut_output: u8,
    /// Byte 14
    pub histogram: u8,
    /// Byte 15, the AE maximum value
    pub max_value: u8,
    /// Byte 16
    pub matrix: u8,
    /// Byte 17
    pub filter: u8,
    /// Byte 18, shading correction coefficient
    pub shading: u8,
    /// Byte 19, dark voltage correction coefficient
    pub dark_current: u8,
}

/// Bytes 20-35, one `u16` per EXECUTE opcode high nibble, `8xh` through `Fxh`
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ExecuteOps([u16; 8]);

impl std::fmt::Debug for ExecuteOps {
    /// The operations rather than the bitmasks, which is what anyone reading
    /// this actually wants
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ops: Vec<String> = self
            .iter()
            .map(|op| format!("{op:?} ({:02X}h)", op.code()))
            .collect();
        write!(f, "[{}]", ops.join(", "))
    }
}

impl ExecuteOps {
    /// Whether EXECUTE operation `op` is supported
    ///
    /// High nibble picks the word, low nibble the bit. Anything below `80h`
    /// has no word and is unsupported by construction
    pub fn supports(&self, op: Op) -> bool {
        let code = op.code();
        let group = (code >> 4).wrapping_sub(8) as usize;
        self.0
            .get(group)
            .is_some_and(|m| m & (1 << (code & 0x0F)) != 0)
    }

    /// Every operation this unit advertises
    pub fn iter(&self) -> impl Iterator<Item = Op> + '_ {
        (0x80..=0xFFu8)
            .map(Op::from)
            .filter(|&op| self.supports(op))
    }

    /// A registry that advertises only `ops`
    #[cfg(test)]
    pub(crate) fn of(ops: &[Op]) -> Self {
        let mut groups = [0u16; 8];
        for op in ops {
            let code = op.code();
            groups[usize::from((code >> 4) - 8)] |= 1 << (code & 0x0F);
        }
        Self(groups)
    }
}

impl Features {
    pub const PAGE_CODE: u8 = 0xE1;
}

impl TryFrom<&Page> for Features {
    type Error = Error;

    fn try_from(page: &Page) -> Result<Self, Self::Error> {
        // Both of the leading fields are extendable, so each one says where the
        // next begins. Only the depths onwards are a fixed count of bytes
        let (cooperation, len) = page.flags(4)?;
        let (types, types_len) = page.flags(4 + len)?;
        let depths = 4 + len + types_len;
        let execute = depths + 9;
        let tail = execute + 16;
        // Extendable in its turn, so the two buffer sizes follow it
        let (additional, additional_len) = page.flags(tail)?;
        let buffers = tail + additional_len;

        let mut groups = [0u16; 8];
        for (n, group) in groups.iter_mut().enumerate() {
            // Low byte first: the first byte of a pair carries ops 0-7 of that
            // high nibble, the second ops 8-15
            *group = u16::from(page.u8(execute + 2 * n)?)
                | u16::from(page.u8(execute + 1 + 2 * n)?) << 8;
        }

        Ok(Self {
            page_length: page.u8(3)?,
            cooperation: HostCooperation::from_bits_truncate(cooperation as u16),
            data_types: DataTypes::from_bits_truncate(types),
            depths: Depths {
                halftone_mask: page.u8(depths)?,
                lut_input: page.u8(depths + 1)?,
                lut_output: page.u8(depths + 2)?,
                histogram: page.u8(depths + 3)?,
                max_value: page.u8(depths + 4)?,
                matrix: page.u8(depths + 5)?,
                filter: page.u8(depths + 6)?,
                shading: page.u8(depths + 7)?,
                dark_current: page.u8(depths + 8)?,
            },
            execute: ExecuteOps(groups),
            additional: additional as u8,
            volatile_buffer: page.u8(buffers)?,
            nonvolatile_buffer: page.u8(buffers + 1)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read off a real LS-9000 ED
    const LS9000: &[u8] = &[
        0x06, 0xE1, 0x00, 0x23, 0x83, 0x0D, 0xA0, 0x80, 0xF0, 0xBA, 0x48, 0x00, 0x00, 0x00, 0x00,
        0x10, 0x00, 0x00, 0x10, 0x10, 0x03, 0x00, 0x06, 0x00, 0x01, 0x00, 0x09, 0x00, 0x02, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x04, 0x00,
    ];

    /// Read off a real LS-8000 ED, whose data types field is a byte shorter
    /// than the LS-9000's, moving everything after it
    const LS8000: &[u8] = &[
        0x06, 0xE1, 0x00, 0x22, 0x83, 0x05, 0xAC, 0x90, 0xF0, 0x3A, 0x00, 0x0E, 0x0E, 0x00, 0x0E,
        0x00, 0x00, 0x0E, 0x0E, 0x03, 0x00, 0x06, 0x00, 0x01, 0x00, 0x09, 0x00, 0x02, 0x00, 0x01,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x04, 0x00,
    ];

    /// 2-2-2-5's SA-21/SA-30 column for the LS-5000
    fn ls5000() -> Vec<u8> {
        let mut p = vec![0u8; 39];
        p[1] = Features::PAGE_CODE;
        p[3] = 35;
        p[4] = 0x83;
        p[5] = 0x0C;
        p[6] = 0x80;
        p[7] = 0xB0;
        p[8] = 0x90;
        p[9] = 0xDA;
        p[10] = 0x7B;
        p
    }

    fn parse(bytes: &[u8]) -> Features {
        let page = Page::new(Features::PAGE_CODE, bytes.to_vec()).expect("page");
        Features::try_from(&page).expect("features")
    }

    /// The cooperation bits are the five `09h-80h` handshakes, so this and
    /// `sense::Coop` have to agree
    #[test]
    fn cooperation_matches_the_coop_handshakes() {
        let c = parse(LS9000).cooperation;
        assert_eq!(
            c,
            HostCooperation::THUMBNAIL      // ASCQ 01h
                | HostCooperation::AVERAGING  // 02h
                | HostCooperation::MULTI_LINE // 04h
                | HostCooperation::TRUNCATED  // 06h
                | HostCooperation::CCD_DATA // 07h
        );
    }

    /// 2-2-2-5's summary gives byte 5 as 05h and byte 6 as ACh. Hardware says
    /// 0Dh and A0h: CCD-data cooperation is real, the LUT is not transferable
    /// (backing 2-11-4's prose), and the max value is readable where both the
    /// summary and the per-bit table claim otherwise
    #[test]
    fn hardware_overrides_the_summary_bytes() {
        let f = parse(LS9000);
        assert!(f.cooperation.contains(HostCooperation::CCD_DATA));
        assert!(!f.data_types.contains(DataTypes::GAMMA_READ));
        assert!(f.data_types.contains(DataTypes::MAX_VALUE_READ));
        // Corroborated by the bit depth for the same datum, also given as 0
        assert_eq!(f.depths.max_value, 16);
    }

    /// Framing is advertised, not inferred: 135 seeks by perforation, 120 by
    /// rectangle. Three CCD lines need host registration, two do not
    #[test]
    fn the_families_advertise_different_framing_and_registration() {
        let nine = parse(LS9000);
        let five = parse(&ls5000());

        assert!(five.data_types.contains(DataTypes::PERFORATION_READ));
        assert!(!nine.data_types.contains(DataTypes::PERFORATION_READ));
        assert!(nine.data_types.contains(DataTypes::BOUNDARY_READ));
        assert!(!five.data_types.contains(DataTypes::BOUNDARY_READ));

        assert!(nine.cooperation.contains(HostCooperation::MULTI_LINE));
        assert!(!five.cooperation.contains(HostCooperation::MULTI_LINE));
    }

    /// High nibble picks the word, low nibble the bit
    #[test]
    fn the_execute_registry_decodes_to_opcodes() {
        let e = parse(LS9000).execute;
        assert_eq!(
            e.iter().map(Op::code).collect::<Vec<_>>(),
            [0x80, 0x81, 0x91, 0x92, 0xA0, 0xB0, 0xB3, 0xC1, 0xD0]
        );
        assert!(!e.supports(Op::Other(0x93)));
        // Nothing below 80h has a word
        assert!(!e.supports(Op::Other(0x7F)));
    }

    /// The extend bit sets the length, not the byte numbers the spec prints:
    /// this unit ends its data types at 3Ah where the LS-9000's BAh carries on
    /// into a fifth byte. Read at the LS-9000's offsets the fields land a byte
    /// early, which is a 14 bit unit reporting a 3 bit dark current, an
    /// autofocus it has refusing to run, and a buffer size read off the padding
    #[test]
    fn a_shorter_data_types_field_moves_everything_after_it() {
        let f = parse(LS8000);
        assert_eq!(f.page_length, 34);

        assert_eq!(
            f.execute.iter().map(Op::code).collect::<Vec<_>>(),
            parse(LS9000)
                .execute
                .iter()
                .map(Op::code)
                .collect::<Vec<_>>()
        );
        assert!(f.execute.supports(Op::AutoFocus));

        // The depths are the unit's 14 bit ADC, not the LS-9000's 16
        assert_eq!(f.depths.max_value, 14);
        assert_eq!(f.depths.shading, 14);
        assert_eq!(f.depths.dark_current, 14);

        // The tail lands inside the page rather than on the space padding
        assert_eq!(
            (f.additional, f.volatile_buffer, f.nonvolatile_buffer),
            (2, 4, 0)
        );
    }

    /// Nothing on either side of the shift is lost: the flags are the same
    /// whether the field the unit sent was four bytes or five
    #[test]
    fn the_extend_bit_itself_is_not_a_flag() {
        let f = parse(LS8000);
        assert_eq!(
            f.cooperation,
            HostCooperation::THUMBNAIL
                | HostCooperation::AVERAGING
                | HostCooperation::MULTI_LINE
                | HostCooperation::TRUNCATED
        );
        // Unlike the LS-9000, this unit takes a downloaded LUT, at the same
        // 14 bits in and out
        assert!(f.data_types.contains(DataTypes::GAMMA_READ));
        assert!(f.data_types.contains(DataTypes::GAMMA_WRITE));
        assert_eq!((f.depths.lut_input, f.depths.lut_output), (14, 14));
    }

    /// A field that never clears its extend bit would run off the end of a u64
    #[test]
    fn an_endless_flags_field_is_refused() {
        let mut p = vec![0u8; 39];
        p[1] = Features::PAGE_CODE;
        p[3] = 35;
        p[4..].fill(0xFF);
        let page = Page::new(Features::PAGE_CODE, p).expect("page");
        assert!(matches!(
            Features::try_from(&page),
            Err(Error::BadField { .. })
        ));
    }
}
