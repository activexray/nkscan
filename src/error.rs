//! High level errors containing protocol and transport errors

use crate::protocol::sense::{Contention, Failure, Fault, Intervention, Outcome};
use crate::transport::{self, Completion};

/// Top crate-level scanner errors
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Transport(#[from] transport::Error),

    #[error("{0}")]
    Busy(Contention),

    #[error("{0}")]
    Media(Intervention),

    #[error("no such scanner")]
    NotFound,

    #[error("{op} is not supported: {reason}")]
    Unsupported { op: &'static str, reason: String },

    #[error("scan cancelled")]
    Cancelled,

    #[error("{0}")]
    Device(Box<Fault>),
}

/// A page that would not parse is our model of the device being wrong, which
/// is the same class of problem as the device reporting a fault
impl From<crate::protocol::caps::Error> for Error {
    fn from(e: crate::protocol::caps::Error) -> Self {
        Self::Device(Box::new(Fault::Caps(e)))
    }
}

impl Error {
    /// Turn a terminal [`Outcome`] into an error
    ///
    /// The completion comes along because the sense bytes belong to it, not to
    /// the outcome, and they are what makes a fault reportable.
    ///
    /// `Working`, `NeedsHost` and `StateChanged` are the retry loop's business
    /// and should never reach here. They are not unreachable, though, since a
    /// caller that skips the loop will produce one, so they fall through to a
    /// fault rather than a panic.
    pub fn from_outcome(outcome: Outcome, completion: &Completion) -> Self {
        let sense = || completion.sense.clone();
        match outcome {
            Outcome::NeedsOperator(i) => Self::Media(i),
            Outcome::Contended(c) => Self::Busy(c),
            Outcome::Refused(r) => Self::Device(Box::new(Fault::Rejected(r, sense()))),
            Outcome::Failed(f) => Self::Device(Box::new(Fault::Reported(f, sense()))),
            _ => Self::Device(Box::new(Fault::Reported(Failure::Unrecognized, sense()))),
        }
    }
}
