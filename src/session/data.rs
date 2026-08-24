//! The typed READ and SEND data records, and EXECUTE. Sections 2-11, 2-14, 2-15

use super::{PROBE_TIMEOUT, Session, malformed};
use crate::{
    error::Error,
    protocol::{
        caps::other::DataTypes,
        cdbs::{Execute, GetParameter, Read, Send, SendDiagnostic, SetParameter},
        curves::Curves,
        data::{self, BoundaryType2, FrameTable, PerfInformation},
        sense::{self, Failure, Fault},
        window::Channel,
    },
    transport::{Data, Sense, Status},
};
use std::sync::Arc;
use std::time::Duration;
use tracing::*;

impl Session {
    /// READ one data type, in two passes so the data header can size the second
    ///
    /// Only for the types the data header
    /// precedes. Image data carries none and goes through
    /// [`read_image`](Session::read_image)
    pub fn read_data(
        &mut self,
        kind: data::DataType,
        color: u8,
    ) -> Result<(data::Header, data::Values), Error> {
        let (header, valid) = self.read_record(kind, color)?;
        Ok((header, data::Values::decode(kind.scalar(), &valid)))
    }

    /// As [`read_data`](Self::read_data), but the valid bytes unsplit
    ///
    /// The records with a structure of their own are easier to read this way
    /// than out of [`Values`](data::Values)
    pub fn read_record(
        &mut self,
        kind: data::DataType,
        color: u8,
    ) -> Result<(data::Header, Vec<u8>), Error> {
        let row = kind.row();
        if !row.header {
            return Err(Error::Unsupported {
                op: "read data type",
                reason: format!("{kind:?} carries no data header to size a read by"),
            });
        }
        let (width, qualifier, color) = self.addressing(kind, row.read, color, "read data type")?;
        let code = row.code;

        let mut fetch = |len: u32| -> Result<Vec<u8>, Error> {
            let cmd = Read::new(code, color, qualifier, len);
            let mut buf = vec![0u8; cmd.allocation_length()];
            debug!("cdb for read {:02x} {:02x?}", code, &cmd.cdb());
            let completion = self.run(&cmd.cdb(), Data::In(&mut buf), PROBE_TIMEOUT)?;
            buf.truncate(completion.transferred);
            debug!("recv {:02x?}", buf);
            Ok(buf)
        };

        // The header reports what the unit holds whatever we asked for, so one
        // short read is enough to size the real one
        let probe = fetch(data::HEADER as u32)?;
        let (probe, _) = data::Header::from_bytes(&probe)
            .ok_or_else(|| malformed(format!("{kind:?} header was {} bytes", probe.len())))?;

        let raw = fetch(data::HEADER as u32 + probe.length)?;
        let (header, payload) = data::Header::from_bytes(&raw)
            .ok_or_else(|| malformed(format!("{kind:?} header was {} bytes", raw.len())))?;

        // Analog gain reports 16 bytes against a documented 8, and the tail is
        // stale, so the table wins wherever it fixes a count
        let valid: &[u8] = match row.count {
            Some(n) => payload
                .get(..n as usize * width as usize)
                .unwrap_or(payload),
            None => payload,
        };
        debug!(?header, bytes = valid.len(), "read data");
        Ok((header, valid.to_vec()))
    }

    /// What a READ or SEND of `kind` has to carry: element width, the qualifier
    /// encoding it, and the channel 2-11-3 lets it name
    ///
    /// `offered` is the `Features` bit for the direction asked for
    fn addressing(
        &self,
        kind: data::DataType,
        offered: Option<DataTypes>,
        color: u8,
        op: &'static str,
    ) -> Result<(u8, u8, u8), Error> {
        let refuse = |reason| Error::Unsupported { op, reason };

        match offered {
            Some(bit) if self.caps.features.data_types.contains(bit) => {}
            _ => return Err(refuse(format!("this unit does not offer {kind:?}"))),
        }
        let Some((width, qualifier)) = kind.qualifier() else {
            return Err(refuse(format!("{kind:?} has no addressing qualifier")));
        };

        Ok((width, qualifier, if kind.per_color() { color } else { 0 }))
    }

    /// SEND one data type, 2-12
    pub fn send_data(&mut self, kind: data::DataType, color: u8, body: &[u8]) -> Result<(), Error> {
        let (_, qualifier, color) =
            self.addressing(kind, kind.row().write, color, "send data type")?;

        let cmd = Send::new(kind.row().code, color, qualifier, body.len() as u32);
        debug!(
            "cdb for send {:02x} {:02x?} data {:02x?}",
            kind.row().code,
            &cmd.cdb(),
            &body
        );
        self.run(&cmd.cdb(), Data::Out(body), PROBE_TIMEOUT)?;
        Ok(())
    }

    /// Where the unit currently thinks each frame is, 2-11-6
    pub fn boundaries(&mut self) -> Result<data::Boundary, Error> {
        let (_, record) = self.read_record(data::DataType::Boundary, 0)?;
        let boundary = data::Boundary::from_bytes(&record)
            .ok_or_else(|| malformed(format!("Boundary was {} bytes", record.len())))?;

        self.frames = Some(FrameTable::Boundary(boundary.clone()));

        Ok(boundary)
    }

    /// The frame table as far as this session knows it
    ///
    /// `None` until something has read or written one
    pub fn frames(&self) -> Option<&data::Boundary> {
        match self.frames.as_ref() {
            Some(FrameTable::Boundary(boundary)) => Some(boundary),
            _ => None,
        }
    }

    pub fn boundaries_type2(&mut self) -> Result<data::BoundaryType2, Error> {
        let (_, record) = self.read_record(data::DataType::Boundary2, 0)?;
        let boundary = data::BoundaryType2::from_bytes(&record)
            .ok_or_else(|| malformed(format!("BoundaryType2 was {} bytes", record.len())))?;

        self.frames = Some(FrameTable::BoundaryType2(boundary.clone()));

        Ok(boundary)
    }

    /// The frame table as far as this session knows it
    ///
    /// `None` until something has read or written one
    pub fn frames_type2(&self) -> Option<&data::BoundaryType2> {
        match self.frames.as_ref() {
            Some(FrameTable::BoundaryType2(boundary)) => Some(boundary),
            _ => None,
        }
    }

    /// Tell the unit where each frame is
    ///
    /// 2-11-6: after a thumbnail of strip film the host works these out and
    /// sends them, which is the only way frames the unit cannot measure for
    /// itself come to have a length
    pub fn set_boundaries(&mut self, boundary: &data::Boundary) -> Result<(), Error> {
        let bytes = boundary.to_bytes()?;
        self.send_data(data::DataType::Boundary, 0, &bytes)?;
        self.frames = Some(FrameTable::Boundary(boundary.clone()));
        Ok(())
    }

    /// 2-11-9: alternate Type2 indexing for roll feeders
    pub fn set_boundaries_type2(&mut self, boundary: &data::BoundaryType2) -> Result<(), Error> {
        let bytes = boundary.to_bytes()?;
        self.send_data(data::DataType::Boundary2, 0, &bytes)?;
        self.frames = Some(FrameTable::BoundaryType2(boundary.clone()));
        Ok(())
    }

    /// Derive `FramePosition`s for arbitrary frame tops, registering each
    /// against the unit's own perforation reading at its address
    fn derive_positions(
        &mut self,
        frames: &[data::Rect],
    ) -> Result<Vec<data::FramePosition>, Error> {
        let caps = &self.caps;
        let origin = caps.address.y_axis.address_range.start;
        let end = caps.address.y_axis.address_range.last;
        // The resolution discovery's pass ran at, so an address maps onto the
        // perforation table with the same column arithmetic that built it
        let asked = u32::from(caps.address.thumbnail_resolution.start).max(1);
        let optical_y = u32::from(caps.address.y_axis.optical_dpi)
            .max(u32::from(caps.address.x_axis.optical_dpi));
        let pitch = (optical_y / asked).max(1);

        let perfs = self.read_perforations()?;
        let mut positions = Vec::with_capacity(frames.len());
        for (n, r) in frames.iter().enumerate() {
            if r.top < origin || r.top > end {
                return Err(malformed(format!(
                    "frame {}: top {} is outside the axis range {origin}..={end}, \
                     so there is nowhere to register it",
                    n + 1,
                    r.top
                )));
            }
            // Nearest column: detection's own tops are exact multiples of the
            // pitch away from the origin, an edited one may not be
            let col = ((r.top - origin) + pitch / 2) / pitch;
            match perfs.at(col as usize) {
                Some(perf) => positions.push(data::FramePosition::new(r.top, perf)),
                None => {
                    return Err(malformed(format!(
                        "frame {}: no perforation reading at address {} (column {col}), \
                         so this frame cannot be registered for the transport",
                        n + 1,
                        r.top
                    )));
                }
            }
        }
        Ok(positions)
    }

    /// The boundary table `frames` corresponds to, for the loaded medium
    pub fn rebuild_table(&mut self, frames: &[data::Rect]) -> Result<FrameTable, Error> {
        // A discovered table decides by being one; no table yet - the
        // `--frames-file` case, where this session skipped discovery - decides
        // by what the unit's family speaks, which is also what the flag that
        // carried through the probe says
        let type2 = match self.frames.as_ref() {
            Some(table) => matches!(table, FrameTable::BoundaryType2(_)),
            None => self.uses_frame_type_2(),
        };

        if !type2 {
            return Ok(FrameTable::Boundary(data::Boundary {
                frames: frames.to_vec(),
            }));
        }
        let mut positions = self.derive_positions(frames)?;
        // Sorted by top, as discovery built the original
        positions.sort_by_key(|f| f.top);
        Ok(FrameTable::BoundaryType2(BoundaryType2 {
            frames: positions,
        }))
    }

    /// Send an edited set of frame boundaries as one table, before any scanning
    ///
    /// Returns the table that was sent.
    pub fn update_frames(&mut self, frames: &[data::Rect]) -> Result<FrameTable, Error> {
        let table = self.rebuild_table(frames)?;
        match &table {
            FrameTable::Boundary(b) => self.set_boundaries(b)?,
            FrameTable::BoundaryType2(t) => self.set_boundaries_type2(t)?,
        }
        Ok(table)
    }

    /// Make sure the unit's boundary table carries a registration for `rect`'s
    /// top line, before anything moves toward it
    ///
    /// Callers that know a whole edited set up front should prefer rebuilding and
    /// sending the table once themselves ([`Self::rebuild_table`]); this is the
    /// fallback for callers who arrive one frame at a time.
    pub fn ensure_frame_registration(&mut self, rect: &data::Rect) -> Result<(), Error> {
        let missing = match self.frames.as_ref() {
            Some(FrameTable::BoundaryType2(t)) => !t.frames.iter().any(|f| f.top == rect.top),
            _ => false,
        };
        if !missing {
            return Ok(());
        }

        // Cloned ahead of the unit round-trip below, which borrows mutably
        let mut amended = match self.frames.as_ref() {
            Some(FrameTable::BoundaryType2(t)) => t.clone(),
            _ => unreachable!("checked above"),
        };
        let entry = *self
            .derive_positions(std::slice::from_ref(rect))?
            .first()
            .expect("one frame in, one position out");
        // Kept sorted by top, as discovery built the original
        match amended.frames.binary_search_by(|f| f.top.cmp(&rect.top)) {
            Ok(i) => amended.frames[i] = entry,
            Err(i) => amended.frames.insert(i, entry),
        }
        info!(top = rect.top, "registered an edited frame position");
        self.set_boundaries_type2(&amended)
    }

    pub fn read_perforations(&mut self) -> Result<data::PerfInformation, Error> {
        let (_, record) = self.read_record(data::DataType::Perforation, 0)?;
        self.test_unit_ready(Duration::from_millis(500))?;

        let perfs = PerfInformation::from_bytes(&record)
            .ok_or_else(|| malformed(format!("PerfInfo was {} bytes", record.len())))?;
        Ok(perfs)
    }

    pub fn read_boundaries_type2(&mut self) -> Result<data::BoundaryType2, Error> {
        let (_, record) = self.read_record(data::DataType::Boundary2, 0)?;

        let bounds = BoundaryType2::from_bytes(&record)
            .ok_or_else(|| malformed(format!("BoundaryType2 was {} bytes", record.len())))?;
        Ok(bounds)
    }

    /// The exposure the unit measured for this channel when it started up
    ///
    /// 2-11-8, `DataType::WhiteBalanceExposure`, one 4-byte value. Across the
    /// visible channels the ratios are the unit's own white balance, so metering
    /// that wants to preserve neutral starts from these rather than from
    /// whatever the last session left in the descriptors.
    ///
    /// 2-11-3 lists only the default, R, G and B qualifiers, but the unit
    /// answers for infrared as well and Nikon Scan reads it in every capture.
    /// The qualifier is the window identifier
    pub fn white_balance(&mut self, channel: Channel) -> Result<u32, Error> {
        let color = channel.id();
        let (_, values) = self.read_data(data::DataType::WhiteBalanceExposure, color)?;
        let data::Values::Longs(v) = values else {
            return Err(malformed(format!(
                "WhiteBalanceExposure color {color} did not come back as longs"
            )));
        };
        let exposure = *v
            .first()
            .ok_or_else(|| malformed(format!("WhiteBalanceExposure color {color} was empty")))?;
        debug!(color, exposure, "start-up exposure");
        Ok(exposure)
    }

    /// What the unit remembers about the film and the images on it
    ///
    /// 2-11-7, `DataType::Setup`, per color. Holds the base level and, for each
    /// image, what a prescan decided. Survives across sessions
    pub fn setup(&mut self, color: u8) -> Result<data::Setup, Error> {
        let (_, values) = self.read_data(data::DataType::Setup, color)?;
        let data::Values::Bytes(record) = values else {
            return Err(malformed("Setup did not come back as bytes".into()));
        };
        data::Setup::from_bytes(&record)
            .ok_or_else(|| malformed(format!("Setup was {} bytes", record.len())))
    }

    /// Read the CCD's own response curves once and cache them on the session
    ///
    /// `CcdData` is not per-color, so one read covers every channel. The
    /// measurement type is fixed at 0, the only one Nikon Scan or this
    /// driver uses. Returns whether curves were cached; `false` covers both
    /// a unit that offers none and a reply that does not match the page
    /// describing it
    pub fn fetch_curves(&mut self) -> bool {
        let Some(ccd) = self.caps.ccd.clone() else {
            return false;
        };
        let rows = usize::from(self.caps.address.lines).max(1);
        let (_, values) = match self
            .read_data(data::DataType::CcdData, 0)
            .inspect_err(|e| debug!(%e, "no CCD curves to correct with"))
        {
            Ok(v) => v,
            Err(_) => return false,
        };
        let data::Values::Words(words) = values else {
            debug!("CcdData did not come back as words");
            return false;
        };
        let curves = Curves::parse(&ccd, &words, rows, 0);
        if curves.is_none() {
            warn!(
                curves = ccd.curves(),
                points = ccd.points.len(),
                got = words.len(),
                "the CCD curves do not match the page describing them, scanning uncorrected"
            );
        }
        self.curves = curves.map(Arc::new);
        self.curves.is_some()
    }

    /// The cached CCD curves, refcount-bumped for the decoder thread
    pub fn curves(&self) -> Option<Arc<Curves>> {
        self.curves.clone()
    }

    /// Read the initiator cooperative action parameter a SCAN just asked for
    pub fn cooperation(&mut self) -> Result<data::CooperativeAction, Error> {
        let (_, values) = self.read_data(data::DataType::Cooperation, 0)?;
        let data::Values::Bytes(record) = values else {
            return Err(malformed("Cooperation did not come back as bytes".into()));
        };
        data::CooperativeAction::from_bytes(&record)
            .ok_or_else(|| malformed(format!("Cooperation was {} bytes", record.len())))
    }

    /// Set the operation parameter, activate the operation, and confirm its
    /// termination
    ///
    /// 2-14: EXECUTE performs the operation *after* returning GOOD status, and
    /// no command other than a basic command may be issued before the operation
    /// termination is confirmed by TEST UNIT READY. So all three are one call
    pub fn execute(
        &mut self,
        operation: data::Op,
        params: data::Operation,
        timeout: Duration,
    ) -> Result<(), Error> {
        if !self.caps.features.execute.supports(operation) {
            return Err(Error::Unsupported {
                op: "execute operation",
                reason: format!("this unit does not offer {operation:?}"),
            });
        }

        let block = params.to_bytes();
        let cmd = SetParameter::new(operation.code(), block.len() as u32);
        self.run(&cmd.cdb(), Data::Out(&block), PROBE_TIMEOUT)?;

        debug!(?operation, ?params, "executing");
        self.run(&Execute.cdb(), Data::None, PROBE_TIMEOUT)?;

        // 2-8: a failed operation reports 02h-04h-02h and nothing else. The
        // real cause is only readable once, so take it while it is there
        match self.test_unit_ready(timeout) {
            Err(Error::Device(fault))
                if matches!(*fault, Fault::Reported(Failure::Mechanism, _)) =>
            {
                match self.diagnose() {
                    // The wrapper says mechanical whatever the cause was;
                    // `sense::diagnosed` is what actually reads it
                    Ok(Some(sense)) => Err(Error::Device(Box::new(Fault::Reported(
                        sense::diagnosed(&sense),
                        Some(sense),
                    )))),
                    _ => Err(Error::Device(fault)),
                }
            }
            other => other,
        }
    }

    /// Read back what an operation is currently set to
    ///
    /// 2-16, the other half of SET PARAMETER. Worth it after an autofocus: the
    /// unit reports the focus position it settled on, which is what makes a
    /// focus repeatable without focusing again
    pub fn get_parameter(&mut self, operation: data::Op) -> Result<data::Operation, Error> {
        if !self.caps.features.execute.supports(operation) {
            return Err(Error::Unsupported {
                op: "get parameter",
                reason: format!("this unit does not offer {operation:?}"),
            });
        }

        let cmd = GetParameter::new(operation.code(), data::Operation::LENGTH as u32);
        let mut buf = vec![0u8; cmd.allocation_length()];
        let completion = self.run(&cmd.cdb(), Data::In(&mut buf), PROBE_TIMEOUT)?;
        buf.truncate(completion.transferred);

        let params = data::Operation::from_bytes(&buf)
            .ok_or_else(|| malformed(format!("{operation:?} was {} bytes", buf.len())))?;
        debug!(?operation, ?params, "read parameters");
        Ok(params)
    }

    /// Ask what actually went wrong, after a generic mechanical error
    ///
    /// 2-8. The concrete fault only comes back here, and reading it clears it,
    /// so there is one chance at it. `None` means the unit had nothing to say.
    pub fn diagnose(&mut self) -> Result<Option<Sense>, Error> {
        let completion =
            self.transport
                .execute(&SendDiagnostic.cdb(), Data::None, PROBE_TIMEOUT)?;
        debug!(status = ?completion.status, sense = ?completion.sense, "diagnostic");
        Ok(completion
            .sense
            .filter(|_| completion.status == Status::CheckCondition))
    }
}
