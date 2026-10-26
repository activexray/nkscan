//! The bulk pipes over WebUSB, which cannot cancel a transfer (WICG/webusb#25), so the next call takes its result

use super::{Link, Pipes, clear_stall, transfer_err};
use crate::{time::sleep, transport::Error};
use nusb::transfer::Buffer;
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
    time::Duration,
};
use tracing::trace;

impl Link for Pipes {
    fn in_max_packet(&self) -> usize {
        self.in_max_packet
    }

    async fn write_out(&mut self, bytes: &[u8], timeout: Duration) -> Result<(), Error> {
        // A queued write with these bytes is this write sent late. Do not send it twice: the LS-40 refuses a repeated READ with 05h-2Ch
        let ours = match self.ep_out.pending() {
            0 => {
                self.ep_out.submit(Buffer::from(bytes));
                self.late_out.clear();
                self.late_out.extend_from_slice(bytes);
                true
            }
            _ if self.late_out == bytes => {
                trace!("a write that timed out is still queued, so it is this one");
                true
            }
            _ => false,
        };
        let Some(sent) = within(timeout, self.ep_out.next_complete()).await else {
            return Err(Error::Timeout(timeout));
        };
        let sent = sent.into_result();
        if let Err(e) = sent {
            clear_stall(&mut self.ep_out, e).await;
        }
        // A different write went late and ours did not go, so time out and the handshake drains and sends again
        if !ours {
            trace!("a write that timed out went through late, ours did not go");
            return Err(Error::Timeout(timeout));
        }
        sent.map(drop).map_err(|e| transfer_err(e, timeout))
    }

    async fn transfer_in(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>, Error> {
        // A read that timed out gets the unit's next bytes, so take its result even if its length is not `len`
        if self.ep_in.pending() == 0 {
            self.ep_in.submit(Buffer::new(len));
        } else {
            trace!("a read that timed out is still outstanding, so it takes this answer");
        }
        let Some(got) = within(timeout, self.ep_in.next_complete()).await else {
            return Err(Error::Timeout(timeout));
        };
        match got.into_result() {
            Ok(buf) => Ok(buf.into_vec()),
            Err(e) => {
                clear_stall(&mut self.ep_in, e).await;
                Err(transfer_err(e, timeout))
            }
        }
    }
}

/// `future`'s output, or `None` after `timeout`. Dropping `next_complete` leaves its transfer queued
async fn within<F: Future>(timeout: Duration, future: F) -> Option<F::Output> {
    let mut future = pin!(future);
    let mut timer = pin!(sleep(timeout));
    poll_fn(|cx| {
        if let Poll::Ready(output) = future.as_mut().poll(cx) {
            return Poll::Ready(Some(output));
        }
        timer.as_mut().poll(cx).map(|()| None)
    })
    .await
}
