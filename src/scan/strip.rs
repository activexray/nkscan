//! Finding the frames of a strip in its thumbnail
//!
//! A camera exposes the same window each time, and a transport advances by the
//! same amount each time. The frames are therefore all one size and one
//! distance apart, and the fit is two numbers: where the first gap is, and the
//! pitch.
//!
//! The film between two frames holds no picture, so it reads the same all the
//! way down the sensor. A picture does not. This finds the gaps by that, and
//! the frames are what lies between them. The level is never used, so which
//! way the film reads does not matter.
//!
//! The count falls out of the fit. The film that holds a picture is so many
//! pitches long, and the pitch is what is being searched, so each candidate
//! pitch says how many frames it would take to cover that film. A frame
//! nobody exposed is still counted where the film either side of it is, since
//! the fit spans it; one at the end of the strip is not, because nothing in
//! the pass distinguishes it from the bare film past the last frame.
//!
//! The format bounds the pitch search. It is not the answer: a camera gate is
//! not the rectangle the caller scans.

use crate::protocol::decode::Image;
use std::ops::{Range, RangeInclusive};
use tracing::*;

/// Rows to drop at each end of the sensor axis. The holder edge is a step that
/// reads as detail in every column
const TRIM: usize = 8;

/// Columns either side of a gap that count as it. The frames are only about
/// evenly spaced, so an even fit lands near a gap and not always on it
const REACH: usize = 2;

/// The share of columns taken to be picture, which sets the scale of the
/// detail signal
const SCALE: f32 = 0.90;

/// The share taken to be bare film, which is the flat end of the signal
const BARE: f32 = 0.10;

/// How far from bare film's own level a column may read and still be bare, as
/// a share of the way to what a picture reads
const TOLERANCE: f32 = 0.15;

/// The longest flat stretch that lies inside one picture rather than between
/// two, as a share of the format. A picture is not busy end to end - a sky, a
/// wall, a wash of shadow - and those parts of it read as flat as film
const CLOSE: f32 = 0.70;

/// The pitch to search, in twentieths of the format. A short wind leaves the
/// frames almost touching and a long one leaves film between them
const PITCH: RangeInclusive<usize> = 18..=31;

/// The fewest columns of picture a frame may have
const PICTURE: usize = 8;

/// The least of the format a run of picture must span to be a frame's, as a
/// share of it
///
/// The edge of the holder's opening, and the cut end of the strip, put a step
/// across the sensor: every row of those columns differs, which is exactly
/// what a picture looks like here. They span a few columns against the format
/// or more that a frame's picture runs, so a short run is one of those and
/// says nothing about where the film with pictures on it begins or ends
const RUN: f32 = 0.50;

/// The frames of a strip, in columns of the thumbnail
#[derive(Debug, Clone, PartialEq)]
pub struct Strip {
    /// The picture of each frame, between the gaps either side. Not the
    /// rectangle a scan of it takes: see [`Strip::scans`]
    pub frames: Vec<Range<usize>>,
    /// Columns from one gap to the next
    pub pitch: usize,
    /// Detail in the pictures less detail in the gaps
    pub contrast: f32,
}

/// How much picture each column holds
///
/// The spread of a column down the sensor axis. Film between two frames reads
/// the same all the way down, whether it is clear, dense or holder. A picture
/// does not
fn detail(image: &Image) -> Vec<f32> {
    let band = TRIM..image.rows.saturating_sub(TRIM);
    let mut out = vec![0.0; image.cols];
    if band.len() < 2 || image.colors.is_empty() {
        return out;
    }
    // The pass's own full scale. A pass read before anything stretched it does
    // not fill 16 bits
    let full = match image.bits {
        1..16 => ((1u32 << image.bits) - 1) as f32,
        _ => f32::from(u16::MAX),
    };
    let n = band.len() as f32;

    for (x, out) in out.iter_mut().enumerate() {
        let mut sum = 0.0;
        for plane in &image.colors {
            let at = |y: usize| f32::from(plane[y * image.cols + x]) / full;
            let mean = band.clone().map(at).sum::<f32>() / n;
            let var = band.clone().map(|y| (at(y) - mean).powi(2)).sum::<f32>() / n;
            sum += var.sqrt();
        }
        *out = sum / image.colors.len() as f32;
    }

    // Against the pass's own picture, not full scale: a thumbnail of a flat
    // strip is not a thumbnail of nothing
    let mut sorted = out.clone();
    sorted.sort_by(f32::total_cmp);
    let scale = sorted[(sorted.len() as f32 * SCALE) as usize % sorted.len()];
    if scale > 0.0 {
        for v in &mut out {
            *v = (*v / scale).min(1.0);
        }
    }
    out
}

/// The detail of a pass, with running totals so a stretch of it costs the same
/// to read however long it is
struct Detail {
    at: Vec<f32>,
    upto: Vec<f32>,
}

impl Detail {
    fn new(at: Vec<f32>) -> Self {
        let mut upto = Vec::with_capacity(at.len() + 1);
        upto.push(0.0);
        for v in &at {
            upto.push(upto[upto.len() - 1] + v);
        }
        Self { at, upto }
    }

    /// The mean over a stretch of columns, 0 where there is none
    fn mean(&self, span: Range<usize>) -> f32 {
        let end = span.end.min(self.at.len());
        if span.start >= end {
            return 0.0;
        }
        (self.upto[end] - self.upto[span.start]) / (end - span.start) as f32
    }

    /// The columns that hold a picture
    ///
    /// A pass runs from the holder, over the film, and out past the end of it.
    /// Only the film with pictures on it holds frames, and how long that is
    /// says how many frames a proposed pitch would take.
    ///
    /// Three things separate it from the rest of the pass, and `length` - the
    /// format - is the ruler for two of them. Bare film reads flat, so a
    /// column within [`TOLERANCE`] of the flattest in the pass is not picture.
    /// A flat stretch shorter than [`CLOSE`] of the format is inside one
    /// picture rather than between two. And a run of picture shorter than
    /// [`RUN`] of the format is an edge rather than a frame's
    fn film(&self, length: usize) -> Range<usize> {
        let mut sorted = self.at.clone();
        sorted.sort_by(f32::total_cmp);
        let at = |q: f32| sorted[((sorted.len() as f32 * q) as usize).min(sorted.len() - 1)];
        let bare = at(BARE);
        let level = bare + (at(SCALE) - bare) * TOLERANCE;
        let close = (length as f32 * CLOSE) as usize;
        let least = ((length as f32 * RUN) as usize).max(1);

        let mut runs: Vec<Range<usize>> = Vec::new();
        let mut x = 0;
        while x < self.at.len() {
            if self.at[x] <= level {
                x += 1;
                continue;
            }
            let from = x;
            while x + 1 < self.at.len() && self.at[x + 1] > level {
                x += 1;
            }
            match runs.last_mut() {
                Some(last) if from - last.end <= close => last.end = x + 1,
                _ => runs.push(from..x + 1),
            }
            x += 1;
        }

        let kept: Vec<&Range<usize>> = runs.iter().filter(|r| r.len() >= least).collect();
        match (kept.first(), kept.last()) {
            // Whatever lies between the first run of picture and the last is
            // film too: a gap, or a frame nobody exposed
            (Some(a), Some(b)) => a.start..b.end,
            // Flat end to end: an empty holder, or a pass that saw no film
            _ => 0..0,
        }
    }

    /// The lowest detail within [`REACH`] of `at`, which is the best a gap
    /// asked for there could be
    fn gap(&self, at: usize) -> f32 {
        let to = (at + REACH + 1).min(self.at.len());
        let from = at.saturating_sub(REACH).min(to.saturating_sub(1));
        self.at[from..to].iter().copied().fold(f32::MAX, f32::min)
    }
}

/// The picture between gap `k` and the next, clear of both
fn picture(first: usize, pitch: usize, k: usize) -> Range<usize> {
    let gap = first + k * pitch;
    (gap + REACH + 1)..(gap + pitch).saturating_sub(REACH)
}

/// Detail in the pictures less detail in the gaps
///
/// Both terms are needed. Gaps alone would put every frame on the holder,
/// which is as empty. Pictures alone would slide the fit onto the busiest film
fn score(detail: &Detail, first: usize, pitch: usize, frames: usize) -> f32 {
    let mut gaps = 0.0;
    for k in 0..=frames {
        gaps += detail.gap(first + k * pitch);
    }
    let mut pictures = 0.0;
    for k in 0..frames {
        pictures += detail.mean(picture(first, pitch, k));
    }
    pictures / frames as f32 - gaps / (frames + 1) as f32
}

/// Fit the frames of a strip to its thumbnail
///
/// `length` is the frame the caller scans, in columns, which bounds the pitch
/// search. `None` where the pass holds no frames
pub fn find(image: &Image, length: usize) -> Option<Strip> {
    if length == 0 || image.cols == 0 {
        return None;
    }
    let detail = Detail::new(detail(image));
    let cols = image.cols;
    let film = detail.film(length);
    debug!(?film, cols, length, "the picture in the pass");

    let least = PICTURE + 2 * REACH + 2;
    let mut best: Option<(f32, usize, usize, usize)> = None;
    for pitch in (length * PITCH.start() / 20).max(least)..=(length * PITCH.end() / 20).max(least) {
        // The film runs from the first frame's picture to the last one's, so
        // it is that many pitches long bar the gap the last frame has no
        // picture over. Nearest rather than rounded either way: that gap is a
        // fraction of a pitch, and so is however far the level put the ends of
        // the run inside or outside the pictures themselves
        let frames = (film.len() + pitch / 2) / pitch;
        if frames == 0 {
            continue;
        }
        // A frame either side of the picture, since the outermost frames may
        // be blank and hold no picture to be found by
        let from = film.start.saturating_sub(pitch);
        let to = (film.end + pitch).min(cols.saturating_sub(1));
        // A fit that runs off the film is not a fit on this strip
        let Some(last) = to.checked_sub(frames * pitch).filter(|&l| l >= from) else {
            continue;
        };
        for first in from..=last {
            let at = score(&detail, first, pitch, frames);
            if best.is_none_or(|(had, ..)| at > had) {
                best = Some((at, first, pitch, frames));
            }
        }
    }

    let (contrast, first, pitch, frames) = best?;
    let found: Vec<Range<usize>> = (0..frames).map(|k| picture(first, pitch, k)).collect();
    debug!(?found, pitch, contrast, "fitted the strip");
    Some(Strip {
        frames: found,
        pitch,
        contrast,
    })
}

impl Strip {
    /// The rectangles to scan, `extent` columns each
    ///
    /// The format is usually longer than the picture, because it has to hold
    /// any picture of that format whole. Centering it on the picture spreads
    /// the difference over both ends
    pub fn scans(&self, extent: usize, cols: usize) -> Vec<Range<usize>> {
        self.frames
            .iter()
            .map(|frame| {
                let middle = frame.start + frame.len() / 2;
                let start = middle
                    .saturating_sub(extent / 2)
                    .min(cols.saturating_sub(extent));
                start..(start + extent).min(cols)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocol::decode::Samples,
        scan::boundaries::{Polarity, tests::Strip as Film},
    };

    /// Fit a rendered strip
    fn fit(film: &Film, length: usize) -> Strip {
        let samples: Samples = film.render();
        let layout = film.layout();
        let image = Image::new(&layout, &samples).expect("the buffer is the layout's size");
        find(&image, length).expect("a strip with frames on it")
    }

    /// Every frame drawn has its middle inside the frame fitted to it
    ///
    /// Not the edges: a gap is several columns wide and which of them the
    /// ladder calls the boundary is not something the film decides
    fn holds(found: &Strip, want: &[usize], length: usize) {
        let places: Vec<Range<usize>> = found.frames.clone();
        assert_eq!(places.len(), want.len(), "{places:?} against {want:?}");
        for (place, &top) in places.iter().zip(want) {
            let middle = top + length / 2;
            assert!(
                place.contains(&middle),
                "{places:?} should each hold the middle of a frame of {want:?}"
            );
        }
    }

    /// The frames of a strip are all the same size and all the same distance
    /// apart, because a camera's window and a transport's advance are
    #[test]
    fn a_fitted_strip_is_regular() {
        let film = Film::new(vec![30, 162, 294, 426], 120, Polarity::Negative);
        let found = fit(&film, 120);
        assert!(
            found.pitch.abs_diff(132) <= REACH,
            "pitch {} in {:?}",
            found.pitch,
            found.frames
        );
        for pair in found.frames.windows(2) {
            assert_eq!(pair[0].len(), pair[1].len(), "{:?}", found.frames);
            assert_eq!(pair[1].start - pair[0].start, found.pitch);
        }
    }

    /// Which way the film reads does not come into it. A gap is flat and a
    /// picture is not, whether the gap is the brightest film there is or the
    /// densest
    #[test]
    fn polarity_does_not_change_the_answer() {
        let want = [30, 162, 294, 426];
        let fits: Vec<Strip> = [Polarity::Positive, Polarity::Negative]
            .into_iter()
            .map(|polarity| fit(&Film::new(want.to_vec(), 120, polarity), 120))
            .collect();
        holds(&fits[0], &want, 120);
        assert_eq!(fits[0].frames, fits[1].frames);
    }

    /// The bare gate past the end of the film is the largest step in the pass
    /// and holds no picture, so no frame belongs against it
    #[test]
    fn the_bare_gate_past_the_film_is_not_a_frame() {
        let mut film = Film::new(vec![30, 162, 294], 120, Polarity::Positive);
        film.feed = 560;
        film.gate = Some((430, 520));
        holds(&fit(&film, 120), &[30, 162, 294], 120);
    }

    /// A frame under the holder mask is still where it is. Moved down to clear
    /// the mask it would crop the picture that is showing
    #[test]
    fn a_frame_behind_the_holder_mask_keeps_its_place() {
        let mut film = Film::new(vec![20, 152, 284], 120, Polarity::Positive);
        film.mask = 40;
        holds(&fit(&film, 120), &[20, 152, 284], 120);
    }

    /// A flat picture reads as evenly as a gap, and the ladder is what puts it
    /// back: the other frames say where this one has to be
    #[test]
    fn a_flat_picture_is_still_a_frame() {
        for polarity in [Polarity::Positive, Polarity::Negative] {
            let mut film = Film::new(vec![30, 162, 294], 120, polarity);
            film.flat = Some(1);
            holds(&fit(&film, 120), &[30, 162, 294], 120);
        }
    }

    /// An unexposed frame is the same film as the gap around it. The frames
    /// either side of it are what place it: the film runs past it, so the fit
    /// spans it and the ladder puts a frame where the wind says
    #[test]
    fn a_frame_with_no_picture_in_it_still_gets_a_place() {
        let mut film = Film::new(vec![30, 162, 294, 426], 120, Polarity::Positive);
        film.blank = Some(2);
        holds(&fit(&film, 120), &[30, 162, 294, 426], 120);
    }

    /// A wind barely longer than the gate leaves the frames all but touching,
    /// and the pitch comes back that short rather than being rounded up to
    /// something a strip that fits the format better would have
    #[test]
    fn a_short_wind_comes_back_short() {
        let film = Film::new(vec![30, 155, 280], 120, Polarity::Negative);
        let found = fit(&film, 120);
        assert!(
            found.pitch.abs_diff(125) <= REACH,
            "pitch {} in {:?}",
            found.pitch,
            found.frames
        );
        holds(&found, &[30, 155, 280], 120);
    }

    /// The film with pictures on it is so many pitches long, and that is the
    /// count. Nothing else says it
    #[test]
    fn the_film_says_how_many_frames() {
        let film = Film::new(vec![30, 162, 294, 426], 120, Polarity::Negative);
        let found = fit(&film, 120);
        holds(&found, &[30, 162, 294, 426], 120);
        assert_eq!(found.frames.len(), 4);
    }

    /// The holder's opening has an edge at each end, and a cut film has one
    /// too. Each is a step across the sensor, which is what a picture looks
    /// like here, but a few columns of it is not the format and so is not a
    /// frame's picture. Counting from the first column of detail to the last
    /// would take the whole opening and find a frame that was never there
    #[test]
    fn an_edge_at_the_end_of_the_pass_is_not_a_frame() {
        let mut film = Film::new(vec![30, 162, 294], 120, Polarity::Negative);
        film.feed = 700;
        film.edges = vec![(20, 24), (540, 578)];
        let found = fit(&film, 120);
        assert_eq!(found.frames.len(), 3, "{:?}", found.frames);
        holds(&found, &[30, 162, 294], 120);
    }

    /// A holder with nothing in it has no film in the pass to put a frame on
    #[test]
    fn an_empty_holder_holds_no_frames() {
        let film = Film::new(Vec::new(), 120, Polarity::Positive);
        let samples = film.render();
        let layout = film.layout();
        let image = Image::new(&layout, &samples).expect("the buffer is the layout's size");
        assert!(find(&image, 120).is_none());
    }

    /// The format is the frame the caller asked for and the picture is what
    /// the camera left, and a scan takes the first centered on the second
    #[test]
    fn a_scan_is_the_format_over_the_middle_of_the_picture() {
        let found = Strip {
            frames: vec![10..110, 130..230],
            pitch: 120,
            contrast: 1.0,
        };
        assert_eq!(found.scans(60, 400), vec![30..90, 150..210]);
        // Never off the end of the pass, whatever the fit said
        assert_eq!(found.scans(60, 200), vec![30..90, 140..200]);
    }
}
