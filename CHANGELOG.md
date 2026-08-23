# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.0]

### Added

- macOS SCSI transport over IOKit, so FireWire units work there through ASFireWire.
- `--lock-wb`, beside the existing `--unlock-wb`. Color negative now meters each channel separately by default, matching Nikon Scan; slide, Kodachrome and black and white keep the factory balance.
- Scanner capabilities are exposed to Python, so a GUI can hide controls the attached unit does not have.
- `Metering::locks_white_balance`, `Session::read_image_within`, and `scan::window::MAX_SAMPLES` is now public.

### Changed

- **Breaking:** `Session::abort` answers whether the unit has the command, rather than `()`.
- **Breaking:** `Attach` has a new `ScsiTask` variant on macOS, so an exhaustive match there needs an arm for it.
- macOS builds use `core-foundation-sys` rather than `core-foundation`.

### Fixed

- Ctrl-c stops a scan in about a second instead of reading out the rest of the frame.
- Cancelling a scan no longer wedges the unit until a power cycle.
- A command that times out no longer breaks every command after it.
- Inserting a film holder no longer ends the run with `timed out after 5s`.
- Metering no longer stalls for fifteen seconds a frame on some holders.
- Ctrl-c gives the film back, unless `--no-eject`.
- Progress bars and log lines no longer overwrite each other, and finished bars no longer pile up.
- No thumbnail bar on units that frame without a thumbnail pass.
- The echoed `^C` and the Enter that answers a prompt no longer stay on screen.
- Ejecting no longer reports a failure when it worked.
- A strip feeder's empty gate no longer aborts a run between strips.
- Frame detection names the frame it drops rather than losing it quietly.
- Frame length self-calibrates from the strip's own edges, so a gate that varies from the nominal format no longer shifts every frame.

## [0.7.0]

### Added

- `scan::framing::discover`/`discover_with`, moving the CLI's frame-discovery dispatch into the library
- `scan::frame::scan_frame`/`scan_frame_with`, the same for one frame's focus/meter/scan/clean sequence.
- `scan::clean::clean_frame`, the dust-removal glue (decimated calibration, falling back to the whole frame).
- `Samples::to_full_scale`, stretching a pass's samples to 16 bits in place.
- `nkscan scan` now runs on top of all four of the above rather than duplicating them.
- `src/python.rs`, the `python` feature's pyo3 bindings. See PYTHON.md.
- `FilmFormat` implements `FromStr`/`Display`, and `FilmFormat::resolve` picks a format from an explicit one or the loaded holder/adapter, replacing the CLI's own copy of that logic.

### Changed

- `scan_pass_with`, `scan_thumbnail_with`, `autoexpose_with` and `autoexpose_frame_with`'s progress closure returns `ControlFlow<()>` now; `Break` cancels with `Error::Cancelled`, draining the rest rather than sending `ABORT`.
- `anyhow` and `tracing-subscriber` moved behind the `cli` feature. Neither was ever used outside `src/bin/`; a library-only build no longer pulls either in.
- `framing::discover_with` resolves a missing `--format` itself now instead of requiring the caller to.

## [0.6.0]

### Fixed

- Type2 perforation framing picked each frame's perforation triple by matching a re-derived address against the table's nearest entry instead of just using the perforation record itself, which has an entry for every thumbnail column.

### Changed

- `FramePosition::new` takes the already-detected top and one `PerforationInformation` record directly, and no longer returns `Option`. `PerfInformation::nearest` is gone; look the record up with the new `PerfInformation::at` instead.
- Per-chunk image READ logging dropped from `debug` to `trace`, and `nusb`'s own per-USB-transfer logging now defaults to `warn`, so `--log debug` isn't drowned out by either.
- Crate-level usage docs added, and broken intra-doc links across the public API fixed, ahead of the crates.io release.

## [0.5.1]

### Fixed

- Perforation framing (`frames_type2`, the SA-21/SA-30 on an LS-5000/LS-50) placed a frame's Y address with a different, un-truncated pitch than the one used to bound it, drifting further off the further a frame sat down the strip. It now reuses the one address the bounds check already computed.

## [0.5.0]

### Fixed

- Lots of little bugs with the old frame-detection algorithm is out, we have rewritten the logic to be way more robust.
- Every 120 format was scanned at the size its name says rather than the size it exposes. 6x4.5 is now 41.5 mm, 6x7 69.5 and 6x8 76.
- 6x9 scans now work
- The SF-210 never scanned anything. Its slides sit in the magazine rather than the gate, so nothing ever read as loaded. Units that advertise LOAD (2-15-3 D1h) are now asked to take film in.
- The IA-20 scanned one frame and ejected the cartridge. Its Y address range spans every frame the loaded film carries, so a 25 exposure roll now comes back as 25 frames.
- An FH-3 strip holder in an MA-21 could not be advanced. The batch waited for the holder to leave the gate, which sliding it along never does. Where the unit has no UNLOAD to give the film back with, the operator presses Enter once they have replaced or advanced it, from [#33](https://github.com/activexray/nkscan/pull/33)

### Added

- Frames that overlap come back as overlapping rectangles, each keeping the edge it was found by, so a strip the transport under-advanced is scanned with the film they share in both.
- Every frame carries a little film either side, up to two percent of the format
- `--format half` for 35mm shot half frame (18 x 24 mm).
- `--format IX240` (or `aps`) now scans APS's 30.2 mm recorded frame
- A feeder works through its magazine a slide at a time, and the batch ends when the unit says the supply is empty

### Changed

- `scan::boundaries::detect` and `scan::thumbnail::frames`/`frames_type2` take the film's polarity rather than an `Option` of it.
- `scan::boundaries::Detected` is the column each frame starts at and the pitch, nothing else.
- `Framing::Caller` is `Framing::Address`, and `framing::single_frame` is `framing::frames`, which returns every frame the address page describes rather than assuming the gate holds one.
- `Session::load` takes film in, mirroring `Session::eject`. Both do nothing where the unit does not advertise the operation.
- A load that found nothing to take is `Intervention::NothingToLoad` rather than `NoMedium`.

## [0.4.5]

### Added

- `nkscan eject`, which ejects whatever is loaded without scanning it.
- `nkscan scan --thumbnail` keeps the framing pass as
  `<basename>_<n>_thumbnail.tiff`, written before detection runs.

## [0.4.4]

### Fixed

- The batch scan loop treated `eject`'s no-op on a mount with no `UNLOAD`,
  such as the MA-21, as ready to go: `media_loaded()` still read true off the
  slide it already had seated, so the same frame was scanned again with no
  prompt at all. It now waits for that slide to actually come out first.

## [0.4.3]

### Fixed

- `Chunks::drain` read past the end of every pass looking for multi-line
  registration seams, whether or not the unit had raised that cooperation.
  The LS-5000 never raises it and never answers that READ either, so metering
  stalled for `MOVE_TIMEOUT` and the unit stopped answering anything after.
  The READ is now sent only once `MultiLineRegistration` actually came off
  the wire.
- `eject` failed on an adapter with no `UNLOAD`, such as a single-slide mount,
  ending an otherwise complete scan in an error. It is now a no-op there.
- `--superfine` silently did nothing on a unit that never raises multi-line
  cooperation, such as the LS-40/LS-50, since the scan was already single-line
  either way. It now says so.

## [0.4.2]

### Added

- Frame discovery for adapters that offer no frame table and no perforation
  read, such as a mount adapter (MA-21) carrying a single slide: the frame is
  the whole opening the address page already describes, so `nkscan scan` runs
  against them instead of refusing to start.

### Fixed

- CCD row-curve correction sized a `CcdData` reply against colors × types
  from the measurement page, when 2-11-10 says a reply is CCD lines × types.
  A unit whose line count differs from its color count, such as LS-50 and
  LS-5000's 2-line CCD against 3 measured colors, scanned every frame
  uncorrected instead.

## [0.4.1]

### Fixed

- An image READ asks for whole lines as the unit transfers them, which counts
  the bytes 2-11-5-3 attaches to each line. A length ending mid-line was rounded
  up to the next whole one by the unit, and the extra bytes were read as the
  status that follows, ending the scan a timeout later on an invalid phase byte.
- A bulk read that returns more bytes than were asked for is an error rather
  than a warning and a truncation. The extra bytes have already been taken from
  the endpoint, so the next read starts partway through the unit's answer.

## [0.4.0]

### Added

- `--clean`, which removes dust and scratches with the infrared channel, an implementation of the pipeline @a6o documented for [openICE](https://github.com/a6o/openICE)
- Coolscan V (LS-50), with the SA-21 and SA-30 adapters, from [#26](https://github.com/activexray/nkscan/pull/26)

### Changed

- A pass's colors come back as one buffer per channel, `Samples::colors`,
  rather than one buffer with the channels interleaved into it. Nothing
  downstream has to stride past two channels it does not want.
- `protocol::data::Image` is `SetupImage`, distinct from
  `protocol::decode::Image` which is what a finished pass actually is.
- Samples are stretched to fill 16 bits once, when a pass lands, instead of
  per sample on the way into a TIFF. A unit that scans 14 bits deep now looks
  the same as a 16-bit one to everything after the pass, which is what lets
  `--clean` work on either: ICE's thresholds are absolute against a 65535
  full scale, so a 14-bit frame left alone reads as one enormous defect.

### Fixed

- `calibrate` returns `None` rather than a NaN when a prescan holds no clear
  film to measure against. The NaN used to reach every downstream constant
  and flag the whole frame as dust.

## [0.3.4]

### Changed

- A pass's samples come back as `protocol::decode::Samples`, color and
  infrared in their own buffers, instead of one `Vec<u16>` with infrared
  interleaved into it. `Session::scan_pass_with`, `scan_thumbnail_with`,
  `scan::thumbnail::frames` and `Image::new` all take it now.

### Fixed

- `nkscan dump` stopped a page's hexdump at its declared length instead of
  printing the padding after it as though it were data.

## [0.3.3]

### Fixed

- The capability pages are walked rather than indexed. Extend bits and length
  prefixes mean the unit decides where each field sits, so a spec's byte
  numbers only hold for the model it documents. Units that spend fewer bytes
  than the LS-9000 had every field after the first short one read a byte early,
  which on an LS-8000 ED refused to scan for want of an autofocus it has.
- A field the page ends before takes its documented default instead of whatever
  the transport padded with.

## [0.3.2]

### Added

- `nkscan dump`, which prints every VPD page for debugging
- Probing now logs what it found: the raw page bytes at trace, and the declared
  page length, host cooperation and EXECUTE operations at debug.

## [0.3.1]

### Added

- `--log`, so verbosity can be set without shell-specific `RUST_LOG=` syntax
  (which cmd.exe and PowerShell don't accept inline). `RUST_LOG` still wins
  when set, since it can target individual modules.

### Fixed

- Color codes no longer leak into redirected or piped output (e.g. a log
  captured for an issue report): ANSI is now gated on stdout actually being a
  terminal.
- On Windows, legacy `cmd.exe`/PowerShell windows not hosted in Windows
  Terminal now render color instead of printing raw escape codes; the process
  opts the console into VT processing at startup.

## [0.3.0]

A complete and total rewrite.
Nothing of the 0.2.0 API survives, so the entry below it describes a crate that no longer exists rather than anything you can still call.
The driver now reads what a unit advertises and works from that, instead of carrying a table of what each model and holder is supposed to be able to do.

### Added

- Four layers with the boundaries actually enforced: `transport` moves SCSI
  bytes, `protocol` is types and parsing with no IO, `session` holds an open
  unit and the state that outlives one command, `scan` decides what a pass
  should do.
- Windows support, through the `scsiscan.sys` class driver, alongside the Linux
  sg path and the nusb USB path that covers all three platforms.
- Frame detection from a whole-strip thumbnail, for the holders that publish
  rectangles with no lengths. The film format is the caller's to supply
  (`--format`); nothing on the wire carries it.
- Host-side metering, for the units that run no AE pass of their own. Scales
  per-channel exposure to the ADC ceiling and takes another pass only when a
  channel came back clipped.
- CCD row-response correction from `DataType::CcdData`, which is the banding a
  three-line pass has and a single-line one does not.
- Autofocus per frame, and the focus position read back so a focus is
  repeatable without focusing again.
- Nikon's own scanner profiles, one per model per film type, embedded and
  converted to take the linear samples a pass produces. See
  `profiles/README.md`: they are not covered by this crate's license.
- `nkscan scan` batches a holder at a time and prompts for the next, writing
  16-bit TIFF with the infrared mask in a file of its own.
- Release binaries for macOS, both Apple Silicon and Intel, alongside the Linux
  and Windows ones. The macOS ones are unsigned, so Gatekeeper quarantines a
  downloaded one until `xattr -d com.apple.quarantine` clears it.
- CI cross-checks every target `rust-toolchain.toml` names. Only the host was
  ever compiled before, so `src/transport/windows.rs` first met a compiler when
  a release tag reached the Windows job and failed there, and the macOS side had
  gone the same way.
- A release asserts its own portability before it uploads anything: statically
  linked on Linux, no VC++ runtime imports on Windows, and no dylib outside the
  system paths on macOS.

### Changed

- Releases are built by GitHub Actions on each platform's own runner, and CI
  runs cargo directly, so neither goes through Nix. The flake stays for devshells
  and to provide binaries as a flake.
- The Linux and Windows binaries are static. musl links the whole libc in and
  `+crt-static` folds in the MSVC runtime, so neither has a system library to
  match on the machine that runs it.

- Sense data is read as what to do next rather than as an error, so polling,
  unit attentions and the vendor cooperation handshake are absorbed by the
  retry loop instead of surfacing to callers.
- A pass is decoded as it streams, into a buffer the caller owns, rather than
  after a scan-sized read.

### Fixed

- The published Linux binary had its ELF interpreter baked to a `/nix/store`
  glibc, so it ran on a Nix machine and nowhere else, failing with "No such file
  or directory" for a file that was plainly there.

### Removed

- The Python bindings, temporarily. There is no `#[pymodule]` on this branch;
  the `python` feature and `pyproject.toml` are kept for when they come back,
  and the jobs that would build and publish a wheel are commented out in both
  workflows.

## [0.2.0] - 2026-08-01

### Added

- Adapter and model vocabulary for all six Coolscans; every scanner Nikon Scan
  supported is now recognized by name (`nkscan --list` reports an attached
  8000, 4000 or IV as undriven rather than staying silent about it).
- `Session::sensed_frames()`, so a host can place frames from what the
  transport reports instead of assuming a pitch; `prepare()` takes
  `offsets_mm` as a sequence, superseding `offset_mm`.
- The overview pass, previously computed and thrown away, is now exposed.
- Support for the SA-30 holder on the LS-50.

### Changed

- Frame placement is picked from what the scanner can actually do rather than
  from absent arguments.
- Refusals are machine-readable: `UnsupportedError` now carries `feature`,
  `reason`, `allowed` and `asked` alongside the message, so callers branch on
  `err.reason == "not_implemented"` instead of on the wording.
- Capabilities are computed from the model and the loaded adapter instead of
  being declared per driver.
- The five options nothing had ever checked are now gated and enforced.
- `nikon::capabilities::Capabilities` was renamed to `nikon::limits::DeviceLimits`.
- The pixel-depth width list was dropped; the bit depth comes off the device
  rather than from a table.

### Fixed

- The LS-5000 sends SCAN with a zero control byte and is no longer "totally
  untested" in the README (only a strip scan is proven; the roll transport,
  multi-sample readout and metering are still inference).
- Page 0xC8 byte 4 is no longer described as an aperture count.

### Removed

- `ResolvedPlacement`, a byte-for-byte copy of `Placement`.
- Three abstractions that were a bool in a costume.
- The pixel-depth width list (this library never downsamples).

## [0.1.0] - 2026-07-30

Initial release.
