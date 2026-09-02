//! SCSI-over-USB transport implemented with nusb

use super::{Completion, Data, Error, Sense, Status, Transport};
use nusb::{
    DeviceInfo, Endpoint, Interface, MaybeFuture,
    transfer::{Buffer, Bulk, In, Out, TransferError},
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

/// The first wait before sending a busy unit its command again, and the longest
/// that wait becomes
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

/// A USB-connected Nikon scanner
#[allow(dead_code)]
pub struct UsbTransport {
    ep_out: Endpoint<Bulk, Out>,
    ep_in: Endpoint<Bulk, In>,
    in_max_packet: usize,
    /// We have to keep a handle to the interface here as the lifetime of the endpoints are attached to it
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
        // We never cancel transfers, so a cancelled one is always the timeout
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
            // The flag does not outlive the process that set it, and a
            // program that died mid-command left its answer on the pipe, so a
            // fresh handle assumes the worst and drains before its first command
            dirty: true,
        })
    }

    /// Read off whatever the last command left in the pipe
    ///
    /// Being one command out of step is not something a caller can act on: the
    /// next phase byte is a stale data byte, the one after that is worse, and
    /// the way out is a power cycle. Draining until the pipe comes back empty
    /// puts the two ends back in step, which is verified against an LS-50 that
    /// had been desynced on purpose - eight stale bytes drained and the unit
    /// answered INQUIRY again without a reset
    fn resync(&mut self) {
        let mut dropped = 0usize;
        while dropped < RESYNC_LIMIT {
            match self
                .ep_in
                .transfer_blocking(Buffer::new(self.in_max_packet), RESYNC_TIMEOUT)
                .into_result()
            {
                Ok(b) if !b.is_empty() => dropped += b.len(),
                // Empty or refused: the unit has nothing more to say
                _ => break,
            }
        }
        if dropped > 0 {
            warn!(
                bytes = dropped,
                "the last command left its answer in the pipe, dropped it to get back in step"
            );
        }
        self.dirty = false;
    }

    /// Write `bytes` to the Bulk Out endpoint
    fn write_out(&mut self, bytes: &[u8], timeout: Duration) -> Result<(), Error> {
        self.ep_out
            .transfer_blocking(bytes.into(), timeout)
            .into_result()
            .map_err(|e| transfer_err(e, timeout))?;
        Ok(())
    }

    /// Read bytes from the Bulk In endpoint, filling out and returning the number of bytes read
    ///
    /// The request is rounded up to whole packets, so the unit can answer with
    /// more than `out` holds. Those bytes are off the wire and there is nowhere
    /// to put them: whatever is read next starts mid-answer, so this ends the
    /// transport rather than dropping them and carrying on
    fn read_in(&mut self, out: &mut [u8], timeout: Duration) -> Result<usize, Error> {
        // We need to request a whole-number of self.in_max_packet-sized chunks
        let req = out.len().max(1).div_ceil(self.in_max_packet) * self.in_max_packet;
        let buf = self
            .ep_in
            .transfer_blocking(Buffer::new(req), timeout)
            .into_result()
            .map_err(|e| transfer_err(e, timeout))?;
        if buf.len() > out.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "device sent {} bytes for a {}-byte read, so the stream is out of step",
                    buf.len(),
                    out.len()
                ),
            )
            .into());
        }
        out[..buf.len()].copy_from_slice(&buf);
        Ok(buf.len())
    }

    /// Read a whole data IN phase
    ///
    /// The unit sends the data in pieces. An LS-40 sends one image line for
    /// each piece, so one read gets the first line only. We read until we have
    /// the transfer length the CDB asked for, and after the first piece we ask
    /// for the size of that piece. Nikon's own USB code does the same.
    fn read_data_in(
        &mut self,
        out: &mut [u8],
        deadline: Instant,
        timeout: Duration,
    ) -> Result<usize, Error> {
        let mut done = 0;
        let mut piece = out.len();
        while done < out.len() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout(timeout));
            }
            let want = piece.min(out.len() - done);
            let got = self.read_in(&mut out[done..done + want], left)?;
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
}

impl Transport for UsbTransport {
    fn max_transfer(&self) -> usize {
        // Hard-code 128 KB as a normal-ish chunk size
        // Should be fine?
        128 * 1024
    }

    fn execute(
        &mut self,
        cdb: &[u8],
        mut data: Data,
        timeout: Duration,
    ) -> Result<Completion, Error> {
        // A command that failed partway through left the unit mid-answer, so
        // clear that before starting another rather than reading its leftovers
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
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout(timeout));
            }
            // Cleared again only by a run that reaches the end of the handshake
            self.dirty = true;
            match self.exchange(cdb, data.reborrow(), left)? {
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
                    sleep(BUSY_WAIT.min(left));
                }
            }
        }
    }
}

/// What one try at a command gave us
enum Attempt {
    /// The unit answered the command
    Done(Completion),
    /// The command is spent and can go out again. `drain` says the unit went
    /// quiet rather than answering, so it may still answer late
    Resend { drain: bool },
}

impl UsbTransport {
    /// Ask what the unit wants next, `None` where it did not take the question
    /// or did not answer it inside `wait`
    fn phase_check(&mut self, wait: Duration) -> Result<Option<u8>, Error> {
        match self.write_out(&[PHASE_CHECK_CODE], wait) {
            Ok(()) => {}
            Err(Error::Timeout(_)) => return Ok(None),
            Err(e) => return Err(e),
        }
        let mut phase = [0u8; 1];
        match self.read_in(&mut phase, wait) {
            Ok(0) => Err(io::Error::new(io::ErrorKind::InvalidData, "empty phase response").into()),
            Ok(_) => {
                trace!(phase = format!("{:02X}h", phase[0]), "phase");
                Ok(Some(phase[0]))
            }
            Err(Error::Timeout(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// One whole command, from the CDB to the status bytes
    fn exchange(&mut self, cdb: &[u8], data: Data, timeout: Duration) -> Result<Attempt, Error> {
        // Following the phase spec from 1-1-2 in LS5K spec

        // A unit that will not take the command has not run it
        match self.write_out(cdb, timeout.min(PHASE_WAIT)) {
            Ok(()) => {}
            Err(Error::Timeout(_)) => return Ok(Attempt::Resend { drain: true }),
            Err(e) => return Err(e),
        }

        // It holds the command from here. BUSY says it is working on that one,
        // so we ask it again: a second command would be a second command, and
        // the LS-40 refuses a repeated READ with 05h-2Ch
        let deadline = Instant::now() + timeout;
        let mut wait = BUSY_WAIT;
        let mut bound = PHASE_WAIT;
        let phase = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout(timeout));
            }
            match self.phase_check(left.min(bound))? {
                Some(PHASE_BUSY) => {
                    sleep(wait.min(left));
                    wait = (wait * 2).min(BUSY_WAIT_MAX);
                    bound = RECHECK_WAIT;
                }
                Some(phase) => break phase,
                // Nothing at all, so there is nothing to wait for
                None => return Ok(Attempt::Resend { drain: true }),
            }
        };

        // Now we act of the phase byte we got back (could be not what we asked for on errors)
        let transferred = match (phase, data) {
            (PHASE_STATUS, Data::None) => 0,
            (PHASE_STATUS, x) => {
                debug!("We requested a non-none data phase {:?} but got none", x);
                0
            }
            (PHASE_DATA_OUT, Data::Out(x)) => {
                self.write_out(x, timeout)?;
                x.len()
            }
            (PHASE_DATA_IN, Data::In(x)) => self.read_data_in(x, deadline, timeout)?,
            (PHASE_NONE, _) => {
                return Err(
                    io::Error::new(io::ErrorKind::InvalidData, "no phase after command").into(),
                );
            }
            (p @ (PHASE_DATA_IN | PHASE_DATA_OUT), d) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("device reported phase {p:#04x} but command supplied {d:?}"),
                )
                .into());
            }
            (x, _) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid phase byte {x:#04x}"),
                )
                .into());
            }
        };

        // 1-1-2-2 has the host send 06h (status reception code) here, and
        // Nikon's own USB code sends it. An LS-50 sends the status without it,
        // after a data IN, a data OUT and a status phase. An LS-40 stops
        // answering after it.

        // 1-1-5-2 says we'll always get 8 bytes back from status
        let mut sb = [0u8; 8];
        let n = self.read_in(&mut sb, timeout)?;

        if n != 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("status phase returned {n} bytes, expected 8"),
            )
            .into());
        }

        // According to table 1-1-5-1 this will only ever be Good or CheckCondition, which is reasonable
        let status = Status::from(sb[0]);

        let sense = {
            Some(Sense {
                key: sb[1],
                asc: sb[2],
                ascq: sb[3],
                tsc: Some(sb[4]),
                // This wrapper carries the four sense bytes and nothing else,
                // so there is no ILI or information field to read
                ili: false,
                information: None,
                raw: sb.to_vec(),
            })
        };

        Ok(Attempt::Done(Completion {
            status,
            sense,
            transferred,
        }))
    }
}
