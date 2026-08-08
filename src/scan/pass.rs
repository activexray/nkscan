//! Taking one scan pass over the film
//!
//! Every kind of pass, thumbnail, prescan and scan alike, is the same four
//! commands. The I/O lives on [`Session`]; this module holds the pure types
//! and the decoder builder.

use crate::{
    error::Error,
    protocol::{
        caps::set_window::ColorInterleaving, curves::Curves, data::CooperativeAction, image::Layout,
    },
};

/// A finished scan pass
///
/// The samples are the caller's buffer, so this struct carries only what describes them
#[derive(Debug, Clone)]
pub struct Pass {
    /// The stream's shape, as far as 2-10's formula describes it
    pub layout: Layout,
    /// What the unit asked the host to do with the data, if anything
    pub cooperation: Option<CooperativeAction>,
    /// Whether every block the layout promised arrived
    pub complete: bool,
    /// Image rows and columns: the sensor (the layout's pixels) and the feed
    /// (its lines)
    pub rows: usize,
    pub cols: usize,
}

/// Build a decoder for a scan pass, applying CCD correction only for multi-line
pub fn decoder<'a>(
    layout: &Layout,
    curves: Option<&'a Curves>,
) -> Result<crate::protocol::decode::Decoder<'a>, Error> {
    let decoder = crate::protocol::decode::Decoder::new(layout)?;
    match curves.filter(|_| {
        layout
            .interleaving
            .contains(ColorInterleaving::MULTILINE_SIMULTANEOUS)
    }) {
        Some(curves) => Ok(decoder.correcting(curves)),
        None => Ok(decoder),
    }
}
