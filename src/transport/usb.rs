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
/// Tell the unit we are ready to receive the phase response
const STATUS_RECEPTION_CODE: u8 = 0x06;

// Phase codes (LS5K table 1-1-2-1)
const PHASE_NONE: u8 = 0x00;
const PHASE_STATUS: u8 = 0x01;
const PHASE_DATA_OUT: u8 = 0x02;
const PHASE_DATA_IN: u8 = 0x03;
const PHASE_BUSY: u8 = 0x04;

// The unit answers one phase check for each command. Nikon's own USB driver
// (NKDUSCAN.dll) checks the phase one time, and reports an error for a phase
// code it does not expect. It never sends a second D0h. The LS-40 stops
// answering the pipe after a second D0h, and only a power cycle recovers it.
// So a unit that answers BUSY gets the command again instead
/// The first wait before we send a busy unit its command again
const BUSY_WAIT: Duration = Duration::from_millis(5);
/// The longest that wait becomes
const BUSY_WAIT_MAX: Duration = Duration::from_millis(250);

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
            dirty: false,
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
        let mut wait = BUSY_WAIT;
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
                Attempt::Busy => {
                    debug!(?wait, "the unit is busy, we send the command again");
                    sleep(wait.min(left));
                    wait = (wait * 2).min(BUSY_WAIT_MAX);
                }
            }
        }
    }
}

/// What one try at a command gave us
enum Attempt {
    /// The unit answered the command
    Done(Completion),
    /// The unit is still busy with the command before this one
    Busy,
}

impl UsbTransport {
    /// One whole command, from the CDB to the status bytes
    fn exchange(&mut self, cdb: &[u8], data: Data, timeout: Duration) -> Result<Attempt, Error> {
        // Following the phase spec from 1-1-2 in LS5K spec

        // First we write the cdb
        self.write_out(cdb, timeout)?;

        // Then we check the phase, one time only
        let mut phase = [0u8; 1];
        self.write_out(&[PHASE_CHECK_CODE], timeout)?;
        if self.read_in(&mut phase, timeout)? == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty phase response").into());
        }
        trace!(phase = format!("{:02X}h", phase[0]), "phase");
        if phase[0] == PHASE_BUSY {
            return Ok(Attempt::Busy);
        }

        // Now we act of the phase byte we got back (could be not what we asked for on errors)
        let transferred = match (phase[0], data) {
            (PHASE_STATUS, Data::None) => 0,
            (PHASE_STATUS, x) => {
                debug!("We requested a non-none data phase {:?} but got none", x);
                0
            }
            (PHASE_DATA_OUT, Data::Out(x)) => {
                self.write_out(x, timeout)?;
                x.len()
            }
            (PHASE_DATA_IN, Data::In(x)) => self.read_in(x, timeout)?,
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

        // Finally grab the sense codes at the end by indicating we are ready to receive the status
        self.write_out(&[STATUS_RECEPTION_CODE], timeout)?;

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
