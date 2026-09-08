//! Where the picture is in a pass the unit positioned itself
//!
//! Where a unit positions the film by its perforation table, the window does
//! not say where the frame appears. The film is latched by the record at the
//! frame's own line, and the frame shows up wherever that leaves it. So the
//! pass has to be read.
//!
//! Bare film sits at the end of the pass's own range whatever was
//! photographed: the brightest thing a negative holds, the densest a slide
//! does. The picture is what lies inside the run of it at each end. The film
//! type says which end is which, and that is all it says.
//!
//! A level here, not the texture `scan::strip` reads. A thumbnail line is a
//! third of a millimeter of film and averages the grain away. A pass at
//! scanning resolution does not, so bare film in one is not flat.
//!
//! Thanks to @toesoe, who worked out that a collapsed thumbnail is all this
//! takes.

use crate::protocol::decode::Image;
use std::ops::Range;

/// Rows to drop at each end of the sensor: the opening's edges are holder
const TRIM: usize = 8;

/// The share of columns taken to be bare film, and the share taken to be
/// picture. A positioned pass has film at both ends and picture between, so
/// neither is a small part of it
const FILM: f32 = 0.98;
const BULK: f32 = 0.50;

/// How far from bare film's own level a column may read and still be film, as
/// a share of the way to what the picture reads. Film carries the mask, the
/// base and whatever the lamp does across the gate
const TOLERANCE: f32 = 0.15;

/// The fewest columns that count as a run of film, so one blown highlight in a
/// picture is not the film beside it
const RUN: usize = 3;

/// How far bare film has to read from the picture before there is any film in
/// the pass, as a share of full scale
const SEPARATION: f32 = 0.05;

/// Which way the loaded film reads, which nothing in a pass can say for itself
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    /// Slide film, where unexposed film is the densest thing on the strip
    Positive,
    /// Negative film, where unexposed film is the brightest thing on the strip
    Negative,
}

/// A frame to be found in a pass the unit positioned
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Picture {
    /// Which way the loaded film reads
    pub polarity: Polarity,
    /// The frame's own length along the feed, which a pass that clipped the
    /// picture is measured back from
    pub extent: u32,
}

/// The columns of a positioned pass that hold the picture
///
/// The picture lies between the run of bare film before it and the run after
/// it. A pass that opened inside the picture has no run in front and comes
/// back starting at column 0. One that closed inside it comes back ending at
/// the last column. The whole pass where there is no film in it at all, which
/// is a frame nobody exposed or a rectangle from the middle of one
pub fn locate(image: &Image, polarity: Polarity) -> Range<usize> {
    let band = TRIM..image.rows.saturating_sub(TRIM);
    // The pass's own full scale. A pass read before anything stretched it does
    // not fill 16 bits
    let full = match image.bits {
        1..16 => ((1u32 << image.bits) - 1) as f32,
        _ => f32::from(u16::MAX),
    };
    let planes = image.colors.len().max(1);
    let levels: Vec<f32> = (0..image.cols)
        .map(|x| {
            let mut sum = 0.0;
            for plane in &image.colors {
                sum += band
                    .clone()
                    .map(|y| f32::from(plane[y * image.cols + x]) / full)
                    .sum::<f32>()
                    / band.len().max(1) as f32;
            }
            sum / planes as f32
        })
        .collect();
    if levels.len() < 2 {
        return 0..levels.len();
    }

    let mut sorted = levels.clone();
    sorted.sort_by(f32::total_cmp);
    let at = |q: f32| sorted[((sorted.len() as f32 * q) as usize).min(sorted.len() - 1)];
    let bare = match polarity {
        Polarity::Negative => at(FILM),
        Polarity::Positive => at(1.0 - FILM),
    };
    let bulk = at(BULK);
    // Nothing in the pass reads anything like bare film, so none of it is
    if (bare - bulk).abs() < SEPARATION {
        return 0..levels.len();
    }
    let cut = bare + (bulk - bare) * TOLERANCE;
    let is_film = |x: usize| match polarity {
        Polarity::Negative => levels[x] >= cut,
        Polarity::Positive => levels[x] <= cut,
    };

    // The last run before the middle of the pass and the first run after it
    let middle = image.cols / 2;
    let (mut start, mut end) = (0, image.cols);
    let mut x = 0;
    while x < levels.len() {
        if !is_film(x) {
            x += 1;
            continue;
        }
        let from = x;
        while x + 1 < levels.len() && is_film(x + 1) {
            x += 1;
        }
        let run = from..x + 1;
        if run.len() >= RUN {
            if run.end <= middle {
                start = run.end;
            } else if run.start >= middle && end == image.cols {
                end = run.start;
            }
        }
        x += 1;
    }
    start..end.max(start)
}

/// Where the picture starts, in columns, which is negative where the pass
/// opened inside it
///
/// A pass that opens inside the picture is the one that most needs moving and
/// the one with no film in front of it, so this measures from the film behind
/// the picture instead, `extent` columns back. `found` is [`locate`]'s answer
/// over a pass of `cols` columns. `None` where the pass shows no bare film,
/// which places nothing
pub fn start(found: &Range<usize>, cols: usize, extent: usize) -> Option<i32> {
    if found.start > 0 {
        return Some(found.start as i32);
    }
    match found.end < cols {
        true => Some(found.end as i32 - extent as i32),
        false => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::{decode::Samples, image::Layout};

    pub(crate) const SENSOR: usize = 64;

    /// One column of a thumbnail, down the sensor. `contrast` is what tells a
    /// picture from film with nothing on it
    fn column(plane: &mut [u16], feed: usize, x: usize, level: u16, contrast: f32) {
        for y in 0..SENSOR {
            // Nothing periodic with the frame, so no run of columns is alike
            let swing = ((y * 7 + x * 3) % 11) as f32 / 11.0 - 0.5;
            let v = f32::from(level) * (1.0 + contrast * swing);
            plane[y * feed + x] = v.clamp(0.0, f32::from(u16::MAX)) as u16;
        }
    }

    /// What a strip of film reads at, by polarity
    struct Levels {
        /// The film between two frames, which is unexposed
        between: u16,
        /// What a picture averages
        picture: u16,
    }

    fn levels(polarity: Polarity) -> Levels {
        match polarity {
            // Unexposed slide is maximum density, just above the holder
            Polarity::Positive => Levels {
                between: 700,
                picture: 9000,
            },
            // Unexposed negative is base plus mask, the brightest film there is
            Polarity::Negative => Levels {
                between: 30000,
                picture: 8000,
            },
        }
    }

    /// A thumbnail of a strip
    ///
    /// `frames` gives each start, all `length` long. `flat` names one whose
    /// picture has no variation, `blank` one never exposed.
    pub(crate) struct Strip {
        pub(crate) feed: usize,
        pub(crate) length: usize,
        pub(crate) polarity: Polarity,
        pub(crate) frames: Vec<usize>,
        pub(crate) flat: Option<usize>,
        pub(crate) blank: Option<usize>,
        /// Columns of bare backlight past the end of the film
        pub(crate) gate: Option<(usize, usize)>,
        /// Columns of holder mask before the film starts
        pub(crate) mask: usize,
        /// Stretches where the sensor sees a step rather than film: the edge
        /// of the holder's opening, or the cut end of the strip. Every row of
        /// such a column differs, the way a picture's does
        pub(crate) edges: Vec<(usize, usize)>,
    }

    impl Strip {
        pub(crate) fn new(frames: Vec<usize>, length: usize, polarity: Polarity) -> Self {
            let feed = frames.iter().max().unwrap_or(&0) + length + 60;
            Self {
                feed,
                length,
                polarity,
                frames,
                flat: None,
                blank: None,
                gate: None,
                mask: 0,
                edges: Vec::new(),
            }
        }

        pub(crate) fn render(&self) -> Samples {
            let level = levels(self.polarity);
            let mut colors = vec![vec![0u16; SENSOR * self.feed]; 3];

            for x in 0..self.feed {
                let inside = self
                    .frames
                    .iter()
                    .position(|&top| (top..top + self.length).contains(&x));

                let (value, contrast) = match inside {
                    // A step across the sensor, whatever the film beneath it
                    _ if self.edges.iter().any(|&(a, b)| (a..b).contains(&x)) => (30000, 0.95),
                    _ if x < self.mask => (140, 0.10),
                    _ if self.gate.is_some_and(|(a, b)| (a..b).contains(&x)) => (65200, 0.0),
                    Some(n) if Some(n) == self.blank => (level.between, 0.0),
                    Some(n) if Some(n) == self.flat => (level.picture, 0.0),
                    // A picture, which is never the same twice down the sensor
                    Some(_) => (level.picture, 0.55),
                    None => (level.between, 0.0),
                };
                for plane in &mut colors {
                    column(plane, self.feed, x, value, contrast);
                }
            }
            Samples { colors, ir: None }
        }

        /// The layout [`Self::render`]'s samples are read back through
        pub(crate) fn layout(&self) -> Layout {
            Layout::single_line(SENSOR as u32, self.feed as u32, vec![1, 2, 3])
        }
    }

    /// A pass a positioned unit returns: film edge to edge, one frame in it.
    /// The frame is where the unit put it, not where the window asked
    fn positioned(frames: Vec<usize>, length: usize, polarity: Polarity) -> (Samples, usize) {
        let mut strip = Strip::new(frames, length, polarity);
        strip.feed = strip.frames.iter().max().unwrap_or(&0) + length + 23;
        let feed = strip.feed;
        (strip.render(), feed)
    }

    fn located(samples: &Samples, feed: usize, polarity: Polarity) -> Range<usize> {
        let layout = Layout::single_line(SENSOR as u32, feed as u32, vec![1, 2, 3]);
        let image = Image::new(&layout, samples).expect("the buffer is the layout's size");
        super::locate(&image, polarity)
    }

    /// The picture is found by the film either side of it, at whatever offset
    /// into the pass the unit left it
    #[test]
    fn a_positioned_frame_is_found_wherever_it_sits() {
        for polarity in [Polarity::Positive, Polarity::Negative] {
            for gap in [17, 37] {
                let (samples, feed) = positioned(vec![gap], 120, polarity);
                let found = located(&samples, feed, polarity);
                assert!(
                    found.start.abs_diff(gap) <= 2 && found.end.abs_diff(gap + 120) <= 2,
                    "{polarity:?} at {gap}: got {found:?}, wanted {gap}..{}",
                    gap + 120
                );
            }
        }
    }

    /// A pass that opened inside the picture has no film in front of it, and
    /// says so by starting at column 0. The caller has nothing to place the
    /// frame by and must leave it where it was
    #[test]
    fn a_pass_that_opens_inside_the_picture_says_so() {
        let (samples, feed) = positioned(vec![0], 120, Polarity::Negative);
        assert_eq!(located(&samples, feed, Polarity::Negative).start, 0);
    }

    /// An unexposed frame is the same film as the gap around it, so the pass
    /// is film end to end and there is no picture in it to find
    #[test]
    fn a_frame_that_was_never_exposed_has_no_picture() {
        let mut strip = Strip::new(vec![30], 120, Polarity::Positive);
        strip.blank = Some(0);
        strip.feed = 173;
        let samples = strip.render();
        assert_eq!(
            located(&samples, strip.feed, Polarity::Positive).start,
            0,
            "nothing in the pass places this frame"
        );
    }

    /// A picture with no variation in it is still a picture: what tells it
    /// from the film around it is the level, not the texture
    #[test]
    fn a_flat_picture_is_found_by_its_level() {
        for polarity in [Polarity::Positive, Polarity::Negative] {
            let mut strip = Strip::new(vec![29], 120, polarity);
            strip.flat = Some(0);
            strip.feed = 172;
            let samples = strip.render();
            let found = located(&samples, strip.feed, polarity);
            assert!(
                found.start.abs_diff(29) <= 2 && found.end.abs_diff(149) <= 2,
                "{polarity:?}: got {found:?}"
            );
        }
    }
}
