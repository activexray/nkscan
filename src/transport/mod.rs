//! Abstraction layer and cross-platform implementation of the transports for the scanners (USB/FireWire)
//!
//! No protocol-layer stuff in here, just a thin abstraction over the various platforms'/OSs' IO

use std::{fmt, io, time::Duration};

#[cfg(target_os = "linux")]
pub mod linux;
pub mod usb;
#[cfg(target_os = "windows")]
pub mod windows;

/// How much sense data we ask a transport for, matches the Linux kernel's SCSI_SENSE_BUFFERSIZE.
/// The transports will basically always return less
const SENSE_REQUEST_LEN: usize = 96;

/// The SCSI operation error type
///
/// This will only exist if we didn't complete a SCSI transaction
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("timed out after {0:?}")]
    Timeout(Duration),
}

/// Data phase types and their associated data
pub enum Data<'a> {
    /// No data to transfer
    None,
    /// Host reads this many bytes from the device
    In(&'a mut [u8]),
    /// Host writes these bytes to the device
    Out(&'a [u8]),
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

/// The SCSI completion
/// This contains status, sense, and how many bytes were transferred
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
/// The sense codes and data, the precise meaning of these are delegated to the wire spec
pub struct Sense {
    /// Main sense key
    pub key: u8,
    /// Additional sense code
    pub asc: u8,
    /// Additional sense code qualifier
    pub ascq: u8,
    /// Tertiary sense code? Nikon never defines this but only uses in USB transport
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

/// The SCSI transport abstraction
pub trait Transport: Send {
    /// The maximum size in bytes we can transfer in a single operation.
    fn max_transfer(&self) -> usize;

    /// Perform a SCSI transaction with the "command data block" bytes `cdb` writing/reading the data phase contained in `data`.
    ///
    /// This returns errors on link-layer errors and Ok on completion which includes sense data carried along the way.
    /// `timeout` is not a promise as some backends (like Windows), don't have a mechanism for it.
    fn execute(&mut self, cdb: &[u8], data: Data, timeout: Duration) -> Result<Completion, Error>;
}
