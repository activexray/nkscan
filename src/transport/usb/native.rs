//! The bulk pipes over a native USB stack, which blocks with a timeout
//!
//! nusb cancels a transfer when its timeout runs out, so a read that timed out
//! is gone and the next one starts fresh. The futures here do their IO before
//! they return, which is what keeps a session future ready the first time it is
//! polled

use super::{Link, Pipes, clear_stall, transfer_err};
use crate::transport::Error;
use nusb::transfer::Buffer;
use std::time::Duration;

impl Link for Pipes {
    fn in_max_packet(&self) -> usize {
        self.in_max_packet
    }

    async fn write_out(&mut self, bytes: &[u8], timeout: Duration) -> Result<(), Error> {
        let sent = self
            .ep_out
            .transfer_blocking(bytes.into(), timeout)
            .into_result();
        match sent {
            Ok(_) => Ok(()),
            Err(e) => {
                clear_stall(&mut self.ep_out, e).await;
                Err(transfer_err(e, timeout))
            }
        }
    }

    async fn transfer_in(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>, Error> {
        let got = self
            .ep_in
            .transfer_blocking(Buffer::new(len), timeout)
            .into_result();
        match got {
            Ok(buf) => Ok(buf.into_vec()),
            Err(e) => {
                clear_stall(&mut self.ep_in, e).await;
                Err(transfer_err(e, timeout))
            }
        }
    }
}
