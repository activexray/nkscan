//! High-level scanning operations
//!
//! This is where we work out what a scan should do, from what the unit says it
//! can do, and then order the session calls to do it. Checks that a single
//! argument is legal stay down in `session`; picking between two mechanisms
//! happens here, once, before anything moves.

pub mod autoexpose;
pub mod boundaries;
pub mod clean;
pub mod focus;
pub mod frame;
pub mod framing;
pub mod meter;
pub mod pass;
pub mod profile;
pub mod thumbnail;
pub mod window;
