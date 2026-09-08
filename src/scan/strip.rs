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
//! The count comes from the holder. A frame nobody exposed reads as bare film,
//! so a count measured from the pass leaves it out, and a blank scan is a
//! better answer than a missing one.
//!
//! The format bounds the pitch search. It is not the answer: a camera gate is
//! not the rectangle the caller scans.

use crate::protocol::decode::Image;
use std::ops::Range;
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

/// The share taken to be bare film. Halfway between this and [`SCALE`] is
/// where a column stops being film
const BARE: f32 = 0.10;

/// The pitch to search, in twentieths of the format. A short wind leaves the
/// frames almost touching and a long one leaves film between them
const PITCH: Range<usize> = 18..31;

/// The fewest columns of picture a frame may have
const PICTURE: usize = 8;

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
    /// A pass runs from the holder to past the end of the film. Neither of
    /// those holds a picture, so neither holds a frame
    fn film(&self) -> Range<usize> {
        let mut sorted = self.at.clone();
        sorted.sort_by(f32::total_cmp);
        let at = |q: f32| sorted[((sorted.len() as f32 * q) as usize).min(sorted.len() - 1)];
        let level = (at(BARE) + at(SCALE)) / 2.0;
        let (Some(first), Some(last)) = (
            self.at.iter().position(|&v| v > level),
            self.at.iter().rposition(|&v| v > level),
        ) else {
            // Flat end to end: an empty holder, or a pass that saw no film
            return 0..0;
        };
        first..(last + 1).min(self.at.len())
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
/// search. `frames` is the count the holder takes. Without it the film the
/// pass found sets the count, which leaves out any frame nobody exposed.
/// `None` where the pass holds no frames
pub fn find(image: &Image, frames: Option<usize>, length: usize) -> Option<Strip> {
    if length == 0 || image.cols == 0 {
        return None;
    }
    let detail = Detail::new(detail(image));
    let cols = image.cols;
    let film = detail.film();

    // The format is shorter than the pitch, because a wind leaves film between
    // the frames, so this rounds down to the frames that fit
    let frames = frames.unwrap_or(film.len() / length);
    debug!(?film, cols, frames, "the picture in the pass");
    if frames == 0 {
        return None;
    }

    let least = PICTURE + 2 * REACH + 2;
    let mut best: Option<(f32, usize, usize)> = None;
    for pitch in (length * PITCH.start / 20).max(least)..=(length * PITCH.end / 20).max(least) {
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
                best = Some((at, first, pitch));
            }
        }
    }

    let (contrast, first, pitch) = best?;
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

    /// Fit a rendered strip, telling it `frames` where the caller would know
    fn fit(film: &Film, frames: Option<usize>, length: usize) -> Strip {
        let samples: Samples = film.render();
        let layout = film.layout();
        let image = Image::new(&layout, &samples).expect("the buffer is the layout's size");
        find(&image, frames, length).expect("a strip with frames on it")
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
        let found = fit(&film, Some(4), 120);
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
            .map(|polarity| fit(&Film::new(want.to_vec(), 120, polarity), Some(4), 120))
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
        holds(&fit(&film, Some(3), 120), &[30, 162, 294], 120);
    }

    /// A frame under the holder mask is still where it is. Moved down to clear
    /// the mask it would crop the picture that is showing
    #[test]
    fn a_frame_behind_the_holder_mask_keeps_its_place() {
        let mut film = Film::new(vec![20, 152, 284], 120, Polarity::Positive);
        film.mask = 40;
        holds(&fit(&film, Some(3), 120), &[20, 152, 284], 120);
    }

    /// A flat picture reads as evenly as a gap, and the ladder is what puts it
    /// back: the other frames say where this one has to be
    #[test]
    fn a_flat_picture_is_still_a_frame() {
        for polarity in [Polarity::Positive, Polarity::Negative] {
            let mut film = Film::new(vec![30, 162, 294], 120, polarity);
            film.flat = Some(1);
            holds(&fit(&film, Some(3), 120), &[30, 162, 294], 120);
        }
    }

    /// An unexposed frame is the same film as the gap around it. Nothing shows
    /// there, so the count and the wind are the whole of what places it, which
    /// is the reason the count is asked for rather than measured
    #[test]
    fn a_frame_with_no_picture_in_it_still_gets_a_place() {
        let mut film = Film::new(vec![30, 162, 294, 426], 120, Polarity::Positive);
        film.blank = Some(2);
        holds(&fit(&film, Some(4), 120), &[30, 162, 294, 426], 120);
    }

    /// A wind barely longer than the gate leaves the frames all but touching,
    /// and the pitch comes back that short rather than being rounded up to
    /// something a strip that fits the format better would have
    #[test]
    fn a_short_wind_comes_back_short() {
        let film = Film::new(vec![30, 155, 280], 120, Polarity::Negative);
        let found = fit(&film, Some(3), 120);
        assert!(
            found.pitch.abs_diff(125) <= REACH,
            "pitch {} in {:?}",
            found.pitch,
            found.frames
        );
        holds(&found, &[30, 155, 280], 120);
    }

    /// Where the holder does not say, the film the pass found has room for so
    /// many frames of the format and no more
    #[test]
    fn the_film_says_how_many_frames_when_nothing_else_does() {
        let film = Film::new(vec![30, 162, 294, 426], 120, Polarity::Negative);
        let found = fit(&film, None, 120);
        holds(&found, &[30, 162, 294, 426], 120);
    }

    /// A holder with nothing in it has no film in the pass to put a frame on
    #[test]
    fn an_empty_holder_holds_no_frames() {
        let film = Film::new(Vec::new(), 120, Polarity::Positive);
        let samples = film.render();
        let layout = film.layout();
        let image = Image::new(&layout, &samples).expect("the buffer is the layout's size");
        assert!(find(&image, None, 120).is_none());
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
