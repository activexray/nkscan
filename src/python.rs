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
        caps::{
            Capabilities as RustCapabilities,
            set_window::{ColorInterleaving, ScanKind},
        },
        data::{Op, Rect},
        decode::Samples,
        window::Channel,
    },
    scan::{
        autoexpose::Exposures,
        boundaries::Polarity,
        frame::{self, Phase},
        framing::{self, Framing},
        meter::Metering,
        pass::Progress,
        profile::Film,
        window::{MAX_SAMPLES, Recipe},
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

    // What follows is for a caller deciding which controls to offer. Every
    // field is read off the unit's own pages, so a control gated on one is
    // gated on what this scanner will actually accept
    /// Frames the loaded holder can hold, 0 where the unit publishes no table
    max_frames: u8,
    /// The thumbnail pass's resolution range, empty where there is no thumbnail
    thumbnail_dpi: (u16, u16),
    /// Focus positions the unit accepts
    focus_range: (u16, u16),
    /// Most readings of one line a pass may ask for
    max_samples: u8,
    /// How this unit finds frames: "published", "thumbnail", "perforation" or
    /// "address". Only the middle two take a thumbnail pass
    framing: String,
    /// Whether a thumbnail pass happens at all, and so whether a caller can
    /// offer to keep it
    thumbnail: bool,
    /// Whether the CCD can read its lines at once. False means a "superfine"
    /// control has nothing to switch
    multi_line: bool,
    /// Whether the unit can give the medium back on its own
    eject: bool,
    /// Whether the unit focuses itself
    autofocus: bool,
    /// Whether the unit runs an AE pass itself. False on every unit seen, an
    /// LS-9000 included, so metering is host-side and the white-balance
    /// controls are what decide it
    hardware_metering: bool,
    /// The reading modes this unit offers, by name
    interleavings: Vec<String>,
}

#[gen_stub_pymethods]
#[pymethods]
impl PyCapabilities {
    /// Whether this film type is metered with its channels held together
    ///
    /// The default a `lock_white_balance` control should start from. It follows
    /// the film rather than the scanner, so it is the same answer everywhere -
    /// it lives here because this is where a caller already looks
    #[staticmethod]
    fn locks_white_balance(film: &str) -> PyResult<bool> {
        let film = match film.to_ascii_lowercase().as_str() {
            "positive" | "slide" => Film::Positive,
            "negative" => Film::Negative,
            "kodachrome" => Film::Kodachrome,
            "mono" | "monochrome" | "monochromenegative" => Film::MonochromeNegative,
            other => {
                return Err(PyRuntimeError::new_err(format!(
                    "unknown film type {other:?}"
                )));
            }
        };
        Ok(Metering::locks_white_balance(film))
    }
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

            max_frames: caps.address.max_frames,
            thumbnail_dpi: (
                caps.address.thumbnail_resolution.start,
                caps.address.thumbnail_resolution.last,
            ),
            focus_range: (
                caps.address.focus_range.start,
                caps.address.focus_range.last,
            ),
            max_samples: MAX_SAMPLES,
            framing: match framing::Framing::choose(caps) {
                Framing::Published => "published",
                Framing::Thumbnail => "thumbnail",
                Framing::Perforation => "perforation",
                Framing::Address => "address",
            }
            .to_string(),
            thumbnail: matches!(
                framing::Framing::choose(caps),
                Framing::Thumbnail | Framing::Perforation
            ),
            multi_line: caps.reads_lines_at_once(),
            eject: caps.features.execute.supports(Op::Unload),
            autofocus: caps.features.execute.supports(Op::AutoFocus),
            hardware_metering: caps
                .set_window
                .kind
                .intersects(ScanKind::AE | ScanKind::AE_WB),
            interleavings: caps
                .set_window
                .interleaving
                .iter_names()
                .map(|(n, _)| n.to_ascii_lowercase())
                .collect(),
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

/// One plane, zero-copy, as a 2D numpy array
fn plane_to_numpy(py: Python<'_>, plane: Vec<u16>, rows: usize, cols: usize) -> Py<PyArray2<u16>> {
    plane
        .into_pyarray(py)
        .reshape([rows, cols])
        .expect("plane is rows * cols long")
        .unbind()
}

/// Zero-copy numpy arrays for `samples`, keyed by channel name, using `ids` (in `samples`'
/// plane order) to name them
fn colors_to_numpy(
    py: Python<'_>,
    ids: &[u8],
    samples: Vec<Vec<u16>>,
    rows: usize,
    cols: usize,
) -> HashMap<String, Py<PyArray2<u16>>> {
    ids.iter()
        .zip(samples)
        .map(|(&id, plane)| (channel_name(id), plane_to_numpy(py, plane, rows, cols)))
        .collect()
}

/// What discovery found
#[gen_stub_pyclass]
#[pyclass(name = "Discovery", frozen, get_all, module = "nkscan")]
pub struct PyDiscovery {
    frames: Vec<(u32, u32, u32, u32)>,
    /// One array per channel, keyed the way `ScanResult.colors` is;
    /// `None` where the mechanism that found `frames` needed no thumbnail pass
    thumbnail: Option<HashMap<String, Py<PyArray2<u16>>>>,
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
    /// `format` is one of "135", "half", "IX240", "16", "645", "66", "67",
    /// "68", "69", or a custom frame length in mm as a string (e.g. "56").
    /// Only asked for by the two of four discovery mechanisms that need a
    /// thumbnail pass to find frames, and even there only where the loaded
    /// holder does not fix or narrow it by itself, so it can usually be left
    /// `None`. `positive` is which way the loaded film reads.
    /// `Discovery.thumbnail`, where the mechanism took one, is what a caller
    /// wanting to nudge `Discovery.frames` by hand shows the operator: a
    /// rectangle handed to `scan_frame` needs no match in it, so a nudged one
    /// works the same as a detected one, just slower if the stage has to home
    /// first to reach it
    #[pyo3(signature = (format=None, positive=false, progress=None))]
    fn discover_frames(
        &self,
        py: Python<'_>,
        format: Option<&str>,
        positive: bool,
        progress: Option<Py<PyAny>>,
    ) -> PyResult<PyDiscovery> {
        let format = format
            .map(str::parse)
            .transpose()
            .map_err(pyo3::exceptions::PyValueError::new_err)?;
        let polarity = if positive {
            Polarity::Positive
        } else {
            Polarity::Negative
        };

        let (frames, thumbnail, ids, samples, shape) = py.detach(move || {
            self.with(|session| {
                let mut samples = Samples::default();
                let discovery =
                    framing::discover_with(session, format, polarity, &mut samples, |p| {
                        report(&progress, "discover", 0, p)
                    })?;
                let frames: Vec<_> = discovery
                    .frames
                    .into_iter()
                    .map(|r| (r.top, r.left, r.bottom, r.right))
                    .collect();
                match discovery.thumbnail {
                    Some(pass) => {
                        let ids: Vec<u8> = pass.layout.colors().collect();
                        let shape = (pass.rows, pass.cols);
                        Ok((frames, true, ids, samples.colors, shape))
                    }
                    None => Ok((frames, false, Vec::new(), Vec::new(), (0, 0))),
                }
            })
        })?;

        let thumbnail = thumbnail.then(|| {
            let (rows, cols) = shape;
            Python::attach(|py| colors_to_numpy(py, &ids, samples, rows, cols))
        });
        Ok(PyDiscovery { frames, thumbnail })
    }

    /// Send an edited set of frame boundaries to the unit as one table
    ///
    /// Call this after modifying what [`Session.discover_frames`][Self.discover_frames]
    /// returned, before scanning. Every rectangle becomes a frame entry, with
    /// perforation registration derived per top on roll film.
    #[pyo3(signature = (frames))]
    fn set_frames(&self, py: Python<'_>, frames: Vec<(u32, u32, u32, u32)>) -> PyResult<()> {
        let rects: Vec<Rect> = frames
            .into_iter()
            .map(|(top, left, bottom, right)| Rect {
                top,
                left,
                bottom,
                right,
            })
            .collect();
        py.detach(move || self.with(|session| session.update_frames(&rects).map(|_| ())))
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
                let caps = session.capabilities();
                let dpi = dpi.unwrap_or(caps.address.x_axis.optical_dpi);
                let interleaving = if superfine || !caps.reads_lines_at_once_at(dpi) {
                    ColorInterleaving::LINE_WITHOUT_DISTANCE
                } else {
                    ColorInterleaving::MULTILINE_SIMULTANEOUS
                };
                let recipe = Recipe {
                    dpi,
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
                    let colors = colors_to_numpy(py, &ids, buf.colors, rows, cols);
                    let ir = buf.ir.map(|plane| plane_to_numpy(py, plane, rows, cols));
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
    m.add_class::<PyDiscovery>()?;
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
