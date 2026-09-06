//! Scratch: read Boundary Type2 back with Nikon Scan's own CDB.

use nkscan::{device, session::Session, transport::Data};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let want = std::env::args().nth(1);
    let devices = device::list();
    let device = match &want {
        Some(loc) => devices
            .iter()
            .find(|d| format!("{d:?}").contains(loc.as_str()))
            .ok_or("no such scanner")?,
        None => devices.first().ok_or("no scanner found")?,
    };
    let mut session = Session::open(device.open()?)?;

    for len in [6u32, 0x3a, 52, 58] {
        let [_, hi, mid, lo] = len.to_be_bytes();
        let cdb = [0x28, 0, 0x8f, 0, 0x00, 0x03, hi, mid, lo, 0x80];
        let mut buf = vec![0u8; len as usize];
        print!("len {len:3} cdb {cdb:02x?} -> ");
        match session.run(&cdb, Data::In(&mut buf), Duration::from_secs(10)) {
            Ok(c) => {
                buf.truncate(c.transferred);
                println!("{} bytes {}", c.transferred, hex(&buf));
            }
            Err(e) => println!("{e}"),
        }
    }
    println!("\nnkscan's own path, type 1:");
    match session.boundaries() {
        Ok(t) => println!("  {} frames {:?}", t.frames.len(), t.frames.first()),
        Err(e) => println!("  {e}"),
    }
    println!("\nnkscan's own path, type 2:");
    match session.read_boundaries_type2() {
        Ok(t) => println!("  {} frames {:?}", t.frames.len(), t.frames),
        Err(e) => println!("  {e}"),
    }
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x} ")).collect()
}
