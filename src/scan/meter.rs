//! Metering we do ourselves, for units with no hardware AE
//!
//! `Exposure` decides when this is needed. We take an
//! ordinary low-resolution pass and work out the per-channel exposures from it.
//! Nikon Scan does the same: nothing in the capture corpus uses a setup scan
//! kind or reads `DataType::MaxValue`.
//!
//! The sensor is linear in integration time, so one proportional step gets us
//! there. The exception is a clipped pass. A channel sitting at full scale
//! could be anywhere above it, so we halve it and measure again.

use crate::{
    error::Error,
    protocol::{caps::Capabilities, decode::Image, window::Window},
};

/// How to meter a frame
#[derive(Debug, Clone, Copy)]
pub struct Metering {
    /// Where to put each channel's high tail, as a fraction of full scale.
    /// Under 1.0 to leave room for a correction that overshoots
    pub target: f32,
    /// Which sample counts as the high tail. Keeps a dust speck or a few blown
    /// pixels from setting the exposure
    pub percentile: f32,
    /// Move the visible channels by one factor so they keep their proportions,
    /// and the film keeps its cast. Off means each one fills the range by
    /// itself, which takes the orange mask off a negative
    pub lock_white_balance: bool,
    /// How close to `target` counts as landed, as a fraction. A pass that comes
    /// back inside this needs no correction and no further pass
    pub tolerance: f32,
    /// Passes to take before giving up on converging. A clipped channel needs
    /// at least two, since halving it is a retreat rather than a correction
    pub max_passes: usize,
}

impl Default for Metering {
    fn default() -> Self {
        Self {
            target: 0.97,
            percentile: 0.999,
            lock_white_balance: false,
            tolerance: 0.02,
            max_passes: 3,
        }
    }
}

impl Metering {
    /// New exposures for `windows`, from a pass taken with the old ones
    ///
    /// The result lines up with `windows`. A channel the pass tells us nothing
    /// about keeps the exposure it had.
    pub fn apply(
        &self,
        caps: &Capabilities,
        image: &Image,
        windows: &[Window],
    ) -> Result<Vec<u32>, Error> {
        if image.channels.len() != windows.len() {
            return Err(Error::Unsupported {
                op: "metering",
                reason: format!(
                    "the pass carried {} channels and there are {} windows",
                    image.channels.len(),
                    windows.len()
                ),
            });
        }

        // `SetWindowFunction` bytes 16-24. Anything outside it comes back as
        // common error 2
        let limit = &caps.set_window.exposure;
        let ceiling = ceiling(image.bits);
        let target = (f32::from(ceiling) * self.target.clamp(0.0, 1.0)) as u16;

        // What each channel asks to be scaled by, before the lock has a say
        let steps: Vec<Option<f64>> = self
            .measure(image)
            .into_iter()
            .map(|level| level.and_then(|l| step(l, target, ceiling)))
            .collect();

        // Locked, we move them all by the smallest factor any of them wants.
        // That puts the most constrained channel on target and keeps the rest
        // below it. Infrared measures what is in the way, not color, so it is
        // never part of the lock
        let locked = self.lock_white_balance.then(|| {
            steps
                .iter()
                .zip(windows)
                .filter(|(_, w)| w.channel().is_color())
                .filter_map(|(s, _)| *s)
                .fold(f64::INFINITY, |a, b| a.min(b))
        });

        Ok(windows
            .iter()
            .zip(&steps)
            .map(|(w, own)| {
                let scale = match locked {
                    Some(f) if w.channel().is_color() && f.is_finite() => Some(f),
                    _ => *own,
                };
                match scale {
                    Some(s) => (f64::from(w.exposure) * s)
                        .round()
                        .clamp(f64::from(limit.start), f64::from(limit.last))
                        as u32,
                    None => w.exposure,
                }
            })
            .collect())
    }
}

impl Metering {
    /// Whether a pass landed close enough that correcting it is not worth
    /// another pass
    ///
    /// A channel with nothing to measure counts as settled: there is no
    /// correction to make from a level of zero. A clipped one never does,
    /// since where it actually sits is unknown.
    pub fn settled(&self, image: &Image) -> bool {
        let ceiling = ceiling(image.bits);
        let target = (f32::from(ceiling) * self.target.clamp(0.0, 1.0)) as u16;

        self.measure(image).into_iter().all(|level| {
            match level.and_then(|l| step(l, target, ceiling)) {
                None => true,
                Some(scale) => (scale - 1.0).abs() <= f64::from(self.tolerance),
            }
        })
    }

    /// The high tail of each channel, in the order the image interleaves them
    ///
    /// What the exposures get decided from, so it is worth looking at alone
    pub fn measure(&self, image: &Image) -> Vec<Option<u16>> {
        (0..image.channels.len())
            .map(|channel| tail(image, channel, self.percentile))
            .collect()
    }
}

/// Full scale for a sample of `bits` valid bits
fn ceiling(bits: u8) -> u16 {
    match bits {
        0 | 16.. => u16::MAX,
        b => (1u16 << b) - 1,
    }
}

/// What to scale an exposure by to move `level` onto `target`
///
/// A channel at full scale could be anywhere above it, so we halve it and
/// measure again. One reading zero gives us nothing to scale.
fn step(level: u16, target: u16, ceiling: u16) -> Option<f64> {
    match level {
        l if l >= ceiling => Some(0.5),
        0 => None,
        l => Some(f64::from(target) / f64::from(l)),
    }
}

/// The `percentile` brightest sample of one channel, or `None` where the pass
/// carried none for it
///
/// Counted rather than sorted, so a plane is read once and never copied
fn tail(image: &Image, channel: usize, percentile: f32) -> Option<u16> {
    let mut counts = vec![0u32; usize::from(u16::MAX) + 1];
    let mut total = 0usize;
    for sample in image.plane(channel) {
        counts[usize::from(sample)] += 1;
        total += 1;
    }
    if total == 0 {
        return None;
    }

    // The same element a sort would put at this index
    let at = ((total - 1) as f32 * percentile.clamp(0.0, 1.0)) as usize;
    let mut seen = 0usize;
    for (value, count) in counts.iter().enumerate() {
        seen += *count as usize;
        if seen > at {
            return Some(value as u16);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        caps::{
            Page,
            address::Address,
            identity::Identity,
            other::Features,
            set_window::{ColorInterleaving, SetWindowFunction},
        },
        decode::Decoder,
        image::Layout,
        window::{Channel, Composition, LENGTH},
    };

    /// Enough of a unit to carry an exposure range wide enough not to clamp
    fn caps() -> Capabilities {
        let mut p = vec![0u8; 91];
        p[1] = Address::PAGE_CODE;
        p[3] = 87;
        p[18..20].copy_from_slice(&4000u16.to_be_bytes());
        p[20..22].copy_from_slice(&4000u16.to_be_bytes());
        let address = Address::try_from(&Page::new(Address::PAGE_CODE, p).unwrap()).unwrap();

        let mut d = vec![0u8; 28];
        d[1] = SetWindowFunction::PAGE_CODE;
        d[3] = 24;
        d[17..21].copy_from_slice(&1u32.to_be_bytes());
        d[21..25].copy_from_slice(&0x3FFFFFFu32.to_be_bytes());
        let set_window =
            SetWindowFunction::try_from(&Page::new(SetWindowFunction::PAGE_CODE, d).unwrap())
                .unwrap();

        let mut e = vec![0u8; 39];
        e[1] = Features::PAGE_CODE;
        e[3] = 35;
        let features = Features::try_from(&Page::new(Features::PAGE_CODE, e).unwrap()).unwrap();

        let mut i = vec![0u8; 36];
        i[4] = 31;

        Capabilities {
            identity: Identity::parse(&i).unwrap(),
            address,
            features,
            set_window,
            ccd: None,
            frames: None,
        }
    }

    fn image<'a>(layout: &'a Layout, samples: &'a [u16]) -> Image<'a> {
        Image::new(layout, samples).unwrap()
    }

    const PIXELS: u32 = 4;
    const LINES: u32 = 2;

    fn windows(ids: &[u8], exposure: u32) -> Vec<Window> {
        let visible = ids
            .iter()
            .filter(|&&id| Channel::from(id).is_color())
            .count();
        ids.iter()
            .map(|&id| {
                let mut w = Window::try_from(&[0u8; LENGTH][..]).unwrap();
                w.id = id;
                w.exposure = exposure;
                w.resolution = (4000, 4000);
                w.size = (PIXELS, LINES);
                w.bpp = 16;
                w.color_interleaving = ColorInterleaving::LINE_WITHOUT_DISTANCE;
                w.composition = match visible {
                    1 => Composition::MultilevelBW,
                    _ => Composition::MultilevelRGB,
                };
                w
            })
            .collect()
    }

    /// A pass holding one flat level per channel, put through the real decoder
    /// so metering is tested against what a scan actually hands it
    fn decoded(windows: &[Window], levels: &[u16]) -> (Layout, Vec<u16>) {
        let mut raw = Vec::new();
        for _ in 0..LINES {
            for &level in levels {
                for _ in 0..PIXELS {
                    raw.extend_from_slice(&level.to_be_bytes());
                }
            }
        }
        let layout = Layout::new(&caps(), windows, 4000).unwrap();
        let mut decoder = Decoder::new(&layout).unwrap();
        let mut samples = vec![0u16; decoder.samples()];
        decoder.push(&raw, &mut samples).unwrap();
        (layout, samples)
    }

    /// Linear in integration time, so the step is just the ratio
    #[test]
    fn each_channel_lands_on_the_target() {
        let m = Metering {
            target: 1.0,
            ..Default::default()
        };
        let w = windows(&[1, 2, 3], 1000);
        // A third, a half and a quarter of full scale
        let (l, s) = decoded(&w, &[21845, 32767, 16383]);
        let got = m.apply(&caps(), &image(&l, &s), &w).unwrap();
        assert_eq!(got, vec![3000, 2000, 4000]);
    }

    /// Locked, they all move by the smallest factor any of them asked for
    #[test]
    fn locking_scales_the_set_by_its_most_constrained_channel() {
        let m = Metering {
            target: 1.0,
            lock_white_balance: true,
            ..Default::default()
        };
        let w = windows(&[1, 2, 3], 1000);
        let (l, s) = decoded(&w, &[21845, 32767, 16383]);
        let got = m.apply(&caps(), &image(&l, &s), &w).unwrap();
        // Green asked for 2x and wants it least, so nothing overshoots
        assert_eq!(got, vec![2000, 2000, 2000]);
    }

    /// A channel at full scale could be anywhere above it, so it halves
    #[test]
    fn a_clipped_channel_comes_down_instead_of_scaling() {
        let m = Metering {
            target: 1.0,
            ..Default::default()
        };
        let w = windows(&[1, 2, 3], 1000);
        let (l, s) = decoded(&w, &[65535, 32767, 32767]);
        let got = m.apply(&caps(), &image(&l, &s), &w).unwrap();
        assert_eq!(got, vec![500, 2000, 2000]);
    }

    /// Infrared measures what is in the way, not color, so a lock leaves it out
    #[test]
    fn infrared_meters_on_its_own_even_when_locked() {
        let m = Metering {
            target: 1.0,
            lock_white_balance: true,
            ..Default::default()
        };
        let w = windows(&[1, 2, 3, Channel::Infrared.id()], 1000);
        let (l, s) = decoded(&w, &[21845, 32767, 32767, 16383]);
        let got = m.apply(&caps(), &image(&l, &s), &w).unwrap();
        // The visible three lock to green's 2x; infrared takes its own 4x
        assert_eq!(got, vec![2000, 2000, 2000, 4000]);
    }

    /// A pass already on target needs no correction and no further pass
    #[test]
    fn a_pass_on_target_is_settled() {
        let m = Metering {
            target: 1.0,
            ..Default::default()
        };
        let w = windows(&[1, 2, 3], 1000);
        let settled = |levels: &[u16]| {
            let (l, s) = decoded(&w, levels);
            m.settled(&image(&l, &s))
        };

        // Full scale is the target, and 65535 is clipped rather than landed
        assert!(settled(&[65000, 65000, 65000]));
        assert!(!settled(&[65535, 65000, 65000]));
        // Half of target is nowhere near it
        assert!(!settled(&[32767, 65000, 65000]));
        // Nothing to measure is nothing to correct
        assert!(settled(&[0, 65000, 65000]));
    }

    /// A dark channel gives us nothing to scale, so it keeps what it had
    #[test]
    fn a_dark_channel_keeps_what_it_had() {
        let m = Metering::default();
        let w = windows(&[1, 2, 3], 1000);
        let (l, s) = decoded(&w, &[0, 32767, 32767]);
        let got = m.apply(&caps(), &image(&l, &s), &w).unwrap();
        assert_eq!(got[0], 1000);
    }
}
