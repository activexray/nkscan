//! REMOVE LATER: what the CCD's own response curves say
//!
//! `DataType::CcdData` is what each CCD row reads at the calibration levels
//! `CcdMeasurement` lists. Rows that disagree are the banding a three-line pass
//! has and a single-line one does not, so how far apart they sit is how much
//! there is to correct on this particular unit.
//!
//! ```text
//! cargo run --example ccd
//! ```
//!
//! Reads only. Nothing moves.

use nkscan::{
    device::{self, Selector},
    protocol::{
        curves::Curves,
        data::{DataType, Values},
    },
    session::Session,
};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let devices = device::list();
    let mut session = Session::open(Selector::Only.resolve(&devices)?.open()?)?;

    let ccd = session
        .capabilities()
        .ccd
        .clone()
        .ok_or_else(|| anyhow::anyhow!("this unit publishes no CCD measurement page"))?;
    let points = ccd.points.len();
    println!(
        "{:?}, {} scans, {} types, {points} points: {:?}",
        ccd.colors, ccd.scans, ccd.types, ccd.points
    );
    println!("{} curves of {points}\n", ccd.curves());

    let (_, values) = session.read_data(DataType::CcdData, 0)?;
    let Values::Words(words) = values else {
        anyhow::bail!("CcdData did not come back as words");
    };
    println!("{} samples\n", words.len());

    // What the correction would actually do here. Rows that already agree leave
    // it near the identity; a table built from the wrong slots would not be
    let rows = usize::from(session.capabilities().address.lines).max(1);
    match Curves::parse(&ccd, &words, rows, 0) {
        None => println!("the curves do not fit the page, so nothing would be corrected\n"),
        Some(built) => {
            println!("how far the correction moves a sample, per CCD row:");
            for row in 0..built.rows() {
                let (mut worst, mut at, mut sum) = (0u32, 0u32, 0u64);
                for v in 0..=u16::MAX {
                    let moved = u32::from(built.correct(row, v)).abs_diff(u32::from(v));
                    sum += u64::from(moved);
                    if moved > worst {
                        (worst, at) = (moved, u32::from(v));
                    }
                }
                println!(
                    "  row {row}: mean {:.1}, worst {worst} at sample {at}",
                    sum as f64 / f64::from(u32::from(u16::MAX) + 1)
                );
            }
            println!();
        }
    }

    let curves: Vec<&[u16]> = words.chunks_exact(points).collect();
    for (n, curve) in curves.iter().enumerate() {
        println!("{n:2}: {curve:?}");
    }

    // Whatever the 30 group into, curves that sit on top of each other have
    // nothing between them to correct
    println!(
        "\nspread at each point, across all {} curves:",
        curves.len()
    );
    for p in 0..points {
        let at: Vec<u16> = curves.iter().map(|c| c[p]).collect();
        let (lo, hi) = (
            *at.iter().min().unwrap_or(&0),
            *at.iter().max().unwrap_or(&0),
        );
        let mean = at.iter().map(|&v| u32::from(v)).sum::<u32>() / at.len().max(1) as u32;
        println!(
            "  point {p:2} level {:6}: {lo:6} to {hi:6}, spread {:5} ({:.2}% of the mean)",
            ccd.points.get(p).copied().unwrap_or(0),
            hi - lo,
            100.0 * f64::from(hi - lo) / f64::from(mean.max(1))
        );
    }
    Ok(())
}
