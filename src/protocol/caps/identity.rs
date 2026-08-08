//! Standard INQUIRY data, 2-2-1 in both specs
//!
//! Note: this is not a VPD page

use super::Error;

/// Peripheral device type 6, which is what a scanner reports
pub const SCANNER: u8 = 0x06;

/// The 36 mandatory bytes both units return
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Byte 0 bits 5-7. Nonzero means the unit is not supported on this LUN
    pub qualifier: u8,
    /// Byte 0 bits 0-4. [`SCANNER`] for the ones we drive
    pub device_type: u8,
    /// Byte 1 bit 7, set when the medium is removable
    pub removable: bool,
    /// Byte 2 bits 0-2. Both units report 2, meaning SCSI-2
    pub ansi_version: u8,
    /// Bytes 8-15
    pub vendor: String,
    /// Bytes 16-31
    pub product: String,
    /// Bytes 32-35
    pub revision: String,
}

impl Identity {
    /// The spec's recommended allocation length, and all either unit returns
    pub const LENGTH: usize = 36;

    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < Self::LENGTH {
            return Err(Error::Truncated {
                // Standard INQUIRY data has no page code of its own
                page: 0,
                need: Self::LENGTH,
                got: bytes.len(),
            });
        }
        Ok(Self {
            qualifier: bytes[0] >> 5,
            device_type: bytes[0] & 0x1F,
            removable: bytes[1] & 0x80 != 0,
            ansi_version: bytes[2] & 0x07,
            vendor: ascii(&bytes[8..16]),
            product: ascii(&bytes[16..32]),
            revision: ascii(&bytes[32..36]),
        })
    }

    /// Whether this is a scanner we should be talking to at all
    pub fn is_scanner(&self) -> bool {
        self.qualifier == 0 && self.device_type == SCANNER
    }
}

/// The identification fields are space-padded ASCII
fn ascii(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ls9000() -> Vec<u8> {
        let mut p = vec![b' '; 36];
        p[0] = 0x06;
        p[8..16].copy_from_slice(b"Nikon   ");
        p[16..32].copy_from_slice(b"LS-9000 ED      ");
        p
    }

    /// Both halves of byte 0 matter. 2-2-2 note 1: a qualifier of 011b means
    /// the unit is unsupported on this LUN whatever the device type says
    #[test]
    fn only_a_type_six_with_a_zero_qualifier_is_a_scanner() {
        assert!(Identity::parse(&ls9000()).unwrap().is_scanner());

        let mut p = ls9000();
        p[0] = 0b011_00110;
        assert!(!Identity::parse(&p).unwrap().is_scanner());

        p[0] = 0x00; // a disk
        assert!(!Identity::parse(&p).unwrap().is_scanner());
    }

    /// The identification fields are space-padded on the wire
    #[test]
    fn the_identification_fields_are_trimmed() {
        let id = Identity::parse(&ls9000()).unwrap();
        assert_eq!(id.vendor, "Nikon");
        assert_eq!(id.product, "LS-9000 ED");
    }
}
