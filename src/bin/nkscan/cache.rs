//! The last discovery of each unit, kept in the user's cache directory until eject

use nkscan::protocol::caps::Capabilities;
use std::{env, fs, path::PathBuf};
use tracing::*;

/// The platform's cache directory for this program
fn dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("nkscan").join("cache"))
    } else if cfg!(target_os = "macos") {
        env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches/nkscan"))
    } else {
        env::var_os("XDG_CACHE_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .map(|d| d.join("nkscan"))
    };
    if base.is_none() {
        debug!("no cache directory, so each run finds the frames again");
    }
    base
}

/// One file for each model. The bytes contain the unit, adapter and film that they are for
fn path(caps: &Capabilities) -> Option<PathBuf> {
    let name: String = caps
        .identity
        .product
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    Some(dir()?.join(format!("{name}.discovery")))
}

/// The bytes of the last discovery on this unit, if there are any
pub fn load(caps: &Capabilities) -> Option<Vec<u8>> {
    fs::read(path(caps)?).ok()
}

/// Keep `bytes` for the next run. If this fails, the next run finds the frames again
pub fn save(caps: &Capabilities, bytes: &[u8]) {
    let Some(path) = path(caps) else { return };
    // A rename replaces the old file in one step, so a stopped write leaves no partial file
    let temp = path.with_extension("tmp");
    let written = path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::write(&temp, bytes))
        .and_then(|()| fs::rename(&temp, &path));
    match written {
        Ok(()) => debug!(path = %path.display(), "kept the discovery"),
        Err(e) => {
            warn!(%e, path = %path.display(), "could not keep the discovery for the next run")
        }
    }
}

/// Remove the discovery, because the film it is for is out of the unit
pub fn forget(caps: &Capabilities) {
    let Some(path) = path(caps) else { return };
    match fs::remove_file(&path) {
        Ok(()) => debug!(path = %path.display(), "removed the discovery"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(%e, path = %path.display(), "could not remove the discovery"),
    }
}
