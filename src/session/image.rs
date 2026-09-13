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
    /// Read one transfer of image data into `buf`, continuing where the last
    /// read stopped
    ///
    /// `buf` is one READ, so the caller sizes it to a length the unit will
    /// split on: [`image_chunks`](Session::image_chunks) is what does that.
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
        let want = buf.len();
        let cmd = Read::new(
            DataType::Image.row().code,
            0,
            layout.width_code(),
            want as u32,
        );

        trace!(cdb = ?cmd.cdb(), want, "executing image READ");

        match self.run(&cmd.cdb(), Data::In(buf), timeout) {
            Ok(completion) => {
                trace!(
                    transferred = completion.transferred,
                    want, "image READ completed"
                );
                Ok(completion.transferred)
            }
            // 2-11-5: reading past the end of the image is how it says the
            // image is spent, not a fault
            Err(Error::Device(fault))
                if matches!(*fault, Fault::Rejected(Refusal::OutOfSequence, _)) =>
            {
                debug!("end of stream reached");
                Ok(0)
            }
            // 2-11: a transfer shorter than asked for comes back as CHECK
            // CONDITION with ILI set and the shortfall in the information
            // field. The data still arrived, so count it
            Err(Error::Device(fault)) => match short(&fault) {
                Some(missing) => {
                    debug!(missing, "the unit had less than we asked for");
                    Ok(want.saturating_sub(missing as usize))
                }
                None => Err(Error::Device(fault)),
            },
            Err(e) => {
                debug!(error = ?e, "image READ failed");
                Err(e)
            }
        }
    }

    /// Stream the image a chunk at a time, without a buffer the size of the scan
    ///
    /// Each chunk ends where [`Layout::granules`] say the unit will split, so
    /// no chunk straddles a boundary it will not.
    ///
    /// Dropping one closes the scan, whatever route the caller took out of it
    pub fn image_chunks<'a>(&'a mut self, layout: &Layout) -> Result<Chunks<'a>, Error> {
        let chunk = self.chunk_size(layout)?;
        Ok(Chunks {
            session: self,
            layout: layout.clone(),
            chunk,
            remaining: layout.total_bytes(),
            at: 0,
            spent: false,
            closed: false,
            surplus: 0,
        })
    }

    /// The most one READ may ask for
    ///
    /// Bounded by what the transport can carry and by `Address`'s general SCSI
    /// buffer size. What a READ actually asks for inside that is
    /// [`Granules::take`](crate::protocol::image::Granules::take)'s business,
    /// since it is where in a line the stream has got to that says where the
    /// next one may stop
    fn chunk_size(&self, layout: &Layout) -> Result<usize, Error> {
        let mut chunk = self.transport.max_transfer();
        if let Some(limit) = self.caps.address.scsi_buffer {
            chunk = chunk.min(usize::from(limit));
        }

        let granule = layout.granules.widest();
        if granule > chunk {
            return Err(Error::Unsupported {
                op: "image read",
                reason: format!(
                    "this unit reads in units of {granule} bytes, and no more than {chunk} can be transferred at once"
                ),
            });
        }
        Ok(chunk)
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
    /// The most one chunk can be, bounded by what the transport can carry
    chunk: usize,
    remaining: u64,
    /// How far into a line the stream has got, which is what says where the
    /// next READ may stop
    at: usize,
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

        let want = self
            .layout
            .granules
            .take(self.at, self.remaining, self.chunk);
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
                self.at = self.layout.granules.advance(self.at, got);
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
            let left = match self.remaining {
                0 if self.layout.multiline_registered => self.chunk as u64,
                0 => break,
                left => left,
            };
            let want = self.layout.granules.take(self.at, left, self.chunk);
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
                    self.at = self.layout.granules.advance(self.at, got);
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

#[cfg(test)]
mod tests {
    use crate::protocol::image::Granules;

    /// A packed three-row pass over a line count that does not divide by three
    /// leaves a part-group at the end, and asking for it as it stands is what
    /// puts the phase protocol out of step
    #[test]
    fn the_last_chunk_is_a_whole_number_of_granules() {
        let g = Granules::every(70_200);
        let chunk = g.rest * 2;
        assert_eq!(g.take(0, chunk as u64, chunk), chunk);
        assert_eq!(g.take(0, g.rest as u64 + 1, chunk), chunk);
        assert_eq!(g.take(0, 23_400, chunk), g.rest);
        assert_eq!(g.take(0, 1, chunk), g.rest);
    }

    /// Rounding up never asks for more than one chunk
    #[test]
    fn rounding_stays_inside_a_chunk() {
        let g = Granules::every(1024);
        let chunk = g.rest * 4;
        assert_eq!(g.take(0, chunk as u64 - 1, chunk), chunk);
        assert_eq!(g.take(0, u64::MAX, chunk), chunk);
        // A chunk that is not a whole number of them stops short of it
        assert_eq!(g.take(0, u64::MAX, chunk + 1), chunk);
    }

    /// A unit that constrains nothing reads exactly what is left
    #[test]
    fn an_unconstrained_unit_reads_what_is_left() {
        assert_eq!(Granules::NONE.take(0, 7, 4096), 7);
    }

    /// Issue 52: an LS-5000 reading two packed rows of four samples with
    /// infrared riding in the first of them. The line is 207872 bytes, which no
    /// transfer can carry, and every READ ends on a reading instead
    #[test]
    fn a_line_too_long_to_carry_is_read_a_reading_at_a_time() {
        let g = Granules {
            first: 31_744 * 2,
            rest: 24_064 * 2,
            line: 103_936 * 2,
        };
        let chunk = 131_072;

        // The first reading of a line is the long one, and two of them together
        // are all that fits
        assert_eq!(g.take(0, g.line as u64, chunk), 63_488 + 48_128);
        // Two more of the short ones finish the line off
        assert_eq!(g.take(63_488 + 48_128, g.line as u64, chunk), 48_128 * 2);
        assert_eq!(g.advance(63_488 + 48_128, 48_128 * 2), 0);

        // The tail of a pass is rounded up to the reading it ends in
        assert_eq!(g.take(0, 1, chunk), 63_488);
        assert_eq!(g.take(63_488, 1, chunk), 48_128);
    }

    /// The whole of that pass, 5670 lines of it, walked the way `fill` walks it
    ///
    /// Every READ fits the transfer, starts and ends on a reading, and the last
    /// one lands on the end of the pass rather than past it
    #[test]
    fn a_pass_of_uneven_readings_is_read_to_the_end() {
        let g = Granules {
            first: 31_744 * 2,
            rest: 24_064 * 2,
            line: 103_936 * 2,
        };
        let chunk = 131_072;
        let total = 103_936u64 * 5670;

        let (mut read, mut at, mut reads) = (0u64, 0, 0);
        while read < total {
            let want = g.take(at, total - read, chunk);
            assert!((1..=chunk).contains(&want), "{want} bytes is no transfer");
            read += want as u64;
            at = g.advance(at, want);
            // Wherever a READ stops is a reading boundary
            assert!(
                at == 0 || (at - g.first).is_multiple_of(g.rest),
                "stopped at {at}"
            );
            reads += 1;
        }
        assert_eq!(read, total);
        assert_eq!(at, 0);
        // Two READs a line, 111616 bytes and then 96256
        assert_eq!(reads, 5670);
    }
}
