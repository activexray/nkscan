//! Scratch: run `scan::strip::find` against saved thumbnails and draw the fit.
//!
//! The format comes from the file name where it is in it, so the whole
//! `thumbnails/` corpus can be run in one loop. The count in the name is what
//! the fit is checked against, never an input to it.
//!
//! ```text
//! cargo run --example fit_strip -- <thumbnail.tiff>... [--format f135] [-o dir]
//! ```

use nkscan::{
    protocol::{caps::film::FilmFormat, decode::Image},
    scan::strip,
};
use std::{env, path::PathBuf};
use tiff::{
    decoder::{Decoder, DecodingResult, Limits, ifd::Value},
    encoder::{Rational, TiffEncoder, colortype::RGB16},
    tags::Tag,
};

fn resolution(decoder: &mut Decoder<impl std::io::Read + std::io::Seek>, tag: Tag) -> Option<f32> {
    match decoder.get_tag(tag).ok()? {
        Value::Rational(n, d) if d != 0 => Some(n as f32 / d as f32),
        Value::Float(v) => Some(v),
        _ => None,
    }
}

/// The corpus names a format the way a photographer does, `--format` the way
/// `FilmFormat` spells it, and `scripts/annotate.py` sends the latter
fn format_of(name: &str) -> FilmFormat {
    match name {
        n if n.contains("6x45") || n == "f645" => FilmFormat::F645,
        n if n.contains("6x6") || n.contains("holga") || n == "f66" => FilmFormat::F66,
        n if n.contains("6x7") || n == "f67" => FilmFormat::F67,
        n if n.contains("6x8") || n == "f68" => FilmFormat::F68,
        n if n.contains("6x9") || n == "f69" => FilmFormat::F69,
        "f135half" | "half" => FilmFormat::F135Half,
        "ix240" => FilmFormat::IX240,
        "f16" => FilmFormat::F16,
        _ => FilmFormat::F135,
    }
}

/// `..._6frames.tiff` says six. The last such word in the name, so
/// `..._mising_frames_4frames` is four
fn frames_of(name: &str) -> Option<usize> {
    let at = name.rfind("frames")?;
    let digits: String = name[..at]
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.chars().rev().collect::<String>().parse().ok()
}

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
    let mut inputs = Vec::new();
    let mut format_arg = None;
    let mut out_dir = PathBuf::from(".");

    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--format" => {
                format_arg = Some(format_of(&args[i + 1].to_ascii_lowercase()));
                i += 2;
            }
            "-o" | "--out" => {
                out_dir = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            path => {
                inputs.push(PathBuf::from(path));
                i += 1;
            }
        }
    }

    for input in &inputs {
        let name = input
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        // What the name says, to check the fit against
        let want = frames_of(&name);
        let format = format_arg.unwrap_or_else(|| format_of(&name));

        let file = std::fs::File::open(input).unwrap();
        let mut decoder = Decoder::new(std::io::BufReader::new(file))
            .unwrap()
            .with_limits(Limits::unlimited());
        let (cols, rows) = decoder.dimensions().unwrap();
        let dpi = resolution(&mut decoder, Tag::XResolution).unwrap_or(97.0);
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
        let started = std::time::Instant::now();
        let Some(found) = strip::find(&image, nominal) else {
            eprintln!("{name}: no fit");
            continue;
        };
        let took = started.elapsed();

        let verdict = match want {
            Some(n) if n != found.frames.len() => " MISCOUNT",
            _ => "",
        };
        println!(
            "{name}: {cols}x{rows} @{dpi:.0}dpi {format:?} want {want:?} nominal {nominal}\n  \
             {} frames{verdict} pitch {} picture {} contrast {:.3} in {}ms",
            found.frames.len(),
            found.pitch,
            found.frames[0].len(),
            found.contrast,
            took.as_millis()
        );
        let places: Vec<String> = found
            .frames
            .iter()
            .map(|f| format!("{}..{}", f.start, f.end))
            .collect();
        println!("  {}", places.join(" "));

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
        // The picture that was found, in the frame's own color, and the
        // rectangle a scan of it would take, in white dashes
        for (n, frame) in found.frames.iter().enumerate() {
            let color = palette[n % palette.len()];
            for thick in 0..2 {
                for y in 0..h {
                    set(frame.start + thick, y, color);
                    set(frame.end.saturating_sub(1 + thick), y, color);
                }
                for x in frame.clone() {
                    set(x, thick, color);
                    set(x, h.saturating_sub(1 + thick), color);
                }
            }
        }
        // The rectangle a scan takes is the format over the middle of the
        // picture, as `thumbnail::frames` puts it
        for frame in &found.frames {
            let middle = frame.start + frame.len() / 2;
            let start = middle
                .saturating_sub(nominal / 2)
                .min(w.saturating_sub(nominal));
            for y in (0..h).filter(|y| y % 8 < 4) {
                set(start, y, [65535, 65535, 65535]);
                set(
                    (start + nominal).min(w).saturating_sub(1),
                    y,
                    [65535, 65535, 65535],
                );
            }
        }

        let out = out_dir.join(format!("{name}_fit.tiff"));
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
    }
}
