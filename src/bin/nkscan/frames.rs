//! The detected frame boundaries, saved to disk and read back

use anyhow::{Context, Result};
use nkscan::protocol::data::{FramePosition, FrameTable, Rect};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Bump when the shape changes so an old file says so rather than misreads
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub struct Saved {
    pub version: u32,
    /// Product string of the unit this came from. A mismatch is a warning,
    /// not a refusal: the coordinates may still be right
    pub product: Option<String>,
    /// Which framing mechanism produced this, as information only
    pub mechanism: String,
    pub table: TableDto,
    /// The frames a scan will take, in scan order. This is the part to edit
    pub frames: Vec<RectDto>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TableDto {
    /// What was sent as `DataType::Boundary`
    Boundary { frames: Vec<RectDto> },
    /// What was sent as `DataType::BoundaryType2` (perforation-indexed)
    #[serde(rename = "boundary_type2")]
    BoundaryType2 { frames: Vec<FramePosDto> },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct RectDto {
    pub top: u32,
    pub left: u32,
    pub bottom: u32,
    pub right: u32,
}

impl From<Rect> for RectDto {
    fn from(r: Rect) -> Self {
        Self { top: r.top, left: r.left, bottom: r.bottom, right: r.right }
    }
}

impl From<RectDto> for Rect {
    fn from(r: RectDto) -> Self {
        Rect { top: r.top, left: r.left, bottom: r.bottom, right: r.right }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct FramePosDto {
    pub top: u32,
    pub perf_number: u16,
    pub perf_decimal: u8,
    pub pulse_number: u8,
}

impl From<FramePosition> for FramePosDto {
    fn from(f: FramePosition) -> Self {
        Self {
            top: f.top,
            perf_number: f.perf_number,
            perf_decimal: f.perf_decimal,
            pulse_number: f.pulse_number,
        }
    }
}

impl From<FramePosDto> for FramePosition {
    fn from(f: FramePosDto) -> Self {
        FramePosition {
            top: f.top,
            perf_number: f.perf_number,
            perf_decimal: f.perf_decimal,
            pulse_number: f.pulse_number,
        }
    }
}

/// Pack a discovery result's table and frames for writing
pub fn from_discovery(
    product: Option<String>,
    mechanism: &str,
    table: &FrameTable,
    frames: &[Rect],
) -> Saved {
    let table = match table {
        FrameTable::Boundary(b) => TableDto::Boundary {
            frames: b.frames.iter().map(|&r| r.into()).collect(),
        },
        FrameTable::BoundaryType2(t) => TableDto::BoundaryType2 {
            frames: t.frames.iter().map(|&f| f.into()).collect(),
        },
    };
    Saved {
        version: VERSION,
        product,
        mechanism: mechanism.to_string(),
        table,
        frames: frames.iter().map(|&r| r.into()).collect(),
    }
}

impl Saved {
    /// The frames a scan will take, in scan order
    pub fn frames(&self) -> Vec<Rect> {
        self.frames.iter().map(|&r| r.into()).collect()
    }
}

pub fn save(path: &Path, saved: &Saved) -> Result<()> {
    let json = serde_json::to_string_pretty(saved).context("serializing the frame boundaries")?;
    std::fs::write(path, json + "\n")
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn load(path: &Path) -> Result<Saved> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let saved: Saved =
        serde_json::from_str(&text).context("parsing the frame boundaries file")?;
    if saved.version != VERSION {
        anyhow::bail!(
            "{} is a v{} boundaries file, but this build reads v{}",
            path.display(),
            saved.version,
            VERSION
        );
    }
    Ok(saved)
}
