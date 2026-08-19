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
