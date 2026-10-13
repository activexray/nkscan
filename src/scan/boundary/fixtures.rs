//! Reading the test fixtures, which only the tests do

/// A 16-bit RGB thumbnail TIFF, as color planes, rows and columns
pub(crate) fn read_tiff(path: &std::path::Path) -> (Vec<Vec<u16>>, usize, usize) {
    use tiff::decoder::{Decoder, DecodingResult};
    let mut decoder = Decoder::new(std::fs::File::open(path).expect("tiff")).expect("decoder");
    let (cols, rows) = decoder.dimensions().expect("dimensions");
    let DecodingResult::U16(rgb) = decoder.read_image().expect("image") else {
        panic!("{} is not 16-bit", path.display());
    };
    let planes = (0..3)
        .map(|c| rgb.iter().skip(c).step_by(3).copied().collect())
        .collect();
    (planes, rows as usize, cols as usize)
}
