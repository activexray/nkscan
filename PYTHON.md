# nkscan for Python

We include python bindings to the library component via PyO3.
Most useful library features are exported to design python-based applications and GUIs.
Data is passed from rust to python with zero-copy numpy pointers, so we should keep some semblance of speed (although you get what you pay for with python).

The bindings need Python 3.13+.

## Building from scratch

```bash
pip install maturin
maturin develop --features python   # editable install into the active venv
# or: maturin build --release --features python --out dist
```

You can also just install from PyPI with

```bash
pip install nkscan
```

## Quick start

```python
import nkscan

device = nkscan.list_devices()[0]
session = nkscan.Session.open(device)
# or, if you already know where it is: nkscan.Session("/dev/sg4")

if not session.media_loaded():
    session.load()

discovery = session.discover_frames()  # or format="66" where the holder can't tell on its own
print(discovery.frames)  # [(top, left, bottom, right), ...]

result = session.scan_frame(discovery.frames[0], clean=True)
red = result.colors["red"]        # numpy uint16, shape (rows, cols)
print(result.dpi, red.shape, red.dtype)

session.close()  # or use Session as a context manager
```

## What the scanner can do

`session.capabilities` reports what the attached unit offers, so a GUI can hide controls it does not have rather than failing once a scan is under way.

```python
caps = session.capabilities
print(caps.vendor, caps.product, caps.optical_dpi)

if not caps.multi_line:
    hide("superfine")        # this unit never reads three lines at once
if not caps.thumbnail:
    hide("keep thumbnail")   # it frames from a page, with no thumbnail pass
if not caps.eject:
    hide("eject")            # the operator takes the holder out by hand
if not caps.autofocus:
    hide("autofocus")
```

`x_dpi_range`, `y_dpi_range`, `focus_range`, `max_samples` and `max_frames` bound the controls that take a number. `framing` says how frames are found ("published", "thumbnail", "perforation" or "address"), and `interleavings` lists the reading modes offered.

## White balance

`scan_frame(lock_white_balance=...)` decides whether the channels are metered together or one at a time. The right default follows the film, not the scanner:

```python
lock = nkscan.Capabilities.locks_white_balance("negative")  # False
session.scan_frame(frame, lock_white_balance=lock)
```

Color negative meters each channel separately, which takes the orange mask off before the ADC and is what Nikon Scan does. Slide, Kodachrome and black and white keep the factory balance.

`scan_frame` defaults to `True` whatever the film, so pass this explicitly when scanning negatives. `caps.hardware_metering` is `False` on every unit seen, an LS-9000 included - metering happens here rather than in the scanner, which is what makes the setting matter at all.

## Nudging frames by hand

`discover_frames` returns the thumbnail it detected against, keyed the same way `ScanResult.colors` is:

```python
discovery = session.discover_frames()  # or format="66" where the holder can't tell on its own
if discovery.thumbnail is not None:
    show_to_operator(discovery.thumbnail)  # dict[str, NDArray[uint16]]
```

You can use that to write logic to move around the discovery.frames.

## Progress and cancellation

Both `discover_frames` and `scan_frame` take a `progress` callback.
Returning `False` cancels the pass in progress; anything else (including `None`) continues.

```python
def on_progress(phase, pass_number, bytes_done, bytes_total):
    print(phase, pass_number, bytes_done, "/", bytes_total)
    return bytes_done < bytes_total // 2  # bail out partway through, for real

session.scan_frame(frame, progress=on_progress)
```

## Errors

Everything raises a subclass of `nkscan.ScannerError`. `TransientError` (and its children `TransportError`, `DeviceBusy`) is what's worth retrying.
`UnsupportedError` carries `.op` and `.reason` attributes.

```python
try:
    session.scan_frame(frame)
except nkscan.ScanCancelled:
    pass
except nkscan.TransientError:
    retry()
```

## The stub

`nkscan.pyi` is auto generated, not hand-written `src/python.rs`.
Regenerate it after changing the bindings:

```bash
cargo run --features python --bin stub_gen
```

CI fails if it's stale.
