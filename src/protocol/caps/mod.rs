//! The capabilities a scanner advertises
//!
//! Every decision we make in operation will revolve around what a scanner says it can do, rather than trying to map a priori capabilities from known scanners
//! These are primarily built from the "page code field list" data starting from table 2-2-1-2

pub mod address;
pub mod ccd;
pub mod frames;
pub mod identity;
pub mod other;
pub mod set_window;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("page {page:02X}h truncated: need {need} bytes, got {got}")]
    Truncated { page: u8, need: usize, got: usize },

    #[error("page {page:02X}h byte {byte}: {what} = {value:#04x}")]
    BadField {
        page: u8,
        byte: usize,
        what: &'static str,
        value: u32,
    },
}

#[derive(Debug)]
/// All of the capabilities the attached scanner reports
pub struct Capabilities {
    pub identity: identity::Identity,
    pub address: address::Address,
    pub features: other::Features,
    pub set_window: set_window::SetWindowFunction,
    /// Missing from the page-00h list, so a unit may genuinely not have it
    pub ccd: Option<ccd::CcdMeasurement>,
    /// Published only when `Address` byte 16 sets `FRAME_RECTS`
    pub frames: Option<frames::Frames>,
}

/// One page from "Vital Product Data"
#[derive(Debug)]
pub struct Page {
    code: u8,
    bytes: Vec<u8>,
}

/// Utilities for reading values off the page
impl Page {
    /// Build a new page from VPD data
    pub fn new(code: u8, bytes: Vec<u8>) -> Result<Self, Error> {
        // byte 3 is the page length, so anything shorter is not a page
        if bytes.len() < 4 {
            return Err(Error::Truncated {
                page: code,
                need: 4,
                got: bytes.len(),
            });
        }
        // asked for one page, got another
        if bytes[1] != code {
            return Err(Error::BadField {
                page: code,
                byte: 1,
                what: "page code",
                value: bytes[1] as u32,
            });
        }
        Ok(Self { code, bytes })
    }

    fn array<const N: usize>(&self, i: usize) -> Result<[u8; N], Error> {
        self.bytes
            .get(i..i + N)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| Error::Truncated {
                page: self.code,
                need: i + N,
                got: self.bytes.len(),
            })
    }

    fn u8(&self, i: usize) -> Result<u8, Error> {
        Ok(self.array::<1>(i)?[0])
    }

    fn be16(&self, i: usize) -> Result<u16, Error> {
        Ok(u16::from_be_bytes(self.array(i)?))
    }

    fn be32(&self, i: usize) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.array(i)?))
    }

    /// Zero means "absent" in several fields on this page
    fn opt_u8(&self, i: usize) -> Result<Option<u8>, Error> {
        Ok(Some(self.u8(i)?).filter(|&v| v != 0))
    }

    fn opt_be16(&self, i: usize) -> Result<Option<u16>, Error> {
        Ok(Some(self.be16(i)?).filter(|&v| v != 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reading past what arrived is an error, not a panic. The one place
    /// this is checked, since every page goes through these accessors
    #[test]
    fn a_short_page_errors_rather_than_panicking() {
        let page = Page::new(0xC1, vec![0x06, 0xC1, 0x00, 0x04]).unwrap();
        assert!(matches!(
            page.be32(4),
            Err(Error::Truncated {
                need: 8,
                got: 4,
                ..
            })
        ));
    }

    /// Asking for one page and getting another
    #[test]
    fn a_mismatched_page_code_is_refused() {
        assert!(matches!(
            Page::new(0xC1, vec![0x06, 0xD1, 0x00, 0x04]),
            Err(Error::BadField { byte: 1, .. })
        ));
    }
}
