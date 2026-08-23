//! One place the bars and the log agree on stderr
//!
//! indicatif draws by moving the cursor back over the lines it owns. Anything
//! else writing to the same stream lands in the middle of that: the half-drawn
//! bar stops being redrawable and stays in the scrollback, once per log line,
//! with the log line itself hanging off the end of the bar's padding.
//!
//! Every bar is registered here and every log line goes out through
//! [`suspend`](MultiProgress::suspend), so the two take turns rather than
//! overwriting each other.

use indicatif::{MultiProgress, ProgressBar};
use std::{
    io::{self, Write},
    sync::{LazyLock, Mutex},
};
use tracing_subscriber::fmt::MakeWriter;

static BARS: LazyLock<MultiProgress> = LazyLock::new(MultiProgress::new);

/// What [`add`] handed out, so [`clear`] can finish bars nobody else did
///
/// `MultiProgress` erases the terminal but keeps its members, and every
/// `suspend` redraws them - so erasing alone puts an abandoned bar straight
/// back on the screen
static HELD: Mutex<Vec<ProgressBar>> = Mutex::new(Vec::new());

/// Register a bar so it draws alongside the others and out of the log's way
pub fn add(bar: ProgressBar) -> ProgressBar {
    let bar = BARS.add(bar);
    if let Ok(mut held) = HELD.lock() {
        // A strip is a bar per frame plus one per metering pass, so drop the
        // ones already done rather than holding every bar of a long batch
        held.retain(|b| !b.is_finished());
        held.push(bar.clone());
    }
    bar
}

/// Finish with a bar and take it out of the drawing
///
/// `finish_and_clear` stops a bar advancing but leaves it a member of the
/// `MultiProgress`, which redraws every member on the next `suspend` - so the
/// finished line comes straight back, stacked above whatever is drawn next
pub fn done(bar: ProgressBar) {
    bar.finish_and_clear();
    BARS.remove(&bar);
    if let Ok(mut held) = HELD.lock() {
        held.retain(|b| !b.is_finished());
    }
}

/// Take down whatever is still drawn
///
/// A pass that ends early leaves its bar where it was - the `finish_and_clear`
/// calls on the way out of a frame are skipped by the `?` that carries a
/// cancellation up
pub fn clear() {
    if let Ok(mut held) = HELD.lock() {
        held.drain(..).for_each(|b| {
            b.finish_and_clear();
            // Finishing stops it advancing; only removing it takes it out of
            // what the next `suspend` will redraw
            BARS.remove(&b);
        });
    }
    let _ = BARS.clear();
}

/// A `tracing` writer that lifts the bars for the width of one event
pub struct Writer;

impl<'a> MakeWriter<'a> for Writer {
    type Writer = Line;

    fn make_writer(&'a self) -> Line {
        Line(Vec::new())
    }
}

/// One formatted event, held until it can be written between redraws
pub struct Line(Vec<u8>);

impl Write for Line {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    /// The bars are what this is synchronising against, and they are only
    /// stood down in [`Drop`], once the whole event is here
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        // The fmt layer takes a writer per event, so this is one whole line
        // rather than a fragment of one
        let _ = BARS.suspend(|| io::stderr().write_all(&self.0));
    }
}
