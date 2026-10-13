//! The steps that `CBoundarySearch` (LS5000.md3) and `CBrownieBoundarySearch` (LS9000.md3) share

/// An 8-bit analysis image: rows run across the film, columns along it
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Gray {
    pub w: usize,
    pub h: usize,
    /// Row-major, `h` rows of `w`
    pub px: Vec<u8>,
}

impl Gray {
    pub fn at(&self, x: usize, y: usize) -> u8 {
        self.px[y * self.w + x]
    }

    pub fn column(&self, x: usize) -> impl Iterator<Item = u8> + '_ {
        (0..self.h).map(move |y| self.at(x, y))
    }

    /// Only the columns `cols`
    pub fn crop_columns(&mut self, cols: std::ops::Range<usize>) {
        self.px = (0..self.h)
            .flat_map(|y| {
                self.px[y * self.w + cols.start..y * self.w + cols.end]
                    .iter()
                    .copied()
            })
            .collect();
        self.w = cols.len();
    }

    /// Drop `n` rows from each edge, or `None` where there are not that many
    pub fn crop_rows(&mut self, n: usize) -> Option<()> {
        let h = self.h.checked_sub(2 * n)?;
        self.px = self.px[n * self.w..(n + h) * self.w].to_vec();
        self.h = h;
        Some(())
    }

    /// `MakeHistgram`: 256 bins over the whole image
    pub fn histogram(&self) -> [u32; 256] {
        let mut hist = [0u32; 256];
        for &v in &self.px {
            hist[usize::from(v)] += 1;
        }
        hist
    }

    /// `GetMaxGrayscaleValue`: the brightest pixel of a column
    pub fn column_max(&self, x: usize) -> u8 {
        self.column(x).max().unwrap_or(0)
    }

    /// `MakeBinaryImage`'s threshold: the first gray level above `pct` percent of the image
    pub fn percentile(&self, pct: i64) -> u8 {
        let limit = (self.w * self.h) as i64 * pct.clamp(0, 100) / 100;
        self.histogram()
            .iter()
            .scan(0i64, |seen, &n| {
                *seen += i64::from(n);
                Some(*seen)
            })
            .position(|seen| seen > limit)
            .map_or(u8::MAX, |t| t as u8)
    }

    /// `AccumulateImage`: the number of pixels at or below `t` in each column
    pub fn dark_counts(&self, t: u8) -> Vec<i64> {
        (0..self.w)
            .map(|x| self.column(x).filter(|&v| v <= t).count() as i64)
            .collect()
    }
}

/// A profile read before column 0: Nikon reads the Win32 heap header there, which changes its frames
fn heap_header(i: i64) -> i64 {
    match i {
        -1 => 0x0008_0701,
        -2 => 0x0241_0241,
        _ => 0,
    }
}

/// `profile[i]`, read as Nikon reads it outside the profile
pub(crate) fn read(profile: &[i64], i: i64) -> i64 {
    match usize::try_from(i) {
        Ok(i) => profile.get(i).copied().unwrap_or(0),
        Err(_) => heap_header(i),
    }
}

/// The highest column of `cols` from `best`; of equal peaks, the first in `cols` wins
pub(crate) fn climb(
    profile: &[i64],
    best: (i64, i64),
    cols: impl Iterator<Item = i64>,
) -> (i64, i64) {
    cols.fold(best, |(v, at), i| {
        let r = read(profile, i);
        if r > v { (r, i) } else { (v, at) }
    })
}

/// The count and the position of the highest column before the last, the rightmost of equals
pub(crate) fn argmax_from_right(profile: &[i64]) -> (i64, i64) {
    climb(profile, (0, 0), (0..profile.len() as i64 - 1).rev())
}

/// `FindPeekBetweenTwo`: the rightmost maximum of `[a, b]`, or `a + 2` if all are zero
pub(crate) fn peak_between(profile: &[i64], a: i64, b: i64) -> i64 {
    let last = profile.len() as i64 - 1;
    let (a, b) = (a.min(b).clamp(0, last), a.max(b).clamp(0, last));
    climb(profile, (0, a + 2), (a..=b).rev()).1
}

/// `CountPeek`: the full-height columns, less runs longer than 100 that are not at the right edge
pub(crate) fn count_peaks(profile: &[i64], h: usize) -> i64 {
    let h = h as i64;
    let runs: Vec<&[i64]> = profile.chunk_by(|a, b| (*a == h) == (*b == h)).collect();
    let last = runs.len().saturating_sub(1);
    runs.iter()
        .enumerate()
        .filter(|(_, run)| run[0] == h)
        .map(|(i, run)| match run.len() {
            n if n > 100 && i != last => 0,
            n => n as i64,
        })
        .sum()
}

/// `GetEdgeFromBlackFilm`: set unexposed leader and the dark tail of the strip to zero
pub(crate) fn edge_from_black_film(acc: &mut [i64], h: usize) {
    let w = acc.len();
    let floor = h as i64 - 0xf;
    let mut blank = Vec::new();
    let mut x = 0;
    for run in acc.chunk_by(|a, b| (*a >= floor) == (*b >= floor)) {
        let len = run.len();
        if run[0] >= floor && len > 0x78 {
            blank.push(if x < 10 {
                0..(len + x).saturating_sub(8)
            } else if w < len + x + 10 {
                x + 8..w
            } else {
                x + 8..(len + x - 8).max(x + 8)
            });
        }
        x += len;
    }
    for r in blank {
        acc[r].fill(0);
    }
    let lim = h as i64 - 10;
    let tail = acc
        .windows(2)
        .rev()
        .take_while(|pair| pair[0] > lim && pair[1] > lim)
        .count();
    acc[w - tail..].fill(0);
}

/// `CheckEndAddress` (LS5000 `1008d5b0`, LS9000 `100979e0`): the frame end, up to 4 columns left
pub(crate) fn check_end(img: &Gray, acc2: &[i64], mut p: i64) -> i64 {
    if p <= 9 {
        return p;
    }
    for i in (p - 4..p).rev() {
        let m = i64::from(img.column_max(i as usize));
        if acc2[i as usize] == 0 || m > 0x14 {
            return i + 1;
        }
        if m < 5 {
            p += 1;
        }
    }
    p
}

/// `GetLastLineOfLastFrame` (LS5000 `1008dcd0`, LS9000 `10098140`): the last line near `end`
pub(crate) fn last_line(img: &Gray, acc2: &[i64], end: i64, lim: i64) -> i64 {
    let w = img.w as i64;
    if end >= w {
        return w - 1;
    }
    let at = climb(acc2, (0, end + 1), end - 1..w.min(lim)).1;
    check_end(img, acc2, at) - 1
}

/// `GetFrameByAccImage`: each frame's last line, from the gap before the next frame
pub(crate) fn ends(
    img: &Gray,
    acc2: &[i64],
    starts: &[i64],
    window: i64,
    last_line: impl Fn(i64) -> i64,
) -> Vec<i64> {
    starts
        .iter()
        .zip(starts.iter().skip(1).map(Some).chain(std::iter::once(None)))
        .map(|(&s, next)| match next {
            Some(&next) if s < next - 3 => {
                let at = climb(acc2, (0, next - 3), next - window..next).1;
                match check_end(img, acc2, at) {
                    0 => 0,
                    e => e - 1,
                }
            }
            _ => last_line(s),
        })
        .collect()
}

/// The frame geometry a search works to, in thumbnail lines
pub(crate) struct Geometry<F: Fn(i64) -> i64> {
    /// The shortest frame, and the minimum distance between two boundaries
    pub min: i64,
    /// The gap between frames
    pub gap: i64,
    /// One frame's start to the next
    pub period: i64,
    /// How long a frame is taken to be when looking for its end
    pub length: i64,
    /// How far before the next frame its gap is looked for
    pub window: i64,
    /// The widest a frame starting at a line may be
    pub max_width: F,
}

/// `AccumulateMain`: dark counts at the highest percentile (27 to 9) with at most `limit` full columns
pub(crate) fn lower_percentile(img: &Gray, limit: i64) -> Vec<i64> {
    let mut pct = 0x1b;
    loop {
        // MakeBinaryImage and AccumulateImage
        let colsum = img.dark_counts(img.percentile(pct));
        if count_peaks(&colsum, img.h) <= limit || pct <= 9 {
            return colsum;
        }
        pct -= match pct {
            ..6 => 1,
            6..0x15 => 2,
            _ => 3,
        };
    }
}

/// The start of `GetFrameByAccImage`: the profile, and whether column 0 was set to full height
pub(crate) fn gap_profile(mut acc: Vec<i64>, h: usize, margin: i64) -> (Vec<i64>, bool) {
    edge_from_black_film(&mut acc, h);
    let h = h as i64;
    let forced = acc[0] < h - margin && acc[1] < h - margin;
    if forced {
        acc[0] = h;
    }
    (acc, forced)
}

/// `ComplementBoundary`: add boundaries until there are `n`
pub(crate) fn complement<F: Fn(i64) -> i64>(
    acc2: &[i64],
    mut st: Vec<i64>,
    n: usize,
    g: &Geometry<F>,
    check_start: impl Fn(i64) -> i64,
) -> Vec<i64> {
    let w = acc2.len() as i64;
    let mw = &g.max_width;
    let after = |last: i64| {
        check_start(peak_between(
            acc2,
            last + g.min + g.gap,
            last + mw(last) + g.gap,
        ))
    };
    if let Some(&last) = st.last()
        && st.len() < n
        && (w - last) / 2 > g.period
    {
        st.push(after(last));
    }
    if st.len() < n && st[0] - g.period > 0 {
        let s0 = st[0];
        st.push(check_start(peak_between(
            acc2,
            s0 - g.gap - g.min,
            s0 - mw(s0 - g.period),
        )));
    }
    st.sort_unstable();

    // Fill gaps wider than a frame
    for i in 0..st.len().saturating_sub(1) {
        if st.len() >= n {
            break;
        }
        let (a, b) = (st[i], st[i + 1]);
        let span = b - a;
        let lim = mw(a) + g.gap;
        if span <= lim + 0x14 {
            continue;
        }
        let fits = |part: i64| g.min + g.gap < part && part < lim;
        let fill: Vec<i64> = match span / g.period {
            1 => vec![b - g.period],
            2 => vec![a + if fits(span / 2) { span / 2 } else { g.period }],
            3 if fits(span / 3) => vec![a + span / 3, a + 2 * (span / 3)],
            3 => vec![a + g.period, b - g.period],
            k => (1..k).map(|j| a + j * (span / k)).collect(),
        };
        let room = n - st.len();
        st.extend(fill.into_iter().take(room));
    }
    st.sort_unstable();

    // Then add at the end that has space
    while st.len() < n {
        let (s0, last) = (st[0], st[st.len() - 1]);
        let left = s0 - g.min + 0x1e;
        let right = w - g.min - last - 0x5a;
        st.push(if left <= 0 && right <= 0 {
            last
        } else if right < left {
            check_start((s0 - g.period).max(0))
        } else {
            after(last)
        });
        st.sort_unstable();
    }
    st
}

/// The end of `GetFrameByAccImage` and `Execute`: each frame's first and last line
///
/// Starts after the first `n` only go into the last sort (`SortFrameNumber`), which moves no end.
pub(crate) fn finish<F: Fn(i64) -> i64>(
    img: &Gray,
    acc2: &[i64],
    mut starts: Vec<i64>,
    n: usize,
    forced: bool,
    g: &Geometry<F>,
    bound: i64,
) -> Vec<(i64, i64)> {
    let (frames, extra) = starts.split_at_mut(n);
    for s in frames.iter_mut() {
        if *s != 0 || !forced {
            *s += 1;
        }
    }
    let ends = ends(img, acc2, frames, g.window, |s| {
        last_line(img, acc2, s + g.length, s + (g.max_width)(s))
    });
    let w = img.w as i64;
    let checked: Vec<(i64, i64)> = frames
        .iter()
        .zip(ends)
        .map(|(&s, e)| {
            let s = s.max(0);
            let len = e - s;
            let mut e = if len < g.min { s + g.period } else { e };
            if (g.max_width)(s) < len {
                e = s + (g.max_width)(s);
            }
            if e >= bound {
                (w - 1 - (e - s), w - 1)
            } else {
                (s, e)
            }
        })
        .collect();
    let mut sorted: Vec<i64> = checked
        .iter()
        .map(|&(s, _)| s)
        .chain(extra.iter().copied())
        .collect();
    sorted.sort_unstable();
    sorted
        .into_iter()
        .zip(checked)
        .map(|(s, (_, e))| (s, e))
        .collect()
}

/// Zero `profile` within `r` of `c`
pub(crate) fn suppress(profile: &mut [i64], c: i64, r: i64) {
    let w = profile.len() as i64;
    let from = (c - r).clamp(0, w);
    let to = (c + r).clamp(from, w);
    profile[from as usize..to as usize].fill(0);
}
