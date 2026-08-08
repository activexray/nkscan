//! Mode parameters, 2-3 and 2-6
//!
//! Both units implement exactly one page, Measurement Units, with exactly one
//! variable field in it. Everything else is fixed, and setting any of it to
//! something other than what is documented is answered with common error 2

/// The only page either unit implements
pub const MEASUREMENT_UNITS: u8 = 0x03;

/// The mode parameter header, 2-3-2. Byte 3 is the block descriptor length, which is 0 or 8
const HEADER: usize = 4;

/// The units of one step of window dimension
/// SET WINDOW positions are inches multiplied by this, so at the unit's maximum resolution a step is one pixel and at 1200 it is not.
/// Only those two values are accepted (2-3-4 note 5)
pub fn divisor(reply: &[u8]) -> Option<u16> {
    let page = reply.get(HEADER + usize::from(*reply.get(3)?)..)?;
    // Bit 7 is PS and bit 6 reserved, so the code is the low six bits
    (page.first()? & 0x3F == MEASUREMENT_UNITS).then_some(())?;
    Some(u16::from_be_bytes([*page.get(4)?, *page.get(5)?]))
}

/// A parameter list that sets the divisor and nothing else
///
/// The block descriptor is omitted, which 2-3-2 note 4 explicitly permits, so
/// this is one of the four accepted lengths
pub fn set_divisor(divisor: u16) -> [u8; 12] {
    let [hi, lo] = divisor.to_be_bytes();
    [
        // Header. Every field is reserved on the way out, including the length
        // byte MODE SENSE fills in
        0,
        0,
        0,
        0, // Page code with PS unset, then the fixed parameter length
        MEASUREMENT_UNITS,
        6, // Basic measurement unit: 0 is inches, the only one either unit has
        0,
        0,
        hi,
        lo, // Reserved
        0,
        0,
    ]
}
