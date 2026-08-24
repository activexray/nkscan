//! Dumps scanner RAM through the READ BUFFER backdoor
//!
//! Needs a firmware whose FW:0x4A114 buffer table covers the target address.
//! The coolscan-mods build (READ BUFFER patches) maps buffer 0 to base 0,
//! size 0xFFFFFF, which makes offsets absolute across the first 16 MB

use crate::cli;
use anyhow::{anyhow, bail};
use nkscan::{
    device,
    protocol::{
        cdbs::ReadBuffer,
        sense::{Outcome, interpret},
    },
    transport::Data,
};
use std::time::Duration;

/// Per-command timeout; RAM reads answer immediately when permitted
const TIMEOUT: Duration = Duration::from_secs(5);

/// The handler truncates its transfer length to 16 bits (FW:0x028884)
const MAX_CHUNK: u32 = 0xFFFF;

pub fn run(args: cli::Ram) -> anyhow::Result<()> {
    let devices = device::list();
    let device = (if let Some(d) = args.device {
        device::Selector::Location(d)
    } else {
        device::Selector::Only
    })
    .resolve(&devices)
    .map_err(|e| {
        let list: Vec<_> = devices.iter().map(ToString::to_string).collect();
        anyhow!("{e}\n\nattached:\n  {}", list.join("\n  "))
    })?;

    let mut transport = device.open()?;

    if args.address < args.base {
        bail!(
            "--address {:#x} is below --base {:#x}",
            args.address,
            args.base
        );
    }
    let offset = args.address - args.base;

    let mut out = Vec::with_capacity(args.len as usize);
    let mut remaining = args.len;
    let mut addr = offset;
    while remaining > 0 {
        let chunk = remaining
            .min(MAX_CHUNK)
            .min(transport.max_transfer() as u32);
        let cmd = ReadBuffer {
            id: 0,
            offset: addr,
            length: chunk,
        };
        let mut buf = vec![0u8; chunk as usize];
        let completion = transport.execute(&cmd.cdb(), Data::In(&mut buf), TIMEOUT)?;
        match interpret(&completion) {
            Outcome::Complete | Outcome::CompleteWith(_) => {}
            other => {
                let at = args.address + (out.len() as u32);
                bail!(
                    "READ BUFFER at {:#x} refused: {}",
                    at,
                    ErrorDisplay(&other, &completion)
                )
            }
        }
        let got = completion.transferred;
        if got == 0 {
            break;
        }
        out.extend_from_slice(&buf[..got]);
        addr += got as u32;
        remaining -= got as u32;
        // A short read means the unit ran out of whatever it is willing to
        // give; pushing on would just re-ask for the same window
        if got < chunk as usize {
            break;
        }
    }

    println!("read {} bytes from {:#x}", out.len(), args.address);

    match &args.out {
        Some(path) => {
            std::fs::write(path, &out)?;
            println!("wrote {path:?}");
        }
        None => hexdump(&out),
    }
    Ok(())
}

struct ErrorDisplay<'a>(
    &'a nkscan::protocol::sense::Outcome,
    &'a nkscan::transport::Completion,
);
impl std::fmt::Display for ErrorDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.1.sense {
            Some(ref s) => write!(f, "{:?}", s),
            None => write!(f, "{:?}", self.0),
        }
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
