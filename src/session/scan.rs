//! Taking scan passes: thumbnail, prescan, and full resolution
//!
//! Every kind of pass is the same sequence: stage the windows, start the scan,
//! read the stream a chunk at a time while a decoder unscrambles it.

use super::Session;
use crate::{
    error::Error,
    protocol::{decode::Samples, image::Layout, window::Window},
    scan::pass::{self, Pass, Progress},
    session::window::Started,
};
use std::{
    collections::VecDeque,
    ops::ControlFlow,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};
use tracing::*;

/// How many raw chunks the reader and decoder have in flight between them
const POOL: usize = 3;

/// A chunk handed from the reader thread to the decoder, or how the stream ended
enum Chunk {
    Data(Vec<u8>),
    End,
    Failed(Error),
}

struct TruncationState {
    line: usize,
    offset_in_line: usize,
}

impl TruncationState {
    fn new() -> Self {
        Self {
            line: 0,
            offset_in_line: 0,
        }
    }
}

fn strip_truncation(buf: &mut Vec<u8>, state: &mut TruncationState, layout: &Layout) {
    // 2-11-5-3 counts the invalid bytes per CCD row, so packed rows carry one
    // set each and the whole group is what a line means here
    let rows = usize::from(layout.packed_rows);

    let line_bytes = layout.bytes_per_line() as usize * rows;

    let (first_bytes, last_bytes) = layout.truncated_bytes_line;
    let first_bytes = first_bytes as usize * rows;
    let last_bytes = last_bytes as usize * rows;

    let total_lines = layout.lines as usize;
    let first_line = layout.truncated_lines_frame.0 as usize;
    let last_line = total_lines - layout.truncated_lines_frame.1 as usize;

    let mut read = 0;
    let mut write = 0;

    while read < buf.len() {
        let remaining_in_line = line_bytes - state.offset_in_line;
        let n = remaining_in_line.min(buf.len() - read);

        let line = state.line;

        // Is this line part of the actual image?
        if line >= first_line && line < last_line {
            let line_start = state.offset_in_line;
            let line_end = state.offset_in_line + n;

            // Keep only the intersection with:
            //
            //     [first_bytes, line_bytes - last_bytes)
            //
            let keep_start = line_start.max(first_bytes);
            let keep_end = line_end.min(line_bytes - last_bytes);

            if keep_start < keep_end {
                let src_start = read + (keep_start - line_start);
                let src_end = read + (keep_end - line_start);
                let len = src_end - src_start;

                buf.copy_within(src_start..src_end, write);
                write += len;
            }
        }

        read += n;
        state.offset_in_line += n;

        if state.offset_in_line == line_bytes {
            state.offset_in_line = 0;
            state.line += 1;
        }
    }

    buf.truncate(write);
}

impl Session {
    /// Stage the windows and start a scan pass, returning once the data is ready
    ///
    /// `timeout` bounds the wait for the unit to report ready after SCAN, and
    /// nothing else. Each read of the data that follows carries its own
    /// `MOVE_TIMEOUT`, so a long pass is bounded a chunk
    /// at a time rather than as a whole.
    ///
    /// The caller owes the unit a read: a scan whose data is never read locks
    /// out every command that follows
    pub fn start_pass(&mut self, windows: &[Window], timeout: Duration) -> Result<Started, Error> {
        for w in windows {
            self.set_window(w)?;
        }
        let started = self.scan(windows)?;
        // Whether the unit reports ready as soon as it is streaming or only
        // once the whole pass is taken decides what this budget has to cover
        let waited = Instant::now();
        self.test_unit_ready(timeout)?;
        debug!(ready_in = ?waited.elapsed(), "scan ready");
        Ok(started)
    }

    /// Start a pass and unscramble it into `samples` as it arrives
    ///
    /// `samples` is resized for this pass's shape; the caller owns it, so a
    /// batch reuses the one allocation pass to pass rather than growing a new
    /// one. `samples.color` is row-major, channels interleaved per pixel, and
    /// `samples.ir` is likewise but only `Some` where the windows carried
    /// infrared. See [`Samples`].
    pub fn scan_pass(
        &mut self,
        windows: &[Window],
        timeout: Duration,
        samples: &mut Samples,
    ) -> Result<Pass, Error> {
        self.scan_pass_with(windows, timeout, samples, |_| ControlFlow::Continue(()))
    }

    /// The same as [`Self::scan_pass`], telling `on` how far along the pass is
    /// after every chunk and letting it cancel the pass by returning `Break`
    ///
    /// `on` runs on the decoding thread between chunks, so anything slow in it
    /// is time the unit spends waiting for the next read with its buffer filling.
    /// A cancelled pass fails with [`Error::Cancelled`]; the unread remainder is
    /// drained by [`Chunks`](super::image::Chunks)'s own `Drop`, the same path
    /// a consumer that simply stops reading already takes, so nothing here has
    /// to wait for the mechanism or send `ABORT`
    pub fn scan_pass_with(
        &mut self,
        windows: &[Window],
        timeout: Duration,
        samples: &mut Samples,
        mut on: impl FnMut(Progress) -> ControlFlow<()>,
    ) -> Result<Pass, Error> {
        let started = self.start_pass(windows, timeout)?;
        let layout = started.layout.clone();

        let total = layout.total_bytes();

        let curves = self.curves();
        let mut decoder = pass::decoder(&layout, curves.as_deref())?;
        samples.resize_for(&decoder);

        let timing = Timing::default();
        let mut decoding = Duration::ZERO;
        let mut idle = Duration::ZERO;
        let mut truncation = TruncationState::new();
        let reader_layout = layout.clone();

        thread::scope(|scope| {
            let (full_tx, full_rx) = mpsc::channel::<Chunk>();
            let (empty_tx, empty_rx) = mpsc::channel::<Vec<u8>>();
            let timing = &timing;
            scope.spawn(move || read_chunks(self, &reader_layout, &full_tx, &empty_rx, timing));

            let mut out = Ok(());
            let mut bytes = 0u64;

            loop {
                let waited = Instant::now();
                let msg = full_rx.recv();
                idle += waited.elapsed();

                let mut chunk = match msg {
                    Ok(Chunk::Data(buf)) => buf,
                    Ok(Chunk::End) | Err(_) => break,
                    Ok(Chunk::Failed(e)) => {
                        out = Err(e);
                        break;
                    }
                };

                bytes += chunk.len() as u64;

                strip_truncation(&mut chunk, &mut truncation, &layout);

                let pushed = Instant::now();
                let decoded = decoder.push(&chunk, samples);
                decoding += pushed.elapsed();

                let _ = empty_tx.send(chunk);
                if let Err(e) = decoded {
                    out = Err(e);
                    break;
                }
                let flow = on(Progress {
                    bytes,
                    total,
                    blocks: decoder.decoded(),
                });
                if flow.is_break() {
                    out = Err(Error::Cancelled);
                    break;
                }
            }
            out
        })?;

        // `starved` is the only one of these the unit can feel: it is time we
        // spent not asking for data, with its buffer filling behind the stage
        debug!(
            blocks = decoder.decoded(),
            complete = decoder.complete(),
            chunks = Timing::get(&timing.chunks),
            bytes = Timing::get(&timing.bytes),
            read_ms = Timing::get(&timing.read) / 1_000_000,
            starved_ms = Timing::get(&timing.starved) / 1_000_000,
            decode_ms = decoding.as_millis(),
            idle_ms = idle.as_millis(),
            "pass"
        );
        let (rows, cols) = decoder.shape();
        Ok(Pass {
            layout: started.layout,
            cooperation: started.cooperations,
            complete: decoder.complete(),
            rows,
            cols,
        })
    }

    /// Scan everything loaded at the lowest resolution
    ///
    /// Builds its own windows from the capabilities (whole strip, lowest dpi,
    /// one channel per color), seeds white balance, and takes the pass
    pub fn scan_thumbnail(&mut self, samples: &mut Samples) -> Result<Pass, Error> {
        self.scan_thumbnail_with(samples, |_| ControlFlow::Continue(()))
    }

    /// The same as [`Self::scan_thumbnail`], letting `on` cancel by returning `Break`
    pub fn scan_thumbnail_with(
        &mut self,
        samples: &mut Samples,
        on: impl FnMut(Progress) -> ControlFlow<()>,
    ) -> Result<Pass, Error> {
        if !crate::scan::thumbnail::available(self.capabilities()) {
            return Err(Error::Unsupported {
                op: "thumbnail",
                reason: "this unit and adapter do not offer thumbnail scanning".into(),
            });
        }

        let windows = crate::scan::thumbnail::windows(self.capabilities())?;
        let windows = self.seed_white_balance(&windows)?;
        self.scan_pass_with(&windows, THUMBNAIL_TIMEOUT, samples, on)
    }
}

/// Long enough for a whole-strip pass at thumbnail resolution
const THUMBNAIL_TIMEOUT: Duration = Duration::from_secs(600);

/// Where a pass spent its time, so a unit that pauses can be told from a
/// decoder that will not keep up
///
/// The unit streams while the stage runs and has only its own buffer to hold
/// what we have not taken yet. Nothing is read while the reader waits for a
/// buffer to come back, so `starved` is time we spend not asking for data, and
/// it is the only one of these that stalls the mechanism. `idle` is the other
/// way round: the decoder had nothing to do because the unit had nothing to give
#[derive(Default)]
struct Timing {
    /// In READ, which is the unit's own pace
    read: AtomicU64,
    /// Waiting for the decoder to give a buffer back
    starved: AtomicU64,
    chunks: AtomicU64,
    bytes: AtomicU64,
}

impl Timing {
    fn add(counter: &AtomicU64, by: u64) {
        counter.fetch_add(by, Ordering::Relaxed);
    }

    fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

/// Read the whole stream off the unit a chunk at a time, forwarding each chunk
/// down `full` and drawing the buffer to fill from the pool `empty` keeps up
fn read_chunks(
    session: &mut Session,
    layout: &Layout,
    full: &Sender<Chunk>,
    empty: &Receiver<Vec<u8>>,
    timing: &Timing,
) {
    let mut chunks = match session.image_chunks(layout) {
        Ok(chunks) => chunks,
        Err(e) => {
            let _ = full.send(Chunk::Failed(e));
            let _ = full.send(Chunk::End);
            return;
        }
    };

    let mut pool: VecDeque<Vec<u8>> = (0..POOL).map(|_| vec![0u8; chunks.capacity()]).collect();

    loop {
        let mut buf = match pool.pop_front() {
            Some(buf) => buf,
            None => {
                let waited = Instant::now();
                let buf = match empty.recv() {
                    Ok(buf) => {
                        trace!("got empty buffer");
                        buf
                    }
                    Err(_) => return,
                };
                Timing::add(&timing.starved, waited.elapsed().as_nanos() as u64);
                buf
            }
        };

        let reading = Instant::now();
        let filled = chunks.fill(&mut buf);
        Timing::add(&timing.read, reading.elapsed().as_nanos() as u64);

        match filled {
            Some(Ok(got)) => {
                Timing::add(&timing.chunks, 1);
                Timing::add(&timing.bytes, got as u64);
            }
            Some(Err(e)) => {
                let _ = full.send(Chunk::Failed(e));
                let _ = full.send(Chunk::End);
                return;
            }
            None => {
                let _ = full.send(Chunk::End);
                return;
            }
        }
        if full.send(Chunk::Data(buf)).is_err() {
            return;
        }
    }
}
