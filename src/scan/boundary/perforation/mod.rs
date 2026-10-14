//! Frames on a strip that the unit counts by perforation (LS5000.md3)
//!
//! [`resample`] the thumbnail against the 8Eh records, find the [`frames`], and make the 8Fh [`table`].

mod adjust;
mod resample;
mod search;

use super::{analysis_image, saturating};
use crate::{
    protocol::{
        data::{BoundaryType2, FramePosition, PerfInformation},
        decode::Image,
    },
    scan::profile::Film,
};
use resample::Spec;
use std::ops::RangeInclusive;
use tracing::*;

/// Resampled lines in one frame (`OffsetAnalyze`, `1008b9f0`)
const FRAME_LINES: usize = 144;

/// The maximum nudge in lines (`CSAThumbCtrl::BaseLineOffset`, Mac `10049ae0`)
const MAX_NUDGE: i64 = 0x90;

/// `RemakeThumbnail` (`1008a110`): the pass lines that Nikon keeps, as indices into `perfs`
pub fn resample(perfs: &PerfInformation, optical_dpi: u16, line_pitch: u32) -> Vec<usize> {
    resample::resample(&perfs.perfs, &Spec::for_dpi(optical_dpi), line_pitch)
}

/// The first line of each frame in the [`resample`]d thumbnail; `frames` is the number the strip holds
pub fn frames(
    image: &Image,
    perfs: &PerfInformation,
    optical_dpi: u16,
    line_pitch: u32,
    film: Film,
    frames: usize,
) -> Option<Vec<usize>> {
    if image.colors.len() < 3 {
        return None;
    }
    let kept = resample(perfs, optical_dpi, line_pitch);
    let valid = kept.len();
    if valid == 0 {
        return None;
    }
    // OffsetAnalyze (1008b9f0): the resampled lines, then lines up to two frames past the last
    let lines = ((frames + 2) * FRAME_LINES).min(image.stride);
    // Nikon turns the image only if it has more lines than pixels
    if lines < image.rows {
        return None;
    }
    let gray = analysis_image(image, &kept, film, lines, saturating);
    let found = search::search(gray, frames, film.is_negative())?;
    let found = adjust::adjust(&found, valid as i64)?;
    debug!(?found, valid, "frames on the resampled thumbnail");
    // Nikon reads a line below 1 as line 1
    Some(
        found
            .iter()
            .map(|&(s, _)| usize::try_from(s).unwrap_or(0))
            .collect(),
    )
}

/// `ThumbnailReadCheck` (`10083180`): the 8Fh table for frames at `lines` of the resampled `kept`
pub fn table(
    perfs: &PerfInformation,
    optical_dpi: u16,
    kept: &[usize],
    lines: impl IntoIterator<Item = i64>,
) -> BoundaryType2 {
    let spec = Spec::for_dpi(optical_dpi);
    let valid = kept.len() as i64;
    let mut frames: Vec<FramePosition> = lines
        .into_iter()
        .filter_map(|line| {
            let r = perfs.at(*kept.get(line.clamp(1, valid.max(1)) as usize - 1)?)?;
            Some(FramePosition::new(spec.address(r), r))
        })
        .collect();
    frames.sort_by_key(|f| f.top);
    BoundaryType2 { frames }
}

/// Lines Nikon shows before a frame's record line (`CSAThumbCtrl::ReadData`, `100814f0`)
const SHOWN_BEFORE: usize = 2;

/// The feed address of each line of the [`resample`]d thumbnail. The addresses never decrease
///
/// A line takes the record address of the line [`SHOWN_BEFORE`] lines after it.
/// Where that address is lower than the one before it, the line keeps the one before it.
/// This occurs where a decimal changes and past the end of the film.
pub(crate) fn addresses(perfs: &PerfInformation, optical_dpi: u16, kept: &[usize]) -> Vec<u32> {
    let spec = Spec::for_dpi(optical_dpi);
    let records: Vec<u32> = kept
        .iter()
        .filter_map(|&line| perfs.at(line))
        .scan(0, |last, r| {
            *last = spec.address(r).max(*last);
            Some(*last)
        })
        .collect();
    let last = records.len().saturating_sub(1);
    (0..records.len())
        .map(|i| records[(i + SHOWN_BEFORE).min(last)])
        .collect()
}

/// The entry for a rectangle at `top`: the last record at or before `top`
pub fn record_at(perfs: &PerfInformation, optical_dpi: u16, top: u32) -> Option<FramePosition> {
    let spec = Spec::for_dpi(optical_dpi);
    let r = perfs.perfs.iter().rev().find(|r| spec.address(r) <= top)?;
    Some(FramePosition::new(top, r))
}

/// `CSAThumbCtrl::BaseLineOffset` (Mac `10049ae0`): the nudge range of a frame at line `start`
pub fn nudge_range(valid: usize, start: usize) -> RangeInclusive<i64> {
    let (start, valid) = (start as i64, valid as i64);
    let max = if ((valid - start) as u32) <= MAX_NUDGE as u32 {
        0
    } else {
        (valid - (start + MAX_NUDGE)).min(MAX_NUDGE)
    };
    -start.min(MAX_NUDGE)..=max
}

#[cfg(test)]
mod tests;
