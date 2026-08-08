//! Turning the scan stream into an image. Section 2-11-3
//!
//! The stream carries no header, so [`Layout`] is the only description of it.
//! Bytes arrive in whatever lengths the transport allows, so a decoder holds
//! whatever part of a line the last chunk left and writes complete lines only.
//!
//! Output is one sample per channel per pixel, channels in the order `Layout`
//! lists them, so a caller can wrap it as `(rows, cols, channels)` without
//! copying. Whatever the interleaving, the geometry is the same: the sensor bar
//! is the image's rows and the stage's feed positions are its columns, with the
//! bar read out backwards.

use crate::{
    error::Error,
    protocol::{
        caps::set_window::ColorInterleaving, curves::Curves, image::Layout, window::Channel,
    },
};

/// A stream we cannot make an image of
fn bad(reason: String) -> Error {
    Error::Unsupported {
        op: "decode",
        reason,
    }
}

/// A view of an unscrambled pass
///
/// The samples are the buffer a [`Decoder`] filled, which the caller owns, so
/// nothing here copies an image. The identifiers are a handful of bytes and are
/// owned, which keeps the view off the layout's lifetime.
#[derive(Debug, Clone)]
pub struct Image<'a> {
    /// One sample per channel per pixel, channels interleaved
    pub samples: &'a [u16],
    pub rows: usize,
    pub cols: usize,
    /// Channel identifiers, in the order `samples` interleaves them
    pub channels: Vec<u8>,
    /// Valid bits in a sample, which can be fewer than 16
    pub bits: u8,
}

impl<'a> Image<'a> {
    /// Read a filled buffer as the image `layout` describes
    pub fn new(layout: &Layout, samples: &'a [u16]) -> Result<Self, Error> {
        let decoder = Decoder::new(layout)?;
        if samples.len() < decoder.samples() {
            return Err(bad(format!(
                "{} samples is short of the {} this layout describes",
                samples.len(),
                decoder.samples()
            )));
        }
        let (rows, cols) = decoder.shape();
        Ok(Self {
            samples,
            rows,
            cols,
            channels: layout.channels.clone(),
            bits: layout.bits_per_sample,
        })
    }

    /// One row, channels interleaved in the order `channels` lists them
    pub fn row(&self, y: usize) -> &'a [u16] {
        let stride = self.cols * self.channels.len();
        &self.samples[y * stride..(y + 1) * stride]
    }

    /// Every sample of one channel
    pub fn plane(&self, channel: usize) -> impl Iterator<Item = u16> + use<'a> {
        self.samples
            .iter()
            .skip(channel)
            .step_by(self.channels.len())
            .copied()
    }
}

/// How a stream is scrambled, and where one block of it belongs
///
/// The sensor bar is the image's vertical axis and reads out backwards. The
/// stage advances one column per position and the CCD's rows sit `gap` columns
/// apart, so `gap` stage positions fill a contiguous run of `gap * ccd_lines`
/// columns: `[row 0 x gap][row 1 x gap][row 2 x gap]`. A single-line pass is
/// the same readout with one CCD row, where `gap` is 1 and a block is one line
/// of the feed, so there is only one ordering and nothing to dispatch on.
struct Transposed<'a> {
    /// Output rows, which is the sensor bar
    rows: usize,
    /// Output columns, which is stage positions times CCD rows
    cols: usize,
    /// CCD rows read at once
    ccd_lines: usize,
    /// Stage positions in one interleave block
    gap: usize,
    /// Where each output channel's samples sit, in the output's order
    slots: Vec<Slot>,
    /// Samples in one readout
    readout: usize,
    /// Samples in one stage position
    stage: usize,
    bytes_per_sample: usize,
    /// The rows' remap onto each other, where the unit gave us one
    curves: Option<&'a Curves>,
}

/// Where one output channel sits in a stage position
///
/// A stage position emits the color channels, then the channels read once, then
/// the color channels again for each further reading. So the first reading is
/// not where the rest continue from, and the slots are not the SCAN order.
#[derive(Debug, Clone, Copy)]
struct Slot {
    /// The first reading
    first: usize,
    /// The second, after which each further reading steps by `stride`
    next: usize,
    stride: usize,
    /// Readings to average. 1 for a channel multi-sampling does not repeat
    readings: usize,
}

impl Slot {
    fn nth(&self, reading: usize) -> usize {
        match reading {
            0 => self.first,
            r => self.next + (r - 1) * self.stride,
        }
    }

    /// One per output channel, in the order the output interleaves them
    fn every(channels: &[u8], readings: usize) -> Vec<Self> {
        let colors = channels
            .iter()
            .filter(|id| Channel::from(**id).is_color())
            .count();
        let once = channels.len() - colors;

        let (mut color, mut other) = (0, 0);
        channels
            .iter()
            .map(|id| match Channel::from(*id).is_color() {
                true => {
                    color += 1;
                    Slot {
                        first: color - 1,
                        next: colors + once + color - 1,
                        stride: colors,
                        readings,
                    }
                }
                false => {
                    other += 1;
                    Slot {
                        first: colors + other - 1,
                        next: 0,
                        stride: 0,
                        readings: 1,
                    }
                }
            })
            .collect()
    }
}

impl Transposed<'_> {
    /// Bytes one [`emit`](Self::emit) consumes
    fn block_bytes(&self) -> usize {
        self.gap * self.stage * self.bytes_per_sample
    }

    /// Blocks the layout promises
    fn blocks(&self) -> usize {
        self.cols / (self.gap * self.ccd_lines)
    }

    /// Rows and columns of the image: sensor pixels down, feed positions across
    fn shape(&self) -> (usize, usize) {
        (self.rows, self.cols)
    }

    /// Samples the output holds
    fn samples(&self) -> usize {
        self.rows * self.cols * self.slots.len()
    }

    fn emit(&self, n: usize, block: &[u8], out: &mut [u16]) {
        let first = n * self.gap * self.ccd_lines;
        for col in 0..self.gap * self.ccd_lines {
            // A block lays its stage positions out under each CCD row in turn
            let (position, line) = (col % self.gap, col / self.gap);
            let x = first + col;
            let base = position * self.stage + line;

            for p in 0..self.rows {
                // The bar reads out opposite to increasing y
                let y = self.rows - 1 - p;
                let sample = base + p * self.ccd_lines;
                let at = (y * self.cols + x) * self.slots.len();

                for (c, slot) in self.slots.iter().enumerate() {
                    // Integers throughout. A nibble caps the readings at 16, so
                    // the sum cannot leave a u32, and rounding to nearest keeps
                    // the average off a systematic bias toward zero
                    let mut sum = 0u32;
                    for r in 0..slot.readings {
                        let off = (sample + slot.nth(r) * self.readout) * self.bytes_per_sample;
                        let raw = sample_at(&block[off..off + self.bytes_per_sample]);
                        // Before the average, not after: the remap is not
                        // linear, so the two orders disagree
                        sum += u32::from(match self.curves {
                            Some(curves) => curves.correct(line, raw),
                            None => raw,
                        });
                    }
                    let n = slot.readings as u32;
                    out[at + c] = ((sum + n / 2) / n) as u16;
                }
            }
        }
    }
}

/// One big-endian sample, whichever width 2-11-3 gave it
#[inline]
fn sample_at(sample: &[u8]) -> u16 {
    match sample {
        [b] => u16::from(*b),
        [hi, lo] => u16::from_be_bytes([*hi, *lo]),
        _ => unreachable!("the width is checked when the decoder is built"),
    }
}

/// Unscrambles a scan into a caller-owned buffer
///
/// A chunk can end anywhere, so partial blocks are held until the rest arrives
pub struct Decoder<'a> {
    ordering: Transposed<'a>,
    /// A block the last chunk ended part-way through
    carry: Vec<u8>,
    /// Blocks emitted so far
    done: usize,
}

impl<'a> Decoder<'a> {
    /// A decoder for a stream shaped like `layout`
    ///
    /// Both supported interleavings are the same readout: the sensor bar sits
    /// on the output's Y axis and reads out backwards, stage positions tile
    /// against CCD rows in blocks of the line gap, and each channel and
    /// multi-sample repeat gets its own readout slot. The three-line mode reads
    /// every CCD row at once; [`LINE_WITHOUT_DISTANCE`](ColorInterleaving::LINE_WITHOUT_DISTANCE)
    /// is the same with one row, where the gap is 1 and a block is one line of
    /// the feed.
    ///
    /// Whatever the line count, a feed position hands back `readouts()`
    /// exposures (2-11-5-2's reading count per line): the first reading's
    /// colors, then the channels that are never repeated, then the colors again
    /// for each further reading. So single-line multi-pass is a format-1 line
    /// read as many times as the multiple-reading number asks, with the
    /// once-read channels between the first and second of them.
    pub fn new(layout: &Layout) -> Result<Self, Error> {
        if !matches!(layout.bytes_per_sample, 1 | 2) {
            return Err(bad(format!(
                "{} bytes a sample is neither of the widths 2-11-3 defines",
                layout.bytes_per_sample
            )));
        }
        let bytes_per_sample = usize::from(layout.bytes_per_sample);
        let (rows, cols) = (layout.pixels as usize, layout.lines as usize);

        if !layout.interleaving.intersects(
            ColorInterleaving::MULTILINE_SIMULTANEOUS | ColorInterleaving::LINE_WITHOUT_DISTANCE,
        ) {
            return Err(bad(format!(
                "{:?} is not an ordering this decodes yet",
                layout.interleaving
            )));
        }

        // 2-11-3-1 format 1 is the same readout with one CCD row: `gap` 1 and a
        // block of one line of the feed, so repeats sit exactly where they do
        // in the three-line readout

        let ccd_lines = if layout
            .interleaving
            .contains(ColorInterleaving::MULTILINE_SIMULTANEOUS)
        {
            usize::from(layout.ccd_lines).max(1)
        } else {
            // A single line, whatever the unit advertises for the three-line mode
            1
        };
        let gap = match ccd_lines {
            1 => 1,
            _ => layout.registration_gap as usize,
        };
        let strip = gap * ccd_lines;
        if gap == 0 || !cols.is_multiple_of(strip) {
            return Err(bad(format!(
                "{cols} columns is not a whole number of {strip}-column blocks"
            )));
        }
        let readout = rows * ccd_lines;
        let ordering = Transposed {
            rows,
            cols,
            ccd_lines,
            gap,
            slots: Slot::every(
                &layout.channels,
                usize::from(layout.readings_per_line).max(1),
            ),
            readout,
            stage: layout.readouts() as usize * readout,
            bytes_per_sample,
            curves: None,
        };

        Ok(Self {
            carry: Vec::with_capacity(ordering.block_bytes()),
            ordering,
            done: 0,
        })
    }

    /// Correct the CCD's rows against each other as the stream is unscrambled
    ///
    /// Only the three-line mode has rows to correct. A single-line pass reads
    /// every sample off one row, so there is no inter-line mismatch and a
    /// correction would only distort it.
    pub fn correcting(mut self, curves: &'a Curves) -> Self {
        if self.ordering.ccd_lines > 1 {
            self.ordering.curves = Some(curves);
        }
        self
    }

    /// Rows and columns of the image: the sensor bar down, the feed across
    pub fn shape(&self) -> (usize, usize) {
        self.ordering.shape()
    }

    /// Samples the output buffer has to hold
    pub fn samples(&self) -> usize {
        self.ordering.samples()
    }

    /// Blocks emitted so far, of the [`Layout`]'s total
    pub fn decoded(&self) -> usize {
        self.done
    }

    /// Whether every block the layout promised arrived
    pub fn complete(&self) -> bool {
        self.done == self.ordering.blocks()
    }

    /// Feed the next chunk, writing whatever blocks it completes into `out`
    ///
    /// Bytes past the last block the layout promised are dropped: the unit pads
    /// a short read rather than truncating one.
    pub fn push(&mut self, chunk: &[u8], out: &mut [u16]) -> Result<(), Error> {
        if out.len() < self.samples() {
            return Err(bad(format!(
                "the output holds {} samples and this stream needs {}",
                out.len(),
                self.samples()
            )));
        }

        let width = self.ordering.block_bytes();
        let mut rest = chunk;

        // Finish the block the last chunk ended part-way through
        if !self.carry.is_empty() {
            let take = (width - self.carry.len()).min(rest.len());
            self.carry.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.carry.len() < width {
                return Ok(());
            }
            let block = std::mem::take(&mut self.carry);
            self.take(&block, out);
            self.carry = block;
            self.carry.clear();
        }

        let mut blocks = rest.chunks_exact(width);
        for block in &mut blocks {
            self.take(block, out);
        }
        self.carry.extend_from_slice(blocks.remainder());
        Ok(())
    }

    /// Emit one block, unless the layout has already had every block it wanted
    fn take(&mut self, block: &[u8], out: &mut [u16]) {
        if self.done >= self.ordering.blocks() {
            return;
        }
        self.ordering.emit(self.done, block, out);
        self.done += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::image::Layout;

    /// A 1494 x 1494 three-channel 16-bit stream, which is what a 666 dpi
    /// single-line pass over a 6x6 frame produces
    fn layout(pixels: u32, lines: u32, channels: Vec<u8>) -> Layout {
        Layout::single_line(pixels, lines, channels)
    }

    /// Channel `c` of pixel `x` on line `y` carries a value that says so
    fn stream(pixels: usize, lines: usize, channels: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for y in 0..lines {
            for c in 0..channels {
                for x in 0..pixels {
                    let v = (y * 1000 + x * 10 + c) as u16;
                    out.extend_from_slice(&v.to_be_bytes());
                }
            }
        }
        out
    }

    /// The wire puts a whole channel down before the next; the output
    /// interleaves them per pixel, sensor pixels down the image with the bar
    /// read out backwards
    #[test]
    fn a_line_of_planes_comes_out_interleaved() {
        let l = layout(4, 3, vec![1, 2, 3]);
        let mut d = Decoder::new(&l).unwrap();
        let mut out = vec![0u16; d.samples()];
        d.push(&stream(4, 3, 3), &mut out).unwrap();

        assert!(d.complete());
        assert_eq!(d.shape(), (4, 3));
        for s in 0..4usize {
            for f in 0..3usize {
                for c in 0..3usize {
                    let got = out[((4 - 1 - s) * 3 + f) * 3 + c];
                    assert_eq!(got, (f * 1000 + s * 10 + c) as u16, "{s},{f},{c}");
                }
            }
        }
    }

    /// A single-line pass is the three-line readout with one CCD row, so the
    /// same wire bytes come out the same image whichever interleaving names them
    #[test]
    fn single_line_and_three_line_read_one_geometry() {
        let single = layout(4, 3, vec![1, 2, 3]);
        let mut three = single.clone();
        three.interleaving = ColorInterleaving::MULTILINE_SIMULTANEOUS;
        three.ccd_lines = 1;
        three.registration_gap = 0;

        let raw = stream(4, 3, 3);
        let mut a = vec![0u16; Decoder::new(&single).unwrap().samples()];
        let mut b = vec![0u16; Decoder::new(&three).unwrap().samples()];
        Decoder::new(&single).unwrap().push(&raw, &mut a).unwrap();
        Decoder::new(&three).unwrap().push(&raw, &mut b).unwrap();

        assert_eq!(a, b);
    }

    /// The transport splits where it likes, including mid-sample
    #[test]
    fn a_stream_split_anywhere_decodes_the_same() {
        let l = layout(4, 3, vec![1, 2, 3]);
        let whole = stream(4, 3, 3);

        let mut want = vec![0u16; Decoder::new(&l).unwrap().samples()];
        Decoder::new(&l).unwrap().push(&whole, &mut want).unwrap();

        for split in [1usize, 3, 7, 16, 23, 48] {
            let mut d = Decoder::new(&l).unwrap();
            let mut got = vec![0u16; d.samples()];
            for piece in whole.chunks(split) {
                d.push(piece, &mut got).unwrap();
            }
            assert!(d.complete(), "split {split} left {} lines", d.decoded());
            assert_eq!(got, want, "split {split}");
        }
    }

    /// A short read leaves what did not arrive where it was
    #[test]
    fn a_short_stream_writes_only_what_arrived() {
        let l = layout(4, 3, vec![1, 2, 3]);
        let mut d = Decoder::new(&l).unwrap();
        let mut out = vec![0u16; d.samples()];
        let whole = stream(4, 3, 3);
        d.push(&whole[..whole.len() / 3], &mut out).unwrap();

        assert!(!d.complete());
        assert_eq!(d.decoded(), 1);
        // One block is one feed position read into the whole sensor bar, so
        // column 0 carries the first wire line and nothing else has moved
        for (y, s) in [(0usize, 3usize), (1, 2), (2, 1), (3, 0)] {
            let at = (y * 3) * 3;
            assert_eq!(
                &out[at..at + 3],
                &[(s * 10) as u16, (s * 10 + 1) as u16, (s * 10 + 2) as u16],
                "row {y}"
            );
        }
        // The other feed positions have not arrived, so their columns are blank
        for y in 0..4usize {
            for x in 1..3usize {
                for c in 0..3usize {
                    assert_eq!(out[(y * 3 + x) * 3 + c], 0, "row {y} col {x} ch {c}");
                }
            }
        }
    }

    /// The unit pads rather than truncating, so anything past the last line
    /// the layout promised is not ours to write
    #[test]
    fn padding_past_the_last_line_is_dropped() {
        let l = layout(4, 2, vec![1, 2, 3]);
        let mut d = Decoder::new(&l).unwrap();
        let mut out = vec![0u16; d.samples()];
        d.push(&stream(4, 4, 3), &mut out).unwrap();

        assert_eq!(d.decoded(), 2);
        assert!(d.complete());
    }

    /// A real 666 dpi pass off an LS-9000, 1494 x 1494 x 3 at 16 bits, decoded
    /// in the 256 KB pieces the transport hands over
    ///
    /// `scan.raw` is a scratch dump whatever the last run happened to write, so
    /// this checks it is the pass in question and reports out if it is not
    #[test]
    fn a_real_pass_decodes_into_a_photograph() {
        let Ok(raw) = std::fs::read("scan.raw") else {
            eprintln!("no scan.raw, skipping");
            return;
        };
        let (w, h) = (1494usize, 1494usize);
        let l = layout(w as u32, h as u32, vec![1, 2, 3]);
        let mut d = Decoder::new(&l).unwrap();
        if raw.len() != d.samples() * 2 {
            eprintln!(
                "scan.raw is {} bytes, not the {} of a single-line {w}x{h} pass, skipping",
                raw.len(),
                d.samples() * 2
            );
            return;
        }

        let mut out = vec![0u16; d.samples()];
        for piece in raw.chunks(262_144) {
            d.push(piece, &mut out).unwrap();
        }
        assert!(d.complete());

        // A photograph, not noise: neighbouring pixels agree far better than
        // distant ones. Scrambled planes would flatten the difference
        let at = |y: usize, x: usize, c: usize| f64::from(out[(y * w + x) * 3 + c]);
        let (mut near, mut far, mut n) = (0.0, 0.0, 0.0);
        for y in (10..h - 10).step_by(37) {
            for x in (10..w - 800).step_by(37) {
                near += (at(y, x, 1) - at(y, x + 1, 1)).abs();
                far += (at(y, x, 1) - at(y, x + 700, 1)).abs();
                n += 1.0;
            }
        }
        eprintln!("neighbour {:.0}, distant {:.0}", near / n, far / n);
        assert!(near * 4.0 < far, "near {} far {}", near / n, far / n);
    }

    #[test]
    fn an_output_too_small_is_refused() {
        let l = layout(4, 3, vec![1, 2, 3]);
        let mut d = Decoder::new(&l).unwrap();
        assert!(d.push(&[], &mut [0u16; 4]).is_err());
    }
}

#[cfg(test)]
mod transposed {
    use super::*;

    /// A three-line layout: `rows` sensor pixels by `stages` stage positions
    fn layout(rows: u32, stages: u32, gap: u32, channels: Vec<u8>, readings: u8) -> Layout {
        Layout {
            pixels: rows,
            lines: stages * 3,
            pitch: 1,
            line_pitch: 1,
            dpi: 4000,
            bytes_per_sample: 2,
            bits_per_sample: 16,
            channels,
            interleaving: ColorInterleaving::MULTILINE_SIMULTANEOUS,
            readings_per_line: readings,
            ccd_lines: 3,
            registration_gap: gap,
            granule: 1,
        }
    }

    /// Every sample says where it came from, so a misplaced one is legible
    fn tag(stage: usize, slot: usize, pixel: usize, line: usize) -> u16 {
        (stage * 1000 + slot * 100 + pixel * 10 + line) as u16
    }

    /// The wire order: stage positions, each holding its readouts, each holding
    /// the sensor bar with the CCD's rows interleaved per pixel
    fn stream(stages: usize, readouts: usize, rows: usize, lines: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for stage in 0..stages {
            for slot in 0..readouts {
                for pixel in 0..rows {
                    for line in 0..lines {
                        out.extend_from_slice(&tag(stage, slot, pixel, line).to_be_bytes());
                    }
                }
            }
        }
        out
    }

    /// One block of `gap` stage positions fills `gap * 3` columns as
    /// `[row 0 x gap][row 1 x gap][row 2 x gap]`, and the bar reads out
    /// backwards
    #[test]
    fn a_block_tiles_stage_positions_against_ccd_rows() {
        let (rows, stages, gap) = (4usize, 2usize, 2u32);
        let l = layout(rows as u32, stages as u32, gap, vec![1, 2, 3], 1);
        let mut d = Decoder::new(&l).unwrap();
        assert_eq!(d.shape(), (rows, stages * 3));

        let mut out = vec![0u16; d.samples()];
        d.push(&stream(stages, 3, rows, 3), &mut out).unwrap();
        assert!(d.complete());

        let (_, cols) = d.shape();
        for stage in 0..stages {
            for line in 0..3 {
                let x = line * gap as usize + stage;
                for pixel in 0..rows {
                    let y = rows - 1 - pixel;
                    for channel in 0..3 {
                        assert_eq!(
                            out[(y * cols + x) * 3 + channel],
                            tag(stage, channel, pixel, line),
                            "stage {stage} line {line} pixel {pixel} channel {channel}"
                        );
                    }
                }
            }
        }
    }

    /// Infrared sits after the first color triple and is not repeated, so a
    /// repeated color averages its readings and infrared takes slot 3 alone
    #[test]
    fn multi_sampling_averages_the_colors_and_leaves_infrared_alone() {
        let l = layout(2, 1, 1, vec![9, 1, 2, 3], 2);
        let mut d = Decoder::new(&l).unwrap();
        let mut out = vec![0u16; d.samples()];
        // 3 colors x 2 readings + 1 infrared
        d.push(&stream(1, 7, 2, 3), &mut out).unwrap();
        assert!(d.complete());

        let (_, cols) = d.shape();
        for line in 0..3 {
            for pixel in 0..2 {
                let (y, x) = (2 - 1 - pixel, line);
                let at = (y * cols + x) * 4;
                // Channel order is the layout's: infrared first, from slot 3
                assert_eq!(out[at], tag(0, 3, pixel, line));
                for color in 0..3 {
                    let first = tag(0, color, pixel, line);
                    let second = tag(0, 4 + color, pixel, line);
                    assert_eq!(out[at + 1 + color], (first + second) / 2);
                }
            }
        }
    }

    /// Columns have to divide into whole interleave blocks
    #[test]
    fn a_ragged_column_count_is_refused() {
        let mut l = layout(4, 2, 4, vec![1, 2, 3], 1);
        l.lines = 7;
        assert!(Decoder::new(&l).is_err());
    }

    /// A single-line layout: one CCD row, `gap` 1, a block of one feed line
    fn single(rows: u32, stages: u32, channels: Vec<u8>, readings: u8) -> Layout {
        Layout {
            pixels: rows,
            lines: stages,
            pitch: 1,
            line_pitch: 1,
            dpi: 4000,
            bytes_per_sample: 2,
            bits_per_sample: 16,
            channels,
            interleaving: ColorInterleaving::LINE_WITHOUT_DISTANCE,
            readings_per_line: readings,
            ccd_lines: 1,
            registration_gap: 1,
            granule: 1,
        }
    }

    /// Single-line multi-pass is format 1 read as many times as the window
    /// asked: per feed position the color channels for each reading, with the
    /// once-read infrared between the first reading and the rest
    #[test]
    fn a_single_line_averages_multi_pass_and_leaves_infrared_alone() {
        let (rows, stages, readings) = (2usize, 2usize, 4u8);
        let l = single(rows as u32, stages as u32, vec![9, 1, 2, 3], readings);
        // 3 colors x 4 readings + 1 infrared
        let readouts = 13usize;
        let mut d = Decoder::new(&l).unwrap();
        let mut out = vec![0u16; d.samples()];
        d.push(&stream(stages, readouts, rows, 1), &mut out)
            .unwrap();
        assert!(d.complete());

        let (_, cols) = d.shape();
        for stage in 0..stages {
            for pixel in 0..rows {
                let (y, x) = (rows - 1 - pixel, stage);
                let at = (y * cols + x) * 4;
                assert_eq!(
                    out[at],
                    tag(stage, 3, pixel, 0),
                    "stage {stage} pixel {pixel} IR"
                );
                for color in 0..3 {
                    let slots = [color, 4 + color, 7 + color, 10 + color];
                    let reads: Vec<u16> = slots.iter().map(|&s| tag(stage, s, pixel, 0)).collect();
                    let want = reads.iter().map(|&s| u32::from(s)).sum::<u32>() / 4;
                    assert_eq!(
                        out[at + 1 + color],
                        want as u16,
                        "stage {stage} pixel {pixel} color {color}"
                    );
                }
            }
        }
    }

    /// A single-line pass of a channel multi-sampling does not repeat is one
    /// exposure per feed position, however many readings the window asked for
    #[test]
    fn a_single_line_reads_once_read_channels_once() {
        let (rows, stages) = (2usize, 2usize);
        let l = single(rows as u32, stages as u32, vec![9], 4);
        let mut d = Decoder::new(&l).unwrap();
        assert_eq!(d.shape(), (rows, stages));
        let mut out = vec![0u16; d.samples()];
        d.push(&stream(stages, 1, rows, 1), &mut out).unwrap();
        assert!(d.complete());

        let (_, cols) = d.shape();
        for stage in 0..stages {
            for pixel in 0..rows {
                let (y, x) = (rows - 1 - pixel, stage);
                assert_eq!(out[y * cols + x], tag(stage, 0, pixel, 0));
            }
        }
    }
}
