//! An implementation of IR-based dust removal following @a6o's [openICE](https://github.com/a6o/openICE)
//!
//! Some design notes: gotta go fast at all costs.
//! I don't care about matchin Nikon bit-for-bit. It need to look correct and not waste my time.

use rayon::prelude::*;
use std::sync::LazyLock;
use wide::f32x4;

// -----  constants

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
    /// Fraction of full scale this crate's AE metered the IR channel to.
    /// Tracks [`crate::scan::meter::Metering::target`]
    pub metering_target: f32,
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
            theta: profile.theta + theta_for_metering_target(opts.metering_target),
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

/// Compute the gate-bias term for our AE target.
/// This is important because we intentionally don't AE to hit the upper limit of u16 to avoid clipping.
/// The ref doc values assume that the exprapolated brightest intensity is u16::MAX, so we'd end up flagging way more than we should.
pub fn theta_for_metering_target(target: f32) -> f32 {
    density(anchor(target.clamp(0.0, 1.0))) - M
}

// ----- density calculations

/// `D(v) = M/16 * log2(v + 1)`
#[inline]
fn density(v: u16) -> f32 {
    (f32::from(v) + 1.0).log2() * (M / 16.0)
}

/// `D` for every 16-bit sample as a 256 KB LUT
static LUT: LazyLock<Box<[f32]>> = LazyLock::new(|| (0..=u16::MAX).map(density).collect());

/// `D^-1(d) = 2^(16d/M) - 1`, rounded and clamped to a 16-bit sample
#[inline]
fn from_density_scalar(d: f32) -> u16 {
    let v = (d * (16.0 / M)).exp2() - 1.0;
    v.round().clamp(0.0, M) as u16
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
            let mut lanes = src.chunks_exact(4).zip(dst.chunks_exact_mut(4));
            for (s, d) in &mut lanes {
                let v = (f32x4::new(s.try_into().expect("chunked by four")) * scale).exp2()
                    - f32x4::ONE;
                let v = v.round_int().to_array();
                d.copy_from_slice(&v.map(|v| v.clamp(0, i32::from(u16::MAX)) as u16));
            }
            let done = src.len() / 4 * 4;
            for (&s, d) in src[done..].iter().zip(&mut dst[done..]) {
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
pub fn calibrate(prescan: &Prescan) -> Option<Calibration> {
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

    // No clear tile survived the slope filter
    // So, assume no leak rather than hand back a NaN that flags the whole frame
    let c = if den > 0.0 { (num / den) as f32 } else { 0.0 };

    Some(Calibration {
        c,
        ir_ref: (ir_raw - c * r_ref) / (1.0 - c),
    })
}

// ----- step 3, gate

/// strip the dye crosstalk out of IR, leaving `g`, which responds to defects only.
/// Fused with step 1, so red never needs a density plane of its own to avoid an alloc
pub fn gate(red: &[u16], ir: &[u16], p: &Params) -> Vec<f32> {
    debug_assert_eq!(red.len(), ir.len(), "one red and one IR sample per pixel");

    let lut = &*LUT;
    // Densities add, so killing the dye is a subtraction, not a division
    let (c, inv_c) = (p.c, 1.0 / (1.0 - p.c));
    let mut g = vec![0.0f32; ir.len()];
    ir.par_chunks(CHUNK)
        .zip(red.par_chunks(CHUNK))
        .zip(g.par_chunks_mut(CHUNK))
        .for_each(|((ir, red), g)| {
            for ((&ir, &red), g) in ir.iter().zip(red).zip(g) {
                *g = (lut[usize::from(ir)] - c * lut[usize::from(red)]) * inv_c - p.theta;
            }
        });
    g
}

// ----- step 4, confidence

/// Compute the clean-confidence weight, in `[w_floor, 1]`.
pub fn confidence(g: &[f32], cols: usize, p: &Params) -> Vec<f32> {
    let mut w = vec![0.0f32; g.len()];
    g.par_chunks(cols)
        .zip(w.par_chunks_mut(cols))
        .for_each(|(g, w)| {
            if !p.min3 {
                for (&g, w) in g.iter().zip(w) {
                    *w = p.weight(g);
                }
                return;
            }
            // The two edge columns repeat themselves.
            // splitting them off leaves the interior as three flat slices that vectorize
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
        });
    w
}

// ----- step 5, decide

/// AND of `src` over columns `x - k`, `x`, `x + k`, clamped at the edges
fn and3_cols(src: &[bool], cols: usize, k: usize) -> Vec<bool> {
    let mut out = vec![false; src.len()];
    src.par_chunks(cols)
        .zip(out.par_chunks_mut(cols))
        .for_each(|(src, dst)| {
            // The interior clamps nothing, so it stays three flat slices
            let body = cols.saturating_sub(2 * k);
            let (lo, mid, hi) = (&src[..body], &src[k.min(cols)..], &src[(2 * k).min(cols)..]);
            for (((&lo, &mid), &hi), d) in lo.iter().zip(mid).zip(hi).zip(&mut dst[k.min(cols)..]) {
                *d = lo && mid && hi;
            }
            for x in (0..k.min(cols)).chain(cols.saturating_sub(k)..cols) {
                dst[x] = src[x.saturating_sub(k)] && src[x] && src[(x + k).min(cols - 1)];
            }
        });
    out
}

/// AND of `src` over rows `y - k`, `y`, `y + k`, clamped at the edges
fn and3_rows(src: &[bool], rows: usize, cols: usize, k: usize) -> Vec<bool> {
    let mut out = vec![false; src.len()];
    out.par_chunks_mut(cols).enumerate().for_each(|(y, dst)| {
        let up = &src[y.saturating_sub(k) * cols..][..cols];
        let mid = &src[y * cols..][..cols];
        let down = &src[(y + k).min(rows - 1) * cols..][..cols];
        for (((&up, &mid), &down), d) in up.iter().zip(mid).zip(down).zip(dst) {
            *d = up && mid && down;
        }
    });
    out
}

/// decide which pixels are worth reconstructing.
pub fn decide(g: &[f32], w: &[f32], rows: usize, cols: usize, p: &Params) -> Vec<bool> {
    debug_assert_eq!(g.len(), w.len(), "one gate and one weight per pixel");
    debug_assert_eq!(g.len(), rows * cols, "rows * cols must cover the plane");

    let dark: Vec<bool> = g.par_iter().map(|&g| g < p.phi).collect();
    let row_dark = and3_cols(&and3_cols(&dark, cols, 1), cols, 3);
    let col_dark = and3_rows(&and3_rows(&dark, rows, cols, 1), rows, cols, 3);
    drop(dark);

    let mut mask = vec![false; g.len()];
    mask.par_chunks_mut(cols).enumerate().for_each(|(y, mask)| {
        let above = &row_dark[y.saturating_sub(4) * cols..][..cols];
        let below = &row_dark[(y + 4).min(rows - 1) * cols..][..cols];
        let sides = &col_dark[y * cols..][..cols];
        let w = &w[y * cols..][..cols];
        for (x, m) in mask.iter_mut().enumerate() {
            if p.clamp_l3 && w[x] >= 1.0 {
                continue;
            }
            *m = !(above[x]
                || below[x]
                || sides[x.saturating_sub(4)]
                || sides[(x + 4).min(cols - 1)]);
        }
    });
    mask
}

// ----- steps 6 to 8, rebuild

/// The four normalized convolutions at one pixel
#[derive(Default)]
struct Levels {
    /// `C_l`, the confidence mass
    c: [f32; 4],
    /// `P_l`, the gate pyramid
    p: [f32; 4],
    /// `L_l` per channel, filled only when the caller asked for color
    l: [[f32; 4]; 3],
}

/// The planes and geometry every tap reads
struct Frame<'a> {
    g: &'a [f32],
    w: &'a [f32],
    colors: [&'a [u16]; 3],
    lut: &'a [f32],
    rows: usize,
    cols: usize,
}

/// One level's running numerator and denominator
#[derive(Default, Clone, Copy)]
struct Sums {
    kw: f32,
    kwg: f32,
    kwd: [f32; 3],
}

impl Sums {
    /// Accumulate the run of cells `lo..=hi`, all at kernel weight `k`
    #[inline]
    fn span<const COLOR: bool>(&mut self, f: &Frame, k: f32, lo: usize, hi: usize) {
        let colors = f.colors.map(|plane| &plane[lo..=hi]);
        for (j, (&w, &g)) in f.w[lo..=hi].iter().zip(&f.g[lo..=hi]).enumerate() {
            let kw = w * k;
            self.kw += kw;
            self.kwg += kw * g;
            if COLOR {
                for (d, plane) in self.kwd.iter_mut().zip(colors) {
                    *d += kw * f.lut[usize::from(plane[j])];
                }
            }
        }
    }

    /// normalize
    #[inline]
    fn finish(self, out: &mut Levels, level: usize) {
        let inv = if self.kw > 0.0 { 1.0 / self.kw } else { 0.0 };
        out.c[level] = self.kw;
        out.p[level] = self.kwg * inv;
        for (l, d) in out.l.iter_mut().zip(self.kwd) {
            l[level] = d * inv;
        }
    }
}

/// `base + delta`, or `None` where that falls off a plane of `len`
#[inline]
fn offset(base: usize, delta: i32, len: usize) -> Option<usize> {
    let v = base as isize + delta as isize;
    (v >= 0 && v < len as isize).then_some(v as usize)
}

/// `base + delta`, clamped into a plane of `len`
#[inline]
fn clamped(base: usize, delta: i32, len: usize) -> usize {
    (base as isize + delta as isize).clamp(0, len as isize - 1) as usize
}

/// the pyramids at one pixel
fn pyramids_at<const COLOR: bool>(f: &Frame, y: usize, x: usize) -> Levels {
    let (rows, cols) = (f.rows, f.cols);
    let mut out = Levels::default();

    // levels 0 and 1: the 9x9 and 5x5 boxes, every cell at unit weight
    for (level, (spans, k)) in [(&LEVEL0[..], 1.0 / 69.0), (&LEVEL1[..], 1.0 / 21.0)]
        .into_iter()
        .enumerate()
    {
        let mut sums = Sums::default();
        for &(dy, dx_lo, dx_hi) in spans {
            let Some(ny) = offset(y, dy, rows) else {
                continue;
            };
            let base = ny * cols;
            sums.span::<COLOR>(
                f,
                k,
                base + clamped(x, dx_lo, cols),
                base + clamped(x, dx_hi, cols),
            );
        }
        sums.finish(&mut out, level);
    }

    // level 2: the 3x3 binomial tent, weight t[dy] * t[dx] / 16
    let mut sums = Sums::default();
    for dy in -1..=1 {
        let Some(ny) = offset(y, dy, rows) else {
            continue;
        };
        for dx in -1..=1 {
            let Some(nx) = offset(x, dx, cols) else {
                continue;
            };
            let i = ny * cols + nx;
            let k = TENT[(dy + 1) as usize] * TENT[(dx + 1) as usize] / 16.0;
            sums.span::<COLOR>(f, k, i, i);
        }
    }
    sums.finish(&mut out, 2);

    // level 3: the raw pixel, no kernel
    let i = y * cols + x;
    out.c[3] = f.w[i];
    out.p[3] = f.g[i];
    if COLOR {
        for (l, plane) in out.l.iter_mut().zip(f.colors) {
            l[3] = f.lut[usize::from(plane[i])];
        }
    }
    out
}

/// Uniform `[0, 1)`, fixed per (pixel, channel).
///
/// NOTE: we're not using a real RNG for perf reasons.
/// https://xkcd.com/221/
fn uniform(pixel: usize, channel: usize) -> f32 {
    let mut x = (pixel as u64) << 2 | channel as u64;
    // Stafford's Mix13
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((x ^ (x >> 31)) & 0x00ff_ffff) as f32 / (1u32 << 24) as f32
}

#[inline]
fn dither(x: f32, pixel: usize, channel: usize, p: &Params) -> f32 {
    let [lo, hi] = p.eta;
    if x <= lo || x >= hi {
        return 0.0;
    }
    let envelope = 4.0 / (hi - lo).powi(2) * (x - lo) * (hi - x);
    let d = envelope * (uniform(pixel, channel) - 0.5) * ALPHA[channel] * x;
    if x + d > lo && x + d < hi { d } else { 0.0 }
}

/// What we rebuilt
pub struct Patch {
    pub at: Vec<u32>,
    pub density: [Vec<f32>; 3],
}

/// pyramids blended into a reconstructed density per channel, dithered and clamped
pub fn reconstruct_core(
    g: &[f32],
    w: &[f32],
    colors: [&[u16]; 3],
    mask: &[bool],
    p: &Params,
    rows: usize,
    cols: usize,
) -> Patch {
    let f = &Frame {
        g,
        w,
        colors,
        lut: &LUT,
        rows,
        cols,
    };
    let at: Vec<u32> = (0..(rows * cols) as u32)
        .into_par_iter()
        .filter(|&i| mask[i as usize])
        .collect();

    let mut density = std::array::from_fn::<_, 3, _>(|_| vec![0.0f32; at.len()]);
    let mut keep = vec![true; at.len()];
    let [r, gr, b] = &mut density;

    (&at, r, gr, b, &mut keep)
        .into_par_iter()
        .for_each(|(&i, r, gr, b, keep)| {
            let i = i as usize;
            let (y, x) = (i / cols, i % cols);
            let center = pyramids_at::<true>(f, y, x);

            let mut lo = [0.0f32; 4];
            let mut hi = [0.0f32; 4];
            for level in 1..=3 {
                lo[level] = center.p[level] - center.p[level - 1];
                hi[level] = lo[level];
            }
            if p.cross_beta {
                for (dy, dx) in [(0, -1), (0, 1), (-1, 0), (1, 0)] {
                    let (Some(ny), Some(nx)) = (offset(y, dy, rows), offset(x, dx, cols)) else {
                        continue;
                    };
                    let n = pyramids_at::<false>(f, ny, nx);
                    for level in 1..=3 {
                        let d = n.p[level] - n.p[level - 1];
                        lo[level] = lo[level].min(d);
                        hi[level] = hi[level].max(d);
                    }
                }
            }

            let mut acc = [0.0f32; 3];
            for (ch, acc) in acc.iter_mut().enumerate() {
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
                    let r = if detail < lo_t {
                        detail - lo_t
                    } else if detail > hi_t {
                        detail - hi_t
                    } else {
                        0.0
                    };
                    let confidence = match level {
                        1 => (2.0 * center.c[1]).min(1.0),
                        2 => center.c[2],
                        _ => center.c[3] * center.c[3],
                    };
                    a += r * confidence;
                }

                // add dithering
                *acc = a + dither(a, i, ch, p);
            }

            if p.clamp_l3 {
                if acc.iter().any(|&a| a <= 0.0) {
                    *keep = false;
                    return;
                }
                for (acc, l) in acc.iter_mut().zip(center.l) {
                    *acc = acc.max(l[3]); // only fill, never darken
                }
            }
            (*r, *gr, *b) = (acc[0], acc[1], acc[2]);
        });

    if keep.iter().all(|&k| k) {
        return Patch { at, density };
    }
    Patch {
        at: at
            .iter()
            .zip(&keep)
            .filter_map(|(&i, &k)| k.then_some(i))
            .collect(),
        density: density.map(|plane| {
            plane
                .into_iter()
                .zip(&keep)
                .filter_map(|(d, &k)| k.then_some(d))
                .collect()
        }),
    }
}

// ----- the rest of the owl

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
    let p = Params::new(opts, cal);
    let [red, green, blue] = color;

    // 1 and 3. Log-density, then IR gating
    let g = gate(&*red, ir, &p);

    // 4. How confident we are that a given pixel is clean
    let w = confidence(&g, cols, &p);

    // 5. Decide which pixels are worth reconstructing
    let mask = decide(&g, &w, rows, cols, &p);

    // 6 to 8. pyramids, dithered and clamped
    let patch = reconstruct_core(&g, &w, [red, green, blue], &mask, &p, rows, cols);
    drop((g, w, mask));

    // 9. Back to linear
    for (plane, density) in [red, green, blue].into_iter().zip(&patch.density) {
        for (&i, v) in patch.at.iter().zip(from_density(density)) {
            let out = &mut plane[i as usize];
            *out = if p.clamp_l3 { v.max(*out) } else { v };
        }
    }
    patch.at.len()
}
