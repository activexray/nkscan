//! Asking a unit what it is, before there is a session to ask through

use super::PROBE_TIMEOUT;
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
use tracing::*;

/// Run one INQUIRY and hand back however many bytes actually arrived
pub fn inquiry(t: &mut dyn Transport, cmd: Inquiry) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0u8; cmd.allocation_length()];
    let completion = t.execute(&cmd.cdb(), Data::In(&mut buf), PROBE_TIMEOUT)?;
    match interpret(&completion) {
        Outcome::Complete | Outcome::CompleteWith(_) => {}
        other => return Err(Error::from_outcome(other, &completion)),
    }
    buf.truncate(completion.transferred);
    Ok(buf)
}

/// Read a VPD page from the scanner
pub fn vpd(t: &mut dyn Transport, code: u8) -> Result<Page, Error> {
    Ok(Page::new(code, inquiry(t, Inquiry::vpd(code))?)?)
}

/// Ask the scanner what it can do
///
/// Safe to call with a unit attention outstanding: 2-2 note 5 says INQUIRY is
/// performed regardless, and does not clear it
pub fn capabilities(t: &mut dyn Transport) -> Result<Capabilities, Error> {
    let identity = Identity::parse(&inquiry(t, Inquiry::standard())?)?;
    // Opening the wrong node is easy, and everything below assumes a scanner
    if !identity.is_scanner() {
        return Err(Error::NotFound);
    }

    let address = Address::try_from(&vpd(t, Address::PAGE_CODE)?)?;
    // The page only exists when `Address` says it does
    let frames = match address
        .coordinate_base
        .contains(CoordinateBase::FRAME_RECTS)
    {
        true => Some(Frames::try_from(&vpd(t, Frames::PAGE_CODE)?)?),
        false => None,
    };

    Ok(Capabilities {
        identity,
        address,
        features: Features::try_from(&vpd(t, Features::PAGE_CODE)?)?,
        set_window: SetWindowFunction::try_from(&vpd(t, SetWindowFunction::PAGE_CODE)?)?,
        // Neither spec lists this one in page 00h, so a refusal means the unit
        // has not got it rather than that anything went wrong
        ccd: match vpd(t, CcdMeasurement::PAGE_CODE) {
            Ok(page) => Some(CcdMeasurement::try_from(&page)?),
            Err(Error::Device(fault)) if matches!(*fault, Fault::Rejected(..)) => {
                debug!("this unit has no CCD measurement page");
                None
            }
            Err(e) => return Err(e),
        },
        frames,
    })
}
