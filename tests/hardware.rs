//! Tests that need a scanner. Ignored by default, so `cargo test` stays pure:
//!
//! ```text
//! cargo test --test hardware -- --ignored
//! ```

use nkscan::{
    device,
    protocol::{
        caps::set_window::ColorInterleaving,
        decode::{Image, Samples},
    },
    scan::{
        boundaries::Polarity,
        frame::{self, Options},
        framing::{self, Framing},
        window::Recipe,
    },
    session::Session,
};
use std::ops::ControlFlow;

/// A rectangle scanned twice in one session comes back in the same place
///
/// Needs a perforation-framed unit with a 35mm strip loaded. Such a unit
/// positions the film by its own frame table, and a pass that corrects a
/// frame's place has to put the measured table back: registering the
/// correction replaces the entry the original top selects, so the second scan
/// selects the entry below it and reads the wrong film. Before the fix the
/// second pass came back 87 columns out with the frame's tail off the end.
#[test]
#[ignore = "needs a perforation-framed scanner with 35mm film loaded"]
fn a_frame_scanned_twice_comes_back_in_the_same_place() {
    let devices = device::list();
    let device = devices.first().expect("a scanner");
    let mut session = Session::open(device.open().expect("open")).expect("session");
    if Framing::choose(session.capabilities()) != Framing::Perforation {
        eprintln!("not a perforation-framed unit, nothing to test");
        return;
    }
    session.stage().expect("stage");

    let mut samples = Samples::default();
    let discovery = framing::discover(&mut session, None, &mut samples).expect("discovery");
    let frame = *discovery
        .frames
        .get(3)
        .or(discovery.frames.first())
        .expect("a frame");

    let recipe = Recipe {
        dpi: 500,
        samples: 1,
        interleaving: ColorInterleaving::LINE_WITHOUT_DISTANCE,
        infrared: false,
    };

    let mut round = || {
        let scanned = frame::scan_frame_with(
            &mut session,
            &recipe,
            frame,
            Options {
                polarity: Some(Polarity::Negative),
                ..Options::default()
            },
            &mut samples,
            |_, _| ControlFlow::Continue(()),
        )
        .expect("scan");
        let image = Image::new(&scanned.pass.layout, &samples).expect("image");
        let plane = image.colors.first().expect("a color plane");
        let mean = plane.iter().map(|&s| f64::from(s)).sum::<f64>() / plane.len() as f64;
        (scanned.frame_lines, mean)
    };

    let (first, first_mean) = round();
    let (second, second_mean) = round();

    let moved = first.start.abs_diff(second.start);
    assert!(
        moved <= 4,
        "the frame moved {moved} columns between passes: {first:?} then {second:?}"
    );
    // The same film either way, so the level cannot jump
    let ratio = first_mean.max(second_mean) / first_mean.min(second_mean);
    assert!(
        ratio < 1.05,
        "the passes read different film: {first_mean:.0} then {second_mean:.0}"
    );
}
