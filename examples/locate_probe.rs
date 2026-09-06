//! Scratch: run the frame finder on dumped pass planes, offline.
//!
//! ```text
//! cargo run --example locate_probe -- <pass.tiff> [<more-planes.tiff>...] <expected-length> <negative|positive>
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
    let mut expected = None;
    let mut positive = false;
    for arg in a {
        if arg.ends_with(".tiff") {
            paths.push(arg);
        } else if expected.is_none() {
            expected = Some(arg.parse()?);
        } else {
            positive = arg == "positive" || arg == "pos";
        }
    }
    let expected = expected.ok_or("expected length in columns is required after the tiff paths")?;
    run(&paths, expected, positive)
}

fn run(
    paths: &[String],
    expected: usize,
    positive: bool,
) -> Result<(), Box<dyn std::error::Error>> {
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
    let found = boundaries::locate(&image, expected, polarity);
    println!(
        "{:?} (expected {expected}, {} planes)",
        found,
        image.colors.len()
    );
    Ok(())
}
