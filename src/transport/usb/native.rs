//! The bulk pipes over native USB. nusb cancels a transfer at its timeout, so the next read starts fresh

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
