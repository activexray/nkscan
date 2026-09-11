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
        frame::{self, Options},
        framing::{self, Framing},
        window::Recipe,
    },
    session::Session,
};
use std::ops::ControlFlow;

/// The mean of each column of the first color plane, down the feed
fn profile(image: &Image) -> Vec<f64> {
    let plane = image.colors.first().expect("a color plane");
    (0..image.cols)
        .map(|x| {
            let sum: f64 = (0..image.rows)
                .map(|y| f64::from(plane[y * image.cols + x]))
                .sum();
            sum / image.rows as f64
        })
        .collect()
}

/// The shift, in columns, that puts `b` over `a` best
///
/// The same film read twice aligns at 0. Film the unit positioned differently
/// aligns somewhere else, or nowhere
fn shift(a: &[f64], b: &[f64], most: usize) -> isize {
    let cost = |by: isize| {
        let (from, to) = (most as isize, a.len() as isize - most as isize);
        let mut sum = 0.0;
        for x in from..to {
            let d = a[x as usize] - b[(x + by) as usize];
            sum += d * d;
        }
        sum
    };
    (-(most as isize)..=most as isize)
        .min_by(|&p, &q| cost(p).total_cmp(&cost(q)))
        .expect("a shift")
}

/// A rectangle scanned twice in one session comes back in the same place
///
/// Needs a perforation-framed unit with a 35mm strip loaded. Such a unit
/// positions the film by its own frame table, and a pass that registered its
/// rectangle has to put the measured table back: registering replaces the
/// entry the original top selects, so the second scan selects the entry below
/// it and reads the wrong film. Before the fix the second pass came back 87
/// columns out with the frame's tail off the end.
///
/// The pass is the rectangle, so the two passes are the same film only if the
/// unit positioned the film the same way both times.
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
            Options::default(),
            &mut samples,
            |_, _| ControlFlow::Continue(()),
        )
        .expect("scan");
        let image = Image::new(&scanned.pass.layout, &samples).expect("image");
        profile(&image)
    };

    let first = round();
    let second = round();
    assert_eq!(
        first.len(),
        second.len(),
        "the passes are different lengths"
    );

    let moved = shift(&first, &second, 40);
    assert!(
        moved.abs() <= 4,
        "the film moved {moved} columns between the two passes"
    );
}
