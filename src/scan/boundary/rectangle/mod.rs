//! `CBrownieBoundarySearch`, Nikon Scan's 120 frame search (LS9000.md3), for 88h rectangles

use super::{
    analysis_image,
    core::{
        Geometry, Gray, argmax_from_right, complement, finish, gap_profile, lower_percentile,
        suppress,
    },
    top_bits,
};
use crate::{
    protocol::{caps::film::FilmFormat, decode::Image},
    scan::profile::Film,
};

/// The FH-869S strip holder, for which `CutImageEdge` removes more of the ends
const STRIP_HOLDER: u8 = 0x17;

/// `Initialize` (`100976a0`): the frame pitch and gap in lines, and the frames in a strip
fn folder(format: FilmFormat) -> Option<(i64, i64, usize)> {
    Some(match format {
        FilmFormat::F645 => (138, 16, 4),
        FilmFormat::F66 => (186, 20, 3),
        FilmFormat::F67 => (228, 29, 2),
        FilmFormat::F68 => (254, 29, 2),
        FilmFormat::F69 => (274, 29, 2),
        _ => return None,
    })
}

/// The first and last thumbnail line of each frame on a 120 strip; `None` if Nikon Scan finds none
///
/// `OffsetAnalyze` (`100964d0`): after a repeated frame, each frame is the last `1/N` of the strip.
pub fn frames(
    image: &Image,
    film: Film,
    format: FilmFormat,
    holder: Option<u8>,
) -> Option<Vec<(i64, i64)>> {
    let (_, _, n) = folder(format)?;
    let found = search(image, film, format, holder)?;
    let h = image.stride as i64;
    let last = (h - h / n as i64, h);
    let repeat = found.windows(2).position(|w| w[0] == w[1]);
    Some(
        found
            .iter()
            .enumerate()
            .map(|(i, &f)| match repeat {
                Some(r) if i > r => last,
                _ => f,
            })
            .filter(|&(_, e)| e != 0)
            .collect(),
    )
}

/// `CBrownieBoundarySearch::Execute` (`10098380`): each frame's first and last line
pub(crate) fn search(
    image: &Image,
    film: Film,
    format: FilmFormat,
    holder: Option<u8>,
) -> Option<Vec<(i64, i64)>> {
    let (pitch, gap, n) = folder(format)?;
    let negative = film.is_negative();
    let columns: Vec<usize> = (0..image.stride).collect();
    let mut img = analysis_image(image, &columns, film, image.stride, top_bits);
    let geometry = Geometry {
        min: pitch - 10,
        gap,
        period: pitch + gap,
        length: pitch,
        window: 2 * gap,
        // TellMeTheMaxWidth (10097580)
        max_width: |_| pitch + 5,
    };

    // CutImageEdge(7) (10096fa0): remove seven rows at each edge, and the ends of the strip
    let bound = img.w as i64;
    let (head, tail) = if holder == Some(STRIP_HOLDER) {
        (46, 18)
    } else {
        (8, 33)
    };
    if img.w < head + tail {
        return None;
    }
    img.crop_rows(7)?;
    img.crop_columns(head..img.w - tail);
    // CutImageEnd (100970b0): remove empty trailing columns if a frame stays
    let w = img.w;
    let filled = |x: usize| {
        if negative {
            img.column(x).map(u64::from).sum::<u64>() > w as u64
        } else {
            img.column(x).any(|p| p != 0xff)
        }
    };
    let end = (1..w).rev().find(|&x| filled(x)).unwrap_or(0) + 1;
    if end as i64 >= geometry.min {
        img.crop_columns(0..end);
    }

    // AccumulateMain (100980d0)
    let w = img.w as i64;
    let colsum = lower_percentile(&img, ((n as i64 + 1) * gap).max(w - pitch * n as i64));

    // GetFrameByAccImage (100981a0)
    let (acc, forced) = gap_profile(colsum, img.h, 10);
    let starts = detect(&img, acc.clone(), acc.clone(), &geometry, head as i64);
    let starts = complement(&acc, starts, n, &geometry, |p| check_start(&img, &acc, p));
    Some(finish(&img, &acc, starts, n, forced, &geometry, bound))
}

/// `GetPeekWidth` (`10097590`): the last column of the peak at `x`, if it is wide enough for a gap
fn gap_end(acc: &[i64], h: i64, gap: i64, x: i64) -> Option<i64> {
    let thr = acc[x as usize] - 5;
    if thr < h - 20 || x == 0 {
        return None;
    }
    let high = |&i: &i64| acc[i as usize] >= thr;
    let right = (x..(acc.len() as i64).min(x + 0x28))
        .take_while(high)
        .count() as i64;
    let left = ((x - 0x27).max(0)..x).rev().take_while(high).count() as i64;
    (gap - 4 <= right + left || x < gap - 4).then_some(x + right - 1)
}

/// `CheckStartAddress` (`10097970`): the first line of the frame, at most four lines right of `p`
fn check_start(img: &Gray, acc2: &[i64], mut p: i64) -> i64 {
    for i in p + 1..(p + 5).min(img.w as i64) {
        let m = i64::from(img.column_max(i as usize));
        let a = acc2[i as usize];
        if a == 0 || m > 0x14 || (acc2[i as usize - 1] - a) as u32 > 0x14 {
            return i - 1;
        }
        if m < 5 {
            p += 1;
        }
    }
    p
}

/// `DetectBoundary` (`10097e60`): the boundaries, offset by the `head` columns that were cropped
fn detect<F: Fn(i64) -> i64>(
    img: &Gray,
    mut acc: Vec<i64>,
    acc2: Vec<i64>,
    g: &Geometry<F>,
    head: i64,
) -> Vec<i64> {
    let (w, h) = (img.w as i64, img.h as i64);
    let mut found = Vec::new();
    // A boundary found moves the suppression to where its frame starts
    while let (1.., mut pos) = argmax_from_right(&acc) {
        if pos < w - g.min
            && let Some(end) = gap_end(&acc, h, g.gap, pos)
        {
            pos = check_start(img, &acc2, end);
            found.push(pos + head);
        }
        suppress(&mut acc, pos, g.min);
    }
    let mut acc = acc2.clone();
    for &s in &found {
        suppress(&mut acc, s - head, g.min);
    }
    while let (1.., mut pos) = argmax_from_right(&acc) {
        if pos < w - g.min {
            pos = check_start(img, &acc2, pos);
            found.push(pos + head);
        }
        suppress(&mut acc, pos, g.min);
    }
    found.sort_unstable();
    found
}

#[cfg(test)]
mod tests;
