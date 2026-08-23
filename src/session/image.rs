//! Moving image data off the unit. Section 2-11-3
//!
//! Bytes and nothing else: type `00h` has no data header and no length of its
//! own, and 2-11 has consecutive reads carry on rather than restart. What the
//! bytes mean is [`Layout`]'s business, and unscrambling them is a decoder's.

use super::{DRAIN_TIMEOUT, MOVE_TIMEOUT, Session};
use crate::{
    error::Error,
    protocol::{
        cdbs::Read,
        data::DataType,
        image::Layout,
        sense::{Fault, Refusal},
    },
    transport::Data,
};
use std::time::Duration;
use tracing::*;

impl Session {
    /// Read image data into `buf`, continuing where the last read stopped
    ///
    /// Answers how much arrived. Short of `buf` means the unit ran out, either
    /// by transferring less than asked or by answering `05h-2Ch` once the image
    /// is spent
    pub fn read_image(&mut self, layout: &Layout, buf: &mut [u8]) -> Result<usize, Error> {
        self.read_image_within(layout, buf, MOVE_TIMEOUT)
    }

    /// The same, on a budget the caller sets
    ///
    /// The first read of a pass waits for the stage to reach position, which is
    /// what [`MOVE_TIMEOUT`] is sized for. A read partway through one has no
    /// such waiting to do, and giving it a stage move's worth of budget is how
    /// a unit that has stopped answering holds the program for three minutes
    /// instead of saying so
    pub fn read_image_within(
        &mut self,
        layout: &Layout,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, Error> {
        let chunk = self.chunk_size(layout)?;
        let code = DataType::Image.row().code;
        let width = layout.width_code();
        let len = buf.as_mut().len();

        let mut done = 0;
        while done < buf.len() {
            let want = chunk.min(buf.len() - done);

            let cmd = Read::new(code, 0, width, want as u32);
            let slice = &mut buf[done..done + want];

            trace!(
                cdb = ?cmd.cdb(),
                want,
                done,
                left = len - done,
                "executing image READ"
            );

            match self.run(&cmd.cdb(), Data::In(slice), timeout) {
                Ok(completion) => {
                    trace!(
                        transferred = completion.transferred,
                        want, "image READ completed"
                    );

                    done += completion.transferred;
                    if completion.transferred < want {
                        break;
                    }
                }
                // 2-11-5: reading past the end of the image is how it says the
                // image is spent, not a fault
                Err(Error::Device(fault))
                    if matches!(*fault, Fault::Rejected(Refusal::OutOfSequence, _)) =>
                {
                    debug!(done, "end of stream reached");
                    break;
                }
                // 2-11: a transfer shorter than asked for comes back as CHECK
                // CONDITION with ILI set and the shortfall in the information
                // field. The data still arrived, so count it and stop
                Err(Error::Device(fault)) => match short(&fault) {
                    Some(missing) => {
                        done += want.saturating_sub(missing as usize);
                        debug!(missing, "the unit had less than we asked for");
                        break;
                    }
                    None => return Err(Error::Device(fault)),
                },
                Err(e) => {
                    debug!(error = ?e, "image READ failed");
                    return Err(e);
                }
            }
        }
        // Once a chunk, so hundreds of megabytes of scan is thousands of lines
        trace!(bytes = done, "read image");
        Ok(done)
    }

    /// Stream the image a chunk at a time, without a buffer the size of the scan
    ///
    /// Each chunk is a whole number of [`Layout::granule`]s, so a decoder can
    /// consume them without straddling a boundary the unit will not split on.
    ///
    /// Dropping one closes the scan, whatever route the caller took out of it
    pub fn image_chunks<'a>(&'a mut self, layout: &Layout) -> Result<Chunks<'a>, Error> {
        let chunk = self.chunk_size(layout)?;
        Ok(Chunks {
            session: self,
            layout: layout.clone(),
            chunk,
            remaining: layout.total_bytes(),
            spent: false,
            closed: false,
            surplus: 0,
        })
    }

    /// How much to ask for in one READ
    ///
    /// Bounded by what the transport can carry and by `Address`'s general SCSI
    /// buffer size, then rounded down to whole granules
    fn chunk_size(&self, layout: &Layout) -> Result<usize, Error> {
        let mut chunk = self.transport.max_transfer();
        if let Some(limit) = self.caps.address.scsi_buffer {
            chunk = chunk.min(usize::from(limit));
        }

        let granule = layout.granule;
        if granule > chunk {
            return Err(Error::Unsupported {
                op: "image read",
                reason: format!(
                    "this unit reads in units of {granule} bytes, and no more than {chunk} can be transferred at once"
                ),
            });
        }
        Ok(chunk / granule * granule)
    }
}

/// How far a transfer fell short, when that is what the unit reported
fn short(fault: &Fault) -> Option<u32> {
    let (Fault::Reported(_, Some(sense)) | Fault::Rejected(_, Some(sense))) = fault else {
        return None;
    };
    sense.ili.then_some(sense.information).flatten()
}

/// Image data read off the unit a chunk at a time, into buffers the caller
/// provides
///
/// Not an [`Iterator`]: each chunk lands in a buffer the caller owns and can
/// reuse, so a pool of them can shuttle whole chunks between threads without a
/// copy. Reading into [`fill`](Chunks::fill) rather than owning a buffer is
/// what lets the transport hand each chunk over whole.
pub struct Chunks<'a> {
    session: &'a mut Session,
    layout: Layout,
    /// Bytes in one chunk, bounded by what the transport can carry
    chunk: usize,
    remaining: u64,
    /// Nothing more will be handed out, which says nothing about whether the
    /// unit is finished with the scan
    spent: bool,
    /// The unit has been read to the end and the scan is over
    closed: bool,
    /// Bytes the unit held past what the layout promised
    ///
    /// The modes that raise a cooperative request carry more than the arithmetic
    /// says: a re-registered multi-line pass has extra lines at the seams. They
    /// are read and dropped rather than left behind, since a scan the host walks
    /// away from stays open and every command after it is refused out of
    /// sequence
    surplus: u64,
}

impl Chunks<'_> {
    /// Fill `buf` with the next chunk, answering how much arrived, or `None`
    /// once the image is spent
    ///
    /// `buf` is sized to the chunk (or what is left of it) and truncated to
    /// what actually arrived, so one buffer can be reused for every chunk
    /// without reallocating
    pub fn fill(&mut self, buf: &mut Vec<u8>) -> Option<Result<usize, Error>> {
        if self.spent {
            return None;
        }
        if self.remaining == 0 {
            // Reading past the end is only worth it once the unit has said a
            // re-registered multi-line pass left seams behind. Nothing else
            // does, and a unit that never raised it is not guaranteed to
            // answer a READ it was never going to get: some just stop
            // answering the handle entirely
            if self.layout.multiline_registered {
                self.drain();
            } else {
                self.spent = true;
                self.closed = true;
            }
            return None;
        }

        let want = self.chunk.min(self.remaining as usize);
        buf.resize(want, 0);
        let layout = &self.layout;

        trace!(
            remaining = self.remaining,
            buf_len = buf.len(),
            "issuing image READ from fill"
        );
        match self.session.read_image(layout, &mut buf[..want]) {
            Err(e) => {
                self.spent = true;
                Some(Err(e))
            }
            Ok(0) => {
                self.spent = true;
                None
            }
            Ok(got) => {
                let rem = self.remaining as i64 - got as i64;

                if rem < 0 {
                    self.remaining = 0;
                } else {
                    self.remaining = rem as u64;
                }
                // The unit ran out before the layout said it would
                if got < want {
                    self.spent = true;
                }
                buf.truncate(got);
                Some(Ok(got))
            }
        }
    }

    /// Read off whatever the unit still holds, so the scan closes
    ///
    /// 2-11-5: the unit answers a read past the end of the image with `05h-2Ch`,
    /// and that is what ends a scan. Stopping at the layout's own arithmetic
    /// instead leaves it open, and the next command that is not a basic one is
    /// refused with the same code
    fn drain(&mut self) {
        self.spent = true;
        self.closed = true;
        // The surplus is the seams of a re-registered pass, a fraction of it. A
        // unit answering every read in full is not going to say it is spent,
        // and this also runs on the way out of a pass that already went wrong,
        // so give up rather than read forever
        let limit = self.layout.total_bytes().max(self.chunk as u64);
        let mut buf = vec![0u8; self.chunk];
        loop {
            // Never ask for more than the pass still owes. Reading past the end
            // is what 2-11-5 says ends a scan, but only a re-registered
            // multi-line pass is holding anything back there, and a unit that
            // was never going to answer such a read stops answering the handle
            // entirely rather than saying so - the same reason `fill` does not
            // reach for it either. Asking for a whole chunk against the 28 KiB
            // an interrupted pass had left is what hung an LS-50 hard enough to
            // need a power cycle
            let want = match self.remaining {
                0 if self.layout.multiline_registered => self.chunk,
                0 => break,
                left => self.chunk.min(left as usize),
            };
            // A stage move's budget here is what turns a unit that has stopped
            // answering into three silent minutes. The pass is already running,
            // so a chunk is either on its way or it is never coming
            match self.session.read_image_within(
                &self.layout.clone(),
                &mut buf[..want],
                DRAIN_TIMEOUT,
            ) {
                Ok(0) => break,
                Ok(got) => {
                    // Only what arrives past what the layout promised is
                    // surplus; the rest is the pass's own remainder
                    let owed = (got as u64).min(self.remaining);
                    self.remaining -= owed;
                    self.surplus += got as u64 - owed;
                    debug!(
                        got,
                        remaining = self.remaining,
                        surplus = self.surplus,
                        "read off part of the remainder"
                    );
                    if got < want {
                        break;
                    }
                    if self.surplus >= limit {
                        warn!(
                            bytes = self.surplus,
                            limit,
                            "the unit is still handing data back, giving up on closing the scan"
                        );
                        break;
                    }
                }
                Err(e) => {
                    warn!(
                        %e,
                        remaining = self.remaining,
                        "the unit stopped giving data, so the scan is left open - it will not \
                         take another command until it is power cycled"
                    );
                    break;
                }
            }
        }
        if self.surplus > 0 {
            warn!(
                bytes = self.surplus,
                "the unit held more than the layout promised, so the pass is not what it was read as"
            );
        }
    }

    /// Bytes one chunk holds, which is what the transport can carry at once
    pub fn capacity(&self) -> usize {
        self.chunk
    }
}

/// A scan the host walks away from stays open, and every command after it is
/// refused out of sequence
///
/// A pass the unit cut short, one a decoder rejected, and one whose consumer
/// hung up all leave the scan open, and this is the one place every route out
/// passes through.
///
/// 2-13's ABORT is what closes it, not reading to the end. Reading was the
/// earlier choice here, on the grounds that aborting mid-move risks the
/// mechanism - but a USBPcap capture of NikonScan's own Stop button against a
/// Coolscan V shows it aborting 9.1 s into a 2 MB/s readout, at the boundary
/// right after a READ's status, with GOOD back in 1.6 ms, no data read
/// afterwards and no endpoint cleanup of any kind. The hazard is aborting a
/// stage move, which is a different operation from a readout.
///
/// That matters because the alternative costs whatever is left of the pass: the
/// unit sets the pace, so a cancelled 40-minute scan used to take the rest of
/// the 40 minutes to stop.
impl Drop for Chunks<'_> {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.spent = true;
        self.closed = true;
        info!(remaining = self.remaining, "stopping the scan");
        match self.session.abort() {
            Ok(true) => {}
            // Nothing in either spec's command list is optional here, but
            // `abort` tolerates a unit that has never heard of it, and reading
            // to the end is the only other way to close a scan
            Ok(false) => {
                debug!("no ABORT on this unit, so reading the remainder off instead");
                self.closed = false;
                self.drain();
            }
            Err(e) => warn!(
                %e,
                remaining = self.remaining,
                "could not stop the scan, so it is left open - the next command \
                 will be refused out of sequence"
            ),
        }
    }
}
