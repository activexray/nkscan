//! SCSI-over-USB transport implemented with nusb

use super::{Completion, Data, Error, Sense, Status, Transport};
use nusb::{
    DeviceInfo, Endpoint, Interface, MaybeFuture,
    transfer::{Buffer, Bulk, BulkOrInterrupt, EndpointDirection, In, Out, TransferError},
};
use std::{
    io,
    thread::sleep,
    time::{Duration, Instant},
};
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

/// A USB-connected Nikon scanner
pub struct UsbTransport {
    ep_out: Endpoint<Bulk, Out>,
    ep_in: Endpoint<Bulk, In>,
    in_max_packet: usize,
    /// We have to keep a handle to the interface here as the lifetime of the endpoints are attached to it
    #[allow(dead_code)]
    interface: Interface,
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

/// Clear an endpoint the unit halted
///
/// A stall stands until CLEAR_FEATURE, which 1-1-6-1-1 has this unit
/// supporting. Leaving it set would fail every transfer after the one that
/// stalled, so the command that hit it is the only one that has to be lost
fn clear_stall<T: BulkOrInterrupt, D: EndpointDirection>(
    ep: &mut Endpoint<T, D>,
    e: TransferError,
) {
    if e != TransferError::Stall {
        return;
    }
    match ep.clear_halt().wait() {
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

impl Chunk {
    /// The byte count, where an over-long answer ends the command
    fn count(self, asked: usize) -> Result<usize, Error> {
        match self {
            Chunk::Got(n) => Ok(n),
            Chunk::TooMuch(n) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "device sent {n} bytes for a {asked}-byte read, so the stream is out of step"
                ),
            )
            .into()),
        }
    }
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
    pub fn open(info: DeviceInfo) -> io::Result<Self> {
        let device = info.open().wait()?;
        // Unlike other platforms, macOS only sets a configuration itself for
        // composite-class devices, so a vendor-class scanner opens unconfigured
        // there. Windows and Linux always report an active configuration, which
        // is what keeps `set_configuration` (unsupported on Windows) out of
        // reach of the platforms that cannot take it
        if device.active_configuration().is_err() {
            device
                .set_configuration(1)
                .wait()
                .map_err(io::Error::other)?;
        }
        // From the spec, the interface is 0
        let interface = device.claim_interface(0).wait()?;
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
        Ok(Self {
            ep_out,
            ep_in,
            in_max_packet,
            interface,
            // A program that died mid-command left its answer on the pipe, and
            // a fresh handle cannot tell. We do not drain to find out: the read
            // that says the pipe is empty is a read that times out, and that
            // costs every open 200 ms on Linux and over two seconds on
            // Windows. The first command reads the leftovers as its phase
            // byte, and that is a desync, which drains and sends the command
            // again
            dirty: false,
        })
    }

    /// Read off whatever the last command left in the pipe
    ///
    /// Being one command out of step is not something a caller can act on: the
    /// next phase byte is a stale data byte, the one after that is worse, and
    /// the way out is a power cycle. Draining until the pipe comes back empty
    /// puts the two ends back in step, which is verified against an LS-50 that
    /// had been desynced on purpose: eight stale bytes drained and the unit
    /// answered INQUIRY again without a reset
    fn resync(&mut self) {
        let mut dropped = 0usize;
        while dropped < RESYNC_LIMIT {
            match self.transfer_in(self.in_max_packet, RESYNC_TIMEOUT) {
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

    /// Write `bytes` to the Bulk Out endpoint
    fn write_out(&mut self, bytes: &[u8], timeout: Duration) -> Result<(), Error> {
        let sent = self
            .ep_out
            .transfer_blocking(bytes.into(), timeout)
            .into_result();
        match sent {
            Ok(_) => Ok(()),
            Err(e) => {
                clear_stall(&mut self.ep_out, e);
                Err(transfer_err(e, timeout))
            }
        }
    }

    /// One read off the Bulk In endpoint, of enough whole packets to hold
    /// `want` bytes
    ///
    /// nusb takes the request in whole packets, so a `want` that does not land
    /// on a packet boundary leaves room for the unit to answer with up to a
    /// packet more than that. What the surplus means is the caller's to say
    fn transfer_in(&mut self, want: usize, timeout: Duration) -> Result<Buffer, Error> {
        let req = want.max(1).div_ceil(self.in_max_packet) * self.in_max_packet;
        let got = self
            .ep_in
            .transfer_blocking(Buffer::new(req), timeout)
            .into_result();
        got.map_err(|e| {
            clear_stall(&mut self.ep_in, e);
            transfer_err(e, timeout)
        })
    }

    /// Read from the Bulk In endpoint into `out`, saying what arrived
    ///
    /// `out` takes as much as it holds either way, so a [`Chunk::TooMuch`] is
    /// the caller's to judge rather than a read it has to do again
    fn read_in(&mut self, out: &mut [u8], timeout: Duration) -> Result<Chunk, Error> {
        let buf = self.transfer_in(out.len(), timeout)?;
        let n = buf.len().min(out.len());
        out[..n].copy_from_slice(&buf[..n]);
        match buf.len() > out.len() {
            true => Ok(Chunk::TooMuch(buf.len())),
            false => Ok(Chunk::Got(n)),
        }
    }

    /// Read the closing packet of a phase, where the unit fills it out to a
    /// whole number of packets instead of ending it short
    ///
    /// Whatever `out` asks for is what the phase has left, so a
    /// [`Chunk::TooMuch`] here is that padding, not the pipe out of step -
    /// unlike a read that has more of the phase still to come after it
    fn read_last(&mut self, out: &mut [u8], timeout: Duration) -> Result<usize, Error> {
        match self.read_in(out, timeout)? {
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
    /// The unit sends the data in pieces. An LS-40 sends one image line for
    /// each piece, so one read gets the first line only. We read until we have
    /// the transfer length the CDB asked for, and after the first piece we ask
    /// for the size of that piece. Nikon's own USB code does the same.
    fn read_data_in(&mut self, out: &mut [u8], budget: Budget) -> Result<usize, Error> {
        let mut done = 0;
        let mut piece = out.len();
        while done < out.len() {
            let want = piece.min(out.len() - done);
            // The phase is as long as the CDB's transfer length, which need not
            // end on a packet boundary, and the unit fills out that last
            // packet rather than ending it short. A surplus before then is not
            // padding, it is the stream out of step
            let last = done + want == out.len();
            let got = if last {
                self.read_last(&mut out[done..done + want], budget.left()?)?
            } else {
                self.read_in(&mut out[done..done + want], budget.left()?)?
                    .count(want)?
            };
            // The unit has nothing more to send
            if got == 0 {
                break;
            }
            if done == 0 && got < out.len() {
                piece = got;
            }
            done += got;
        }
        Ok(done)
    }

    /// Ask what the unit wants next, in no more than `bound` and no more than
    /// the command has left
    ///
    /// Both halves read the budget, so a check that spends its whole allowance
    /// asking cannot then spend it again listening
    fn phase_check(&mut self, budget: Budget, bound: Duration) -> Result<Phase, Error> {
        match self.write_out(&[PHASE_CHECK_CODE], budget.left()?.min(bound)) {
            Ok(()) => {}
            Err(Error::Timeout(_)) => return Ok(Phase::Silent),
            Err(e) => return Err(e),
        }
        let mut phase = [0u8; 1];
        match self.read_in(&mut phase, budget.left()?.min(bound)) {
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
    fn exchange(&mut self, cdb: &[u8], data: Data, budget: Budget) -> Result<Attempt, Error> {
        // Following the phase spec from 1-1-2 in LS5K spec

        // A unit that will not take the command has not run it
        match self.write_out(cdb, budget.left()?.min(PHASE_WAIT)) {
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
            match self.phase_check(budget, bound)? {
                Phase::Code(PHASE_BUSY) => {
                    sleep(wait.min(budget.left()?));
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
                self.write_out(x, budget.left()?)?;
                x.len()
            }
            (PHASE_DATA_IN, Data::In(x)) => self.read_data_in(x, budget)?,
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
        let n = self.read_last(&mut sb, budget.left()?)?;

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
}

impl Transport for UsbTransport {
    fn max_transfer(&self) -> usize {
        // A cap on what one READ asks for, not a limit of the unit
        128 * 1024
    }

    fn execute(
        &mut self,
        cdb: &[u8],
        mut data: Data,
        timeout: Duration,
    ) -> Result<Completion, Error> {
        // A command that failed partway through left the unit mid-answer, so
        // drop that before starting another rather than reading its leftovers
        // as this one's phases
        if self.dirty {
            self.resync();
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
            match self.exchange(cdb, data.reborrow(), budget)? {
                Attempt::Done(completion) => {
                    self.dirty = false;
                    return Ok(completion);
                }
                Attempt::Resend { drain } => {
                    // A check that ran out of time can still be answered, so
                    // drop the answer before the command goes out again
                    if drain {
                        debug!("the unit stopped answering, we send the command again");
                        self.resync();
                    }
                    sleep(BUSY_WAIT.min(budget.left()?));
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
                    self.resync();
                }
            }
        }
    }
}
