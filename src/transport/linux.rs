//! SCSI transport on Linux via the SCSI-Generic Userspace Module

use super::{Completion, Data, Error, SENSE_REQUEST_LEN, Status, Transport, sense_from_fixed};
use bitflags::bitflags;
use nix::{ioctl_read_bad, ioctl_readwrite_bad, ioctl_write_ptr_bad};
use std::{
    fmt,
    fs::{File, OpenOptions},
    io,
    os::{fd::AsRawFd, raw::c_void},
    path::Path,
    ptr::null_mut,
    time::Duration,
};
use tracing::*;

#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Direction {
    /// SCSI Test Unit Ready, or similar commands where there is no data transfer associated with it
    None = -1,
    /// WRITE, user memory to device
    ToDev = -2,
    /// READ, device to user memory
    FromDev = -3,
}

bitflags! {
    /// SCSI Flags
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Flags: u32 {
        const DIRECT_IO = 1;
        const UNUSED_LUN_INHIBIT = 2;
        const MMAP_IO = 4;
        /// For testing bus speed
        const NO_DXFER = 0x10000;
        /// Q_AT_HEAD for this driver, Q_AT_TAIL for block devices
        const Q_AT_TAIL = 0x10;
        const Q_AT_HEAD = 0x20;
    }
}

#[repr(u16)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum HostStatus {
    /// No Error
    Ok = 0x00,
    /// Couldn't connect before timeout period
    NoConnect = 0x01,
    /// Bus stayed busy through time out period
    BusBusy = 0x02,
    /// Timed out for other reason
    Timeout = 0x03,
    /// Bad target, device may not be responding
    BadTarget = 0x04,
    /// Told to abort for some other reason
    Abort = 0x05,
    /// Parity error
    Parity = 0x06,
    /// Internal error detected in the host adapter
    Error = 0x07,
    /// The SCSI bus or the device has been reset
    Reset = 0x08,
    /// Got an interrupt we weren't expecting
    BadIntr = 0x09,
    /// Force command past mid-layer
    Passthrough = 0x0A,
    /// The low-level driver wants a retry
    SoftError = 0x0B,
    /// Unknown/Unexpected byte
    Unknown(u16),
}

impl From<u16> for HostStatus {
    fn from(value: u16) -> Self {
        match value {
            0x00 => Self::Ok,
            0x01 => Self::NoConnect,
            0x02 => Self::BusBusy,
            0x03 => Self::Timeout,
            0x04 => Self::BadTarget,
            0x05 => Self::Abort,
            0x06 => Self::Parity,
            0x07 => Self::Error,
            0x08 => Self::Reset,
            0x09 => Self::BadIntr,
            0x0A => Self::Passthrough,
            0x0B => Self::SoftError,
            x => Self::Unknown(x),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct Info(u32);

impl Info {
    pub const CHECK: u32 = 0x1;
    pub const DIRECT_IO: u32 = 0x2;
    pub const MIXED_IO: u32 = 0x4;

    pub const fn check_status(self) -> u32 {
        self.0 & 0x1
    }

    pub const fn io_type(self) -> u32 {
        self.0 & 0x6
    }
}

impl fmt::Debug for Info {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Bit 0 unset is OK, and bits 1-2 both unset is INDIRECT_IO: both are
        // the zero value of their field
        let check = match self.check_status() {
            Self::CHECK => "CHECK",
            _ => "OK",
        };
        let io = match self.io_type() {
            Self::DIRECT_IO => "DIRECT_IO",
            Self::MIXED_IO => "MIXED_IO",
            _ => "INDIRECT_IO",
        };
        write!(f, "{check} | {io}")
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct SgIoHdr {
    /// [input] Interface ID: Must be 'S'
    interface_id: i32,
    /// [input] Data Trasfer Direction
    dxfer_direction: Direction,
    /// [input] The length of bytes of the SCSI command in `cmdp`
    cmd_len: u8,
    /// [input] Max size that can be written back to the `sbp` pointer
    mx_sb_len: u8,
    /// [input] Number of scatter/gather elements in an array pointed to by `dxferp`. 0 implies s/g is not used and `dxferp` points to the data transfer buffer
    iovec_count: u16,
    /// [input] number of bytes to be moved in the data transfer associated with this command
    dxfer_len: u32,
    /// [input/output]  data transfer memory or scatter gather list
    dxferp: *mut c_void,
    /// [input] the SCSI command to execute. must be `cmd_bytes` long. This memory is read-only.
    cmdp: *mut u8,
    /// [output] sense buffer memory (SCSI error information) of at most `mx_sb_len` bytes long
    sbp: *mut u8,
    /// [input] timeout in milliseconds. u32::MAX for no timeout
    timeout: u32,
    /// [input] SCSI flags
    flags: Flags,
    /// [input] user-provided command id that will be present in the response to help matching requests in a queue
    pack_id: i32,
    /// [input] user-provided pointer to something that you might need in the response (to hold some state information)
    usr_ptr: *mut c_void,
    /// [output] SCSI-standard status byte. Bits 0,6,and 7 can contain vendor information
    status: u8,
    /// [output] `status`, except (status & 0x3e) >> 1). So, stripped of vendor info to match Linux status code
    masked_status: u8,
    /// [output] "messaging level". Most modern chipsets hide this and will return zero.
    msg_status: u8,
    /// [output] The actual number of bytes written to the `sbp`. Will always be <= `mx_sb_len`
    sb_len_wr: u8,
    /// [output] errors from the host adapter
    host_status: u16,
    /// [output] errors from the software driver
    driver_status: u16,
    /// [output] Data transfer length residual, dxfer_len - number of bytes actually transfered. Only reports underruns. Apparently some adapters report an incorrect number so you shouldn't trust this by default.
    resid: i32,
    /// [output] duration in milliseconds from the SCSI command being sent until when sg was informed it completed
    duration: u32,
    /// [output] A bunch of flags for useful info
    info: Info,
}

/// The SG_IO request code, from `<scsi/sg.h>`.
/// This predates the `_IOC` encoding convention, so it's a fixed
/// literal rather than something built from direction/size bits,
/// hence the "bad" ioctl flavor below.
const SG_IO: u16 = 0x2285;
/// Set and read back the per-fd reserved buffer, which caps a single transfer
const SG_SET_RESERVED_SIZE: u16 = 0x2275;
const SG_GET_RESERVED_SIZE: u16 = 0x2272;

ioctl_readwrite_bad!(sg_io, SG_IO, SgIoHdr);
ioctl_write_ptr_bad!(sg_set_reserved_size, SG_SET_RESERVED_SIZE, i32);
ioctl_read_bad!(sg_get_reserved_size, SG_GET_RESERVED_SIZE, i32);

/// What to ask the reserved buffer to be, in bytes
const WANTED_RESERVED_SIZE: i32 = 512 * 1024;

/// A SCSI-Generic Nikon Scanner
pub struct SgTransport {
    /// /dev/sg* file
    file: File,
    max_transfer: u32,
}

impl SgTransport {
    /// Open a /dev/sg* as an SgDevice
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let fd = file.as_raw_fd();

        // Best effort: the kernel clamps to what the queue can carry, and a refusal just leaves the default in place
        let mut reserved = 0i32;
        unsafe {
            let _ = sg_set_reserved_size(fd, &WANTED_RESERVED_SIZE);
            sg_get_reserved_size(fd, &mut reserved)
                .map_err(|e| io::Error::other(format!("SG_GET_RESERVED_SIZE: {e}")))?;
        }
        let max_transfer = reserved.max(0) as u32;
        debug!(max_transfer, "Reserved buffer");

        Ok(Self { file, max_transfer })
    }
}

// --- Implementation of SCSI transport

impl Transport for SgTransport {
    #[instrument(skip_all, fields(cdb = ?cdb, ?data))]
    fn execute(&mut self, cdb: &[u8], data: Data, timeout: Duration) -> Result<Completion, Error> {
        // A trace comparable with the NKDSBP2 proxy logs, so a session can be
        // diffed against a Nikon Scan capture command for command
        if tracing::enabled!(target: "nkscan::cdb", Level::TRACE) {
            let hex = |b: &[u8]| {
                b.iter()
                    .map(|x| format!("{x:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            match &data {
                Data::Out(out) => {
                    trace!(target: "nkscan::cdb", "CDB ({} bytes): {}\n  DATA-OUT ({} bytes): {}",
                        cdb.len(), hex(cdb), out.len(), hex(out))
                }
                _ => trace!(target: "nkscan::cdb", "CDB ({} bytes): {}", cdb.len(), hex(cdb)),
            }
        }

        let mut cmd = cdb.to_vec();
        let mut sb = [0u8; SENSE_REQUEST_LEN];

        let (dir, data, data_len) = match data {
            Data::None => (Direction::None, null_mut(), 0),
            Data::In(x) => (Direction::FromDev, x.as_mut_ptr() as *mut c_void, x.len()),
            Data::Out(x) => (Direction::ToDev, x.as_ptr() as *mut c_void, x.len()),
        };

        let mut hdr = SgIoHdr {
            interface_id: b'S' as i32,
            dxfer_direction: dir,
            cmd_len: cmd.len() as u8,
            mx_sb_len: sb.len() as u8,
            iovec_count: 0,
            dxfer_len: data_len as u32,
            dxferp: data,
            cmdp: cmd.as_mut_ptr(),
            sbp: sb.as_mut_ptr(),
            timeout: timeout.as_millis().min(u32::MAX as u128) as u32,
            flags: Flags::empty(),
            pack_id: 0,
            usr_ptr: null_mut(),
            status: 0,
            masked_status: 0,
            msg_status: 0,
            sb_len_wr: 0,
            host_status: 0,
            driver_status: 0,
            resid: 0,
            duration: 0,
            info: Info(0),
        };

        // SAFETY: `self.file` is an open file, so `as_raw_fd()` gives a valid fd for the
        // duration of this call. `hdr` is a fully-initialized `SgIoHdr` living on this
        // stack frame. Its `cmdp`/`sbp`/`dxferp` pointers come from `cmd`, `sb`, and
        // `data`, all of which outlive the call (they're not touched again until after
        // it returns), and `cmd_len`/`mx_sb_len`/`dxfer_len` are set from those same
        // buffers' actual lengths, so the kernel can't read or write past them.
        unsafe { sg_io(self.file.as_raw_fd(), &mut hdr) }.map_err(io::Error::from)?;

        trace!(
            host_status = ?HostStatus::from(hdr.host_status),
            driver_status = hdr.driver_status,
            duration_ms = hdr.duration,
            info = ?hdr.info,
            "SG_IO completed"
        );

        // Handle the bus-level faults first
        match hdr.host_status.into() {
            HostStatus::Timeout => return Err(Error::Timeout(timeout)),
            HostStatus::Ok => (),
            x => return Err(io::Error::other(format!("SCSI bus fault: {x:?}")).into()),
        }
        let status = Status::from(hdr.status);

        // `sb_len_wr` is the kernel's count of sense bytes it wrote, bounded by
        // `mx_sb_len`, but clamp it anyway so a corrupt count cannot index past
        // the buffer
        let sn = (hdr.sb_len_wr as usize).min(sb.len());

        if sn > 0 && sn < 14 {
            warn!(sb = ?&sb[..sn], sb_len_wr = hdr.sb_len_wr, "short sense buffer");
        }

        // If the status implies there should be sense data but we didn't get any, error
        if status == Status::CheckCondition && sn < 14 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("CHECK CONDITION with {sn} bytes of sense, need 14"),
            )
            .into());
        }

        // Unpack sense data if we have it
        let sense = if sn >= 14 {
            // Everything past byte 13 is unexplored. `firewire-sbp2` repacks
            // SBP-2's status quadlets into a synthetic 70h buffer, and how much
            // of the original survives is unknown. In particular whether
            // Nikon's 4th tuple element (TSC) lands anywhere, and whether bytes
            // 15-17 carry the sense-key-specific field, which is a progress
            // indicator under NOT READY and a field pointer under ILLEGAL
            // REQUEST. Log the whole buffer until that is settled.
            trace!(
                sb_len_wr = hdr.sb_len_wr,
                response_code = format!("{:#04x}", sb[0]),
                additional_length = sb[7],
                tail = ?&sb[14..sn],
                raw = ?&sb[..sn],
                "sense"
            );
            // Nikon's 4th tuple element, in SBP-2 quadlet 5's
            // sense_key-dependent field, which `firewire-sbp2` repacks to
            // bytes 15-17. Confirmed on an LS-9000: two unit attentions
            // identical in key/ASC/ASCQ differed only here, and both
            // 02h-3Ah-00h-01h and 02h-04h-01h-01h matched 2-1-2 exactly.
            // Note SKSV (bit 7) is unset, so this is not SPC sense-key
            // specific: Nikon is using the vendor half of the field. A
            // minimal fixed-format buffer is 14 bytes and stops before
            // this field, so only read it when the kernel wrote that far
            let tsc = (sn >= 16).then_some(sb[15]);
            Some(sense_from_fixed(&sb[..sn], tsc))
        } else {
            None
        };

        // `resid` is `dxfer_len` minus what actually moved, and sg warns some
        // adapters report it incorrectly, so only an underrun is trusted
        let transferred = data_len.saturating_sub(hdr.resid.max(0) as usize);

        Ok(Completion {
            status,
            sense,
            transferred,
        })
    }

    fn max_transfer(&self) -> usize {
        self.max_transfer as usize
    }
}
