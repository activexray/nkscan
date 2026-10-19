//! Host metering, a port of Nikon Scan's `CNkIPPrescan::Execute`
//!
//! LS5000.md3 Mac `1009eb50`, LS9000.md3 `10044d90`. Each number is Nikon's,
//! from the decompiled function that the item names.

pub(crate) mod negative;

use crate::{
    error::Error,
    protocol::{
        caps::{Capabilities, film::FilmFormat},
        decode::Image,
        window::{Channel, Window},
    },
    scan::profile::Film,
};
use negative::{COLOR_NEGATIVE, Crop, Inputs, MONOCHROME_NEGATIVE};
use tracing::*;

/// How to meter a frame
#[derive(Debug, Clone, Copy)]
pub struct Metering {
    /// The percentile that is a channel's brightest sample; 1.0 is Nikon Scan's
    pub percentile: f32,
    /// Move the visible channels by one factor, so they keep the unit's balance
    pub lock_white_balance: bool,
    /// The film type, which sets the prescan exposure and how the prescan is read
    pub film: Film,
    /// The frame format, which sets the measured crop; `None` measures the whole frame
    pub format: Option<FilmFormat>,
    /// The film base density per channel off the thumbnail; `None` uses Nikon Scan's defaults
    pub base_density: Option<[f64; 3]>,
}

impl Default for Metering {
    fn default() -> Self {
        Self::new(Film::default())
    }
}

/// What one prescan says to do next
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Prescan again at these exposures
    Again(Vec<u32>),
    /// Scan at these
    Done(Vec<u32>),
}

/// Nikon Scan's infrared target, 85% of full scale (Mac `100a005c`)
pub const INFRARED_TARGET: f32 = 0.85;

/// Nikon Scan's maximum exposure, whatever the unit allows
const MAX_EXPOSURE: u32 = 0x3ff_ffff;

/// Nikon Scan's film base density if the thumbnail gives none (`ReadBaseDensity`, Mac `100a1970`)
pub(crate) const DEFAULT_BASE_DENSITY: [f64; 3] = [0.316, 0.737, 0.886];

impl Metering {
    /// Metering for `film`, with the white balance lock from [`Self::locks_white_balance`]
    pub fn new(film: Film) -> Self {
        Self {
            percentile: 1.0,
            lock_white_balance: Self::locks_white_balance(film),
            film,
            format: None,
            base_density: None,
        }
    }

    /// Whether Nikon Scan meters the channels of `film` together; a negative is metered per channel
    pub fn locks_white_balance(film: Film) -> bool {
        !film.is_negative()
    }

    /// The prescan exposures from the start-up `base` (`CNkIPPreviewScan::SetDataInfo`, Mac `1009c320`)
    pub(crate) fn seed(&self, windows: &[Window], base: &[u32]) -> Vec<u32> {
        let factors = match self.film {
            Film::Negative => [1.65959, 3.98107, 5.37032],
            Film::MonochromeNegative => [1.86209, 2.0893, 2.0893],
            Film::Positive => [1.20009, 1.22458, 1.27356],
            Film::Kodachrome => [0.995, 1.0, 1.01],
        };
        windows
            .iter()
            .zip(base)
            .map(|(w, &b)| (f64::from(b) * color(w).map_or(0.9, |c| factors[c])) as u32)
            .collect()
    }

    /// The number of the last prescan, which gets no correction (`SetDataInfo` `+0x48`, Mac `1009e250`)
    fn passes(&self) -> usize {
        if self.film.is_negative() { 3 } else { 2 }
    }

    /// The next step after prescan `pass` (from 1); `base` is the start-up exposures in `windows` order
    pub(crate) fn step(
        &self,
        caps: &Capabilities,
        image: &Image,
        windows: &[Window],
        base: &[u32],
        pass: usize,
    ) -> Result<Step, Error> {
        let carried = image.colors.len() + usize::from(!image.ir.is_empty());
        if carried != windows.len() || windows.len() != base.len() {
            return Err(Error::Unsupported {
                op: "metering",
                reason: format!(
                    "the pass carried {carried} channels for {} windows",
                    windows.len()
                ),
            });
        }
        let crop = self.crop(caps, &windows[0], image);
        let current: Vec<u32> = windows.iter().map(|w| w.exposure).collect();
        let colors: Vec<usize> = (0..windows.len())
            .filter(|&i| color(&windows[i]).is_some())
            .collect();
        let measured: Vec<u16> = (0..colors.len())
            .map(|plane| high(image, image.colors[plane], crop, self.percentile))
            .collect();
        debug!(?crop, ?measured, pass, "metering levels");

        // Positives read as one channel unless asked not to
        let levels: Vec<u16> = match (self.film.is_negative(), self.lock_white_balance) {
            (false, true) => vec![measured.iter().copied().max().unwrap_or(0); measured.len()],
            _ => measured,
        };

        let mut next = current.clone();
        // Infrared is metered alone, on the whole frame
        if let Some(i) = (0..windows.len()).find(|&i| color(&windows[i]).is_none()) {
            let ir = high(image, image.ir, Crop::whole(image), self.percentile);
            if ir > 0 {
                next[i] = scale(
                    current[i],
                    65535.0 * f64::from(INFRARED_TARGET) / f64::from(ir),
                );
            }
        }

        // Below half scale, go to 90% and prescan again; if clipped, restart (`100a0068`, `100a0b4c`)
        if pass < self.passes() {
            let mut again = false;
            for (&i, &level) in colors.iter().zip(&levels) {
                let c = color(&windows[i]).expect("a color window");
                if level <= 0x7fff {
                    if current[i] != MAX_EXPOSURE {
                        next[i] = scale(current[i], 65535.0 / f64::from(level.max(1)) * 0.9);
                        again = true;
                    }
                } else if level == 0xffff {
                    next[i] = scale(base[i], self.clipped(c, pass));
                    again = true;
                }
            }
            if again {
                return Ok(Step::Again(self.limit(caps, next)));
            }
        }

        if self.film.is_negative() {
            let channel = |c: usize| colors.iter().position(|&i| color(&windows[i]) == Some(c));
            let (Some(r), Some(g), Some(b)) = (channel(0), channel(1), channel(2)) else {
                // A negative needs all three colors; fewer are read as a positive
                return Ok(Step::Done(self.positive(caps, &colors, &levels, next)));
            };
            let pick = |v: &[u32]| [v[colors[r]], v[colors[g]], v[colors[b]]];
            let planes = Image {
                colors: vec![image.colors[r], image.colors[g], image.colors[b]],
                ..*image
            };
            let inputs = Inputs {
                base: pick(base),
                prescan: pick(&current),
                base_density: self.base_density.unwrap_or(DEFAULT_BASE_DENSITY),
                curve: match self.film {
                    Film::MonochromeNegative => &MONOCHROME_NEGATIVE,
                    _ => &COLOR_NEGATIVE,
                },
                percentile: self.percentile,
            };
            let Some(mut metered) = negative::exposures(&planes, crop, &inputs) else {
                warn!("the prescan gave the film curve nothing to place; scanning as prescanned");
                return Ok(Step::Done(self.limit(caps, next)));
            };
            debug!(?metered, "off the film curve");
            if self.lock_white_balance {
                // The smallest factor of the three, so that no channel clips
                let k = (0..3)
                    .map(|c| f64::from(metered[c]) / f64::from(inputs.base[c]))
                    .fold(f64::INFINITY, f64::min);
                metered = std::array::from_fn(|c| scale(inputs.base[c], k));
            }
            for (c, &plane) in [r, g, b].iter().enumerate() {
                next[colors[plane]] = metered[c];
            }
            return Ok(Step::Done(self.limit(caps, next)));
        }
        Ok(Step::Done(self.positive(caps, &colors, &levels, next)))
    }

    /// A positive's exposures, with the brightest sample at full scale (Mac `100a04a4`)
    fn positive(
        &self,
        caps: &Capabilities,
        colors: &[usize],
        levels: &[u16],
        mut next: Vec<u32>,
    ) -> Vec<u32> {
        for (&i, &level) in colors.iter().zip(levels) {
            if level > 0 {
                next[i] = scale(next[i], 65535.0 / f64::from(level));
            }
        }
        self.limit(caps, next)
    }

    /// The restart factor on the base for clipped channel `c` (Win `100ced78`, Mac `100a018c`)
    fn clipped(&self, c: usize, pass: usize) -> f64 {
        const COLOR: [[f64; 4]; 3] = [
            [1.65959, 1.15942, 0.9, 0.9],
            [3.98107, 2.84605, 1.4264, 0.9],
            [5.37032, 3.19332, 1.60045, 0.9],
        ];
        const MONOCHROME: [[f64; 3]; 3] = [
            [1.86209, 0.94242, 0.9],
            [2.0893, 1.04713, 0.9],
            [2.0893, 1.04713, 0.9],
        ];
        match self.film {
            Film::Negative => COLOR[c][pass.min(3)],
            Film::MonochromeNegative => MONOCHROME[c][pass.min(2)],
            Film::Positive => 0.98,
            Film::Kodachrome => [0.8955, 0.9, 0.909][c],
        }
    }

    /// The measured crop in prescan pixels (LS5000.md3 Mac `1009e250`, LS9000.md3 `1003a780`)
    fn crop(&self, caps: &Capabilities, window: &Window, image: &Image) -> Crop {
        let device: Option<[u32; 4]> = match self.format {
            Some(FilmFormat::F135) => Some([36, 245, 50, 375].map(|v| v * 14)),
            Some(FilmFormat::IX240) => Some([0x1c, 0xb2, 0x21, 0x11b].map(|v| v * 14)),
            Some(FilmFormat::F645) => Some([547, 8417, 675, 6021]),
            Some(FilmFormat::F66) => Some([547, 8417, 675, 8289]),
            Some(FilmFormat::F67) => Some([547, 8417, 675, 10341]),
            Some(FilmFormat::F68) => Some([547, 8417, 675, 11529]),
            Some(FilmFormat::F69) => Some([547, 8417, 675, 12501]),
            _ => None,
        };
        let Some([x0, x1, y0, y1]) = device else {
            return Crop::whole(image);
        };
        let (pixels, lines) = (image.rows - 1, image.cols - 1);
        let (ox, oy) = (
            caps.address.x_axis.optical_dpi,
            caps.address.y_axis.optical_dpi,
        );
        let reduce = |optical: u16, dpi: u16| u32::from(optical / dpi.max(1)).max(1);
        let (rx, ry) = (
            reduce(ox, window.resolution.0),
            reduce(oy, window.resolution.1),
        );
        let at = |v: u32, optical: u16, r: u32, last: usize| {
            let dots = (u64::from(v) * u64::from(optical) + 2000) / 4000;
            ((dots / u64::from(r)) as usize).min(last)
        };
        Crop {
            x: (at(x0, ox, rx, pixels), at(x1, ox, rx, pixels)),
            y: (at(y0, oy, ry, lines), at(y1, oy, ry, lines)),
        }
    }

    /// Clamp to the exposures the unit accepts (`SetWindowFunction` bytes 16-24)
    fn limit(&self, caps: &Capabilities, exposures: Vec<u32>) -> Vec<u32> {
        let limit = &caps.set_window.exposure;
        exposures
            .into_iter()
            .map(|e| e.clamp(limit.start, limit.last.min(MAX_EXPOSURE)))
            .collect()
    }
}

/// Which of red, green and blue a window carries
pub(crate) fn color(w: &Window) -> Option<usize> {
    match w.channel() {
        Channel::Red => Some(0),
        Channel::Green => Some(1),
        Channel::Blue => Some(2),
        _ => None,
    }
}

/// `exposure` times `factor`, truncated as Nikon Scan does, up to [`MAX_EXPOSURE`]
pub(crate) fn scale(exposure: u32, factor: f64) -> u32 {
    (f64::from(exposure) * factor).min(f64::from(MAX_EXPOSURE)) as u32
}

/// A sample of `bits` valid bits on Nikon Scan's 16-bit scale
pub(crate) fn widen(v: u16, bits: u8) -> u16 {
    let full = f64::from(ceiling(bits));
    (f64::from(v) * 65535.0 / full).round() as u16
}

/// The `percentile` sample of `plane` in `crop` on the 16-bit scale; `crop.x` counts rows from the bottom
fn high(image: &Image, plane: &[u16], crop: Crop, percentile: f32) -> u16 {
    let mut counts = vec![0u32; 1 << 16];
    for y in crop.y.0..=crop.y.1 {
        for x in crop.x.0..=crop.x.1 {
            let v = plane[(image.rows - 1 - x) * image.stride + y];
            counts[usize::from(widen(v, image.bits))] += 1;
        }
    }
    let total: u32 = counts.iter().sum();
    let above = ((1.0 - f64::from(percentile.clamp(0.0, 1.0))) * f64::from(total)) as u32;
    let mut seen = 0;
    counts
        .iter()
        .rposition(|&n| {
            seen += n;
            seen > above
        })
        .unwrap_or(0) as u16
}

/// Full scale for a sample of `bits` valid bits
pub(crate) fn ceiling(bits: u8) -> u16 {
    match bits {
        0 | 16.. => u16::MAX,
        b => (1u16 << b) - 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocol::window::LENGTH,
        scan::{boundary::fixtures::read_tiff, thumbnail, window::tests::caps},
    };
    use std::path::PathBuf;

    fn fixture(path: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(path)
    }

    /// A prescan from an LS-50 Nikon Scan capture, 281 14-bit samples a line, as planes
    fn prescan(name: &str, channels: usize) -> Vec<Vec<u16>> {
        let raw: Vec<u16> = std::fs::read(fixture(&format!("meter/{name}.bin")))
            .expect("prescan")
            .chunks(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let pixels = 281;
        let lines = raw.len() / (pixels * channels);
        (0..channels)
            .map(|c| {
                let mut plane = vec![0; pixels * lines];
                for l in 0..lines {
                    for x in 0..pixels {
                        plane[(pixels - 1 - x) * lines + l] = raw[(l * channels + c) * pixels + x];
                    }
                }
                plane
            })
            .collect()
    }

    fn image(planes: &[Vec<u16>]) -> Image<'_> {
        let rows = 281;
        let cols = planes[0].len() / rows;
        Image {
            colors: planes[..3].iter().map(Vec::as_slice).collect(),
            ir: planes.get(3).map_or(&[], Vec::as_slice),
            rows,
            cols,
            stride: cols,
            bits: 14,
        }
    }

    /// Windows 1, 2, 3 (and 9) at the prescan's resolution and these exposures
    fn windows(exposures: &[u32]) -> Vec<Window> {
        [1, 2, 3, 9]
            .into_iter()
            .zip(exposures)
            .map(|(id, &exposure)| {
                let mut w = Window::try_from(&[0u8; LENGTH][..]).unwrap();
                w.id = id;
                w.exposure = exposure;
                w.resolution = (285, 285);
                w
            })
            .collect()
    }

    fn unit() -> Capabilities {
        let mut caps = caps();
        caps.set_window.exposure = std::range::RangeInclusive {
            start: 1,
            last: MAX_EXPOSURE,
        };
        caps
    }

    /// The film base density of a fixture thumbnail at Nikon Scan's exposures
    fn base_density(name: &str, base: [u32; 3], taken: [u32; 3]) -> [f64; 3] {
        let (planes, rows, cols) = read_tiff(&fixture(&format!("framing/{name}/thumbnail.tiff")));
        let image = Image {
            colors: planes.iter().map(Vec::as_slice).collect(),
            ir: &[],
            rows,
            cols,
            stride: cols,
            bits: 14,
        };
        let ratio = std::array::from_fn(|c| f64::from(base[c]) / f64::from(taken[c]));
        thumbnail::base_density(&image, ratio).expect("a base density")
    }

    /// On a 2900 dpi LS-40, the 135 crop measures the same area of film as on a 4000 dpi unit
    #[test]
    fn the_135_crop_is_rescaled_to_the_units_optical_resolution() {
        let planes = vec![vec![0u16; 400 * 600]; 3];
        let image = Image {
            colors: planes.iter().map(Vec::as_slice).collect(),
            ir: &[],
            rows: 400,
            cols: 600,
            stride: 600,
            bits: 14,
        };
        let metering = Metering {
            format: Some(FilmFormat::F135),
            ..Metering::new(Film::Positive)
        };
        let mut w = windows(&[1])[0].clone();
        w.resolution = (400, 400);
        let four = metering.crop(&unit(), &w, &image);
        assert_eq!((four.x, four.y), ((50, 343), (70, 525)));

        let mut ls40 = unit();
        ls40.address.x_axis.optical_dpi = 2900;
        ls40.address.y_axis.optical_dpi = 2900;
        w.resolution = (290, 290);
        let crop = metering.crop(&ls40, &w, &image);
        // At 2900 dpi, the 4000 dpi bounds 504, 3430, 700 and 5250 are 365, 2487, 508 and 3806
        assert_eq!((crop.x, crop.y), ((36, 248), (50, 380)));
    }

    /// `coolscan_v_frame_test` gets Nikon Scan's exposures to the count
    #[test]
    fn a_color_negative_is_exposed_as_nikon_exposes_it() {
        let base = [48132, 51833, 35369];
        let density = base_density("coolscan_v_frame_test", base, [59250, 147413, 135781]);
        assert!((density[0] - 0.4172).abs() < 1e-4, "{density:?}");
        let metering = Metering {
            format: Some(FilmFormat::F135),
            base_density: Some(density),
            ..Metering::new(Film::Negative)
        };
        let seeded = metering.seed(&windows(&base), &base);
        assert_eq!(seeded, [79879, 206350, 189942]);

        let planes = prescan("color_negative", 4);
        let w = windows(&[79879, 206350, 189942, 302968]);
        let Step::Done(got) = metering
            .step(
                &unit(),
                &image(&planes),
                &w,
                &[48132, 51833, 35369, 336631],
                1,
            )
            .unwrap()
        else {
            panic!("one prescan was enough for Nikon Scan");
        };
        assert_eq!(got[..3], [109319, 313660, 368912]);
    }

    /// `coolscan_v_bw`: the first prescan is too dark, so each channel goes to 90%, then the curve
    #[test]
    fn a_black_and_white_negative_is_brought_up_then_read_off_its_curve() {
        // Inferred from Nikon Scan's prescan exposures; the capture holds no 8Ch read
        let base = [47596, 51545, 35160, 334764];
        let density = base_density(
            "coolscan_v_bw",
            base[..3].try_into().unwrap(),
            [61923, 78760, 49610],
        );
        let metering = Metering {
            format: Some(FilmFormat::F135),
            base_density: Some(density),
            ..Metering::new(Film::MonochromeNegative)
        };

        let planes = prescan("mono_negative_1", 4);
        let w = windows(&[88629, 107692, 73459, 301288]);
        let step = metering
            .step(&unit(), &image(&planes), &w, &base, 1)
            .unwrap();
        let Step::Again(next) = step else {
            panic!("Nikon Scan prescanned again, this said {step:?}");
        };
        assert_eq!(next[..3], [296863, 321107, 185595]);

        // Nikon Scan's code, emulated on these inputs; its capture sent 176472, 318763, 220587
        let metering = Metering {
            base_density: Some([0.4468, 0.737, 0.886]),
            ..metering
        };
        let planes = prescan("mono_negative_2", 3);
        let w = windows(&[296863, 321107, 185595]);
        let step = metering
            .step(&unit(), &image(&planes), &w, &base[..3], 2)
            .unwrap();
        assert_eq!(step, Step::Done(vec![176308, 319607, 220587]));
    }

    /// `coolscan_v_slide`: one factor puts the brightest sample at full scale
    #[test]
    fn a_slide_is_exposed_as_nikon_exposes_it() {
        let metering = Metering {
            format: Some(FilmFormat::F135),
            ..Metering::new(Film::Positive)
        };
        let planes = prescan("slide", 4);
        let w = windows(&[57120, 63120, 44778, 301288]);
        let Step::Done(got) = metering
            .step(
                &unit(),
                &image(&planes),
                &w,
                &[47597, 51544, 35160, 334764],
                1,
            )
            .unwrap()
        else {
            panic!("one prescan was enough for Nikon Scan");
        };
        assert_eq!(got[..3], [76667, 84720, 60101]);
        // Nikon Scan's was 334093: its infrared reads a max this does not
        assert!(got[3].abs_diff(334093) < 10, "{got:?}");
    }

    /// Unlocked, a slide's channels each put their own brightest at full scale
    #[test]
    fn an_unlocked_slide_is_metered_per_channel() {
        let metering = Metering {
            format: Some(FilmFormat::F135),
            lock_white_balance: false,
            ..Metering::new(Film::Positive)
        };
        let planes = prescan("slide", 4);
        let w = windows(&[57120, 63120, 44778, 301288]);
        let Step::Done(got) = metering
            .step(
                &unit(),
                &image(&planes),
                &w,
                &[47597, 51544, 35160, 334764],
                1,
            )
            .unwrap()
        else {
            panic!("every channel reads");
        };
        // Red is the brightest, so it is where it was locked; the others go further
        assert_eq!(got[0], 76667);
        assert!(got[1] > 84720 && got[2] > 60101, "{got:?}");
    }

    /// A locked negative keeps the start-up balance, with the smallest factor
    #[test]
    fn a_locked_negative_keeps_the_start_up_balance() {
        let base = [48132, 51833, 35369];
        let metering = Metering {
            format: Some(FilmFormat::F135),
            lock_white_balance: true,
            ..Metering::new(Film::Negative)
        };
        let planes = prescan("color_negative", 4);
        let w = windows(&[79879, 206350, 189942, 302968]);
        let Step::Done(got) = metering
            .step(
                &unit(),
                &image(&planes),
                &w,
                &[48132, 51833, 35369, 336631],
                1,
            )
            .unwrap()
        else {
            panic!("one prescan");
        };
        let k: Vec<f64> = (0..3)
            .map(|c| f64::from(got[c]) / f64::from(base[c]))
            .collect();
        assert!(k.windows(2).all(|p| (p[0] - p[1]).abs() < 1e-4), "{k:?}");
    }
}
