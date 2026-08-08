//! Dump a scanner's INQUIRY pages
//!
//! ```text
//! cargo run --example dump                   # the only scanner attached
//! cargo run --example dump -- usb:1-4        # a particular one
//! cargo run --example dump -- /dev/sg0
//! ```
//!
//! Reads only. INQUIRY does not touch the mechanism, so nothing moves.

use anyhow::{anyhow, bail};
use clap::Parser;
use nkscan::{
    device::{self, Selector},
    protocol::{
        caps::{
            self, Page, address::Address, ccd::CcdMeasurement, other::Features,
            set_window::SetWindowFunction,
        },
        cdbs::Inquiry,
    },
    session,
    transport::Transport,
};
#[derive(Parser)]
#[command(about = "Dump a scanner's INQUIRY pages. Reads only; nothing moves.")]
struct Args {
    /// Which scanner, as `--list` prints it. Omit when only one is attached
    device: Option<String>,

    /// Show what is attached and stop
    #[arg(short, long)]
    list: bool,
}

/// Documented in LS-9000 2-2-2-7 but missing from its own page 00h list, so it
/// is worth asking for even when the unit does not admit to it
const UNLISTED: &[u8] = &[0xE3];

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let devices = device::list();
    if devices.is_empty() {
        bail!("no scanners found");
    }

    if args.list {
        for device in &devices {
            println!("{device}");
        }
        return Ok(());
    }

    let asked = args.device.unwrap_or_default();
    let selector: Selector = asked.parse()?;
    let device = selector.resolve(&devices).map_err(|e| {
        let list: Vec<_> = devices.iter().map(ToString::to_string).collect();
        let asked = if asked.is_empty() {
            String::new()
        } else {
            format!(" for {asked:?}")
        };
        anyhow!("{e}{asked}\n\nattached:\n  {}", list.join("\n  "))
    })?;

    println!("{device}\n");
    let mut transport = device.open()?;

    // Standard INQUIRY: who is this, and is it even a scanner
    let Some(std_data) = probe(transport.as_mut(), Inquiry::standard()) else {
        bail!("standard INQUIRY was refused");
    };
    println!("== standard INQUIRY ==");
    hexdump(&std_data);

    // Page 00h lists the pages the unit admits to
    let Some(list) = probe(transport.as_mut(), Inquiry::vpd(0x00)) else {
        bail!("page 00h was refused, so there is nothing to enumerate");
    };
    // Byte 3 is the page length. The unit pads the rest of whatever allocation
    // we asked for, so taking everything after byte 4 picks up the padding too
    let length = usize::from(*list.get(3).unwrap_or(&0));
    let listed: Vec<u8> = list.get(4..4 + length).unwrap_or_default().to_vec();
    println!("\n== page 00h ==");
    hexdump(&list);
    println!(
        "\n  lists {} pages: {}",
        listed.len(),
        listed
            .iter()
            .map(|p| format!("{p:02X}h"))
            .collect::<Vec<_>>()
            .join(" ")
    );

    // Our unit lists the CcdMeasurement page even though its spec's table
    // 2-2-1-2 does not, but ask
    // anyway for one that follows the document
    let unlisted: Vec<u8> = UNLISTED
        .iter()
        .copied()
        .filter(|code| !listed.contains(code))
        .collect();

    for &code in listed.iter().chain(&unlisted) {
        if code == 0x00 {
            continue;
        }
        let unlisted = if listed.contains(&code) {
            ""
        } else {
            " (unlisted)"
        };
        println!("\n== page {code:02X}h{unlisted} ==");
        match probe(transport.as_mut(), Inquiry::vpd(code)) {
            Some(bytes) => {
                hexdump(&bytes);
                // Everything a parser prints can be diffed against the
                // bracketed values in that page's section of the spec
                if let Some(decoded) = decode(code, &bytes) {
                    println!("\n{decoded}");
                }
            }
            None => println!("  refused"),
        }
    }

    Ok(())
}

/// Pretty-print the pages we have a parser for. `None` means no parser yet
fn decode(code: u8, bytes: &[u8]) -> Option<String> {
    fn show<T: std::fmt::Debug>(parsed: Result<T, caps::Error>) -> String {
        match parsed {
            Ok(v) => format!("  {v:#?}"),
            Err(e) => format!("  did not parse: {e}"),
        }
    }

    let page = match Page::new(code, bytes.to_vec()) {
        Ok(page) => page,
        Err(e) => return Some(format!("  did not parse: {e}")),
    };
    match code {
        Address::PAGE_CODE => Some(show(Address::try_from(&page))),
        Features::PAGE_CODE => Some(show(Features::try_from(&page))),
        SetWindowFunction::PAGE_CODE => Some(show(SetWindowFunction::try_from(&page))),
        CcdMeasurement::PAGE_CODE => Some(show(CcdMeasurement::try_from(&page))),
        _ => None,
    }
}

/// Ask for a page, treating a refusal as "this unit does not have it"
///
/// 2-2 note 4: a CHECK CONDITION to INQUIRY means the unit cannot produce what
/// was asked for, which when probing arbitrary page codes is the expected
/// answer rather than a failure
fn probe(transport: &mut dyn Transport, cmd: Inquiry) -> Option<Vec<u8>> {
    match session::probe::inquiry(transport, cmd) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            println!("  {e}");
            None
        }
    }
}

fn hexdump(bytes: &[u8]) {
    for (n, row) in bytes.chunks(16).enumerate() {
        let hex: Vec<_> = row.iter().map(|b| format!("{b:02X}")).collect();
        let text: String = row
            .iter()
            .map(|&b| {
                if (0x20..0x7F).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!("  {:04X}  {:<47}  {text}", n * 16, hex.join(" "));
    }
}
