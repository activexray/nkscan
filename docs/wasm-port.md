# Driving nkscan from wasm for a browser scanner app

## Context

`nkscan` today is a single crate (13.8k lines) that drives Nikon film scanners over
three synchronous transports (nusb/USB, Linux SG_IO, Windows `scsiscan.sys`) and does
all its image work — decode, frame detection, autoexposure, digital ICE, TIFF output —
on the host. The goal is to compile the library to `wasm32-unknown-unknown` and drive a
scanner directly from a browser page over WebUSB, running the whole pipeline in wasm.

The premise that `wide` will carry digital ICE across is correct but not the hard part.
The blocking constraint is that **the entire `Transport`/`Session` stack is synchronous
and nusb offers no blocking API on wasm**, so the sync/async mismatch has to be resolved
before anything else matters.

Decisions taken: async-ify `Transport` and `Session`; run the full pipeline (ICE + TIFF)
in wasm; accept a nightly toolchain for the wasm build only.

## Verified constraints

Each of these was checked in source, not assumed.

| Constraint | Evidence |
|---|---|
| `MaybeFuture::wait` does not exist on wasm | `nusb-0.2.7/src/maybe_future.rs:184,208` — `#[cfg(not(target_arch = "wasm32"))]`. Breaks `device.rs:129`, `usb.rs:55,64,68` at compile time |
| `Endpoint::transfer_blocking` does not exist on wasm | `nusb-0.2.7/src/device.rs:871`. Breaks `usb.rs:93,110` |
| `Instant::now()` panics on wasm | `std/src/sys/time/unsupported.rs:13` "time not implemented on this platform". 10 sites: `usb.rs:144,146`, `session/mod.rs:390,466`, `session/scan.rs:118,176,193,316,329`, `bin/nkscan/scan.rs:353` |
| `thread::sleep` is `memory_atomic_wait32` on wasm | `std/src/sys/thread/wasm.rs`. Needs `+atomics`, and `Atomics.wait` is illegal on the browser main thread. 4 sites: `usb.rs:159`, `windows.rs:176`, `session/mod.rs:497`, `bin/nkscan/scan.rs:538` |
| `std::thread::spawn` unsupported on wasm | `std/src/sys/thread/unsupported.rs:19`. Breaks the `thread::scope` reader pipeline at `session/scan.rs:166-170` |
| `DeviceInfo::bus_id()`/`port_chain()` are not available on wasm | `nusb-0.2.7/src/enumeration.rs:182-220` — gated to linux/macos/windows. `Attach::Usb { bus, ports }` (`device.rs:26-31`) has no wasm equivalent |
| `wide`'s SIMD needs an explicit target feature | `wide-1.6.1/src/f32x4_.rs:15,92` gate the `v128` paths on `target_feature = "simd128"`, off by default. It is also only used in `from_density` (`dust.rs:286-303`); the rest of `dust.rs` relies on autovectorization, which needs the same flag |
| WebUSB works in a Worker | `nusb-0.2.7/src/platform/webusb/mod.rs:62-70` handles `WorkerGlobalScope` as well as `Window` |
| `tiff`, `moxcms`, `wide`, `thiserror`, `bitflags`, `tracing` are wasm-clean | No `build.rs`, no C, pure Rust throughout the lock tree |

### Memory ceiling

wasm32 has a 4 GiB address space and allocation failure aborts. `dust::clean`
(`dust.rs:827`) holds `g` (4N) + `w` (4N) simultaneously, and `decide` (`dust.rs:515-540`)
allocates six N-byte bool planes on top (`dark`, two `and3_cols` results, two `and3_rows`
results, `mask`).

| Frame | Pixels | `Samples` (3×u16 + IR) | `dust::clean` peak | Total |
|---|---|---|---|---|
| 35mm @ 4000 dpi | 21.4 M | 171 MB | ~300 MB | ~470 MB — fine |
| 6×9 @ 4000 dpi | 79.1 M | 632 MB | ~1.1 GB | **~1.75 GB**, before TIFF output |

Medium format at full resolution will not fit reliably. Phase 5 addresses this.

### Deployment constraints (not code, but they shape the product)

- WebUSB is Chromium-only. No Firefox, no Safari.
- Cross-origin isolation (`COOP: same-origin`, `COEP: require-corp`) is required for
  `SharedArrayBuffer`, which rayon-on-wasm needs. Rules out `file://`.
- On Windows the scanner binds to `scsiscan.sys`; WebUSB needs it rebound to WinUSB.
  On Linux it needs a udev rule.

---

## Phase 0 — Spike WebUSB against real hardware first

**Do this before writing any Rust.** Chromium refuses to expose protected interface
classes. The LS5K spec declares the scanner vendor-specific with bulk endpoints
`0x01`/`0x82` (table 1-1-6-2-4, mirrored at `usb.rs:70-71`), which should be claimable,
but this is the single assumption that would invalidate the whole plan.

A ~50-line static page: `requestDevice({filters:[{vendorId: 0x04b0}]})` → `open()` →
`selectConfiguration(1)` → `claimInterface(0)` → `transferOut(1, <INQUIRY cdb>)` →
phase-poll `0xD0` → `transferIn(2, 64)`. Build the CDB by hand from
`protocol/cdbs.rs::Inquiry`. If a plausible INQUIRY response comes back, proceed.

Also measure `transferIn` throughput at the 128 KB chunk size `UsbTransport::max_transfer`
uses, so you know up front whether the browser can keep up with the stage.

## Phase 1 — Portability shims (native behavior unchanged)

- **`src/time.rs`**: re-export `std::time::Instant` natively, `web_time::Instant` on wasm.
  Add `web-time = "1"` under `[target.'cfg(target_arch = "wasm32")'.dependencies]`
  (already in the lock tree via `indicatif`). Update the 10 `Instant` imports.
- **`src/rt.rs`**: mirror nusb's own pattern at `maybe_future.rs:56-64` — a `MaybeSend`
  marker trait that is `Send` off wasm and blanket-implemented on wasm, plus
  `BoxFuture<'a, T>` (`Pin<Box<dyn Future<Output = T> + Send + 'a>>`, without `+ Send`
  on wasm, because `web-sys` types are not `Send`). Add `pub async fn sleep(Duration)`
  backed by `futures-timer 3` with its `wasm-bindgen` feature.
- Temporarily gate `pub mod usb;` (`transport/mod.rs:10`) and the nusb use in `device.rs`
  on `not(target_arch = "wasm32")` so the rest of the crate can start compiling for wasm.

## Phase 2 — Async `Transport` and `Session`

The mechanical core of the work. ~57 `pub fn` on `Session` plus the four free functions
in `session/probe.rs`.

- **`transport/mod.rs`**: `Transport::execute` returns `BoxFuture<'a, Result<Completion, Error>>`
  rather than `Result<...>`. `async fn` in trait is not `dyn`-compatible and
  `Box<dyn Transport>` is load-bearing (`device.rs:77`, `session/mod.rs:91`), so this must
  be a hand-rolled boxed future. Supertrait `Send` becomes `MaybeSend`.
- **Native transports stay blocking.** `usb.rs`, `linux.rs`, `windows.rs` wrap their
  existing bodies in `Box::pin(async move { ... })`; the blocking calls inside never
  yield, so native behavior is byte-identical to today and no executor is needed. The one
  real change is `usb.rs:159`'s 5 ms phase-poll `sleep` becoming `rt::sleep(..).await`.
  This is what keeps the refactor cheap.
- **`Session`**: `pub fn` → `pub async fn` across `session/{mod,data,window,focus,image,
  autoexpose,scan,probe}.rs`, threading `.await`. `Chunks::fill` (`session/image.rs:174`)
  becomes async too.
- **Add a poison flag to `Session`.** Async brings free cancellation, which is genuinely
  useful for a browser Cancel button — but a future dropped mid-`execute` leaves the
  LS5K 1-1-2 phase machine desynced, and the next command would read a stale phase byte.
  Set a flag on entry to `execute` and clear it on clean exit; fail fast if it is already
  set. This is new behavior that async makes possible, not a port artifact.
- **`session/scan.rs:166`**: keep the `thread::scope` reader/decoder pipeline behind
  `#[cfg(not(target_arch = "wasm32"))]`, and add a serial `async` read-then-decode loop
  for wasm (~50 lines). wasm loses the read/decode overlap; the existing
  `read_ms`/`decode_ms`/`starved_ms` debug line (`session/scan.rs:218-226`) quantifies
  exactly how much, so measure it on native first.
- **CLI boundary**: add `pollster = "0.4"` (zero-dep) under the `cli` feature and
  `block_on` the top-level flows in `bin/nkscan/{main,scan,dump,eject}.rs`.
  `examples/` and `benches/` are offline and untouched.

## Phase 3 — Move output into the library

A browser app needs the TIFF encoder, the luminance conversion, and the ICE orchestration,
all of which currently live in the CLI binary.

New `src/output/` behind a feature `output = ["dep:tiff", "dep:moxcms"]`, which `cli`
implies:

- `bin/nkscan/io.rs` → `output/tiff.rs`. Refactor `write_planes` (`io.rs:243`) to take
  `W: Write + Seek` instead of a path — the `tiff` encoder is already generic over that,
  so wasm can hand it a `Cursor<Vec<u8>>` and the CLI a `BufWriter<File>`. Keep
  `paths`/`next_free`/`thumbnail_path` (`io.rs:31-64`) CLI-side; they touch the filesystem.
  `to_full_scale` (`io.rs:192`) moves with the rest.
- `bin/nkscan/mono.rs` → `output/mono.rs` (unchanged; `moxcms` is wasm-clean).
- `clean_frame` + `decimate` + `PRESCAN_PIXELS` (`bin/nkscan/scan.rs:423-493`) →
  `output/clean.rs` or `dust::clean_frame`. This is the piece that turns a `Pass` +
  `Samples` into a calibrated `dust::clean` call, and both frontends need it verbatim.

## Phase 4 — WebUSB transport

- **`src/transport/webusb.rs`**: `WebUsbTransport` over nusb's webusb backend. Same LS5K
  1-1-2 phase machine as `usb.rs:130-232`, but `Endpoint::submit` + `next_complete`
  (`.await`) instead of `transfer_blocking`, and `.await` instead of `.wait()` on open /
  `set_configuration` / `claim_interface`.
- Extract the phase state machine from `usb.rs` into a shared helper generic over an
  async `write_out`/`read_in` pair, so the two implementations of the same wire protocol
  cannot drift. The `read_in` packet-rounding and over-read check (`usb.rs:104-127`) is
  subtle enough to be worth having in exactly one place.
- **WebUSB has no per-transfer timeout.** Race each transfer future against
  `rt::sleep(timeout)` so `Error::Timeout` keeps meaning what `transport/mod.rs:29` says
  it means.
- **`device.rs`**: `bus_id`/`port_chain` are unavailable, so add
  `#[cfg(target_arch = "wasm32")] Attach::WebUsb(...)` keyed on nusb's webusb `DeviceId`
  and a `request_device()` entry point. `requestDevice` needs a user gesture and so is
  called from the main thread; the worker then finds the already-permitted device via
  `list_devices()`. `scsi_devices()`'s `read_dir("/dev")` (`device.rs:158`) is already
  Linux-gated.

## Phase 5 — wasm build and threading

- Add under `cfg(target_arch = "wasm32")`: `wasm-bindgen`, `wasm-bindgen-futures`,
  `wasm-bindgen-rayon`, `console_error_panic_hook`, and a `tracing` layer that writes to
  `console` (`tracing-subscriber`'s `EnvFilter`/`fmt` is a no-op sink on wasm — see
  `bin/nkscan/main.rs:46-55`).
- `.cargo/config.toml` for `wasm32-unknown-unknown`:
  `-C target-feature=+atomics,+bulk-memory,+mutable-globals,+simd128` and
  `-Z build-std=std,panic_abort`. **`+simd128` is not optional** — without it `wide`
  silently falls back to scalar. Pin nightly separately; `rust-toolchain.toml` stays on
  stable 1.97 for native.
- JS must call `initThreadPool(navigator.hardwareConcurrency)` before any `dust::` entry
  point. Serve cross-origin-isolated.
- Add a `parallel` feature (default on) gating `rayon` in `dust.rs` with a serial
  fallback, so a stable single-threaded wasm build stays available as an escape hatch.
  Rayon is confined to `dust.rs` — 17 sites, all `par_chunks`/`into_par_iter`.
- **Binary size**: `scan/profile.rs:79-83` embeds 12 Nikon ICC profiles via `include_bytes!`
  (~2.6 MB). Gate behind a feature and measure the gzipped wasm; consider fetching them
  at runtime instead.

## Phase 6 — Memory

- Stream the TIFF to a `FileSystemWritableFileStream` rather than buffering a
  `Cursor<Vec<u8>>` — saves ~474 MB on a 6×9 frame. `write_planes` already writes
  strip-by-strip, so this is a `Write + Seek` adapter, not a restructure.
- **Band-tile `dust::clean`.** The vertical dependency radius is only ±4 rows:
  `pyramids_at`'s largest kernel is the 9×9 `LEVEL0` box (`dust.rs:620-640`), and
  `decide` chains `and3_rows(k=1)` then `and3_rows(k=3)` and reads `row_dark` at ±4
  (`dust.rs:526-537`). A 16-row halo is ample. Process bands of ~1024 rows, allocating
  `g`/`w`/`mask` per band. Drops the ~1.1 GB peak to a few hundred MB. Note the halo must
  distinguish a band edge from a real image edge, since `and3_rows` clamps at `rows - 1`.
- Until that lands, have the wasm entry point reject frames where `rows * cols * 14`
  exceeds a budget, with an error naming the resolution to use instead.

## Verification

1. **Native regression is the priority** — Phase 2 touches every command path.
   - `cargo clippy --all-targets` across the five targets in `rust-toolchain.toml` (CI
     already cross-checks all of them).
   - Scan one 35mm frame with `--clean` on real hardware before and after Phase 2 and
     compare the output TIFFs byte-for-byte. `dust.rs` is untouched, so any difference is
     a protocol-path bug.
   - Compare the `read_ms` / `starved_ms` / `decode_ms` debug line before and after to
     confirm the native scoped-thread path still overlaps read and decode.
   - `cargo bench --bench dust` unchanged (offline, no `Session`).
2. `cargo +nightly check --target wasm32-unknown-unknown -Z build-std=std,panic_abort`
   after each of Phases 1, 3, 4, 5.
3. **Browser bring-up**, in order, on a minimal `wasm-bindgen --target web` page served
   cross-origin-isolated: `requestDevice` → `Session::open` → INQUIRY → `capabilities` →
   `scan_thumbnail` → full 35mm pass → `--clean` equivalent → TIFF download.
4. **End-to-end equivalence**: scan the same 35mm frame through the CLI and through the
   browser and diff the TIFFs. They should be identical; `dust.rs` and `protocol/decode.rs`
   are the same code on both paths.
5. Confirm rayon actually engages in the browser (compare `dust::clean` wall time against
   a `parallel`-off build) and that `+simd128` is active (compare `from_density` timing).
