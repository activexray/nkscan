//! Finding the frames without scanning them
//!
//! Half of a two-step workflow: `discover` runs whatever framing pass the unit
//! needs, writes the detected boundaries to `<basename>_<n>_frames.json`, and
//! leaves the film where it is. The operator edits the rectangles in that file.
//!
//! Nothing here ejects by default, because the point is to scan the same strip
//! afterwards.

use crate::{cancel, cli, common, frames, io};
use anyhow::Result;
use nkscan::{device, error::Error, protocol::decode::Samples, scan::framing, session::Session};
use std::{ops::ControlFlow, path::PathBuf};
use tracing::*;

/// Find the frames on what is loaded, and write them out
pub fn run(args: cli::Discover) -> Result<()> {
    let devices = device::list();
    let device = (if let Some(d) = args.device.clone() {
        device::Selector::Location(d)
    } else {
        device::Selector::Only
    })
    .resolve(&devices)?;

    let mut session = Session::open(device.open()?)?;
    info!("connected to scanner");

    match discover_cancellable(&mut session, &args) {
        Err(e) if matches!(e.downcast_ref::<Error>(), Some(Error::Cancelled)) => {
            info!("cancelled");
            Ok(())
        }
        other => other,
    }
}

fn discover_cancellable(session: &mut Session, args: &cli::Discover) -> Result<()> {
    let cli::Discover {
        device: _,
        basename,
        format,
        film,
        thumbnail: save_thumbnail,
        eject,
    } = args;

    let product = session.capabilities().identity.product.clone();

    common::wait_for_film(session, "load a film strip", true)?;

    // One buffer for the thumbnail pass, as the scan command keeps per strip
    let mut samples = Samples::default();

    // Numbered like everything else through this basename, so a re-discover of
    // the same strip overwrites its own output rather than anyone else's
    let first = io::next_free(basename);

    let mechanism = framing::Framing::choose(session.capabilities());
    let mut bar = common::PassBar::new("thumbnail");
    let discovery = framing::discover_with(session, *format, (*film).into(), &mut samples, |p| {
        bar.update(p);
        if cancel::requested() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })?;
    bar.done();

    if *save_thumbnail && let Some(pass) = &discovery.thumbnail {
        let path = io::write_thumbnail(basename, first, &samples, pass)?;
        info!("wrote {}", path.display());
    }

    let path = PathBuf::from(format!("{}_{first}_frames.json", basename.display()));
    let saved = frames::from_discovery(
        Some(product.to_string()),
        &format!("{mechanism:?}"),
        &discovery.frames,
    );
    frames::save(&path, &saved)?;
    info!(frames = discovery.frames.len(), "{}", path.display());

    if *eject {
        let ejected = session.eject()?;
        if ejected {
            info!("ejected");
        }
    } else {
        info!("film left loaded for `nkscan scan --frames-file`");
    }
    Ok(())
}
