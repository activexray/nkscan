//! Unification of all the different transports to actually present the user

#[cfg(target_os = "linux")]
use crate::transport::linux::SgTransport;
use crate::{
    error::Error,
    protocol::{caps::identity::Identity, cdbs::Inquiry, model::Model},
    session::Session,
    transport::{self, Data, Status, Transport, usb::UsbTransport},
};
use nusb::MaybeFuture;
/// Only the two platforms with a SCSI node to open name one
#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::path::PathBuf;
use std::{
    fmt,
    str::FromStr,
    thread::sleep,
    time::{Duration, Instant},
};
use tracing::{debug, warn};

/// Asking a device who it is should never take long
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often to check whether a reset device has reappeared
const REENUMERATE_POLL: Duration = Duration::from_millis(200);

/// How long a reset device gets to reappear before this gives up. A bus
/// reset does not reappear on any fixed schedule, so this polls for it
/// rather than guessing a single sleep
const REENUMERATE_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a scanner is
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attach {
    Usb {
        /// nusb's bus identifier
        bus: String,
        /// Hub port chain, which survives a replug into the same port
        ports: Vec<u8>,
    },
    #[cfg(target_os = "linux")]
    Sg(PathBuf), // /dev/sgN
    #[cfg(target_os = "windows")]
    Scanner(PathBuf), // \\.\ScannerN
}

impl fmt::Display for Attach {
    /// The form `list` prints and a [`Selector`] accepts
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Attach::Usb { bus, ports } => {
                let chain: Vec<_> = ports.iter().map(u8::to_string).collect();
                write!(f, "usb:{bus}-{}", chain.join("."))
            }
            #[cfg(target_os = "linux")]
            Attach::Sg(p) => write!(f, "{}", p.display()),
            #[cfg(target_os = "windows")]
            Attach::Scanner(p) => write!(f, "{}", p.display()),
        }
    }
}

/// A scanner this library can drive
#[derive(Debug, Clone)]
pub struct Device {
    pub attach: Attach,
    /// From standard INQUIRY, absent for a unit we could not open
    pub identity: Option<Identity>,
    /// Which scanner this is, from the product ID where it is on USB and from
    /// the INQUIRY answer otherwise. A USB unit another process is holding
    /// still has one
    pub model: Option<Model>,
}

impl Device {
    /// What to show a person
    pub fn name(&self) -> String {
        match (&self.identity, self.model) {
            (Some(id), _) => format!("{} {}", id.vendor, id.product),
            // Claiming is exclusive, so a unit in use answers nothing. The bus
            // still says which model it is
            (None, Some(model)) => format!("{} (in use)", model.name()),
            (None, None) => "(in use)".into(),
        }
    }

    /// Open one of the devices we found and box it into a transport.
    /// This trait object needs to be object-safe and Send, which it is
    pub fn open(&self) -> Result<Box<dyn Transport>, Error> {
        let io = |e: std::io::Error| Error::Transport(e.into());
        match &self.attach {
            Attach::Usb { bus, ports } => {
                let info = usb_devices()
                    .into_iter()
                    .find(|d| d.bus_id() == bus && d.port_chain() == ports)
                    .ok_or(Error::NotFound)?;
                Ok(Box::new(UsbTransport::open(info).map_err(io)?))
            }
            #[cfg(target_os = "linux")]
            Attach::Sg(path) => Ok(Box::new(SgTransport::open(path).map_err(io)?)),
            #[cfg(target_os = "windows")]
            Attach::Scanner(path) => Ok(Box::new(
                crate::transport::windows::ScsiScanDevice::open(path).map_err(io)?,
            )),
        }
    }
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:<20} {}", self.name(), self.attach)
    }
}

/// Ask a USB scanner to reset itself at the bus level, forcing it to
/// re-enumerate. A no-op everywhere else, since there is nothing here to
/// reset the same way
///
/// A unit an earlier command left mid-transaction sometimes stops answering
/// its bulk endpoints at all - the software equivalent of unplugging it,
/// worth trying once before asking for a real power cycle
pub fn reset(device: &Device) -> Result<(), Error> {
    let io = |e: std::io::Error| Error::Transport(e.into());
    let Attach::Usb { bus, ports } = &device.attach else {
        return Ok(());
    };
    let info = usb_devices()
        .into_iter()
        .find(|d| d.bus_id() == bus && d.port_chain() == ports)
        .ok_or(Error::NotFound)?;
    UsbTransport::reset(info).map_err(io)
}

/// Open a session against `device`, retried once with a [`reset`] if the
/// very first command times out
///
/// A unit an earlier command left mid-transaction sometimes stops answering
/// its bulk endpoints at all, which reads as that first command timing out
/// before a session even exists. A reset is not guaranteed to clear a unit
/// that is genuinely wedged, but it is worth one try before asking the
/// operator for a power cycle
pub fn connect(device: &Device) -> Result<Session, Error> {
    match Session::open(device.open()?) {
        Err(Error::Transport(transport::Error::Timeout(_))) => {
            warn!("the scanner did not answer - resetting the USB connection and trying once more");

            // A device mid-reset can lose the connection reset() is itself
            // using out from under it - that races the same disconnect this
            // is about to poll through, not a reason to give up early
            if let Err(e) = reset(device) {
                debug!(%e, "reset itself did not confirm, still waiting for the device to come back");
            }

            let deadline = Instant::now() + REENUMERATE_TIMEOUT;
            let reappeared = loop {
                if let Some(found) = list().into_iter().find(|d| d.attach == device.attach) {
                    break found;
                }
                if Instant::now() >= deadline {
                    return Err(Error::NotFound);
                }
                sleep(REENUMERATE_POLL);
            };
            Session::open(reappeared.open()?)
        }
        other => other,
    }
}

/// List all the devices this library thinks it can drive
pub fn list() -> Vec<Device> {
    let mut found: Vec<Device> = usb_devices()
        .into_iter()
        .map(|info| Device {
            attach: Attach::Usb {
                bus: info.bus_id().to_string(),
                ports: info.port_chain().to_vec(),
            },
            model: Model::from_usb(info.vendor_id(), info.product_id()),
            // Claiming is exclusive, so a unit another process holds probes as
            // `None` rather than dropping out of the list
            identity: UsbTransport::open(info)
                .ok()
                .and_then(|mut t| probe(&mut t)),
        })
        .collect();
    found.extend(scsi_devices());
    found
}

/// The USB units, filtered on vendor and product before anything is opened
///
/// Which product IDs are scanners is this layer's business rather than the
/// transport's, whose job stops at moving bytes
fn usb_devices() -> Vec<nusb::DeviceInfo> {
    let all = match nusb::list_devices().wait() {
        Ok(all) => all,
        Err(e) => {
            debug!(%e, "could not enumerate USB");
            return Vec::new();
        }
    };
    all.filter(|dev| Model::from_usb(dev.vendor_id(), dev.product_id()).is_some())
        .collect()
}

/// Ask a transport who it is, and nothing else
fn probe(transport: &mut dyn Transport) -> Option<Identity> {
    let cmd = Inquiry::standard();
    let mut buf = vec![0u8; cmd.allocation_length()];
    let completion = transport
        .execute(&cmd.cdb(), Data::In(&mut buf), PROBE_TIMEOUT)
        .ok()?;
    if completion.status != Status::Good {
        return None;
    }
    buf.truncate(completion.transferred);
    Identity::parse(&buf).ok().filter(Identity::is_scanner)
}

#[cfg(target_os = "linux")]
fn scsi_devices() -> Vec<Device> {
    use crate::transport::linux::SgTransport;

    let Ok(entries) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|path| {
            path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("sg") && n.len() > 2 && n[2..].bytes().all(|b| b.is_ascii_digit())
            })
        })
        .filter_map(|path| {
            // Most of these are disks and optical drives. Opening one is
            // harmless, and the INQUIRY tells us to leave it alone.
            let mut transport = SgTransport::open(&path).ok()?;
            let identity = probe(&mut transport)?;
            Some(Device {
                attach: Attach::Sg(path),
                model: identity.model(),
                identity: Some(identity),
            })
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn scsi_devices() -> Vec<Device> {
    use crate::transport::windows::ScsiScanDevice;

    // The class driver numbers them from zero, so stop at the first that will
    // not open rather than probing a fixed range
    (0..)
        .map(|n| PathBuf::from(format!(r"\\.\Scanner{n}")))
        .map_while(|path| {
            let mut transport = ScsiScanDevice::open(&path).ok()?;
            Some((path, probe(&mut transport)))
        })
        .filter_map(|(path, identity)| {
            let identity = identity?;
            Some(Device {
                attach: Attach::Scanner(path),
                model: identity.model(),
                identity: Some(identity),
            })
        })
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn scsi_devices() -> Vec<Device> {
    Vec::new()
}

/// Which scanner a caller means
///
/// Whatever `list` prints in its location column is a valid selector, so there
/// is no translation between what you see and what you type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// Nothing given: the only scanner, if there is exactly one
    Only,
    /// An exact location, as [`Attach`] displays it
    Location(String),
}

impl FromStr for Selector {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        Ok(if s.is_empty() {
            Selector::Only
        } else {
            Selector::Location(s.to_string())
        })
    }
}

impl Selector {
    /// Pick the one device this refers to
    pub fn resolve<'a>(&self, devices: &'a [Device]) -> Result<&'a Device, SelectError> {
        let matches: Vec<&Device> = match self {
            Selector::Only => devices.iter().collect(),
            Selector::Location(loc) => devices
                .iter()
                .filter(|d| d.attach.to_string().eq_ignore_ascii_case(loc))
                .collect(),
        };
        match matches.as_slice() {
            [one] => Ok(one),
            [] => Err(SelectError::NotFound),
            many => Err(SelectError::Ambiguous(
                many.iter().map(|d| d.attach.to_string()).collect(),
            )),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SelectError {
    #[error("no scanner matched")]
    NotFound,
    #[error("more than one scanner matched: {}", .0.join(", "))]
    Ambiguous(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::caps::identity::SCANNER;

    fn usb(bus: &str, ports: &[u8], product: &str) -> Device {
        Device {
            attach: Attach::Usb {
                bus: bus.into(),
                ports: ports.to_vec(),
            },
            identity: Some(Identity {
                qualifier: 0,
                device_type: SCANNER,
                removable: true,
                ansi_version: 2,
                vendor: "Nikon".into(),
                product: product.into(),
                revision: "1.00".into(),
            }),
            model: Model::from_product(product),
        }
    }

    /// Two of the same model plus one of another, which is the case the
    /// topological location exists for: `iSerialNumber` is 0 on every USB unit
    fn attached() -> Vec<Device> {
        vec![
            usb("1", &[3, 2], "LS-5000 ED"),
            usb("1", &[4], "LS-5000 ED"),
            usb("2", &[1], "LS-9000 ED"),
        ]
    }

    /// What `list` prints has to be what a selector accepts, or there is a
    /// translation step for a user to get wrong
    #[test]
    fn identical_models_are_told_apart_by_port() {
        let devices = attached();
        assert_eq!(devices[0].attach.to_string(), "usb:1-3.2");

        let picked = "usb:1-4".parse::<Selector>().unwrap().resolve(&devices);
        assert_eq!(picked.unwrap().attach, devices[1].attach);
    }

    /// Never guess: no match and several matches are both errors
    #[test]
    fn no_selector_needs_exactly_one_scanner() {
        assert!(Selector::Only.resolve(&attached()[..1]).is_ok());
        assert!(matches!(
            Selector::Only.resolve(&attached()),
            Err(SelectError::Ambiguous(_))
        ));
        assert!(matches!(
            Selector::Only.resolve(&[]),
            Err(SelectError::NotFound)
        ));
        assert!(matches!(
            "usb:9-9".parse::<Selector>().unwrap().resolve(&attached()),
            Err(SelectError::NotFound)
        ));
    }
}
