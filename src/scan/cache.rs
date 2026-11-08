//! A discovery that a later session can use again
//!
//! A new discovery measures the frames again and can put a frame some pixels from where the last one put it.
//! The caller decides where to keep the bytes.

use super::{
    framing::{Discovery, Framing},
    pass::Pass,
    profile::Film,
    thumbnail::LinePitch,
};
use crate::{
    error::Error,
    protocol::{
        caps::{Capabilities, film::FilmFormat},
        data::{Boundary, BoundaryType2, FrameTable, Rect},
        decode::Samples,
        image::Layout,
    },
    session::Session,
};
use tracing::*;

/// The start of the bytes. Change the number when the layout changes, so that older bytes are not read
const MAGIC: &[u8] = b"nkscan discovery 1\n";

/// The unit, adapter, film and format that a discovery is valid for, because each changes the frames or the thumbnail
fn key(caps: &Capabilities, film: Film, format: Option<FilmFormat>) -> String {
    format!(
        "{} {} {:?} {:?} {film:?} {format:?}",
        caps.identity.product.trim(),
        caps.identity.revision.trim(),
        caps.adapter,
        caps.address.holder_id,
    )
}

impl Discovery {
    /// Encode for [`resume`]. Nikon Scan also keeps its table and thumbnail until eject (LS5000.md3 `FUN_10018e10`)
    ///
    /// `samples` is the thumbnail from [`discover`](super::framing::discover), and `film` and `format` are its arguments
    pub fn to_bytes(
        &self,
        caps: &Capabilities,
        film: Film,
        format: Option<FilmFormat>,
        samples: &Samples,
    ) -> Result<Vec<u8>, Error> {
        let mut out = Writer(MAGIC.to_vec());
        out.bytes(key(caps, film, format).as_bytes());

        match &self.table {
            FrameTable::Boundary(b) => {
                out.u8(0);
                out.bytes(&b.to_bytes()?);
            }
            FrameTable::BoundaryType2(b) => {
                out.u8(1);
                out.bytes(&b.to_bytes()?);
            }
        }
        out.u32(self.frames.len() as u32);
        for r in &self.frames {
            [r.top, r.left, r.bottom, r.right]
                .into_iter()
                .for_each(|v| out.u32(v));
        }
        out.option(self.line_pitch, |out, p| out.u32(p.0));
        out.option(self.column_addresses.as_ref(), |out, a| out.u32s(a));
        out.option(self.row_addresses.as_ref(), |out, a| out.u32s(a));
        out.option(self.contrast, |out, c| out.0.extend(c.to_le_bytes()));
        out.option(self.base_density, |out, d| {
            d.into_iter().for_each(|v| out.0.extend(v.to_le_bytes()));
        });

        // Full scale, because the decoded layout has 16 bits
        out.option(self.thumbnail.as_ref(), |out, pass| {
            let mut planes = Samples {
                colors: samples.colors.clone(),
                ir: None,
            };
            planes.to_full_scale(pass.layout.sample_bits());
            out.u32(pass.rows as u32);
            out.u32(pass.cols as u32);
            out.u8(u8::from(pass.complete));
            out.u32(pass.blocks as u32);
            out.bytes(&pass.layout.colors().collect::<Vec<_>>());
            for plane in &planes.colors {
                out.u32(plane.len() as u32);
                plane.iter().for_each(|&v| out.0.extend(v.to_le_bytes()));
            }
        });
        Ok(out.0)
    }
}

/// Decode bytes from [`Discovery::to_bytes`], send the frame table again, and put the thumbnail in `samples`
///
/// `None` if the bytes are for a different unit, adapter, film or format, or if the medium changed.
/// After a medium change, the unit returns `28h-00h` (see [`Session::medium_changes`]) and reports a different table
pub async fn resume(
    session: &mut Session,
    bytes: &[u8],
    film: Film,
    format: Option<FilmFormat>,
    samples: &mut Samples,
) -> Result<Option<Discovery>, Error> {
    let Some((wanted, discovery)) = decode(bytes, samples) else {
        debug!("not a discovery this build reads");
        return Ok(None);
    };
    if wanted != key(session.capabilities(), film, format) {
        debug!(%wanted, "the discovery is for a different unit, adapter, film or format");
        return Ok(None);
    }
    if session.medium_changes() > 0 {
        debug!(
            changes = session.medium_changes(),
            "the medium changed since the session opened"
        );
        return Ok(None);
    }

    match (Framing::choose(session.capabilities()), &discovery.table) {
        (Framing::Thumbnail, FrameTable::Boundary(table)) => {
            // If the unit cannot report its table, the medium change count is the only check
            if let Ok(held) = session.boundaries().await
                && held != *table
            {
                debug!(?held, "the unit reports a different table");
                return Ok(None);
            }
            session.set_boundaries(table).await?;
        }
        (Framing::Perforation, FrameTable::BoundaryType2(table)) => {
            if let Ok(held) = session.read_boundaries_type2().await
                && held != *table
            {
                debug!(?held, "the unit reports a different table");
                return Ok(None);
            }
            session.set_boundaries_type2(table).await?;
        }
        // These come from a page, so discovery sent no table
        (Framing::Published | Framing::Address, _) => {}
        (mechanism, _) => {
            debug!(?mechanism, "the table is not the type this unit uses");
            return Ok(None);
        }
    }
    info!(frames = discovery.frames.len(), "used the last discovery");
    Ok(Some(discovery))
}

/// The key and the discovery. The thumbnail planes go into `samples`
fn decode(bytes: &[u8], samples: &mut Samples) -> Option<(String, Discovery)> {
    let mut r = Reader(bytes.strip_prefix(MAGIC)?);
    let key = String::from_utf8(r.bytes()?.to_vec()).ok()?;
    let table = match r.u8()? {
        0 => FrameTable::Boundary(Boundary::from_bytes(r.bytes()?)?),
        1 => FrameTable::BoundaryType2(BoundaryType2::from_bytes(r.bytes()?)?),
        _ => return None,
    };
    let frames = (0..r.u32()?)
        .map(|_| {
            Some(Rect {
                top: r.u32()?,
                left: r.u32()?,
                bottom: r.u32()?,
                right: r.u32()?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let line_pitch = r.option(|r| r.u32().map(LinePitch))?;
    let column_addresses = r.option(Reader::u32s)?;
    let row_addresses = r.option(Reader::u32s)?;
    let contrast = r.option(|r| r.take::<4>().map(f32::from_le_bytes))?;
    let base_density = r.option(|r| {
        let mut d = [0.0; 3];
        for v in &mut d {
            *v = f64::from_le_bytes(r.take()?);
        }
        Some(d)
    })?;
    let thumbnail = r.option(|r| {
        let (rows, cols) = (r.u32()? as usize, r.u32()? as usize);
        let complete = r.u8()? != 0;
        let blocks = r.u32()? as usize;
        let channels = r.bytes()?.to_vec();
        let mut colors = Vec::with_capacity(channels.len());
        for _ in &channels {
            let len = r.u32()? as usize;
            if len != rows * cols {
                return None;
            }
            let plane = r.slice(2 * len)?;
            colors.push(
                plane
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&b| u16::from_le_bytes(b))
                    .collect(),
            );
        }
        Some((
            Pass {
                layout: Layout::single_line(rows as u32, cols as u32, channels),
                cooperation: Vec::new(),
                complete,
                blocks,
                rows,
                cols,
            },
            colors,
        ))
    })?;
    if !r.0.is_empty() {
        return None;
    }

    let thumbnail = thumbnail.map(|(pass, colors)| {
        *samples = Samples { colors, ir: None };
        pass
    });
    Some((
        key,
        Discovery {
            table,
            frames,
            thumbnail,
            line_pitch,
            column_addresses,
            row_addresses,
            contrast,
            base_density,
        },
    ))
}

/// Little-endian fields, with a length before each field that has no fixed size
struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }

    fn u32(&mut self, v: u32) {
        self.0.extend(v.to_le_bytes());
    }

    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend(b);
    }

    fn u32s(&mut self, v: &[u32]) {
        self.u32(v.len() as u32);
        v.iter().for_each(|&v| self.u32(v));
    }

    fn option<T>(&mut self, v: Option<T>, put: impl FnOnce(&mut Self, T)) {
        self.u8(u8::from(v.is_some()));
        if let Some(v) = v {
            put(self, v);
        }
    }
}

/// Reads what [`Writer`] writes. `None` if the bytes end too early or do not parse
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn slice(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }

    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.slice(N)?.try_into().ok()
    }

    fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|[v]| v)
    }

    fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_le_bytes)
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.slice(n)
    }

    fn u32s(&mut self) -> Option<Vec<u32>> {
        (0..self.u32()?).map(|_| self.u32()).collect()
    }

    /// `Some(None)` if the field is absent, `None` if the bytes do not parse
    fn option<T>(&mut self, get: impl FnOnce(&mut Self) -> Option<T>) -> Option<Option<T>> {
        match self.u8()? {
            0 => Some(None),
            1 => get(self).map(Some),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::data::FramePosition;
    use crate::scan::window::tests::caps;

    fn discovery(samples: &mut Samples) -> Discovery {
        let (rows, cols) = (3, 4);
        *samples = Samples {
            colors: (0..3)
                .map(|c| (0..12).map(|i| c * 100 + i).collect())
                .collect(),
            ir: None,
        };
        Discovery {
            table: FrameTable::BoundaryType2(BoundaryType2 {
                frames: vec![FramePosition {
                    top: 882,
                    perf_number: 1,
                    perf_decimal: 0x81,
                    pulse_number: 18,
                }],
            }),
            frames: vec![Rect {
                top: 882,
                left: 0,
                bottom: 6841,
                right: 3946,
            }],
            thumbnail: Some(Pass {
                layout: Layout::single_line(rows, cols, vec![1, 2, 3]),
                cooperation: Vec::new(),
                complete: true,
                blocks: 4,
                rows: rows as usize,
                cols: cols as usize,
            }),
            line_pitch: None,
            column_addresses: Some(vec![0, 42, 84, 126]),
            row_addresses: Some(vec![30, 20, 10]),
            contrast: None,
            base_density: Some([0.25, 0.5, 0.75]),
        }
    }

    /// Decoding gives every field of the discovery, and the thumbnail goes into the caller's buffer
    #[test]
    fn a_discovery_comes_back_whole() {
        let caps = caps();
        let mut samples = Samples::default();
        let found = discovery(&mut samples);
        let bytes = found
            .to_bytes(&caps, Film::Negative, None, &samples)
            .expect("encodes");

        let mut back = Samples::default();
        let (key, resumed) = decode(&bytes, &mut back).expect("decodes");
        assert_eq!(key, super::key(&caps, Film::Negative, None));
        assert_eq!(resumed.table, found.table);
        assert_eq!(resumed.frames, found.frames);
        assert_eq!(resumed.column_addresses, found.column_addresses);
        assert_eq!(resumed.row_addresses, found.row_addresses);
        assert_eq!(resumed.base_density, found.base_density);
        assert_eq!(back.colors, samples.colors);
        let pass = resumed.thumbnail.expect("a thumbnail");
        assert_eq!((pass.rows, pass.cols, pass.blocks), (3, 4, 4));
        assert!(pass.complete);
        assert_eq!(pass.layout.colors().collect::<Vec<_>>(), [1, 2, 3]);
    }

    /// A different film or format gives a different key, and older or short bytes do not decode
    #[test]
    fn a_discovery_is_only_for_what_made_it() {
        let caps = caps();
        let mut samples = Samples::default();
        let bytes = discovery(&mut samples)
            .to_bytes(&caps, Film::Negative, None, &samples)
            .expect("encodes");
        let (key, _) = decode(&bytes, &mut Samples::default()).expect("decodes");
        assert_ne!(key, super::key(&caps, Film::Positive, None));
        assert_ne!(
            key,
            super::key(&caps, Film::Negative, Some(FilmFormat::F135))
        );

        let mut older = bytes.clone();
        older[MAGIC.len() - 2] = b'0';
        assert!(decode(&older, &mut Samples::default()).is_none());
        assert!(decode(&bytes[..bytes.len() - 1], &mut Samples::default()).is_none());
    }
}
