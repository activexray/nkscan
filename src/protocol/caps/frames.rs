//! Additional address information page, 2-2-2-6, code 0xC8
//!
//! Where the holder's frames sit. Only published when `Address` byte 16 sets
//! [`FRAME_RECTS`](super::address::CoordinateBase::FRAME_RECTS)

use super::{Error, Page};

/// One image's rectangle, in the same coordinates a window origin uses
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// X address of the left edge
    pub left: u32,
    /// Y address of the top edge
    pub top: u32,
    /// Extent along X
    pub width: u32,
    /// Extent along Y, `None` where the unit does not know it
    ///
    /// A masked holder publishes its frames outright. A strip holder does not:
    /// 2-11-3 measures the length during thumbnail scanning, so until the host
    /// has found the boundaries itself and sent them back as
    /// `DataType::Boundary`, this is 0.
    /// `None` is that obligation, not a missing field
    pub length: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Frames {
    /// Byte 4, which follows the attached holder rather than the model
    pub images: Vec<Frame>,
}

impl Frames {
    /// Whether the unit knows where every frame ends, or the host owes it
    /// boundary detection first
    pub fn measured(&self) -> bool {
        !self.images.is_empty() && self.images.iter().all(|f| f.length.is_some())
    }
}

impl Frames {
    pub const PAGE_CODE: u8 = 0xC8;

    /// Bytes one rectangle occupies
    const STRIDE: usize = 16;
}

impl TryFrom<&Page> for Frames {
    type Error = Error;

    fn try_from(page: &Page) -> Result<Self, Self::Error> {
        let count = usize::from(page.u8(4)?);
        let mut images = Vec::with_capacity(count);
        for n in 0..count {
            let at = 5 + n * Frames::STRIDE;
            images.push(Frame {
                left: page.be32(at)?,
                top: page.be32(at + 4)?,
                width: page.be32(at + 8)?,
                length: Some(page.be32(at + 12)?).filter(|&l| l != 0),
            });
        }
        Ok(Self { images })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read off a real LS-9000 with one 6x9 strip loaded
    const LS9000: &[u8] = &[
        0x06, 0xC8, 0x00, 0x11, 0x01, 0x00, 0x00, 0x02, 0x06, 0x00, 0x00, 0x08, 0xBC, 0x00, 0x00,
        0x23, 0x04, 0x00, 0x00, 0x00, 0x00,
    ];

    fn parse(bytes: &[u8]) -> Frames {
        let page = Page::new(Frames::PAGE_CODE, bytes.to_vec()).expect("page");
        Frames::try_from(&page).expect("frames")
    }

    /// The left edge is the X offset every captured SET WINDOW used
    #[test]
    fn one_strip_publishes_one_rectangle() {
        let frames = parse(LS9000);
        assert_eq!(frames.images.len(), 1);
        assert_eq!(
            frames.images[0],
            Frame {
                left: 518,
                top: 2236,
                width: 8964,
                length: None,
            }
        );
    }

    /// A measured length arrives as a real value rather than as the 0 that
    /// means the unit has not looked yet
    #[test]
    fn a_measured_length_is_not_absent() {
        let mut p = LS9000.to_vec();
        p[17..21].copy_from_slice(&13176u32.to_be_bytes());
        assert_eq!(parse(&p).images[0].length, Some(13176));
    }
}
