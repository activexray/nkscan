//! SCSI transport on Windows, via the scanner class driver scsiscan.sys, which will be any scanner that talks via scsi

use super::{Completion, Data, Error, Status, Transport, sense_from_fixed};
use std::{io, os::windows::ffi::OsStrExt, path::Path, ptr, thread::sleep, time::Duration};
use tracing::*;
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_WORKING_SET_QUOTA, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    },
    System::IO::DeviceIoControl,
};

/// `FILE_DEVICE_SCANNER << 16 | function 4 << 2 | METHOD_OUT_DIRECT`
const IOCTL_SCSISCAN_CMD: u32 = 0x0019_0012;

/// `SCSISCAN_CMD::srb_flags` direction values
const SRB_FLAGS_NO_DATA: u32 = 0x00;
const SRB_FLAGS_DATA_IN: u32 = 0x40;
const SRB_FLAGS_DATA_OUT: u32 = 0x80;

/// The status byte the driver writes back through `srb_status`
const SRB_STATUS_SUCCESS: u8 = 0x01;
const SRB_STATUS_ERROR: u8 = 0x04;
const SRB_STATUS_BUSY: u8 = 0x05;
/// Set alongside a base status when the driver has filled the sense buffer
const SRB_STATUS_AUTOSENSE_VALID: u8 = 0x80;
/// The base status lives in the low six bits; the top two are flags
const SRB_STATUS_MASK: u8 = 0x3F;

/// How much sense to ask for
const SENSE_LENGTH: usize = 32;
const QUOTA_RETRIES: usize = 200;
const QUOTA_RETRY_DELAY: Duration = Duration::from_millis(50);
const MAX_TRANSFER: usize = 128 * 1024;

/// Microsoft's `SCSISCAN_CMD`, from `scsiscan.h`
#[repr(C)]
struct ScsiScanCmd {
    reserved1: u32,
    size: u32,
    srb_flags: u32,
    cdb_length: u8,
    sense_length: u8,
    reserved2: u8,
    reserved3: u8,
    transfer_length: u32,
    cdb: [u8; 16],
    srb_status: *mut u8,
    sense_buffer: *mut u8,
}

/// A scanner reachable through `scsiscan.sys`
pub struct ScsiScanDevice {
    handle: HANDLE,
}

// The handle is owned by this struct and every use goes through `&mut self`
unsafe impl Send for ScsiScanDevice {}

impl ScsiScanDevice {
    /// Open a scanner by device path, conventionally `\\.\Scanner0`
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: `wide` is NUL terminated and outlives the call, and the two null pointers
        // are documented as optional for security attributes and template file.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        debug!(?path, "Opened scanner");
        Ok(Self { handle })
    }
}

impl Drop for ScsiScanDevice {
    fn drop(&mut self) {
        // SAFETY: `handle` came from a successful `CreateFileW` and nothing else closes it
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

impl Transport for ScsiScanDevice {
    fn max_transfer(&self) -> usize {
        MAX_TRANSFER
    }

    // `timeout` is ignored: `SCSISCAN_CMD` has no timeout field
    #[instrument(skip_all, fields(cdb = ?cdb, ?data))]
    fn execute(&mut self, cdb: &[u8], data: Data, _timeout: Duration) -> Result<Completion, Error> {
        if cdb.len() > 16 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SCSISCAN_CMD carries at most a 16-byte CDB",
            )
            .into());
        }

        let mut srb_status = 0u8;
        let mut sb = [0u8; SENSE_LENGTH];

        let mut padded = [0u8; 16];
        padded[..cdb.len()].copy_from_slice(cdb);

        let (srb_flags, data_ptr, data_len) = match data {
            Data::None => (SRB_FLAGS_NO_DATA, ptr::null_mut(), 0usize),
            Data::In(x) => (SRB_FLAGS_DATA_IN, x.as_mut_ptr(), x.len()),
            Data::Out(x) => (SRB_FLAGS_DATA_OUT, x.as_ptr() as *mut u8, x.len()),
        };

        let mut cmd = ScsiScanCmd {
            reserved1: 0,
            size: size_of::<ScsiScanCmd>() as u32,
            srb_flags,
            cdb_length: cdb.len() as u8,
            sense_length: SENSE_LENGTH as u8,
            reserved2: 0,
            reserved3: 0,
            transfer_length: data_len as u32,
            cdb: padded,
            srb_status: &mut srb_status,
            sense_buffer: sb.as_mut_ptr(),
        };

        let mut returned = 0u32;
        let mut attempt = 0;
        loop {
            // SAFETY: `handle` is open for the life of `self`. `cmd` is fully initialized on
            // this stack frame, and its `srb_status`/`sense_buffer` pointers borrow locals that
            // outlive the call. The data buffer is passed with its own length, so the driver
            // cannot map past it. `METHOD_OUT_DIRECT` means that buffer is the *output*
            // parameter whichever way the data actually flows.
            let ok = unsafe {
                DeviceIoControl(
                    self.handle,
                    IOCTL_SCSISCAN_CMD,
                    (&raw mut cmd).cast(),
                    size_of::<ScsiScanCmd>() as u32,
                    data_ptr.cast(),
                    data_len as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            };

            if ok != 0 {
                break;
            }

            // Direct I/O could not lock the buffer
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_WORKING_SET_QUOTA as i32) || attempt >= QUOTA_RETRIES
            {
                return Err(e.into());
            }
            attempt += 1;
            debug!(attempt, "Working set quota exceeded, retrying");
            sleep(QUOTA_RETRY_DELAY);
        }

        trace!(
            srb_status = format!("0x{srb_status:02x}"),
            returned, attempt, "SCSISCAN_CMD completed"
        );

        // There is no SCSI status byte in this struct, so the status is inferred from the SRB
        let status = match srb_status & SRB_STATUS_MASK {
            SRB_STATUS_SUCCESS => Status::Good,
            SRB_STATUS_ERROR => Status::CheckCondition,
            SRB_STATUS_BUSY => Status::Busy,
            // A bus or driver level fault
            _ => {
                return Err(io::Error::other(format!(
                    "SCSI bus fault: srb_status {srb_status:#04x}"
                ))
                .into());
            }
        };

        let sense = if srb_status & SRB_STATUS_AUTOSENSE_VALID != 0 {
            debug!(sense_raw = ?sb, "raw sense buffer");
            // The driver reports no written length, so the fields are read at their
            // fixed-format offsets and the whole 32 bytes are kept verbatim
            if !matches!(sb[0] & 0x7F, 0x70 | 0x71) {
                warn!(
                    response_code = sb[0],
                    "autosense buffer is not fixed-format"
                );
            }
            // Same byte the sg path uses. Confirmed on an LS-9000: this
            // driver reports 02h-04h-01h and 06h-28h-00h each carrying
            // their documented 01h here, so both repack SBP-2 quadlet 5 to
            // bytes 15-17 the same way
            sense_from_fixed(sb, Some(sb[15]))
        } else {
            None
        };

        if status == Status::CheckCondition && sense.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "CHECK CONDITION without autosense",
            )
            .into());
        }

        Ok(Completion {
            status,
            sense,
            transferred: returned as usize,
        })
    }
}
