//! The "other information" page, 0xE1

use super::{Error, Page};
use crate::protocol::data::Op;
use bitflags::bitflags;

#[derive(Debug, Clone)]
pub struct Features {
    /// Declared page length. Byte 3
    pub page_length: u8,
    /// What the host (this software) needs to do rather than the scanner. Byte 4,5
    pub cooperation: HostCooperation,
    /// What types are available for READ/SEND. Bytes 6 - 10
    pub data_types: DataTypes,
    /// Bit depths for the various things. Bytes 11-19
    pub depths: Depths,
    /// EXECUTE operation support. Bytes 20 - 35
    pub execute: ExecuteOps,
    /// Other other additional information (jfc nikon). Byte 36
    pub additional: u8,
    /// RAM buffer area. Byte 37
    pub volatile_buffer: u8,
    /// NV buffer area. Byte 38
    pub nonvolatile_buffer: u8,
}

bitflags! {
    /// Bytes 4 and 5, assembled as `byte4 | byte5 << 8`.
    /// A bit set means *the initiator* does that work, not the scanner
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
}

impl Features {
    pub const PAGE_CODE: u8 = 0xE1;
}

impl TryFrom<&Page> for Features {
    type Error = Error;

    fn try_from(page: &Page) -> Result<Self, Self::Error> {
        let cooperation = HostCooperation::from_bits_truncate(
            u16::from(page.u8(4)?) | u16::from(page.u8(5)?) << 8,
        );

        let mut types = 0u64;
        for (n, byte) in (6..=10).enumerate() {
            types |= u64::from(page.u8(byte)?) << (8 * n);
        }

        let mut groups = [0u16; 8];
        for (n, group) in groups.iter_mut().enumerate() {
            // Low byte first: byte 20 carries 8xh ops 0-7, byte 21 ops 8-15
            *group = u16::from(page.u8(20 + 2 * n)?) | u16::from(page.u8(21 + 2 * n)?) << 8;
        }

        Ok(Self {
            page_length: page.u8(3)?,
            cooperation,
            data_types: DataTypes::from_bits_truncate(types),
            depths: Depths {
                halftone_mask: page.u8(11)?,
                lut_input: page.u8(12)?,
                lut_output: page.u8(13)?,
                histogram: page.u8(14)?,
                max_value: page.u8(15)?,
                matrix: page.u8(16)?,
                filter: page.u8(17)?,
                shading: page.u8(18)?,
                dark_current: page.u8(19)?,
            },
            execute: ExecuteOps(groups),
            additional: page.u8(36)?,
            volatile_buffer: page.u8(37)?,
            nonvolatile_buffer: page.u8(38)?,
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
}
