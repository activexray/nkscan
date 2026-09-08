//! Scratch: acceptance test for frame placement on a perforation-framed unit.
//!
//! Discovers the frames, scans one through the product path, writes the whole
//! pass and reports which lines of it the scan decided are the frame, so the
//! decision can be checked against the film in the pass.
//!
//! ```text
//! cargo run --example verify_frame -- <frame-index> <dpi> <out-pass.tiff>
//! ```

use nkscan::{
    device,
    protocol::{caps::set_window::ColorInterleaving, decode::Samples},
    scan::{
        boundaries::Polarity,
        frame::{self, Options},
        framing,
        pass::Pass,
        window::Recipe,
    },
    session::Session,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let mut a = std::env::args().skip(1);
    let n: usize = a.next().ok_or("frame index, from 0")?.parse()?;
    let dpi: u16 = a.next().unwrap_or("500".into()).parse()?;
    let out = a.next().unwrap_or("verify.tiff".into());
    let positive = a.next().is_some_and(|p| p == "positive");

    let devices = device::list();
    let device = devices.first().ok_or("no scanner found")?;
    let mut session = Session::open(device.open()?)?;
    session.stage()?;

    let polarity = match positive {
        true => Polarity::Positive,
        false => Polarity::Negative,
    };

    let mut samples = Samples::default();
    let discovery = framing::discover(&mut session, None, &mut samples)?;
    println!(
        "detected {} frames: {:?}",
        discovery.frames.len(),
        discovery.frames.iter().map(|f| f.top).collect::<Vec<_>>()
    );
    let frame = discovery.frames[n];

    let recipe = Recipe {
        dpi,
        samples: 1,
        interleaving: ColorInterleaving::LINE_WITHOUT_DISTANCE,
        infrared: false,
    };

    let options = Options {
        exposures: None,
        lock_white_balance: false,
        clean: false,
        polarity: Some(polarity),
    };
    let scanned = frame::scan_frame_with(
        &mut session,
        &recipe,
        frame,
        options,
        &mut samples,
        |_, _| std::ops::ControlFlow::Continue(()),
    )?;

    println!(
        "exposures {:?}",
        scanned.exposures.iter().collect::<Vec<_>>()
    );
    write_pass(&out, &scanned.pass, &samples)?;
    println!(
        "pass {} rows x {} cols, frame_lines {:?} ({} lines)",
        scanned.pass.rows,
        scanned.pass.cols,
        scanned.frame_lines,
        scanned.frame_lines.len()
    );
    Ok(())
}

fn write_pass(
    path: &str,
    pass: &Pass,
    samples: &Samples,
) -> Result<(), Box<dyn std::error::Error>> {
    use nkscan::protocol::decode::Image;
    let image = Image::new(&pass.layout, samples)?;
    for (n, plane) in image.colors.iter().enumerate() {
        let mut file = std::io::BufWriter::new(std::fs::File::create(format!("{path}.{n}.tiff"))?);
        let mut tiff = tiff::encoder::TiffEncoder::new(&mut file)?;
        tiff.write_image::<tiff::encoder::colortype::Gray16>(
            image.cols as u32,
            image.rows as u32,
            &plane[..image.rows * image.cols],
        )?;
    }
    Ok(())
}
