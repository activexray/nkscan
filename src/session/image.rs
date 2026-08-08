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

        let mut done = 0;
        while done < buf.len() {
            let want = chunk.min(buf.len() - done);
            let cmd = Read::new(code, 0, width, want as u32);
            let slice = &mut buf[done..done + want];

            match self.run(&cmd.cdb(), Data::In(slice), MOVE_TIMEOUT) {
                Ok(completion) => {
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
                Err(e) => return Err(e),
            }
        }
        debug!(bytes = done, "read image");
        Ok(done)
    }

    /// Stream the image a chunk at a time, without a buffer the size of the scan
    ///
    /// Each chunk is a whole number of [`Layout::granule`]s, so a decoder can
    /// consume them without straddling a boundary the unit will not split on
    pub fn image_chunks<'a>(&'a mut self, layout: &Layout) -> Result<Chunks<'a>, Error> {
        let chunk = self.chunk_size(layout)?;
        Ok(Chunks {
            session: self,
            layout: layout.clone(),
            chunk,
            remaining: layout.total_bytes(),
            spent: false,
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
    spent: bool,
}

impl Chunks<'_> {
    /// Fill `buf` with the next chunk, answering how much arrived, or `None`
    /// once the image is spent
    ///
    /// `buf` is sized to the chunk (or what is left of it) and truncated to
    /// what actually arrived, so one buffer can be reused for every chunk
    /// without reallocating
    pub fn fill(&mut self, buf: &mut Vec<u8>) -> Option<Result<usize, Error>> {
        if self.spent || self.remaining == 0 {
            return None;
        }

        let want = self.chunk.min(self.remaining as usize);
        buf.resize(want, 0);
        let layout = &self.layout;
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
                self.remaining -= got as u64;
                // The unit ran out before the layout said it would
                if got < want {
                    self.spent = true;
                }
                buf.truncate(got);
                Some(Ok(got))
            }
        }
    }

    /// Bytes one chunk holds, which is what the transport can carry at once
    pub fn capacity(&self) -> usize {
        self.chunk
    }

    /// Bytes the layout still expects
    pub fn remaining(&self) -> u64 {
        self.remaining
    }
}
