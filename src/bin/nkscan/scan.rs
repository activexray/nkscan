//! Scanning every frame of every holder the operator feeds in
//!
//! The order is the captures': find the frames, then per frame focus, meter and
//! take the pass. What that costs is a stage move each way, so it is done once
//! per frame rather than once per channel.

use crate::{cli, io};
use anyhow::{anyhow, bail};
use indicatif::{ProgressBar, ProgressStyle};
use nkscan::{
    device, dust,
    protocol::{
        caps::{film::FilmFormat, other::HostCooperation, set_window::ColorInterleaving},
        data::{Boundary, FrameTable},
        decode::Samples,
        model::Model,
        window::Channel,
    },
    scan::{
        autoexpose::Exposures,
        focus::Focus,
        framing::{self, Framing},
        meter::Metering,
        pass::{Pass, Progress},
        profile, thumbnail,
        window::Recipe,
    },
    session::Session,
};
use std::{
    borrow::Cow,
    time::{Duration, Instant},
};
use tracing::*;

/// Long enough for a full resolution pass over the largest frame
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(1800);

/// How often to ask whether a holder has gone in
const HOLDER_POLL: Duration = Duration::from_millis(500);

/// How often the spinner moves while that is going on
const SPINNER_TICK: Duration = Duration::from_millis(120);

/// Scan what the flags ask for
pub fn run(args: cli::Scan) -> anyhow::Result<()> {
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
    let mut session = Session::open(device.open()?)?;
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

    let uses_ma21 = uses_adapter
        && session
            .capabilities()
            .address
            .connected_adapter
            .is_some_and(|id| id == 34);

    // Nothing can be framed before something is loaded
    wait_for_film(
        &mut session,
        &format!(
            "Load a film {}",
            if uses_adapter { "strip" } else { "holder" }
        ),
    )?;

    // One buffer for every strip
    let mut samples = Samples::default();

    // A strip at a time until the operator stops feeding them
    loop {
        // Decide how the frames are found from what this unit and adapter
        // advertise. This picks one of four mechanisms; we only thumbnail
        // where the adapter offers it and the unit publishes no lengths.
        let framing = Framing::choose(session.capabilities());
        debug!(?framing, "Frame discovery mechanism");

        // Resolve the film format up front where it will be needed, so a
        // missing --format fails before the thumbnail pass
        let film_format = match framing {
            Framing::Thumbnail | Framing::Perforation => Some(resolve_format(
                format,
                if !uses_adapter {
                    session.capabilities().address.holder_id
                } else {
                    session.capabilities().address.connected_adapter
                },
            )?),
            _ => None,
        };

        let (_table, scan_frames) = match framing {
            Framing::Published => {
                let table = framing::table(session.capabilities())?;
                let frames = table.frames.clone();

                (FrameTable::Boundary(table), frames)
            }
            Framing::Thumbnail => {
                let bar = pass_bar("thumbnail");
                let pass = session.scan_thumbnail_with(&mut samples, |p| bar.report(p))?;
                bar.finish_and_clear();
                debug!(
                    "thumbnail {} x {} in {} channels, complete={}",
                    pass.cols,
                    pass.rows,
                    pass.layout.channels.len(),
                    pass.complete
                );

                let film_format = film_format.expect("resolved before the pass");
                let optical_dpi = session.capabilities().address.y_axis.optical_dpi;
                let length = film_format.height_dots(optical_dpi);
                info!(?film_format, length, "frame length");

                // Write the detected frames to the scanner's boundary table
                let measured =
                    thumbnail::frames(session.capabilities(), &pass, &samples, length, None)?;
                session.set_boundaries(&measured)?;

                let frames = measured.frames.clone();

                info!(frames = measured.frames.len(), "detected frames");
                (FrameTable::Boundary(measured), frames)
            }
            Framing::Caller => {
                let frame = framing::single_frame(session.capabilities())?;
                (
                    FrameTable::Boundary(Boundary {
                        frames: vec![frame],
                    }),
                    vec![frame],
                )
            }
            Framing::Perforation => {
                // discard old data
                let _ = session.read_perforations()?;
                let _ = session.read_boundaries_type2();
                let bar = pass_bar("thumbnail");

                let pass = session.scan_thumbnail_with(&mut samples, |p| bar.report(p))?;
                bar.finish_and_clear();
                debug!(
                    "thumbnail {} x {} in {} channels, complete={}",
                    pass.cols,
                    pass.rows,
                    pass.layout.channels.len(),
                    pass.complete
                );

                let film_format = film_format.expect("resolved before the pass");
                let optical_dpi = session.capabilities().address.y_axis.optical_dpi;
                let length = film_format.height_dots(optical_dpi);
                info!(?film_format, length, "frame length");

                // Read perf data and use it to generate Boundary Type2 data for telling the scanner
                // where the frames reside
                let perfs = session.read_perforations()?;
                let measured = thumbnail::frames_type2(
                    session.capabilities(),
                    &pass,
                    &samples,
                    &perfs,
                    length,
                    None,
                )?;

                session.set_boundaries_type2(&measured)?;

                let x_start = session.capabilities().address.x_axis.address_range.start;
                let x_boundary = session.capabilities().address.x_axis.boundary;

                let frames = measured
                    .frames
                    .iter()
                    .map(|f| f.rect(x_start, x_boundary, length))
                    .collect();
                info!(frames = measured.frames.len(), "detected frames");
                (FrameTable::BoundaryType2(measured), frames)
            }
        };

        // Select all or the requested subset of the frames to scan
        let selected_frames = if frames.is_empty() {
            scan_frames
        } else {
            frames
                .iter()
                .map(|&idx| {
                    scan_frames.get(idx - 1).cloned().ok_or(anyhow!(
                        "Requested frame {} not available. Frames detected: {}",
                        idx,
                        scan_frames.len()
                    ))
                })
                .collect::<anyhow::Result<Vec<_>>>()?
        };

        if selected_frames.is_empty() {
            warn!("No frames on this strip");
        }

        // Where this strip starts writing, so a second strip through the
        // same basename carries on rather than overwriting the first
        let first = io::next_free(&basename);

        // Scan each frame
        for (n, frame) in selected_frames.into_iter().enumerate() {
            // Build the scan windows (one for each color, shared resolution, size, etc) from the frame (just position/size in the scanner)
            let mut windows = recipe.windows(session.capabilities(), frame)?;

            // Autofocus at the center of this frame
            let focused = session.focus_frame(frame, Focus::default())?;
            info!(frame = n + 1, ?focused, "Focused");

            // Autoexpose with reused exposure gains if locked
            let exposures = match &locked {
                Some(held) => held.clone(),
                None => {
                    let bar = pass_bar("metering");
                    let mut shown = 0;
                    let measured =
                        session.autoexpose_frame_with(frame, &recipe, !unlock_wb, |pass, p| {
                            // Each pass starts over, so say which one it is
                            if pass != shown {
                                bar.set_message(format!("meter {pass}"));
                                shown = pass;
                            }
                            bar.report(p);
                        })?;
                    bar.finish_and_clear();
                    // If this was the first frame, save its exposure
                    if lock_ae {
                        locked = Some(measured.clone());
                    }
                    measured
                }
            };
            info!(frame = n + 1, ?exposures, "Metered");

            // Apply the exposures to the windows
            exposures.apply(&mut windows);

            // Perform the scan pass
            let bar = pass_bar(format!("frame {}", n + 1));
            let pass =
                session.scan_pass_with(&windows, SCAN_TIMEOUT, &mut samples, |p| bar.report(p))?;
            bar.finish_and_clear();
            if !pass.complete {
                warn!(
                    frame = n + 1,
                    "The unit gave less than the pass promised, writing what arrived"
                );
            }

            // Everything after the pass assumes full-scale 16-bit samples
            io::to_full_scale(&mut samples, pass.layout.bits_per_sample);

            if clean {
                let model = session.capabilities().identity.model();
                let started = Instant::now();
                let removed = clean_frame(&mut samples, &pass, model)?;
                info!(
                    frame = n + 1,
                    "cleaned {removed} pixels in {} ms",
                    started.elapsed().as_millis()
                );
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

        if uses_ma21 {
            println!();
            println!("Scan complete.");
            println!("Replace or advance the film in the MA-21, then press Enter.");
            println!("Press Ctrl-C to stop.");

            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;

            continue;
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

        if !ejected {
            // No UNLOAD, such as the MA-21, still gives a real not-ready
            // blip when the operator takes out what's seated (2-1's Medium
            // Not Present, case a). wait_for_film alone would not catch it:
            // it only waits when media_loaded() is already false, and right
            // after a scan whatever was loaded is still seated, so it would
            // return at once and this frame would be scanned again
            wait_for_unload(
                &mut session,
                &format!(
                    "Take out the loaded {}",
                    if uses_adapter { "film" } else { "holder" }
                ),
            )?;
        }
        wait_for_film(&mut session, "Load the next strip")?;
    }
    Ok(())
}

/// About how many pixels calibration wants to measure over
const PRESCAN_PIXELS: usize = 2_500_000;

/// Every `step`th pixel of a plane, which is all calibration needs
fn decimate(plane: &[u16], cols: usize, step: usize) -> Vec<u16> {
    let rows = plane.len() / cols;
    let mut out = Vec::with_capacity((rows / step) * (cols / step));
    for y in (0..rows - rows % step).step_by(step) {
        for x in (0..cols - cols % step).step_by(step) {
            out.push(plane[y * cols + x]);
        }
    }
    out
}

/// Run dust removal over a finished pass, in place, returning how many pixels it rebuilt
fn clean_frame(samples: &mut Samples, pass: &Pass, model: Option<Model>) -> anyhow::Result<usize> {
    // The buffer holds the pass's color channels in the stream's own order
    let ids: Vec<u8> = pass.layout.colors().collect();
    let at = |want: Channel| ids.iter().position(|&id| Channel::from(id) == want);
    let (Some(r), Some(g), Some(b)) = (at(Channel::Red), at(Channel::Green), at(Channel::Blue))
    else {
        bail!("--clean needs a red, green and blue plane, this pass has {ids:?}");
    };

    let model = model.map(dust::Model::from).unwrap_or_else(|| {
        warn!("unrecognized scanner, cleaning with a default profile");
        dust::Model::Ls9000
    });
    let opts = dust::Options {
        model,
        quality: dust::Quality::Normal,
        dpi: pass.layout.dpi,
        // What autoexpose::Plan hands the host meter
        metering_target: Metering::default().target,
    };

    let Some(ir) = samples.ir.as_deref() else {
        bail!("--clean needs the infrared pass");
    };

    // Red and infrared only: that is all calibration reads
    let step = ((pass.rows * pass.cols) / PRESCAN_PIXELS).isqrt().max(1);
    let small_red = decimate(&samples.colors[r], pass.cols, step);
    let small_ir = decimate(ir, pass.cols, step);
    let cal = dust::calibrate(&dust::Prescan {
        red: &small_red,
        ir: &small_ir,
        rows: pass.rows / step,
        cols: pass.cols / step,
    })
    .or_else(|| {
        // A frame with little clear film can have none left after decimation while the full pass still has plenty
        warn!("no clear film in the decimated prescan, calibrating off the whole frame");
        dust::calibrate(&dust::Prescan {
            red: &samples.colors[r],
            ir,
            rows: pass.rows,
            cols: pass.cols,
        })
    })
    .ok_or_else(|| anyhow!("no clear film in this frame to calibrate --clean against"))?;
    debug!(?cal, step, "ICE calibration");

    let [pr, pg, pb] = samples
        .colors
        .get_disjoint_mut([r, g, b])
        .expect("three distinct color planes");
    let count = dust::clean([pr, pg, pb], ir, &cal, pass.rows, pass.cols, &opts);
    Ok(count)
}

/// Wait until a holder is loaded, then put the unit in a state to scan from
fn wait_for_film(session: &mut Session, prompt: &str) -> anyhow::Result<()> {
    // An eject leaves what we know about the holder behind, so ask again before
    // believing anything is in there
    session.refresh()?;

    if !session.media_loaded()? {
        // The spinner is the affordance on a terminal, and hidden anywhere else, so the log says it too
        info!("{prompt}");
        let spinner = ProgressBar::new_spinner();
        spinner.set_message(format!("{prompt}. Ctrl-c to stop"));
        spinner.enable_steady_tick(SPINNER_TICK);
        loop {
            std::thread::sleep(HOLDER_POLL);
            session.refresh()?;
            if session.media_loaded()? {
                spinner.finish_and_clear();
                info!("Strip loaded");
                break;
            }
        }
    }
    session.stage()?;
    Ok(())
}

/// Wait until nothing is loaded
///
/// A mount with no UNLOAD, such as the MA-21, leaves the object it already
/// had seated exactly as it was: media_loaded() reads true again at once
/// unless the operator has actually taken it out, same as the not-ready blip
/// a real eject gives, just by hand. `wait_for_film` needs that blip to have
/// happened before its own "is one loaded yet" wait means anything
fn wait_for_unload(session: &mut Session, prompt: &str) -> anyhow::Result<()> {
    session.refresh()?;
    if !session.media_loaded()? {
        return Ok(());
    }
    info!("{prompt}");
    let spinner = ProgressBar::new_spinner();
    spinner.set_message(format!("{prompt}. Ctrl-c to stop"));
    spinner.enable_steady_tick(SPINNER_TICK);
    loop {
        std::thread::sleep(HOLDER_POLL);
        session.refresh()?;
        if !session.media_loaded()? {
            spinner.finish_and_clear();
            break;
        }
    }
    Ok(())
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

/// The film format to measure frames against, from the flag or the holder
///
/// A holder that takes one format fixes it. One that takes several cannot, and
/// the frame length is not measurable from a thumbnail, so the caller has to say
fn resolve_format(flag: Option<FilmFormat>, holder_id: Option<u8>) -> anyhow::Result<FilmFormat> {
    if let Some(format) = flag {
        return Ok(format);
    }

    let id = holder_id.ok_or_else(|| anyhow!("No holder loaded; supply --format"))?;

    FilmFormat::from_holder(id)
        .or_else(|| FilmFormat::from_adapter(id))
        .ok_or_else(|| {
            let choices = FilmFormat::choices_for_holder(id)
                .map(|c| {
                    format!(
                        " (try: {})",
                        c.iter()
                            .map(cli::format_name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
                .unwrap_or_default();

            anyhow!("This holder does not fix the film format; supply --format{choices}")
        })
}
