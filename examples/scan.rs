//! REMOVE LATER: measure a strip and scan every frame on it
//!
//! Takes the whole-strip thumbnail, works out where the frames are and tells the
//! unit (2-11-6), then focuses, meters and scans each of them in turn at full
//! resolution off the three-line CCD. Every pass is written as 16-bit Netpbm:
//! color planes go together and anything else, infrared being the one that turns
//! up, gets a file of its own.
//!
//! ```text
//! cargo run --release --example scan                  # measure, then scan every frame
//! cargo run --release --example scan -- thumb         # measure the strip and stop
//! cargo run --release --example scan -- keep          # scan against the table already held
//! cargo run --release --example scan -- frame=2       # only the second frame
//! cargo run --release --example scan -- dpi=666       # quicker than full resolution
//! cargo run --release --example scan -- len=6696      # 6x4.5 frames rather than 6x6
//! cargo run --release --example scan -- mono          # one channel rather than color
//! cargo run --release --example scan -- ir            # add the infrared channel
//! cargo run --release --example scan -- singleline    # one line at a time, not the three-line CCD
//! cargo run --release --example scan -- samples=2     # read each line twice and average
//! cargo run --release --example scan -- lockwb        # keep the channels in proportion
//! cargo run --release --example scan -- noae nofocus  # skip metering and focus
//! cargo run --release --example scan -- diagnose      # read a pending fault and stop
//! cargo run --release --example scan -- eject         # give the film back and stop
//! ```
//!
//! This moves the stage. `len` is the film format, which nothing advertises, and
//! is both what the frames are measured against and how tall a window gets.

use std::{
    fs::File,
    io::Write,
    time::{Duration, Instant},
};

use nkscan::{
    device::{self, Selector},
    protocol::{
        caps::{
            Capabilities,
            address::Axis,
            set_window::{ColorInterleaving, ScanKind, ScanMode},
        },
        data::{Boundary, Rect},
        window::{Channel, Composition, Flags, Window},
    },
    scan::{
        expose::{self, Exposure},
        focus::Focus,
        framing::{self, Framing},
        pass::{self, Pass},
        thumbnail,
    },
    session::Session,
};

/// A full-resolution 6x6 frame is half a gigabyte read a chunk at a time,
/// behind minutes of stage travel
const SCAN_TIMEOUT: Duration = Duration::from_secs(1800);

/// Where the whole-strip pass goes
const THUMB: &str = "thumb";

/// What the flags asked for, so scanning a frame takes one argument
struct Settings {
    /// SCAN order, which is also the order the stream interleaves them
    ids: Vec<u8>,
    dpi: u16,
    /// Frame extent along the feed, and so how tall a window is
    format: u32,
    /// Times each line is read for us to average
    readings: u8,
    interleaving: ColorInterleaving,
    focus: bool,
    meter: bool,
    lock_white_balance: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let arg = |name: &str| {
        args.iter()
            .find_map(|a| a.strip_prefix(&format!("{name}="))?.parse::<u32>().ok())
    };

    let devices = device::list();
    let device = Selector::Only.resolve(&devices)?;
    let mut session = Session::open(device.open()?)?;

    // 2-15-3: give the film back and stop
    if has("eject") {
        let started = Instant::now();
        session.eject()?;
        println!("ejected in {:?}", started.elapsed());
        return Ok(());
    }

    // 2-8: whatever a failed operation left behind, read once and gone
    if has("diagnose") {
        match session.diagnose()? {
            Some(sense) => println!("pending fault: {sense:?}"),
            None => println!("the unit reports no pending fault"),
        }
        return Ok(());
    }

    // The captures lead with infrared, and a set has to be given in SCAN order
    let mut ids: Vec<u8> = if has("mono") { vec![1] } else { vec![1, 2, 3] };
    if has("ir") {
        ids.insert(0, Channel::Infrared.id());
    }
    let settings = Settings {
        ids,
        // Full resolution unless something quicker was asked for. Off the ladder
        // the unit rounds and says so with 01h-37h rather than refusing
        dpi: arg("dpi").unwrap_or(u32::from(session.capabilities().address.x_axis.optical_dpi))
            as u16,
        format: arg("len").unwrap_or(8964),
        readings: arg("samples").unwrap_or(1).max(1) as u8,
        // The three-line CCD reads its rows at once, so the stage travels a
        // third as far. One line at a time is the "super fine" path, and the
        // only one the CCD correction has nothing to do in
        interleaving: match has("singleline") {
            true => ColorInterleaving::LINE_WITHOUT_DISTANCE,
            false => ColorInterleaving::MULTILINE_SIMULTANEOUS,
        },
        focus: !has("nofocus"),
        meter: !has("noae"),
        lock_white_balance: has("lockwb"),
    };

    // The setup the captures run once before the first pass. The frame table is
    // not sent here: the strip path measures it after thumbnailing, and `keep`
    // leaves the unit's existing table alone
    let started = Instant::now();
    println!("session open in {:?}", started.elapsed());

    // Where the frames are. The captures open with the whole-strip pass before
    // any frame placement, so the stage does not go out to a frame and back
    let table = match has("keep") {
        true => session.boundaries()?,
        false => measure(&mut session, settings.format)?,
    };
    println!("{} frame(s) on the strip", table.frames.len());
    for (n, frame) in table.frames.iter().enumerate() {
        println!("  frame {}: y {} to {}", n + 1, frame.top, frame.bottom);
    }
    if has("thumb") {
        return Ok(());
    }

    // One buffer for the strip: a full-resolution frame is half a gigabyte,
    // and there is no reason to hold two
    let mut samples = Vec::new();
    let only = arg("frame");

    for (n, frame) in table.frames.iter().enumerate() {
        let number = n as u32 + 1;
        if only.is_some_and(|pick| pick != number) {
            continue;
        }

        let started = Instant::now();
        let taken = scan(&mut session, frame, &settings, &mut samples)?;
        println!(
            "frame {number}: {} x {} at {} dpi, complete={}, owes {:?}, in {:?}",
            taken.cols,
            taken.rows,
            taken.layout.dpi,
            taken.complete,
            taken.cooperation,
            started.elapsed()
        );
        netpbm(&format!("frame{number}"), &samples, &taken)?;
    }
    Ok(())
}

/// Take the whole-strip pass and tell the unit what it says, 2-11-6
///
/// Writes the thumbnail, and a second copy with a bar on every edge the table
/// claims, so the measurement can be looked at against the picture it came from
fn measure(session: &mut Session, format: u32) -> anyhow::Result<Boundary> {
    println!("framing {:?}", Framing::choose(session.capabilities()));
    let started = Instant::now();
    let mut samples = Vec::new();
    let taken = session.scan_thumbnail(&mut samples)?;
    println!(
        "thumbnail {} x {} in {:?}, complete={}",
        taken.cols,
        taken.rows,
        started.elapsed(),
        taken.complete
    );
    netpbm(THUMB, &samples, &taken)?;

    let measured = thumbnail::frames(session.capabilities(), &taken, &samples, format, None)?;
    session.set_boundaries(&measured)?;

    let pitch = taken.layout.line_pitch.max(1);
    let origin = session.capabilities().address.y_axis.address_range.start;
    let columns: Vec<usize> = measured
        .frames
        .iter()
        .flat_map(|frame| [frame.top, frame.bottom])
        .map(|y| (y.saturating_sub(origin) / pitch) as usize)
        .collect();
    mark(&mut samples, &taken, &columns);
    netpbm(&format!("{THUMB}.frames"), &samples, &taken)?;
    Ok(measured)
}

/// Focus on one frame, meter it and scan it
fn scan(
    session: &mut Session,
    frame: &Rect,
    settings: &Settings,
    samples: &mut Vec<u16>,
) -> anyhow::Result<Pass> {
    let mut windows = descriptors(session, frame, settings)?;

    // Focus before metering, which is the order in the captures, so the
    // exposures are measured off a focused frame
    if settings.focus {
        let started = Instant::now();
        let focused = session.focus_with(Focus::default(), &windows)?;
        println!("focus: {focused:?} in {:?}", started.elapsed());
    }

    if settings.meter {
        let exposure = Exposure::choose(session.capabilities(), settings.lock_white_balance)?;
        let started = Instant::now();
        windows = session.expose(&windows, exposure)?;
        let held: Vec<_> = windows.iter().map(|w| (w.id, w.exposure)).collect();
        println!("metered {held:?} in {:?}", started.elapsed());
    }

    Ok(session.scan_pass(&windows, SCAN_TIMEOUT, samples)?)
}

/// The descriptors that scan one frame, from the ones the unit already holds
///
/// The unit keeps whatever the last run left in these, so everything a set has
/// to agree on gets said rather than inherited
fn descriptors(
    session: &mut Session,
    frame: &Rect,
    settings: &Settings,
) -> anyhow::Result<Vec<Window>> {
    let (origin, size) = area(session.capabilities(), frame, settings.format);
    let held = session.windows()?;
    // 2-10-6 has one code for a one-plane output and one for three, and counts
    // the visible planes, so infrared does not sway it
    let composition = match settings
        .ids
        .iter()
        .filter(|id| Channel::from(**id).is_color())
        .count()
    {
        1 => Composition::MultilevelBW,
        _ => Composition::MultilevelRGB,
    };

    settings
        .ids
        .iter()
        .map(|id| {
            let mut w = held
                .iter()
                .find(|w| w.id == *id)
                .ok_or_else(|| anyhow::anyhow!("this unit holds no window {id}"))?
                .clone();
            // A scan is square; the metering pass halves Y for itself
            w.resolution = (settings.dpi, settings.dpi);
            w.origin = origin;
            w.size = size;
            w.scanning_kind = ScanKind::IMAGE;
            w.scanning_mode = ScanMode::HIGH_SPEED;
            w.flags = Flags::POSITIVE;
            w.composition = composition;
            w.color_interleaving = settings.interleaving;
            // Byte 40 carries one less than the reading count, and byte 43 has
            // to say so too
            w.multiple_reading = settings.readings - 1;
            if w.multiple_reading != 0 {
                w.scanning_mode |= ScanMode::MULTI_READING;
            }
            Ok(w)
        })
        .collect()
}

/// The window that scans one frame
///
/// The unit serves film from where the table says a frame starts, so the frame's
/// own front edge is the origin and the stage goes there and stays. Height is the
/// film format rather than the rectangle's: a unit that has measured nothing
/// answers `DataType::Boundary` with one rectangle covering the whole sensor, and
/// a window that long would take the stage past its boundary.
fn area(caps: &Capabilities, frame: &Rect, format: u32) -> ((u32, u32), (u32, u32)) {
    let (x, y) = (&caps.address.x_axis, &caps.address.y_axis);
    let clamp = |v: u32, axis: &Axis| v.clamp(axis.address_range.start, axis.address_range.last);
    (
        (clamp(frame.left, x), clamp(frame.top, y)),
        (
            frame.right.saturating_sub(frame.left).min(x.boundary),
            format.min(y.boundary),
        ),
    )
}

/// Paint the named columns solid, so a frame table can be looked at against
/// the thumbnail it was measured from
fn mark(samples: &mut [u16], pass: &Pass, columns: &[usize]) {
    let channels = pass.layout.channels.len();
    for &x in columns {
        if x >= pass.cols {
            continue;
        }
        for y in 0..pass.rows {
            let at = (y * pass.cols + x) * channels;
            samples[at..at + channels].fill(u16::MAX);
        }
    }
}

/// Write decoded samples where they can be looked at, 16-bit Netpbm
fn netpbm(stem: &str, samples: &[u16], pass: &Pass) -> anyhow::Result<()> {
    let ids = &pass.layout.channels;
    let color: Vec<usize> = (0..ids.len())
        .filter(|&c| Channel::from(ids[c]).is_color())
        .collect();
    plane(stem, samples, pass, &color)?;

    // Infrared is not a color and has no place in an RGB file
    for (c, id) in ids.iter().enumerate() {
        if !Channel::from(*id).is_color() {
            let name = format!(
                "{stem}.{}",
                format!("{:?}", Channel::from(*id)).to_lowercase()
            );
            plane(&name, samples, pass, &[c])?;
        }
    }
    Ok(())
}

/// One file holding the named channels, written a row at a time so the image is
/// never copied whole
fn plane(stem: &str, samples: &[u16], pass: &Pass, channels: &[usize]) -> anyhow::Result<()> {
    let (magic, ext) = match channels.len() {
        1 => ("P5", "pgm"),
        3 => ("P6", "ppm"),
        n => anyhow::bail!("{n} channels has no Netpbm form"),
    };
    let (rows, cols, stride) = (pass.rows, pass.cols, pass.layout.channels.len());
    let dest = format!("{stem}.{ext}");
    let mut file = std::io::BufWriter::new(File::create(&dest)?);
    write!(file, "{magic}\n{cols} {rows}\n65535\n")?;

    let mut row = Vec::with_capacity(cols * channels.len() * 2);
    for y in 0..rows {
        row.clear();
        for x in 0..cols {
            for &c in channels {
                row.extend_from_slice(&samples[(y * cols + x) * stride + c].to_be_bytes());
            }
        }
        file.write_all(&row)?;
    }
    file.flush()?;
    println!("wrote {dest}");
    Ok(())
}
