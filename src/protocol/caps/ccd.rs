//! CCD measurement setting page, code 0xE3

use super::{Error, Page};
use bitflags::bitflags;

#[derive(Debug, Clone)]
pub struct CcdMeasurement {
    /// Declared page length; the page is 4 + this. Byte 3
    pub page_length: u8,
    /// Which channels a measurement covers. Byte 4, with byte 5 reserved
    pub colors: Channels,
    /// Bytes 6,7
    pub resolution: u16,
    /// How many times each measurement is scanned. Byte 8
    pub scans: u8,
    /// Curves per channel. Byte 9
    pub types: u8,
    /// The ratio of each measurement point, byte 11 onward, two bytes each.
    /// Byte 10 says how many there are
    pub points: Vec<u16>,
}

bitflags! {
    /// Byte 4. Two or more may be set at once
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Channels: u8 {
        const RED          = 1 << 0;
        const GREEN        = 1 << 1;
        const BLUE         = 1 << 2;
        const NEUTRAL_GRAY = 1 << 3;
        const CYAN         = 1 << 4;
        const MAGENTA      = 1 << 5;
        const YELLOW       = 1 << 6;
        const BLACK        = 1 << 7;
    }
}

impl CcdMeasurement {
    pub const PAGE_CODE: u8 = 0xE3;

    /// How many response curves a measurement produces
    ///
    /// 2-2-2-7: as many as the channels in byte 4 times the types in byte 9,
    /// which is what sizes the reply to a `DataType::CcdData` READ
    pub fn curves(&self) -> usize {
        self.colors.bits().count_ones() as usize * usize::from(self.types)
    }
}

impl TryFrom<&Page> for CcdMeasurement {
    type Error = Error;

    fn try_from(page: &Page) -> Result<Self, Self::Error> {
        let count = usize::from(page.u8(10)?);
        let points = (0..count)
            .map(|n| page.be16(11 + 2 * n))
            .collect::<Result<_, _>>()?;

        Ok(Self {
            page_length: page.u8(3)?,
            colors: Channels::from_bits_truncate(page.u8(4)?),
            resolution: page.be16(6)?,
            scans: page.u8(8)?,
            types: page.u8(9)?,
            points,
        })
    }
}
