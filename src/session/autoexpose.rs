//! Autoexposure: deciding per-channel exposures before a scan

use crate::{
    error::Error,
    protocol::{
        data::Rect,
        decode::{Image, Samples},
        window::Window,
    },
    scan::{
        autoexpose::{AutoExposure, Exposures, prescan_windows},
        pass::Progress,
        window::Recipe,
    },
    session::Session,
};
use std::{ops::ControlFlow, time::Duration};
use tracing::*;

/// Long enough for a low-resolution pass over a whole frame
const PASS_TIMEOUT: Duration = Duration::from_secs(300);

impl Session {
    /// Meter `frame` and answer the exposures to scan it with
    ///
    /// Builds its own metering windows from `recipe`, so the exposures come off
    /// the frame rather than off whatever the last pass left in the unit, and
    /// cover the channels `recipe` will scan. `lock_white_balance` keeps the
    /// channels in the ratio the unit calls neutral, which is what a scan that
    /// has to stay comparable to the next one wants.
    pub fn autoexpose_frame(
        &mut self,
        frame: Rect,
        recipe: &Recipe,
        lock_white_balance: bool,
    ) -> Result<Exposures, Error> {
        self.autoexpose_frame_with(frame, recipe, lock_white_balance, |_, _| {
            ControlFlow::Continue(())
        })
    }

    /// The same as [`Self::autoexpose_frame`], with `on` given which metering pass is running
    /// (counting from one) and letting it cancel by returning `Break`
    pub fn autoexpose_frame_with(
        &mut self,
        frame: Rect,
        recipe: &Recipe,
        lock_white_balance: bool,
        on: impl FnMut(usize, Progress) -> ControlFlow<()>,
    ) -> Result<Exposures, Error> {
        let windows = recipe
            .metering(self.capabilities())
            .windows(self.capabilities(), frame)?;
        self.autoexpose_with(&windows, lock_white_balance, on)
    }

    /// Meter `windows` and answer their new exposures
    ///
    /// A set is what gets metered rather than a channel: one descriptor per
    /// channel, one exposure in each, and a lock that decides them against one
    /// another. Which mechanism runs is the unit's to advertise.
    pub fn autoexpose(
        &mut self,
        windows: &[Window],
        lock_white_balance: bool,
    ) -> Result<Exposures, Error> {
        self.autoexpose_with(windows, lock_white_balance, |_, _| ControlFlow::Continue(()))
    }

    /// The same as [`Self::autoexpose`], letting `on` cancel by returning `Break`
    ///
    /// A unit that meters for itself takes a pass we never read, so `on` never
    /// runs and cannot cancel that mechanism
    pub fn autoexpose_with(
        &mut self,
        windows: &[Window],
        lock_white_balance: bool,
        mut on: impl FnMut(usize, Progress) -> ControlFlow<()>,
    ) -> Result<Exposures, Error> {
        let mechanism = AutoExposure::choose(self.capabilities(), lock_white_balance)?;
        debug!(?mechanism, "metering");

        match mechanism {
            AutoExposure::Unit(kind) => {
                // The unit meters during a pass of its own. It writes the result
                // into the descriptors, so GET WINDOW is what reports it
                let mut metering = windows.to_vec();
                for w in &mut metering {
                    w.scanning_kind = kind;
                }
                // Its own numbers are the point, not the image, so the pass is
                // stopped rather than read out
                self.start_pass(&metering, PASS_TIMEOUT)?;
                self.abort()?;

                // Only the channels we asked it to meter: the unit holds a
                // descriptor for every channel it has, metered or not
                let held = self.windows()?;
                let mut exposures = Exposures::read(windows);
                for w in windows {
                    if let Some(metered) = held.iter().find(|h| h.id == w.id) {
                        exposures.set(w.channel(), metered.exposure);
                    }
                }
                Ok(exposures)
            }

            AutoExposure::Host(metering) => {
                // Exposures persist in the unit across sessions, so metering from
                // whatever is in the descriptors compounds run over run.
                // `DataType::WhiteBalanceExposure` is the unit's own neutral,
                // measured at start-up, so we start there every time.
                let seeded = self.seed_white_balance(windows)?;

                let mut windows = prescan_windows(self.capabilities(), &seeded);
                let mut samples = Samples::default();
                let mut layout = None;
                let mut n = 0;
                loop {
                    let taken = self
                        .scan_pass_with(&windows, PASS_TIMEOUT, &mut samples, |p| on(n + 1, p))?;
                    let layout = layout.insert(taken.layout);
                    let image = Image::new(layout, &samples)?;
                    n += 1;

                    // Correct from what this pass measured, whether or not
                    // another one follows: the exposures the scan gets are the
                    // ones the last pass asked for, never the ones it ran at
                    let next = metering.apply(self.capabilities(), &image, &windows)?;
                    for (w, exposure) in windows.iter_mut().zip(next) {
                        w.exposure = exposure;
                    }

                    // A level below full scale says exactly what exposure lands
                    // on target, so confirming it costs a pass to learn nothing.
                    // Only a clipped channel, whose correction is a retreat
                    // rather than a measurement, is worth another
                    let measured = metering.measured(&image, &windows);
                    debug!(pass = n, measured, "metering pass");
                    if measured {
                        break;
                    }
                    if n >= metering.max_passes.max(1) {
                        debug!(passes = n, "metering never came off full scale");
                        break;
                    }
                }

                let layout = layout.expect("the loop runs at least once");
                let measured = metering.measure(&Image::new(&layout, &samples)?, &windows);
                for (n, (window, level)) in windows.iter().zip(&measured).enumerate() {
                    let unit = self.setup(window.id).ok();
                    let image = unit.as_ref().and_then(|s| s.images.first());
                    debug!(
                        channel = n,
                        id = window.id,
                        ours = level,
                        base_level = unit.as_ref().map(|s| s.base_level),
                        unit_min = image.map(|i| i.min),
                        unit_max = image.map(|i| i.max),
                        "metering levels"
                    );
                }

                Ok(Exposures::read(&windows))
            }
        }
    }

    /// The same windows with the unit's start-up exposures in them
    ///
    /// Any pass wants this, not just a metered one: the channels do not read
    /// neutral at equal exposures, so a descriptor left at 0 comes back with a
    /// cast. Infrared is seeded the same way, from the same record: metering
    /// scales the exposure it finds, so a channel left at 0 stays at 0 however
    /// many passes it gets, and the mask comes back at whatever the unit
    /// defaults to rather than at the level the film base asks for.
    ///
    /// A channel the unit has no reading for keeps what it came with
    pub fn seed_white_balance(&mut self, windows: &[Window]) -> Result<Vec<Window>, Error> {
        let mut seeded = Vec::with_capacity(windows.len());
        for w in windows {
            let mut w = w.clone();
            match self.white_balance(w.channel()) {
                Ok(exposure) => w.exposure = exposure,
                Err(e) => debug!(id = w.id, %e, "no start-up exposure for this channel"),
            }
            seeded.push(w);
        }
        Ok(seeded)
    }
}
