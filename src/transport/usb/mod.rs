//! SCSI-over-USB transport implemented with nusb, with the LS5K 1-1-2 phase handshake written once against `Link`

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod webusb;

use super::{BoxFuture, Completion, Data, Error, MaybeSend, Sense, Status, Transport};
use crate::time::{Instant, sleep};
use nusb::{
    DeviceInfo, Endpoint, Interface,
    transfer::{Bulk, BulkOrInterrupt, EndpointDirection, In, Out, TransferError},
};
use std::{future::Future, io, time::Duration};
use tracing::*;

// Single-byte transport opcodes on bulk-OUT (LS5K 1-1-2)
/// Tell the unit to prepare a phase response
const PHASE_CHECK_CODE: u8 = 0xD0;

// Phase codes (LS5K table 1-1-2-1)
const PHASE_NONE: u8 = 0x00;
const PHASE_STATUS: u8 = 0x01;
const PHASE_DATA_OUT: u8 = 0x02;
const PHASE_DATA_IN: u8 = 0x03;
const PHASE_BUSY: u8 = 0x04;

/// The first wait before asking a busy unit again, and the longest that wait
/// becomes
///
/// A unit that answers one BUSY was asked early. One that keeps answering BUSY
/// is warming up, which takes it tens of seconds from cold, so the wait grows
/// rather than asking it hundreds of times a second
const BUSY_WAIT: Duration = Duration::from_millis(5);
const BUSY_WAIT_MAX: Duration = Duration::from_millis(250);

/// How long one phase check waits
///
/// Nikon Scan cancels that read at 7 s and sends the command again
const PHASE_WAIT: Duration = Duration::from_secs(7);

/// How long a check after a BUSY waits
///
/// The unit has the command by then and is working on it, so this is how long
/// its answer is worth waiting for before we treat it as gone. An LS-40
/// answers BUSY to the first check of every command and the real phase to the
/// next, tens of milliseconds later
const RECHECK_WAIT: Duration = Duration::from_secs(1);

/// How long a resync read waits before calling the pipe empty
const RESYNC_TIMEOUT: Duration = Duration::from_millis(200);

/// How much a resync drops before giving up on getting back in step. A unit
/// still streaming an image has more than this to say, and dropping all of it
/// would take longer than the command that is waiting
const RESYNC_LIMIT: usize = 1 << 20;

/// What is left of the time a command was given
///
/// Every step of the phase handshake reads its wait from this, so the timeout
/// the caller set covers the whole command and not each of its parts
#[derive(Clone, Copy)]
struct Budget {
    deadline: Instant,
    total: Duration,
}

impl Budget {
    fn new(total: Duration) -> Self {
        Self {
            deadline: Instant::now() + total,
            total,
        }
    }

    /// How long is left, or the timeout error naming what the caller asked for
    fn left(&self) -> Result<Duration, Error> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(Error::Timeout(self.total))
        } else {
            Ok(left)
        }
    }
}

/// The two bulk pipes. A timeout is [`Error::Timeout`], and the link decides what happens to that transfer
trait Link {
    /// The bulk IN endpoint's packet size: 512 for USB 2.0, 64 for USB 1.1 (LS-40)
    fn in_max_packet(&self) -> usize;

    /// Write `bytes` to the bulk OUT endpoint
    fn write_out(
        &mut self,
        bytes: &[u8],
        timeout: Duration,
    ) -> impl Future<Output = Result<(), Error>> + MaybeSend;

    /// Read at most `len` bytes, a whole number of packets, from bulk IN. A short packet ends the read
    fn transfer_in(
        &mut self,
        len: usize,
        timeout: Duration,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + MaybeSend;
}

/// The scanner's interface 0 and its two bulk endpoints
struct Pipes {
    ep_out: Endpoint<Bulk, Out>,
    ep_in: Endpoint<Bulk, In>,
    in_max_packet: usize,
    /// The last write submitted, which is still queued if a write timed out
    #[cfg(target_arch = "wasm32")]
    late_out: Vec<u8>,
    /// We have to keep a handle to the interface here as the lifetime of the endpoints are attached to it
    #[allow(dead_code)]
    interface: Interface,
}

/// A USB-connected Nikon scanner
pub struct UsbTransport {
    phases: Phases<Pipes>,
}

/// The phase handshake over a [`Link`]
struct Phases<L> {
    link: L,
    /// Whether the last command left the phase handshake unfinished
    ///
    /// The unit answers a command in phases and this drives both ends of that,
    /// so a command that returns partway through leaves the unit mid-answer
    /// with the rest of it queued on the IN pipe. Nothing about the next
    /// command tells the unit to start over, so its phase byte is whatever was
    /// left over and every command after it is misframed
    dirty: bool,
}

/// Map a `nusb` transfer error into this crate's SCSI error
fn transfer_err(e: TransferError, timeout: Duration) -> Error {
    let kind = match &e {
        // nusb cancels the transfer itself when the timeout runs out, and we
        // cancel none of our own, so a cancelled one is always that timeout
        TransferError::Cancelled => return Error::Timeout(timeout),
        // Endpoint halted (broken pipe)
        TransferError::Stall => io::ErrorKind::BrokenPipe,
        TransferError::Disconnected => io::ErrorKind::NotConnected,
        // We handed the OS something it would not take: our bug, not the device's
        TransferError::InvalidArgument => io::ErrorKind::InvalidInput,
        TransferError::Fault | TransferError::Unknown(_) => io::ErrorKind::Other,
    };
    Error::Io(io::Error::new(kind, e))
}

/// Wait for a nusb call. Natively awaiting it panics without a runtime, so it blocks, and in a browser it is awaited
pub(crate) async fn settle<F: nusb::MaybeFuture>(call: F) -> F::Output {
    #[cfg(not(target_arch = "wasm32"))]
    return call.wait();
    #[cfg(target_arch = "wasm32")]
    call.await
}

/// Clear an endpoint the unit halted
///
/// A stall stands until CLEAR_FEATURE, which 1-1-6-1-1 has this unit
/// supporting. Leaving it set would fail every transfer after the one that
/// stalled, so the command that hit it is the only one that has to be lost
async fn clear_stall<T: BulkOrInterrupt, D: EndpointDirection>(
    ep: &mut Endpoint<T, D>,
    e: TransferError,
) {
    if e != TransferError::Stall {
        return;
    }
    match settle(ep.clear_halt()).await {
        Ok(()) => warn!("the unit halted the endpoint, cleared it for the next command"),
        Err(e) => warn!(%e, "the unit halted the endpoint and it would not clear"),
    }
}

/// What one read off the IN pipe gave us
enum Chunk {
    /// Bytes copied into the caller's buffer
    Got(usize),
    /// The unit answered with this many bytes for a read that asked for fewer.
    /// The buffer took what it holds and the surplus is off the wire with
    /// nowhere to put it
    TooMuch(usize),
}

/// What a phase check answered
enum Phase {
    /// The unit's phase code
    Code(u8),
    /// The unit did not take the question, or did not answer it in time
    Silent,
    /// The answer was not the one byte 1-1-2-1 defines, so the pipe is out of
    /// step: it gave nothing at all, or an answer we did not ask for
    Stale(usize),
}

/// What one try at a command gave us
enum Attempt {
    /// The unit answered the command
    Done(Completion),
    /// The command is spent and can go out again. `drain` says the unit went
    /// quiet rather than answering, so it may still answer late
    Resend { drain: bool },
    /// The IN pipe holds something we did not put there, so the command goes
    /// out again after a drain. The error says what we read, for the case
    /// where the drain does not fix it
    Desync(Error),
}

impl UsbTransport {
    pub async fn open(info: DeviceInfo) -> io::Result<Self> {
        let device = settle(info.open()).await?;
        // Unlike other platforms, macOS only sets a configuration itself for
        // composite-class devices, so a vendor-class scanner opens unconfigured
        // there. Windows and Linux always report an active configuration, which
        // is what keeps `set_configuration` (unsupported on Windows) out of
        // reach of the platforms that cannot take it
        if device.active_configuration().is_err() {
            settle(device.set_configuration(1))
                .await
                .map_err(io::Error::other)?;
        }
        // From the spec, the interface is 0
        let interface = settle(device.claim_interface(0)).await?;
        // Endpoint addresses from LS5K spec table 1-1-6-2-4
        let ep_out = interface.endpoint::<Bulk, Out>(0x01)?;
        let ep_in = interface.endpoint::<Bulk, In>(0x82)?;
        // 512 for USB 2.0, 64 for USB 1.1 (LS-40)
        let in_max_packet = ep_in.max_packet_size();
        // Protect against zero packet sizes (this would be weird)
        if in_max_packet == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bulk IN endpoint reports zero max packet size",
            ));
        }
        debug!(?device, "Opened scanner");
        let link = Pipes {
            ep_out,
            ep_in,
            in_max_packet,
            #[cfg(target_arch = "wasm32")]
            late_out: Vec::new(),
            interface,
        };
        Ok(Self {
            phases: Phases::new(link),
        })
    }
}

impl<L: Link> Phases<L> {
    fn new(link: L) -> Self {
        Self {
            link,
            // A program that died mid-command left its answer on the pipe, and
            // a fresh handle cannot tell. We do not drain to find out: the read
            // that says the pipe is empty is a read that times out, and that
            // costs every open 200 ms on Linux and over two seconds on
            // Windows. The first command reads the leftovers as its phase
            // byte, and that is a desync, which drains and sends the command
            // again
            dirty: false,
        }
    }

    /// Read off whatever the last command left in the pipe
    ///
    /// Being one command out of step is not something a caller can act on: the
    /// next phase byte is a stale data byte, the one after that is worse, and
    /// the way out is a power cycle. Draining until the pipe comes back empty
    /// puts the two ends back in step, which is verified against an LS-50 that
    /// had been desynced on purpose: eight stale bytes drained and the unit
    /// answered INQUIRY again without a reset
    async fn resync(&mut self) {
        let mut dropped = 0usize;
        while dropped < RESYNC_LIMIT {
            let packet = self.link.in_max_packet();
            match self.transfer_in(packet, RESYNC_TIMEOUT).await {
                Ok(b) if !b.is_empty() => dropped += b.len(),
                // Empty or refused: the unit has nothing more to say
                _ => {
                    if dropped > 0 {
                        warn!(
                            bytes = dropped,
                            "the last command left its answer in the pipe, dropped it to get back in step"
                        );
                    }
                    self.dirty = false;
                    return;
                }
            }
        }
        // The flag stays set, so the next command drains again
        warn!(
            bytes = dropped,
            "the unit has more to say than we will drop, so we are still out of step"
        );
    }

    /// One read off the Bulk In endpoint, of enough whole packets to hold
    /// `want` bytes
    ///
    /// nusb takes the request in whole packets, so a `want` that does not land
    /// on a packet boundary leaves room for the unit to answer with up to a
    /// packet more than that. What the surplus means is the caller's to say
    async fn transfer_in(&mut self, want: usize, timeout: Duration) -> Result<Vec<u8>, Error> {
        let packet = self.link.in_max_packet();
        let req = want.max(1).div_ceil(packet) * packet;
        self.link.transfer_in(req, timeout).await
    }

    async fn write_out(&mut self, bytes: &[u8], timeout: Duration) -> Result<(), Error> {
        self.link.write_out(bytes, timeout).await
    }

    /// Read from the Bulk In endpoint into `out`, saying what arrived
    ///
    /// `out` takes as much as it holds either way, so a [`Chunk::TooMuch`] is
    /// the caller's to judge rather than a read it has to do again
    async fn read_in(&mut self, out: &mut [u8], timeout: Duration) -> Result<Chunk, Error> {
        let buf = self.transfer_in(out.len(), timeout).await?;
        let n = buf.len().min(out.len());
        out[..n].copy_from_slice(&buf[..n]);
        match buf.len() > out.len() {
            true => Ok(Chunk::TooMuch(buf.len())),
            false => Ok(Chunk::Got(n)),
        }
    }

    /// Read the next piece of a phase into `out`, which is all that the phase still has
    ///
    /// A [`Chunk::TooMuch`] is then the padding of the last packet
    async fn read_last(&mut self, out: &mut [u8], timeout: Duration) -> Result<usize, Error> {
        match self.read_in(out, timeout).await? {
            Chunk::Got(n) => Ok(n),
            Chunk::TooMuch(n) => {
                trace!(
                    padding = n - out.len(),
                    "dropped the tail of the last packet"
                );
                Ok(out.len())
            }
        }
    }

    /// Read a whole data IN phase
    ///
    /// Each read asks for all that is left, because a READ can stop in a line
    /// and the next one then starts with a short piece
    async fn read_data_in(&mut self, out: &mut [u8], budget: Budget) -> Result<usize, Error> {
        let mut done = 0;
        while done < out.len() {
            match self.read_last(&mut out[done..], budget.left()?).await? {
                0 => break,
                n => done += n,
            }
        }
        Ok(done)
    }

    /// Ask what the unit wants next, in no more than `bound` and no more than
    /// the command has left
    ///
    /// Both halves read the budget, so a check that spends its whole allowance
    /// asking cannot then spend it again listening
    async fn phase_check(&mut self, budget: Budget, bound: Duration) -> Result<Phase, Error> {
        match self
            .write_out(&[PHASE_CHECK_CODE], budget.left()?.min(bound))
            .await
        {
            Ok(()) => {}
            Err(Error::Timeout(_)) => return Ok(Phase::Silent),
            Err(e) => return Err(e),
        }
        let mut phase = [0u8; 1];
        match self.read_in(&mut phase, budget.left()?.min(bound)).await {
            Ok(Chunk::Got(1)) => {
                trace!(phase = format!("{:02X}h", phase[0]), "phase");
                Ok(Phase::Code(phase[0]))
            }
            // Anything but that one byte and the pipe is out of step: a page of
            // data is the answer to a command that came before ours, and
            // nothing at all is a packet left over from one
            Ok(Chunk::Got(n) | Chunk::TooMuch(n)) => Ok(Phase::Stale(n)),
            Err(Error::Timeout(_)) => Ok(Phase::Silent),
            Err(e) => Err(e),
        }
    }

    /// One whole command, from the CDB to the status bytes
    async fn exchange(
        &mut self,
        cdb: &[u8],
        data: Data<'_>,
        budget: Budget,
    ) -> Result<Attempt, Error> {
        // Following the phase spec from 1-1-2 in LS5K spec

        // A unit that will not take the command has not run it
        match self.write_out(cdb, budget.left()?.min(PHASE_WAIT)).await {
            Ok(()) => {}
            Err(Error::Timeout(_)) => return Ok(Attempt::Resend { drain: true }),
            Err(e) => return Err(e),
        }

        // It holds the command from here. BUSY says it is working on that one,
        // so we ask it again: a second command would be a second command, and
        // the LS-40 refuses a repeated READ with 05h-2Ch
        let mut wait = BUSY_WAIT;
        let mut bound = PHASE_WAIT;
        let phase = loop {
            match self.phase_check(budget, bound).await? {
                Phase::Code(PHASE_BUSY) => {
                    sleep(wait.min(budget.left()?)).await;
                    wait = (wait * 2).min(BUSY_WAIT_MAX);
                    bound = RECHECK_WAIT;
                }
                Phase::Code(phase) => break phase,
                // Nothing at all, so there is nothing to wait for
                Phase::Silent => return Ok(Attempt::Resend { drain: true }),
                Phase::Stale(n) => {
                    return Ok(Attempt::Desync(
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("a phase check read {n} bytes off the pipe"),
                        )
                        .into(),
                    ));
                }
            }
        };

        // Now we act on the phase byte we got back, which on an error is not
        // the one we asked for
        let transferred = match (phase, data) {
            (PHASE_STATUS, Data::None) => 0,
            (PHASE_STATUS, d) => {
                // A refused command has no data phase
                debug!(data = ?d, "the unit went to status with no data phase");
                0
            }
            (PHASE_DATA_OUT, Data::Out(x)) => {
                self.write_out(x, budget.left()?).await?;
                x.len()
            }
            (PHASE_DATA_IN, Data::In(x)) => self.read_data_in(x, budget).await?,
            // 00h is "nothing is received", so the unit does not hold the
            // command we just wrote
            (PHASE_NONE, _) => {
                return Ok(Attempt::Desync(
                    io::Error::new(io::ErrorKind::InvalidData, "no phase after the command").into(),
                ));
            }
            (p @ (PHASE_DATA_IN | PHASE_DATA_OUT), d) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the unit asked for phase {p:02X}h but the command carries {d:?}"),
                )
                .into());
            }
            (x, _) => {
                return Ok(Attempt::Desync(
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{x:02X}h is not a phase code"),
                    )
                    .into(),
                ));
            }
        };

        // 1-1-2-2 has the host send 06h (status reception code) here, and
        // Nikon's own USB code sends it. An LS-50 sends the status without it,
        // after a data IN, a data OUT and a status phase. An LS-40 stops
        // answering after it.

        // 1-1-5-2 says we'll always get 8 bytes back from status. It is the
        // closing packet of its own phase same as a data phase's last one, and
        // the unit pads it out to a whole packet the same way
        let mut sb = [0u8; 8];
        let n = self.read_last(&mut sb, budget.left()?).await?;

        if n != 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("status phase returned {n} bytes, expected 8"),
            )
            .into());
        }

        // According to table 1-1-5-1 this will only ever be Good or CheckCondition, which is reasonable
        let status = Status::from(sb[0]);

        let sense = Some(Sense {
            // 1-1-5-2 gives the key the low nibble and zeroes the rest
            key: sb[1] & 0x0F,
            asc: sb[2],
            ascq: sb[3],
            tsc: Some(sb[4]),
            // This wrapper carries the four sense bytes and nothing else, so
            // there is no ILI or information field to read
            ili: false,
            information: None,
            raw: sb.to_vec(),
        });

        Ok(Attempt::Done(Completion {
            status,
            sense,
            transferred,
        }))
    }

    /// One SCSI transaction, from the CDB to the status bytes, with the
    /// recovery when the pipe is out of step
    async fn execute(
        &mut self,
        cdb: &[u8],
        mut data: Data<'_>,
        timeout: Duration,
    ) -> Result<Completion, Error> {
        // A command that failed partway through left the unit mid-answer, so
        // drop that before starting another rather than reading its leftovers
        // as this one's phases
        if self.dirty {
            self.resync().await;
        }
        // A command that gets no answer is only ever debugged from the bytes
        // that went out for it
        if enabled!(Level::TRACE) {
            let hex: Vec<String> = cdb.iter().map(|b| format!("{b:02X}")).collect();
            trace!(cdb = hex.join(" "), ?data, "command");
        }
        let budget = Budget::new(timeout);
        let mut drained = false;
        loop {
            budget.left()?;
            // Cleared again only by a run that reaches the end of the handshake
            self.dirty = true;
            match self.exchange(cdb, data.reborrow(), budget).await? {
                Attempt::Done(completion) => {
                    self.dirty = false;
                    return Ok(completion);
                }
                Attempt::Resend { drain } => {
                    // A check that ran out of time can still be answered, so
                    // drop the answer before the command goes out again
                    if drain {
                        debug!("the unit stopped answering, we send the command again");
                        self.resync().await;
                    }
                    sleep(BUSY_WAIT.min(budget.left()?)).await;
                }
                // One drain and one resend is the whole recovery. A unit that
                // gives a second phase byte we cannot read is not out of step:
                // it is answering with something we do not understand
                Attempt::Desync(e) => {
                    if drained {
                        return Err(e);
                    }
                    drained = true;
                    warn!(%e, "the pipe is out of step, we drop it and send the command again");
                    self.resync().await;
                }
            }
        }
    }
}

impl Transport for UsbTransport {
    fn max_transfer(&self) -> usize {
        // A cap on what one READ asks for, not a limit of the unit
        1024 * 1024
    }

    fn execute<'a>(
        &'a mut self,
        cdb: &'a [u8],
        data: Data<'a>,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Completion, Error>> {
        Box::pin(self.phases.execute(cdb, data, timeout))
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, Result<(), Error>> {
        // The interface is released only when the endpoints are dropped too
        let Pipes { interface, .. } = self.phases.link;
        Box::pin(async move {
            match settle(interface.release()).await {
                // A native transfer that timed out can keep the interface busy, and the last drop releases it
                #[cfg(not(target_arch = "wasm32"))]
                Err(e) if e.kind() == nusb::ErrorKind::Busy => {
                    debug!(%e, "the interface is still held, leaving it to the drop");
                    Ok(())
                }
                r => r.map_err(|e| Error::Io(e.into())),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// One step of the unit, in the order the handshake expects it
    #[derive(Debug)]
    enum Step {
        /// The host writes these bytes and the unit takes them
        Out(Vec<u8>),
        /// The next read gives these bytes
        In(Vec<u8>),
        /// The next read times out with nothing
        Silent,
    }

    /// A unit that follows a script and fails the test if the host does anything else
    struct Script {
        steps: VecDeque<Step>,
    }

    impl Script {
        fn new(steps: impl IntoIterator<Item = Step>) -> Phases<Self> {
            Phases::new(Self {
                steps: steps.into_iter().collect(),
            })
        }
    }

    impl Link for Script {
        fn in_max_packet(&self) -> usize {
            64
        }

        async fn write_out(&mut self, bytes: &[u8], _: Duration) -> Result<(), Error> {
            match self.steps.pop_front() {
                Some(Step::Out(want)) => {
                    assert_eq!(bytes, &want[..], "the host wrote something else");
                    Ok(())
                }
                other => panic!("the host wrote {bytes:02X?}, the unit expected {other:?}"),
            }
        }

        async fn transfer_in(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>, Error> {
            assert_eq!(len % 64, 0, "a read of {len} bytes is not whole packets");
            match self.steps.pop_front() {
                Some(Step::In(bytes)) => {
                    assert!(
                        bytes.len() <= len,
                        "{} bytes for a {len}-byte read",
                        bytes.len()
                    );
                    Ok(bytes)
                }
                Some(Step::Silent) => Err(Error::Timeout(timeout)),
                other => panic!("the host read, the unit expected {other:?}"),
            }
        }
    }

    const TEST_UNIT_READY: [u8; 6] = [0; 6];

    fn status(key: u8, asc: u8, ascq: u8) -> Step {
        Step::In(vec![0x00, key, asc, ascq, 0, 0, 0, 0])
    }

    fn check() -> Step {
        Step::Out(vec![PHASE_CHECK_CODE])
    }

    fn phase(code: u8) -> Step {
        Step::In(vec![code])
    }

    fn run(phases: &mut Phases<Script>, cdb: &[u8], data: Data<'_>) -> Result<Completion, Error> {
        let completion = pollster::block_on(phases.execute(cdb, data, Duration::from_secs(5)));
        assert!(
            phases.link.steps.is_empty(),
            "the script was not played to the end: {:?}",
            phases.link.steps
        );
        completion
    }

    #[test]
    fn a_command_with_no_data_goes_straight_to_status() {
        let mut unit = Script::new([
            Step::Out(TEST_UNIT_READY.to_vec()),
            check(),
            phase(PHASE_STATUS),
            status(0, 0, 0),
        ]);
        let done = run(&mut unit, &TEST_UNIT_READY, Data::None).expect("command");
        assert_eq!(done.status, Status::Good);
        assert_eq!(done.transferred, 0);
        assert!(!unit.dirty);
    }

    /// The LS-40 answers BUSY to the first phase check of each command, so only the check is sent again
    #[test]
    fn busy_asks_the_phase_again_without_sending_the_command_again() {
        let mut unit = Script::new([
            Step::Out(TEST_UNIT_READY.to_vec()),
            check(),
            phase(PHASE_BUSY),
            check(),
            phase(PHASE_BUSY),
            check(),
            phase(PHASE_STATUS),
            status(0, 0, 0),
        ]);
        run(&mut unit, &TEST_UNIT_READY, Data::None).expect("command");
    }

    /// An LS-40 READ that starts with the last 1522 bytes of a 1722-byte line
    #[test]
    fn a_read_after_a_split_line_takes_whole_lines() {
        let data: Vec<u8> = (0..20_000).map(|i| i as u8).collect();
        let mut steps = vec![Step::Out(vec![0x28; 10]), check(), phase(PHASE_DATA_IN)];
        steps.extend(
            std::iter::once(&data[..1522])
                .chain(data[1522..].chunks(1722))
                .map(|piece| Step::In(piece.to_vec())),
        );
        steps.push(status(0, 0, 0));
        let mut unit = Script::new(steps);

        let mut out = vec![0; data.len()];
        let done = run(&mut unit, &[0x28; 10], Data::In(&mut out)).expect("read");
        assert_eq!(done.transferred, data.len());
        assert_eq!(out, data);
    }

    #[test]
    fn a_data_out_phase_writes_the_payload() {
        let payload = vec![7u8; 20];
        let mut unit = Script::new([
            Step::Out(vec![0x15; 6]),
            check(),
            phase(PHASE_DATA_OUT),
            Step::Out(payload.clone()),
            status(0, 0, 0),
        ]);
        let done = run(&mut unit, &[0x15; 6], Data::Out(&payload)).expect("send");
        assert_eq!(done.transferred, payload.len());
    }

    #[test]
    fn a_failed_command_comes_back_with_its_sense() {
        let mut unit = Script::new([
            Step::Out(TEST_UNIT_READY.to_vec()),
            check(),
            phase(PHASE_STATUS),
            Step::In(vec![0x02, 0x02, 0x3A, 0x00, 0x01, 0, 0, 0]),
        ]);
        let done = run(&mut unit, &TEST_UNIT_READY, Data::None).expect("command");
        assert_eq!(done.status, Status::CheckCondition);
        let sense = done.sense.expect("sense");
        assert_eq!(
            (sense.key, sense.asc, sense.ascq, sense.tsc),
            (2, 0x3A, 0, Some(1))
        );
    }

    /// More than one byte on a phase check is an earlier answer, so drain the pipe and send the command again
    #[test]
    fn a_stale_answer_is_drained_and_the_command_sent_again() {
        let mut unit = Script::new([
            Step::Out(TEST_UNIT_READY.to_vec()),
            check(),
            Step::In(vec![0xAA; 12]),
            // The drain
            Step::In(vec![0xBB; 30]),
            Step::Silent,
            // Again, in step
            Step::Out(TEST_UNIT_READY.to_vec()),
            check(),
            phase(PHASE_STATUS),
            status(0, 0, 0),
        ]);
        run(&mut unit, &TEST_UNIT_READY, Data::None).expect("command");
        assert!(!unit.dirty);
    }

    /// The drain reads the late answer to a phase check that timed out, then sends the command again
    #[test]
    fn a_silent_unit_is_drained_of_its_late_answer_and_asked_again() {
        let mut unit = Script::new([
            Step::Out(TEST_UNIT_READY.to_vec()),
            check(),
            Step::Silent,
            // The drain takes the answer that came late
            phase(PHASE_STATUS),
            Step::Silent,
            Step::Out(TEST_UNIT_READY.to_vec()),
            check(),
            phase(PHASE_STATUS),
            status(0, 0, 0),
        ]);
        run(&mut unit, &TEST_UNIT_READY, Data::None).expect("command");
    }

    /// Drain only once: a second unreadable phase byte is an error
    #[test]
    fn a_second_desync_is_an_error() {
        let mut unit = Script::new([
            Step::Out(TEST_UNIT_READY.to_vec()),
            check(),
            phase(0x77),
            Step::Silent,
            Step::Out(TEST_UNIT_READY.to_vec()),
            check(),
            phase(0x77),
        ]);
        let Err(err) = run(&mut unit, &TEST_UNIT_READY, Data::None) else {
            panic!("a second desync completed");
        };
        assert!(matches!(err, Error::Io(_)), "{err:?}");
        assert!(unit.dirty);
    }

    /// A command that fails partway leaves its answer on the pipe, so the next command drains it first
    #[test]
    fn the_command_after_a_failed_one_drains_first() {
        let mut unit = Script::new([
            Step::Out(TEST_UNIT_READY.to_vec()),
            check(),
            phase(PHASE_STATUS),
            Step::In(vec![0x00; 3]),
        ]);
        let short = run(&mut unit, &TEST_UNIT_READY, Data::None);
        assert!(short.is_err(), "a short status completed");
        assert!(unit.dirty);

        unit.link.steps.extend([
            Step::In(vec![0x00; 5]),
            Step::Silent,
            Step::Out(TEST_UNIT_READY.to_vec()),
            check(),
            phase(PHASE_STATUS),
            status(0, 0, 0),
        ]);
        run(&mut unit, &TEST_UNIT_READY, Data::None).expect("command");
        assert!(!unit.dirty);
    }
}
