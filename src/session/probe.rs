//! Asking a unit what it is, before there is a session to ask through

use super::{PROBE_TIMEOUT, READY_TIMEOUT};
use crate::{
    error::Error,
    protocol::{
        caps::{
            Capabilities, Page,
            address::{Address, CoordinateBase},
            ccd::CcdMeasurement,
            frames::Frames,
            identity::Identity,
            other::Features,
            set_window::SetWindowFunction,
        },
        cdbs::Inquiry,
        sense::{Fault, Outcome, interpret},
    },
    transport::{Data, Transport},
};
use std::time::Duration;
use tracing::*;

/// Run one INQUIRY and hand back however many bytes actually arrived
pub async fn inquiry(t: &mut dyn Transport, cmd: Inquiry) -> Result<Vec<u8>, Error> {
    inquiry_within(t, cmd, PROBE_TIMEOUT).await
}

/// As [`inquiry`], for a caller that knows the unit may take longer to answer
pub async fn inquiry_within(
    t: &mut dyn Transport,
    cmd: Inquiry,
    timeout: Duration,
) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0u8; cmd.allocation_length()];
    let completion = t.execute(&cmd.cdb(), Data::In(&mut buf), timeout).await?;
    match interpret(&completion) {
        Outcome::Complete | Outcome::CompleteWith(_) => {}
        other => return Err(Error::from_outcome(other, &completion)),
    }
    buf.truncate(completion.transferred);
    Ok(buf)
}

/// Read a VPD page from the scanner
///
/// Traces what arrived: a unit we have no spec for is only ever debugged from
/// the bytes it actually sent
pub async fn vpd(t: &mut dyn Transport, code: u8) -> Result<Page, Error> {
    let bytes = inquiry(t, Inquiry::vpd(code)).await?;
    if enabled!(Level::TRACE) {
        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
        trace!(page = format!("{code:02X}h"), bytes = hex.join(" "), "vpd");
    }
    Ok(Page::new(code, bytes)?)
}

/// Documented in LS-9000 2-2-2-7 but missing from its own page 00h list, so
/// worth asking for even when the unit does not admit to it
pub const UNLISTED: &[u8] = &[CcdMeasurement::PAGE_CODE];

/// Every VPD page code this unit carries, in the order to ask for them
///
/// Page 00h enumerates what the unit admits to, and [`UNLISTED`] covers what a
/// spec names but that list leaves out. 00h itself is dropped: it is the list,
/// not a page of capabilities.
pub async fn page_codes(t: &mut dyn Transport) -> Result<Vec<u8>, Error> {
    let list = inquiry(t, Inquiry::vpd(0x00)).await?;
    // Byte 3 is the page length. The unit pads the rest of whatever allocation
    // we asked for, so taking everything after byte 4 picks up the padding too
    let length = usize::from(*list.get(3).unwrap_or(&0));
    let mut codes: Vec<u8> = list.get(4..4 + length).unwrap_or_default().to_vec();
    codes.retain(|&code| code != 0x00);

    let missing: Vec<u8> = UNLISTED
        .iter()
        .copied()
        .filter(|code| !codes.contains(code))
        .collect();
    codes.extend(missing);
    Ok(codes)
}

/// The images of C8h and of the pages after it, while the unit has more
async fn frames(t: &mut dyn Transport) -> Result<Frames, Error> {
    let mut frames = Frames::try_from(&vpd(t, Frames::PAGE_CODE).await?)?;
    for code in Frames::NEXT_PAGES {
        if !frames.incomplete() {
            break;
        }
        frames.extend(&vpd(t, code).await?)?;
    }
    Ok(frames)
}

/// Ask the scanner what it can do
///
/// Safe to call with a unit attention outstanding: 2-2 note 5 says INQUIRY is
/// performed regardless, and does not clear it
pub async fn capabilities(t: &mut dyn Transport) -> Result<Capabilities, Error> {
    // The first command of a session meets the unit however it is, and a cold
    // one stays busy for tens of seconds. The pages below it are read from a
    // unit that has already answered
    let identity = Identity::parse(&inquiry_within(t, Inquiry::standard(), READY_TIMEOUT).await?)?;
    // Opening the wrong node is easy, and everything below assumes a scanner
    if !identity.is_scanner() {
        return Err(Error::NotFound);
    }

    let address = Address::try_from(&vpd(t, Address::PAGE_CODE).await?)?;
    // The page only exists when `Address` says it does
    let frames = match address
        .coordinate_base
        .contains(CoordinateBase::FRAME_RECTS)
    {
        true => Some(frames(t).await?),
        false => None,
    };

    // What the unit will do for itself, which every gate above here reads
    let features = Features::try_from(&vpd(t, Features::PAGE_CODE).await?)?;
    debug!(
        page_length = features.page_length,
        cooperation = ?features.cooperation,
        execute = ?features.execute,
        "features"
    );

    // Only the LS-40, LS-50 and LS-5000 give the holder name on this page (LS5000 2-2-2-2)
    let holder = match address.holder_id.filter(|&id| id != 0) {
        Some(id) if !identity.is_mf_scanner() => match vpd(t, id).await {
            Ok(page) => Some(page.ascii()?),
            Err(Error::Device(fault)) if matches!(*fault, Fault::Rejected(..)) => None,
            Err(e) => return Err(e),
        },
        _ => None,
    };

    Ok(Capabilities {
        identity,
        address,
        features,
        set_window: SetWindowFunction::try_from(&vpd(t, SetWindowFunction::PAGE_CODE).await?)?,
        // Neither spec lists this one in page 00h, so a refusal means the unit
        // has not got it rather than that anything went wrong
        ccd: match vpd(t, CcdMeasurement::PAGE_CODE).await {
            Ok(page) => Some(CcdMeasurement::try_from(&page)?),
            Err(Error::Device(fault)) if matches!(*fault, Fault::Rejected(..)) => {
                debug!("this unit has no CCD measurement page");
                None
            }
            Err(e) => return Err(e),
        },
        frames,
        // A unit without adapters has no page 01h
        adapter: match vpd(t, 0x01).await {
            Ok(page) => Some(page.ascii()?),
            Err(Error::Device(fault)) if matches!(*fault, Fault::Rejected(..)) => None,
            Err(e) => return Err(e),
        },
        holder,
    })
}
