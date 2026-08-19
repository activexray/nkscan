//! Moving image data off the unit. Section 2-11-3
//!
//! Bytes and nothing else: type `00h` has no data header and no length of its
//! own, and 2-11 has consecutive reads carry on rather than restart. What the
//! bytes mean is [`Layout`]'s business, and unscrambling them is a decoder's.

use super::{MOVE_TIMEOUT, Session};
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
use tracing::*;

impl Session {
    /// Read image data into `buf`, continuing where the last read stopped
    ///
    /// Answers how much arrived. Short of `buf` means the unit ran out, either
    /// by transferring less than asked or by answering `05h-2Ch` once the image
    /// is spent
    pub fn read_image(&mut self, layout: &Layout, buf: &mut [u8]) -> Result<usize, Error> {
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

            match self.run(&cmd.cdb(), Data::In(slice), MOVE_TIMEOUT) {
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
            match self.session.read_image(&self.layout.clone(), &mut buf) {
                Ok(0) => break,
                Ok(got) => {
                    self.surplus += got as u64;
                    if got < self.chunk {
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
                    debug!(%e, "the unit stopped giving data");
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
/// Reading to the end is what 2-11-5 says ends one, and only the pass that runs
/// to the layout's own byte count reaches that on its own. A pass the unit cut
/// short, one a decoder rejected, and one whose consumer hung up all leave the
/// scan open, and this is the one place every route out passes through.
///
/// Reading rather than ABORT deliberately: 2-13 stops the scan block where it
/// is, and aborting one mid-move has to wait for the mechanism first or the
/// handle wedges until a power cycle. `drain` gives up on the
/// first error, so there is nothing here to hang on a unit that has stopped
/// answering
impl Drop for Chunks<'_> {
    fn drop(&mut self) {
        if !self.closed {
            debug!("the pass ended early, reading the rest off so the scan closes");
            self.drain();
        }
    }
}
