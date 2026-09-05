//! The detected frame boundaries, saved to disk and read back

use anyhow::{Context, Result};
use nkscan::protocol::data::Rect;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize)]
pub struct Saved {
    /// Product string of the unit this came from. A mismatch is a warning,
    /// not a refusal: the coordinates may still be right
    pub product: Option<String>,
    /// Which framing mechanism produced this, as information only
    pub mechanism: String,
    /// The frames a scan will take, in scan order. This is the part to edit
    pub frames: Vec<RectDto>,
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
        Self {
            top: r.top,
            left: r.left,
            bottom: r.bottom,
            right: r.right,
        }
    }
}

impl From<RectDto> for Rect {
    fn from(r: RectDto) -> Self {
        Rect {
            top: r.top,
            left: r.left,
            bottom: r.bottom,
            right: r.right,
        }
    }
}

/// Pack a discovery result's frames for writing
pub fn from_discovery(product: Option<String>, mechanism: &str, frames: &[Rect]) -> Saved {
    Saved {
        product,
        mechanism: mechanism.to_string(),
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
    std::fs::write(path, json + "\n").with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn load(path: &Path) -> Result<Saved> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let saved: Saved = serde_json::from_str(&text).context("parsing the frame boundaries file")?;
    Ok(saved)
}
