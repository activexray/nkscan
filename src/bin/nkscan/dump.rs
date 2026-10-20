//! Dumps all the VPD pages

use crate::cli;
use anyhow::{anyhow, bail};
use nkscan::{
    device,
    protocol::{
        caps::{
            self, Page, address::Address, ccd::CcdMeasurement, frames::Frames, other::Features,
            set_window::SetWindowFunction,
        },
        cdbs::Inquiry,
    },
    session::probe,
    transport::Transport,
};

/// Dump every INQUIRY page the named unit carries
pub async fn run(args: cli::Dump) -> anyhow::Result<()> {
    let selector = match args.device {
        Some(d) => device::Selector::Location(d),
        None => device::Selector::Only,
    };
    let (device, mut transport) = match selector.open().await {
        Ok(opened) => opened,
        Err(e) => {
            let list: Vec<_> = device::list()
                .await
                .iter()
                .map(ToString::to_string)
                .collect();
            return Err(anyhow!("{e}\n\nattached:\n  {}", list.join("\n  ")));
        }
    };

    println!("{device}\n");

    // Standard INQUIRY: who is this, and is it even a scanner
    let Some(identity) = read(transport.as_mut(), Inquiry::standard()).await else {
        bail!("standard INQUIRY was refused");
    };
    println!("== standard INQUIRY ==");
    hexdump(&identity);

    let codes = probe::page_codes(transport.as_mut())
        .await
        .map_err(|e| anyhow!("page 00h gave nothing to enumerate: {e}"))?;
    println!(
        "\n  {} pages: {}",
        codes.len(),
        codes
            .iter()
            .map(|p| format!("{p:02X}h"))
            .collect::<Vec<_>>()
            .join(" ")
    );

    for code in codes {
        // A page the unit's own list left out may simply not be there
        let unlisted = match probe::UNLISTED.contains(&code) {
            true => " (unlisted)",
            false => "",
        };
        println!("\n== page {code:02X}h{unlisted} ==");
        match read(transport.as_mut(), Inquiry::vpd(code)).await {
            Some(bytes) => {
                page_hexdump(&bytes);
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
        Frames::PAGE_CODE => Some(show(Frames::try_from(&page))),
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
async fn read(transport: &mut dyn Transport, cmd: Inquiry) -> Option<Vec<u8>> {
    match probe::inquiry(transport, cmd).await {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            println!("  {e}");
            None
        }
    }
}

/// Dump a VPD page, stopping where the page says it ends
fn page_hexdump(bytes: &[u8]) {
    let declared = bytes
        .get(3)
        .map_or(bytes.len(), |&n| (4 + usize::from(n)).min(bytes.len()));
    hexdump(&bytes[..declared]);
    if declared < bytes.len() {
        println!("  ... {} bytes of residue after it", bytes.len() - declared);
    }
}

fn hexdump(bytes: &[u8]) {
    for (n, row) in bytes.chunks(16).enumerate() {
        let hex: Vec<_> = row.iter().map(|b| format!("{b:02X}")).collect();
        let text: String = row
            .iter()
            .map(|&b| match (0x20..0x7F).contains(&b) {
                true => b as char,
                false => '.',
            })
            .collect();
        println!("  {:04X}  {:<47}  {text}", n * 16, hex.join(" "));
    }
}
