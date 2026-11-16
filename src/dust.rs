//! An implementation of IR-based dust removal following @a6o's [openICE](https://github.com/a6o/openICE)
//!
//! Some design notes: gotta go fast at all costs.
//! I don't care about matchin Nikon bit-for-bit. It need to look correct and not waste my time.

use crate::scan::profile::Film;
use rayon::prelude::*;
use std::sync::LazyLock;
use wide::f32x4;

// -----  constants

/// The R->IR leakage ICE uses for a positive when no clear tile measures one (ICEDLL.dll 3.0.0.4012, 0x109cc0a8)
const POSITIVE_LEAKAGE: f32 = 0.14;

/// The R->IR leakage ICE uses for a negative when no clear tile measures one (ICEDLL.dll 3.0.0.4012, 0x109cc0a4)
const NEGATIVE_LEAKAGE: f32 = 0.17;

/// The maximum value of a 16-bit sample
const M: f32 = 65535.0;

/// The raw IR value above which film counts as "clear"
const TAU: f32 = 8847.23;

/// Only consider dye leakage between these values (+/-). Another strange Nikon constant.
const SLOPE_LIMIT: f32 = 0.2;

/// Weight floor
const W_FLOOR: f32 = 0.02;

/// Clean-film margin anchor: `b = D(floor(0.98M)) - M`
const B_ANCHOR: f32 = 0.98;

/// Dust floor anchor: `phi = D(floor(0.065M))`
const PHI_ANCHOR: f32 = 0.065;

/// What resolution we switch to the weird nikon horizontally-adjecent pixel thing
const MIN3_DPI: u32 = 550;

/// Detail-band gain, indexed by [`Quality`]
const DETAIL_GAIN: [f32; 2] = [1.25, 1.0];

/// Dither amplitude per channel
const ALPHA: [f32; 3] = [0.015, 0.015, 0.025];

/// How many samples one rayon task takes on a whole-plane pass
const CHUNK: usize = 1 << 16;

/// Which scanner's constants to run. ICE calls these profiles "kinds"
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Model {
    /// Kind 7
    #[default]
    Ls9000,
    /// Kind 8
    Ls5000,
    /// Kind 9
    Ls50,
}

/// Every unit we know of onto the three kinds ICE ships.
///
/// Not the same grouping as [`crate::scan::profile`]'s `Family`, which pairs
/// the LS-50 with the LS-5000 because their color measurements are identical.
/// ICE gives the LS-50 its own coefficient table, so here they are apart.
///
/// Only the LS-9000, LS-5000 and LS-50 are in pipeline.md. The other three
/// are put with the sibling they share a generation and a sensor format with,
/// which is a guess, not something the document says
impl From<crate::protocol::model::Model> for Model {
    fn from(model: crate::protocol::model::Model) -> Self {
        use crate::protocol::model::Model as Unit;
        match model {
            Unit::Ls9000 | Unit::Ls8000 => Self::Ls9000,
            Unit::Ls5000 | Unit::Ls4000 => Self::Ls5000,
            Unit::Ls50 | Unit::Ls40 => Self::Ls50,
        }
    }
}

/// Nikon Scan's two ICE settings
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    /// Detail boosted 1.25x, and no pixel comes out darker than it scanned
    #[default]
    Normal,
    /// Detail at unity with both clamps off, so every pixel is rewritten
    Fine,
}

/// The constants that differ between scanners
struct Profile {
    /// Gate bias
    theta: f32,
    /// IR-reference gain per channel
    gamma: [f32; 3],
    /// Dither band edges, as fractions of full scale
    dither: [f32; 2],
    /// At or below this resolution beta collapses to the center
    contrast_dpi: u32,
    /// Weight-ramp anchor, `D(floor(0.85M)) - M`
    ramp: f32,
    /// Soft-threshold coefficients, `[channel][band] = (a_hi, a_lo)`
    a: [[(f32, f32); 3]; 3],
}

/// Indexed by [`Model`]
const PROFILES: [Profile; 3] = [
    // Kind 7, LS-9000
    Profile {
        theta: 0.0,
        gamma: [1.100; 3],
        dither: [0.04, 0.96],
        contrast_dpi: 950,
        ramp: -960.42,
        a: [
            [(1.360, 1.320), (1.370, 1.300), (1.340, 1.250)],
            [(1.370, 1.300), (1.350, 1.290), (1.300, 1.240)],
            [(1.340, 1.250), (1.320, 1.250), (1.250, 1.210)],
        ],
    },
    // Kind 8, LS-5000
    Profile {
        theta: 1.0,
        gamma: [1.100; 3],
        dither: [0.01, 0.99],
        contrast_dpi: 1600,
        ramp: -960.42,
        a: [
            [(1.210, 1.090), (1.170, 1.080), (1.040, 0.960)],
            [(1.230, 1.130), (1.140, 1.050), (0.930, 0.840)],
            [(1.130, 1.040), (1.080, 1.020), (0.970, 0.890)],
        ],
    },
    // Kind 9, LS-50
    Profile {
        theta: 1.0,
        gamma: [1.000; 3],
        dither: [0.01, 0.99],
        contrast_dpi: 2500,
        ramp: -960.52,
        a: [
            [(2.210, 2.090), (2.170, 2.080), (2.040, 1.960)],
            [(2.230, 2.130), (2.140, 2.050), (1.930, 1.840)],
            [(2.130, 2.040), (2.080, 2.020), (1.970, 1.890)],
        ],
    },
];

/// Row spans `(dy, dx_lo, dx_hi)` of the 9x9 octagonal box: `max(|dx|,|dy|) <= 4` and `|dx| + |dy| <= 6`
const LEVEL0: [(i32, i32, i32); 9] = [
    (-4, -2, 2),
    (-3, -3, 3),
    (-2, -4, 4),
    (-1, -4, 4),
    (0, -4, 4),
    (1, -4, 4),
    (2, -4, 4),
    (3, -3, 3),
    (4, -2, 2),
];

/// The 5x5 octagon: `max(|dx|,|dy|) <= 2` and `|dx| + |dy| <= 3`
const LEVEL1: [(i32, i32, i32); 5] = [(-2, -1, 1), (-1, -2, 2), (0, -2, 2), (1, -2, 2), (2, -1, 1)];

/// The 3x3 binomial tent, weighted `T[dy] * T[dx] / 16`
const TENT: [f32; 3] = [1.0, 2.0, 1.0];

// ----- options

/// What to run the pipeline as
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// model options
    pub model: Model,
    /// quality options
    pub quality: Quality,
    /// scan resolution
    pub dpi: u32,
}

/// All the parameters needed to complete the pass
#[derive(Debug, Clone)]
pub struct Params {
    /// R->IR leakage slope
    c: f32,
    /// Clear-film IR density with the leakage taken out
    ir_ref: f32,
    /// Gate bias
    theta: f32,
    /// `IR_ref + b`
    ramp_bias: f32,
    /// `s`, the ramp's reciprocal slope
    ramp_s: f32,
    /// Dust floor
    phi: f32,
    /// Dither band edges, in density
    eta: [f32; 2],
    /// IR-reference gain per channel
    gamma: [f32; 3],
    /// Soft-threshold coefficients
    a: [[(f32, f32); 3]; 3],
    /// Detail-band gain
    detail_gain: f32,
    /// Clamp to `L_3`.
    /// Off in Fine
    clamp_l3: bool,
    /// Feed `w` the minimum of three horizontal gates instead
    min3: bool,
    /// Measure beta over the 5-point cross rather than the center
    cross_beta: bool,
}

impl Params {
    /// Resolve `opts` against what [`calibrate`] measured off the prescan
    pub fn new(opts: &Options, cal: &Calibration) -> Self {
        let profile = &PROFILES[opts.model as usize];
        Self {
            c: cal.c,
            ir_ref: cal.ir_ref,
            // The gate is read against this frame's own clear film, `ir_ref`,
            // so how far up the IR channel was exposed cancels out. Nikon Scan
            // meters IR to the same 85% this crate does, and its bias is all
            // there is to add
            theta: profile.theta,
            ramp_bias: cal.ir_ref + density(anchor(B_ANCHOR)) - M,
            ramp_s: 1.0 / profile.ramp,
            phi: density(anchor(PHI_ANCHOR)),
            eta: profile.dither.map(|f| density(anchor(f))),
            gamma: profile.gamma,
            a: profile.a,
            detail_gain: DETAIL_GAIN[opts.quality as usize],
            clamp_l3: matches!(opts.quality, Quality::Normal),
            min3: opts.dpi > MIN3_DPI,
            cross_beta: opts.dpi > profile.contrast_dpi,
        }
    }

    /// `w = clamp(1 + (IR_ref + b - g)s, w_floor, 1)`
    #[inline]
    fn weight(&self, gate: f32) -> f32 {
        (1.0 + (self.ramp_bias - gate) * self.ramp_s).clamp(W_FLOOR, 1.0)
    }
}

/// `floor(fraction * M)`
fn anchor(fraction: f32) -> u16 {
    (M * fraction) as u16
}

// ----- density calculations

/// `D(v) = M/16 * log2(v + 1)`
#[inline]
fn density(v: u16) -> f32 {
    (f32::from(v) + 1.0).log2() * (M / 16.0)
}

/// `D` for every 16-bit sample as a 256 KB LUT
static LUT: LazyLock<Box<[f32]>> = LazyLock::new(|| (0..=u16::MAX).map(density).collect());

/// [`LUT`] at a length every `u16` indexes without a bounds check
#[inline(always)]
fn lut() -> &'static [f32; 1 << 16] {
    LUT[..].try_into().expect("one density per sample")
}

/// Least-squares fit of `2^f` over `[0, 1)`, from `f^0` up, within 1.1e-7 of it
const EXP2_FIT: [f32; 7] = [
    1.0,
    0.693_146_8,
    0.240_231_1,
    0.055_478_91,
    0.009_686_188,
    0.001_238_241,
    0.000_218_712_55,
];

/// The exponents [`from_density_scalar`] keeps to, inside the normal `f32` range
const EXP2_LIMIT: f32 = 100.0;

/// `D^-1(d) = 2^(16d/M) - 1`, rounded and clamped to a 16-bit sample
///
/// In arithmetic alone, not a call to `exp2f`, so a loop of it runs in vector lanes
#[inline(always)]
fn from_density_scalar(d: f32) -> u16 {
    let x = (d * (16.0 / M)).clamp(-EXP2_LIMIT, EXP2_LIMIT);
    let n = nearest(x);
    let n = if n > x { n - 1.0 } else { n };
    let fit = EXP2_FIT.iter().rev().fold(0.0, |acc, &k| acc * (x - n) + k);
    // `2^n` from its biased exponent, which the mantissa of `MAGIC + n + bias` holds
    let scale =
        f32::from_bits(integer(n + (f32::MAX_EXP - 1) as f32) << (f32::MANTISSA_DIGITS - 1));
    // Rounded half away from zero, as `round` does
    let v = (fit * scale - 1.0).clamp(0.0, M);
    let n = nearest(v);
    integer(if v - n == 0.5 { n + 1.0 } else { n }) as u16
}

/// `1.5 * 2^23`, where a float's last mantissa bit is 1 for any whole number within `2^22` of it
const ROUNDER: f32 = 1.5 * MAGIC;

/// `v` rounded to the nearest whole number, ties to even, for `|v| < 2^22`.
/// Plain arithmetic, so it runs in vector lanes on SSE2, which has no vector rounding
#[inline(always)]
fn nearest(v: f32) -> f32 {
    (ROUNDER + v) - ROUNDER
}

/// `2^23`, where a float's last mantissa bit is 1
const MAGIC: f32 = (1u32 << (f32::MANTISSA_DIGITS - 1)) as f32;

/// The whole number `v` in `[0, 2^23)` as an integer, from the bits of `MAGIC + v`.
/// Exact, and unlike `as` it has no saturation to check, so a loop of it runs in vector lanes
#[inline(always)]
fn integer(v: f32) -> u32 {
    (MAGIC + v).to_bits() - MAGIC.to_bits()
}

/// Step 1: a whole plane of samples in log-density
pub fn to_density(samples: &[u16]) -> Vec<f32> {
    let lut = &*LUT;
    let mut out = vec![0.0f32; samples.len()];
    samples
        .par_chunks(CHUNK)
        .zip(out.par_chunks_mut(CHUNK))
        .for_each(|(src, dst)| {
            for (&s, d) in src.iter().zip(dst) {
                *d = lut[usize::from(s)];
            }
        });
    out
}

/// Step 9: a whole plane of densities back to linear samples
pub fn from_density(values: &[f32]) -> Vec<u16> {
    let scale = f32x4::splat(16.0 / M);
    let mut out = vec![0u16; values.len()];
    values
        .par_chunks(CHUNK)
        .zip(out.par_chunks_mut(CHUNK))
        .for_each(|(src, dst)| {
            let (src_lanes, src_tail) = src.as_chunks::<4>();
            let (dst_lanes, dst_tail) = dst.as_chunks_mut::<4>();
            for (s, d) in src_lanes.iter().zip(dst_lanes) {
                let v = (f32x4::new(*s) * scale).exp2() - f32x4::ONE;
                *d = v
                    .round_int()
                    .to_array()
                    .map(|v| v.clamp(0, i32::from(u16::MAX)) as u16);
            }
            for (&s, d) in src_tail.iter().zip(dst_tail) {
                *d = from_density_scalar(s);
            }
        });
    out
}

// -----  step 2, calibrate

/// The low-resolution view of the frame calibration measures against
pub struct Prescan<'a> {
    pub red: &'a [u16],
    pub ir: &'a [u16],
    pub rows: usize,
    pub cols: usize,
}

// The IR calibration terms measured off a prescan
#[derive(Debug, Clone)]
pub struct Calibration {
    /// R->IR leakage slope
    pub c: f32,
    /// Clear-film IR density with that leakage removed
    pub ir_ref: f32,
}

/// Step 2: measure `c` and `IR_ref` from a low-resolution scan of the frame.
///
/// Returns `None` when the prescan holds no clear film to measure against, which would otherwise divide by zero and poison the whole pass.
/// Where no clear tile measures `c`, the leakage ICE keeps for `film` stands in.
pub fn calibrate(prescan: &Prescan, film: Film) -> Option<Calibration> {
    // Log-densities of the red and IR channels
    let d_r = to_density(prescan.red);
    let d_ir = to_density(prescan.ir);

    // 1. The two reference levels, IR^2-weighted so the mean leans toward the clearest pixels
    let (num_r, num_ir, den) = (prescan.ir, &d_r, &d_ir)
        .into_par_iter()
        // Find all the "clear" film by thresholding from TAU
        .filter(|&(&ir, _, _)| f32::from(ir) > TAU)
        .fold(
            || (0.0f64, 0.0f64, 0.0f64),
            |(num_r, num_ir, den), (&ir, &r, &ir_dens)| {
                let w = f64::from(ir) * f64::from(ir);
                (
                    num_r + w * f64::from(r),
                    num_ir + w * f64::from(ir_dens),
                    den + w,
                )
            },
        )
        // Pairwise tree combination, for a numerically stable sum
        .reduce(|| (0.0, 0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2));

    if den == 0.0 {
        return None;
    }
    // Average red density of the IR-clear pixels
    let r_ref = (num_r / den) as f32;
    // Average IR density of the same, but contaminated with red leakage
    let ir_raw = (num_ir / den) as f32;

    // 2. The dye->IR crosstalk, a weighted least-squares slope over the 4x4 quadrants of every 8x8 tile that is clear film all the way through
    let cols = prescan.cols;
    let col_tiles = prescan.cols / 8;
    let (num, den): (f64, f64) = (0..(prescan.rows / 8) * col_tiles)
        .into_par_iter()
        .flat_map_iter(|tile| {
            let (row0, col0) = ((tile / col_tiles) * 8, (tile % col_tiles) * 8);
            let idx = |dy: usize, dx: usize| (row0 + dy) * cols + (col0 + dx);
            let mut quadrants: [Option<(f32, f32)>; 4] = [None; 4];

            // Only process 8x8 tiles that are "clear", so any variation inside one has to be dye rather than dust
            if (0..8).all(|dy| (0..8).all(|dx| f32::from(prescan.ir[idx(dy, dx)]) > TAU)) {
                let (mut tile_r, mut tile_ir, mut tile_raw_ir) = (0.0f32, 0.0f32, 0.0f32);
                for dy in 0..8 {
                    for dx in 0..8 {
                        let i = idx(dy, dx);
                        tile_r += d_r[i];
                        tile_ir += d_ir[i];
                        tile_raw_ir += f32::from(prescan.ir[i]);
                    }
                }

                // The four 4x4 quadrants (subtiles) of the 8x8 tile
                let corners = [(0, 0), (0, 4), (4, 0), (4, 4)];
                for (slot, (dy0, dx0)) in quadrants.iter_mut().zip(corners) {
                    let (mut q_r, mut q_ir) = (0.0f32, 0.0f32);
                    for dy in 0..4 {
                        for dx in 0..4 {
                            let i = idx(dy0 + dy, dx0 + dx);
                            q_r += d_r[i];
                            q_ir += d_ir[i];
                        }
                    }
                    let delta_r = q_r / 16.0 - tile_r / 64.0;
                    let delta_ir = q_ir / 16.0 - tile_ir / 64.0;
                    let slope = delta_ir / delta_r;
                    // Throw out the obvious outliers
                    if slope.is_finite() && slope.abs() <= SLOPE_LIMIT {
                        *slot = Some((slope, delta_r * delta_r * tile_raw_ir * tile_raw_ir));
                    }
                }
            }
            quadrants.into_iter().flatten()
        })
        // 3. Weighted average of the surviving quadrant slopes, as a fold
        .fold(
            || (0.0f64, 0.0f64),
            |(num, den), (slope, weight)| {
                (
                    num + f64::from(slope) * f64::from(weight),
                    den + f64::from(weight),
                )
            },
        )
        .reduce(|| (0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));

    // ICE uses its film's leakage where the calibration measures none
    let c = match (den > 0.0, film) {
        (true, _) => (num / den) as f32,
        (false, Film::Negative | Film::MonochromeNegative) => NEGATIVE_LEAKAGE,
        (false, Film::Positive | Film::Kodachrome) => POSITIVE_LEAKAGE,
    };

    Some(Calibration {
        c,
        ir_ref: (ir_raw - c * r_ref) / (1.0 - c),
    })
}

// ----- steps 3 to 9, one band of rows at a time
//
// Every step reads a few rows either side of a pixel at most, so a band of rows
// runs all of them in cache and the frame never holds a gate, weight or mask plane

/// Rows one task cleans
const BAND: usize = 32;

/// The columns a tile reads at most, so a band's working rows fit in L2
const SPAN: usize = 256;

/// Columns a tile reads past each side: a level's reach, the cross of neighbors and the minimum of three
const COL_HALO: usize = REACH + 2;

/// How far a pyramid level reaches past the pixel on any side, which the 9x9 box sets
const REACH: usize = 4;

/// How far the dark test of step 5 reaches past the pixel on any side
const DARK_REACH: usize = 4;

/// Rows a band reads past each edge: a level's reach and one more for the cross of neighbors
const HALO: usize = REACH + 1;

/// What each pyramid level sums per pixel: `w`, `w·g`, and `w·D` of each color
const QUANTITIES: usize = 5;

/// The levels that sum a neighborhood: the 9x9 and 5x5 boxes and the tent. Level 3 is the pixel alone
const SUMMED: usize = 3;

/// The rows the ring keeps, as tall as the 9x9 box
const RING: usize = 2 * REACH + 1;

/// The rows of one ring slot: each quantity, then the density of each color
const SLOT_ROWS: usize = QUANTITIES + 3;

/// The cells of a box, which is the total of its kernel
const fn cells(spans: &[(i32, i32, i32)]) -> f32 {
    let mut n = 0;
    let mut i = 0;
    while i < spans.len() {
        n += spans[i].2 - spans[i].1 + 1;
        i += 1;
    }
    n as f32
}

/// The total of the tent's kernel
const fn tent_total() -> f32 {
    let s = TENT[0] + TENT[1] + TENT[2];
    s * s
}

/// Each summed level's kernel total, which turns its sum of weights into `C_l`
const KERNEL_TOTAL: [f32; SUMMED] = [cells(&LEVEL0), cells(&LEVEL1), tent_total()];

/// The planes a tile reads, and the columns it covers
struct Frame<'a> {
    ir: &'a [u16],
    colors: [&'a [u16]; 3],
    rows: usize,
    /// The columns the tile reads, its own and its halo
    cols: usize,
    /// The columns of the frame
    pitch: usize,
    /// The frame column of the tile's first column
    left: usize,
    /// The tile's own columns, which it rebuilds
    own: std::ops::Range<usize>,
}

impl Frame<'_> {
    /// The tile's columns of row `r` of `plane`
    #[inline(always)]
    fn row<'p>(&self, plane: &'p [u16], r: usize) -> &'p [u16] {
        &plane[r * self.pitch + self.left..][..self.cols]
    }
}

/// A band's own rows `r0..` of the gate and the weight
struct Local<'a> {
    g: &'a [Padded],
    w: &'a [Padded],
    r0: usize,
}

/// The four normalized convolutions at one pixel
struct Levels {
    /// `C_l`, the confidence mass
    c: [f32; 4],
    /// `P_l`, the gate pyramid
    p: [f32; 4],
    /// `L_l` per channel
    l: [[f32; 4]; 3],
}

/// What one band rebuilt: offsets into its rows, and the new samples there
#[derive(Default)]
struct Rebuilt {
    at: Vec<u32>,
    rgb: Vec<[u16; 3]>,
}

/// One row of a tile at the width of the widest tile, so a loop over it needs no bounds checks.
/// A narrower tile leaves the columns past its own unread
type Row = [f32; SPAN];

/// A padded row's length: `REACH` zeros before the row and `3 * REACH` after it, 16 lanes more in all,
/// so a loop over it ends on a whole vector
const PADDED: usize = SPAN + 4 * REACH;

/// A row with zeros either side
type Padded = [f32; PADDED];

/// A row one column wider either side, which repeats its edge column
type Edged = [f32; SPAN + 2];

/// The row past the frame, which adds nothing
static ZEROS: Row = [0.0; SPAN];

/// The factor of the quantity `w` itself
static ONES: Padded = [1.0; PADDED];

/// The padded row past the frame
static PADDED_ZEROS: Padded = [0.0; PADDED];

/// The row inside a padded one
#[inline(always)]
fn inner(row: &Padded) -> &Row {
    row[REACH..][..SPAN].try_into().expect("a row")
}

/// A thread's working rows, kept across the tiles it takes so a tile allocates only what it returns
#[derive(Default)]
struct Scratch {
    /// Steps 3 and 4 over the band's rows and halo, zero past both ends
    g: Vec<Padded>,
    w: Vec<Padded>,
    /// `g < phi` over the same rows
    dark: Vec<u8>,
    /// `dark` over 9 columns, over the same rows
    row_dark: Vec<u8>,
    /// `dark` over 9 rows, for one row
    col_dark: Vec<u8>,
    /// Whether step 5 keeps each pixel of one row
    keep: Vec<u8>,
    /// The pixels step 5 keeps, as frame indices
    mask: Vec<u32>,
    /// The quantities and densities of the last 9 rows, `[row % 9][quantity, then channel]`, zero past both ends
    ring: Vec<Padded>,
    /// One row rebuilt at every column, `[channel]`
    dense: Vec<Row>,
    /// The masked pixels of one row
    lanes: Lanes,
    /// The level sums of two rows, `[row % 2][level][quantity]`
    sums: Vec<Row>,
    /// What the level sums sum down before they sum across
    down: Option<Box<Down>>,
    /// The gate pyramid of three rows, `[row % 3][level]`
    p: Vec<Edged>,
}

/// Steps 3 and 4 over the frame's rows `r0..r1`
#[inline(always)]
fn gate_and_weigh(
    f: &Frame,
    p: &Params,
    g: &mut Vec<Padded>,
    w: &mut Vec<Padded>,
    r0: usize,
    r1: usize,
) {
    let (cols, lut) = (f.cols, lut());
    // Densities add, so killing the dye is a subtraction, not a division
    let (c, inv_c) = (p.c, 1.0 / (1.0 - p.c));
    g.resize(r1 - r0, PADDED_ZEROS);
    // A loop, not a closure, so it runs with the CPU features of its caller
    for (r, g) in (r0..r1).zip(g.iter_mut()) {
        let g = &mut g[REACH..][..cols];
        for ((g, &ir), &red) in g.iter_mut().zip(f.row(f.ir, r)).zip(f.row(f.colors[0], r)) {
            *g = (lut[usize::from(ir)] - c * lut[usize::from(red)]) * inv_c - p.theta;
        }
    }
    w.resize(g.len(), PADDED_ZEROS);
    for (g, w) in g.iter().zip(w.iter_mut()) {
        let (g, w) = (&g[REACH..][..cols], &mut w[REACH..][..cols]);
        if !p.min3 {
            for (&g, w) in g.iter().zip(w) {
                *w = p.weight(g);
            }
            continue;
        }
        // The two edge columns repeat themselves, which leaves the interior as three flat slices
        let last = cols - 1;
        for x in [0, last] {
            w[x] = p.weight(g[x.saturating_sub(1)].min(g[x]).min(g[(x + 1).min(last)]));
        }
        if cols >= 3 {
            let (lo, mid, hi) = (&g[..cols - 2], &g[1..last], &g[2..]);
            for (((&lo, &mid), &hi), w) in lo.iter().zip(mid).zip(hi).zip(&mut w[1..last]) {
                *w = p.weight(lo.min(mid).min(hi));
            }
        }
    }
}

/// Step 5: the pixels of rows `y0..y1` worth rebuilding, as frame indices
///
/// A pixel is kept unless the film is dark across 9 columns 4 rows above or below it,
/// or across 9 rows 4 columns either side, each window clamped to the frame
#[inline(always)]
fn decide(f: &Frame, p: &Params, s: &mut Scratch, r0: usize, y0: usize, y1: usize) {
    let (rows, cols) = (f.rows, f.cols);
    let Scratch {
        g,
        w,
        dark,
        row_dark,
        col_dark,
        keep,
        mask,
        ..
    } = s;
    keep.resize(cols, 0);
    col_dark.clear();
    col_dark.resize(cols + 2 * DARK_REACH, 1);
    dark.clear();
    for g in g.iter() {
        dark.extend(g[REACH..][..cols].iter().map(|&g| u8::from(g < p.phi)));
    }

    // Across 9 columns, every row of the band and its halo. Past both ends counts as dark,
    // so the clamped window is the full one. Each pass doubles the run it ANDs: 2, 4, 8, then 9
    row_dark.clear();
    row_dark.resize(dark.len(), 0);
    for (src, dst) in dark.chunks_exact(cols).zip(row_dark.chunks_exact_mut(cols)) {
        let padded = &mut col_dark[..cols + 2 * DARK_REACH];
        padded[..DARK_REACH].fill(1);
        padded[DARK_REACH..DARK_REACH + cols].copy_from_slice(src);
        let mut width = 1;
        while width < 2 * DARK_REACH {
            for x in 0..padded.len() - width {
                padded[x] &= padded[x + width];
            }
            width *= 2;
        }
        // `padded[x]` is now `src[x - 4..x + 4]`, and the ninth column completes it
        for (x, d) in dst.iter_mut().enumerate() {
            *d = padded[x] & *src.get(x + DARK_REACH).unwrap_or(&1);
        }
    }

    let local = |r: usize| (r - r0) * cols..(r - r0 + 1) * cols;
    let (own, n) = (f.own.clone(), f.own.len());
    mask.clear();
    for y in y0..y1 {
        // Across 9 rows, this row only, `DARK_REACH` columns past each end that repeat the edge one,
        // so each side of `x` is `x` and `x + 2 * DARK_REACH`
        let sides = &mut col_dark[..cols + 2 * DARK_REACH];
        sides.fill(1);
        for r in y.saturating_sub(DARK_REACH)..=(y + DARK_REACH).min(rows - 1) {
            for (d, &s) in sides[DARK_REACH..].iter_mut().zip(&dark[local(r)]) {
                *d &= s;
            }
        }
        let (first, last) = (sides[DARK_REACH], sides[DARK_REACH + cols - 1]);
        sides[..DARK_REACH].fill(first);
        sides[DARK_REACH + cols..].fill(last);
        let (left, right) = (
            &sides[own.start..][..n],
            &sides[own.start + 2 * DARK_REACH..][..n],
        );
        let above = &row_dark[local(y.saturating_sub(DARK_REACH))][own.clone()];
        let below = &row_dark[local((y + DARK_REACH).min(rows - 1))][own.clone()];
        let w = &inner(&w[y - r0])[own.clone()];
        let keep = &mut keep[..n];
        for x in 0..n {
            let clear = above[x] | below[x] | left[x] | right[x] == 0;
            keep[x] = u8::from(clear & !(p.clamp_l3 & (w[x] >= 1.0)));
        }
        // 64 columns to a word, so a run of unkept ones costs one test
        let base = (y * f.pitch + f.left + own.start) as u32;
        for (k, chunk) in keep.chunks(64).enumerate() {
            let mut bits = 0u64;
            for (b, &kept) in chunk.iter().enumerate() {
                bits |= u64::from(kept) << b;
            }
            while bits != 0 {
                mask.push(base + (k * 64) as u32 + bits.trailing_zeros());
                bits &= bits - 1;
            }
        }
    }
}

/// The quantities of input row `r` into its ring slot, `[quantity]`, then its densities, `[channel]`
#[inline(always)]
fn row_quantities(f: &Frame, local: &Local, slot: &mut [Padded], r: usize) {
    let (cols, lut) = (f.cols, lut());
    let k = r - local.r0;
    let (quantities, densities) = slot[..SLOT_ROWS].split_at_mut(QUANTITIES);
    for (plane, density) in f.colors.iter().zip(densities.iter_mut()) {
        for (d, &v) in density[REACH..][..cols].iter_mut().zip(f.row(plane, r)) {
            *d = lut[usize::from(v)];
        }
    }
    // The weight is zero past both ends, so every quantity is
    let w = &local.w[k];
    for (qty, out) in quantities.iter_mut().enumerate() {
        let factor = match qty {
            0 => &ONES,
            1 => &local.g[k],
            _ => &densities[qty - 2],
        };
        for i in 0..PADDED {
            out[i] = w[i] * factor[i];
        }
    }
}

/// The rows of one quantity an octagon sums down at `x`, from `v[REACH + dy]`, the row `dy` away:
/// `wide[k - 1]` sums the rows at least `k` columns either side
#[inline(always)]
fn octagon_down(spans: &[(i32, i32, i32)], v: &[f32; RING], wide: &mut [Padded; REACH], x: usize) {
    let mut reach = 0;
    for &(_, _, r) in spans {
        reach = reach.max(r as usize);
    }
    let mut sum = 0.0;
    for k in (1..=reach).rev() {
        for &(dy, _, r) in spans {
            if r as usize == k {
                sum += v[REACH.wrapping_add_signed(dy as isize)];
            }
        }
        wide[k - 1][x] = sum;
    }
}

/// The octagon `spans` across the rows [`octagon_down`] summed, into `out`: each column offset `dx`
/// takes the rows for `max(|dx|, 1)`
#[inline(always)]
fn octagon_across(spans: &[(i32, i32, i32)], wide: &[Padded; REACH], out: &mut Row) {
    let mut reach = 0;
    for &(_, _, r) in spans {
        reach = reach.max(r as usize);
    }
    for x in 0..SPAN {
        let mut sum = 0.0;
        for dx in -(reach as isize)..=reach as isize {
            sum += wide[dx.unsigned_abs().max(1) - 1][(x + REACH).wrapping_add_signed(dx)];
        }
        out[x] = sum;
    }
}

/// The level sums of row `y` from the ring into `out`, `[level][quantity]`
///
/// Each quantity sums down once for every level, reading each ring row once, then across
#[inline(always)]
fn level_sums(ring: &[Padded], down: &mut Down, out: &mut [Row], r0: usize, r1: usize, y: usize) {
    for qty in 0..QUANTITIES {
        let mut rows: [&Padded; RING] = [&PADDED_ZEROS; RING];
        for (i, row) in rows.iter_mut().enumerate() {
            if let Some(r) = (y + i).checked_sub(REACH).filter(|r| (r0..r1).contains(r)) {
                *row = &ring[(r % RING) * SLOT_ROWS + qty];
            }
        }
        let Down { wide, tent } = down;
        let [box9, box5] = wide;
        for x in 0..PADDED {
            let mut v = [0.0; RING];
            for (v, row) in v.iter_mut().zip(&rows) {
                *v = row[x];
            }
            octagon_down(&LEVEL0, &v, box9, x);
            octagon_down(&LEVEL1, &v, box5, x);
            tent[x] = TENT[0] * v[REACH - 1] + TENT[1] * v[REACH] + TENT[2] * v[REACH + 1];
        }
        octagon_across(&LEVEL0, box9, &mut out[qty]);
        octagon_across(&LEVEL1, box5, &mut out[QUANTITIES + qty]);
        for (x, out) in out[2 * QUANTITIES + qty].iter_mut().enumerate() {
            let c = REACH + x;
            *out = TENT[0] * tent[c - 1] + TENT[1] * tent[c] + TENT[2] * tent[c + 1];
        }
    }
}

/// What the level sums sum down before they sum across
struct Down {
    /// [`octagon_down`] of the 9x9 and the 5x5 box
    wide: [[Padded; REACH]; 2],
    /// The tent's rows weighted
    tent: Padded,
}

/// The gate pyramid of a row of level sums and its gate `g`, `[level]`
#[inline(always)]
fn gate_pyramid(sums: &[Row], g: &Row, p: &mut [Edged], cols: usize) {
    for (level, p) in p[..=SUMMED].iter_mut().enumerate() {
        if level == SUMMED {
            p[1..=SPAN].copy_from_slice(g);
        } else {
            let (w, wg) = (&sums[level * QUANTITIES], &sums[level * QUANTITIES + 1]);
            for x in 0..SPAN {
                // Divided whether or not it is kept, so the loop needs no masked loads
                let ratio = wg[x] / w[x];
                p[x + 1] = if w[x] > 0.0 { ratio } else { 0.0 };
            }
        }
        (p[0], p[cols + 1]) = (p[1], p[cols]);
    }
}

/// What the rebuild of one row reads
#[derive(Clone, Copy)]
struct RowView<'a> {
    /// The level sums, `[level][quantity]`
    sums: [[&'a Row; QUANTITIES]; SUMMED],
    /// The gate pyramids of the rows above, here and below, then of the columns left and right
    /// of here, `[neighbor][level]`. A row or column past the frame repeats this one, which adds no contrast
    pyramids: [[&'a Row; SUMMED + 1]; 5],
    /// The weight, level 3 of `C`
    w: &'a Row,
    /// The densities, level 3 of `L`
    density: [&'a Row; 3],
}

impl RowView<'_> {
    /// The gate pyramid of `neighbor` (0 above, 1 here, 2 below, 3 left, 4 right) at `x`
    #[inline(always)]
    fn pyramid(&self, neighbor: usize, x: usize) -> [f32; 4] {
        let p = &self.pyramids[neighbor];
        [p[0][x], p[1][x], p[2][x], p[3][x]]
    }

    /// The four levels at `x`
    #[inline(always)]
    fn levels(&self, x: usize) -> Levels {
        let mut out = Levels {
            c: [0.0; 4],
            p: self.pyramid(1, x),
            l: [[0.0; 4]; 3],
        };
        for level in 0..SUMMED {
            let w = self.sums[level][0][x];
            let inv = if w > 0.0 { 1.0 / w } else { 0.0 };
            out.c[level] = w * (1.0 / KERNEL_TOTAL[level]);
            out.p[level] = self.sums[level][1][x] * inv;
            for (ch, l) in out.l.iter_mut().enumerate() {
                l[level] = self.sums[level][2 + ch][x] * inv;
            }
        }
        out.c[3] = self.w[x];
        for (l, density) in out.l.iter_mut().zip(self.density) {
            l[3] = density[x];
        }
        out
    }
}

/// Uniform `[0, 1)`, fixed per (pixel, channel).
///
/// NOTE: we're not using a real RNG for perf reasons.
/// https://xkcd.com/221/
#[inline(always)]
fn uniform(pixel: usize, channel: usize) -> f32 {
    let mut x = (pixel as u64) << 2 | channel as u64;
    // Stafford's Mix13
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    // 24 bits fit an i32, which converts to f32 in one instruction on every vector width
    ((x ^ (x >> 31)) & 0x00ff_ffff) as i32 as f32 / (1u32 << 24) as f32
}

#[inline(always)]
/// The dither of `x` in `channel`, from `u`, its [`uniform`]
fn dither(x: f32, u: f32, channel: usize, eta: [f32; 2]) -> f32 {
    let [lo, hi] = eta;
    let envelope = 4.0 / (hi - lo).powi(2) * (x - lo) * (hi - x);
    let d = envelope * (u - 0.5) * ALPHA[channel] * x;
    // Selected rather than branched, so the masked pixels of a row run in vector lanes
    let inside = (x > lo) & (x < hi) & (x + d > lo) & (x + d < hi);
    if inside { d } else { 0.0 }
}

/// The rebuilt densities at `x` before dither, with the pixels `left` and `right` of it
///
/// Branch-free, so a row of it runs in vector lanes
#[inline(always)]
fn rebuild_at<const CROSS: bool>(p: &Params, v: &RowView, x: usize) -> [f32; 3] {
    let center = v.levels(x);
    let mut lo = [0.0f32; 4];
    let mut hi = [0.0f32; 4];
    for level in 1..=3 {
        lo[level] = center.p[level] - center.p[level - 1];
        hi[level] = lo[level];
    }
    // Left, right, above and below, by index, so the loop unrolls before it vectorizes
    if CROSS {
        for neighbor in [3, 4, 0, 2] {
            let n = v.pyramid(neighbor, x);
            for level in 1..=3 {
                let d = n[level] - n[level - 1];
                (lo[level], hi[level]) = (lo[level].min(d), hi[level].max(d));
            }
        }
    }

    let mut out = [0.0; 3];
    for (ch, out) in out.iter_mut().enumerate() {
        // the blurred base plus the light the 9x9 window lost
        let mut a = center.l[ch][0] + p.gamma[ch] * (p.ir_ref - center.p[0]);

        // each detail band, dead-zoned against the IR contrast and scaled by how much clean film backed the estimate
        for level in 1..=3 {
            let detail = (center.l[ch][level] - center.l[ch][level - 1]) * p.detail_gain;
            let (a_hi, a_lo) = p.a[ch][level - 1];
            // A band entirely below zero swaps them, so the larger coefficient always scales the edge further from zero
            let (a_lo, a_hi) = if hi[level] < 0.0 {
                (a_hi, a_lo)
            } else {
                (a_lo, a_hi)
            };
            let (lo_t, hi_t) = (a_lo * lo[level], a_hi * hi[level]);
            // The part of `detail` outside `[lo_t, hi_t]`. Every profile has `a_hi >= a_lo` (a test checks), so
            // `lo_t <= hi_t` and at most one term is not zero. No branch, so NEON keeps it in vector lanes
            let r = (detail - lo_t).min(0.0) + (detail - hi_t).max(0.0);
            let confidence = match level {
                1 => (2.0 * center.c[1]).min(1.0),
                2 => center.c[2],
                _ => center.c[3] * center.c[3],
            };
            a += r * confidence;
        }
        *out = a;
    }
    out
}

/// The masked pixels of row `y`, rebuilt into `out` as 16-bit samples
///
/// The whole row runs in vector lanes into `dense`, then the masked pixels take theirs
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn rebuild_row(
    f: &Frame,
    p: &Params,
    v: &RowView,
    dense: &mut [Row],
    lanes: &mut Lanes,
    mask: &[u32],
    next: &mut usize,
    y: usize,
) {
    let cols = f.cols;
    let row = y * f.pitch + f.left;
    let end = (row + cols) as u32;
    if mask.get(*next).is_none_or(|&i| i >= end) {
        return;
    }
    let [red, green, blue] = dense.get_disjoint_mut([0, 1, 2]).expect("three channels");
    // A constant in each, so neither loop loads the neighbors it does not use
    if p.cross_beta {
        for x in 0..SPAN {
            [red[x], green[x], blue[x]] = rebuild_at::<true>(p, v, x);
        }
    } else {
        for x in 0..SPAN {
            [red[x], green[x], blue[x]] = rebuild_at::<false>(p, v, x);
        }
    }

    // The masked pixels into lanes, which the band finishes all at once
    let start = *next;
    while *next < mask.len() && mask[*next] < end {
        *next += 1;
    }
    let samples = [
        f.row(f.colors[0], y),
        f.row(f.colors[1], y),
        f.row(f.colors[2], y),
    ];
    lanes.fill(
        &mask[start..*next],
        [red, green, blue],
        v.density,
        samples,
        row,
    );
}

/// The lanes [`finish_lanes`] takes at once
const CHUNK_LANES: usize = 64;

/// The masked pixels of a band, one lane each, `[channel][lane]`
#[derive(Default)]
struct Lanes {
    /// The pixels, as frame indices
    i: Vec<u32>,
    /// The rebuilt density before dither
    a: [Vec<f32>; 3],
    /// The density as scanned
    scanned: [Vec<f32>; 3],
    /// The sample as scanned
    sample: [Vec<u16>; 3],
    /// The [`uniform`] of each pixel and channel
    u: [Vec<f32>; 3],
    /// The sample rebuilt
    rgb: [Vec<u16>; 3],
    /// Whether the clamp kept the pixel
    keep: Vec<bool>,
}

impl Lanes {
    /// Empty, for a new band
    fn clear(&mut self) {
        self.i.clear();
        for ch in 0..3 {
            self.a[ch].clear();
            self.scanned[ch].clear();
            self.sample[ch].clear();
        }
    }

    /// Gather `pixels` of the row that starts at frame index `row`, whose samples are `samples`
    #[inline(always)]
    fn fill(
        &mut self,
        pixels: &[u32],
        dense: [&mut Row; 3],
        density: [&Row; 3],
        samples: [&[u16]; 3],
        row: usize,
    ) {
        let (start, n) = (self.i.len(), pixels.len());
        self.i.extend_from_slice(pixels);
        for ch in 0..3 {
            // The row's samples at a fixed width, so a column indexes it as it does the others
            let mut sampled = [0; SPAN];
            sampled[..samples[ch].len()].copy_from_slice(samples[ch]);
            self.a[ch].resize(start + n, 0.0);
            self.scanned[ch].resize(start + n, 0.0);
            self.sample[ch].resize(start + n, 0);
            let lanes = self.a[ch][start..]
                .iter_mut()
                .zip(&mut self.scanned[ch][start..]);
            for ((&i, (a, scanned)), sample) in
                pixels.iter().zip(lanes).zip(&mut self.sample[ch][start..])
            {
                // Below `SPAN` already, which the remainder shows the compiler
                let x = (i as usize - row) % SPAN;
                (*a, *scanned, *sample) = (dense[ch][x], density[ch][x], sampled[x]);
            }
        }
    }

    /// Dither, clamp if `CLAMP` and back to linear, step 9, in every lane
    #[inline(always)]
    fn finish<const CLAMP: bool>(&mut self, p: &Params) {
        // Whole chunks, the lanes past the last pixel left zero and unread
        let n = self.i.len().next_multiple_of(CHUNK_LANES);
        self.i.resize(n, 0);
        for ch in 0..3 {
            self.a[ch].resize(n, 0.0);
            self.scanned[ch].resize(n, 0.0);
            self.sample[ch].resize(n, 0);
            self.u[ch].resize(n, 0.0);
            self.rgb[ch].resize(n, 0);
        }
        self.keep.resize(n, false);
        let [u0, u1, u2] = &mut self.u;
        let [r, g, b] = &mut self.rgb;
        finish_lanes::<CLAMP>(
            p.eta,
            &self.i,
            [&self.a[0], &self.a[1], &self.a[2]],
            [&self.scanned[0], &self.scanned[1], &self.scanned[2]],
            [&self.sample[0], &self.sample[1], &self.sample[2]],
            [u0, u1, u2],
            [r, g, b],
            &mut self.keep,
        );
    }
}

/// [`Lanes::finish`] over separate slices, so the compiler knows the stores touch none of the loads
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn finish_lanes<const CLAMP: bool>(
    eta: [f32; 2],
    i: &[u32],
    a: [&[f32]; 3],
    scanned: [&[f32]; 3],
    sample: [&[u16]; 3],
    u: [&mut [f32]; 3],
    rgb: [&mut [u16]; 3],
    keep: &mut [bool],
) {
    // The hash in a loop of its own, which vectorizes where the whole step would not
    let [u0, u1, u2] = u;
    for (ch, u) in [&mut *u0, &mut *u1, &mut *u2].into_iter().enumerate() {
        for (u, &i) in u.iter_mut().zip(i) {
            *u = uniform(i as usize, ch);
        }
    }
    let [r, g, b] = rgb;
    let (r, g, b) = (
        r.as_chunks_mut::<CHUNK_LANES>().0,
        g.as_chunks_mut::<CHUNK_LANES>().0,
        b.as_chunks_mut::<CHUNK_LANES>().0,
    );
    let keep = keep.as_chunks_mut::<CHUNK_LANES>().0;
    // A chunk of fixed length at a time, so no lane needs a bounds check
    for c in 0..i.len() / CHUNK_LANES {
        let u: [&[f32; CHUNK_LANES]; 3] = [
            &u0.as_chunks().0[c],
            &u1.as_chunks().0[c],
            &u2.as_chunks().0[c],
        ];
        let a: [&[f32; CHUNK_LANES]; 3] = [
            &a[0].as_chunks().0[c],
            &a[1].as_chunks().0[c],
            &a[2].as_chunks().0[c],
        ];
        let scanned: [&[f32; CHUNK_LANES]; 3] = [
            &scanned[0].as_chunks().0[c],
            &scanned[1].as_chunks().0[c],
            &scanned[2].as_chunks().0[c],
        ];
        let sample: [&[u16; CHUNK_LANES]; 3] = [
            &sample[0].as_chunks().0[c],
            &sample[1].as_chunks().0[c],
            &sample[2].as_chunks().0[c],
        ];
        let (r, g, b, keep) = (&mut r[c], &mut g[c], &mut b[c], &mut keep[c]);
        for j in 0..CHUNK_LANES {
            let mut acc = [a[0][j], a[1][j], a[2][j]];
            for (ch, acc) in acc.iter_mut().enumerate() {
                *acc += dither(*acc, u[ch][j], ch, eta);
            }
            keep[j] = !CLAMP | ((acc[0] > 0.0) & (acc[1] > 0.0) & (acc[2] > 0.0));
            let mut out = [0; 3];
            for (ch, (&acc, out)) in acc.iter().zip(&mut out).enumerate() {
                // In Normal a rebuilt pixel never comes out darker than it scanned
                let acc = if CLAMP { acc.max(scanned[ch][j]) } else { acc };
                let s = from_density_scalar(acc);
                *out = if CLAMP { s.max(sample[ch][j]) } else { s };
            }
            [r[j], g[j], b[j]] = out;
        }
    }
}

/// What the rebuild of row `y` reads. `below` says if the frame has a row below it
#[inline(always)]
fn row_view<'a>(
    local: &Local<'a>,
    sums: &'a [Row],
    pyramids: &'a [Edged],
    ring: &'a [Padded],
    y: usize,
    below: bool,
) -> RowView<'a> {
    // Loops, not closures, which the compiler may leave out of line and so lose the lengths
    let sums = &sums[(y % 2) * SUMMED * QUANTITIES..];
    let densities = &ring[(y % RING) * SLOT_ROWS + QUANTITIES..][..3];
    let rows = [y.saturating_sub(1), y, if below { y + 1 } else { y }];
    let mut view = RowView {
        sums: [[&ZEROS; QUANTITIES]; SUMMED],
        pyramids: [[&ZEROS; SUMMED + 1]; 5],
        w: inner(&local.w[y - local.r0]),
        density: [&ZEROS; 3],
    };
    for (level, sums_at) in view.sums.iter_mut().enumerate() {
        for (qty, s) in sums_at.iter_mut().enumerate() {
            *s = &sums[level * QUANTITIES + qty];
        }
    }
    // Here, left and right are one row at three offsets
    let offsets = [1, 1, 1, 0, 2];
    for (k, (&r, offset)) in rows.iter().chain(&[y, y]).zip(offsets).enumerate() {
        for (level, p) in view.pyramids[k].iter_mut().enumerate() {
            let edged = &pyramids[(r % 3) * (SUMMED + 1) + level];
            *p = edged[offset..][..SPAN].try_into().expect("a row");
        }
    }
    for (ch, d) in view.density.iter_mut().enumerate() {
        *d = inner(&densities[ch]);
    }
    view
}

/// Steps 3 to 9 over rows `y0..y1`
#[inline(always)]
fn clean_band(f: &Frame, p: &Params, s: &mut Scratch, y0: usize, y1: usize) -> Rebuilt {
    let (rows, cols) = (f.rows, f.cols);
    let (r0, r1) = (y0.saturating_sub(HALO), (y1 + HALO).min(rows));
    gate_and_weigh(f, p, &mut s.g, &mut s.w, r0, r1);
    decide(f, p, s, r0, y0, y1);
    let mut out = Rebuilt::default();
    if s.mask.is_empty() {
        return out;
    }

    let Scratch {
        g,
        w,
        mask,
        ring,
        dense,
        lanes,
        sums,
        down,
        p: pyramids,
        ..
    } = s;
    let local = Local { g, w, r0 };
    let (sums_len, pyramid_len) = (SUMMED * QUANTITIES, SUMMED + 1);
    ring.resize(RING * SLOT_ROWS, PADDED_ZEROS);
    let down = down.get_or_insert_with(|| {
        Box::new(Down {
            wide: [[PADDED_ZEROS; REACH]; 2],
            tent: PADDED_ZEROS,
        })
    });
    dense.resize(3, ZEROS);
    sums.resize(2 * sums_len, ZEROS);
    pyramids.resize(3 * pyramid_len, [0.0; SPAN + 2]);
    let (mut next, mut filled) = (0, r0);
    lanes.clear();

    // Each row's sums and pyramid, then the row above it, which now has both neighbors
    for yy in y0.saturating_sub(1)..=y1.min(rows - 1) {
        while filled < r1.min(yy + REACH + 1) {
            row_quantities(
                f,
                &local,
                &mut ring[(filled % RING) * SLOT_ROWS..][..SLOT_ROWS],
                filled,
            );
            filled += 1;
        }
        let slot = &mut sums[(yy % 2) * sums_len..][..sums_len];
        level_sums(ring, down, slot, r0, r1, yy);
        gate_pyramid(
            slot,
            inner(&local.g[yy - r0]),
            &mut pyramids[(yy % 3) * pyramid_len..][..pyramid_len],
            cols,
        );
        if let Some(y) = yy.checked_sub(1).filter(|y| (y0..y1).contains(y)) {
            let view = row_view(&local, sums, pyramids, ring, y, true);
            rebuild_row(f, p, &view, dense, lanes, mask, &mut next, y);
        }
    }
    // The frame's last row has no row below it
    if y1 == rows {
        let y = rows - 1;
        let view = row_view(&local, sums, pyramids, ring, y, false);
        rebuild_row(f, p, &view, dense, lanes, mask, &mut next, y);
    }
    let count = lanes.i.len();
    // A constant in each, so neither loop loads what it does not use
    if p.clamp_l3 {
        lanes.finish::<true>(p);
    } else {
        lanes.finish::<false>(p);
    }
    // Every pixel written, and the count moved only past the kept ones, so no branch
    let start = (y0 * f.pitch) as u32;
    out.at.resize(count, 0);
    out.rgb.resize(count, [0; 3]);
    let mut kept = 0;
    let [r, g, b] = [
        &lanes.rgb[0][..count],
        &lanes.rgb[1][..count],
        &lanes.rgb[2][..count],
    ];
    for (j, (&i, &keep)) in lanes.i[..count]
        .iter()
        .zip(&lanes.keep[..count])
        .enumerate()
    {
        (out.at[kept], out.rgb[kept]) = (i - start, [r[j], g[j], b[j]]);
        kept += usize::from(keep);
    }
    out.at.truncate(kept);
    out.rgb.truncate(kept);
    out
}

/// [`clean_band`] compiled for the vector width of the CPU, chosen once per call by [`band_kernel`]
type BandKernel = unsafe fn(&Frame, &Params, &mut Scratch, usize, usize) -> Rebuilt;

fn band_portable(f: &Frame, p: &Params, s: &mut Scratch, y0: usize, y1: usize) -> Rebuilt {
    clean_band(f, p, s, y0, y1)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
fn band_avx2(f: &Frame, p: &Params, s: &mut Scratch, y0: usize, y1: usize) -> Rebuilt {
    clean_band(f, p, s, y0, y1)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl,avx512dq,avx2,fma")]
fn band_avx512(f: &Frame, p: &Params, s: &mut Scratch, y0: usize, y1: usize) -> Rebuilt {
    clean_band(f, p, s, y0, y1)
}

/// The widest [`clean_band`] this CPU runs
fn band_kernel() -> BandKernel {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512f")
            && is_x86_feature_detected!("avx512bw")
            && is_x86_feature_detected!("avx512vl")
            && is_x86_feature_detected!("avx512dq")
        {
            return band_avx512;
        }
        if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
            return band_avx2;
        }
    }
    band_portable
}

/// Remove dust from a frame like magic, returning how many pixels it rebuilt
///
/// [`calibrate`] measures `cal`, separately so a caller can retry a failed
/// measurement at a different scale, or hold one across a strip the way
/// --lock-ae holds an exposure
pub fn clean(
    color: [&mut [u16]; 3],
    ir: &[u16],
    cal: &Calibration,
    rows: usize,
    cols: usize,
    opts: &Options,
) -> usize {
    debug_assert!(
        color.iter().all(|c| c.len() == ir.len()),
        "one sample of each color per IR sample"
    );
    debug_assert_eq!(ir.len(), rows * cols, "rows * cols must cover the planes");
    if rows == 0 || cols == 0 {
        return 0;
    }
    let p = Params::new(opts, cal);
    let kernel = band_kernel();

    // Each tile owns `step` columns and reads `SPAN` columns, at least `COL_HALO` more either side.
    // A whole number of vectors wide, so no loop ends on a part of one
    let tiles = cols.div_ceil(SPAN - 2 * COL_HALO);
    let (step, span) = (cols.div_ceil(tiles), SPAN.min(cols));
    // Every tile reads the planes as they scanned, so none writes until all have run
    let rebuilt: Vec<Rebuilt> = {
        let colors = [&*color[0], &*color[1], &*color[2]];
        (0..rows.div_ceil(BAND) * tiles)
            .into_par_iter()
            .map_init(Scratch::default, |s, job| {
                let (y0, x0) = (job / tiles * BAND, job % tiles * step);
                let x1 = (x0 + step).min(cols);
                let left = x0.saturating_sub(COL_HALO).min(cols - span);
                let f = Frame {
                    ir,
                    colors,
                    rows,
                    cols: span,
                    pitch: cols,
                    left,
                    own: x0 - left..x1 - left,
                };
                // SAFETY: `band_kernel` picks a variant only when the CPU has its features
                unsafe { kernel(&f, &p, s, y0, (y0 + BAND).min(rows)) }
            })
            .collect()
    };

    let count = rebuilt.iter().map(|b| b.at.len()).sum();
    let [red, green, blue] = color;
    let band = BAND * cols;
    (
        red.par_chunks_mut(band),
        green.par_chunks_mut(band),
        blue.par_chunks_mut(band),
        rebuilt.par_chunks(tiles),
    )
        .into_par_iter()
        .for_each(|(red, green, blue, rebuilt)| {
            for rebuilt in rebuilt {
                for (&at, &[r, g, b]) in rebuilt.at.iter().zip(&rebuilt.rgb) {
                    let at = at as usize;
                    (red[at], green[at], blue[at]) = (r, g, b);
                }
            }
        });
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a clear 8x8 tile to measure it, the leakage is the one ICE keeps for the film
    #[test]
    fn every_profile_scales_the_far_edge_more() {
        // The dead zone in `rebuild_at` adds its two sides, which is exact only if `a_hi >= a_lo`
        for profile in &PROFILES {
            for &(a_hi, a_lo) in profile.a.iter().flatten() {
                assert!(a_hi >= a_lo && a_lo > 0.0, "{a_hi} {a_lo}");
            }
        }
    }

    #[test]
    fn an_unmeasured_leakage_is_the_film_default() {
        let (rows, cols) = (16, 16);
        // Clear and dense pixels alternate, so clear film exists but no tile is clear throughout
        let ir: Vec<u16> = (0..rows * cols)
            .map(|i| {
                if (i / cols + i % cols) % 2 == 0 {
                    50000
                } else {
                    1000
                }
            })
            .collect();
        let red = vec![30000; rows * cols];
        let prescan = Prescan {
            red: &red,
            ir: &ir,
            rows,
            cols,
        };

        for (film, c) in [
            (Film::Positive, 0.14),
            (Film::Kodachrome, 0.14),
            (Film::Negative, 0.17),
            (Film::MonochromeNegative, 0.17),
        ] {
            let cal = calibrate(&prescan, film).expect("clear film");
            assert_eq!(cal.c, c, "{film:?}");
        }
    }

    /// A fixed noise in `[-0.5, 0.5)` per pixel and stream
    fn noise(i: usize, stream: usize) -> f32 {
        uniform(i, stream) - 0.5
    }

    /// Flat film with 1% noise and infrared metered to 85%, crossed by
    /// scratches three columns wide that block `depths` of the light in every
    /// channel. Answers how much of the lost red each scratch got back
    fn recovered(depths: &[f32]) -> Vec<f64> {
        const ROWS: usize = 200;
        const PITCH: usize = 100;
        let cols = PITCH * depths.len();
        let depth = |x: usize| (x % PITCH >= 50 && x % PITCH < 53).then(|| depths[x / PITCH]);

        let scene = [30_000.0, 20_000.0, 15_000.0];
        let truth: [Vec<u16>; 3] = std::array::from_fn(|c| {
            (0..ROWS * cols)
                .map(|i| (scene[c] * (1.0 + 0.01 * noise(i, c))) as u16)
                .collect()
        });
        let mut ir: Vec<u16> = (0..ROWS * cols)
            .map(|i| (M * 0.85 * (1.0 + 0.01 * noise(i, 3))) as u16)
            .collect();
        let mut planes = truth.clone();
        for i in 0..ROWS * cols {
            if let Some(d) = depth(i % cols) {
                for plane in &mut planes {
                    plane[i] = (f32::from(plane[i]) * (1.0 - d)) as u16;
                }
                ir[i] = (f32::from(ir[i]) * (1.0 - d)) as u16;
            }
        }
        let dirty = planes[0].clone();

        let cal = calibrate(
            &Prescan {
                red: &planes[0],
                ir: &ir,
                rows: ROWS,
                cols,
            },
            Film::Positive,
        )
        .expect("clear film");
        let opts = Options {
            model: Model::Ls5000,
            quality: Quality::Normal,
            dpi: 4000,
        };
        let [r, g, b] = &mut planes;
        clean([r, g, b], &ir, &cal, ROWS, cols, &opts);

        (0..depths.len())
            .map(|n| {
                let (mut lost, mut back) = (0.0, 0.0);
                for y in 10..ROWS - 10 {
                    for x in n * PITCH + 50..n * PITCH + 53 {
                        let i = y * cols + x;
                        lost += f64::from(truth[0][i]) - f64::from(dirty[i]);
                        back += f64::from(planes[0][i]) - f64::from(dirty[i]);
                    }
                }
                back / lost
            })
            .collect()
    }

    /// A scratch that takes 3% off the infrared is rebuilt, with the infrared
    /// metered to 85% as the host meter does. Before, the gate moved with the
    /// metering target and let through anything under about 17%
    #[test]
    fn shallow_scratches_are_rebuilt() {
        let depths = [0.03, 0.05, 0.10, 0.30];
        for (depth, back) in depths.into_iter().zip(recovered(&depths)) {
            assert!(
                back > 0.9,
                "a {depth} scratch got {back:.2} of its light back"
            );
        }
    }
}
