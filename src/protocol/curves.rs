//! The CCD's own response curves, `DataType::CcdData`
//!
//! A three-line sensor reads film through rows of photosites that do not share
//! a transfer curve, so a flat patch comes off the bar with a pattern repeating
//! every three rows. The block interleave spreads that along the feed, and it is
//! the banding a single-line pass does not have.
//!
//! The unit measures its own rows and hands the result over: one curve per row
//! per type, each sampled at the levels `CcdMeasurement` lists. Correcting a row
//! is re-mapping it onto the reference row. Every curve is sampled at the same
//! levels, so the level cancels and the remap goes through the shared point
//! index rather than through any absolute value.
//!
//! Nothing is measured from the image and nothing is per column: the same raw
//! value off the same row always becomes the same corrected value.
//!
//! Thanks to @a6o for working out what Nikon Scan does with this page.

use crate::protocol::caps::ccd::CcdMeasurement;

/// Samples a lookup table covers
const LEVELS: usize = u16::MAX as usize + 1;

/// One row's remap onto the reference row, as a table
///
/// A table per row rather than an interpolation per sample: a pass has hundreds
/// of millions of samples and only a handful of rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Curves {
    rows: Vec<Vec<u16>>,
}

impl Curves {
    /// Build the tables from a `DataType::CcdData` reply
    ///
    /// `words` is the reply's valid data and `ccd` is the page describing it.
    /// `None` where the two disagree about how much there should be, since a
    /// correction from a misread table would be worse than none.
    pub fn parse(ccd: &CcdMeasurement, words: &[u16], rows: usize, kind: usize) -> Option<Self> {
        let points = ccd.points.len();
        if points < 2 || rows == 0 || words.len() < ccd.curves() * points {
            return None;
        }
        // `repeat * types + type`, established by reading a real unit: curves
        // `i`, `i + types` and `i + 2 * types` agree while the types do not
        let types = usize::from(ccd.types);
        if kind >= types || rows * types > ccd.curves() {
            return None;
        }
        let curve = |row: usize| -> &[u16] {
            let at = (row * types + kind) * points;
            &words[at..at + points]
        };

        // 2-11 does not say the middle row is the reference, but it is the one
        // the others sit either side of, so it moves the fewest samples
        let reference = curve(rows / 2).to_vec();
        Some(Self {
            rows: (0..rows).map(|r| table(curve(r), &reference)).collect(),
        })
    }

    /// The corrected value of `sample`, read off CCD row `row`
    ///
    /// A row the tables do not cover is passed through: an uncorrected sample is
    /// what this always produced before.
    pub fn correct(&self, row: usize, sample: u16) -> u16 {
        match self.rows.get(row) {
            Some(table) => table[usize::from(sample)],
            None => sample,
        }
    }

    /// How many rows the tables cover
    pub fn rows(&self) -> usize {
        self.rows.len()
    }
}

/// One row's table: where `from` reads a value, what `onto` reads at the same
/// point on its own curve
///
/// Both are sampled at the same levels, so the point index is the common ground
/// and the levels never enter the arithmetic. Between points the two curves are
/// taken as straight, and outside them the value is left alone.
fn table(from: &[u16], onto: &[u16]) -> Vec<u16> {
    let mut out = vec![0u16; LEVELS];
    let mut point = 0usize;

    for (sample, slot) in out.iter_mut().enumerate() {
        let sample = sample as u32;
        // The curves rise, so one walk covers every sample
        while point + 2 < from.len() && u32::from(from[point + 1]) <= sample {
            point += 1;
        }

        // Below the first point the curves say nothing. Both start at the dark
        // offset, so mapping into the first segment would lift everything under
        // it onto that offset and crush the bottom of the range to one value
        if sample < u32::from(from[0]) {
            *slot = sample as u16;
            continue;
        }

        let (lo, hi) = (u32::from(from[point]), u32::from(from[point + 1]));
        let (a, b) = (u32::from(onto[point]), u32::from(onto[point + 1]));
        *slot = match hi.checked_sub(lo).filter(|span| *span != 0) {
            // A flat step says nothing about where inside it a sample sits
            None => sample.min(u32::from(u16::MAX)) as u16,
            Some(span) => {
                let along = sample.saturating_sub(lo);
                let mapped = i64::from(a)
                    + i64::from(along) * (i64::from(b) - i64::from(a)) / i64::from(span);
                mapped.clamp(0, i64::from(u16::MAX)) as u16
            }
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::caps::{Page, ccd::CcdMeasurement};

    /// Three levels, so a curve is two straight segments
    fn page(points: &[u16], types: u8) -> CcdMeasurement {
        let mut p = vec![0u8; 11 + points.len() * 2];
        p[1] = CcdMeasurement::PAGE_CODE;
        p[3] = (p.len() - 4) as u8;
        p[4] = 0x07;
        p[9] = types;
        p[10] = points.len() as u8;
        for (n, level) in points.iter().enumerate() {
            p[11 + n * 2..13 + n * 2].copy_from_slice(&level.to_be_bytes());
        }
        CcdMeasurement::try_from(&Page::new(CcdMeasurement::PAGE_CODE, p).unwrap()).unwrap()
    }

    /// `repeat * types + type`, so one type of three rows is every `types`th
    fn words(rows: &[&[u16]], types: usize, kind: usize) -> Vec<u16> {
        let points = rows[0].len();
        let mut out = vec![0u16; 3 * types * points];
        for (r, curve) in rows.iter().enumerate() {
            let at = (r * types + kind) * points;
            out[at..at + points].copy_from_slice(curve);
        }
        out
    }

    /// Rows that already agree have nothing between them, so every sample comes
    /// back as it went in
    #[test]
    fn matching_rows_leave_every_sample_alone() {
        let ccd = page(&[0, 30000, 60000], 1);
        let curve: &[u16] = &[0, 30000, 60000];
        let c = Curves::parse(&ccd, &words(&[curve, curve, curve], 1, 0), 3, 0).unwrap();
        for v in [0u16, 1, 15000, 30000, 45000, 60000, 65535] {
            for row in 0..3 {
                assert_eq!(c.correct(row, v), v, "row {row} sample {v}");
            }
        }
    }

    /// A row reading high is brought back onto the reference, and the reference
    /// is left where it is
    #[test]
    fn a_row_that_reads_high_is_mapped_onto_the_reference() {
        let ccd = page(&[0, 30000, 60000], 1);
        let high: &[u16] = &[0, 33000, 60000];
        let middle: &[u16] = &[0, 30000, 60000];
        let c = Curves::parse(&ccd, &words(&[high, middle, middle], 1, 0), 3, 0).unwrap();

        // Row 1 is the reference of three, so it does not move
        assert_eq!(c.correct(1, 30000), 30000);
        // Row 0 reads 33000 where the reference reads 30000
        assert_eq!(c.correct(0, 33000), 30000);
        // The ends are common to both curves
        assert_eq!(c.correct(0, 0), 0);
        assert_eq!(c.correct(0, 60000), 60000);
        // Half way up the first segment of row 0 is half way up the reference's
        assert_eq!(c.correct(0, 16500), 15000);
    }

    /// The correction is not linear, which is why it has to precede an average
    #[test]
    fn the_remap_is_not_linear() {
        let ccd = page(&[0, 20000, 60000], 1);
        let bent: &[u16] = &[0, 30000, 60000];
        let straight: &[u16] = &[0, 20000, 60000];
        let c = Curves::parse(&ccd, &words(&[bent, straight, straight], 1, 0), 3, 0).unwrap();

        let (a, b) = (10000u16, 50000u16);
        let mean_then_correct = c.correct(0, (a + b) / 2);
        let correct_then_mean = (u32::from(c.correct(0, a)) + u32::from(c.correct(0, b))) / 2;
        assert_ne!(u32::from(mean_then_correct), correct_then_mean);
    }

    /// Every curve starts at the sensor's dark offset, and nothing below it was
    /// measured. Mapping into the first segment would put the whole of the
    /// bottom of the range onto that offset
    #[test]
    fn samples_under_the_first_point_are_left_alone() {
        let ccd = page(&[0, 30000, 60000], 1);
        let high: &[u16] = &[90, 33000, 60000];
        let middle: &[u16] = &[90, 30000, 60000];
        let c = Curves::parse(&ccd, &words(&[high, middle, middle], 1, 0), 3, 0).unwrap();

        for v in 0..90u16 {
            assert_eq!(c.correct(0, v), v, "sample {v} under the dark offset");
        }
        assert_eq!(c.correct(0, 90), 90);
    }

    /// A reply that does not match the page it came with is not a table
    #[test]
    fn a_reply_that_does_not_fit_is_refused() {
        let ccd = page(&[0, 30000, 60000], 1);
        let curve: &[u16] = &[0, 30000, 60000];
        let full = words(&[curve, curve, curve], 1, 0);
        assert!(Curves::parse(&ccd, &full[..full.len() - 1], 3, 0).is_none());
        // A type the page does not have
        assert!(Curves::parse(&ccd, &full, 3, 1).is_none());
    }
}
