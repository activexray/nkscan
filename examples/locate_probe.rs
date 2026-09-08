//! Scratch: run the frame finder on dumped pass planes, offline.
//!
//! ```text
//! cargo run --example locate_probe -- <pass.tiff> [<more-planes.tiff>...] <negative|positive>
//! ```

use nkscan::{
    protocol::{
        decode::{Image, Samples},
        image::Layout,
    },
    scan::boundaries::{self, Polarity},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a = std::env::args().skip(1);
    let mut paths = Vec::new();

    let mut positive = false;
    for arg in a {
        if arg.ends_with(".tiff") {
            paths.push(arg);
        } else {
            positive = arg == "positive" || arg == "pos";
        }
    }
    run(&paths, positive)
}

fn run(paths: &[String], positive: bool) -> Result<(), Box<dyn std::error::Error>> {
    let polarity = match positive {
        true => Polarity::Positive,
        false => Polarity::Negative,
    };

    let mut colors = Vec::new();
    let mut rows = 0usize;
    let mut cols = 0usize;
    for path in paths {
        let mut dec = tiff::decoder::Decoder::new(std::fs::File::open(path)?)?;
        let (w, h) = dec.dimensions().map_err(|e| format!("{e}"))?;
        let plane = match dec.read_image()? {
            tiff::decoder::DecodingResult::U16(v) => v,
            _ => return Err("not 16-bit".into()),
        };
        // The dump is one sensor-major plane: rows of the sensor, columns the feed
        rows = h as usize;
        cols = w as usize;
        println!("{path}: {rows} rows x {cols} cols",);
        colors.push(plane);
    }

    let layout = Layout::single_line(rows as u32, cols as u32, (1..=colors.len() as u8).collect());
    let samples = Samples { colors, ir: None };
    let image = Image::new(&layout, &samples)?;
    let found = boundaries::locate(&image, polarity);
    println!(
        "{:?} ({} of {cols} columns, {} planes)",
        found,
        found.len(),
        image.colors.len()
    );
    Ok(())
}
