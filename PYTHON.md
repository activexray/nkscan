# nkscan for Python

A thin skin over the Rust library: convert arguments, hand decoded planes to
numpy without copying them, release the interpreter while a call blocks on
the scanner. Needs Python 3.13+ (the crate builds one `abi3` wheel that runs
on it and everything later).

## Install

```bash
pip install maturin
maturin develop --features python   # editable install into the active venv
# or: maturin build --release --features python --out dist
```

## Quick start

```python
import nkscan

device = nkscan.list_devices()[0]
session = nkscan.Session.open(device)
# or, if you already know where it is: nkscan.Session("/dev/sg4")

if not session.media_loaded():
    session.load()

discovery = session.discover_frames(format_mm=56.0, positive=False)
print(discovery.frames)  # [(top, left, bottom, right), ...]

result = session.scan_frame(discovery.frames[0], clean=True)
red = result.colors["red"]        # numpy uint16, shape (rows, cols)
print(result.dpi, red.shape, red.dtype)

session.close()  # or use Session as a context manager
```

## Nudging frames by hand

`discover_frames` also returns the thumbnail it detected against, keyed the
same way `ScanResult.colors` is:

```python
discovery = session.discover_frames(format_mm=56.0, positive=False)
if discovery.thumbnail is not None:
    show_to_operator(discovery.thumbnail)  # dict[str, NDArray[uint16]]
```

`scan_frame` doesn't validate its `frame` argument against anything —
`discover_frames` never has to run first, and a rectangle you made up or
nudged by hand works exactly like a detected one. The only cost of a
rectangle the unit doesn't already know about is one extra stage move to
home before it steps to it.

## Progress and cancellation

Both `discover_frames` and `scan_frame` take a `progress` callback. Returning
`False` cancels the pass in progress; anything else (including `None`)
continues.

```python
def on_progress(phase, pass_number, bytes_done, bytes_total):
    print(phase, pass_number, bytes_done, "/", bytes_total)
    return bytes_done < bytes_total // 2  # bail out partway through, for real

session.scan_frame(frame, progress=on_progress)
```

`scan_frame`'s `phase` is `"meter"` or `"scan"`; `discover_frames`'s is always
`"discover"`. `pass_number` counts metering passes from one and is `0`
otherwise.

## Errors

Everything raises a subclass of `nkscan.ScannerError`. `TransientError` (and
its children `TransportError`, `DeviceBusy`) is what's worth retrying;
`UnsupportedError` carries `.op` and `.reason` attributes.

```python
try:
    session.scan_frame(frame)
except nkscan.ScanCancelled:
    pass
except nkscan.TransientError:
    retry()
```

## Why a `dict` of planes and not an `(H, W, 3)` array

The Rust side never interleaves channels — `colors` is one separately
allocated buffer per channel, handed to numpy zero-copy. Stacking them into
one RGB array would need a copy, and would assume exactly three channels in
RGB order, which isn't always true (a mono-negative scan captures one). If
you want a flat image for display: `np.dstack([colors["red"], colors["green"], colors["blue"]])` —
a real copy, done only when you actually want one.

## The stub

`nkscan.pyi` is generated, not hand-written — it can't drift from
`src/python.rs`. Regenerate it after changing the bindings:

```bash
cargo run --features python --bin stub_gen
```

CI fails if it's stale.
