//! Letting the operator see and adjust the frames detection found, before
//! scanning any of them
//!
//! Some strips have no good automatic answer - a frame's leading edge that
//! never got exposed reads identically to the gap before it, and no amount of
//! tuning fixes that, because the signal really is the same either way. This
//! shows one detected frame at a time and lets the operator nudge it with the
//! arrow keys, in the same column space `boundaries::detect` itself works in -
//! detection's own guess is always where a frame starts out.

use crate::io::color_planes;
use anyhow::{Context, Result, anyhow, bail};
use console::{Key, Term};
use image::{DynamicImage, Rgb, RgbImage};
use nkscan::{
    protocol::{data::Rect, decode::Samples},
    scan::{
        framing::{self, Discovery, ReviewContext},
        pass::Pass,
    },
    session::Session,
};
use std::io::{IsTerminal, Write, stdout};
use tracing::warn;

/// What the operator decided
pub enum Outcome {
    /// Scan these frames, whether or not any were nudged
    Proceed(Vec<Rect>),
    /// Stop. Nothing was scanned
    Abort,
}

/// The box drawn around the frame currently being nudged
const CURRENT: [u8; 3] = [255, 255, 255];

/// How much of the frame's own width to show either side of it, for the
/// neighboring gaps to judge the edge against. Scales with the box itself, so
/// growing or shrinking it keeps roughly the same amount of context in view
const CONTEXT: u32 = 1;

/// Show the operator one detected frame at a time and let them nudge it with
/// the arrow keys, or abort the scan outright
pub fn run(session: &mut Session, discovery: &Discovery, samples: &Samples) -> Result<Outcome> {
    let Some(review) = &discovery.review else {
        warn!("--review has nothing to show for this unit's framing mechanism, skipping");
        return Ok(Outcome::Proceed(discovery.frames.clone()));
    };
    if discovery.frames.is_empty() {
        return Ok(Outcome::Proceed(Vec::new()));
    }
    if !stdout().is_terminal() {
        bail!("--review needs an interactive terminal");
    }
    let pass = discovery
        .thumbnail
        .as_ref()
        .expect("a review context implies a thumbnail pass");

    // Tone-mapped once - the picture itself never changes, only which column
    // and width of it is framed and which crop of it is on screen
    let base = tone_map(pass, samples)?;

    // Inline at the cursor, not the terminal's absolute top-left (viuer's own
    // default), or every redraw lands in the same spot regardless of scroll
    let config = viuer::Config {
        absolute_offset: false,
        ..Default::default()
    };
    let term = Term::stdout();

    // Detection's own guess is always where a frame starts out, at the
    // width the whole strip converged on; the arrows only ever move it
    // from there
    let mut edits: Vec<(usize, u32)> = review.columns.iter().map(|&c| (c, review.geometry.width())).collect();
    let total = edits.len();

    for (i, frame) in edits.iter_mut().enumerate() {
        loop {
            let (col, width) = *frame;
            let margin = width * CONTEXT;
            // A fixed-width window that slides and clamps as one piece - two
            // independently-clamped edges would grow the crop instead of
            // panning it, the moment a frame sits within one margin of either
            // edge of the whole strip
            let target = (width + 2 * margin).min(base.width());
            let left = (col as u32).saturating_sub(margin).min(base.width() - target);
            let right = left + target;
            let mut view = image::imageops::crop_imm(&base, left, 0, right - left, base.height()).to_image();
            draw_box(&mut view, col - left as usize, width, CURRENT);

            print!("\x1B[2J\x1B[H"); // clear screen, cursor home
            stdout().flush().ok();
            viuer::print(&DynamicImage::ImageRgb8(view), &config)
                .map_err(|e| anyhow!("rendering the thumbnail: {e}"))?;
            println!(
                "frame {}/{total}: column {col}, width {width}  (\u{2190}/\u{2192} move, \u{2191}/\u{2193} resize, enter accepts, q aborts)",
                i + 1
            );

            match term.read_key()? {
                Key::Enter => break,
                Key::ArrowLeft => try_set(review, frame, (col.saturating_sub(1), width)),
                Key::ArrowRight => try_set(review, frame, (col + 1, width)),
                Key::ArrowUp => try_set(review, frame, (col, width + 1)),
                Key::ArrowDown if width > 1 => try_set(review, frame, (col, width - 1)),
                Key::Char('q') | Key::Escape | Key::CtrlC => return Ok(Outcome::Abort),
                _ => {}
            }
        }
    }

    let frames = framing::commit_frames(session, discovery, &edits)?;
    Ok(Outcome::Proceed(frames))
}

/// Move to `candidate` if it still builds a real frame on whichever mechanism
/// this strip used - a reading to register against for a perforation-framed
/// unit, or simply somewhere the axis reaches for a rect-framed one
fn try_set(review: &ReviewContext, frame: &mut (usize, u32), candidate: (usize, u32)) {
    let (col, width) = candidate;
    let usable = match &review.perfs {
        Some(perfs) => review.geometry.frame_position(col, width, perfs).is_some(),
        None => review.geometry.rect(col, width).is_some(),
    };
    if usable {
        *frame = candidate;
    }
}

/// The thumbnail as a displayable image: full scale, auto-leveled and
/// gamma-corrected the same way the corpus tooling's own preview is
/// (`scripts/annotate.py`'s `-auto-level -gamma 1.4`), since the pass itself
/// is linear 16-bit data that would otherwise render far too dark
fn tone_map(pass: &Pass, samples: &Samples) -> Result<RgbImage> {
    let color = color_planes(pass);
    let mut samples = samples.clone();
    samples.to_full_scale(pass.layout.bits_per_sample);

    let idx: [usize; 3] = match *color.as_slice() {
        [r, g, b] => [r, g, b],
        [g] => [g, g, g],
        ref other => bail!("{} color planes is not a thumbnail --review can show", other.len()),
    };
    let planes: [&[u16]; 3] = idx.map(|p| samples.colors[p].as_slice());

    // Stretch what is actually there to full scale, per channel, so a thin
    // negative or a dark slide is not just black on screen
    let levels = planes.map(|p| {
        let (lo, hi) = p.iter().fold((u16::MAX, 0u16), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        (lo, hi.max(lo + 1))
    });

    const GAMMA: f32 = 1.0 / 1.4;
    let (cols, rows) = (pass.cols, pass.rows);
    let mut buf = vec![0u8; cols * rows * 3];
    for pixel in 0..cols * rows {
        for (c, &(lo, hi)) in levels.iter().enumerate() {
            let stretched = f32::from(planes[c][pixel].saturating_sub(lo)) / f32::from(hi - lo);
            buf[pixel * 3 + c] = (stretched.clamp(0.0, 1.0).powf(GAMMA) * 255.0).round() as u8;
        }
    }
    RgbImage::from_raw(cols as u32, rows as u32, buf).context("building the review preview")
}

/// How many pixels a dash and the gap after it each run, along the box's
/// own edges
const DASH: u32 = 4;

/// Draw `[start, start+width)`'s box, full height, one pixel and dashed so
/// it reads as a marker over the picture rather than a hard crop of it
fn draw_box(image: &mut RgbImage, start: usize, width: u32, color: [u8; 3]) {
    let (w, h) = image.dimensions();
    let start = start as u32;
    let end = (start + width).min(w.saturating_sub(1));
    let dashed = |p: u32| p % (DASH * 2) < DASH;

    let mut set = |x: u32, y: u32| {
        if x < w && y < h {
            image.put_pixel(x, y, Rgb(color));
        }
    };
    for y in 0..h {
        if dashed(y) {
            set(start, y);
            set(end, y);
        }
    }
    for x in start..=end.max(start) {
        if dashed(x - start) {
            set(x, 0);
            set(x, h.saturating_sub(1));
        }
    }
}
