//! `CBoundarySearch`, Nikon Scan's 35mm frame search (LS5000.md3)
//!
//! Positions are signed, because Nikon's arithmetic goes before column 0.

use crate::scan::boundary::core::{
    Geometry, Gray, argmax_from_right, climb, complement, finish, gap_profile, lower_percentile,
    read, suppress,
};
use std::ops::RangeInclusive;

/// The maximum number of frames (`Initialize`, `1008d2c0`)
const MAX_FRAMES: usize = 40;
/// The minimum distance between boundaries and the shortest frame (`DetectBoundary`, `1008da10`)
const SUPPRESS: i64 = 0x84;
/// The frame pitch in resampled lines (`GetFrameByAccImage`, `1008dd30`)
const PITCH: i64 = 0x8f;
/// Columns `MakeBinaryImage` (`1008d430`) thresholds together
const BLOCK: usize = 0x8f;

/// `TellMeTheMaxWidth` (`1008ce70`): the widest a frame starting at `s` may be
fn max_width(s: i64) -> i64 {
    match s {
        ..0x1ad => 0x8a,
        0x2cb.. => 0xbb,
        _ => 0x91,
    }
}

/// `a - b` as unsigned, as Nikon compares it
fn below(a: i64, b: i64) -> u32 {
    (a - b) as u32
}

/// Whether `t` is within `tol` of `s`, exclusive
fn near(t: i64, s: i64, tol: i64) -> bool {
    s - tol < t && t < s + tol
}

/// `Execute` (`1008e0a0`): each frame's first and last column; `None` if `Initialize` refuses
pub(crate) fn search(mut img: Gray, n: usize, negative: bool) -> Option<Vec<(i64, i64)>> {
    if n > MAX_FRAMES {
        return None;
    }
    // CutImageEnd (1008c750): remove trailing zero columns if 132 or more stay
    let w = (0..img.w)
        .rposition(|x| img.column(x).any(|p| p != 0))
        .unwrap_or(0)
        + 1;
    if w >= 0x84 {
        img.crop_columns(0..w);
    }
    // CutImageEdge(7) (1008c6a0)
    img.crop_rows(7)?;

    // AccumulateMain (1008dc70): at most seven full-height columns for each frame
    let colsum = lower_percentile(&img, (n * 7) as i64);
    let local: Vec<i64> = (0..img.w)
        .step_by(BLOCK)
        .flat_map(|x0| local_block(&img, x0..=(x0 + BLOCK).min(img.w) - 1, negative))
        .collect();

    // GetFrameByAccImage (1008dd30)
    let (acc, forced) = gap_profile(colsum, img.h, 5);
    let search = Search {
        img: &img,
        acc2: acc.clone(),
        n,
    };
    let (mut starts, reliable) = search.detect(acc, &local);
    starts.sort_unstable();
    let starts = search.filter_by_pitch(starts, &reliable);
    let starts = complement(&search.acc2, starts, n, &GEOMETRY, |p| {
        search.check_start(p, &search.acc2)
    });
    Some(finish(
        &img,
        &search.acc2,
        starts,
        n,
        forced,
        &GEOMETRY,
        img.w as i64,
    ))
}

/// A 35mm frame (`ComplementBoundary`, `TheFinalCheck`, `GetLastLineOfLastFrame`)
const GEOMETRY: Geometry<fn(i64) -> i64> = Geometry {
    min: SUPPRESS,
    gap: 7,
    period: PITCH,
    length: 0x88,
    window: 10,
    max_width,
};

/// `Histgram2Binary` (`1008c850`): dark counts of `cols` at a threshold from their histogram
fn local_block(
    img: &Gray,
    cols: RangeInclusive<usize>,
    negative: bool,
) -> impl Iterator<Item = i64> + '_ {
    let mut hist = [0u32; 257];
    for x in cols.clone() {
        for p in img.column(x) {
            hist[usize::from(p)] += 1;
        }
    }
    // Nikon reads hist[256] from the next array in its object, which holds -1
    hist[256] = u32::MAX;
    let lo = usize::from(negative);
    let low = (lo..256)
        .find(|&i| (hist[i] != 0 && hist[i + 1] != 0) || hist[i] > 5)
        .unwrap_or(lo) as i64;
    // Nikon does not search if `low` is 255
    let high = (low as usize + 1..256)
        .rev()
        .find(|&i| hist[i] != 0)
        .map_or(low + 1, |i| i as i64);
    let low = if high - low < 0x10 {
        (low - 0xf).max(0)
    } else {
        low
    };
    let h = img.h as f64;
    let width = cols.clone().count() as f64;
    let v = (high - low) as f64 * ((7.0 * h) / (h * width)) * -2.0;
    // _ftol truncates toward zero
    let threshold = low - v as i64;
    cols.map(move |x| img.column(x).filter(|&p| i64::from(p) <= threshold).count() as i64)
}

/// The image and the profile after `GetEdgeFromBlackFilm`
struct Search<'a> {
    img: &'a Gray,
    acc2: Vec<i64>,
    /// Frames to find
    n: usize,
}

impl Search<'_> {
    fn w(&self) -> i64 {
        self.img.w as i64
    }

    fn h(&self) -> i64 {
        self.img.h as i64
    }

    /// `GetPeekWidth` (`1008d110`): whether the peak at `pos` is narrow, as a gap is
    fn peak_is_gap(&self, acc: &[i64], pos: i64) -> bool {
        let w = self.w();
        let thr = acc[pos as usize] - 5;
        if thr < self.h() - 0x14 || pos == 0 {
            return false;
        }
        let high = |&i: &i64| acc[i as usize] >= thr;
        let right = (pos..w.min(pos + 9)).take_while(high).count() as i64;
        let left = ((pos - 8).max(0)..pos).rev().take_while(high).count() as i64;
        if right + left > 8 {
            return false;
        }
        let r = pos + right;
        let l = match pos - 1 - left {
            l if l < 1 => -1,
            l => l,
        };
        let lit = |i: i64| 0xe < read(acc, i) && 0xe < read(acc, i + 1);
        if r <= w - 2 {
            return !lit(r) && (l < 0 || !(0xe < read(acc, l) && 0xe < read(acc, l - 1)));
        }
        // At the end, Nikon reads the heap header before `acc`
        !lit(-1)
    }

    /// `CheckStartAddress` (`1008cce0`): the first column of the frame, right of `pos`
    fn check_start(&self, mut pos: i64, profile: &[i64]) -> i64 {
        let v = read(profile, pos);
        let mut c = 0;
        if pos > 0 {
            // The flat columns before `pos`: up to five within 4, then up to two within 5
            c = ((pos - 4).max(1)..=pos)
                .rev()
                .take_while(|&i| below(v, read(profile, i)) <= 4)
                .count() as i64;
            if c == 5 {
                c += ((pos - 6).max(1)..=pos - 5)
                    .filter(|&i| below(v, read(profile, i)) < 5)
                    .count() as i64;
            }
        }
        let mut i = pos + 1;
        if i >= self.w() {
            return pos;
        }
        let lim = pos + (10 - c);
        while i <= lim {
            let a = read(profile, i);
            if a == 0 {
                return pos;
            }
            if below(v, a) > 0x13 {
                // A sharp drop: find five empty columns
                if 10 - c < 2 {
                    return pos;
                }
                for u in 1..10 - c {
                    let p = i + u;
                    if read(profile, p) == 0 {
                        let run = 1
                            + (p + 1..)
                                .take_while(|&q| read(profile, q) == 0)
                                .take(4)
                                .count() as i64;
                        if run == 5 {
                            return if u + c < 8 { u - 1 + i } else { i - u - c + 7 };
                        }
                        if u + c > 7 {
                            return i;
                        }
                    }
                }
                return pos;
            }
            c += 1;
            if c < 8 {
                pos += 1;
            }
            i += 1;
            if i >= self.w() {
                return pos;
            }
        }
        pos
    }

    /// `GetCheckFreameAddress` (`1008cfc0`): the highest column near `pos`, between boundaries
    fn check_frame(&self, pos: i64, profile: &[i64], found: &[i64]) -> i64 {
        let mut r = match pos % PITCH {
            r if r < 10 || PITCH - r < 10 => 10,
            r => r,
        };
        if let Some(&s) = found.iter().find(|&&s| pos - r < s && s < pos) {
            r = pos - s - 1;
        }
        // Left of `pos`: stop at column 1, unless `pos` is 0 or less
        let lo = if pos > 0 {
            (pos - r + 1).max(1)
        } else {
            pos - r + 1
        };
        let mut best = pos;
        if r > 1 {
            best = climb(profile, (read(profile, pos), pos), (lo..pos).rev()).1;
        }
        if let Some(&s) = found.iter().find(|&&s| pos < s && s < r + pos) {
            r = if s - pos - 1 > 10 {
                s - pos - 11
            } else {
                s - pos - 1
            };
        }
        if r > 1 {
            let hi = (pos + r - 1).min(self.w());
            best = climb(profile, (read(profile, best), best), pos + 1..=hi).1;
        }
        best
    }

    /// `CreateDefaultAccumlateTable` (`1008cea0`): boundaries from the local profile only
    fn default_table(&self, local: &[i64]) -> Vec<i64> {
        let mut tmp = local.to_vec();
        let mut found = Vec::new();
        loop {
            let (best, mut pos) = climb(&tmp, (0, 0), (1..self.w() - 1).rev());
            if best == 0 {
                if tmp[0] == 0 || found.len() >= self.n {
                    return found;
                }
                pos = 0;
            }
            suppress(&mut tmp, pos, SUPPRESS);
            if pos < self.w() - SUPPRESS {
                found.push(self.check_start(pos, local));
            }
        }
    }

    /// `DetectBoundary` (`1008da10`): the boundaries, and the reliable boundaries
    fn detect(&self, mut acc: Vec<i64>, local: &[i64]) -> (Vec<i64>, Vec<i64>) {
        let mut found = Vec::new();
        // A boundary found moves the suppression to where its frame starts
        while let (1.., mut pos) = argmax_from_right(&acc) {
            if pos < self.w() - SUPPRESS && self.peak_is_gap(&acc, pos) {
                pos = self.check_start(pos, &self.acc2);
                found.push(pos);
            }
            suppress(&mut acc, pos, SUPPRESS);
        }
        let mut reliable = found.clone();
        found.sort_unstable();

        let mut acc = self.acc2.clone();
        let mut local_work = local.to_vec();
        for &s in &found {
            suppress(&mut acc, s, SUPPRESS);
        }
        while let (1.., mut pos) = argmax_from_right(&acc) {
            if pos < self.w() - SUPPRESS {
                let p = self.check_frame(pos, &local_work, &found);
                pos = self.check_start(p, &local_work);
                found.push(pos);
                reliable.push(pos);
            }
            suppress(&mut acc, pos, SUPPRESS);
            suppress(&mut local_work, pos, SUPPRESS);
        }
        if found.len() < self.n / 2 {
            let found = self.default_table(local);
            return (found.clone(), found);
        }
        (found, reliable)
    }

    /// `GetFrameByAccImage`: keep an unreliable boundary only a whole number of pitches from another
    fn filter_by_pitch(&self, starts: Vec<i64>, reliable: &[i64]) -> Vec<i64> {
        let pitched = |i: usize, j: usize| {
            let (si, sj) = (starts[i], starts[j]);
            let k = (sj - si) / PITCH;
            let tol = if k != 0 { (k * 9).min(0x14) } else { 10 };
            let next = if i < j { k + 1 } else { k - 1 };
            (k != 0 && near(k * PITCH + si, sj, tol)) || near(next * PITCH + si, sj, tol)
        };
        let keep: Vec<i64> = (0..starts.len())
            .filter(|&i| {
                starts.len() == 1
                    || reliable.contains(&starts[i])
                    || (0..starts.len()).any(|j| j != i && pitched(i, j))
            })
            .map(|i| starts[i])
            .collect();
        if keep.is_empty() { vec![0] } else { keep }
    }
}
