//! An open, exclusive hold of a scanner
//!
//! Ties [`transport`] to [`protocol`](crate::protocol): holds
//! the state that outlives a single command, wraps each CDB in the invariants
//! its section imposes, and absorbs the retry and polling semantics. Deciding
//! what a scan should do belongs above this.

pub mod autoexpose;
pub mod data;
pub mod focus;
pub mod image;
pub mod probe;
pub mod scan;
pub mod window;

use crate::{
    error::Error,
    protocol::{
        caps::{Capabilities, frames::Frames},
        cdbs::{
            Abort, ModeSelect, ModeSense, PageControl, ReleaseUnit, ReserveUnit, TestUnitReady,
        },
        curves::Curves,
        data::{CooperativeAction, FrameTable, Op, Operation},
        mode,
        model::Model,
        sense::{Activity, Change, Coop, Fault, Intervention, Outcome, Refusal, interpret},
    },
    transport::{self, Completion, Data, Transport},
};
use std::sync::Arc;
use std::{
    io,
    thread::sleep,
    time::{Duration, Instant},
};
use tracing::*;

pub struct Session {
    caps: Capabilities,
    transport: Box<dyn Transport>,
    /// What a step of a window coordinate is currently worth
    divisor: u16,
    /// The frame table that windowing uses
    frames: Option<FrameTable>,
    frame_type_2: bool,
    /// CCD row response curves, read once in the preamble
    curves: Option<Arc<Curves>>,
    /// Whether we hold the unit, so [`Drop`] only releases what it took
    reserved: bool,
}

pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait before asking a busy unit again
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A unit that keeps asking is not going to start, so cap the re-issues
///
/// It asks for one job at a time and walks a list of them: a 666 dpi multi-line
/// pass with three readings and infrared asked five times, one each for
/// averaging, registration and the CCD data, with two the sense calls 5. The cap
/// is only there to bound a unit that will not move on, which is what asking for
/// the same thing twice means
const MAX_COOPERATION: usize = 16;

/// Long enough for a full-length stage move
pub(crate) const MOVE_TIMEOUT: Duration = Duration::from_secs(180);

/// What one chunk of a pass already in flight gets
///
/// Reading the remainder off a cancelled pass has none of a first read's
/// waiting to do - the stage is already where it needs to be and the data is
/// either coming or it is not. Sized off the slowest pass observed, a thumbnail
/// at around 50 KiB/s, which puts a 128 KiB chunk under three seconds
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_secs(20);

/// Long enough for a cold unit to warm its lamp and initialize
///
/// Nothing advertised bounds this: `Address` bytes 80,81 are the lamp warm-up
/// maximum, and both specs give them as 0
const READY_TIMEOUT: Duration = Duration::from_secs(180);

/// A device raising unit attentions forever would spin on refresh, so cap those.
/// Polling needs no cap: it sleeps, and the deadline already bounds it
const MAX_CHANGES: usize = 16;

/// A reply we could not make sense of
pub(crate) fn malformed(what: String) -> Error {
    Error::Transport(io::Error::new(io::ErrorKind::InvalidData, what).into())
}

impl Session {
    /// Start a new scanning session
    ///
    /// Pins the measurement unit divisor to the unit's maximum resolution, so
    /// every window coordinate is one pixel and agrees with the addresses and
    /// boundaries `Address` reports.
    pub fn open(mut transport: Box<dyn Transport>) -> Result<Self, Error> {
        let caps = probe::capabilities(transport.as_mut())?;
        let divisor = caps.address.x_axis.dpi_range.last;
        let frame_type_2 = !matches!(caps.identity.model(), Some(Model::Ls8000 | Model::Ls9000));
        let mut session = Self {
            transport,
            caps,
            divisor,
            reserved: false,
            frames: None,
            frame_type_2,
            curves: None,
        };
        // INQUIRY answers while the unit is still initializing, so probing says
        // nothing about readiness. Everything below is a real command, and a
        // cold unit would spend the whole of the first one's budget not ready.
        //
        // An empty unit answers this with no medium, which is a state to open
        // in rather than a failure: the caller is the one who decides whether
        // to wait for something to be loaded
        match session.test_unit_ready(READY_TIMEOUT) {
            Ok(()) => {}
            Err(Error::Media(Intervention::NoMedium)) => debug!("nothing is loaded"),
            // A scan the last process holding the unit never read to the end
            // stays valid and refuses everything, TEST UNIT READY included,
            // until it is stopped. `abort()` is 2-13's unconditional stop, the
            // same recovery `stop_stale_scan` below reaches for at this same
            // sense code, just against a command that can trip over it first
            Err(Error::Device(fault))
                if matches!(*fault, Fault::Rejected(Refusal::OutOfSequence, _)) =>
            {
                debug!("a scan was still valid from the last process, stopping it");
                session.abort()?;
                session.test_unit_ready(READY_TIMEOUT)?;
            }
            Err(e) => return Err(e),
        }
        session.reserved = session.reserve()?;
        session.stop_stale_scan()?;
        session.set_units(divisor)?;

        // The rest of the preamble drives the mechanism, so it waits until
        // there is something loaded to drive it against
        session.fetch_curves();
        match session.media_loaded()? {
            true => session.stage()?,
            false => debug!("nothing is loaded, so the mechanism is left alone"),
        }

        Ok(session)
    }

    /// Whether a holder is loaded
    ///
    /// The frame page is the answer where the unit publishes one: it lists what
    /// is in the holder, and an empty one is an empty unit. Where it does not,
    /// the mechanism is asked instead, and answers `02h-3Ah-00h` with nothing
    /// in it.
    ///
    /// Read afresh either way rather than off what was cached at open, so this
    /// can be asked in a loop while somebody loads one. Nothing moves, which is
    /// the point: a scan drives the stage against film and has to know before
    /// it starts.
    pub fn media_loaded(&mut self) -> Result<bool, Error> {
        if self.caps.frames.is_some() {
            let page = probe::vpd(self.transport.as_mut(), Frames::PAGE_CODE)?;
            let frames = Frames::try_from(&page)?;
            let loaded = !frames.images.is_empty();
            self.caps.frames = Some(frames);
            return Ok(loaded);
        }
        // A holder going in is a mechanism move, and this is what a caller polls
        // while somebody feeds one - so the budget has to cover the draw-in, not
        // just an answer from an idle unit. `run` returns the moment the unit
        // stops reporting itself busy, so this is a ceiling rather than a wait
        match self.test_unit_ready(MOVE_TIMEOUT) {
            Ok(()) => Ok(true),
            Err(Error::Media(Intervention::NoMedium)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Put the unit in the state a scan expects
    ///
    /// Re-establishes the window descriptors, which is what sets the infrared
    /// window's control byte, and parks the focus where it already is. Both move
    /// the mechanism, so this is not for an empty unit: [`open`](Self::open)
    /// runs it only when something is loaded, and a caller that waited for a
    /// holder runs it once one is.
    pub fn stage(&mut self) -> Result<(), Error> {
        let held = self.windows()?;
        let y_boundary = self.caps.address.y_axis.boundary;
        for w in &held {
            // The power-on descriptors describe the hardware maximum, not what
            // the adapter opening allows, so a Y size past the boundary is a
            // stale descriptor that can't be set. Skip it rather than warn
            if w.size.1 > y_boundary {
                debug!(id = w.id, size = w.size.1, "skipping stale power-on window");
                continue;
            }
            if let Err(e) = self.set_window(w) {
                debug!(id = w.id, %e, "this window would not go back");
            }
        }
        if let Ok(params) = self.get_parameter(Op::FocusMove) {
            let at = params.first.min(u32::from(u16::MAX)) as u16;
            match self.focus_to(at) {
                Ok(()) => debug!(at, "staged the focus"),
                Err(e) => debug!(at, %e, "could not stage the focus"),
            }
        }
        Ok(())
    }

    /// Run a command that some units will not have, answering whether it ran
    ///
    /// `Err` stays `Err` for every refusal but the one named
    fn tolerate(&mut self, cdb: &[u8], timeout: Duration, allowed: Refusal) -> Result<bool, Error> {
        match self.run(cdb, Data::None, timeout) {
            Ok(_) => Ok(true),
            Err(Error::Device(fault)) if matches!(*fault, Fault::Rejected(refusal, _) if refusal == allowed) => {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Take the unit, so no other initiator can interleave with us
    ///
    /// Only SBP-2 has more than one initiator, but the 5000 documents the
    /// command too, in 2-4 and not in its own command list, so a unit that has
    /// never heard of it is not an error. Answers whether we got it
    fn reserve(&mut self) -> Result<bool, Error> {
        let held = self.tolerate(&ReserveUnit.cdb(), PROBE_TIMEOUT, Refusal::UnknownOpcode)?;
        if !held {
            debug!("this unit has no RESERVE UNIT");
        }
        Ok(held)
    }

    /// Stop a scan the last program to hold the unit left valid
    ///
    /// While one is, every non-basic command is refused as out of sequence, so
    /// issuing one is how to tell
    fn stop_stale_scan(&mut self) -> Result<(), Error> {
        match self.windows() {
            Ok(_) => Ok(()),
            Err(Error::Device(fault))
                if matches!(*fault, Fault::Rejected(Refusal::OutOfSequence, _)) =>
            {
                debug!("a scan was still valid from earlier, stopping it");
                self.abort().map(drop)
            }
            Err(e) => Err(e),
        }
    }

    /// Stop any scan in progress, answering whether the unit has the command
    ///
    /// 2-13: the scan block stops where it is, and a scan has to be issued again
    /// to read anything. GOOD comes back even when nothing was running, so this
    /// is also how to get to a known state.
    ///
    /// Safe to issue partway through a readout, at a command boundary. A
    /// Coolscan V capture of NikonScan's own Stop button does exactly that -
    /// last READ's status read, then `C0h`, GOOD 1.6 ms later - so the wait
    /// below is a ceiling rather than something a readout abort spends
    pub fn abort(&mut self) -> Result<bool, Error> {
        if !self.tolerate(&Abort.cdb(), PROBE_TIMEOUT, Refusal::UnknownOpcode)? {
            debug!("this unit has no ABORT");
            return Ok(false);
        }
        // An operation activation command, so it answers before it acts
        self.test_unit_ready(MOVE_TIMEOUT)?;
        Ok(true)
    }

    /// Give back whatever is loaded, answering whether the unit did anything
    ///
    /// 2-15-3 `Unload`, which takes no parameter. The captures send an
    /// uninitialized one, so this sends zeros rather than copying that.
    ///
    /// An operation activation command, so the unit answers before the
    /// mechanism has finished and [`execute`](Self::execute) waits it out.
    /// An adapter with nothing to eject, such as a single-slide mount, does
    /// not offer the operation at all: leaving it loaded is not a failure
    pub fn eject(&mut self) -> Result<bool, Error> {
        if !self.caps.features.execute.supports(Op::Unload) {
            debug!("this unit has no UNLOAD");
            return Ok(false);
        }
        match self.execute(Op::Unload, Operation::default(), MOVE_TIMEOUT) {
            Ok(()) => Ok(true),
            // `execute` confirms termination with TEST UNIT READY, and once
            // UNLOAD has actually emptied the gate that reports medium not
            // present. That is the operation succeeding, not it failing -
            // the same reasoning `load` already applies to its own empty
            // answer below
            Err(Error::Media(Intervention::NoMedium)) => Ok(true),
            Err(e) => Err(e),
        }
    }

    /// Take in whatever the adapter has waiting, answering whether anything came
    ///
    /// 2-15-3 `Load`, the mirror of [`eject`](Self::eject) and gated the same
    /// way. A feeder or a cartridge keeps its film behind the gate, so there is
    /// nothing to scan until the unit is told to take some in; an adapter the
    /// operator fills by hand does not offer the operation at all.
    ///
    /// `Ok(false)` covers both nothing to do and nothing left: neither is a
    /// failure, and either way the caller has to ask the operator
    pub fn load(&mut self) -> Result<bool, Error> {
        if !self.caps.features.execute.supports(Op::Load) {
            debug!("this unit has no LOAD");
            return Ok(false);
        }
        match self.execute(Op::Load, Operation::default(), MOVE_TIMEOUT) {
            Ok(_) => Ok(true),
            Err(Error::Media(Intervention::NothingToLoad)) => {
                debug!("the adapter has nothing left to take in");
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// What the scanner says it can do
    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// Whether we use Framing Type2 or not
    pub fn uses_frame_type_2(&self) -> bool {
        self.frame_type_2
    }

    /// Re-read what the scanner says it can do
    ///
    /// Needed after anything that changes the adapter or holder, since several
    /// fields track those rather than the model.
    ///
    /// The CCD curves are not among them: the page describing them and the rows
    /// they cover are the sensor's, so they outlive an adapter change and are
    /// read once at open rather than again here. Dropping them left every pass
    /// after the first refresh decoding uncorrected
    pub fn refresh(&mut self) -> Result<(), Error> {
        self.caps = probe::capabilities(self.transport.as_mut())?;
        Ok(())
    }

    /// Simple readiness check
    pub fn test_unit_ready(&mut self, timeout: Duration) -> Result<(), Error> {
        self.run(&TestUnitReady.cdb(), Data::None, timeout)?;
        Ok(())
    }

    /// One mode page, header and block descriptor included
    pub fn mode_sense(&mut self, page: u8, control: PageControl) -> Result<Vec<u8>, Error> {
        let cmd = ModeSense::new(page, control);
        let mut buf = vec![0u8; cmd.allocation_length()];
        let completion = self.run(&cmd.cdb(), Data::In(&mut buf), PROBE_TIMEOUT)?;
        buf.truncate(completion.transferred);
        Ok(buf)
    }

    /// Read the divisor back off the unit
    ///
    /// It outlives a session: it holds until the next MODE SELECT, a reset or a
    /// power cycle
    pub fn units(&mut self) -> Result<u16, Error> {
        let reply = self.mode_sense(mode::MEASUREMENT_UNITS, PageControl::Current)?;
        mode::divisor(&reply)
            .ok_or_else(|| malformed(format!("no measurement units page in {reply:02x?}")))
    }

    /// Count window coordinates in steps of an inch divided by `divisor`
    ///
    /// 2-3-4 note 5 takes only 1200 or the unit's own maximum resolution and
    /// answers anything else with common error 2
    pub fn set_units(&mut self, divisor: u16) -> Result<(), Error> {
        let max = self.caps.address.x_axis.dpi_range.last;
        if divisor != 1200 && divisor != max {
            return Err(Error::Unsupported {
                op: "measurement units",
                reason: format!("the divisor must be 1200 or {max}, not {divisor}"),
            });
        }

        let list = mode::set_divisor(divisor);
        if enabled!(Level::TRACE) {
            let hex: Vec<String> = list.iter().map(|b| format!("{b:02X}")).collect();
            trace!(divisor, bytes = hex.join(" "), "mode select");
        }
        let cmd = ModeSelect::new(list.len() as u8);
        self.run(&cmd.cdb(), Data::Out(&list), PROBE_TIMEOUT)?;
        self.divisor = divisor;
        Ok(())
    }

    /// Issue a command, absorbing everything that means "not done yet", and
    /// hand back the completion once it has actually terminated
    ///
    /// A cooperative request is not a blocker: the `DataType::Cooperation`
    /// record is read and the command issued again, so this returns when it
    /// has run. `timeout` budgets the whole command including re-issues, not
    /// one transfer
    pub fn run(
        &mut self,
        cdb: &[u8],
        data: Data<'_>,
        timeout: Duration,
    ) -> Result<Completion, Error> {
        let (completion, _) = self.run_handshake(cdb, data, timeout)?;
        Ok(completion)
    }

    /// As [`run`](Self::run), but hands back the cooperation record the unit
    /// asked for
    ///
    /// Only SCAN and READ raise one. 2-7: read the parameter with
    /// `DataType::Cooperation`, do the
    /// work, and issue the command again. SCAN uses this so its caller can
    /// honor whatever the unit asks for once the data is in hand
    pub(crate) fn run_handshake(
        &mut self,
        cdb: &[u8],
        mut data: Data<'_>,
        timeout: Duration,
    ) -> Result<(Completion, Vec<CooperativeAction>), Error> {
        // One budget for the command, re-issues included, rather than one each:
        // a fresh timeout per round would let 16 cooperative requests run for 16
        // times what the caller allowed
        let deadline = Instant::now() + timeout;
        let mut cooperations = Vec::new();
        let mut asked: Vec<(Coop, CooperativeAction)> = Vec::new();
        for _ in 0..=MAX_COOPERATION {
            // `Data::In` holds a `&mut [u8]` and so is not `Copy`. Reborrowing
            // it here is what lets the same command go out more than once
            let payload = match &mut data {
                Data::None => Data::None,
                Data::In(buf) => Data::In(buf),
                Data::Out(buf) => Data::Out(buf),
            };
            let (completion, coop) = self.run_cooperative(cdb, payload, deadline, timeout)?;
            let Some(coop) = coop else {
                return Ok((completion, cooperations));
            };

            // Dispatch on the record rather than the sense: the two specs give
            // the same job different 4th sense bytes.
            //
            // This read carries its own `PROBE_TIMEOUT` rather than the
            // command's deadline: it is a short reply the unit already has in
            // hand, and it is bounded by the cap on the loop
            let record = self.cooperation()?;
            debug!(?coop, ?record, "the unit wants something doing");

            // Each job is asked for once, so the same one coming back means it
            // is not satisfied by reading the record and re-issuing, and asking
            // again would spin until the cap
            if asked.contains(&(coop, record.clone())) {
                return Err(Error::Unsupported {
                    op: "host cooperation",
                    reason: format!("the unit asked for {coop:?} twice over"),
                });
            }
            asked.push((coop, record.clone()));
            cooperations.push(record);
        }

        Err(Error::Unsupported {
            op: "host cooperation",
            reason: format!(
                "the unit asked for {} things and was still going: {asked:?}",
                asked.len()
            ),
        })
    }

    /// One call at a command, absorbing the polling and state-change retries
    /// that need no host action
    ///
    /// A cooperative request comes back on the `Option` rather than as an
    /// error, for [`run_handshake`](Self::run_handshake) to service
    ///
    /// `deadline` bounds this call and `budget` is only what a timeout reports,
    /// since the deadline is the caller's whole command and the remainder of it
    /// would be a misleading thing to name
    fn run_cooperative(
        &mut self,
        cdb: &[u8],
        mut data: Data<'_>,
        deadline: Instant,
        budget: Duration,
    ) -> Result<(Completion, Option<Coop>), Error> {
        let mut changes = 0usize;
        // Polling repeats the same outcome, so only log the transitions
        let mut reported: Option<Activity> = None;

        loop {
            // `Data::In` holds a `&mut [u8]` and so is not `Copy`. Reborrowing
            // it here is what lets the same command go out more than once
            let payload = match &mut data {
                Data::None => Data::None,
                Data::In(buf) => Data::In(buf),
                Data::Out(buf) => Data::Out(buf),
            };

            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Transport(transport::Error::Timeout(budget)));
            }

            let completion = self.transport.execute(cdb, payload, left)?;

            match interpret(&completion) {
                // Done, either quietly or having done something other than what
                // we asked. GET WINDOW is what reports the difference
                Outcome::Complete => return Ok((completion, None)),
                Outcome::CompleteWith(adjustment) => {
                    // The sense-key specific bytes carry a field pointer, which
                    // is the only thing that says what got adjusted
                    info!(
                        ?adjustment,
                        opcode = cdb.first(),
                        sense = ?completion.sense,
                        "the scanner had a note about that"
                    );
                    return Ok((completion, None));
                }

                // The scanner wants post-processing before it will go on
                Outcome::NeedsHost(coop) => return Ok((completion, Some(coop))),

                // Not yet. Polling is re-issuing
                Outcome::Working(activity) => {
                    if reported.replace(activity) != Some(activity) {
                        debug!(?activity, "waiting");
                    }
                    sleep(POLL_INTERVAL);
                }

                // Unit attention: the command did not run. Several queue up --
                // ejecting raises an adapter change and a reset. Capabilities
                // track the adapter, so what we cached may now be stale
                Outcome::StateChanged(change) => {
                    debug!(?change, "device state changed under us, re-issuing");
                    self.refresh()?;
                    changes += 1;
                    if changes >= MAX_CHANGES {
                        return Err(unsettled(change, changes));
                    }
                }

                terminal => return Err(Error::from_outcome(terminal, &completion)),
            }
        }
    }
}

/// A unit that keeps raising attentions is not faulted, it is unusable
fn unsettled(change: Change, changes: usize) -> Error {
    warn!(
        ?change,
        changes, "giving up on a device that will not settle"
    );
    Error::Unsupported {
        op: "command",
        reason: format!(
            "the unit raised {changes} unit attentions without running it, last {change:?}"
        ),
    }
}

/// A reservation only clears on RELEASE, a reset or a power cycle, so one we
/// drop on the floor locks the unit out of every other program until then
impl Drop for Session {
    fn drop(&mut self) {
        if !self.reserved {
            return;
        }
        if let Err(e) = self.run(&ReleaseUnit.cdb(), Data::None, PROBE_TIMEOUT) {
            warn!(%e, "could not release the scanner");
        }
    }
}
