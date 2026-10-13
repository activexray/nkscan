//! `RemakeThumbnail` (LS5000.md3 `1008a110`): drop thumbnail lines until each perforation has 19

use crate::protocol::data::PerforationInformation;

/// Nikon's address grid for perforation records (`RemakeThumbnail`, `1008a110`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Spec {
    /// Addresses from one perforation to the next
    pitch: u32,
    /// Addresses per perforation decimal; the fifth decimal is the remainder
    phase: u32,
    /// Addresses per encoder pulse
    step: u32,
}

impl Spec {
    /// The grid for a unit of `optical_dpi`; Nikon selects it by the thumbnail width
    pub fn for_dpi(optical_dpi: u16) -> Self {
        match optical_dpi {
            4000 => Self {
                pitch: 756,
                phase: 154,
                step: 7,
            },
            _ => Self {
                pitch: 540,
                phase: 110,
                step: 5,
            },
        }
    }

    /// The lines in one perforation, less one, at `line_pitch`
    fn lines_per_perforation(&self, line_pitch: u32) -> i64 {
        (0.5 + f64::from(self.pitch) / f64::from(line_pitch.max(1))) as i64
    }

    /// `PerfoToRect` (`10089ab0`): the address of a record
    pub fn address(&self, r: &PerforationInformation) -> u32 {
        let decimal = u32::from(r.perf_decimal);
        let length = if decimal < 4 {
            self.phase
        } else {
            self.pitch - 4 * self.phase
        };
        u32::from(r.perf_number) * self.pitch
            + decimal * self.phase
            + u32::from(r.pulse_number).min(length - 1) * self.step
    }
}

/// `SearchNextPerfo` (`10089bb0`): the first line after `s` on a later perforation
fn next_perforation(records: &[PerforationInformation], s: usize) -> usize {
    let n = records.len();
    let Some(v) = records.get(s).map(|r| r.perf_number) else {
        return s + 1;
    };
    (s + 1..n)
        .find(|&i| records[i].perf_number > v)
        .unwrap_or(n.max(s + 1))
}

/// `SearchNextPhase` (`10089bf0`): the first line after `s` on a different decimal
fn next_phase(records: &[PerforationInformation], s: usize) -> usize {
    let n = records.len();
    let Some(d) = records.get(s).map(|r| r.perf_decimal) else {
        return s + 1;
    };
    (s + 1..n)
        .find(|&i| records[i].perf_decimal != d)
        .unwrap_or(n.max(s + 1))
}

/// `RemakeThumbnailForPerfo` and `ForPhase`: keep `[s, e)` less `drop` evenly spaced lines
fn thin(out: &mut Vec<usize>, n: usize, s: usize, e: usize, drop: i64) {
    if drop < 1 {
        out.extend(s..e.min(n));
        return;
    }
    let mut s = s;
    for i in 0..drop {
        let left = e as i64 - (s as i64 + 1);
        let keep = (left / (drop - i)).max(0) as usize;
        out.extend((s + 1..s + 1 + keep).filter(|&l| l < n));
        s += 1 + keep;
    }
}

/// `RemakeThumbnail` (`1008a110`): the pass line of each kept line, up to a count-switching flag
pub(crate) fn resample(
    records: &[PerforationInformation],
    spec: &Spec,
    line_pitch: u32,
) -> Vec<usize> {
    let n = records.len();
    let k = spec.lines_per_perforation(line_pitch);
    let mut out = Vec::with_capacity(n);
    if n == 0 {
        return out;
    }
    let perf = |i: usize| records.get(i).map(|r| r.perf_number);

    let p1 = next_perforation(records, 0);
    let p2 = next_perforation(records, p1);
    let mut reference = Vec::new();
    thin(&mut reference, n, p1, p2, p2 as i64 - p1 as i64 - k - 1);

    // The length of each decimal in the reference interval
    let kept: Vec<PerforationInformation> = reference.iter().map(|&i| records[i].clone()).collect();
    let mut lengths = [0i64; 5];
    let mut phases = 0;
    let mut start = 0;
    for length in &mut lengths {
        let end = next_phase(&kept, start);
        *length = end as i64 - start as i64;
        phases += 1;
        if kept.len() <= end {
            break;
        }
        start = end;
    }

    // Decimals before the first perforation change
    let mut leading = 0;
    let mut i = 0;
    while i < n && perf(i) == perf(0) {
        i = next_phase(records, i);
        leading += 1;
    }

    let first = next_phase(records, 0);
    out.extend(0..first);
    let mut cur = first;
    let mut j = phases as i64 - leading;
    loop {
        j += 1;
        if phases as i64 <= j {
            break;
        }
        let e = next_phase(records, cur);
        // Keep all lines if there is no reference length for this decimal
        let target = usize::try_from(j)
            .ok()
            .map_or(e as i64 - cur as i64, |j| lengths[j]);
        thin(&mut out, n, cur, e, e as i64 - cur as i64 - target);
        cur = e;
        if perf(e) != perf(0) {
            break;
        }
    }
    out.extend_from_slice(&reference);

    let mut c = next_perforation(records, cur);
    loop {
        let e = next_perforation(records, c);
        if !(e < n && !records[e].count_switching_flag) {
            break;
        }
        thin(&mut out, n, c, e, e as i64 - c as i64 - k - 1);
        c = e;
    }
    out.extend(c..n);
    out
}
