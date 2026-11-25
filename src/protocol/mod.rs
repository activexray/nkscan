//! Transport-agnostic implementations of the Nikon wire-protocol
//!
//! Types and parsing only. Nothing here performs IO or holds device state.
//! Anything that has to talk to a unit lives in [`session`](crate::session),
//! and anything that decides what a whole scan should do lives above that.

pub mod caps;
pub mod cdbs;
pub mod curves;
pub mod data;
pub mod decode;
pub mod image;
pub mod mode;
pub mod model;
pub mod sense;
pub mod window;

/// The big-endian `u16` at `i`, in a record already checked to be long enough
pub(crate) fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

/// The big-endian `u32` at `i`, in a record already checked to be long enough
pub(crate) fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
