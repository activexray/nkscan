//! Command descriptor blocks
//!
//! Payloads live elsewhere: the SET WINDOW descriptor in `window.rs`, READ and
//! SEND data records and the SET PARAMETER block both in `data.rs`

/// The control byte both specs give as 0
///
/// Bit 7 is vendor specific in SCSI, and Nikon Scan sets it on INQUIRY with
/// EVPD, SET WINDOW, GET WINDOW and READ, on every one of those in the whole
/// capture corpus, and on nothing else. It is not decoration: a SET WINDOW for
/// the infrared window is refused with `05h-24h`, invalid field in CDB, until
/// it is set.
const VENDOR: u8 = 0x80;

/// INQUIRY, 2-2
///
/// EVPD 0 asks for the standard INQUIRY data
/// EVPD 1 asks for the VPD page named in byte 2.
/// Returns CHECK CONDITION only when the unit cannot produce what was asked for
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inquiry {
    /// None for the standard data, Some(code) for a VPD page
    page: Option<u8>,
    /// How many bytes to make room for
    allocation_length: u8,
}

impl Inquiry {
    /// The standard INQUIRY data. 36 is the spec's recommended allocation
    pub fn standard() -> Self {
        Self {
            page: None,
            allocation_length: 36,
        }
    }

    /// A Vital Product Data page
    pub fn vpd(page: u8) -> Self {
        Self {
            page: Some(page),
            allocation_length: u8::MAX,
        }
    }

    /// How large a buffer the data phase needs
    pub fn allocation_length(&self) -> usize {
        self.allocation_length as usize
    }

    pub fn cdb(&self) -> [u8; 6] {
        [
            0x12,
            // Byte 1: LUN and reserved are zero, bit 0 is EVPD
            self.page.is_some() as u8,
            // Byte 2: only meaningful when EVPD is set
            self.page.unwrap_or(0),
            0,
            self.allocation_length,
            // Nikon Scan sets it on the vendor pages and not on standard
            // INQUIRY, which is the only place the corpus distinguishes the two
            match self.page {
                Some(_) => VENDOR,
                None => 0,
            },
        ]
    }
}

#[derive(Debug)]
/// TEST UNIT READY, 2-1
///
/// No fields: both units are single-LUN (1-1-3-1), and a nonzero LUN is
/// answered with 05h-25h LOGICAL UNIT NOT SUPPORTED
pub struct TestUnitReady;

impl TestUnitReady {
    pub fn cdb(&self) -> [u8; 6] {
        [0; 6]
    }
}

#[derive(Debug)]
/// RESERVE UNIT, 2-4
///
/// Gain exclusive control until we ReleaseUnit
pub struct ReserveUnit;

impl ReserveUnit {
    pub fn cdb(&self) -> [u8; 6] {
        [0x16, 0, 0, 0, 0, 0]
    }
}

#[derive(Debug)]
/// RELEASE UNIT, 2-5
///
///  Only the initiator holding the reservation can release it
pub struct ReleaseUnit;

impl ReleaseUnit {
    pub fn cdb(&self) -> [u8; 6] {
        [0x17, 0, 0, 0, 0, 0]
    }
}

/// Which copy of a mode page to report, 2-6-2
///
/// Saved values (3) are refused with SAVING PARAMETERS NOT SUPPORTED, so there is no variant for them
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageControl {
    /// Current mode
    Current = 0,
    /// Masks the variables you're allowed to change
    Variable = 1,
    /// What a power-on restores
    Default = 2,
}

#[derive(Debug)]
/// MODE SENSE (6), 2-6
///
/// Measurement Units (03h) is the only page either unit implements, so 3Fh "all pages" returns the same thing
pub struct ModeSense {
    page: u8,
    control: PageControl,
}

impl ModeSense {
    pub fn new(page: u8, control: PageControl) -> Self {
        Self { page, control }
    }

    /// Header, block descriptor and page come to 20 bytes at most
    pub fn allocation_length(&self) -> usize {
        20
    }

    pub fn cdb(&self) -> [u8; 6] {
        [
            0x1A,
            // PF is bit 4 and must be set. DBD is bit 3, left unset so the
            // reply carries the block descriptor, eight fixed bytes, and
            // the parser skips whatever length is reported anyway
            0x10,
            (self.control as u8) << 6 | self.page,
            0,
            self.allocation_length() as u8,
            0,
        ]
    }
}

#[derive(Debug)]
/// MODE SELECT (6), 2-3
pub struct ModeSelect {
    parameter_list_length: u8,
}

impl ModeSelect {
    pub fn new(parameter_list_length: u8) -> Self {
        Self {
            parameter_list_length,
        }
    }

    pub fn cdb(&self) -> [u8; 6] {
        // PF (bit 4) must be set, SP (bit 0) must be unset: neither unit can
        // save pages, and asking gets common error 1
        [0x15, 0x10, 0, 0, self.parameter_list_length, 0]
    }
}

#[derive(Debug)]
/// From the spec 2-10
pub struct GetWindow {
    /// `None` asks for every window the unit has defined or defaulted;
    /// `Some(id)` asks for one. Ids are 0 default, 1 R, 2 G, 3 B, 4 neutral gray
    window: Option<u8>,
    transfer_length: u32,
}

impl GetWindow {
    /// Every window the unit has defined or defaulted
    pub fn all(transfer_length: u32) -> Self {
        Self {
            window: None,
            transfer_length,
        }
    }

    /// One window by identifier
    pub fn single(window: u8, transfer_length: u32) -> Self {
        Self {
            window: Some(window),
            transfer_length,
        }
    }

    pub fn allocation_length(&self) -> usize {
        self.transfer_length as usize
    }

    pub fn cdb(&self) -> [u8; 10] {
        debug_assert!(
            self.transfer_length <= 0xFF_FFFF,
            "GET WINDOW length is 24 bits"
        );
        let [_, hi, mid, lo] = self.transfer_length.to_be_bytes();
        [
            0x25,
            self.window.is_some() as u8,
            0,
            0,
            0,
            self.window.unwrap_or(0),
            hi,
            mid,
            lo,
            VENDOR,
        ]
    }
}

#[derive(Debug)]
/// SCAN, 2-7
///
/// Trigger a scan.
/// The data out is the window identifier list, one byte each, and 2-7 gives the length as [0..4]
pub struct Scan {
    windows: u8,
}

impl Scan {
    pub fn new(windows: u8) -> Self {
        Self { windows }
    }

    pub fn cdb(&self) -> [u8; 6] {
        [0x1B, 0, 0, 0, self.windows, 0]
    }
}

#[derive(Debug)]
/// Table 2-11-1
/// READ transfers data from the unit to us
pub struct Read {
    /// "Data type code"
    dtc: u8,
    /// "Data type qualifier": color element up top, element width below
    dtq: u16,
    /// Blocks, and a block is a byte here
    transfer_length: u32,
}

impl Read {
    pub fn new(dtc: u8, color: u8, width_code: u8, transfer_length: u32) -> Self {
        Self {
            dtc,
            dtq: u16::from(color) << 8 | u16::from(width_code),
            transfer_length,
        }
    }

    pub fn allocation_length(&self) -> usize {
        self.transfer_length as usize
    }

    pub fn cdb(&self) -> [u8; 10] {
        debug_assert!(self.transfer_length <= 0xFF_FFFF, "READ length is 24 bits");
        let [_, hi, mid, lo] = self.transfer_length.to_be_bytes();
        let [dtq_hi, dtq_lo] = self.dtq.to_be_bytes();
        [0x28, 0, self.dtc, 0, dtq_hi, dtq_lo, hi, mid, lo, VENDOR]
    }
}

#[derive(Debug)]
/// Table 2-12-1
/// SEND command transfered the data from initiator to the unit
pub struct Send {
    /// "Data type code"
    dtc: u8,
    /// "Data type qualifier"
    dtq: u16,
    /// Transfer length: u32
    transfer_length: u32,
}

impl Send {
    pub fn new(dtc: u8, color: u8, width_code: u8, transfer_length: u32) -> Self {
        Self {
            dtc,
            dtq: u16::from(color) << 8 | u16::from(width_code),
            transfer_length,
        }
    }

    pub fn cdb(&self) -> [u8; 10] {
        debug_assert!(self.transfer_length <= 0xFF_FFFF, "SEND length is 24 bits");
        let [_, hi, mid, lo] = self.transfer_length.to_be_bytes();
        let [dtq_hi, dtq_lo] = self.dtq.to_be_bytes();
        [0x2A, 0, self.dtc, 0, dtq_hi, dtq_lo, hi, mid, lo, 0]
    }
}

#[derive(Debug)]
/// SEND DIAGNOSTIC, 2-8
///
/// The self-test with no parameter list, which is the only form either unit
/// supports. After an operation activation command has failed, this is what
/// reports the concrete fault behind a generic `02h-04h-02h`
pub struct SendDiagnostic;

impl SendDiagnostic {
    pub fn cdb(&self) -> [u8; 6] {
        // Byte 1: PF unset, SelfTest set. DevOfL and UnitOfL must both be unset
        [0x1D, 0x04, 0, 0, 0, 0]
    }
}

#[derive(Debug)]
/// ABORT 2-13-1
///
/// Aborts a scanning operation started by SCAN
pub struct Abort;

impl Abort {
    pub fn cdb(&self) -> [u8; 6] {
        [0xC0, 0, 0, 0, 0, 0]
    }
}

#[derive(Debug)]
/// SET PARAMETER 2-15-1
///
/// Loads the parameters for the operation EXECUTE will then run. Byte 2 is the
/// operation, not the opcode
pub struct SetParameter {
    operation: u8,
    parameter_length: u32,
}

impl SetParameter {
    pub fn new(operation: u8, parameter_length: u32) -> Self {
        Self {
            operation,
            parameter_length,
        }
    }

    pub fn cdb(&self) -> [u8; 10] {
        debug_assert!(
            self.parameter_length <= 0xFF_FFFF,
            "SET PARAMETER length is 24 bits"
        );
        let [_, hi, mid, lo] = self.parameter_length.to_be_bytes();
        [0xE0, 0, self.operation, 0, 0, 0, hi, mid, lo, 0]
    }
}

#[derive(Debug)]
/// GET PARAMETER 2-16-1
///
/// Reads back the current settings of an operation, in the same shape SET
/// PARAMETER writes them. Byte 2 is the operation, not the opcode
pub struct GetParameter {
    operation: u8,
    parameter_length: u32,
}

impl GetParameter {
    pub fn new(operation: u8, parameter_length: u32) -> Self {
        Self {
            operation,
            parameter_length,
        }
    }

    pub fn allocation_length(&self) -> usize {
        self.parameter_length as usize
    }

    pub fn cdb(&self) -> [u8; 10] {
        debug_assert!(
            self.parameter_length <= 0xFF_FFFF,
            "GET PARAMETER length is 24 bits"
        );
        let [_, hi, mid, lo] = self.parameter_length.to_be_bytes();
        [0xE1, 0, self.operation, 0, 0, 0, hi, mid, lo, 0]
    }
}

#[derive(Debug)]
/// EXECUTE 2-14-1
///
/// Perform the operation specified by SET PARAMETER
pub struct Execute;

impl Execute {
    pub fn cdb(&self) -> [u8; 6] {
        [0xC1, 0, 0, 0, 0, 0]
    }
}

#[derive(Debug)]
/// From the spec 2-9
pub struct SetWindow {
    transfer_length: u32,
}

impl SetWindow {
    /// However many bytes of header plus descriptors are going out
    pub fn new(transfer_length: u32) -> Self {
        Self { transfer_length }
    }

    pub fn cdb(&self) -> [u8; 10] {
        debug_assert!(
            self.transfer_length <= 0xFF_FFFF,
            "SET WINDOW length is 24 bits"
        );
        let [_, hi, mid, lo] = self.transfer_length.to_be_bytes();
        [0x24, 0, 0, 0, 0, 0, hi, mid, lo, VENDOR]
    }
}
