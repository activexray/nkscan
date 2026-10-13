//! Autoexposure: deciding per-channel exposures before a scan

use crate::{
    error::Error,
    protocol::{
        caps::film::FilmFormat,
        data::Rect,
        decode::{Image, Samples},
        window::Window,
    },
    scan::{
        autoexpose::{AutoExposure, Exposures, prescan_windows},
        meter::{Metering, Step},
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
    /// cover the channels `recipe` will scan
    pub fn autoexpose_frame(
        &mut self,
        frame: Rect,
        recipe: &Recipe,
        metering: &Metering,
    ) -> Result<Exposures, Error> {
        self.autoexpose_frame_with(frame, recipe, metering, |_, _| ControlFlow::Continue(()))
    }

    /// The same as [`Self::autoexpose_frame`], with `on` given which metering pass is running
    /// (counting from one) and letting it cancel by returning `Break`
    pub fn autoexpose_frame_with(
        &mut self,
        frame: Rect,
        recipe: &Recipe,
        metering: &Metering,
        on: impl FnMut(usize, Progress) -> ControlFlow<()>,
    ) -> Result<Exposures, Error> {
        let windows = recipe
            .metering(self.capabilities())
            .windows(self.capabilities(), frame)?;
        // The crop needs a format; the holder can give it
        let mut metering = *metering;
        if metering.format.is_none() {
            metering.format = FilmFormat::resolve(None, self.capabilities()).ok();
        }
        self.autoexpose_with(&windows, &metering, on)
    }

    /// Meter `windows` and answer their new exposures
    ///
    /// A set is what gets metered rather than a channel: one descriptor per
    /// channel, one exposure in each, and a lock that decides them against one
    /// another. Which mechanism runs is the unit's to advertise.
    pub fn autoexpose(
        &mut self,
        windows: &[Window],
        metering: &Metering,
    ) -> Result<Exposures, Error> {
        self.autoexpose_with(windows, metering, |_, _| ControlFlow::Continue(()))
    }

    /// The same as [`Self::autoexpose`], letting `on` cancel by returning `Break`
    ///
    /// A unit that meters for itself takes a pass we never read, so `on` never
    /// runs and cannot cancel that mechanism
    pub fn autoexpose_with(
        &mut self,
        windows: &[Window],
        metering: &Metering,
        mut on: impl FnMut(usize, Progress) -> ControlFlow<()>,
    ) -> Result<Exposures, Error> {
        let mechanism = AutoExposure::choose(self.capabilities(), metering)?;
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
                // measured at start-up, and Nikon Scan meters from it
                let seeded = self.seed_white_balance(windows)?;
                let base: Vec<u32> = seeded.iter().map(|w| w.exposure).collect();

                let mut windows = prescan_windows(self.capabilities(), &seeded);
                for (w, e) in windows.iter_mut().zip(metering.seed(&seeded, &base)) {
                    w.exposure = e;
                }
                let mut samples = Samples::default();
                for pass in 1.. {
                    let taken =
                        self.scan_pass_with(&windows, PASS_TIMEOUT, &mut samples, |p| on(pass, p))?;
                    let image = Image::new(&taken.layout, &samples)?;
                    let step = metering.step(self.capabilities(), &image, &windows, &base, pass)?;
                    debug!(pass, ?step, "metering pass");
                    let (Step::Again(next) | Step::Done(next)) = &step;
                    for (w, &exposure) in windows.iter_mut().zip(next) {
                        w.exposure = exposure;
                    }
                    if matches!(step, Step::Done(_)) {
                        break;
                    }
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
