//! Scanning every frame of every holder the operator feeds in
//!
//! The order is the captures': find the frames, then per frame focus, meter and
//! take the pass. What that costs is a stage move each way, so it is done once
//! per frame rather than once per channel.

use crate::{cancel, cli, io};
use anyhow::{anyhow, bail};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use nkscan::{
    device,
    error::Error,
    protocol::{caps::set_window::ColorInterleaving, decode::Samples},
    scan::{
        autoexpose::Exposures,
        frame::{self, Phase},
        framing::{self, Framing},
        meter::Metering,
        pass::Progress,
        profile,
        window::Recipe,
    },
    session::Session,
};
use std::{
    borrow::Cow,
    io::{IsTerminal, Write},
    ops::ControlFlow,
    time::Duration,
};
use tracing::*;

/// How often to ask whether a holder has gone in
const HOLDER_POLL: Duration = Duration::from_millis(500);

/// Long enough to tell a unit that is still answering from one that is gone,
/// and no longer: this only gates whether the eject below is worth attempting
const STILL_THERE: Duration = Duration::from_secs(5);

/// How often the spinner moves while that is going on
const SPINNER_TICK: Duration = Duration::from_millis(120);

/// Scan what the flags ask for
pub fn run(args: cli::Scan) -> anyhow::Result<()> {
    // First grab the requested device, or the first one
    let devices = device::list();
    let device = (if let Some(d) = args.device.clone() {
        device::Selector::Location(d)
    } else {
        device::Selector::Only
    })
    .resolve(&devices)?;

    // Start up a scan session. The session is opened here rather than inside
    // the run below so that a cancel is caught while it is still open: a
    // stopped run has film in the gate, and giving it back is the one thing
    // that still has to happen
    let mut session = Session::open(device.open()?)?;
    info!("connected to scanner");

    let no_eject = args.no_eject;

    // Every checkpoint below reports a Ctrl-c the same way the library
    // already reports a cancelled pass, so this is the one place that turns
    // it back into a clean stop rather than a failure
    match run_cancellable(&mut session, args) {
        Err(e) if matches!(e.downcast_ref::<Error>(), Some(Error::Cancelled)) => {
            if !no_eject {
                give_the_film_back(&mut session);
            }
            info!("cancelled");
            Ok(())
        }
        other => other,
    }
}

/// Hand the medium back on the way out of a cancelled run
///
/// Nothing above this is going to act on a failure - the run is already over -
/// so this reports rather than propagates.
///
/// The check first is not redundant. An eject is a stage move and carries a
/// stage move's timeout, so asking a unit that has already stopped answering
/// to do one costs three minutes of nothing. Every answer but a transport
/// failure means it is still there: a cancelled run leaves a scan open, and
/// being refused out of sequence is that saying so
fn give_the_film_back(session: &mut Session) {
    if let Err(Error::Transport(e)) = session.test_unit_ready(STILL_THERE) {
        warn!(%e, "the unit is not answering, so the film stays in the gate - power cycle it");
        return;
    }
    // A Ctrl-c while waiting for a strip has nothing to give back, and UNLOAD
    // against an empty gate still runs the mechanism for the best part of ten
    // seconds before saying so
    match session.media_loaded() {
        Ok(false) => {
            debug!("nothing was loaded, so there is nothing to give back");
            return;
        }
        Ok(true) => {}
        Err(e) => debug!(%e, "could not tell whether anything is loaded, trying the eject anyway"),
    }
    match session.eject() {
        Ok(true) => info!("ejected"),
        Ok(false) => debug!("this unit cannot give the medium back"),
        Err(e) => warn!(%e, "could not eject on the way out, so the film is still loaded"),
    }
}

fn run_cancellable(session: &mut Session, args: cli::Scan) -> anyhow::Result<()> {
    let cli::Scan {
        device: _,
        basename,
        unlock_wb,
        lock_wb,
        lock_ae,
        dpi,
        samples,
        superfine,
        frames,
        ir,
        clean,
        no_eject,
        thumbnail: save_thumbnail,
        format,
        film,
    } = args;

    // Silver grains stop infrared as they stop light, so the mask a
    // black and white negative returns is the picture again rather than
    // the dust on it
    if film == cli::FilmType::Mono && (ir || clean) {
        bail!("--ir and --clean do not work on black and white negatives");
    }

    // What the scans get tagged with, which follows the film type
    let icc = profile::nikon(&session.capabilities().identity, film.into());
    if icc.is_none() {
        warn!(
            product = session.capabilities().identity.product,
            "No color profile for this unit and film, so the scans will carry none"
        );
    }
    let caps = session.capabilities();
    let dpi = dpi.unwrap_or(caps.address.x_axis.optical_dpi);
    let multiline_supported = caps.reads_lines_at_once();
    let multiline_at_dpi = caps.reads_lines_at_once_at(dpi);

    let color_interleave = if !superfine && multiline_at_dpi {
        ColorInterleaving::MULTILINE_SIMULTANEOUS
    } else {
        ColorInterleaving::LINE_WITHOUT_DISTANCE
    };

    if superfine && !multiline_supported {
        warn!("this scanner has no multi-line scanning mode, so --superfine changes nothing");
    }
    if !superfine && multiline_supported && !multiline_at_dpi {
        info!(
            "at {dpi} dpi all the CCD rows give one output line, so the scan reads one row at a time and takes more time"
        );
    }

    debug!(
        ccd_lines = caps.address.lines,
        ?color_interleave,
        "selected scan interleaving"
    );

    // What every frame gets scanned with
    // Checked before anything moves
    let recipe = Recipe {
        dpi,
        samples,
        interleaving: color_interleave,
        // Cleaning reads the mask whether or not the operator wants it kept
        infrared: ir || clean,
    };
    // Everything the recipe asks for is checked against the pages here, before
    // the thumbnail pass and the stage move that would otherwise come first
    recipe.supported(session.capabilities())?;
    let hold_white_balance = hold_white_balance(film, unlock_wb, lock_wb);

    // State for the first frame's exposures, reused for the rest so a strip comes out consistent rather than per-frame optimal
    let mut locked: Option<Exposures> = None;

    let uses_adapter = !session.capabilities().identity.is_mf_scanner()
        && session
            .capabilities()
            .address
            .adapter_id
            .is_some_and(|id| id > 0);

    // Nothing can be framed before something is loaded
    wait_for_film(
        session,
        &format!(
            "load a film {}",
            if uses_adapter { "strip" } else { "holder" }
        ),
        true,
    )?;

    // One buffer for every strip
    let mut samples = Samples::default();

    // A strip at a time until the operator stops feeding them
    loop {
        // Nothing is moving between strips, so this is always safe to act on
        if cancel::requested() {
            return Err(Error::Cancelled.into());
        }

        // Only to warn early if --thumbnail would save nothing; discover_with
        // resolves --format itself, against whichever mechanism this picks
        let framing = Framing::choose(session.capabilities());
        if save_thumbnail && !matches!(framing, Framing::Thumbnail | Framing::Perforation) {
            warn!("this unit frames without a thumbnail pass, so --thumbnail saves nothing");
        }

        // Where this strip starts writing, so a second strip through the same
        // basename carries on rather than overwriting the first. Taken before
        // discovery because the thumbnail is numbered with it
        let first = io::next_free(&basename);

        // Only two of the four mechanisms take a pass to find the frames; the
        // others read them off a page and would leave a bar claiming a pass was
        // running. Made on the first report with a length for the same reason
        // the metering bars are
        let takes_pass = matches!(framing, Framing::Thumbnail | Framing::Perforation);
        let mut bar: Option<ProgressBar> = None;
        let discovery = framing::discover_with(session, format, film.into(), &mut samples, |p| {
            if takes_pass && bar.is_none() && p.total > 0 {
                bar = Some(pass_bar("thumbnail", p.total));
            }
            if let Some(bar) = &bar {
                bar.report(p);
            }
            if cancel::requested() {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })?;
        if let Some(bar) = bar {
            crate::progress::done(bar);
        }

        if save_thumbnail && let Some(pass) = &discovery.thumbnail {
            let path = io::write_thumbnail(&basename, first, &samples, pass)?;
            info!("wrote {}", path.display());
        }

        // Select all or the requested subset of the frames to scan
        let selected_frames = if frames.is_empty() {
            discovery.frames
        } else {
            frames
                .iter()
                .map(|&idx| {
                    discovery.frames.get(idx - 1).cloned().ok_or(anyhow!(
                        "Requested frame {} not available. Frames detected: {}",
                        idx,
                        discovery.frames.len()
                    ))
                })
                .collect::<anyhow::Result<Vec<_>>>()?
        };

        if selected_frames.is_empty() {
            warn!("No frames on this strip");
        }

        // Scan each frame
        for (n, frame) in selected_frames.into_iter().enumerate() {
            // Nothing is moving between frames either - the stage only
            // starts heading for the next one once this loop continues
            if cancel::requested() {
                return Err(Error::Cancelled.into());
            }

            // Lazy, and at most one live at a time: metering may not run at
            // all (a locked exposure), and indicatif has no idea what to do
            // with two bars neither owns unless one finishes before the next starts
            let mut meter_bar: Option<ProgressBar> = None;
            let mut scan_bar: Option<ProgressBar> = None;
            let mut shown = 0;

            let options = frame::Options {
                exposures: locked.as_ref(),
                lock_white_balance: hold_white_balance,
                clean,
            };
            let scanned = frame::scan_frame_with(
                session,
                &recipe,
                frame,
                options,
                &mut samples,
                |phase, p| {
                    match phase {
                        Phase::Meter(pass) => {
                            // A pass does not know how long it is until its
                            // first chunk lands, so a bar made when the phase
                            // starts draws `0 B/0 B` and sits at it. Waiting for
                            // a length keeps that line off the screen, and names
                            // the bar for the pass it is rather than renaming it
                            if meter_bar.is_none() && p.total > 0 {
                                meter_bar = Some(pass_bar(format!("meter {pass}"), p.total));
                                shown = pass;
                            }
                            if let Some(bar) = &meter_bar {
                                // Each pass starts over, so say which one it is
                                if pass != shown {
                                    bar.set_message(format!("meter {pass}"));
                                    shown = pass;
                                }
                                bar.report(p);
                            }
                        }
                        Phase::Scan => {
                            if let Some(bar) = meter_bar.take() {
                                crate::progress::done(bar);
                            }
                            if scan_bar.is_none() && p.total > 0 {
                                scan_bar = Some(pass_bar(format!("frame {}", n + 1), p.total));
                            }
                            if let Some(bar) = &scan_bar {
                                bar.report(p);
                            }
                        }
                    }
                    if cancel::requested() {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                },
            )?;
            if let Some(bar) = meter_bar {
                crate::progress::done(bar);
            }
            if let Some(bar) = scan_bar {
                crate::progress::done(bar);
            }

            info!(frame = n + 1, exposures = ?scanned.exposures, "metered");
            if lock_ae {
                locked.get_or_insert_with(|| scanned.exposures.clone());
            }

            let pass = scanned.pass;
            if !pass.complete {
                warn!(
                    frame = n + 1,
                    "The unit gave less than the pass promised, writing what arrived"
                );
            }
            if let Some(removed) = scanned.cleaned {
                info!(frame = n + 1, "cleaned {removed} pixels");
            }

            let written = io::write_frame(
                &basename,
                first + n,
                &samples,
                &pass,
                icc,
                film == cli::FilmType::Mono,
                ir,
            )?;
            info!(
                frame = n + 1,
                "{} x {} at {} dpi, wrote {}",
                pass.cols,
                pass.rows,
                pass.layout.dpi,
                written
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        if no_eject {
            break;
        }
        let ejected = session.eject()?;
        if ejected {
            info!("ejected");
        }

        // Naming frames is a targeted scan rather than a batch, and the
        // numbers mean nothing on the next holder anyway
        if !frames.is_empty() {
            break;
        }

        // A unit with a supply behind the gate takes the next medium in itself,
        // and is also the only one that can say when it has run out
        if framing::self_feeding(session.capabilities()) {
            session.refresh()?;
            if !ready(session, true)? {
                info!("nothing left to load");
                break;
            }
            info!("film loaded");
            session.stage()?;
            continue;
        }

        // A unit with no UNLOAD cannot give the medium back, and what the
        // operator does with it is theirs to say: a strip holder is advanced
        // where a mount is swapped, and neither has to leave the gate
        if !ejected && !confirm("Replace or advance the film, then press Enter")? {
            break;
        }
        wait_for_film(session, "load the next strip", false)?;
    }
    Ok(())
}

/// Whether to meter the channels as one group, from the film type and whatever
/// the operator asked for
///
/// The film type decides on its own, and each flag overrides it. An override
/// that contradicts the default is worth saying out loud: on a negative it is
/// the difference between the orange mask coming off before the ADC and being
/// quantised through, and there is nothing in the output that says which
/// happened
fn hold_white_balance(film: cli::FilmType, unlock: bool, lock: bool) -> bool {
    let default = Metering::locks_white_balance(film.into());
    let asked = match (lock, unlock) {
        (true, _) => true,
        (_, true) => false,
        // clap rejects both at once, so this is neither
        _ => return default,
    };
    let name = |held: bool| if held { "locked" } else { "unlocked" };
    if asked == default {
        info!(
            ?film,
            "white balance is already {} for this film, so the flag changes nothing",
            name(default)
        );
    } else {
        warn!(
            ?film,
            "white balance {} by request; this film is otherwise metered {}",
            name(asked),
            name(default)
        );
    }
    asked
}

/// Whether there is film to scan
///
/// A feeder or a cartridge keeps its film behind the gate, so nothing reads as
/// loaded until the unit is told to take some in. `take_in` is whether it may:
/// false once a medium has been scanned and ejected, where loading again would
/// only bring the same film back
fn ready(session: &mut Session, take_in: bool) -> Result<bool, Error> {
    if session.media_loaded()? {
        return Ok(true);
    }
    if !take_in || !session.load()? {
        return Ok(false);
    }
    // The address page now describes the medium that came in
    session.refresh()?;
    session.media_loaded()
}

/// The same, counting anything the operator can put right as "not yet": an open
/// door is what the prompt is for
fn waiting(session: &mut Session, take_in: bool) -> Result<bool, Error> {
    match ready(session, take_in) {
        Err(Error::Media(condition)) => {
            debug!(%condition, "waiting on the operator");
            Ok(false)
        }
        other => other,
    }
}

/// [`Session::refresh`], tolerating the medium-not-present it can itself hit:
/// a strip feeder's gate sits empty between strips, and that is the state this
/// is called from a loop to wait out, not a failure of the refresh
fn refresh_while_empty(session: &mut Session) -> Result<(), Error> {
    match session.refresh() {
        Ok(()) | Err(Error::Media(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Wait until a holder is loaded, then put the unit in a state to scan from
fn wait_for_film(session: &mut Session, prompt: &str, take_in: bool) -> anyhow::Result<()> {
    // An eject leaves what we know about the holder behind, so ask again before
    // believing anything is in there
    refresh_while_empty(session)?;

    if !waiting(session, take_in)? {
        // The spinner is the affordance on a terminal, and hidden anywhere else, so the log says it too
        info!("{prompt}");
        let spinner = ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden());
        spinner.set_style(ProgressStyle::default_spinner());
        spinner.set_message(format!("{prompt}. Ctrl-c to stop"));
        let spinner = crate::progress::add(spinner);
        spinner.enable_steady_tick(SPINNER_TICK);
        loop {
            // Nothing is moving while this waits, so stopping here is always safe
            if cancel::requested() {
                return Err(Error::Cancelled.into());
            }
            std::thread::sleep(HOLDER_POLL);
            refresh_while_empty(session)?;
            // Retrying the load is what picks up a supply that was refilled
            if waiting(session, take_in)? {
                crate::progress::done(spinner);
                break;
            }
        }
    }
    info!("film loaded");
    session.stage()?;
    Ok(())
}

/// Ask the operator for something and wait for them, answering whether anyone
/// was there to answer. Polls for Ctrl-c on a background reader thread, same
/// as every other wait
fn confirm(prompt: &str) -> anyhow::Result<bool> {
    eprintln!("{prompt}. Ctrl-c to stop");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = tx.send(std::io::stdin().read_line(&mut line).map(|n| n > 0));
    });
    loop {
        if cancel::requested() {
            return Err(Error::Cancelled.into());
        }
        match rx.recv_timeout(HOLDER_POLL) {
            Ok(answered) => {
                // The Enter that answered this has been echoed as a line of its
                // own, so step back over it rather than leaving a blank line
                let mut err = std::io::stderr().lock();
                if err.is_terminal() {
                    let _ = err.write_all(b"\x1b[1A\x1b[2K");
                }
                return Ok(answered?);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(false),
        }
    }
}

/// A bar for one pass
///
/// The length is not known until the first chunk arrives, so it starts empty
/// and learns. Hidden by indicatif when stderr is not a terminal, and drawn no
/// more than 20 times a second, which is what keeps the callback off the
/// scanner's back
fn pass_bar(label: impl Into<Cow<'static, str>>, total: u64) -> ProgressBar {
    // A bar built the usual way draws straight to stderr, so styling and naming
    // it here would leave that first line behind the moment `add` moves it onto
    // the shared draw target. Built hidden, it draws nothing until it is added
    let bar = ProgressBar::with_draw_target(Some(total), ProgressDrawTarget::hidden());
    bar.set_style(
        ProgressStyle::with_template(
            "{msg:<9} [{bar:30}] {bytes}/{total_bytes}  {bytes_per_sec}  eta {eta}",
        )
        .expect("a template of ours")
        .progress_chars("=> "),
    );
    bar.set_message(label.into());
    crate::progress::add(bar)
}

/// Moving a pass's progress onto a bar
trait Report {
    fn report(&self, progress: Progress);
}

impl Report for ProgressBar {
    fn report(&self, progress: Progress) {
        // The layout's own total, which the cooperative modes can make wrong, so
        // it is set every time rather than once
        self.set_length(progress.total);
        self.set_position(progress.bytes);
    }
}
