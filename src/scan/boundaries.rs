//! Finding the frames on a strip, from a thumbnail of it
//!
//! 2-11-6 leaves this to the host: the unit takes one pass over everything
//! loaded and expects a table of rectangles back. The film between two frames
//! carries no picture, so a thumbnail collapsed across the sensor is a square
//! wave down the feed, and a frame is one run of it.
//!
//! A frame is looked for as a pair of edges the film's own length apart, rather
//! than as one edge at a time. The cut end of the film and the empty gate past
//! it are the strongest edges in the pass and belong to no frame, and requiring
//! both ends of a frame is what leaves them out.
//!
//! Sums and differences of samples throughout, so there is nothing to round.
//!
//! Thanks to @toesoe, who worked out that a collapsed thumbnail is all this
//! takes.

use crate::protocol::{decode::Image, window::Channel};
use tracing::*;

/// Fraction of the rows dropped from each side before a column is summed
///
/// The pass covers the adapter's whole opening, whose edges are the holder
/// rather than film
const TRIM: usize = 8;

/// An edge is measured over the frame's length divided by this, either side
///
/// Scales with the film rather than with the resolution, so the same fraction
/// of a frame is looked at whatever a unit thumbnails at
const REACH: usize = 24;

/// A start has to score this fraction of the best one to be a frame
const THRESHOLD: i64 = 4;

/// Which way a frame reads against the film between the frames
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    /// Frames are the brighter part. Unexposed slide film develops to maximum
    /// density, so the film between the frames is the darkest thing on a strip
    Positive,
    /// Frames are the darker part. An unexposed negative develops to its base,
    /// which is the brightest thing on a strip
    Negative,
}

impl Polarity {
    /// Which way an edge into a frame runs
    const fn sign(self) -> i64 {
        match self {
            Self::Positive => 1,
            Self::Negative => -1,
        }
    }
}

/// A frame a thumbnail shows
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    /// Column of the thumbnail the frame starts at
    pub col: usize,
    /// Whether both of this frame's edges showed, or the row is where the pitch
    /// says a frame that neither edge showed has to be
    pub measured: bool,
}

/// What a strip turned out to hold
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub frames: Vec<Found>,
    /// Which way the frames read, as given or as worked out
    pub polarity: Polarity,
    /// Columns from one frame to the next, which is a frame plus the film wound
    /// on between two of them. 0 where there was only one frame to measure it
    /// from
    pub pitch: usize,
}

/// The frames in a thumbnail
///
/// `length` is the frame's extent along the feed in columns of `image`, which
/// runs across the image's columns with the sensor down its rows: the film
/// format, which nothing advertises. A `polarity` of `None` reads the strip
/// both ways and keeps whichever it answers to.
///
/// A frame at either end of the strip that shows neither of its edges is left
/// out rather than guessed at. Inside the run there is a frame on each side of
/// it to say it is there; outside it there is nothing.
pub fn detect(image: &Image, length: usize, polarity: Option<Polarity>) -> Detected {
    let profile = profile(image);
    let steps = steps(&profile, (length / REACH).max(1));
    let read = |polarity| {
        let score = pairs(&steps, length, polarity);
        let starts = starts(&score, length);
        (polarity, starts)
    };

    // Read the wrong way round, a strip has its frames where the film between
    // them is, and answers with far less
    let (polarity, starts) = match polarity {
        Some(polarity) => read(polarity),
        None => [read(Polarity::Positive), read(Polarity::Negative)]
            .into_iter()
            .max_by_key(|(_, starts)| starts.iter().map(|(_, score)| score).sum::<i64>())
            .expect("a strip reads one of two ways"),
    };

    let columns: Vec<usize> = starts.iter().map(|(col, _)| *col).collect();
    let pitch = pitch(&columns);
    let frames = ladder(&columns, pitch);
    debug!(?polarity, pitch, found = frames.len(), "measured the strip");
    Detected {
        frames,
        polarity,
        pitch,
    }
}

/// Each column of the thumbnail as one number, summed across the film
///
/// A frame ends wherever the picture does, in every channel at once, so the
/// colors are summed together. Infrared reads what is in the way rather than
/// what was photographed and says nothing about where a frame ends, so it is
/// left out of a pass that has anything else.
fn profile(image: &Image) -> Vec<u64> {
    let stride = image.channels.len();
    let mut summed: Vec<usize> = (0..stride)
        .filter(|c| Channel::from(image.channels[*c]).is_color())
        .collect();
    if summed.is_empty() {
        summed = (0..stride).collect();
    }

    let trim = image.rows / TRIM;
    let band = trim..image.rows - trim;
    (0..image.cols)
        .map(|x| {
            band.clone()
                .map(|y| {
                    let row = image.row(y);
                    summed
                        .iter()
                        .map(|c| u64::from(row[x * stride + c]))
                        .sum::<u64>()
                })
                .sum()
        })
        .collect()
}

/// How much the film brightens across each column
///
/// The difference between the `reach` columns after a column and the `reach`
/// before it, so a transition narrower than that reads as one step rather than
/// as a ramp. Columns without a full window on both sides score 0: the ends of
/// the pass are the holder rather than film.
fn steps(profile: &[u64], reach: usize) -> Vec<i64> {
    let sum = |cols: &[u64]| cols.iter().sum::<u64>() as i64;
    let mut out = vec![0i64; profile.len()];
    for x in reach..profile.len().saturating_sub(reach) {
        out[x] = sum(&profile[x..x + reach]) - sum(&profile[x - reach..x]);
    }
    out
}

/// How much each column looks like the start of a frame
///
/// A frame is two edges `length` apart running opposite ways, and it is only as
/// convincing as its weaker end. One strong edge on its own scores nothing,
/// which is what keeps the end of the film and the empty gate out of the table.
fn pairs(steps: &[i64], length: usize, polarity: Polarity) -> Vec<i64> {
    let sign = polarity.sign();
    (0..steps.len())
        .map(|x| match steps.get(x + length) {
            Some(end) => (sign * steps[x]).min(-sign * end).max(0),
            None => 0,
        })
        .collect()
}

/// The columns a frame starts at, in order down the film
///
/// Frames cannot overlap, so of two starts less than a frame apart only the
/// stronger is real. A column scoring under a fraction of the best is not the
/// edge of a frame but something inside one.
fn starts(score: &[i64], length: usize) -> Vec<(usize, i64)> {
    let best = score.iter().copied().max().unwrap_or(0);
    if best <= 0 {
        return Vec::new();
    }

    let mut ranked: Vec<(usize, i64)> = score
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, score)| score * THRESHOLD >= best)
        .collect();
    ranked.sort_by_key(|(col, score)| (std::cmp::Reverse(*score), *col));

    let mut kept: Vec<(usize, i64)> = Vec::new();
    for (col, score) in ranked {
        if kept.iter().all(|(taken, _)| taken.abs_diff(col) >= length) {
            kept.push((col, score));
        }
    }
    kept.sort_by_key(|(col, _)| *col);
    kept
}

/// Columns from one frame to the next
///
/// The film is wound on by the same amount between every pair of frames on a
/// strip, so the middle spacing stands for all of them and one start in the
/// wrong place cannot move it. A frame that showed neither edge only ever makes
/// a spacing a whole multiple too big, never too small, so of two middles the
/// lower is the one to keep.
fn pitch(columns: &[usize]) -> usize {
    let mut gaps: Vec<usize> = columns.windows(2).map(|pair| pair[1] - pair[0]).collect();
    gaps.sort_unstable();
    gaps.get(gaps.len().saturating_sub(1) / 2)
        .copied()
        .unwrap_or(0)
}

/// Every frame the run of starts accounts for, including the ones it skips
///
/// A frame with no picture in it, an unexposed one on slide film say, shows
/// neither of its edges and leaves a hole in an otherwise even run. The frames
/// on each side of the hole are what says it is a frame rather than a gap.
fn ladder(columns: &[usize], pitch: usize) -> Vec<Found> {
    let Some(&first) = columns.first() else {
        return Vec::new();
    };
    let mut out = vec![Found {
        col: first,
        measured: true,
    }];
    if pitch == 0 {
        return out;
    }

    for pair in columns.windows(2) {
        // Rounded, so a run that drifts by a few columns still counts as one
        // apart
        let apart = (pair[1] - pair[0] + pitch / 2) / pitch;
        for n in 1..apart {
            out.push(Found {
                col: pair[0] + n * pitch,
                measured: false,
            });
        }
        out.push(Found {
            col: pair[1],
            measured: true,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::image::Layout;

    /// A thumbnail of a strip: `frames` runs of `length` columns `pitch` apart,
    /// each holding a picture, on film that is flat between them
    ///
    /// `skip` names a frame that came out blank, so neither of its edges shows
    fn strip(
        frames: usize,
        first: usize,
        length: usize,
        pitch: usize,
        polarity: Polarity,
        skip: Option<usize>,
    ) -> (Vec<u16>, usize, usize) {
        let (sensor, feed) = (16usize, first + frames * pitch + length);
        // Film between the frames, either side of what a picture averages
        let (between, picture) = match polarity {
            Polarity::Positive => (2000u16, 20000u16),
            Polarity::Negative => (40000u16, 20000u16),
        };

        let mut samples = vec![0u16; sensor * feed * 3];
        for x in 0..feed {
            let inside = (0..frames).find(|n| {
                let top = first + n * pitch;
                (top..top + length).contains(&x)
            });
            let level = match inside {
                Some(n) if Some(n) != skip => {
                    // A picture, so the column is not flat and no two are alike
                    picture + (x % 32) as u16 * 300
                }
                _ => between,
            };
            for y in 0..sensor {
                for c in 0..3 {
                    samples[(y * feed + x) * 3 + c] = level;
                }
            }
        }
        (samples, sensor, feed)
    }

    fn image(samples: &[u16], sensor: usize, feed: usize) -> Image<'_> {
        // The single-line ordering, whose columns are the feed and rows the
        // sensor
        let layout = Layout::single_line(sensor as u32, feed as u32, vec![1, 2, 3]);
        Image::new(&layout, samples).expect("the buffer is the layout's size")
    }

    #[test]
    fn every_frame_of_an_even_strip_is_found() {
        for polarity in [Polarity::Positive, Polarity::Negative] {
            let (samples, sensor, feed) = strip(4, 30, 120, 132, polarity, None);
            let found = detect(&image(&samples, sensor, feed), 120, Some(polarity));

            assert_eq!(found.pitch, 132, "{polarity:?}");
            assert_eq!(
                found.frames,
                (0..4)
                    .map(|n| Found {
                        col: 30 + n * 132,
                        measured: true
                    })
                    .collect::<Vec<_>>(),
                "{polarity:?}"
            );
        }
    }

    /// Nothing has to say which way the film reads: the wrong way round looks
    /// for frames where the film between them is
    #[test]
    fn the_polarity_comes_out_of_the_strip() {
        for polarity in [Polarity::Positive, Polarity::Negative] {
            let (samples, sensor, feed) = strip(3, 30, 120, 132, polarity, None);
            let found = detect(&image(&samples, sensor, feed), 120, None);
            assert_eq!(found.polarity, polarity);
            assert_eq!(found.frames.len(), 3, "{polarity:?}");
        }
    }

    /// A blank frame shows neither edge, and the frames around it are what put
    /// it back
    #[test]
    fn a_frame_with_no_picture_in_it_still_gets_a_place() {
        let (samples, sensor, feed) = strip(4, 30, 120, 132, Polarity::Positive, Some(2));
        let found = detect(
            &image(&samples, sensor, feed),
            120,
            Some(Polarity::Positive),
        );

        assert_eq!(found.frames.len(), 4);
        assert_eq!(
            found.frames[2],
            Found {
                col: 30 + 2 * 132,
                measured: false
            }
        );
        assert!(
            found
                .frames
                .iter()
                .enumerate()
                .all(|(n, f)| f.measured == (n != 2))
        );
    }

    /// The cut end of the film and the empty gate past it are the strongest
    /// edges in the pass and belong to no frame
    #[test]
    fn the_end_of_the_film_is_not_a_frame() {
        let (mut samples, sensor, feed) = strip(2, 30, 120, 132, Polarity::Positive, None);
        let tail = 30 + 2 * 132;
        for y in 0..sensor {
            for s in &mut samples[(y * feed + tail) * 3..(y * feed + feed) * 3] {
                *s = u16::MAX;
            }
        }
        let found = detect(
            &image(&samples, sensor, feed),
            120,
            Some(Polarity::Positive),
        );
        assert_eq!(found.frames.len(), 2);
        assert!(found.frames.iter().all(|f| f.col < tail));
    }

    #[test]
    fn an_empty_holder_holds_no_frames() {
        let samples = vec![2000u16; 16 * 400 * 3];
        let found = detect(&image(&samples, 16, 400), 120, None);
        assert!(found.frames.is_empty());
        assert_eq!(found.pitch, 0);
    }
}
