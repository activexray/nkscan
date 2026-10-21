//! Additional address information pages, 2-2-2-6, codes C8h to CBh
//!
//! Where the holder's frames sit. Only published when `Address` byte 16 sets
//! [`FRAME_RECTS`](super::address::CoordinateBase::FRAME_RECTS)
//!
//! One page holds 15 images. The images after that are on C9h, CAh and CBh,
//! which only the FH-816 has (LS9000 2-2-2-1). On C8h the rectangles start
//! at byte 5, after the image count. On the next pages they start at byte 4.
//! The page length gives the number of rectangles on a page, as LS9000.md3
//! reads it (`1009e960` for C8h, `1009eb90` for the next pages).

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
    /// The images on all pages read, in order
    pub images: Vec<Frame>,
    /// C8h byte 4: the images the unit has on all pages. It follows the
    /// attached holder and not the model
    count: usize,
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

    /// The pages after C8h, in order
    pub const NEXT_PAGES: [u8; 3] = [0xC9, 0xCA, 0xCB];

    /// Bytes one rectangle occupies
    const STRIDE: usize = 16;

    /// Whether the unit has images on a page not read yet
    pub fn incomplete(&self) -> bool {
        self.images.len() < self.count
    }

    /// Add the images of the next page, C9h to CBh
    pub fn extend(&mut self, page: &Page) -> Result<(), Error> {
        self.images.extend(rectangles(page, 4)?);
        Ok(())
    }
}

/// The rectangles of a page, from byte `first`
fn rectangles(page: &Page, first: usize) -> Result<Vec<Frame>, Error> {
    let count = usize::from(page.u8(3)? >> 4);
    (0..count)
        .map(|n| {
            let at = first + n * Frames::STRIDE;
            Ok(Frame {
                left: page.be32(at)?,
                top: page.be32(at + 4)?,
                width: page.be32(at + 8)?,
                length: Some(page.be32(at + 12)?).filter(|&l| l != 0),
            })
        })
        .collect()
}

impl TryFrom<&Page> for Frames {
    type Error = Error;

    fn try_from(page: &Page) -> Result<Self, Self::Error> {
        Ok(Self {
            images: rectangles(page, 5)?,
            count: usize::from(page.u8(4)?),
        })
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

    /// The bytes of a page with `count` rectangles from `first`, numbered from `from`
    fn page(code: u8, first: usize, count: usize, from: u32) -> Vec<u8> {
        let mut p = vec![0u8; first];
        p[1] = code;
        for n in from..from + count as u32 {
            for v in [n, n * 100, 1000, 2000] {
                p.extend_from_slice(&v.to_be_bytes());
            }
        }
        p[3] = (p.len() - 4) as u8;
        p
    }

    /// More than 15 images continue on C9h, where the rectangles start at byte 4
    #[test]
    fn images_after_the_fifteenth_are_on_the_next_page() {
        let mut first = page(Frames::PAGE_CODE, 5, 15, 1);
        first[4] = 20;
        let mut frames = parse(&first);
        assert_eq!(frames.images.len(), 15);
        assert!(frames.incomplete());

        let next = Frames::NEXT_PAGES[0];
        let next = Page::new(next, page(next, 4, 5, 16)).expect("page");
        frames.extend(&next).expect("next page");
        assert!(!frames.incomplete());
        let lefts: Vec<u32> = frames.images.iter().map(|f| f.left).collect();
        assert_eq!(lefts, (1..=20).collect::<Vec<_>>());
    }
}
