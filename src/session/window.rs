//! GET WINDOW, SET WINDOW and SCAN. Sections 2-9, 2-10 and 2-7

use super::{MOVE_TIMEOUT, PROBE_TIMEOUT, Session, malformed};
use crate::{
    error::Error,
    protocol::{
        caps::set_window::ScanKind,
        cdbs::{GetWindow, Scan, SetWindow},
        data::{CooperativeAction, Rect},
        image::Layout,
        window::{self, GetWindowHeader, SetWindowHeader, Window},
    },
    transport::Data,
};
use tracing::*;

impl Session {
    /// One GET WINDOW, exactly as asked: header plus descriptors, unparsed
    fn get_window_raw(&mut self, cmd: GetWindow) -> Result<Vec<u8>, Error> {
        let mut buf = vec![0u8; cmd.allocation_length()];
        let completion = self.run(&cmd.cdb(), Data::In(&mut buf), PROBE_TIMEOUT)?;
        buf.truncate(completion.transferred);
        Ok(buf)
    }

    /// Read back every window descriptor the unit currently holds
    ///
    /// Two passes: a transfer longer than what is there gets refused, so the
    /// header has to say how much there is first
    pub fn windows(&mut self) -> Result<Vec<Window>, Error> {
        let probe = self.get_window_raw(GetWindow::all(window::HEADER as u32))?;
        let (probe, _) =
            GetWindowHeader::from_bytes(&probe).map_err(|e| malformed(e.to_string()))?;

        let data = self.get_window_raw(GetWindow::all(2 + u32::from(probe.data_length)))?;
        let (header, descriptors) =
            GetWindowHeader::from_bytes(&data).map_err(|e| malformed(e.to_string()))?;
        let stride = usize::from(header.descriptor_length);
        debug!(stride, bytes = descriptors.len(), "window descriptors");

        if stride < window::LENGTH {
            return Err(malformed(format!(
                "descriptor stride of {stride} is shorter than the {} bytes 2-10-3 defines",
                window::LENGTH
            )));
        }

        descriptors
            .chunks_exact(stride)
            .map(|d| Window::try_from(d).map_err(|e| malformed(e.to_string())))
            .collect()
    }

    /// Whether the stage has to home before it can reach this window
    ///
    /// Only an image window sits in a frame; a thumbnail spans everything. With
    /// no table read or written there is nothing to judge against
    fn homes_to_reach(&self, window: &Window) -> bool {
        if !window.scanning_kind.contains(ScanKind::IMAGE) {
            return false;
        }
        let Some(frames) = self.frames().filter(|f| !f.frames.is_empty()) else {
            return false;
        };
        let (left, top) = window.origin;
        frames
            .holding(Rect {
                top,
                left,
                bottom: top + window.size.1,
                right: left + window.size.0,
            })
            .is_none()
    }

    /// Define one window, which is also what moves the stage
    ///
    /// An image window inside a known frame steps straight to it; one no frame
    /// holds sends the mechanism out to its home stop first. The unit takes it
    /// either way and the image is the same, so this says so rather than refusing
    pub fn set_window(&mut self, window: &Window) -> Result<(), Error> {
        window.validate(&self.caps)?;
        if self.homes_to_reach(window) {
            warn!(
                id = window.id,
                ?window.origin,
                ?window.size,
                "no frame holds this window, so the stage will home to reach it"
            );
        }

        let header = SetWindowHeader {
            descriptor_length: window::LENGTH as u16,
        };
        let mut payload = Vec::with_capacity(window::HEADER + window::LENGTH);
        payload.extend_from_slice(&header.to_bytes());
        payload.extend_from_slice(&window.to_bytes());

        let cmd = SetWindow::new(payload.len() as u32);
        debug!(id = window.id, "setting window");
        self.run(&cmd.cdb(), Data::Out(&payload), MOVE_TIMEOUT)?;
        Ok(())
    }

    /// Scan the windows named, and hand back what it will produce
    ///
    /// 2-7: the unit answers, then scans, so this returns once it has started
    /// and [`test_unit_ready`](Session::test_unit_ready) says when the data is
    /// there.
    ///
    /// A cooperative request is not a blocker. The captures read the
    /// `DataType::Cooperation` record and
    /// send SCAN again with nothing in between: it says what the host will owe
    /// the *data*, not what has to happen before the scan runs. Whatever it
    /// asks for comes back on [`Started::cooperation`] for the caller to honor
    /// once the image is read.
    pub fn scan(&mut self, windows: &[Window]) -> Result<Started, Error> {
        // Checks every rule spanning the set on the way
        let layout = Layout::new(&self.caps, windows, self.divisor)?;

        let ids: Vec<u8> = windows.iter().map(|w| w.id).collect();
        let cmd = Scan::new(ids.len() as u8);

        // 2-7: the unit answers, then asks what the host will owe the data.
        // `run_handshake` reads the `DataType::Cooperation` record, sends SCAN
        // again with nothing in between, and hands the record back so the
        // caller can honor it once the image is read
        let (_, cooperation) = self.run_handshake(&cmd.cdb(), Data::Out(&ids), MOVE_TIMEOUT)?;
        debug!(?ids, ?cooperation, "scanning");

        Ok(Started {
            layout,
            cooperation,
        })
    }
}

/// A scan that has started
#[derive(Debug, Clone)]
pub struct Started {
    /// What the stream will look like
    pub layout: Layout,
    /// What the unit asked the host to do with the data, if anything. Reading
    /// the record is what lets the scan proceed; honoring it is the caller's
    pub cooperation: Option<CooperativeAction>,
}
