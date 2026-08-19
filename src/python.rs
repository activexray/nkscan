//! Python bindings, gated behind the `python` feature
//!
//! A thin skin over [`session`](crate::session) and [`scan::frame`](crate::scan::frame):
//! converts arguments, hands the decoded planes to numpy without copying
//! them, and releases the interpreter for however long a call blocks on the
//! scanner.

use crate::{
    device::{self, Device as RustDevice},
    error::Error,
    protocol::{
        caps::{Capabilities as RustCapabilities, set_window::ColorInterleaving},
        data::Rect,
        decode::Samples,
        window::Channel,
    },
    scan::{
        autoexpose::Exposures,
        boundaries::Polarity,
        frame::{self, Phase},
        framing,
        pass::Progress,
        window::Recipe,
    },
    session::Session as RustSession,
};
use numpy::{IntoPyArray, PyArray2, PyArrayMethods};
use pyo3::{exceptions::PyRuntimeError, prelude::*};
use pyo3_stub_gen::{create_exception, define_stub_info_gatherer, derive::*};
use std::{collections::HashMap, ops::ControlFlow, sync::Mutex};

// ----- errors -----

create_exception!(
    nkscan,
    ScannerError,
    PyRuntimeError,
    "Base for every error this crate raises"
);
create_exception!(nkscan, TransientError, ScannerError, "Worth retrying");
create_exception!(
    nkscan,
    TransportError,
    TransientError,
    "The link to the scanner failed"
);
create_exception!(
    nkscan,
    DeviceBusy,
    TransientError,
    "Something else has the scanner"
);
create_exception!(nkscan, DeviceNotFound, ScannerError, "No such scanner");
create_exception!(
    nkscan,
    MediaError,
    ScannerError,
    "Something a person has to go fix"
);
create_exception!(
    nkscan,
    UnsupportedError,
    ScannerError,
    "This unit or adapter cannot do that. Carries `.op` and `.reason`"
);
create_exception!(
    nkscan,
    ScanCancelled,
    ScannerError,
    "A progress callback returned False"
);

impl From<Error> for PyErr {
    fn from(error: Error) -> Self {
        match error {
            Error::Transport(e) => TransportError::new_err(e.to_string()),
            Error::Busy(c) => DeviceBusy::new_err(c.to_string()),
            Error::Media(i) => MediaError::new_err(i.to_string()),
            Error::NotFound => DeviceNotFound::new_err("no such scanner"),
            Error::Unsupported { op, reason } => {
                let err = UnsupportedError::new_err(format!("{op}: {reason}"));
                Python::attach(|py| {
                    let _ = err.value(py).setattr("op", op);
                    let _ = err.value(py).setattr("reason", &reason);
                });
                err
            }
            Error::Cancelled => ScanCancelled::new_err("scan cancelled"),
            Error::Device(fault) => ScannerError::new_err(fault.to_string()),
        }
    }
}

fn closed() -> PyErr {
    ScannerError::new_err("session is closed")
}

// ----- device discovery -----

/// A scanner this library found, and can open
#[gen_stub_pyclass]
#[pyclass(name = "Device", frozen, module = "nkscan")]
pub struct PyDevice(RustDevice);

#[gen_stub_pymethods]
#[pymethods]
impl PyDevice {
    /// Where it is
    #[getter]
    fn location(&self) -> String {
        self.0.attach.to_string()
    }

    /// What to show a person
    #[getter]
    fn name(&self) -> String {
        self.0.name()
    }

    fn __repr__(&self) -> String {
        format!("Device({:?})", self.location())
    }
}

/// Every scanner this library thinks it can drive
#[gen_stub_pyfunction]
#[pyfunction]
fn list_devices() -> Vec<PyDevice> {
    device::list().into_iter().map(PyDevice).collect()
}

// ----- capabilities -----

/// What the scanner says it can do, projected down to what a caller needs to
/// place a scan. Grows as bindings turn up a real need for more of it
#[gen_stub_pyclass]
#[pyclass(name = "Capabilities", frozen, get_all, module = "nkscan")]
pub struct PyCapabilities {
    vendor: String,
    product: String,
    revision: String,
    model: Option<String>,
    x_dpi_range: (u16, u16),
    y_dpi_range: (u16, u16),
    optical_dpi: u16,
}

impl From<&RustCapabilities> for PyCapabilities {
    fn from(caps: &RustCapabilities) -> Self {
        Self {
            vendor: caps.identity.vendor.clone(),
            product: caps.identity.product.clone(),
            revision: caps.identity.revision.clone(),
            model: caps.identity.model().map(|m| m.name().to_string()),
            x_dpi_range: (
                caps.address.x_axis.dpi_range.start,
                caps.address.x_axis.dpi_range.last,
            ),
            y_dpi_range: (
                caps.address.y_axis.dpi_range.start,
                caps.address.y_axis.dpi_range.last,
            ),
            optical_dpi: caps.address.x_axis.optical_dpi,
        }
    }
}

// ----- a finished scan -----

/// What one frame's scan produced
#[gen_stub_pyclass]
#[pyclass(name = "ScanResult", frozen, get_all, module = "nkscan")]
pub struct PyScanResult {
    /// One array per captured channel, keyed by name ("red", "green", "blue", ...)
    colors: HashMap<String, Py<PyArray2<u16>>>,
    /// The infrared plane, where the recipe asked for it
    ir: Option<Py<PyArray2<u16>>>,
    dpi: u32,
    rows: usize,
    cols: usize,
    /// What the frame was exposed at, keyed the same way as `colors`
    exposures: HashMap<String, u32>,
    /// Pixels dust removal rebuilt, where asked for
    cleaned: Option<usize>,
}

fn channel_name(id: u8) -> String {
    format!("{:?}", Channel::from(id)).to_lowercase()
}

// ----- a session -----

/// An open, exclusive hold of a scanner
#[gen_stub_pyclass]
#[pyclass(name = "Session", module = "nkscan")]
pub struct PySession(Mutex<Option<RustSession>>);

impl PySession {
    fn with<T>(&self, f: impl FnOnce(&mut RustSession) -> Result<T, Error>) -> PyResult<T> {
        let mut guard = self.0.lock().expect("not poisoned");
        let session = guard.as_mut().ok_or_else(closed)?;
        Ok(f(session)?)
    }
}

fn open_device(device: &RustDevice) -> Result<PySession, Error> {
    let transport = device.open()?;
    let session = RustSession::open(transport)?;
    Ok(PySession(Mutex::new(Some(session))))
}

#[gen_stub_pymethods]
#[pymethods]
impl PySession {
    /// Start a session against whatever `list_devices` reports at this `location`
    #[new]
    fn new(py: Python<'_>, location: &str) -> PyResult<Self> {
        let location = location.to_string();
        py.detach(move || {
            let devices = device::list();
            let device = device::Selector::Location(location)
                .resolve(&devices)
                .map_err(|e| DeviceNotFound::new_err(e.to_string()))?;
            open_device(device).map_err(PyErr::from)
        })
    }

    /// Start a session against `device`
    #[staticmethod]
    fn open(py: Python<'_>, device: &PyDevice) -> PyResult<Self> {
        let dev = device.0.clone();
        py.detach(move || open_device(&dev)).map_err(PyErr::from)
    }

    /// What the scanner says it can do
    #[getter]
    fn capabilities(&self) -> PyResult<PyCapabilities> {
        self.with(|s| Ok(PyCapabilities::from(s.capabilities())))
    }

    /// Whether a holder is loaded
    fn media_loaded(&self, py: Python<'_>) -> PyResult<bool> {
        py.detach(|| self.with(RustSession::media_loaded))
    }

    /// Put the unit in the state a scan expects
    fn stage(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.with(RustSession::stage))
    }

    /// Give back whatever is loaded, answering whether the unit did anything
    fn eject(&self, py: Python<'_>) -> PyResult<bool> {
        py.detach(|| self.with(RustSession::eject))
    }

    /// Take in whatever the adapter has waiting, answering whether anything came
    fn load(&self, py: Python<'_>) -> PyResult<bool> {
        py.detach(|| self.with(RustSession::load))
    }

    /// Find every frame on whatever is loaded
    ///
    /// `format_mm` is the frame's length along the feed; only asked for by
    /// the two of four discovery mechanisms that need a thumbnail pass to
    /// find frames, so it may be left `None` on a masked or address-framed
    /// adapter. `positive` is which way the loaded film reads
    #[pyo3(signature = (format_mm=None, positive=false, progress=None))]
    fn discover_frames(
        &self,
        py: Python<'_>,
        format_mm: Option<f64>,
        positive: bool,
        progress: Option<Py<PyAny>>,
    ) -> PyResult<Vec<(u32, u32, u32, u32)>> {
        let format =
            format_mm.map(|mm| crate::protocol::caps::film::FilmFormat::Custom(mm.round() as u32));
        let polarity = if positive {
            Polarity::Positive
        } else {
            Polarity::Negative
        };

        py.detach(move || {
            self.with(|session| {
                let mut samples = Samples::default();
                let discovery =
                    framing::discover_with(session, format, polarity, &mut samples, |p| {
                        report(&progress, "discover", 0, p)
                    })?;
                Ok(discovery
                    .frames
                    .into_iter()
                    .map(|r| (r.top, r.left, r.bottom, r.right))
                    .collect())
            })
        })
    }

    /// Focus, meter, take the pass over `frame`, and optionally clean it
    ///
    /// `frame` is `(top, left, bottom, right)`, one of `discover_frames`'s. `exposures`,
    /// keyed the way `ScanResult.exposures` is, reuses an exposure already decided
    /// rather than metering this frame fresh
    #[pyo3(signature = (
        frame,
        dpi=None,
        samples=1,
        superfine=false,
        infrared=false,
        clean=false,
        lock_white_balance=true,
        exposures=None,
        progress=None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn scan_frame(
        &self,
        py: Python<'_>,
        frame: (u32, u32, u32, u32),
        dpi: Option<u16>,
        samples: u8,
        superfine: bool,
        infrared: bool,
        clean: bool,
        lock_white_balance: bool,
        exposures: Option<HashMap<String, u32>>,
        progress: Option<Py<PyAny>>,
    ) -> PyResult<PyScanResult> {
        let (top, left, bottom, right) = frame;
        let frame = Rect {
            top,
            left,
            bottom,
            right,
        };

        let locked = exposures.map(|by_name| {
            let mut e = Exposures::default();
            for (name, value) in by_name {
                e.set(channel_from_name(&name), value);
            }
            e
        });

        py.detach(move || {
            self.with(|session| {
                let interleaving = if superfine {
                    ColorInterleaving::LINE_WITHOUT_DISTANCE
                } else {
                    ColorInterleaving::MULTILINE_SIMULTANEOUS
                };
                let recipe = Recipe {
                    dpi: dpi.unwrap_or(session.capabilities().address.x_axis.optical_dpi),
                    samples,
                    interleaving,
                    infrared: infrared || clean,
                };
                recipe.supported(session.capabilities())?;

                let mut buf = Samples::default();
                let options = frame::Options {
                    exposures: locked.as_ref(),
                    lock_white_balance,
                    clean,
                };
                let scanned = frame::scan_frame_with(
                    session,
                    &recipe,
                    frame,
                    options,
                    &mut buf,
                    |phase, p| match phase {
                        Phase::Meter(pass) => report(&progress, "meter", pass, p),
                        Phase::Scan => report(&progress, "scan", 0, p),
                    },
                )?;

                let ids: Vec<u8> = scanned.pass.layout.colors().collect();
                let (rows, cols) = (scanned.pass.rows, scanned.pass.cols);

                Python::attach(|py| {
                    let colors = ids
                        .iter()
                        .zip(buf.colors)
                        .map(|(&id, plane)| {
                            let array = plane
                                .into_pyarray(py)
                                .reshape([rows, cols])
                                .expect("plane is rows * cols long")
                                .unbind();
                            (channel_name(id), array)
                        })
                        .collect();
                    let ir = buf.ir.map(|plane| {
                        plane
                            .into_pyarray(py)
                            .reshape([rows, cols])
                            .expect("plane is rows * cols long")
                            .unbind()
                    });
                    let exposures = scanned
                        .exposures
                        .iter()
                        .map(|(c, e)| (channel_name(c.id()), e))
                        .collect();

                    Ok(PyScanResult {
                        colors,
                        ir,
                        dpi: scanned.pass.layout.dpi,
                        rows,
                        cols,
                        exposures,
                        cleaned: scanned.cleaned,
                    })
                })
            })
        })
    }

    /// Drop the hold on the scanner. A closed session refuses every other method
    fn close(&self) {
        *self.0.lock().expect("not poisoned") = None;
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, _args: &Bound<'_, pyo3::types::PyTuple>) {
        self.close();
    }
}

/// Report progress on `on`, if there is one, throttled to roughly 10 updates a
/// second, and translate a `False` return into a cancel
fn report(on: &Option<Py<PyAny>>, phase: &str, pass: usize, p: Progress) -> ControlFlow<()> {
    let Some(on) = on else {
        return ControlFlow::Continue(());
    };
    Python::attach(|py| {
        let Ok(result) = on.call1(py, (phase, pass, p.bytes, p.total)) else {
            return ControlFlow::Continue(());
        };
        match result.extract::<bool>(py) {
            Ok(false) => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        }
    })
}

/// The channel a `scan_frame`/`ScanResult` name refers to
fn channel_from_name(name: &str) -> Channel {
    match name.to_lowercase().as_str() {
        "red" => Channel::Red,
        "green" => Channel::Green,
        "blue" => Channel::Blue,
        "infrared" => Channel::Infrared,
        "neutralgray" => Channel::NeutralGray,
        _ => Channel::Default,
    }
}

// ----- the module -----

#[pymodule]
#[pyo3(name = "nkscan")]
fn nkscan_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDevice>()?;
    m.add_class::<PyCapabilities>()?;
    m.add_class::<PySession>()?;
    m.add_class::<PyScanResult>()?;
    m.add_function(wrap_pyfunction!(list_devices, m)?)?;

    let py = m.py();
    m.add("ScannerError", py.get_type::<ScannerError>())?;
    m.add("TransientError", py.get_type::<TransientError>())?;
    m.add("TransportError", py.get_type::<TransportError>())?;
    m.add("DeviceBusy", py.get_type::<DeviceBusy>())?;
    m.add("DeviceNotFound", py.get_type::<DeviceNotFound>())?;
    m.add("MediaError", py.get_type::<MediaError>())?;
    m.add("UnsupportedError", py.get_type::<UnsupportedError>())?;
    m.add("ScanCancelled", py.get_type::<ScanCancelled>())?;
    Ok(())
}

define_stub_info_gatherer!(stub_info);
