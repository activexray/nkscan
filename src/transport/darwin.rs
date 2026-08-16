//! SCSI transport on macOS, via IOKit's SCSITaskDeviceInterface
//!
//! IOSCSIArchitectureModelFamily publishes a SCSITaskUserClient for any SCSI
//! peripheral no in-kernel driver claims, which is every scanner. This is how
//! FireWire units reach us on a modern Mac: the ASFireWire dext's synthetic
//! HBA turns SBP-2 into ordinary SAM targets, and we look the same to it as
//! any other initiator.
//!
//! The interface is COM-shaped (CFPlugIn), so the vtables below are transcribed
//! from `IOKit/scsi/SCSITaskLib.h` and must match it field for field.

use super::{Completion, Data, Error, Status, Transport, sense_from_fixed};
use core_foundation_sys::uuid::{CFUUIDGetConstantUUIDWithBytes, CFUUIDGetUUIDBytes, CFUUIDRef};
use io_kit_sys::{
    IOIteratorNext, IOObjectRelease, IORegistryEntryGetRegistryEntryID, IORegistryEntryIDMatching,
    IOServiceGetMatchingService, IOServiceGetMatchingServices, IOServiceMatching,
    ret::{kIOReturnSuccess, kIOReturnTimeout},
    types::io_service_t,
};
use std::{ffi::c_void, io, ptr, time::Duration};
use tracing::{debug, instrument};

/// `kSCSIDataTransfer_*` from SCSITask.h
const TRANSFER_FROM_INITIATOR: u8 = 0x01;
const TRANSFER_FROM_TARGET: u8 = 0x02;

/// `kSCSIServiceResponse_TASK_COMPLETE`: the target answered and the task
/// status byte is a real SAM status. Any other response means the status byte
/// holds a transport failure code instead
const SERVICE_RESPONSE_TASK_COMPLETE: u32 = 2;
/// The two failure codes that are timeouts: task timeout and protocol timeout
const FAILURE_TASK_TIMEOUT: u32 = 0x01;
const FAILURE_PROTOCOL_TIMEOUT: u32 = 0x02;

#[allow(non_snake_case)]
mod ffi {
    use core_foundation_sys::uuid::CFUUIDBytes;
    use io_kit_sys::types::io_service_t;
    use std::ffi::c_void;

    /// The 18-byte fixed-format `SCSI_Sense_Data` ExecuteTaskSync fills on
    /// CHECK CONDITION
    pub const SENSE_LEN: usize = 18;

    /// `SCSITaskSGElement` = `IOVirtualRange` on LP64: both fields 64-bit
    #[repr(C)]
    pub struct SGElement {
        pub address: u64,
        pub length: u64,
    }

    /// `IOCFPlugInInterface`: IUnknown, version pair, then three methods we
    /// never call
    #[repr(C)]
    pub struct IOCFPlugInInterface {
        pub _reserved: *mut c_void,
        pub QueryInterface: unsafe extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> i32,
        pub AddRef: unsafe extern "C" fn(*mut c_void) -> u32,
        pub Release: unsafe extern "C" fn(*mut c_void) -> u32,
        pub version: u16,
        pub revision: u16,
        pub Probe: *const c_void,
        pub Start: *const c_void,
        pub Stop: *const c_void,
    }

    /// `SCSITaskDeviceInterface` from SCSITaskLib.h
    #[repr(C)]
    pub struct SCSITaskDeviceInterface {
        pub _reserved: *mut c_void,
        pub QueryInterface: unsafe extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> i32,
        pub AddRef: unsafe extern "C" fn(*mut c_void) -> u32,
        pub Release: unsafe extern "C" fn(*mut c_void) -> u32,
        pub version: u16,
        pub revision: u16,
        pub IsExclusiveAccessAvailable: unsafe extern "C" fn(*mut c_void) -> u8,
        pub AddCallbackDispatcherToRunLoop: *const c_void,
        pub RemoveCallbackDispatcherFromRunLoop: *const c_void,
        pub ObtainExclusiveAccess: unsafe extern "C" fn(*mut c_void) -> i32,
        pub ReleaseExclusiveAccess: unsafe extern "C" fn(*mut c_void) -> i32,
        pub CreateSCSITask: unsafe extern "C" fn(*mut c_void) -> *mut *mut SCSITaskInterface,
    }

    /// `SCSITaskInterface` from SCSITaskLib.h. Slots this transport never
    /// calls are kept as bare pointers, but every slot must be present or the
    /// ones after it move
    #[repr(C)]
    pub struct SCSITaskInterface {
        pub _reserved: *mut c_void,
        pub QueryInterface: unsafe extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> i32,
        pub AddRef: unsafe extern "C" fn(*mut c_void) -> u32,
        pub Release: unsafe extern "C" fn(*mut c_void) -> u32,
        pub version: u16,
        pub revision: u16,
        pub IsTaskActive: *const c_void,
        pub SetTaskAttribute: *const c_void,
        pub GetTaskAttribute: *const c_void,
        pub SetCommandDescriptorBlock: unsafe extern "C" fn(*mut c_void, *const u8, u8) -> i32,
        pub GetCommandDescriptorBlockSize: *const c_void,
        pub GetCommandDescriptorBlock: *const c_void,
        pub SetScatterGatherEntries:
            unsafe extern "C" fn(*mut c_void, *mut SGElement, u8, u64, u8) -> i32,
        pub SetTimeoutDuration: unsafe extern "C" fn(*mut c_void, u32) -> i32,
        pub GetTimeoutDuration: *const c_void,
        pub SetTaskCompletionCallback: *const c_void,
        pub ExecuteTaskAsync: *const c_void,
        pub ExecuteTaskSync:
            unsafe extern "C" fn(*mut c_void, *mut [u8; SENSE_LEN], *mut u32, *mut u64) -> i32,
        pub AbortTask: *const c_void,
        pub GetSCSIServiceResponse: unsafe extern "C" fn(*mut c_void, *mut u32) -> i32,
        pub GetTaskState: *const c_void,
        pub GetTaskStatus: *const c_void,
        pub GetRealizedDataTransferCount: *const c_void,
        pub GetAutoSenseData: *const c_void,
        pub SetAutoSenseDataBuffer: *const c_void,
        pub ResetForNewTask: *const c_void,
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        pub fn IOCreatePlugInInterfaceForService(
            service: io_service_t,
            pluginType: core_foundation_sys::uuid::CFUUIDRef,
            interfaceType: core_foundation_sys::uuid::CFUUIDRef,
            theInterface: *mut *mut *mut IOCFPlugInInterface,
            theScore: *mut i32,
        ) -> i32;
        pub fn IODestroyPlugInInterface(interface: *mut *mut IOCFPlugInInterface) -> i32;
    }
}

/// `kIOSCSITaskDeviceUserClientTypeID`
fn user_client_type_id() -> CFUUIDRef {
    unsafe {
        CFUUIDGetConstantUUIDWithBytes(
            ptr::null(),
            0x7D,
            0x66,
            0x67,
            0x8E,
            0x08,
            0xA2,
            0x11,
            0xD5,
            0xA1,
            0xB8,
            0x00,
            0x30,
            0x65,
            0x7D,
            0x05,
            0x2A,
        )
    }
}

/// `kIOCFPlugInInterfaceID`
fn plugin_interface_id() -> CFUUIDRef {
    unsafe {
        CFUUIDGetConstantUUIDWithBytes(
            ptr::null(),
            0xC2,
            0x44,
            0xE8,
            0x58,
            0x10,
            0x9C,
            0x11,
            0xD4,
            0x91,
            0xD4,
            0x00,
            0x50,
            0xE4,
            0xC6,
            0x42,
            0x6F,
        )
    }
}

/// `kIOSCSITaskDeviceInterfaceID`
fn device_interface_id() -> CFUUIDRef {
    unsafe {
        CFUUIDGetConstantUUIDWithBytes(
            ptr::null(),
            0x1B,
            0xBC,
            0x41,
            0x32,
            0x08,
            0xA5,
            0x11,
            0xD5,
            0x90,
            0xED,
            0x00,
            0x30,
            0x65,
            0x7D,
            0x05,
            0x2A,
        )
    }
}

/// A SCSI peripheral held through the SCSITask user client, exclusively:
/// nothing else (VueScan included) can talk to the unit while this is open
pub struct ScsiTaskTransport {
    device: *mut *mut ffi::SCSITaskDeviceInterface,
    plugin: *mut *mut ffi::IOCFPlugInInterface,
    /// Whether [`Self::open`] got as far as taking exclusive access, so `Drop`
    /// only gives back what we hold
    exclusive: bool,
}

// The vtable calls have no thread affinity as long as they come one at a time,
// which &mut already guarantees. Only the async path (which this transport
// does not use) binds to a run loop
unsafe impl Send for ScsiTaskTransport {}

impl ScsiTaskTransport {
    /// IORegistry entry IDs of every SCSI peripheral nub currently published.
    /// Most will be disks a kernel driver owns; those refuse [`Self::open`]
    /// and fall out there
    pub fn entry_ids() -> Vec<u64> {
        let mut found = Vec::new();
        unsafe {
            let matching = IOServiceMatching(c"IOSCSIPeripheralDeviceNub".as_ptr());
            if matching.is_null() {
                return found;
            }
            let mut iter = 0;
            // Consumes `matching`
            if IOServiceGetMatchingServices(0, matching, &mut iter) != kIOReturnSuccess {
                return found;
            }
            loop {
                let service = IOIteratorNext(iter);
                if service == 0 {
                    break;
                }
                let mut id = 0u64;
                if IORegistryEntryGetRegistryEntryID(service, &mut id) == kIOReturnSuccess {
                    found.push(id);
                }
                IOObjectRelease(service);
            }
            IOObjectRelease(iter);
        }
        found
    }

    /// Open the nub with this IORegistry entry ID and take exclusive access
    pub fn open(entry_id: u64) -> io::Result<Self> {
        unsafe {
            // Consumes the matching dict
            let service = IOServiceGetMatchingService(0, IORegistryEntryIDMatching(entry_id));
            if service == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no IORegistry entry {entry_id:#x}"),
                ));
            }
            let claimed = Self::claim(service);
            IOObjectRelease(service);
            let mut transport = claimed?;
            let kr = ((**transport.device).ObtainExclusiveAccess)(transport.device.cast());
            if kr != kIOReturnSuccess {
                // Drop releases the interfaces
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    format!("could not get exclusive access: {kr:#010x}"),
                ));
            }
            transport.exclusive = true;
            debug!(entry_id, "opened SCSITask device");
            Ok(transport)
        }
    }

    /// The CFPlugIn dance: instantiate the user client and ask it for the
    /// device interface. Fails cleanly on nubs without a SCSITaskUserClient
    /// (anything a kernel driver already drives)
    unsafe fn claim(service: io_service_t) -> io::Result<Self> {
        unsafe {
            let mut plugin: *mut *mut ffi::IOCFPlugInInterface = ptr::null_mut();
            let mut score = 0i32;
            let kr = ffi::IOCreatePlugInInterfaceForService(
                service,
                user_client_type_id(),
                plugin_interface_id(),
                &mut plugin,
                &mut score,
            );
            if kr != kIOReturnSuccess || plugin.is_null() {
                return Err(io::Error::other(format!(
                    "no SCSITask user client: {kr:#010x}"
                )));
            }
            let mut device: *mut c_void = ptr::null_mut();
            let hresult = ((**plugin).QueryInterface)(
                plugin.cast(),
                CFUUIDGetUUIDBytes(device_interface_id()),
                &mut device,
            );
            if hresult != 0 || device.is_null() {
                ffi::IODestroyPlugInInterface(plugin);
                return Err(io::Error::other(format!(
                    "QueryInterface(SCSITaskDeviceInterface): {hresult:#010x}"
                )));
            }
            Ok(Self {
                device: device.cast(),
                plugin,
                exclusive: false,
            })
        }
    }
}

impl Drop for ScsiTaskTransport {
    fn drop(&mut self) {
        unsafe {
            if self.exclusive {
                ((**self.device).ReleaseExclusiveAccess)(self.device.cast());
            }
            ((**self.device).Release)(self.device.cast());
            ffi::IODestroyPlugInInterface(self.plugin);
        }
    }
}

impl Transport for ScsiTaskTransport {
    fn max_transfer(&self) -> usize {
        // Same normal-ish chunk as the other transports
        128 * 1024
    }

    #[instrument(skip_all, fields(cdb = ?cdb, ?data))]
    fn execute(&mut self, cdb: &[u8], data: Data, timeout: Duration) -> Result<Completion, Error> {
        if cdb.len() > 16 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SetCommandDescriptorBlock takes at most a 16-byte CDB",
            )
            .into());
        }

        unsafe {
            let task = ((**self.device).CreateSCSITask)(self.device.cast());
            if task.is_null() {
                return Err(io::Error::other("CreateSCSITask returned null").into());
            }
            let result = run_task(task, cdb, data, timeout);
            ((**task).Release)(task.cast());
            result
        }
    }
}

/// One command on a fresh task. Split out so the caller can release the task
/// on every path
unsafe fn run_task(
    task: *mut *mut ffi::SCSITaskInterface,
    cdb: &[u8],
    data: Data,
    timeout: Duration,
) -> Result<Completion, Error> {
    unsafe {
        let this: *mut c_void = task.cast();
        let ok = |kr: i32, what: &str| -> Result<(), Error> {
            if kr == kIOReturnSuccess {
                Ok(())
            } else {
                Err(io::Error::other(format!("{what}: {kr:#010x}")).into())
            }
        };

        ok(
            ((**task).SetCommandDescriptorBlock)(this, cdb.as_ptr(), cdb.len() as u8),
            "SetCommandDescriptorBlock",
        )?;

        // One scatter-gather entry covering the whole buffer. A command with
        // no data phase keeps the task's default of no transfer
        let mut range;
        match data {
            Data::In(buf) => {
                range = ffi::SGElement {
                    address: buf.as_mut_ptr() as u64,
                    length: buf.len() as u64,
                };
                ok(
                    ((**task).SetScatterGatherEntries)(
                        this,
                        &mut range,
                        1,
                        range.length,
                        TRANSFER_FROM_TARGET,
                    ),
                    "SetScatterGatherEntries",
                )?;
            }
            Data::Out(buf) => {
                range = ffi::SGElement {
                    address: buf.as_ptr() as u64,
                    length: buf.len() as u64,
                };
                ok(
                    ((**task).SetScatterGatherEntries)(
                        this,
                        &mut range,
                        1,
                        range.length,
                        TRANSFER_FROM_INITIATOR,
                    ),
                    "SetScatterGatherEntries",
                )?;
            }
            Data::None => {}
        }

        let ms = u32::try_from(timeout.as_millis())
            .unwrap_or(u32::MAX)
            .max(1);
        ok(
            ((**task).SetTimeoutDuration)(this, ms),
            "SetTimeoutDuration",
        )?;

        let mut sense = [0u8; ffi::SENSE_LEN];
        let mut task_status = u32::MAX;
        let mut transferred = 0u64;
        let kr = ((**task).ExecuteTaskSync)(this, &mut sense, &mut task_status, &mut transferred);
        if kr == kIOReturnTimeout {
            return Err(Error::Timeout(timeout));
        }
        ok(kr, "ExecuteTaskSync")?;

        // TASK_COMPLETE means `task_status` is SAM; anything else makes it a
        // transport failure code instead, where 1 and 2 are the timeouts
        let mut response = 0u32;
        ok(
            ((**task).GetSCSIServiceResponse)(this, &mut response),
            "GetSCSIServiceResponse",
        )?;
        if response != SERVICE_RESPONSE_TASK_COMPLETE {
            return match task_status {
                FAILURE_TASK_TIMEOUT | FAILURE_PROTOCOL_TIMEOUT => Err(Error::Timeout(timeout)),
                other => Err(io::Error::other(format!(
                    "task not delivered: response {response}, failure code {other:#x}"
                ))
                .into()),
            };
        }

        // Autosense is only filled on CHECK CONDITION; 0x70/0x71 says the
        // buffer really holds fixed-format sense
        let has_sense = matches!(sense[0] & 0x7F, 0x70 | 0x71);
        Ok(Completion {
            status: Status::from(task_status as u8),
            sense: has_sense.then(|| sense_from_fixed(&sense, None)),
            transferred: transferred as usize,
        })
    }
}
