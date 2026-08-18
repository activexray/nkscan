//! Finding the frames on a strip, from a thumbnail of it
//!
//! 2-11-6 leaves this to the host: the unit takes one pass over everything
//! loaded and expects a table of rectangles back.
//!
//! Brightness cannot tell a frame from the film between two frames. Unexposed
//! slide is as dark as a shadow, and the holder and the bare gate are the
//! extremes of the whole pass. What does separate them is that unexposed film
//! is even down the sensor and a picture is not, so a column is scored on how
//! much it varies. A flat picture needs the film type as well: that says which
//! side of the unexposed level a picture lies.
//!
//! Frames are then placed by tiling the whole strip at once rather than by
//! picking edges one at a time, so a strip that under-advanced comes back with
//! frames that overlap and the film they share in both.
//!
//! The frame length is the caller's: with a few frames on a strip the holder
//! and backlight edges dominate any autocorrelation, so it cannot be measured.
//!
//! Ratios and logarithms throughout, so floats here where the rest of the pass
//! handling is samples.
//!
//! Thanks to @toesoe, who worked out that a collapsed thumbnail is all this
//! takes.

use super::meter::ceiling;
use crate::protocol::decode::Image;
use tracing::*;

// ----- reading the film

/// Rows dropped from each end of the sensor, as a fraction: the opening's edges
/// are holder
const TRIM: usize = 8;

/// Added to a column's level before dividing its variation by it, as a fraction
/// of full scale. Without it the holder's read noise reads as a picture
const FLOOR: f32 = 0.01;

/// At or under this fraction of full scale the pass carried nothing
///
/// Near zero on purpose. A 35mm feeder reads a flat zero over the travel past
/// the film, and an underexposed slide's darkest frames sit not far above it
const DARK: f32 = 0.001;

/// Over this fraction of full scale a column is the bare gate: film always
/// attenuates something
const BRIGHT: f32 = 0.98;

/// Past this multiple of a film's own reach is the holder, not film. Nothing
/// else keeps it off the picture's side of the level test on a negative
const OVERSHOOT: f32 = 1.5;

/// The same the other way, past the unexposed film itself
const UNDERSHOOT: f32 = 0.25;

/// How far along a film's reach a flat column has to sit to be a picture
///
/// A fraction rather than a density. A thin negative holds its whole picture
/// close to its base, which no fixed distance separates
const SPREAD: f32 = 0.5;

/// The least that may come to, in density
const SPREAD_FLOOR: f32 = 0.08;

/// How far into the tail of the flat columns the unexposed film sits, in
/// thousandths
///
/// Well in: a 35mm wind is short enough that the film between two frames is a
/// twentieth of everything flat on the strip
const TAIL: usize = 50;

/// The shortest run of flat film worth a reading: the frame length over this
const FLAT_RUN: usize = 24;

/// The narrowest run of picture worth keeping: the frame length over this
///
/// Skewed film puts part of one column past its cut edge and the rest on film,
/// which is the strongest step in the pass over a couple of columns
const SPECK: usize = 32;

// ----- placing the frames

/// How much a column has to look like a picture to count as one
const THETA: f32 = 0.5;

/// The closest two frames may start, as a fraction of the frame length. Under
/// one, so a transport that under-advanced leaves two frames sharing film
const MIN_PITCH: f32 = 0.75;

/// What an edge at each end of a frame is worth, as a fraction of the frame
/// length. Large, because within a picture the body score barely moves
const EDGE_BONUS: f32 = 1.0;

/// What placing a frame costs, so a marginal one is not worth adding
const FRAME_COST: f32 = 0.08;

/// How far either side of an end an edge is measured: the frame length over
/// this. Narrow enough to resolve the gap a 35mm wind leaves
const EDGE_REACH: usize = 24;

/// How far a spacing may sit off a whole number of pitches and still be even:
/// the pitch over this
const EVEN: usize = 10;

// ----- what comes out

/// Which way a frame reads against the film between the frames
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    /// Frames are the less dense part: unexposed slide is maximum density
    Positive,
    /// Frames are the denser part: an unexposed negative is its own base
    Negative,
}

impl Polarity {
    /// Which way a picture lies from the unexposed film's density
    const fn sign(self) -> f32 {
        match self {
            Self::Positive => 1.0,
            Self::Negative => -1.0,
        }
    }
}

/// What a strip turned out to hold
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    /// The column of the thumbnail each frame starts at
    pub frames: Vec<usize>,
    /// Columns from one frame to the next, less than a frame where two
    /// overlap. 0 with nothing to measure it from
    pub pitch: usize,
}

/// The frames in a thumbnail
///
/// `length` is the film format in columns of `image`, whose columns are the
/// feed and rows the sensor. `polarity` comes from the film type.
///
/// Frames may come back overlapping, and are left that way: the film really is
/// in both, and each keeps the edge it was found by.
pub fn detect(image: &Image, length: usize, polarity: Polarity) -> Detected {
    let columns = columns(image);
    let split = otsu(&columns.texture);
    let unexposed = Unexposed::measure(&columns, split, polarity, length);

    let (mut picture, film) = score_columns(&columns, split, unexposed.as_ref());
    open(&mut picture, (length / SPECK).max(1));

    // Only film with nothing on it marks an edge. The holder is as blank as any
    // gap and sits where a strip's first frame begins, so without this a frame
    // registers to the film's cut edge
    let gap: Vec<f32> = film
        .iter()
        .zip(&picture)
        .map(|(&film, &picture)| match film {
            true => 1.0 - picture,
            false => 0.0,
        })
        .collect();

    let min_pitch = ((length as f32 * MIN_PITCH) as usize).max(1);
    let starts = tile(&picture, &gap, length, min_pitch);
    let pitch = pitch(&starts);
    let frames = ladder(&starts, pitch);

    debug!(
        ?polarity,
        length,
        pitch,
        found = frames.len(),
        "measured the strip"
    );
    Detected { frames, pitch }
}

// ----- what the film itself reads at

/// Where a strip's unexposed film sits and how far its pictures reach from
/// there, in density
///
/// A slide puts two whole density between base and highlight where a thin
/// negative holds everything within a quarter of one, so both the flat-picture
/// test and the edge of the holder are the film's own rather than fixed.
struct Unexposed {
    /// The density of the film between two frames
    base: f32,
    /// How far a picture reaches from it
    reach: f32,
    /// How far off it a flat column has to sit to be a picture
    spread: f32,
    /// Which way that is, from the film type
    sign: f32,
}

impl Unexposed {
    /// What a strip's flat columns say, where they say anything
    fn measure(columns: &Columns, split: f32, polarity: Polarity, length: usize) -> Option<Self> {
        let base = base(columns, split, polarity, length)?;
        let sign = polarity.sign();

        // Only the picture's side of the unexposed film says how far it goes
        let mut off: Vec<f32> = (0..columns.density.len())
            .filter(|&x| columns.lit[x])
            .map(|x| sign * (base - columns.density[x]))
            .filter(|&off| off > 0.0)
            .collect();
        // The far end rather than the furthest: one clipped column is not the
        // scale
        off.sort_by(f32::total_cmp);
        let reach = match off.is_empty() {
            true => SPREAD_FLOOR,
            false => off[off.len() * 9 / 10],
        };

        Some(Self {
            base,
            reach,
            spread: (reach * SPREAD).max(SPREAD_FLOOR),
            sign,
        })
    }

    /// How far a column sits off the unexposed film, the way a picture lies
    fn off(&self, density: f32) -> f32 {
        self.sign * (self.base - density)
    }

    /// Whether a column is film at all: anything outside this film's own reach
    /// is the holder
    fn is_film(&self, density: f32) -> bool {
        let off = self.off(density);
        off <= self.reach * OVERSHOOT && off >= -self.reach * UNDERSHOOT
    }

    /// How much a column with no variation in it looks like a picture, from 0
    /// to 1
    fn flat_picture(&self, density: f32) -> f32 {
        (self.off(density) / self.spread).clamp(0.0, 1.0)
    }
}

/// Drop the runs of picture too narrow to be one
///
/// Erosion then dilation, `width` either side: takes out anything narrower and
/// leaves everything wider where it was.
fn open(picture: &mut [f32], width: usize) {
    let window = |v: &[f32], x: usize, pick: fn(f32, f32) -> f32| {
        let (from, to) = (x.saturating_sub(width), (x + width + 1).min(v.len()));
        v[from..to].iter().copied().fold(v[x], pick)
    };
    let eroded: Vec<f32> = (0..picture.len())
        .map(|x| window(picture, x, f32::min))
        .collect();
    for (x, wide) in picture.iter_mut().enumerate() {
        *wide = window(&eroded, x, f32::max);
    }
}

/// What each column of the thumbnail looks like, across the film
struct Columns {
    /// How much a column varies down the sensor, against its own level: the
    /// same whatever the exposure and whatever the orange mask does to a channel
    texture: Vec<f32>,
    /// The column's level as `log10(full scale / mean)`
    density: Vec<f32>,
    /// Whether the column is film at all. No light gets through the holder, and
    /// nothing attenuates the bare gate
    lit: Vec<bool>,
}

/// Measure every column of the thumbnail
fn columns(image: &Image) -> Columns {
    let full = f32::from(ceiling(image.bits));
    let (floor, dark, bright) = (full * FLOOR, full * DARK, full * BRIGHT);

    let trim = image.rows / TRIM;
    let band = trim..image.rows.saturating_sub(trim);
    let (rows, planes) = (band.len(), image.colors.len());

    let mut out = Columns {
        texture: vec![0.0; image.cols],
        density: vec![0.0; image.cols],
        lit: vec![false; image.cols],
    };
    if rows < 2 || planes == 0 {
        return out;
    }

    for x in 0..image.cols {
        let (mut texture, mut density) = (0.0f32, 0.0f32);
        // A column is only the holder, or only the gate, where every channel
        // says so
        let (mut all_dark, mut all_bright) = (true, true);

        for plane in &image.colors {
            let at = |y: usize| f32::from(plane[y * image.cols + x]);
            let level = band.clone().map(at).sum::<f32>() / rows as f32;
            let step = band
                .clone()
                .skip(1)
                .map(|y| (at(y) - at(y - 1)).abs())
                .sum::<f32>()
                / (rows - 1) as f32;

            texture += step / (level + floor);
            density += (full / level.max(1.0)).log10();
            // At or under, so a flat zero past the film counts even where the
            // cut rounds to nothing
            all_dark &= level <= dark;
            all_bright &= level > bright;
        }

        let lit = !all_dark && !all_bright;
        out.texture[x] = match lit {
            true => texture / planes as f32,
            // Not film, so there is no picture in it to measure
            false => 0.0,
        };
        out.density[x] = density / planes as f32;
        out.lit[x] = lit;
    }
    out
}

/// The texture split and what each side of it averages, which is the scale a
/// column is scored against
///
/// Both sides, so the scale is this film's own contrast: a negative carries a
/// third of a slide's. Both, because the split can land hard against one, and
/// then measuring only the other reads a gap as an even chance of a picture.
struct Contrast {
    split: f32,
    low: f32,
    top: f32,
}

impl Contrast {
    /// The two populations either side of the split, or `None` where a pass
    /// carried nothing that varies: an empty holder rather than a strip
    fn measure(texture: &[f32], split: f32) -> Option<Self> {
        let (below, above): (Vec<f32>, Vec<f32>) = texture.iter().partition(|&&t| t < split);
        let mean = |side: Vec<f32>| match side.is_empty() {
            true => None,
            false => Some(side.iter().sum::<f32>() / side.len() as f32),
        };
        Some(Self {
            split,
            low: mean(below).unwrap_or(split),
            top: mean(above).filter(|top| *top > split)?,
        })
    }

    /// A column against the population it falls in, from 0 to 1. The split is
    /// an even chance and each side's average is certain
    fn score(&self, texture: f32) -> f32 {
        match texture >= self.split {
            true => 0.5 + 0.5 * (texture - self.split) / (self.top - self.split),
            false => 0.5 - 0.5 * (self.split - texture) / (self.split - self.low).max(f32::EPSILON),
        }
        .clamp(0.0, 1.0)
    }
}

/// How much each column looks like a picture rather than the film between two
/// frames, from 0 to 1, and whether it is film at all
fn score_columns(
    columns: &Columns,
    split: f32,
    unexposed: Option<&Unexposed>,
) -> (Vec<f32>, Vec<bool>) {
    let Some(contrast) = Contrast::measure(&columns.texture, split) else {
        return (vec![0.0; columns.texture.len()], columns.lit.clone());
    };

    (0..columns.texture.len())
        .map(|x| {
            if !columns.lit[x] {
                return (0.0, false);
            }
            let varies = contrast.score(columns.texture[x]);
            let Some(film) = unexposed else {
                return (varies, true);
            };
            match film.is_film(columns.density[x]) {
                true => (varies.max(film.flat_picture(columns.density[x])), true),
                false => (0.0, false),
            }
        })
        .unzip()
}

/// The density of the film between the frames, where the strip shows any
///
/// Only a flat run with a picture each side. The holder and the gate are flat
/// too but sit at the ends of the pass, and neither is lit.
fn base(columns: &Columns, split: f32, polarity: Polarity, length: usize) -> Option<f32> {
    let shortest = (length / FLAT_RUN).max(4);
    let mut flat: Vec<f32> = Vec::new();

    for (start, end) in runs(&columns.texture, split) {
        if start == 0 || end == columns.texture.len() || end - start < shortest {
            continue;
        }
        flat.extend(
            (start..end)
                .filter(|&x| columns.lit[x])
                .map(|x| columns.density[x]),
        );
    }
    if flat.len() < shortest {
        return None;
    }

    // Well into the tail: a flat run is not always a gap, since an even sky is
    // flat too and one run often spans a gap and the picture beside it
    flat.sort_by(f32::total_cmp);
    let last = flat.len() - 1;
    let tail = last * TAIL / 1000;
    Some(match polarity {
        Polarity::Positive => flat[last - tail],
        Polarity::Negative => flat[tail],
    })
}

/// The runs of columns under `split`, as half-open ranges
fn runs(values: &[f32], split: f32) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for x in 0..=values.len() {
        match (x < values.len() && values[x] < split, start) {
            (true, None) => start = Some(x),
            (false, Some(from)) => {
                out.push((from, x));
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// The threshold that splits a profile into its two populations
///
/// Otsu, 256 bins, so the split does not depend on how much of the pass turned
/// out to be film. A fixed percentile lands in the wrong population.
fn otsu(values: &[f32]) -> f32 {
    const BINS: usize = 256;
    let (lo, hi) = values
        .iter()
        .fold((f32::MAX, f32::MIN), |(l, h), &v| (l.min(v), h.max(v)));
    if hi <= lo {
        return lo;
    }

    let mut counts = [0usize; BINS];
    for &v in values {
        let bin = ((v - lo) / (hi - lo) * BINS as f32) as usize;
        counts[bin.min(BINS - 1)] += 1;
    }

    let total = values.len() as f64;
    let all: f64 = counts
        .iter()
        .enumerate()
        .map(|(i, &c)| i as f64 * c as f64)
        .sum();
    let (mut under, mut under_sum, mut best, mut split) = (0f64, 0f64, -1f64, 0usize);
    for (i, &count) in counts.iter().enumerate() {
        under += count as f64;
        under_sum += i as f64 * count as f64;
        let over = total - under;
        if under == 0.0 || over == 0.0 {
            continue;
        }
        let apart = under_sum / under - (all - under_sum) / over;
        let score = under * over * apart * apart;
        if score > best {
            best = score;
            split = i;
        }
    }
    lo + (split as f32 + 0.5) * (hi - lo) / BINS as f32
}

/// Prefix sums along the strip, so what a run of columns comes to is one
/// subtraction
struct Sums {
    cols: usize,
    /// Picture score less what covering a column costs
    inside: Vec<f32>,
    /// How much a column looks like the film between two frames
    between: Vec<f32>,
    /// Picture score alone
    covered: Vec<f32>,
}

impl Sums {
    fn new(picture: &[f32], gap: &[f32]) -> Self {
        let cols = picture.len();
        let mut sums = Self {
            cols,
            inside: vec![0f32; cols + 1],
            between: vec![0f32; cols + 1],
            covered: vec![0f32; cols + 1],
        };
        for x in 0..cols {
            sums.inside[x + 1] = sums.inside[x] + picture[x] - THETA;
            sums.between[x + 1] = sums.between[x] + gap[x];
            sums.covered[x + 1] = sums.covered[x] + picture[x];
        }
        sums
    }

    /// What covering `from..to` is worth
    fn worth(&self, from: usize, to: usize) -> f32 {
        self.inside[to] - self.inside[from]
    }

    /// What a run averages, kept inside the strip
    fn mean(&self, run: &[f32], (from, to): (usize, usize)) -> f32 {
        let (from, to) = (from.min(self.cols), to.min(self.cols));
        match to > from {
            true => (run[to] - run[from]) / (to - from) as f32,
            false => 0.0,
        }
    }

    /// What both ends of `from..to` are worth as edges
    fn edges(&self, from: usize, to: usize, reach: usize) -> f32 {
        self.edge((from, from + reach), (from.saturating_sub(reach), from))
            + self.edge((to.saturating_sub(reach), to), (to, to + reach))
    }

    /// What one end of a frame is worth as an edge: picture on the inside, film
    /// with nothing on it outside
    ///
    /// Multiplied, so blank both sides is worth nothing. That is what keeps a
    /// frame off the holder, which is as blank as any gap, and what makes a
    /// frame register to the one edge it can see
    fn edge(&self, inner: (usize, usize), outer: (usize, usize)) -> f32 {
        self.mean(&self.covered, inner) * self.mean(&self.between, outer)
    }
}

/// Where to put the frames: the best whole arrangement, not the best edges one
/// at a time
///
/// A covered column is worth what it looks like a picture, less what covering a
/// gap costs, and a frame is worth extra for an edge at each end. Two frames
/// may start closer than a frame is long; the film they share counts once, or
/// the score would rise for packing frames in.
fn tile(picture: &[f32], gap: &[f32], length: usize, min_pitch: usize) -> Vec<usize> {
    let cols = picture.len();
    if length == 0 || cols < length {
        return Vec::new();
    }

    let sums = Sums::new(picture, gap);
    let last = cols - length;
    let reach = (length / EDGE_REACH).max(1);
    let bonus = EDGE_BONUS * length as f32;
    let cost = FRAME_COST * length as f32;

    // What an arrangement whose last frame starts here comes to, and which
    // frame came before it
    let mut best = vec![0f32; last + 1];
    let mut prior = vec![usize::MAX; last + 1];
    // The best any start up to here comes to, and which start that was
    let mut highest = vec![(0f32, usize::MAX); last + 1];

    for start in 0..=last {
        let end = start + length;
        let edges = bonus * sums.edges(start, end, reach);
        let alone = sums.worth(start, end) + edges - cost;

        // On its own, or first after a frame that ended before this one began
        let (mut score, mut from) = (alone, usize::MAX);
        if start >= length {
            let (before, at) = highest[start - length];
            if before > 0.0 {
                (score, from) = (before + alone, at);
            }
        }

        // Or overlapping the one before, which already counted the shared film
        if start >= min_pitch {
            let first = (start + 1).saturating_sub(length);
            for (prev, before) in (first..).zip(&best[first..=start - min_pitch]) {
                let shared = before + sums.worth(prev + length, end) + edges - cost;
                if shared > score {
                    (score, from) = (shared, prev);
                }
            }
        }

        best[start] = score;
        prior[start] = from;
        highest[start] = match start > 0 && highest[start - 1].0 >= score {
            true => highest[start - 1],
            false => (score, start),
        };
    }

    // Nothing on the strip was worth a frame
    let (top, mut at) = highest[last];
    if top <= 0.0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    while at != usize::MAX {
        out.push(at);
        at = prior[at];
    }
    out.reverse();
    out
}

/// Columns from one frame to the next
///
/// The middle spacing, which one start in the wrong place cannot move. A frame
/// that showed nothing makes a spacing a multiple too big and two that overlap
/// make one too small, so of two middles keep the lower.
fn pitch(columns: &[usize]) -> usize {
    let mut gaps: Vec<usize> = columns.windows(2).map(|pair| pair[1] - pair[0]).collect();
    gaps.sort_unstable();
    gaps.get(gaps.len().saturating_sub(1) / 2)
        .copied()
        .unwrap_or(0)
}

/// Every frame the run of starts accounts for, including the ones it skips
///
/// An unexposed frame reads as the film between two frames, because that is
/// what it is. Only an even run says one is missing: a spacing that is a
/// multiple of the wind has a frame in it. A slipped transport or a pair of
/// overlapping frames says nothing, and nothing is filled in.
fn ladder(columns: &[usize], pitch: usize) -> Vec<usize> {
    let Some(&first) = columns.first() else {
        return Vec::new();
    };
    if pitch == 0 || !even(columns, pitch) {
        return columns.to_vec();
    }

    let mut out = vec![first];
    for pair in columns.windows(2) {
        let apart = (pair[1] - pair[0] + pitch / 2) / pitch;
        out.extend((1..apart).map(|n| pair[0] + n * pitch));
        out.push(pair[1]);
    }
    out
}

/// Whether every spacing is a whole number of `pitch`, give or take a drift of
/// a few columns
fn even(columns: &[usize], pitch: usize) -> bool {
    let slack = (pitch / EVEN).max(1);
    columns.windows(2).all(|pair| {
        let apart = pair[1] - pair[0];
        let whole = ((apart + pitch / 2) / pitch).max(1);
        apart.abs_diff(whole * pitch) <= slack
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{decode::Samples, image::Layout};

    const SENSOR: usize = 64;

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
    struct Strip {
        feed: usize,
        length: usize,
        polarity: Polarity,
        frames: Vec<usize>,
        flat: Option<usize>,
        blank: Option<usize>,
        /// Columns of bare backlight past the end of the film
        gate: Option<(usize, usize)>,
        /// Columns of holder mask before the film starts
        mask: usize,
    }

    impl Strip {
        fn new(frames: Vec<usize>, length: usize, polarity: Polarity) -> Self {
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
            }
        }

        fn render(&self) -> Samples {
            let level = levels(self.polarity);
            let mut colors = vec![vec![0u16; SENSOR * self.feed]; 3];

            for x in 0..self.feed {
                let inside = self
                    .frames
                    .iter()
                    .position(|&top| (top..top + self.length).contains(&x));

                let (value, contrast) = match inside {
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

        fn detect(&self) -> Detected {
            let samples = self.render();
            let layout = Layout::single_line(SENSOR as u32, self.feed as u32, vec![1, 2, 3]);
            let image = Image::new(&layout, &samples).expect("the buffer is the layout's size");
            super::detect(&image, self.length, self.polarity)
        }

        fn tops(&self) -> Vec<usize> {
            self.detect().frames
        }
    }

    /// Every frame within a column or two of where it was drawn
    fn close(got: &[usize], want: &[usize], slack: usize) {
        assert_eq!(got.len(), want.len(), "got {got:?}, wanted {want:?}");
        for (g, w) in got.iter().zip(want) {
            assert!(
                g.abs_diff(*w) <= slack,
                "got {got:?}, wanted {want:?} within {slack}"
            );
        }
    }

    #[test]
    fn every_frame_of_an_even_strip_is_found() {
        for polarity in [Polarity::Positive, Polarity::Negative] {
            let strip = Strip::new(vec![30, 162, 294, 426], 120, polarity);
            let found = strip.detect();
            close(&found.frames, &[30, 162, 294, 426], 2);
            assert_eq!(found.pitch, 132, "{polarity:?}");
        }
    }

    /// The failure this rewrite is for: the bare gate is the largest step in
    /// the pass, and pairing edges by frame length put a frame against it
    #[test]
    fn the_bare_gate_past_the_film_is_not_a_frame() {
        let mut strip = Strip::new(vec![30, 162, 294], 120, Polarity::Positive);
        strip.feed = 560;
        strip.gate = Some((430, 520));
        close(&strip.tops(), &[30, 162, 294], 2);
    }

    /// A frame under the holder mask is still where it is, and the table says
    /// so. Moved down to clear the mask it would crop the picture showing
    #[test]
    fn a_frame_behind_the_holder_mask_keeps_its_place() {
        let mut strip = Strip::new(vec![20, 152, 284], 120, Polarity::Positive);
        strip.mask = 40;
        let tops = strip.tops();
        close(&tops, &[20, 152, 284], 3);
        assert!(
            tops[0] + 120 >= 140,
            "{tops:?} should still hold all the picture the mask leaves showing"
        );
    }

    /// A flat picture is as even as a gap. Which side of the unexposed film it
    /// sits on puts it back, and that is what the film type says
    #[test]
    fn a_flat_picture_is_still_a_frame() {
        for polarity in [Polarity::Positive, Polarity::Negative] {
            let mut strip = Strip::new(vec![30, 162, 294], 120, polarity);
            strip.flat = Some(1);
            close(&strip.tops(), &[30, 162, 294], 2);
        }
    }

    /// An unexposed frame is the same film as the gap around it, so only an
    /// even run either side says it is there
    #[test]
    fn a_frame_with_no_picture_in_it_still_gets_a_place() {
        let mut strip = Strip::new(vec![30, 162, 294, 426], 120, Polarity::Positive);
        strip.blank = Some(2);
        let found = strip.detect();
        assert_eq!(found.frames.len(), 4, "{:?}", found.frames);
        // Nothing showed there, so the wind is all that puts it where it is
        assert_eq!(
            found.frames[2],
            found.frames[1] + found.pitch,
            "pitch {} in {:?}",
            found.pitch,
            found.frames
        );
    }

    /// Two frames sharing film come back sharing it, each keeping the edge it
    /// was found by
    #[test]
    fn frames_that_overlap_come_back_overlapping() {
        let strip = Strip::new(vec![30, 132, 294], 120, Polarity::Negative);
        let tops = strip.tops();
        close(&tops, &[30, 132, 294], 3);
        assert!(
            tops[1] < tops[0] + 120,
            "{tops:?} should have the first two frames sharing film"
        );
    }

    /// Spacings that do not divide say nothing about a frame nothing showed,
    /// so nothing is invented. These are the overlapping 6x6 negative's
    #[test]
    fn an_uneven_run_is_not_laddered() {
        let strip = Strip::new(vec![30, 130, 294], 120, Polarity::Negative);
        let found = strip.detect();
        assert_eq!(found.frames.len(), 3, "{:?}", found.frames);
    }

    #[test]
    fn an_empty_holder_holds_no_frames() {
        let strip = Strip::new(Vec::new(), 120, Polarity::Positive);
        let found = strip.detect();
        assert!(found.frames.is_empty(), "{:?}", found.frames);
        assert_eq!(found.pitch, 0);
    }
}
