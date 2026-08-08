//! Transport-agnostic implementations of the wire protocol of the various scanners
//!
//! Types and parsing only. Nothing here performs IO or holds device state:
//! anything that has to talk to a unit lives in [`session`](crate::session),
//! and anything that decides what a whole scan should do lives above that.

pub mod caps;
pub mod cdbs;
pub mod curves;
pub mod data;
pub mod decode;
pub mod image;
pub mod mode;
pub mod sense;
pub mod window;
