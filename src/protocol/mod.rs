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
