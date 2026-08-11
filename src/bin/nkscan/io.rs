//! Writing a finished pass out as 16-bit TIFF
//!
//! One file for the color planes and, where the pass carried infrared, one
//! beside it for that. Infrared is not a color and a fourth sample would be
//! read as alpha, so it does not share a file.
//!
//! Uncompressed: the samples are film grain, which deflates poorly, and the
//! write is hundreds of megabytes either way.

use crate::mono::{self, Luminance};
use anyhow::{Context, Result, bail};
use nkscan::{
    protocol::{decode::Samples, window::Channel},
    scan::pass::Pass,
};
use std::{
    fs::File,
    io::BufWriter,
    path::{Path, PathBuf},
};
use tiff::{
    encoder::{
        Rational, TiffEncoder,
        colortype::{ColorType, Gray16, RGB16},
    },
    tags::{ResolutionUnit, Tag},
};
use tracing::{debug, warn};

/// What a frame's files are named, one-indexed, infrared beside the color
pub fn paths(basename: &Path, n: usize) -> (PathBuf, PathBuf) {
    let stem = basename.display();
    (
        PathBuf::from(format!("{stem}_{n}.tiff")),
        PathBuf::from(format!("{stem}_{n}_IR.tiff")),
    )
}

/// The first frame number whose files are both free
///
/// A batch appends rather than overwrites, so a second strip through the same
/// basename carries on where the first stopped
pub fn next_free(basename: &Path) -> usize {
    (1..)
        .find(|&n| {
            let (color, ir) = paths(basename, n);
            !color.exists() && !ir.exists()
        })
        .expect("the range is unbounded")
}

/// Write one frame, returning the files it made
///
/// `icc` is embedded as-is where it is given. Without one the file says nothing
/// about its color, which is the truth: the samples are linear and this unit
/// has no characterization of its own.
///
/// `mono` writes one gray plane rather than three color ones. The pass is a
/// color one either way, since the unit scans RGB whatever the film is, so the
/// three become one the way Nikon's driver does it: through the monochrome
/// profile to XYZ, keeping Y. That is what `icc` is for on a mono write, and
/// the file is tagged with a gray space instead. Without a profile to convert
/// through there is nothing to weight the channels by, so it keeps green.
pub fn write_frame(
    basename: &Path,
    n: usize,
    samples: &Samples,
    pass: &Pass,
    icc: Option<&[u8]>,
    mono: bool,
) -> Result<Vec<PathBuf>> {
    // Positions within the color buffer, which carries only the color
    // channels of `pass.layout.channels`, in the same relative order
    let color_ids: Vec<u8> = pass.layout.colors().collect();
    let plane = |channel: Channel| {
        color_ids
            .iter()
            .position(|&id| Channel::from(id) == channel)
    };

    // R, G, B whatever order the stream interleaves them in. A unit that scans
    // one channel calls it the default rather than green
    let color: Vec<usize> = [Channel::Red, Channel::Green, Channel::Blue]
        .into_iter()
        .filter_map(plane)
        .collect();
    let color = match color.is_empty() {
        true => plane(Channel::Default).into_iter().collect(),
        false => color,
    };

    let (color_path, ir_path) = paths(basename, n);
    let mut written = Vec::new();
    let stride = color_ids.len();

    // Three channels and a profile to weigh them by is the only way to make
    // luminance. Anything less falls back to the channel 2-11-3 calls the
    // default, which is green
    let luminance = match (mono, color.len(), icc) {
        (true, 3, Some(icc)) => Some(Luminance::from_profile(icc)?),
        (true, _, _) => {
            warn!("no monochrome profile for this unit, so the gray is the green channel alone");
            None
        }
        (false, _, _) => None,
    };
    let fallback = mono
        .then(|| plane(Channel::Green).or_else(|| plane(Channel::Default)))
        .flatten();

    match (&luminance, fallback, color.len()) {
        (Some(luminance), _, _) => {
            let gray = mono::gray_profile()?;
            let planes = [color[0], color[1], color[2]];
            write_planes::<Gray16>(
                &color_path,
                &samples.color,
                pass,
                Source::Luminance { planes, luminance },
                Some(&gray),
                stride,
            )?
        }
        (None, Some(gray), _) => write_planes::<Gray16>(
            &color_path,
            &samples.color,
            pass,
            Source::Planes(&[gray]),
            None,
            stride,
        )?,
        (None, None, 3) => write_planes::<RGB16>(
            &color_path,
            &samples.color,
            pass,
            Source::Planes(&color),
            icc,
            stride,
        )?,
        (None, None, 1) => write_planes::<Gray16>(
            &color_path,
            &samples.color,
            pass,
            Source::Planes(&color),
            icc,
            stride,
        )?,
        (None, None, n) => bail!("{n} color planes is not a TIFF this writes"),
    }
    written.push(color_path);

    if let Some(ir) = &samples.ir {
        // The mask measures obstructions rather than color, so no profile
        write_planes::<Gray16>(&ir_path, ir, pass, Source::Planes(&[0]), None, 1)?;
        written.push(ir_path);
    }

    Ok(written)
}

/// A table stretching `bits`-deep samples to fill 16, or `None` where they
/// already do
///
/// 0 stays 0 and the deepest value reaches 65535, evenly spaced between, which
/// for 8 bits is a multiply by 257. The scaling is linear, so the samples stay
/// linear. Without it an 8-bit scan would sit in the bottom 0.4% of the range,
/// which is both wrong to look at and wrong to hand a profile
fn full_scale(bits: u8) -> Option<Vec<u16>> {
    if bits == 0 || bits >= 16 {
        return None;
    }
    let top = u32::from(u16::MAX >> (16 - bits));
    Some(
        (0..=u32::from(u16::MAX))
            .map(|v| ((v.min(top) * 65535 + top / 2) / top) as u16)
            .collect(),
    )
}

/// Where a file's samples come from
enum Source<'a> {
    /// These channels of the pass, as they are
    Planes(&'a [usize]),
    /// One value weighed out of three channels, which is what a gray file of a
    /// color pass is
    Luminance {
        planes: [usize; 3],
        luminance: &'a Luminance,
    },
}

impl Source<'_> {
    /// Samples this puts in the file for each pixel
    fn per_pixel(&self) -> usize {
        match self {
            Source::Planes(planes) => planes.len(),
            Source::Luminance { .. } => 1,
        }
    }
}

/// Write `source` to one file, a strip at a time
///
/// `samples` interleaves `stride` channels; a TIFF holds only what is going in
/// this file, so each strip is gathered rather than copied
fn write_planes<C>(
    path: &Path,
    samples: &[u16],
    pass: &Pass,
    source: Source<'_>,
    icc: Option<&[u8]>,
    stride: usize,
) -> Result<()>
where
    C: ColorType<Inner = u16>,
{
    let file =
        BufWriter::new(File::create(path).with_context(|| format!("creating {}", path.display()))?);
    let mut tiff = TiffEncoder::new(file)?;
    let mut image = tiff.new_image::<C>(pass.cols as u32, pass.rows as u32)?;

    // 2-10 resolves both axes to the same pitch for a scan, so one number
    image.resolution(
        ResolutionUnit::Inch,
        Rational {
            n: pass.layout.dpi,
            d: 1,
        },
    );
    if let Some(icc) = icc {
        image.encoder().write_tag(Tag::IccProfile, icc)?;
    }

    // A sample carries the unit's own depth, so an 8-bit unit fills the low byte
    // of a u16 and nothing else. TIFF has no way to say "14 bits in 16", so the
    // samples are stretched to fill the container
    let scale = full_scale(pass.layout.bits_per_sample);

    let at = |pixel: usize, plane: usize| {
        let raw = samples[pixel * stride + plane];
        match &scale {
            Some(table) => table[usize::from(raw)],
            None => raw,
        }
    };

    let per_pixel = source.per_pixel();
    let mut strip = Vec::new();
    let mut done = 0usize;
    while image.next_strip_sample_count() > 0 {
        let count = image.next_strip_sample_count() as usize;
        let pixels = count / per_pixel;
        let first = done / per_pixel;

        strip.clear();
        strip.reserve(count);
        for pixel in first..first + pixels {
            match &source {
                Source::Planes(planes) => strip.extend(planes.iter().map(|&p| at(pixel, p))),
                Source::Luminance { planes, luminance } => strip.push(luminance.of([
                    at(pixel, planes[0]),
                    at(pixel, planes[1]),
                    at(pixel, planes[2]),
                ])),
            }
        }
        image.write_strip(&strip)?;
        done += count;
    }
    image.finish()?;

    debug!(path = %path.display(), samples = per_pixel, "wrote");
    Ok(())
}
