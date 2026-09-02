//! Abstraction layer and cross-platform implementation of the transports for the scanners (USB/FireWire)
//!
//! No protocol-layer stuff in here, just a thin abstraction over the various platforms'/OSs' IO

use std::{fmt, io, time::Duration};

#[cfg(target_os = "macos")]
pub mod darwin;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod usb;
#[cfg(target_os = "windows")]
pub mod windows;

/// How much sense data we ask a transport for, matches the Linux kernel's SCSI_SENSE_BUFFERSIZE.
/// The transports will basically always return less
///
/// The sg path is the only one that gets to ask: `scsiscan.sys` fixes its own
/// buffer and the USB wrapper carries eight bytes whatever we do
#[cfg(target_os = "linux")]
const SENSE_REQUEST_LEN: usize = 96;

/// The SCSI operation error type
///
/// Only for a transaction that never completed. A completed one, even a
/// failed one, comes back as a [`Completion`]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("timed out after {0:?}")]
    Timeout(Duration),
}

/// Which way, if any, a command's data phase moves
pub enum Data<'a> {
    /// No data to transfer
    None,
    /// Host reads this many bytes from the device
    In(&'a mut [u8]),
    /// Host writes these bytes to the device
    Out(&'a [u8]),
}

impl<'a> Data<'a> {
    /// Borrow the data phase again, so we can send a command more than once
    pub(crate) fn reborrow(&mut self) -> Data<'_> {
        match self {
            Data::None => Data::None,
            Data::In(buf) => Data::In(buf),
            Data::Out(buf) => Data::Out(buf),
        }
    }
}

impl fmt::Debug for Data<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Data::None => write!(f, "None"),
            Data::In(b) => write!(f, "In({} bytes)", b.len()),
            Data::Out(b) => write!(f, "Out({} bytes)", b.len()),
        }
    }
}

/// One SCSI command's result, once it completed
#[derive(Clone, PartialEq, Eq)]
pub struct Completion {
    /// The completion status
    pub status: Status,
    /// The optional sense payload
    pub sense: Option<Sense>,
    /// How many bytes were transferred in this operation
    pub transferred: usize,
}

#[derive(Clone, PartialEq, Eq)]
/// The sense codes and data, as raw as the transport reports them. Their
/// precise meaning is [`crate::protocol::sense`]'s job, not this layer's
pub struct Sense {
    /// Main sense key
    pub key: u8,
    /// Additional sense code
    pub asc: u8,
    /// Additional sense code qualifier
    pub ascq: u8,
    /// Tertiary sense code, Nikon's own 4th sense element and not part of
    /// any SCSI spec. Populated on every transport where the buffer runs
    /// long enough to carry it
    pub tsc: Option<u8>,
    /// Incorrect length indicator, byte 2 bit 5. The transfer was shorter than
    /// asked for, and 2-11 uses it to say a read ran past what the unit holds
    pub ili: bool,
    /// Bytes 3-6, when the valid bit says they mean anything. Under `ili` this
    /// is how far short the transfer fell
    pub information: Option<u32>,
    /// The full raw sense buffer
    pub raw: Vec<u8>,
}

impl fmt::Debug for Sense {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02X}h-{:02X}h-{:02X}h", self.key, self.asc, self.ascq)?;
        if self.ili {
            write!(f, " ILI")?;
            if let Some(n) = self.information {
                write!(f, "({n})")?;
            }
        }
        match self.tsc {
            Some(t) => write!(f, "-{t:02X}h")?,
            None => write!(f, "-??")?,
        }
        write!(f, " raw={:02X?}", self.raw)
    }
}

/// Parse a fixed-format (70h/71h) sense buffer into [`Sense`]
///
/// The caller guarantees at least the 14 minimal bytes and passes the slice it
/// is willing to trust as `raw` together with the `tsc` value it can justify,
/// since the platforms differ on how far the driver wrote
///
/// The SCSI passthroughs only: the USB wrapper carries its four sense bytes
/// in a shape of its own and builds a [`Sense`] directly
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn sense_from_fixed(buffer: &[u8], tsc: Option<u8>) -> Sense {
    Sense {
        key: buffer[2] & 0xF,
        ili: buffer[2] & 0x20 != 0,
        // The valid bit, byte 0 bit 7, says the information field means
        // something
        information: (buffer[0] & 0x80 != 0)
            .then(|| u32::from_be_bytes([buffer[3], buffer[4], buffer[5], buffer[6]])),
        asc: buffer[12],
        ascq: buffer[13],
        tsc,
        raw: buffer.to_vec(),
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
/// The "high-level" SCSI status
pub enum Status {
    /// Everything is fine
    Good,
    /// Something was not fine and the sense data buffer has more details
    CheckCondition,
    /// Something is happening we need to wait for (SBP-2 only)
    Busy,
    /// Some other process has exclusive control (SBP-2 only)
    ReservationConflict,
    /// An unexpected status code
    Other(u8),
}

impl From<u8> for Status {
    fn from(value: u8) -> Self {
        // These are the only codes we will expect across all scanners
        match value {
            0x00 => Self::Good,
            0x02 => Self::CheckCondition,
            0x08 => Self::Busy,
            0x18 => Self::ReservationConflict,
            x => Self::Other(x),
        }
    }
}

/// What one platform backend has to implement to carry a CDB and its data phase
pub trait Transport: Send {
    /// The maximum size in bytes we can transfer in a single operation.
    fn max_transfer(&self) -> usize;

    /// Run one SCSI transaction: `cdb` is the command, `data` its data phase
    ///
    /// Errors are link-layer only; a failed command still comes back `Ok` as
    /// a [`Completion`] carrying its sense. `timeout` is best-effort: some
    /// backends, Windows among them, have no way to enforce it
    fn execute(&mut self, cdb: &[u8], data: Data, timeout: Duration) -> Result<Completion, Error>;
}
