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
            set_window::{ScanMode, SetWindowFunction},
        },
        cdbs::Inquiry,
        model::Quirks,
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

    let mut set_window = SetWindowFunction::try_from(&vpd(t, SetWindowFunction::PAGE_CODE).await?)?;
    if identity
        .quirks()
        .contains(Quirks::UNADVERTISED_MULTI_READING)
    {
        set_window.mode |= ScanMode::MULTI_READING;
    }

    Ok(Capabilities {
        identity,
        address,
        features,
        set_window,
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::transport::{BoxFuture, Completion, Sense, Status};
    use std::collections::HashMap;

    /// Answers INQUIRY with the pages of a capture, and refuses other pages
    pub(crate) struct Replay(pub(crate) HashMap<String, Vec<u8>>);

    impl Transport for Replay {
        fn max_transfer(&self) -> usize {
            65536
        }

        fn execute<'a>(
            &'a mut self,
            cdb: &'a [u8],
            data: Data<'a>,
            _: Duration,
        ) -> BoxFuture<'a, Result<Completion, crate::transport::Error>> {
            Box::pin(std::future::ready(Ok(self.answer(cdb, data))))
        }
    }

    impl Replay {
        fn answer(&mut self, cdb: &[u8], data: Data) -> Completion {
            let key = match cdb[1] & 1 {
                0 => "std".to_owned(),
                _ => format!("{:02x}", cdb[2]),
            };
            match (self.0.get(&key), data) {
                (Some(page), Data::In(buf)) => {
                    let n = page.len().min(buf.len());
                    buf[..n].copy_from_slice(&page[..n]);
                    Completion {
                        status: Status::Good,
                        sense: None,
                        transferred: n,
                    }
                }
                _ => Completion {
                    status: Status::CheckCondition,
                    sense: Some(Sense {
                        key: 5,
                        asc: 0x24,
                        ascq: 0,
                        tsc: None,
                        ili: false,
                        information: None,
                        raw: Vec::new(),
                    }),
                    transferred: 0,
                },
            }
        }
    }

    /// The pages of `unit`'s capture under `tests/fixtures/thumbnail_window`, keyed as [`Replay`] looks them up
    pub(crate) fn pages(unit: &str) -> HashMap<String, Vec<u8>> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/thumbnail_window")
            .join(unit);
        std::fs::read_dir(&dir)
            .expect("fixture")
            .filter_map(|e| {
                let name = e.ok()?.file_name().into_string().ok()?;
                let code = name.strip_prefix("page_")?.strip_suffix(".bin")?.to_owned();
                Some((code, std::fs::read(dir.join(&name)).ok()?))
            })
            .collect()
    }

    /// A Coolscan V capture: byte 5 of its SET WINDOW page is 46h, without
    /// `MULTI_READING`, and the unit reads a line up to 16 times anyway
    #[test]
    fn an_ls50_offers_the_multi_reading_its_page_leaves_out() {
        let mut ls50 = pages("ls50");
        assert_eq!(ls50["d1"][5] & ScanMode::MULTI_READING.bits(), 0);
        let caps = crate::session::block_on(capabilities(&mut Replay(ls50.clone()))).expect("caps");
        assert!(caps.set_window.mode.contains(ScanMode::MULTI_READING));

        // Under another model name, the probe uses the pages without a change
        let product = ls50.get_mut("std").expect("standard INQUIRY");
        product[16..24].copy_from_slice(b"LS-40 ED");
        let caps = crate::session::block_on(capabilities(&mut Replay(ls50))).expect("caps");
        assert!(!caps.set_window.mode.contains(ScanMode::MULTI_READING));
    }
}
