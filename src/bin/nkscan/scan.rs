//! Scanning every frame of every holder the operator feeds in
//!
//! The order is the captures': find the frames, then per frame focus, meter and
//! take the pass. What that costs is a stage move each way, so it is done once
//! per frame rather than once per channel.

use crate::{cancel, cli, io};
use anyhow::{anyhow, bail};
use indicatif::{ProgressBar, ProgressStyle};
use nkscan::{
    device,
    error::Error,
    protocol::{
        caps::{other::HostCooperation, set_window::ColorInterleaving},
        decode::Samples,
    },
    scan::{
        autoexpose::Exposures,
        frame::{self, Phase},
        framing::{self, Framing},
        pass::Progress,
        profile,
        window::Recipe,
    },
    session::Session,
};
use std::{borrow::Cow, ops::ControlFlow, time::Duration};
use tracing::*;

/// How often to ask whether a holder has gone in
const HOLDER_POLL: Duration = Duration::from_millis(500);

/// How often the spinner moves while that is going on
const SPINNER_TICK: Duration = Duration::from_millis(120);

/// Scan what the flags ask for
pub fn run(args: cli::Scan) -> anyhow::Result<()> {
    // Every checkpoint below reports a Ctrl-c the same way the library
    // already reports a cancelled pass, so this is the one place that turns
    // it back into a clean stop rather than a failure
    match run_cancellable(args) {
        Err(e) if matches!(e.downcast_ref::<Error>(), Some(Error::Cancelled)) => {
            info!("Cancelled");
            Ok(())
        }
        other => other,
    }
}

fn run_cancellable(args: cli::Scan) -> anyhow::Result<()> {
    let cli::Scan {
        device,
        basename,
        unlock_wb,
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

    // First grab the requested device, or the first one
    let devices = device::list();
    let device = (if let Some(d) = device {
        device::Selector::Location(d)
    } else {
        device::Selector::Only
    })
    .resolve(&devices)?;

    // Start up a scan session
    let mut session = device::connect(device)?;
    info!("Connected to scanner");

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

    let mut color_interleave = ColorInterleaving::LINE_WITHOUT_DISTANCE;

    // A unit that never raises multi-line cooperation is never put into
    // MULTILINE_SIMULTANEOUS below, superfine or not, so the flag has nothing
    // to opt out of on it
    let multiline_offered = session
        .capabilities()
        .features
        .cooperation
        .contains(HostCooperation::MULTI_LINE);
    if superfine && !multiline_offered {
        warn!("this unit never scans multi-line, so --superfine changes nothing");
    }
    if !superfine && multiline_offered {
        color_interleave = ColorInterleaving::MULTILINE_SIMULTANEOUS;
    }

    // What every frame gets scanned with
    // Checked before anything moves
    let recipe = Recipe {
        dpi: dpi.unwrap_or(session.capabilities().address.x_axis.optical_dpi),
        samples,
        interleaving: color_interleave,
        // Cleaning reads the mask whether or not the operator wants it kept
        infrared: ir || clean,
    };
    // Everything the recipe asks for is checked against the pages here, before
    // the thumbnail pass and the stage move that would otherwise come first
    recipe.supported(session.capabilities())?;
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
        &mut session,
        &format!(
            "Load a film {}",
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

        let bar = pass_bar("thumbnail");
        let discovery =
            framing::discover_with(&mut session, format, film.into(), &mut samples, |p| {
                bar.report(p);
                if cancel::requested() {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })?;
        bar.finish_and_clear();

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
                lock_white_balance: !unlock_wb,
                clean,
            };
            let scanned = frame::scan_frame_with(
                &mut session,
                &recipe,
                frame,
                options,
                &mut samples,
                |phase, p| {
                    match phase {
                        Phase::Meter(pass) => {
                            let bar = meter_bar.get_or_insert_with(|| pass_bar("metering"));
                            // Each pass starts over, so say which one it is
                            if pass != shown {
                                bar.set_message(format!("meter {pass}"));
                                shown = pass;
                            }
                            bar.report(p);
                        }
                        Phase::Scan => {
                            if let Some(bar) = meter_bar.take() {
                                bar.finish_and_clear();
                            }
                            let bar = scan_bar
                                .get_or_insert_with(|| pass_bar(format!("frame {}", n + 1)));
                            bar.report(p);
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
                bar.finish_and_clear();
            }
            if let Some(bar) = scan_bar {
                bar.finish_and_clear();
            }

            info!(frame = n + 1, exposures = ?scanned.exposures, "Metered");
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
            info!("Ejected");
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
            if !ready(&mut session, true)? {
                info!("Nothing left to load");
                break;
            }
            info!("Film loaded");
            session.stage()?;
            continue;
        }

        // A unit with no UNLOAD cannot give the medium back, and what the
        // operator does with it is theirs to say: a strip holder is advanced
        // where a mount is swapped, and neither has to leave the gate
        if !ejected && !confirm("Replace or advance the film, then press Enter")? {
            break;
        }
        wait_for_film(&mut session, "Load the next strip", false)?;
    }
    Ok(())
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
        let spinner = ProgressBar::new_spinner();
        spinner.set_message(format!("{prompt}. Ctrl-c to stop"));
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
                spinner.finish_and_clear();
                break;
            }
        }
    }
    info!("Film loaded");
    session.stage()?;
    Ok(())
}

/// Ask the operator for something and wait for them, answering whether anyone
/// was there to answer. A blocked read has no checkpoint of its own to reach,
/// so a Ctrl-c here only stops on the second press - see `cancel`
fn confirm(prompt: &str) -> anyhow::Result<bool> {
    eprintln!("{prompt}. Ctrl-c to stop");
    let mut line = String::new();
    Ok(std::io::stdin().read_line(&mut line)? > 0)
}

/// A bar for one pass
///
/// The length is not known until the first chunk arrives, so it starts empty
/// and learns. Hidden by indicatif when stderr is not a terminal, and drawn no
/// more than 20 times a second, which is what keeps the callback off the
/// scanner's back
fn pass_bar(label: impl Into<Cow<'static, str>>) -> ProgressBar {
    let bar = ProgressBar::new(0);
    bar.set_style(
        ProgressStyle::with_template(
            "{msg:<9} [{bar:30}] {bytes}/{total_bytes}  {bytes_per_sec}  eta {eta}",
        )
        .expect("a template of ours")
        .progress_chars("=> "),
    );
    bar.set_message(label.into());
    bar
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
