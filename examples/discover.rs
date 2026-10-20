//! Scratch: find the frames on a perforation unit's thumbnail and save a framing fixture.
//!
//! Does not write the frame table. Saves `thumbnail.tiff`, `perforation.bin`, `boundary2.bin` and `meta.txt`.
//!
//! ```text
//! cargo run --features cli --example discover -- <film> <out dir>
//! ```

use nkscan::{
    device,
    scan::{boundary::perforation, profile::Film},
    session::Session,
};
use std::{env, fs, path::PathBuf};
use tiff::encoder::{TiffEncoder, colortype::RGB16};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    pollster::block_on(async {
        tracing_subscriber::fmt()
            .with_env_filter("nkscan=debug")
            .with_writer(std::io::stderr)
            .init();
        let args: Vec<String> = env::args().skip(1).collect();
        let film: Film = args
            .first()
            .map(String::as_str)
            .unwrap_or("negative")
            .parse()?;
        let out = PathBuf::from(args.get(1).map(String::as_str).unwrap_or("discover"));
        fs::create_dir_all(&out)?;

        let devices = device::list().await;
        let device = devices.first().ok_or("no scanner")?;
        let mut session = Session::open(device.open().await?).await?;
        session.stage().await?;
        let caps = session.capabilities();
        println!(
            "adapter {:?}, holds {} frames, Y {:?}, optical {}",
            caps.adapter,
            caps.address.loaded_frames,
            caps.address.y_axis.address_range,
            caps.address.y_axis.optical_dpi
        );
        let optical = caps.address.y_axis.optical_dpi;
        let held = usize::from(caps.address.loaded_frames);

        // What discover does before the pass
        let _ = session.read_perforations().await?;
        let _ = session.read_boundaries_type2().await;
        let mut samples = Default::default();
        let pass = session.scan_thumbnail(&mut samples, film).await?;
        let perfs = session.read_perforations().await?;
        let image = pass.image(&samples)?;
        println!(
            "thumbnail {}x{} (stride {}), {} bits, line pitch {}, {} records",
            image.rows,
            image.cols,
            image.stride,
            image.bits,
            pass.layout.line_pitch,
            perfs.perfs.len()
        );

        let line_pitch = pass.layout.line_pitch;
        let starts = perforation::frames(&image, &perfs, optical, line_pitch, film, held);
        let kept = perforation::resample(&perfs, optical, line_pitch);
        println!("{} lines kept, frames start at {starts:?}", kept.len());
        let table = perforation::table(
            &perfs,
            optical,
            &kept,
            starts.iter().flatten().map(|&s| s as i64),
        );
        for f in &table.frames {
            println!(
                "  top {:6} perf {:3} decimal {:#04x} pulse {:3}",
                f.top, f.perf_number, f.perf_decimal, f.pulse_number
            );
        }

        // The raw thumbnail, rows by the full stride, as the fixtures hold it
        let (rows, cols) = (image.rows, image.stride);
        let chunky: Vec<u16> = (0..rows * cols)
            .flat_map(|i| (0..3).map(move |c| (c, i)))
            .map(|(c, i)| samples.colors[c][i])
            .collect();
        let mut tiff = TiffEncoder::new(fs::File::create(out.join("thumbnail.tiff"))?)?;
        tiff.write_image::<RGB16>(cols as u32, rows as u32, &chunky)?;

        // The 8Eh record as the unit sends it: 3-byte length, 4 bytes a record
        let mut record = Vec::with_capacity(4 + 4 * perfs.perfs.len());
        let length = (4 * perfs.perfs.len() + 1) as u32;
        record.extend_from_slice(&length.to_be_bytes()[1..]);
        record.push(4);
        for p in &perfs.perfs {
            record.extend_from_slice(&p.perf_number.to_be_bytes());
            record.push(p.perf_decimal | (u8::from(p.count_switching_flag) << 7));
            record.push(p.pulse_number);
        }
        fs::write(out.join("perforation.bin"), record)?;
        if !table.frames.is_empty() {
            fs::write(out.join("boundary2.bin"), table.to_bytes()?)?;
        }
        fs::write(
            out.join("meta.txt"),
            format!(
                "bits {}\nfilm {}\nframes {held}\nline_pitch {line_pitch}\n",
                image.bits,
                match film {
                    Film::Negative => "negative",
                    Film::MonochromeNegative => "mono",
                    _ => "positive",
                }
            ),
        )?;
        println!("saved to {}", out.display());
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}
