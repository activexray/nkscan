//! `CDataManager::BoundaryAdjustment` (LS5000.md3 `10089d20`): fit the frames to 144 resampled lines

/// Resampled lines in one frame (`LimitsBoundary`, `100892b0`)
const FRAME: i64 = 0x90;
/// A gap wider than this is a missing frame (`MiddleBoundaryAdjustment`, `10089330`)
const GAP: i64 = 0x60;

/// A frame's first and last line
type Span = (i64, i64);

/// Whether the gap before frame `i` is wide enough to hold a frame
fn gap_before(b: &[Span], i: usize) -> bool {
    (b[i].0 - b[i - 1].1).abs() > GAP
}

/// Put `frame` at `i` in `out`, then the frames of `from` after it; Nikon reads `from`, not `out`
fn insert(out: &mut [Span], from: &[Span], i: usize, frame: Span) {
    out[i + 1..].copy_from_slice(&from[i..from.len() - 1]);
    out[i] = frame;
}

/// The adjusted frames, up to the first with no end; `None` if the first starts after line `valid`
pub(crate) fn adjust(frames: &[Span], valid: i64) -> Option<Vec<Span>> {
    let n = frames.len();
    let mut b = frames.to_vec();

    // LimitsBoundary (100892b0): no frame is longer than 144 or goes past `valid`
    for i in 0..n {
        if (b[i].1 - b[i].0) as u32 > FRAME as u32 {
            b[i].1 = b[i].0 + FRAME;
        }
        if valid < b[i].0 {
            b[i] = *b.get(i.checked_sub(1)?)?;
        } else if valid < b[i].1 {
            b[i].1 = valid;
            // Nikon reads before its array for the first frame; this is not reproduced
            if i > 0 && ((valid - FRAME) as u32) < (b[i - 1].1 as u32) {
                b[i].0 = b[i - 1].1 + 1;
            }
        }
    }

    // MiddleBoundaryAdjustment (10089330): a frame into each wide gap
    let mut mid = b.clone();
    for i in (1..n.saturating_sub(1)).filter(|&i| gap_before(&b, i)) {
        insert(&mut mid, &b, i, (b[i - 1].1 + 3, b[i - 1].1 + 0x93));
    }
    if n >= 2 && gap_before(&b, n - 1) {
        mid[n - 1] = (b[n - 2].1 + 3, b[n - 2].1 + 0x93);
    }

    // Add frames before the first, then fill gaps again from `mid`
    let (k, mut out) = match mid.first() {
        Some(&(s, _)) if s >= 0x61 => {
            // A count above `n` becomes `n - 1`, as Nikon's does
            let k = match (s / GAP) as usize {
                k if k > n => n - 1,
                k => k,
            };
            let lead = (0..k as i64).map(|j| (j * FRAME, (j + 1) * FRAME));
            (k, lead.chain(mid[..n - k].iter().copied()).collect())
        }
        _ => (1, mid.clone()),
    };
    for u in (k..n.saturating_sub(1)).filter(|&u| gap_before(&mid, u)) {
        insert(&mut out, &mid, u, (mid[u].0 - FRAME, mid[u].0));
    }

    // OffsetAnalyze (1008b9f0) stops at the first frame with no end
    Some(out.into_iter().take_while(|&(_, e)| e != 0).collect())
}
