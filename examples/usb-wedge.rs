//! Probe a wedged USB scanner's bulk endpoints
//!
//! Run this against a unit that has stopped answering, *before* power cycling
//! it. It tells three states apart that all look like "the scanner stopped
//! answering" from outside: a halted endpoint, stale bytes left in the IN pipe
//! by an aborted transfer, and a unit waiting on the rest of a phase handshake.
//!
//! Nothing here goes through the crate's transport - the point is to see the
//! endpoints raw.

use nusb::{
    MaybeFuture,
    transfer::{Buffer, Bulk, In, Out},
};
use std::time::Duration;

/// LS5K 1-1-2: ask the unit to prepare a phase response
const PHASE_CHECK_CODE: u8 = 0xD0;
/// A standard INQUIRY for 36 bytes
const INQUIRY: [u8; 6] = [0x12, 0x00, 0x00, 0x00, 0x24, 0x00];

/// Long enough for a healthy unit, short enough not to sit here
const SHORT: Duration = Duration::from_millis(300);

fn main() {
    let info = nusb::list_devices()
        .wait()
        .expect("could not enumerate USB")
        .find(|d| d.vendor_id() == 0x04b0)
        .expect("no Nikon device on the bus");
    println!(
        "device {:04x}:{:04x} at bus {} ports {:?}",
        info.vendor_id(),
        info.product_id(),
        info.bus_id(),
        info.port_chain()
    );

    let device = info.open().wait().expect("open");
    let interface = device.claim_interface(0).wait().expect("claim interface 0");
    let mut ep_in = interface.endpoint::<Bulk, In>(0x82).expect("bulk in 0x82");
    let mut ep_out = interface.endpoint::<Bulk, Out>(0x01).expect("bulk out 0x01");
    let mps = ep_in.max_packet_size();
    println!("claimed interface 0, IN max packet {mps}\n");

    // A pass the host walked away from can have a whole image still queued,
    // so this reads until the pipe comes back empty rather than a fixed few
    println!("1. anything already queued on IN?");
    let mut stale = 0usize;
    let mut reads = 0usize;
    loop {
        match ep_in
            .transfer_blocking(Buffer::new(mps), SHORT)
            .into_result()
        {
            Ok(b) => {
                if reads == 0 {
                    println!("   first read: {} bytes {:02X?}", b.len(), &b[..b.len().min(32)]);
                }
                stale += b.len();
                reads += 1;
            }
            Err(e) => {
                println!("   read {reads} ended it: {e:?}");
                break;
            }
        }
    }
    println!("   -> {stale} stale bytes drained over {reads} reads\n");

    println!("2. clear_halt on both endpoints");
    println!("   IN  {:?}", ep_in.clear_halt().wait());
    println!("   OUT {:?}", ep_out.clear_halt().wait());
    println!();

    println!("3. IN again, after the halt clear");
    match ep_in
        .transfer_blocking(Buffer::new(mps), SHORT)
        .into_result()
    {
        Ok(b) => println!(
            "   {} bytes {:02X?}",
            b.len(),
            &b[..b.len().min(32)]
        ),
        Err(e) => println!("   {e:?}"),
    }
    println!();

    println!("4. bare phase check (D0h out, 1 byte in)");
    match ep_out
        .transfer_blocking(PHASE_CHECK_CODE.to_le_bytes().to_vec().into(), SHORT)
        .into_result()
    {
        Ok(_) => println!("   OUT ok"),
        Err(e) => println!("   OUT {e:?}"),
    }
    match ep_in
        .transfer_blocking(Buffer::new(mps), SHORT)
        .into_result()
    {
        Ok(b) => println!("   IN {} bytes {:02X?}", b.len(), &b[..b.len().min(8)]),
        Err(e) => println!("   IN {e:?}"),
    }
    println!();

    // Deliberately leaves the unit mid-transaction: a CDB and a phase check
    // with no data phase, no status reception code and no status read. This is
    // the state an `execute` that returns early leaves behind, so it is how to
    // test whether that alone is what wedges a unit
    if std::env::args().nth(1).as_deref() != Some("--desync") {
        println!("5. skipped (pass --desync to leave the unit mid-transaction)");
        return;
    }
    println!("5. INQUIRY CDB, then phase check, then walk away");
    match ep_out
        .transfer_blocking(INQUIRY.to_vec().into(), SHORT)
        .into_result()
    {
        Ok(_) => println!("   OUT ok"),
        Err(e) => println!("   OUT {e:?}"),
    }
    match ep_out
        .transfer_blocking(PHASE_CHECK_CODE.to_le_bytes().to_vec().into(), SHORT)
        .into_result()
    {
        Ok(_) => println!("   phase OUT ok"),
        Err(e) => println!("   phase OUT {e:?}"),
    }
    match ep_in
        .transfer_blocking(Buffer::new(mps), SHORT)
        .into_result()
    {
        Ok(b) => println!("   phase IN {} bytes {:02X?}", b.len(), &b[..b.len().min(8)]),
        Err(e) => println!("   phase IN {e:?}"),
    }
}
