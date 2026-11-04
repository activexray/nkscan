//! Scanning one frame: focus, meter, take the pass, and optionally clean it

use crate::{
    error::Error,
    protocol::{
        data::Rect,
        decode::Samples,
        sense::{Fault, Refusal},
    },
    scan::{
        autoexpose::Exposures,
        clean::clean_frame,
        focus::{Focus, Focused},
        framing,
        meter::Metering,
        pass::{Pass, Progress},
        window::Recipe,
    },
    session::Session,
};
use std::{ops::ControlFlow, time::Duration};
use tracing::*;

/// Long enough for a full-resolution pass over the largest frame
const SCAN_TIMEOUT: Duration = Duration::from_secs(1800);

/// Which pass `scan_frame_with`'s progress belongs to
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Metering, and which pass of it, counting from one
    Meter(usize),
    /// The scan pass itself
    Scan,
    /// The pass of infrared alone, after the colors, under [`InfraredPass::Separate`]
    Infrared,
}

/// Which pass takes the infrared plane of a scan that has one
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InfraredPass {
    /// The same pass as the colors, at the same readings
    #[default]
    WithColors,
    /// A separate pass after the colors, at one reading and with the same
    /// exposures. The lens and the film do not move between the passes
    ///
    /// Use it with more than one reading on a unit that has
    /// [`Quirks::INFRARED_EVERY_READING`](crate::protocol::model::Quirks::INFRARED_EVERY_READING).
    /// Infrared has the longest exposure, so to read it on every reading costs
    /// much more time than the colors
    Separate,
}

impl InfraredPass {
    /// Whether `recipe` takes infrared in a separate pass. This is true only
    /// with infrared and more than one reading, because with one reading the
    /// second pass saves no time
    pub fn separates(self, recipe: &Recipe) -> bool {
        self == Self::Separate && recipe.infrared && recipe.samples > 1
    }
}

/// What to do beyond taking the pass
#[derive(Debug, Clone, Copy, Default)]
pub struct Options<'a> {
    /// Reuse an exposure already decided; `None` meters this frame fresh
    pub exposures: Option<&'a Exposures>,
    /// How to meter the frame; used only if `exposures` is `None`
    pub metering: Metering,
    /// Run dust removal over the result in place
    pub clean: bool,
    /// How to focus before the pass
    pub focus: Focus,
    /// Which pass takes the infrared plane, where the recipe has one
    pub infrared: InfraredPass,
}

/// What one frame's scan produced
pub struct Scanned {
    /// The pass over the rectangle that was asked for, which is that rectangle
    /// at both ends
    pub pass: Pass,
    /// What the frame was exposed at
    pub exposures: Exposures,
    /// Pixels dust removal rebuilt, where asked for
    pub cleaned: Option<usize>,
    /// The focus result
    pub focused: Focused,
    /// The lens position during the pass. `None` if the unit does not report it
    pub focus_position: Option<u16>,
}

/// Focus, meter, and take the pass over `frame`
pub async fn scan_frame(
    session: &mut Session,
    recipe: &Recipe,
    frame: Rect,
    options: Options<'_>,
    samples: &mut Samples,
) -> Result<Scanned, Error> {
    scan_frame_with(session, recipe, frame, options, samples, |_, _| {
        ControlFlow::Continue(())
    })
    .await
}

/// The same as [`scan_frame`], with `on` told which [`Phase`] is running and able to cancel it by returning `Break`
pub async fn scan_frame_with(
    session: &mut Session,
    recipe: &Recipe,
    frame: Rect,
    options: Options<'_>,
    samples: &mut Samples,
    mut on: impl FnMut(Phase, Progress) -> ControlFlow<()>,
) -> Result<Scanned, Error> {
    let separate = options.infrared.separates(recipe);
    if options.infrared == InfraredPass::Separate && !separate {
        debug!(
            samples = recipe.samples,
            infrared = recipe.infrared,
            "a separate infrared pass saves no time here, so infrared is read with the colors"
        );
    }
    // Metering includes infrared, so the two passes use the same exposures
    let colors = Recipe {
        infrared: recipe.infrared && !separate,
        ..*recipe
    };

    let (pass, exposures, focused, focus_position) = at_frame(session, frame, async |session| {
        let (focused, focus_position) = focus_on(session, frame, options.focus).await?;

        let exposures = match options.exposures {
            Some(locked) => locked.clone(),
            None => {
                session
                    .autoexpose_frame_with(frame, recipe, &options.metering, |pass, p| {
                        on(Phase::Meter(pass), p)
                    })
                    .await?
            }
        };

        let mut windows = colors.windows(session.capabilities(), frame)?;
        exposures.apply(&mut windows);

        let pass = session
            .scan_pass_with(&windows, SCAN_TIMEOUT, samples, |p| on(Phase::Scan, p))
            .await?;
        if separate {
            let shape = (pass.rows, pass.cols);
            let ir = infrared_alone(session, recipe, frame, &exposures, shape, &mut on).await?;
            samples.ir = Some(ir);
        }
        Ok((pass, exposures, focused, focus_position))
    })
    .await?;
    samples.to_full_scale(pass.layout.sample_bits());

    let cleaned = options
        .clean
        .then(|| clean_frame(samples, &pass, session.capabilities().identity.model()))
        .transpose()?;

    Ok(Scanned {
        pass,
        exposures,
        cleaned,
        focused,
        focus_position,
    })
}

/// Scan infrared alone over `frame` for [`InfraredPass::Separate`], and return
/// its plane, which must have the color pass's `shape`
///
/// The pass reads once at `exposures`, immediately after the color pass, so
/// the lens and the film do not move. If the unit refuses a window list of
/// infrared alone, the infrared comes from a pass of all channels at one
/// reading. Any other error, a cancel included, ends the scan
async fn infrared_alone(
    session: &mut Session,
    recipe: &Recipe,
    frame: Rect,
    exposures: &Exposures,
    shape: (usize, usize),
    on: &mut impl FnMut(Phase, Progress) -> ControlFlow<()>,
) -> Result<Vec<u16>, Error> {
    let once = Recipe {
        samples: 1,
        ..*recipe
    };
    let mut alone = Samples::default();

    let mut windows = once.infrared_windows(session.capabilities(), frame)?;
    exposures.apply(&mut windows);
    let taken = session
        .scan_pass_with(&windows, SCAN_TIMEOUT, &mut alone, |p| {
            on(Phase::Infrared, p)
        })
        .await;
    let pass = match taken {
        Err(Error::Device(fault)) if refuses_the_windows(&fault) => {
            warn!(
                %fault,
                "the unit refused infrared alone, so the infrared comes from a pass of all channels at one reading"
            );
            let mut windows = once.windows(session.capabilities(), frame)?;
            exposures.apply(&mut windows);
            session
                .scan_pass_with(&windows, SCAN_TIMEOUT, &mut alone, |p| {
                    on(Phase::Infrared, p)
                })
                .await?
        }
        taken => taken?,
    };

    if (pass.rows, pass.cols) != shape {
        return Err(Error::Unsupported {
            op: "infrared pass",
            reason: format!(
                "it is {}x{} and the color pass {}x{}, so the planes cannot be paired",
                pass.rows, pass.cols, shape.0, shape.1
            ),
        });
    }
    // The decoder does not scale one reading to 16 bits as it does an average, so this pass scales its own plane
    alone.to_full_scale(pass.layout.sample_bits());
    alone.ir.ok_or_else(|| Error::Unsupported {
        op: "infrared pass",
        reason: "the pass returned no infrared plane".into(),
    })
}

/// Whether the unit refused the window list before the scan started
///
/// 2-7 calls this an invalid combination of windows. The LS-50 reports a
/// composition that it does not accept as an invalid field in the parameter
/// list. A second pass cannot correct other errors
fn refuses_the_windows(fault: &Fault) -> bool {
    matches!(
        fault,
        Fault::Rejected(Refusal::BadParamField | Refusal::BadWindowCombo, _)
    )
}

/// Meter `frame` without taking its pass
///
/// The exposures are the ones [`scan_frame_with`] meters for itself, so handing
/// them to a later scan through [`Options::exposures`] exposes that frame the
/// way this one would have been. `on` is told which metering pass is running,
/// counting from one, and can cancel it by returning `Break`
pub async fn meter_frame_with(
    session: &mut Session,
    recipe: &Recipe,
    frame: Rect,
    metering: &Metering,
    on: impl FnMut(usize, Progress) -> ControlFlow<()>,
) -> Result<Exposures, Error> {
    at_frame(session, frame, async |session| {
        session
            .autoexpose_frame_with(frame, recipe, metering, on)
            .await
    })
    .await
}

/// Focus on `frame` without metering or scanning it
///
/// Returns the focus result and the lens position. The position is `None` if
/// the unit does not report it. To scan at this focus, give the scan
/// [`Focus::Hold`]
pub async fn focus_frame(
    session: &mut Session,
    frame: Rect,
    focus: Focus,
) -> Result<(Focused, Option<u16>), Error> {
    at_frame(session, frame, async |session| {
        focus_on(session, frame, focus).await
    })
    .await
}

/// Focus on `frame`, then read the lens position. The frame must already be
/// in the unit's frame table
async fn focus_on(
    session: &mut Session,
    frame: Rect,
    focus: Focus,
) -> Result<(Focused, Option<u16>), Error> {
    let focused = session.focus_frame(frame, focus).await?;
    let position = session.focus_position().await.ok();
    if let Some(position) = position {
        info!(position, "focused at");
    }
    Ok((focused, position))
}

/// Run `f` with `frame` reachable, and leave the unit's frame table as measured
async fn at_frame<T>(
    session: &mut Session,
    frame: Rect,
    f: impl AsyncFnOnce(&mut Session) -> Result<T, Error>,
) -> Result<T, Error> {
    // Where the unit positions the film by its own frame table, the window's Y
    // selects a table entry and is an offset into that frame, not an address.
    // The rectangle must be in the table before the window can reach it
    let registered = framing::register(session, frame).await?;

    let taken = f(session).await;

    // Put the measured table back, whether or not `f` got that far, so the
    // next rectangle of this session starts from it. Written and not
    // registered: `register` starts from the measured table, finds the entry it
    // wants already there, and leaves this pass's table in the unit
    if registered
        && let Some(measured) = session.frames_type2().cloned()
        && let Err(e) = session.set_boundaries_type2_for_pass(&measured).await
    {
        warn!(
            %e,
            "could not put the measured frame table back, so a later rectangle can position against this one"
        );
    }

    taken
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocol::caps::set_window::ColorInterleaving,
        session::{
            probe::{
                capabilities,
                tests::{Replay, pages},
            },
            tests::{Unit, check, good, session},
        },
        transport::{Completion, Data},
    };
    use std::sync::{Arc, Mutex};

    fn recipe(samples: u8, infrared: bool) -> Recipe {
        Recipe {
            dpi: 4000,
            samples,
            interleaving: ColorInterleaving::LINE_WITHOUT_DISTANCE,
            infrared,
        }
    }

    /// Only a pass with more than one reading of infrared saves time
    #[test]
    fn only_infrared_at_several_readings_gets_a_pass_of_its_own() {
        assert!(InfraredPass::Separate.separates(&recipe(4, true)));
        assert!(!InfraredPass::Separate.separates(&recipe(1, true)));
        assert!(!InfraredPass::Separate.separates(&recipe(4, false)));
        assert!(!InfraredPass::WithColors.separates(&recipe(4, true)));
    }

    /// A unit that answers each SCAN with `answer`, given its window list, and
    /// fills every byte of a pass's image with the number of the SCAN that
    /// started it. The log is each SCAN's window list
    fn unit(
        answer: impl Fn(&[u8]) -> Completion + Send + 'static,
    ) -> (Session, Arc<Mutex<Vec<Vec<u8>>>>) {
        let scans = Arc::new(Mutex::new(Vec::new()));
        let log = scans.clone();
        let unit = Unit(
            move |cdb: &[u8], data: Data<'_>| match (cdb[0], cdb.get(2), data) {
                // TEST UNIT READY and SET WINDOW
                (0x00 | 0x24, ..) => good(0),
                (0x1B, _, Data::Out(ids)) => {
                    log.lock().unwrap().push(ids.to_vec());
                    answer(ids)
                }
                (0x28, Some(0x00), Data::In(buf)) => {
                    buf.fill(log.lock().unwrap().len() as u8);
                    good(buf.len())
                }
                // Nothing else is needed, so the unit does not have it
                _ => check(0x05, 0x20, 0x00),
            },
        );
        let ls50 = Replay(pages("ls50"));
        let caps = crate::session::block_on(capabilities(&mut { ls50 })).expect("LS-50 pages");
        (session(unit, caps), scans)
    }

    /// Two readings with infrared, a pass of its own for it, a 16 by 16 frame
    fn scan_separately(
        session: &mut Session,
        on: impl FnMut(Phase, Progress) -> ControlFlow<()>,
    ) -> (Result<Scanned, Error>, Samples) {
        let frame = Rect {
            top: 0,
            left: 0,
            bottom: 16,
            right: 16,
        };
        let exposures = Exposures::default();
        let options = Options {
            exposures: Some(&exposures),
            focus: Focus::Hold,
            infrared: InfraredPass::Separate,
            ..Options::default()
        };
        let mut samples = Samples::default();
        let scanned = crate::session::block_on(scan_frame_with(
            session,
            &recipe(2, true),
            frame,
            options,
            &mut samples,
            on,
        ));
        (scanned, samples)
    }

    fn go(_: Phase, _: Progress) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    /// Every sample of `plane` came from pass `n`, stretched from `bits` as a scan stretches it
    fn from_pass(plane: &[u16], n: u8, bits: u8) -> bool {
        let mut sent = Samples {
            colors: vec![vec![u16::from_be_bytes([n, n])]],
            ir: None,
        };
        sent.to_full_scale(bits);
        !plane.is_empty() && plane.iter().all(|&v| v == sent.colors[0][0])
    }

    #[test]
    fn infrared_alone_follows_the_colors() {
        let (mut session, scans) = unit(|_| good(0));
        let (scanned, samples) = scan_separately(&mut session, go);
        let bits = scanned.expect("the scan").pass.layout.bits_per_sample;

        assert_eq!(*scans.lock().unwrap(), [vec![1, 2, 3], vec![9]]);
        assert!(samples.colors.iter().all(|plane| from_pass(plane, 1, bits)));
        assert!(from_pass(samples.ir.as_deref().expect("infrared"), 2, bits));
    }

    /// The LS-50 answers a one-window list it will not take with 05h-26h-02h
    #[test]
    fn a_refused_infrared_pass_is_taken_with_the_colors_at_one_reading() {
        let (mut session, scans) = unit(|ids| match ids {
            [9] => check(0x05, 0x26, 0x02),
            _ => good(0),
        });
        let (scanned, samples) = scan_separately(&mut session, go);
        let bits = scanned.expect("the scan").pass.layout.bits_per_sample;

        assert_eq!(
            *scans.lock().unwrap(),
            [vec![1, 2, 3], vec![9], vec![9, 1, 2, 3]]
        );
        // The colors are the multi-sampled pass's, not the one-reading one's
        assert!(samples.colors.iter().all(|plane| from_pass(plane, 1, bits)));
        assert!(from_pass(samples.ir.as_deref().expect("infrared"), 3, bits));
    }

    #[test]
    fn a_cancelled_infrared_pass_ends_the_scan() {
        let (mut session, scans) = unit(|_| good(0));
        let (scanned, _) = scan_separately(&mut session, |phase, _| match phase {
            Phase::Infrared => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        });

        assert!(matches!(scanned, Err(Error::Cancelled)));
        assert_eq!(*scans.lock().unwrap(), [vec![1, 2, 3], vec![9]]);
    }

    /// A failure is not a refusal of the window list, so no second pass
    #[test]
    fn a_failed_infrared_pass_ends_the_scan() {
        let (mut session, scans) = unit(|ids| match ids {
            [9] => check(0x04, 0x00, 0x00),
            _ => good(0),
        });
        let (scanned, _) = scan_separately(&mut session, go);

        assert!(matches!(scanned, Err(Error::Device(_))));
        assert_eq!(*scans.lock().unwrap(), [vec![1, 2, 3], vec![9]]);
    }
}
