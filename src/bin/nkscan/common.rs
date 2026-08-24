//! Film handling and progress helpers shared by the scanning commands
//!
//! Lifted out of `scan` so `discover` can drive the same load-and-wait flow
//! against the same prompts, and draw its pass on the same bars.

use crate::{cancel, progress};
use anyhow::Result;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use nkscan::{
    error::Error,
    scan::pass::Progress,
    session::Session,
};

/// Moving a pass's progress onto a bar
pub trait Report {
    fn report(&self, progress: Progress);
}

impl Report for ProgressBar {
    fn report(&self, progress: Progress) {
        // The layout's own total, which the cooperative modes can make wrong, so
        // it is set every time rather than once
        self.set_length(progress.total);
        self.set_position(progress.bytes);
    }
}
use std::{borrow::Cow, io::Write, io::IsTerminal, time::Duration};
use tracing::*;

/// How often to ask whether a holder has gone in
pub const HOLDER_POLL: Duration = Duration::from_millis(500);

/// How often the spinner moves while that is going on
const SPINNER_TICK: Duration = Duration::from_millis(120);

/// Whether there is film to scan
///
/// A feeder or a cartridge keeps its film behind the gate, so nothing reads as
/// loaded until the unit is told to take some in. `take_in` is whether it may:
/// false once a medium has been scanned and ejected, where loading again would
/// only bring the same film back
pub fn ready(session: &mut Session, take_in: bool) -> Result<bool, Error> {
    if session.media_loaded()? {
        return Ok(true);
    }
    if !take_in || !session.load()? {
        return Ok(false);
    }
    // The address page now describes the medium that came in
    session.refresh()?;
    session.media_loaded()
}

/// The same, counting anything the operator can put right as "not yet": an open
/// door is what the prompt is for
fn waiting(session: &mut Session, take_in: bool) -> Result<bool, Error> {
    match ready(session, take_in) {
        Err(Error::Media(condition)) => {
            debug!(%condition, "waiting on the operator");
            Ok(false)
        }
        other => other,
    }
}

/// [`Session::refresh`], tolerating the medium-not-present it can itself hit:
/// a strip feeder's gate sits empty between strips, and that is the state this
/// is called from a loop to wait out, not a failure of the refresh
fn refresh_while_empty(session: &mut Session) -> Result<(), Error> {
    match session.refresh() {
        Ok(()) | Err(Error::Media(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Wait until a holder is loaded, then put the unit in a state to scan from
pub fn wait_for_film(session: &mut Session, prompt: &str, take_in: bool) -> Result<()> {
    // An eject leaves what we know about the holder behind, so ask again before
    // believing anything is in there
    refresh_while_empty(session)?;

    if !waiting(session, take_in)? {
        // The spinner is the affordance on a terminal, and hidden anywhere else, so the log says it too
        info!("{prompt}");
        let spinner = ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden());
        spinner.set_style(ProgressStyle::default_spinner());
        spinner.set_message(format!("{prompt}. Ctrl-c to stop"));
        let spinner = progress::add(spinner);
        spinner.enable_steady_tick(SPINNER_TICK);
        loop {
            // Nothing is moving while this waits, so stopping here is always safe
            if cancel::requested() {
                return Err(Error::Cancelled.into());
            }
            std::thread::sleep(HOLDER_POLL);
            refresh_while_empty(session)?;
            // Retrying the load is what picks up a supply that was refilled
            if waiting(session, take_in)? {
                progress::done(spinner);
                break;
            }
        }
    }
    info!("film loaded");
    session.stage()?;
    Ok(())
}

/// Ask the operator for something and wait for them, answering whether anyone
/// was there to answer. Polls for Ctrl-c on a background reader thread, same
/// as every other wait
pub fn confirm(prompt: &str) -> Result<bool> {
    eprintln!("{prompt}. Ctrl-c to stop");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = tx.send(std::io::stdin().read_line(&mut line).map(|n| n > 0));
    });
    loop {
        if cancel::requested() {
            return Err(Error::Cancelled.into());
        }
        match rx.recv_timeout(HOLDER_POLL) {
            Ok(answered) => {
                // The Enter that answered this has been echoed as a line of its
                // own, so step back over it rather than leaving a blank line
                let mut err = std::io::stderr().lock();
                if err.is_terminal() {
                    let _ = err.write_all(b"\x1b[1A\x1b[2K");
                }
                return Ok(answered?);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(false),
        }
    }
}

/// A bar for one pass
///
/// The length is not known until the first chunk arrives, so it starts empty
/// and learns. Hidden by indicatif when stderr is not a terminal, and drawn no
/// more than 20 times a second, which is what keeps the callback off the
/// scanner's back
pub fn pass_bar(label: impl Into<Cow<'static, str>>, total: u64) -> ProgressBar {
    // A bar built the usual way draws straight to stderr, so styling and naming
    // it here would leave that first line behind the moment `add` moves it onto
    // the shared draw target. Built hidden, it draws nothing until it is added
    let bar = ProgressBar::with_draw_target(Some(total), ProgressDrawTarget::hidden());
    bar.set_style(
        ProgressStyle::with_template(
            "{msg:<9} [{bar:30}] {bytes}/{total_bytes}  {bytes_per_sec}  eta {eta}",
        )
        .expect("a template of ours")
        .progress_chars("=> "),
    );
    bar.set_message(label.into());
    progress::add(bar)
}

/// A pass bar that learns its length from the first chunk
///
/// Discovery passes do not know how long they are until data starts arriving,
/// so the bar stays hidden until then rather than drawing `0 B/0 B`. One of
/// these per pass; [`PassBar::done`] hands it back to the shared draw target.
pub struct PassBar {
    label: &'static str,
    bar: Option<ProgressBar>,
}

impl PassBar {
    pub fn new(label: &'static str) -> Self {
        Self { label, bar: None }
    }

    /// Report one chunk onto the bar, creating the bar if this was the first
    pub fn update(&mut self, p: Progress) {
        if self.bar.is_none() && p.total > 0 {
            self.bar = Some(pass_bar(self.label, p.total));
        }
        if let Some(bar) = &self.bar {
            bar.report(p);
        }
    }

    /// Retire the bar, whether or not any data ever arrived
    pub fn done(self) {
        if let Some(bar) = self.bar {
            progress::done(bar);
        }
    }
}
