//! The bulk pipes over WebUSB, which is async and cannot cancel a transfer
//!
//! A transfer that times out here is still queued on its endpoint, and nothing
//! takes it back: WebUSB has no cancellation (WICG/webusb#25). Whatever the
//! unit sends next lands in it, and completions come back in the order the
//! transfers went out. So a timed-out read is not dropped. The next read takes
//! its answer instead of queueing behind it, and a write first waits for the
//! one still ahead of it. The handshake above sees the same thing it sees from
//! a native stack: a read that timed out, and later reads that carry on from
//! wherever the unit is

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
        // A write that timed out is still ahead of this one, and its completion
        // is the next to come back, so it has to finish before ours can be read
        while self.ep_out.pending() > 0 {
            let Some(earlier) = within(timeout, self.ep_out.next_complete()).await else {
                return Err(Error::Timeout(timeout));
            };
            trace!("a write that timed out went through late");
            if let Err(e) = earlier.into_result() {
                clear_stall(&mut self.ep_out, e).await;
            }
        }

        self.ep_out.submit(Buffer::from(bytes));
        let Some(sent) = within(timeout, self.ep_out.next_complete()).await else {
            return Err(Error::Timeout(timeout));
        };
        match sent.into_result() {
            Ok(_) => Ok(()),
            Err(e) => {
                clear_stall(&mut self.ep_out, e).await;
                Err(transfer_err(e, timeout))
            }
        }
    }

    async fn transfer_in(&mut self, len: usize, timeout: Duration) -> Result<Vec<u8>, Error> {
        // The unit's next bytes go to a read that timed out, so that read is
        // this one. Its length can differ from `len`: what it returns is still
        // no more than one short packet past what the unit had to say, which
        // is what the handshake judges a read by
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

/// `future`'s output, or `None` once `timeout` passes first
///
/// Dropping a `next_complete` future leaves its transfer queued, which is what
/// lets a timed-out transfer finish under a later call
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
