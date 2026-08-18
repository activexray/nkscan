use divan::counter::ItemsCount;
use nkscan::{
    dust::{self, Calibration, Options, Params, Prescan},
    protocol::decode::Samples,
};
use std::sync::LazyLock;
use tiff::decoder::{Decoder, DecodingResult, Limits};

fn main() {
    divan::main();
}

/// The real frame's shape: 6x9 at 4000 DPI
const ROWS: usize = 8964;
const COLS: usize = 8820;

/// 35mm full-frame at 4000 DPI: 36x24mm. No captured frame table for this
/// format is in the corpus (only 6x9), so this is the geometric size,
/// `mm / 25.4 * dpi`, not a measured one
const ROWS_35: usize = 3780;
const COLS_35: usize = 5669;

/// A plane's worth of linear samples, the shape `to_density` actually sees
fn plane(n: usize) -> Vec<u16> {
    (0..n).map(|i| (i % 65536) as u16).collect()
}

/// What the fixtures are: an LS-9000 frame at 4000 DPI, metered where this
/// crate's AE puts it
fn options() -> Options {
    Options {
        model: dust::Model::Ls9000,
        quality: dust::Quality::Normal,
        dpi: 4000,
        metering_target: nkscan::scan::meter::Metering::default().target,
    }
}

/// The profile every kernel bench runs under
fn params() -> Params {
    Params::new(
        &options(),
        &Calibration {
            c: 0.05,
            ir_ref: 40_000.0,
        },
    )
}

/// The biggest image, a 6x9 frame at 4000 DPI
#[divan::bench(args = [9440 * 14160])]
fn to_density(bencher: divan::Bencher, n: usize) {
    bencher
        .counter(ItemsCount::new(n))
        .with_inputs(|| plane(n))
        .bench_values(|samples| dust::to_density(&samples));
}

fn read_u16(path: &str) -> Option<Vec<u16>> {
    let file = std::fs::File::open(path)
        .inspect_err(|e| eprintln!("{path}: {e}"))
        .ok()?;
    let mut decoder = Decoder::new(std::io::BufReader::new(file))
        .inspect_err(|e| eprintln!("{path}: {e}"))
        .ok()?
        .with_limits(Limits::unlimited());
    match decoder.read_image() {
        Ok(DecodingResult::U16(v)) => Some(v),
        Ok(other) => {
            eprintln!("{path}: not 16-bit samples ({other:?})");
            None
        }
        Err(e) => {
            eprintln!("{path}: {e}");
            None
        }
    }
}

/// The TIFF itself is still chunky RGB; `Samples` wants planes apart
fn deinterleave3(chunky: &[u16]) -> Vec<Vec<u16>> {
    let mut planes: Vec<Vec<u16>> = (0..3)
        .map(|_| Vec::with_capacity(chunky.len() / 3))
        .collect();
    for pixel in chunky.chunks_exact(3) {
        for (plane, &v) in planes.iter_mut().zip(pixel) {
            plane.push(v);
        }
    }
    planes
}

static SCAN: LazyLock<Option<Samples>> = LazyLock::new(|| {
    Some(Samples {
        colors: deinterleave3(&read_u16("scan_1.tiff")?),
        ir: read_u16("scan_1_IR.tiff"),
    })
});

/// The real AE prescan shape for medium format: 666x333 DPI, 1494 sensor
/// pixels by 1098 stage positions, IR present. Same fixture as
/// `protocol::image::readouts::prescan` -- "as the captures deliver it"
const PRESCAN_ROWS: usize = 1494;
const PRESCAN_COLS: usize = 1098;

/// The synthetic prescan planes the calibrate bench measures
struct Planes {
    colors: Vec<Vec<u16>>,
    ir: Vec<u16>,
}

static PRESCAN: LazyLock<Planes> = LazyLock::new(|| {
    let n = PRESCAN_ROWS * PRESCAN_COLS;
    Planes {
        colors: vec![plane(n), plane(n), plane(n)],
        ir: plane(n),
    }
});

/// A plane's worth of density values, the shape `confidence` sees. Value
/// doesn't matter -- there is no data-dependent branching -- only the count
fn density_plane(n: usize) -> Vec<f32> {
    plane(n).into_iter().map(f32::from).collect()
}

/// A full-res red and IR plane, the real size `clean` calls `gate` at.
/// `gate` takes raw samples now, fused with its own density transform
#[divan::bench(args = [ROWS * COLS])]
fn gate(bencher: divan::Bencher, n: usize) {
    let p = params();
    bencher
        .counter(ItemsCount::new(n))
        .with_inputs(|| (plane(n), plane(n)))
        .bench_values(|(red, ir)| dust::gate(&red, &ir, &p));
}

/// Same shape as `gate`'s output, the input `confidence` sees
#[divan::bench(args = [ROWS * COLS])]
fn confidence(bencher: divan::Bencher, n: usize) {
    let p = params();
    bencher
        .counter(ItemsCount::new(n))
        .with_inputs(|| density_plane(n))
        .bench_values(|g| dust::confidence(&g, COLS, &p));
}

/// Confidence values that stay under the `w >= 1` short-circuit, so every
/// pixel actually walks its probes -- a worst case, since real confidence
/// clips to 1 for a share of clean pixels that skip the probes entirely
fn sub_one_plane(n: usize) -> Vec<f32> {
    (0..n).map(|i| (i % 100) as f32 / 100.0).collect()
}

/// Full-res plane shape. `g` cycles through the full range so both sides of
/// the dust floor phi show up in the probes
#[divan::bench(args = [ROWS * COLS])]
fn decide(bencher: divan::Bencher, n: usize) {
    let p = params();
    bencher
        .counter(ItemsCount::new(n))
        .with_inputs(|| (density_plane(n), sub_one_plane(n)))
        .bench_values(|(g, w)| dust::decide(&g, &w, ROWS, COLS, &p));
}

/// A mask with roughly `pct`% of pixels flagged, evenly spread. Cost here
/// doesn't depend on clustering (each flagged pixel's cost is independent),
/// so spread vs clustered shouldn't matter, unlike a real scan's blobs
fn mask_plane(n: usize, pct: usize) -> Vec<bool> {
    (0..n).map(|i| i % 100 < pct).collect()
}

/// Full-res shape; flagged fraction matches what the pipeline actually sees
/// on a real scan (1.2%, see the mask dump)
#[divan::bench(args = [ROWS * COLS])]
fn reconstruct_core(bencher: divan::Bencher, n: usize) {
    let p = params();
    bencher
        .counter(ItemsCount::new(n))
        .with_inputs(|| {
            (
                density_plane(n),
                sub_one_plane(n),
                plane(n),
                plane(n),
                plane(n),
                mask_plane(n, 1),
            )
        })
        .bench_values(|(g, w, r, gr, b, mask)| {
            dust::reconstruct_core(&g, &w, [&r, &gr, &b], &mask, &p, ROWS, COLS)
        });
}

#[divan::bench]
fn calibrate(bencher: divan::Bencher) {
    bencher
        .counter(ItemsCount::new(PRESCAN_ROWS * PRESCAN_COLS))
        .with_inputs(prescan_image)
        .bench_values(|prescan| dust::calibrate(&prescan));
}

fn prescan_image() -> Prescan<'static> {
    Prescan {
        red: &PRESCAN.colors[0],
        ir: &PRESCAN.ir,
        rows: PRESCAN_ROWS,
        cols: PRESCAN_COLS,
    }
}

/// Nearest-neighbor decimate, so the prescan is the shape AE really hands us
fn decimate(src: &[u16], step: usize, full_rows: usize, full_cols: usize) -> Vec<u16> {
    let (rows, cols) = (full_rows / step, full_cols / step);
    let mut out = Vec::with_capacity(rows * cols);
    for y in 0..rows {
        for x in 0..cols {
            out.push(src[y * step * full_cols + x * step]);
        }
    }
    out
}

/// A prescan decimated off the real scan. The synthetic PRESCAN fixture makes
/// color and IR identical, which gives every crosstalk slope exactly 1.0 --
/// always outside the +-0.2 filter, so c falls back to 0 and the fit never
/// runs. Feeding clean() the full-res frame as its own prescan works too, but
/// then calibrate() costs ~125ms of a number that is supposed to be about
/// everything else
struct Prescan6 {
    colors: Vec<Vec<u16>>,
    ir: Vec<u16>,
}

static SMALL_PRESCAN: LazyLock<Option<Prescan6>> = LazyLock::new(|| {
    let samples = SCAN.as_ref()?;
    Some(Prescan6 {
        colors: samples
            .colors
            .iter()
            .map(|p| decimate(p, 6, ROWS, COLS))
            .collect(),
        ir: decimate(samples.ir.as_ref()?, 6, ROWS, COLS),
    })
});

#[divan::bench]
fn clean(bencher: divan::Bencher) {
    let (Some(samples), Some(pre)) = (SCAN.as_ref(), SMALL_PRESCAN.as_ref()) else {
        eprintln!("no scan_1.tiff/scan_1_IR.tiff at the repo root, skipping");
        return;
    };
    let counted = samples.colors.iter().map(Vec::len).sum::<usize>()
        + samples.ir.as_ref().map_or(0, Vec::len);
    bencher
        .counter(ItemsCount::new(counted))
        .with_inputs(|| {
            // clean() mutates in place, so with_inputs (untimed) gets a fresh
            // clone each sample -- the clone itself isn't what's being
            // measured, clean()'s own work on it is
            let [r, g, b]: [Vec<u16>; 3] =
                samples.colors.clone().try_into().expect("3 color planes");
            let ir = samples.ir.clone().unwrap_or_default();
            let prescan = Prescan {
                red: &pre.colors[0],
                ir: &pre.ir,
                rows: ROWS / 6,
                cols: COLS / 6,
            };
            (r, g, b, ir, prescan)
        })
        .bench_values(|(mut r, mut g, mut b, ir, prescan)| {
            let cal = dust::calibrate(&prescan).expect("clear film in the prescan");
            dust::clean([&mut r, &mut g, &mut b], &ir, &cal, ROWS, COLS, &options())
        });
}

/// splitmix64, so the synthetic 35mm frame gets index-seeded noise without
/// pulling in a `rand` dependency just for a bench fixture
fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// A synthetic IR plane shaped like real clear film: mostly a tight band
/// near-white (`reconstruct_core`'s cost is data-dependent, so `clean_35mm`
/// needs the mask density right, not just the pixel count), with ~0.4% of
/// pixels dropped into dust range. That injection rate is tuned, not
/// physical -- `decide()` flags a wider neighborhood than the seed pixels
/// alone, and 0.4% lands the *flagged* fraction at ~1.2%, matching what
/// `clean`'s real scan_1.tiff fixture flags (see its `mask_dump.tiff` note)
fn synth_ir(n: usize) -> Vec<u16> {
    (0..n)
        .map(|i| {
            let x = splitmix(i as u64 ^ 0xA1);
            if x % 1000 < 4 {
                2000 + (splitmix(x) % 4000) as u16
            } else {
                61000 + (x % 500) as u16
            }
        })
        .collect()
}

/// A synthetic color plane: independent per-channel noise in a mid-scale
/// band, uncorrelated with `synth_ir`'s dust dips the way a real dust speck's
/// visible-channel signature is much weaker than its IR one
fn synth_color(n: usize, salt: u64) -> Vec<u16> {
    (0..n)
        .map(|i| 30000 + (splitmix(i as u64 ^ salt) % 1000) as u16)
        .collect()
}

/// Fully synthetic color+IR planes at the 35mm shape. There's no
/// scan_1.tiff-equivalent capture for this format in the corpus, so unlike
/// `clean` this bench can't lean on a real fixture at all
static SYNTH_35: LazyLock<Planes> = LazyLock::new(|| {
    let n = ROWS_35 * COLS_35;
    Planes {
        colors: vec![
            synth_color(n, 0xB1),
            synth_color(n, 0xB2),
            synth_color(n, 0xB3),
        ],
        ir: synth_ir(n),
    }
});

/// `SYNTH_35`'s prescan, decimated the same way `SMALL_PRESCAN` is off a real
/// scan
static SYNTH_35_PRESCAN: LazyLock<Planes> = LazyLock::new(|| Planes {
    colors: vec![decimate(&SYNTH_35.colors[0], 6, ROWS_35, COLS_35)],
    ir: decimate(&SYNTH_35.ir, 6, ROWS_35, COLS_35),
});

/// End-to-end `clean()` throughput on a synthetic 35mm frame at 4000 DPI --
/// the shape a full-frame strip scan actually produces, which nothing else
/// in this file exercises
#[divan::bench]
fn clean_35mm(bencher: divan::Bencher) {
    let counted = SYNTH_35.colors.iter().map(Vec::len).sum::<usize>() + SYNTH_35.ir.len();
    bencher
        .counter(ItemsCount::new(counted))
        .with_inputs(|| {
            let [r, g, b]: [Vec<u16>; 3] =
                SYNTH_35.colors.clone().try_into().expect("3 color planes");
            let ir = SYNTH_35.ir.clone();
            let prescan = Prescan {
                red: &SYNTH_35_PRESCAN.colors[0],
                ir: &SYNTH_35_PRESCAN.ir,
                rows: ROWS_35 / 6,
                cols: COLS_35 / 6,
            };
            (r, g, b, ir, prescan)
        })
        .bench_values(|(mut r, mut g, mut b, ir, prescan)| {
            let cal = dust::calibrate(&prescan).expect("clear film in the synthetic prescan");
            dust::clean(
                [&mut r, &mut g, &mut b],
                &ir,
                &cal,
                ROWS_35,
                COLS_35,
                &options(),
            )
        });
}
