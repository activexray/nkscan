//! Frame detection on a thumbnail, a port of Nikon Scan 4.0.2's
//!
//! [`perforation`] is for a unit that counts perforations (8Eh, 8Fh), and
//! [`rectangle`] is for 120 film (88h). Each number is Nikon's, from the
//! decompiled function that the item names.

mod core;
#[cfg(test)]
pub(crate) mod fixtures;
pub mod perforation;
pub mod rectangle;

use crate::{protocol::decode::Image, scan::profile::Film};
use core::Gray;

/// Nikon Scan's inverting lookup for negatives, 256 entries each for red, green and blue
static NEGATIVE_LUT: &[u8; 768] = include_bytes!("negative_lut.bin");

/// LS5000.md3's 8-bit positive (`1008a630`): 255 if the high byte is set, else the low byte
pub(crate) fn saturating(u: u16, shift: u32) -> u8 {
    if u > 0xff {
        255
    } else {
        ((u & 0xff) >> shift) as u8
    }
}

/// LS9000.md3's 8-bit positive sample: the top 8 bits (`10094d60`)
pub(crate) fn top_bits(u: u16, shift: u32) -> u8 {
    (u >> shift) as u8
}

/// `ThumbnailAnalyze`, `MakeACopyOfImage`, `RotateImage`: the 8-bit image of `columns`, `lines` long
///
/// Lines after `columns` are zero samples, and the negative lookup also applies to them.
pub(crate) fn analysis_image(
    image: &Image,
    columns: &[usize],
    film: Film,
    lines: usize,
    positive: fn(u16, u32) -> u8,
) -> Gray {
    let shift = u32::from(image.bits.saturating_sub(8));
    let negative = film.is_negative();
    let eight = |c: usize, u: u16| -> u32 {
        if negative {
            u32::from(NEGATIVE_LUT[c * 256 + usize::from((u >> shift) & 0xff)])
        } else {
            u32::from(positive(u, shift))
        }
    };
    let h = image.rows;
    let mut px = vec![0u8; lines * h];
    for x in 0..lines {
        let src = columns.get(x).copied();
        for y in 0..h {
            let sum: u32 = (0..3)
                .map(|c| {
                    let u = src.map_or(0, |s| image.colors[c][y * image.stride + s]);
                    eight(c, u)
                })
                .sum();
            px[y * lines + x] = (sum / 3) as u8;
        }
    }
    Gray { w: lines, h, px }
}
