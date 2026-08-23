//! Dump per-column texture and density off a thumbnail TIFF, for placing
//! ground-truth frame boundaries by hand into `thumbnails/ground_truth.json`
//! (via `scripts/annotate.py`, or directly for fine adjustment).
//!
//! The same signal `boundaries::columns` scores frames against: `texture` is
//! how much a column varies down the sensor, `density` is its mean level as
//! `log10(full scale / mean)`. A frame's real edge is where `texture` lifts
//! off zero and `density` moves off whatever the confirmed gaps in the same
//! file read at - not always visible by eye on the image itself, especially
//! on a thin negative or past an orange mask.
//!
//! ```text
//! cargo run --example column_profile -- <thumbnail.tiff> [bucket] [start] [end]
//! ```
//! `bucket` averages that many columns per line (default 10); `start`/`end`
//! narrow the numeric dump to a column range once the ASCII overview below
//! it has shown roughly where to look.

use std::{env, path::PathBuf};
use tiff::decoder::{Decoder, DecodingResult, Limits};

/// The TIFF itself is chunky RGB; scoring wants planes apart
fn deinterleave3(chunky: &[u16]) -> Vec<Vec<u16>> {
    let mut planes: Vec<Vec<u16>> = (0..3).map(|_| Vec::with_capacity(chunky.len() / 3)).collect();
    for pixel in chunky.chunks_exact(3) {
        for (plane, &v) in planes.iter_mut().zip(pixel) {
            plane.push(v);
        }
    }
    planes
}

fn main() {
    let mut args = env::args().skip(1);
    let input = PathBuf::from(
        args.next()
            .expect("usage: column_profile <in.tiff> [bucket] [start] [end]"),
    );
    let bucket: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    let start_arg: Option<usize> = args.next().and_then(|s| s.parse().ok());
    let end_arg: Option<usize> = args.next().and_then(|s| s.parse().ok());

    let file = std::fs::File::open(&input).unwrap_or_else(|e| panic!("{}: {e}", input.display()));
    let mut decoder = Decoder::new(std::io::BufReader::new(file))
        .unwrap()
        .with_limits(Limits::unlimited());
    let (cols, rows) = decoder.dimensions().unwrap();
    let chunky = match decoder.read_image().unwrap() {
        DecodingResult::U16(v) => v,
        other => panic!("not 16-bit samples ({other:?})"),
    };
    let planes = deinterleave3(&chunky);
    let (cols, rows) = (cols as usize, rows as usize);

    // Same TRIM as boundaries::columns: the opening's top and bottom rows are
    // holder, not film
    let trim = rows / 8;
    let band = trim..rows.saturating_sub(trim);

    let mut texture = vec![0.0f32; cols];
    let mut density = vec![0.0f32; cols];
    for x in 0..cols {
        let (mut t, mut d) = (0.0f32, 0.0f32);
        for plane in &planes {
            let at = |y: usize| f32::from(plane[y * cols + x]);
            let level = band.clone().map(at).sum::<f32>() / band.len() as f32;
            let step = band.clone().skip(1).map(|y| (at(y) - at(y - 1)).abs()).sum::<f32>()
                / (band.len() - 1) as f32;
            t += step / (level + 655.0);
            d += (65535.0 / level.max(1.0)).log10();
        }
        texture[x] = t / planes.len() as f32;
        density[x] = d / planes.len() as f32;
    }

    println!("{}: {cols}x{rows}, bucket={bucket}", input.display());

    // The numbers, over whatever range was asked for
    let lo = start_arg.unwrap_or(0);
    let hi = end_arg.unwrap_or(cols).min(cols);
    for start in (lo..hi).step_by(bucket) {
        let end = (start + bucket).min(cols);
        let t = texture[start..end].iter().cloned().fold(0.0f32, f32::max);
        let d = density[start..end].iter().sum::<f32>() / (end - start) as f32;
        println!("{start:>5} texture={t:>8.4} density={d:>7.3}");
    }

    // An ASCII overview of the whole file, to see where to point the numbers
    // above: texture on top, density below, each column bucketed to its peak
    // (texture) or mean (density) and mapped onto this file's own range
    println!("---- ascii (texture / density, this file's own range) ----");
    let ramp: &[u8] = b" .:-=+*#%@";
    let scale = |v: f32, lo: f32, hi: f32| ramp[(((v - lo) / (hi - lo)).clamp(0.0, 1.0) * (ramp.len() - 1) as f32) as usize] as char;
    let tmax = texture.iter().cloned().fold(0.0f32, f32::max).max(1e-6);
    let dmin = density.iter().cloned().fold(f32::MAX, f32::min);
    let dmax = density.iter().cloned().fold(f32::MIN, f32::max).max(dmin + 1e-6);
    for start in (0..cols).step_by(bucket) {
        let end = (start + bucket).min(cols);
        let t = texture[start..end].iter().cloned().fold(0.0f32, f32::max);
        print!("{}", scale(t, 0.0, tmax));
    }
    println!();
    for start in (0..cols).step_by(bucket) {
        let end = (start + bucket).min(cols);
        let d = density[start..end].iter().sum::<f32>() / (end - start) as f32;
        print!("{}", scale(d, dmin, dmax));
    }
    println!();
}
