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

film = "negative"
discovery = session.discover_frames(film=film)  # add format="66" where the holder can't tell on its own
print(discovery.frames)  # [(top, left, bottom, right), ...]

result = session.scan_frame(
    discovery.frames[0], film=film, base_density=discovery.base_density, clean=True
)
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
    hide("keep thumbnail")   # discovery takes no thumbnail pass
if not caps.eject:
    hide("eject")            # the operator takes the holder out by hand
if not caps.autofocus:
    hide("autofocus")
```

`x_dpi_range`, `y_dpi_range`, `focus_range`, `max_samples` and `max_frames` bound the controls that take a number. `framing` says how frames are found ("published", "thumbnail", "perforation" or "address"), and `interleavings` lists the reading modes offered.

## Metering

`scan_frame` and `meter_frame` meter the frame as Nikon Scan does: a prescan, then the film curve for a negative or full scale for a positive. Give them the same values as `discover_frames`:

- `film`: "positive" (the default), "slide", "negative", "kodachrome" or "mono".
- `format`: the film format, which sets the part of the frame that is metered.
- `base_density`: `Discovery.base_density`, the film base density off the thumbnail. `None` uses Nikon Scan's defaults.

`lock_white_balance` decides whether the channels are metered together or one at a time. It defaults to `Capabilities.locks_white_balance(film)`. Color negative meters each channel separately, which takes the orange mask off before the ADC. Slide, Kodachrome and black and white keep the factory balance. `caps.hardware_metering` is `False` on every unit seen, so metering happens here and not in the scanner.

Every `scan_frame` meters its own frame unless it is handed `exposures`. To expose a batch the same way, meter one frame and pass its exposures to the rest. `meter_frame` meters without scanning; ask for `infrared` there if the scans take the infrared plane or clean:

```python
metering = dict(film=film, base_density=discovery.base_density)
exposures = session.meter_frame(frames[1], infrared=True, **metering)
for frame in frames:
    session.scan_frame(frame, infrared=True, exposures=exposures)
```

## Focus

By default, `scan_frame` autofocuses on the center of the frame. Use `focus` to change this:

- `"auto"`: the unit focuses on the center of the frame. This is the default.
- `(x, y)`: the unit focuses on this point. Each value is a fraction of the frame size.
- An int: the lens moves to this position. The position must be in `caps.focus_range`.
- `"hold"`: the lens does not move.

`focus_frame` focuses without a scan. To focus one time for a batch, focus on one frame and scan all frames with `"hold"`:

```python
focused, position = session.focus_frame(frames[1])  # ("focused", 210)
for frame in frames:
    result = session.scan_frame(frame, focus="hold")
```

`ScanResult.focused` gives the focus result:

- `"focused"`: the unit reached focus, or the lens moved to the position.
- `"not_reached"`: autofocus did not reach focus. The scan continued at the last lens position.
- `"skipped"`: `focus` was `"hold"`.

`ScanResult.focus_position` gives the lens position during the scan. To control the lens directly, use `autofocus(x, y)`, `focus_to(position)` and `focus_position()`.

## Check the result

`ScanResult.complete` is `True` if all blocks of the pass arrived. `ScanResult.blocks` gives the number of blocks that arrived. If a pass is short, the planes contain data only for those blocks.

`Discovery.thumbnail_complete` and `Discovery.thumbnail_blocks` give the same data for the thumbnail pass. `Discovery.contrast` gives the quality of the fallback frame fit, and is `None` where Nikon Scan's search found the frames. Compare it only between strips on the same unit.

## Color profiles

`session.nikon_profile(film)` returns the bytes of Nikon's ICC profile for this unit and film. It returns `None` if Nikon Scan has no profile for them:

```python
icc = session.nikon_profile("negative")
```

## Nudging frames by hand

`scan_frame` scans the rectangle it is given. The pass is that rectangle at both
ends, so there is nothing to crop afterwards. A rectangle moved along the feed, or
cropped, is put in the unit's frame table before the pass, so it reaches the film
it asks for.

`discover_frames` returns its thumbnail pass, keyed the same way
`ScanResult.colors` is, along with the mapping between a thumbnail column and a
feed address. Where the holder publishes its frames, the pass is only a preview:

```python
discovery = session.discover_frames(film=film)
if discovery.thumbnail is not None:
    show_to_operator(discovery.thumbnail)  # dict[str, NDArray[uint16]]
```

Use `discovery.addresses_per_column` to go between the two, so the rectangle the
operator sees is the rectangle that gets scanned:

```python
per = discovery.addresses_per_column          # e.g. 48 on an LS-9000
top, left, bottom, right = discovery.frames[3]

# the frame's own columns of the thumbnail, to draw or slice a preview tile
first, last = round(top / per), round(bottom / per)
tile = {name: plane[:, first:last] for name, plane in discovery.thumbnail.items()}

# and back, for a rectangle the operator dragged
moved = (round(first * per), left, round(last * per), right)
result = session.scan_frame(moved)
```

Read that number from each `Discovery`. Do not compute it as
`optical_dpi / thumbnail_dpi`: the pass rounds it down to whole addresses.

On a unit that counts perforations (`caps.framing == "perforation"`) it is `None`. The
thumbnail is resampled as Nikon Scan resamples it, so its columns are not evenly
spaced. Use `discovery.frames` for the rectangles there.

## Progress and cancellation

Both `discover_frames` and `scan_frame` take a `progress` callback.
Returning `False` cancels the pass in progress; anything else (including `None`) continues.

```python
def on_progress(phase, pass_number, bytes_done, bytes_total):
    print(phase, pass_number, bytes_done, "/", bytes_total)
    return bytes_done < bytes_total // 2  # bail out partway through, for real

session.scan_frame(frame, progress=on_progress)
```

## Logging

`nkscan.init_logging(level, path)` sends the library's log to stderr, or appends it to `path`. Call it again to change the level or the file, or with `"off"` to stop:

```python
nkscan.init_logging("debug", "nkscan.log")
nkscan.init_logging("off")
```

`RUST_LOG` overrides `level`, and can set it per module, for example `nkscan::cdb=trace`.

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
