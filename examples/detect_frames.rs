//! Run `scan::boundaries::detect` against a saved `_thumbnail.tiff` and draw
//! the result back onto a copy of it, for eyeballing frame detection outside
//! a live session.
//!
//! ```text
//! cargo run --example detect_frames -- <thumbnail.tiff> [--format f135] [--polarity negative] [-o out.tiff]
//! ```

use nkscan::{
    protocol::{caps::film::FilmFormat, decode::Image},
    scan::boundaries::{self, Polarity},
};
use std::{env, path::PathBuf};
use tiff::{
    decoder::{Decoder, DecodingResult, Limits, ifd::Value},
    encoder::{Rational, TiffEncoder, colortype::RGB16},
    tags::Tag,
};

/// `XResolution`/`YResolution` are always RATIONAL per the TIFF6 spec, never
/// FLOAT, so `get_tag_f32` rejects them outright (`InvalidTypeForTag`) rather
/// than reading them - which silently drops to `unwrap_or`'s fallback on
/// every real file instead of surfacing the error
fn resolution(decoder: &mut Decoder<impl std::io::Read + std::io::Seek>, tag: Tag) -> Option<f32> {
    match decoder.get_tag(tag).ok()? {
        Value::Rational(n, d) if d != 0 => Some(n as f32 / d as f32),
        Value::Float(v) => Some(v),
        _ => None,
    }
}

fn parse_format(s: &str) -> FilmFormat {
    match s.to_ascii_lowercase().as_str() {
        "ix240" | "aps" => FilmFormat::IX240,
        "f135" | "35mm" => FilmFormat::F135,
        "f135half" | "35mmhalf" => FilmFormat::F135Half,
        "f16" | "16mm" => FilmFormat::F16,
        "f645" | "6x45" => FilmFormat::F645,
        "f66" | "6x6" => FilmFormat::F66,
        "f67" | "6x7" => FilmFormat::F67,
        "f68" | "6x8" => FilmFormat::F68,
        "f69" | "6x9" => FilmFormat::F69,
        mm => FilmFormat::Custom(mm.parse().unwrap_or_else(|_| panic!("unknown format {mm}"))),
    }
}

fn parse_polarity(s: &str) -> Polarity {
    match s.to_ascii_lowercase().as_str() {
        "positive" | "pos" | "slide" => Polarity::Positive,
        "negative" | "neg" => Polarity::Negative,
        other => panic!("unknown polarity {other}"),
    }
}

/// The TIFF itself is chunky RGB; `Samples`/`Image` want planes apart
fn deinterleave3(chunky: &[u16]) -> Vec<Vec<u16>> {
    let mut planes: Vec<Vec<u16>> = (0..3)
        .map(|_| Vec::with_capacity(chunky.len() / 3))
        .collect();
    for pixel in chunky.chunks_exact(3) {
        for (plane, &v) in planes.iter_mut().zip(pixel) {
            plane.push(v);
        }
    }
    planes
}

fn main() {
    let mut args = env::args().skip(1);
    let input = PathBuf::from(
        args.next()
            .expect("usage: detect_frames <in.tiff> [--format F] [--polarity P] [-o out.tiff]"),
    );

    let mut format = FilmFormat::F135;
    let mut polarity = Polarity::Negative;
    let mut dpi_override: Option<f32> = None;
    let mut out = PathBuf::from("detected.tiff");

    let rest: Vec<String> = args.collect();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--format" => {
                format = parse_format(&rest[i + 1]);
                i += 2;
            }
            "--polarity" => {
                polarity = parse_polarity(&rest[i + 1]);
                i += 2;
            }
            "--dpi" => {
                dpi_override = Some(rest[i + 1].parse().unwrap());
                i += 2;
            }
            "-o" | "--out" => {
                out = PathBuf::from(&rest[i + 1]);
                i += 2;
            }
            other => panic!("unknown arg {other}"),
        }
    }

    let file = std::fs::File::open(&input).unwrap_or_else(|e| panic!("{}: {e}", input.display()));
    let mut decoder = Decoder::new(std::io::BufReader::new(file))
        .unwrap()
        .with_limits(Limits::unlimited());
    let (cols, rows) = decoder.dimensions().unwrap();
    let dpi = dpi_override.unwrap_or_else(|| {
        resolution(&mut decoder, Tag::XResolution).unwrap_or_else(|| {
            panic!(
                "{}: no readable XResolution tag, pass --dpi",
                input.display()
            )
        })
    });

    let chunky = match decoder.read_image().unwrap() {
        DecodingResult::U16(v) => v,
        other => panic!("not 16-bit samples ({other:?})"),
    };
    let planes = deinterleave3(&chunky);
    let colors: Vec<&[u16]> = planes.iter().map(Vec::as_slice).collect();

    let image = Image {
        colors,
        ir: &[],
        rows: rows as usize,
        cols: cols as usize,
        bits: 16,
    };

    let nominal = format.height_dots(dpi.round() as u16) as usize;
    let found = boundaries::detect(&image, nominal, polarity);
    let length = found.length;

    eprintln!(
        "{}: {cols}x{rows} @ {dpi:.1} dpi, format={format:?} -> nominal={nominal}px, corrected={length}px, polarity={polarity:?}",
        input.display()
    );
    eprintln!("pitch={}px, {} frame(s):", found.pitch, found.frames.len());
    for (n, &start) in found.frames.iter().enumerate() {
        let end = (start + length).min(cols as usize);
        eprintln!("  #{:<2} columns [{start}, {end})", n + 1);
    }

    // Draw a colored box (magenta) around each detected frame, plus a thin
    // outline color-coded by index so overlapping frames stay distinguishable.
    let palette: [[u16; 3]; 6] = [
        [65535, 0, 0],
        [0, 65535, 0],
        [0, 0, 65535],
        [65535, 65535, 0],
        [65535, 0, 65535],
        [0, 65535, 65535],
    ];
    let mut annotated = chunky.clone();
    let (w, h) = (cols as usize, rows as usize);
    let mut set = |x: usize, y: usize, color: [u16; 3]| {
        if x < w && y < h {
            let i = (y * w + x) * 3;
            annotated[i] = color[0];
            annotated[i + 1] = color[1];
            annotated[i + 2] = color[2];
        }
    };
    for (n, &start) in found.frames.iter().enumerate() {
        let color = palette[n % palette.len()];
        let end = (start + length).min(w.saturating_sub(1));
        for thick in 0..3 {
            for y in 0..h {
                set(start + thick, y, color);
                set(end.saturating_sub(thick), y, color);
            }
            for x in start..=end {
                set(x, thick.min(h.saturating_sub(1)), color);
                set(x, h.saturating_sub(1 + thick), color);
            }
        }
    }

    let file = std::fs::File::create(&out).unwrap();
    let mut tiff = TiffEncoder::new(file).unwrap();
    let mut img = tiff.new_image::<RGB16>(cols, rows).unwrap();
    img.resolution(
        tiff::tags::ResolutionUnit::Inch,
        Rational {
            n: dpi.round() as u32,
            d: 1,
        },
    );
    img.write_data(&annotated).unwrap();
    eprintln!("wrote {}", out.display());
}
