//! A negative's exposures from a prescan, a port of Nikon Scan's `CNegaAlgorithm::Process`
//!
//! LS5000.md3 Win `10011840`, Mac `1015ede0`; LS9000.md3 `100148e0`. The port
//! stops at the exposures and does not make the inversion lookup.

use crate::protocol::decode::Image;

/// A film's characteristic curve: 32 log exposures and the density of each channel at them
pub(crate) struct Curve {
    log_exposure: [f64; 32],
    density: [[f64; 32]; 3],
}

/// Color negative (LS5000.md3 `0x100ce578`, LS9000.md3 `0x100dc670`)
pub(crate) const COLOR_NEGATIVE: Curve = Curve {
    log_exposure: [
        -3.5, -3.4142, -3.2142, -3.0, -2.9435, -2.9173, -2.7692, -2.6892, -2.6142, -2.4142,
        -2.2142, -2.0, -1.7, -1.4, -1.1, -0.8, -0.5, -0.2, 0.1, 0.4, 0.7, 1.0, 1.3, 1.6, 1.9, 2.2,
        2.5, 2.8, 3.1, 3.4, 3.7, 4.0,
    ],
    density: [
        [
            0.316, 0.316, 0.316, 0.316, 0.316, 0.316, 0.3893, 0.4289, 0.466, 0.565, 0.664, 0.77,
            0.9185, 1.067, 1.2155, 1.364, 1.5125, 1.661, 1.8095, 1.958, 2.1065, 2.255, 2.4035,
            2.552, 2.7005, 2.849, 2.9975, 3.146, 3.2945, 3.443, 3.5915, 3.74,
        ],
        [
            0.737, 0.737, 0.737, 0.737, 0.737, 0.7525, 0.8398, 0.887, 0.9313, 1.0493, 1.1673,
            1.2936, 1.4706, 1.6476, 1.8246, 2.0016, 2.1786, 2.3556, 2.5326, 2.7096, 2.8866, 3.0636,
            3.2406, 3.4176, 3.5946, 3.7716, 3.9486, 4.1256, 4.3026, 4.4796, 4.6566, 4.8336,
        ],
        [
            0.886, 0.886, 0.886, 0.886, 0.9227, 0.9398, 1.036, 1.088, 1.1368, 1.2668, 1.3968,
            1.536, 1.731, 1.926, 2.121, 2.316, 2.511, 2.706, 2.901, 3.096, 3.291, 3.486, 3.681,
            3.876, 4.071, 4.266, 4.461, 4.656, 4.851, 5.046, 5.241, 5.436,
        ],
    ],
};

/// Black and white negative, mode 21h (LS5000.md3 `0x100ce978`, LS9000.md3 `0x100dca70`)
pub(crate) const MONOCHROME_NEGATIVE: Curve = Curve {
    log_exposure: [
        -1.5, -1.4, -1.3, -1.2, -1.1, -1.0, -0.9, -0.8, -0.7, -0.6, -0.5, -0.4, -0.3, -0.2, -0.1,
        0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0, 1.1, 1.2, 1.3, 1.4, 1.5, 1.6,
    ],
    density: [
        [
            0.295, 0.37, 0.42, 0.46, 0.505, 0.56, 0.61, 0.65, 0.705, 0.75, 0.81, 0.86, 0.91, 0.96,
            1.01, 1.07, 1.13, 1.19, 1.25, 1.31, 1.38, 1.46, 1.54, 1.62, 1.71, 1.81, 1.9, 2.03,
            2.16, 2.32, 2.52, 2.78,
        ],
        [
            0.313, 0.388, 0.438, 0.478, 0.523, 0.578, 0.628, 0.668, 0.723, 0.768, 0.828, 0.878,
            0.928, 0.978, 1.028, 1.088, 1.148, 1.208, 1.268, 1.328, 1.398, 1.478, 1.558, 1.638,
            1.728, 1.828, 1.918, 2.048, 2.178, 2.338, 2.538, 2.798,
        ],
        [
            0.247, 0.34, 0.39, 0.43, 0.475, 0.53, 0.58, 0.62, 0.675, 0.72, 0.78, 0.83, 0.88, 0.93,
            0.98, 1.04, 1.1, 1.16, 1.22, 1.28, 1.35, 1.43, 1.51, 1.59, 1.68, 1.78, 1.87, 2.0, 2.13,
            2.29, 2.49, 2.75,
        ],
    ],
};

/// The measured rectangle of a prescan, inclusive: `x` along the sensor, `y` along the feed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Crop {
    pub x: (usize, usize),
    pub y: (usize, usize),
}

impl Crop {
    /// All of `image`
    pub fn whole(image: &Image) -> Self {
        Self {
            x: (0, image.rows - 1),
            y: (0, image.cols - 1),
        }
    }
}

/// What `Process` reads besides the prescan
pub(crate) struct Inputs<'a> {
    /// The unit's start-up exposures, which the densities are relative to
    pub base: [u32; 3],
    /// What the prescan was exposed at
    pub prescan: [u32; 3],
    /// The film base's density per channel, from the thumbnail
    pub base_density: [f64; 3],
    pub curve: &'a Curve,
    /// The percentile that is a channel's highlight end; 1.0 is Nikon's
    pub percentile: f32,
}

/// The shadow end and the highlight end of each channel, as samples
type Ends = [(f64, f64); 3];

/// `CNegaAlgorithm::Process`: the scan exposures, or `None` if the prescan gives no data
pub(crate) fn exposures(image: &Image, crop: Crop, i: &Inputs) -> Option<[u32; 3]> {
    let scale: [f64; 3] = std::array::from_fn(|c| f64::from(i.base[c]) / f64::from(i.prescan[c]));
    let (all, neutral) = end_points(image, crop, i, scale)?;

    // Densities relative to the start-up exposure; `log10` through `log2`, as the x87 does
    let density = |v: f64| (65535.0 / v).log2() * std::f64::consts::LOG10_2;
    let all: Ends = std::array::from_fn(|c| {
        let (lo, hi) = all[c];
        ((lo * scale[c]).max(1.0), (hi * scale[c]).max(1.0))
    });
    let neutral_shadow: [f64; 3] = std::array::from_fn(|c| density(neutral[c].0 * scale[c]));
    let neutral_highlight: [f64; 3] = std::array::from_fn(|c| density(neutral[c].1 * scale[c]));

    // The base density is not more than the lowest density on the frame
    let base: [f64; 3] = std::array::from_fn(|c| i.base_density[c].min(neutral_highlight[c]));
    // Move each channel's curve to start at its base
    let curve: [[f64; 32]; 3] = std::array::from_fn(|c| {
        let d = i.curve.density[c];
        let shift = base[c] - d[0];
        d.map(|v| v + shift)
    });
    // A flat curve has nowhere to place anything
    if curve.iter().any(|d| d[0] == d[31]) {
        return None;
    }
    let t = &i.curve.log_exposure;
    let fog: [f64; 3] = std::array::from_fn(|c| base[c] + 0.15);

    // The neutral ends as log exposures over the fog, for AutoFilmMode
    let at = |c: usize, x: f64| interpolate(&curve[c], t, x);
    let shadow_e: [f64; 3] = std::array::from_fn(|c| at(c, neutral_shadow[c]));
    let highlight_e: [f64; 3] = std::array::from_fn(|c| at(c, neutral_highlight[c]));
    let fog_e: [f64; 3] = std::array::from_fn(|c| at(c, fog[c]));
    let range: [(f64, f64); 3] =
        std::array::from_fn(|c| (shadow_e[c] - fog_e[c], highlight_e[c] - fog_e[c]));
    let (shift_r, shift_b) = auto_film_mode(range, shadow_e, highlight_e);

    // ShiftFilmCurve (1000f3f0): move red and blue along log exposure
    let shifted: [[f64; 32]; 3] = [t.map(|v| v - shift_r), *t, t.map(|v| v + shift_b)];
    let at = |c: usize, x: f64| interpolate(&curve[c], &shifted[c], x);

    let shadow_d: [f64; 3] = std::array::from_fn(|c| density(all[c].0));
    let highlight_d: [f64; 3] = std::array::from_fn(|c| density(all[c].1));
    let fog_e: [f64; 3] = std::array::from_fn(|c| at(c, fog[c]));
    let shadow_over: f64 = (0..3)
        .map(|c| at(c, shadow_d[c]) - fog_e[c])
        .fold(f64::MIN, f64::max);
    let highlight_over: f64 = (0..3)
        .map(|c| at(c, highlight_d[c]) - fog_e[c])
        .fold(f64::MAX, f64::min);

    let mut shadow = (0..3)
        .map(|c| at(c, shadow_d[c].min(curve[c][31])))
        .fold(f64::MIN, f64::max);
    let mut highlight = (0..3)
        .map(|c| at(c, highlight_d[c].max(curve[c][0])))
        .fold(f64::MAX, f64::min);

    // The distance of the highlights from clipping
    let weight = (0..3)
        .map(|c| 10f64.powf(-(range[c].1 + 0.23) * 4.0).min(1.0))
        .fold(f64::MIN, f64::max)
        .min(1.0);
    expand_exposure(
        weight,
        highlight_over,
        shadow_over,
        &mut shadow,
        &mut highlight,
    );

    // A density per channel, and the exposure for it
    Some(std::array::from_fn(|c| {
        let d = interpolate(&shifted[c], &curve[c], highlight);
        (f64::from(i.base[c]) * 10f64.powf(d)) as u32
    }))
}

/// `Tanten` (`100100d0`): each channel's ends in `crop`, for all pixels and for the neutral pixels
///
/// A pixel is neutral if it is within a factor of 3 of gray against one other channel. After 90%
/// of the pixels are not neutral, all pixels count.
fn end_points(image: &Image, crop: Crop, i: &Inputs, scale: [f64; 3]) -> Option<(Ends, Ends)> {
    let d = i.base_density;
    let mask = [
        10f64.powf(d[1] - d[0]),
        10f64.powf(d[2] - d[0]),
        10f64.powf(d[2] - d[1]),
    ];
    let neutral = |q: f64| !(q <= 1.0 / 3.0 || 3.0 <= q);
    let widen = |v: u16| usize::from(super::widen(v, image.bits));

    let mut all = vec![[0u32; 3]; 1 << 16];
    let mut gray = vec![[0u32; 3]; 1 << 16];
    let mut gray_n = [0u32; 3];
    let mut off = [0u32; 3];
    let pixels = (crop.x.1 - crop.x.0 + 1) * (crop.y.1 - crop.y.0 + 1);
    for y in crop.y.0..=crop.y.1 {
        for x in crop.x.0..=crop.x.1 {
            let at = (image.rows - 1 - x) * image.stride + y;
            let v = [0, 1, 2].map(|c| widen(image.colors[c][at]));
            let [r, g, b] = std::array::from_fn(|c| v[c] as f64 * scale[c]);
            let rg = neutral(r / (mask[0] * g));
            let rb = neutral(r / (mask[1] * b));
            let gb = neutral(g / (mask[2] * b));
            for (c, near) in [rg || rb, rg || gb, rb || gb].into_iter().enumerate() {
                all[v[c]][c] += 1;
                if near || f64::from(off[c]) / pixels as f64 >= 0.9 {
                    gray[v[c]][c] += 1;
                    gray_n[c] += 1;
                } else {
                    off[c] += 1;
                }
            }
        }
    }
    let ends = |h: &[[u32; 3]], c: usize, n: u32| -> Option<(f64, f64)> {
        let tail = (f64::from(n) * 0.0042) as u32;
        let mut seen = 0;
        let lo = h.iter().position(|b| {
            seen += b[c];
            seen >= tail
        })?;
        let top = ((1.0 - f64::from(i.percentile.clamp(0.0, 1.0))) * f64::from(n)) as u32;
        let mut seen = 0;
        let hi = h.iter().rposition(|b| {
            seen += b[c];
            seen > top
        })?;
        Some((lo as f64, hi as f64))
    };
    let n = pixels as u32;
    let all: Option<Vec<_>> = (0..3).map(|c| ends(&all, c, n)).collect();
    let gray: Option<Vec<_>> = (0..3).map(|c| ends(&gray, c, gray_n[c])).collect();
    Some((all?.try_into().ok()?, gray?.try_into().ok()?))
}

/// `AutoFilmMode` (`10010950`): the shifts of the red and blue curves that make a neutral neutral
fn auto_film_mode(range: [(f64, f64); 3], shadow: [f64; 3], highlight: [f64; 3]) -> (f64, f64) {
    let [(lr, hr), (lg, hg), (lb, hb)] = range;
    let g = lg - hg;
    let near_r = 10f64.powf(-(hr - hg).abs());
    let near_b = 10f64.powf(-(hb - hg).abs());
    let span_r = 3f64.powf(-(lr - hr - g).abs());
    let span_b = 3f64.powf(-(g - (lb - hb)).abs());
    let dense = 10f64.powf(-(0.8 - lr.max(lg).max(lb))).min(1.0);
    let wr = (1.0 - near_r + span_r * near_r) * dense;
    let wb = (1.0 - near_b + span_b * near_b) * dense;
    (
        (shadow[0] - shadow[1]) * wr + (highlight[0] - highlight[1]) * (1.0 - wr) * near_r,
        (shadow[1] - shadow[2]) * wb + (highlight[1] - highlight[2]) * (1.0 - wb) * near_b,
    )
}

/// `ExpandExposure` (`10010b20`): make a short range wider and keep the highlights off the shoulder
fn expand_exposure(
    weight: f64,
    highlight_over: f64,
    shadow_over: f64,
    shadow: &mut f64,
    highlight: &mut f64,
) {
    let was = *shadow;
    if *shadow - *highlight < 0.6 {
        *highlight -= (1.0 - weight) * (0.6 - (*shadow - *highlight));
        *shadow = (*highlight + 0.6).max(was + 0.05);
    } else {
        *shadow += 0.05;
    }
    let span = *shadow - *highlight;
    let reach = if span < 1.9 {
        (0.4f64.powf(highlight_over + 0.23) * 1.9)
            .min(1.9)
            .max(span)
    } else {
        1.9
    };
    if shadow_over < 1.2 {
        *shadow += (1.2 - shadow_over) * (span / reach);
    }
}

/// `GetComplementedValue` (`1000f470`): linear interpolation of `x` from `xs` to `ys`
fn interpolate(xs: &[f64; 32], ys: &[f64; 32], x: f64) -> f64 {
    let Some(i) = (1..32).find(|&i| x < xs[i]) else {
        return ys[31];
    };
    if xs[i] == xs[i - 1] {
        return ys[i - 1];
    }
    (x - xs[i - 1]) * (ys[i] - ys[i - 1]) / (xs[i] - xs[i - 1]) + ys[i - 1]
}
