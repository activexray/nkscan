//! Scratch: measure where the unit actually puts the film for a given
//! perforation record.
//!
//! Writes a Boundary Type2 table whose aimed-at entry carries the record read
//! at thumbnail line L, scans a short window at that entry's top, and reports
//! the column profile so the film landmark in it can be located.
//!
//! ```text
//! cargo run --example place_probe -- <top-line> <record-line> <height-cols> <entries> <out.tiff>
//! ```

use nkscan::{
    device,
    protocol::{
        caps::set_window::ColorInterleaving,
        data::{BoundaryType2, FramePosition, Rect},
        decode::{Image, Samples},
    },
    scan::window::Recipe,
    session::Session,
};
use std::time::Duration;

const PITCH: u32 = 41;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let top_line: usize = a.next().ok_or("top line")?.parse()?;
    let record_line: usize = a.next().ok_or("record line")?.parse()?;
    let height: u32 = a.next().unwrap_or("60".into()).parse()?;
    let entries: usize = a.next().unwrap_or("6".into()).parse()?;
    let out = a.next().unwrap_or("place.tiff".into());
    let dpi: u16 = a.next().unwrap_or("500".into()).parse()?;

    let devices = device::list();
    let device = devices.first().ok_or("no scanner found")?;
    let mut session = Session::open(device.open()?)?;
    session.stage()?;

    let perfs = session.read_perforations()?;
    println!("perf records {}", perfs.perfs.len());
    let rec = |line: usize| -> Result<FramePosition, String> {
        let p = perfs.at(line).ok_or(format!("no record at {line}"))?;
        Ok(FramePosition::new(line as u32 * PITCH, p))
    };

    // The aimed-at entry last, with earlier real entries under it so the table
    // is ordered and every slot points somewhere the stage can go
    let mut table = BoundaryType2::default();
    let spacing = top_line / entries.max(1);
    for k in 0..entries.saturating_sub(1) {
        table.frames.push(rec(k * spacing)?);
    }
    let mut aim = rec(record_line)?;
    aim.top = top_line as u32 * PITCH;
    table.frames.push(aim);
    println!("table {} entries, aim {:?}", table.frames.len(), aim);
    session.set_boundaries_type2(&table)?;

    let top = top_line as u32 * PITCH;
    let x = session.capabilities().address.x_axis.clone();
    let frame = Rect {
        top,
        left: x.address_range.start,
        bottom: top + height * PITCH,
        right: x.address_range.start + x.boundary,
    };
    let recipe = Recipe {
        dpi,
        samples: 1,
        interleaving: ColorInterleaving::LINE_WITHOUT_DISTANCE,
        infrared: false,
    };
    let windows = recipe.windows(session.capabilities(), frame)?;
    println!(
        "window origin {:?} size {:?}",
        windows[0].origin, windows[0].size
    );

    let mut samples = Samples::default();
    let pass = session.scan_pass(&windows, Duration::from_secs(600), &mut samples)?;
    samples.to_full_scale(pass.layout.bits_per_sample);
    println!(
        "pass rows {} cols {} line_pitch {} dpi {}",
        pass.rows, pass.cols, pass.layout.line_pitch, pass.layout.dpi
    );

    let image = Image::new(&pass.layout, &samples)?;
    let plane = image
        .colors
        .get(1)
        .or(image.colors.first())
        .ok_or("no plane")?;
    let mut file = std::io::BufWriter::new(std::fs::File::create(&out)?);
    let mut tiff = tiff::encoder::TiffEncoder::new(&mut file)?;
    tiff.write_image::<tiff::encoder::colortype::Gray16>(
        image.cols as u32,
        image.rows as u32,
        &plane[..image.rows * image.cols],
    )?;
    println!("wrote {out}");
    Ok(())
}
