//! SCSI-over-USB transport implemented with nusb

use super::{Completion, Data, Error, Sense, Status, Transport};
use nusb::{
    DeviceInfo, Endpoint, Interface, MaybeFuture, list_devices,
    transfer::{Buffer, Bulk, In, Out, TransferError},
};
use std::{
    io,
    thread::sleep,
    time::{Duration, Instant},
};
use tracing::*;

/// The "vendor ID" for Nikon
const VID: u16 = 0x04B0;

/// USB-attached Coolscans
const PIDS: &[u16] = &[
    0x4000, // LS-40 ED  (USB 1.1)
    0x4001, // LS-50 ED
    0x4002, // LS-5000 ED
];

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

/// A USB-connected Nikon scanner
#[allow(dead_code)]
pub struct UsbTransport {
    ep_out: Endpoint<Bulk, Out>,
    ep_in: Endpoint<Bulk, In>,
    in_max_packet: usize,
    /// We have to keep a handle to the interface here as the lifetime of the endpoints are attached to it
    interface: Interface,
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
    /// List all attached Coolscan USB devices
    pub fn list() -> io::Result<Vec<DeviceInfo>> {
        Ok(list_devices()
            .wait()?
            .filter(|dev| dev.vendor_id() == VID && PIDS.contains(&dev.product_id()))
            .collect())
    }

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
        })
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
    fn read_in(&mut self, out: &mut [u8], timeout: Duration) -> Result<usize, Error> {
        // We need to request a whole-number of self.in_max_packet-sized chunks
        let req = out.len().max(1).div_ceil(self.in_max_packet) * self.in_max_packet;
        let buf = self
            .ep_in
            .transfer_blocking(Buffer::new(req), timeout)
            .into_result()
            .map_err(|e| transfer_err(e, timeout))?;
        let n = buf.len().min(out.len());
        // this would be weird huh
        if buf.len() > out.len() {
            warn!(
                got = buf.len(),
                want = out.len(),
                "device sent more than requested"
            );
        }
        out[..n].copy_from_slice(&buf[..n]);
        Ok(n)
    }
}

impl Transport for UsbTransport {
    fn max_transfer(&self) -> usize {
        // Hard-code 128 KB as a normal-ish chunk size
        // Should be fine?
        128 * 1024
    }

    fn execute(&mut self, cdb: &[u8], data: Data, timeout: Duration) -> Result<Completion, Error> {
        // Following the phase spec from 1-1-2 in LS5K spec

        // First we write the cdb
        self.write_out(cdb, timeout)?;

        // Then we loop the phase check while the device reports busy
        let mut phase = [0u8; 1];
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout(timeout));
            }
            self.write_out(&[PHASE_CHECK_CODE], left)?;
            if self.read_in(&mut phase, left)? == 0 {
                return Err(
                    io::Error::new(io::ErrorKind::InvalidData, "empty phase response").into(),
                );
            }
            if phase[0] != PHASE_BUSY {
                break;
            }
            sleep(Duration::from_millis(5));
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

        let sense = if status != Status::Good {
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
        } else {
            None
        };

        Ok(Completion {
            status,
            sense,
            transferred,
        })
    }
}
