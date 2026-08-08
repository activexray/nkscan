//! Decode an NKDSBP2 `scsi_trace.bin` capture into something a session's own
//! `RUST_LOG=nkscan::cdb=trace` output can be diffed against.
//!
//! The Windows proxy the Nikon capture corpus comes from writes one record per
//! IOCTL_SCSISCAN_CMD exchange, in the SREC layout documented in
//! `scsi_proxy/README.md`. The old text log truncates large payloads; the `.bin`
//! does not, so it is the one to keep.
//!
//! The point of the tool is the stage-position story: SET WINDOW *is* the move
//! command on this family (`docs/PROTOCOL.md`), so a capture's window origins
//! are its stage movements. The leading `[n]` on every line is the record's
//! position in the exchange, so a NikonScan capture and an `nkscan` run can be
//! diffed in order.
//!
//! ```text
//! cargo run --example trace -- /mnt/storage/NikonScanDecomp/scan_captures/another_normal_scan_of_one_frame/scsi_trace.bin
//! ```
//!
//! READs of image data are summed instead of dumped: one scan is hundreds of
//! megabytes and adds nothing to a movement diff.

use std::{env, fs, path::PathBuf};

use nkscan::protocol::{
    data::{self, Boundary},
    window::Window,
};

/// One SREC record, `scsi_proxy/README.md`
struct Record<'a> {
    /// Correlates with the Windows text log's `[#N]`
    seq: u32,
    /// Command descriptor block; `{cdb_len}` bytes are real
    cdb: &'a [u8],
    /// Whatever the data phase carried
    data: &'a [u8],
}

/// Offset of the CDB within a record, per `scsi_proxy/README.md`
const CDB_AT: usize = 24;
/// Offset of the data length within a record
const LEN_AT: usize = 40;

fn main() -> anyhow::Result<()> {
    let path = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("usage: cargo run --example trace <scsi_trace.bin>"));
    let bytes = fs::read(&path)?;

    let mut records = Vec::new();
    let mut at = 0usize;
    while at + LEN_AT + 4 <= bytes.len() {
        if &bytes[at..at + 4] != b"SREC" {
            anyhow::bail!("bad magic at {at}: {:02?}", &bytes[at..at + 4]);
        }
        let seq = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap());
        let cdb_len = bytes[at + 8] as usize;
        let sense_len = bytes[at + 11] as usize;
        let data_len =
            u32::from_le_bytes(bytes[at + LEN_AT..at + LEN_AT + 4].try_into().unwrap()) as usize;
        let data_end = at + LEN_AT + 4 + data_len;
        if data_end + sense_len > bytes.len() {
            anyhow::bail!(
                "record at {at} claims {data_len} data and {sense_len} sense bytes, \
                 past the end of the capture"
            );
        }
        let cdb = &bytes[at + CDB_AT..at + CDB_AT + cdb_len.min(16)];
        let data = &bytes[at + LEN_AT + 4..data_end];
        records.push(Record { seq, cdb, data });
        at = data_end + sense_len;
    }

    let mut image_reads = 0u64;
    let mut image_bytes = 0u64;

    for (n, r) in records.iter().enumerate() {
        let op = opcode(r.cdb);

        // An image read is the bulk of any scan and has no position to tell
        if op == 0x28 && dtc(r.cdb) == Some(0) {
            image_reads += 1;
            image_bytes += r.data.len() as u64;
            continue;
        }

        match op {
            0x24 => println!(
                "[{n}] seq={} SET WINDOW: {}",
                r.seq,
                window_row(&r.data[8..])
            ),
            0x1B => println!(
                "[{n}] seq={} SCAN windows = [{}]",
                r.seq,
                r.data
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            // A Boundary record is preceded by the six-byte data header 2-11-6 defines
            0x28 if dtc(r.cdb) == Some(0x88) => {
                match r.data.get(data::HEADER..).and_then(Boundary::from_bytes) {
                    Some(b) => {
                        println!(
                            "[{n}] seq={} FRAME BOUNDARY ({} frame(s))",
                            r.seq,
                            b.frames.len()
                        );
                        for f in &b.frames {
                            println!(
                                "      rect top={} left={} bottom={} right={}",
                                f.top, f.left, f.bottom, f.right
                            );
                        }
                    }
                    None => println!("[{n}] seq={} FRAME BOUNDARY (unparseable)", r.seq),
                }
            }
            _ => println!("[{n}] seq={} {}", r.seq, describe(r)),
        }
    }

    println!("\nimage data: {image_bytes} bytes across {image_reads} READs");
    Ok(())
}

/// The 50-byte SET WINDOW descriptor that follows the 8-byte header, in the
/// fields the movement story needs and a few that pin down the scan behaviour
fn window_row(descriptor: &[u8]) -> String {
    let Ok(w) = Window::try_from(descriptor) else {
        return format!("{} bytes, not a descriptor", descriptor.len());
    };
    format!(
        "id={} dpi=({},{}) origin=({},{}) size=({},{}) bpp={} reading={} color=0x{:02x} flags=0x{:02x} kind=0x{:02x} mode=0x{:02x} interleave=0x{:02x} ae={} exposure={}",
        w.id,
        w.resolution.0,
        w.resolution.1,
        w.origin.0,
        w.origin.1,
        w.size.0,
        w.size.1,
        w.bpp,
        w.multiple_reading,
        w.color_ordering,
        w.flags.bits(),
        w.scanning_kind.bits(),
        w.scanning_mode.bits(),
        w.color_interleaving.bits(),
        w.ae_value,
        w.exposure,
    )
}

/// One line for commands whose payload is not geometry
fn describe(r: &Record) -> String {
    let op = opcode(r.cdb);
    match op {
        // READ and SEND name their data type in byte 2
        0x28 | 0x2A => format!(
            "{} [{}] {} byte(s)",
            opcode_name(op),
            data_type(dtc(r.cdb)),
            r.data.len()
        ),
        // SET/GET PARAMETER carry the operation they address in byte 2
        0xE0 | 0xE1 => format!(
            "{} op=0x{:02x}",
            opcode_name(op),
            r.cdb.get(2).copied().unwrap_or(0)
        ),
        _ if r.data.is_empty() => opcode_name(op),
        _ => format!("{} {} bytes", opcode_name(op), r.data.len()),
    }
}

fn opcode(cdb: &[u8]) -> u8 {
    cdb.first().copied().unwrap_or(0)
}

/// The data-type byte (byte 2) of a READ or SEND
fn dtc(cdb: &[u8]) -> Option<u8> {
    cdb.get(2).copied()
}

fn opcode_name(op: u8) -> String {
    let name = match op {
        0x00 => "TEST UNIT READY",
        0x12 => "INQUIRY",
        0x15 => "MODE SELECT",
        0x1A => "MODE SENSE",
        0x1B => "SCAN",
        0x1D => "SEND DIAGNOSTIC",
        0x24 => "SET WINDOW",
        0x25 => "GET WINDOW",
        0x28 => "READ",
        0x2A => "SEND",
        0xC0 => "ABORT",
        0xC1 => "EXECUTE",
        0xE0 => "SET PARAMETER",
        0xE1 => "GET PARAMETER",
        _ => return format!("OPCODE {op:02x}"),
    };
    name.to_string()
}

fn data_type(code: Option<u8>) -> String {
    let label = match code {
        Some(0) => "IMAGE",
        Some(0x02) => "HALFTONE MASK",
        Some(0x03) => "LUT",
        Some(0x80) => "HISTOGRAM",
        Some(0x81) => "MAX VALUE",
        Some(0x82) => "MATRIX",
        Some(0x83) => "FILTER",
        Some(0x84) => "SHADING",
        Some(0x85) => "DARK VOLTAGE",
        Some(0x86) => "MAGNETIC",
        Some(0x87) => "COOPERATION",
        Some(0x88) => "BOUNDARY",
        Some(0x89) => "ANALOG GAMMA",
        Some(0x8A) => "ANALOG GAIN",
        Some(0x8B) => "DIGITAL GAIN",
        Some(0x8C) => "WB EXPOSURE",
        Some(0x8D) => "SETUP",
        Some(0x8E) => "PERFORATION",
        Some(0x8F) => "BOUNDARY 2",
        Some(0x90) => "SHIPMENT WB",
        Some(0x91) => "CCD DATA",
        Some(0x92) => "DRIVER VERSION",
        Some(0x93) => "LEAK VOLUME",
        Some(0xE0) => "RAM BUFFER",
        Some(0xE1) => "EEPROM BUFFER",
        Some(other) => return format!("DATA {other:02x}"),
        None => return "RAW".to_string(),
    };
    label.to_string()
}
