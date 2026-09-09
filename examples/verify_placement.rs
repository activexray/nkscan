//! Scratch: does a pass land on the film the thumbnail said it would?
//!
//! Thumbnails the loaded strip, then scans the frames named on the command
//! line through the product path, writing the thumbnail and every pass. The
//! two are then comparable: each pass is a stretch of the same film the
//! thumbnail covers, so where it matches the thumbnail says where the unit
//! actually put the film, against where the frame table asked for it.
//!
//! ```text
//! cargo run --example verify_placement -- <out-dir> [frame-index]...
//! ```

use nkscan::{
    device,
    protocol::{
        caps::set_window::ColorInterleaving,
        decode::{Image, Samples},
    },
    scan::{
        boundaries::{self, Polarity},
        frame::{self, Options},
        framing,
        pass::Pass,
        window::Recipe,
    },
    session::Session,
};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let mut a = std::env::args().skip(1);
    let dir = a.next().ok_or("an output directory")?;
    let want: Vec<usize> = a.filter_map(|n| n.parse().ok()).collect();
    let dir = Path::new(&dir);
    std::fs::create_dir_all(dir)?;

    let devices = device::list();
    let device = devices.first().ok_or("no scanner found")?;
    let mut session = Session::open(device.open()?)?;
    session.stage()?;

    let mut samples = Samples::default();
    let discovery = framing::discover(&mut session, None, &mut samples)?;
    println!(
        "{} frames, tops {:?}",
        discovery.frames.len(),
        discovery.frames.iter().map(|f| f.top).collect::<Vec<_>>()
    );
    if let Some(pass) = &discovery.thumbnail {
        write(&dir.join("thumbnail"), pass, &samples)?;
        println!("thumbnail {} x {}", pass.rows, pass.cols);
    }

    let want = match want.is_empty() {
        true => (0..discovery.frames.len()).collect(),
        false => want,
    };
    for n in want {
        let Some(&frame) = discovery.frames.get(n) else {
            eprintln!("no frame {n}");
            continue;
        };
        let recipe = Recipe {
            dpi: 500,
            samples: 1,
            interleaving: ColorInterleaving::LINE_WITHOUT_DISTANCE,
            infrared: false,
        };
        let options = Options {
            exposures: None,
            lock_white_balance: false,
            clean: false,
        };
        let scanned = frame::scan_frame_with(
            &mut session,
            &recipe,
            frame,
            options,
            &mut samples,
            |_, _| std::ops::ControlFlow::Continue(()),
        )?;
        // The pass is the rectangle, so the picture should fill it: a run
        // that starts above 0 or ends before the last column is the unit
        // putting the film somewhere other than where the window asked
        let picture = Image::new(&scanned.pass.layout, &samples)
            .ok()
            .map(|image| boundaries::locate(&image, Polarity::Negative));
        println!(
            "frame {n} top {} pass {} x {} picture {:?}",
            frame.top, scanned.pass.rows, scanned.pass.cols, picture
        );
        write(&dir.join(format!("frame_{n}")), &scanned.pass, &samples)?;
    }
    Ok(())
}

fn write(stem: &Path, pass: &Pass, samples: &Samples) -> Result<(), Box<dyn std::error::Error>> {
    let image = Image::new(&pass.layout, samples)?;
    for (n, plane) in image.colors.iter().enumerate() {
        let path = format!("{}.{n}.tiff", stem.display());
        let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
        let mut tiff = tiff::encoder::TiffEncoder::new(&mut file)?;
        tiff.write_image::<tiff::encoder::colortype::Gray16>(
            image.cols as u32,
            image.rows as u32,
            &plane[..image.rows * image.cols],
        )?;
    }
    Ok(())
}
