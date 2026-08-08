//! The typed READ and SEND data records, and EXECUTE. Sections 2-11, 2-14, 2-15

use super::{PROBE_TIMEOUT, Session, malformed};
use crate::{
    error::Error,
    protocol::{
        caps::other::DataTypes,
        cdbs::{Execute, GetParameter, Read, Send, SendDiagnostic, SetParameter},
        curves::Curves,
        data,
        sense::{self, Failure, Fault},
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
            let completion = self.run(&cmd.cdb(), Data::In(&mut buf), PROBE_TIMEOUT)?;
            buf.truncate(completion.transferred);
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
        let Some(width) = kind.row().width else {
            return Err(refuse(format!(
                "{kind:?} takes a width 2-11-2 does not fix"
            )));
        };
        let qualifier = data::width_code(width).expect("2-11-2 widths are all encodable");
        Ok((width, qualifier, if kind.per_color() { color } else { 0 }))
    }

    /// SEND one data type, 2-12
    pub fn send_data(&mut self, kind: data::DataType, color: u8, body: &[u8]) -> Result<(), Error> {
        let (_, qualifier, color) =
            self.addressing(kind, kind.row().write, color, "send data type")?;

        let cmd = Send::new(kind.row().code, color, qualifier, body.len() as u32);
        debug!(?kind, bytes = body.len(), "send data");
        self.run(&cmd.cdb(), Data::Out(body), PROBE_TIMEOUT)?;
        Ok(())
    }

    /// Where the unit currently thinks each frame is, 2-11-6
    pub fn boundaries(&mut self) -> Result<data::Boundary, Error> {
        let (_, record) = self.read_record(data::DataType::Boundary, 0)?;
        let boundary = data::Boundary::from_bytes(&record)
            .ok_or_else(|| malformed(format!("Boundary was {} bytes", record.len())))?;
        self.frames = Some(boundary.clone());
        Ok(boundary)
    }

    /// The frame table as far as this session knows it
    ///
    /// `None` until something has read or written one
    pub fn frames(&self) -> Option<&data::Boundary> {
        self.frames.as_ref()
    }

    /// Tell the unit where each frame is
    ///
    /// 2-11-6: after a thumbnail of strip film the host works these out and
    /// sends them, which is the only way frames the unit cannot measure for
    /// itself come to have a length
    pub fn set_boundaries(&mut self, boundary: &data::Boundary) -> Result<(), Error> {
        let bytes = boundary.to_bytes()?;
        self.send_data(data::DataType::Boundary, 0, &bytes)?;
        self.frames = Some(boundary.clone());
        Ok(())
    }

    /// The exposures the unit measured for neutral white when it started up
    ///
    /// 2-11-8, `DataType::WhiteBalanceExposure`, one 4-byte value per color, answered in R, G, B
    /// order. The ratios are the unit's own white balance, so metering that
    /// wants to preserve neutral starts from these rather than from whatever
    /// the last session left in the descriptors.
    ///
    /// 2-11-3 only gives the qualifier default, R, G and B, so there is no
    /// infrared reading here.
    pub fn white_balance(&mut self) -> Result<[u32; 3], Error> {
        let mut out = [0u32; 3];
        for (n, slot) in out.iter_mut().enumerate() {
            let color = n as u8 + 1;
            let (_, values) = self.read_data(data::DataType::WhiteBalanceExposure, color)?;
            let data::Values::Longs(v) = values else {
                return Err(malformed(format!(
                    "WhiteBalanceExposure color {color} did not come back as longs"
                )));
            };
            *slot = *v.first().ok_or_else(|| {
                malformed(format!("WhiteBalanceExposure color {color} was empty"))
            })?;
        }
        debug!(?out, "start-up white balance");
        Ok(out)
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

    /// The CCD's own response curves, for correcting its rows against each other
    ///
    /// `kind` picks one of the measurement types `CcdMeasurement` counts, whose
    /// meaning nothing states; 0 is the only one anything has used.
    ///
    /// `None` rather than an error where the unit offers no curves or the reply
    /// Read the CCD response curves once and cache them on the session
    ///
    /// `CcdData` is not per-color, so one read covers every channel. `kind` is
    /// the measurement type; 0 is the only one Nikon Scan or this driver uses.
    /// Returns whether curves were cached
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
