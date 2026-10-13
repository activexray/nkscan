//! Against LS-50 captures: the thumbnail, the 8Eh records, and Nikon Scan's 8Fh table

use super::*;
use std::{fs, path::PathBuf};

struct Fixture {
    planes: Vec<Vec<u16>>,
    rows: usize,
    cols: usize,
    bits: u8,
    film: Film,
    frames: usize,
    line_pitch: u32,
    perfs: PerfInformation,
    nikon: BoundaryType2,
}

fn fixture(name: &str) -> Fixture {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/framing")
        .join(name);
    let (planes, rows, cols) =
        crate::scan::boundary::fixtures::read_tiff(&dir.join("thumbnail.tiff"));

    let meta = fs::read_to_string(dir.join("meta.txt")).expect("meta");
    let field = |key: &str| {
        meta.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(' '))
            .unwrap_or_else(|| panic!("{key} in meta.txt"))
            .to_owned()
    };
    let film = match field("film").as_str() {
        "negative" => Film::Negative,
        "positive" => Film::Positive,
        "mono" => Film::MonochromeNegative,
        other => panic!("film {other}"),
    };
    let read = |file: &str| fs::read(dir.join(file)).expect(file);
    Fixture {
        planes,
        rows,
        cols,
        bits: field("bits").parse().expect("bits"),
        film,
        frames: field("frames").parse().expect("frames"),
        line_pitch: field("line_pitch").parse().expect("line_pitch"),
        perfs: PerfInformation::from_bytes(&read("perforation.bin")).expect("8Eh"),
        nikon: BoundaryType2::from_bytes(&read("boundary2.bin")).expect("8Fh"),
    }
}

/// The records, the kept lines, each frame's first line, and Nikon's table
fn found(name: &str) -> (PerfInformation, Vec<usize>, Vec<usize>, BoundaryType2) {
    let f = fixture(name);
    let image = Image {
        colors: f.planes.iter().map(Vec::as_slice).collect(),
        ir: &[],
        rows: f.rows,
        cols: f.cols,
        stride: f.cols,
        bits: f.bits,
    };
    let starts = frames(&image, &f.perfs, 4000, f.line_pitch, f.film, f.frames).expect("frames");
    let kept = resample(&f.perfs, 4000, f.line_pitch);
    (f.perfs, kept, starts, f.nikon)
}

fn lines(starts: &[usize]) -> impl Iterator<Item = i64> + '_ {
    starts.iter().map(|&s| s as i64)
}

fn frames_of(name: &str) -> (BoundaryType2, BoundaryType2) {
    let (perfs, kept, starts, nikon) = found(name);
    (table(&perfs, 4000, &kept, lines(&starts)), nikon)
}

fn tops(table: &BoundaryType2) -> Vec<u32> {
    table.frames.iter().map(|f| f.top).collect()
}

macro_rules! matches_nikon {
    ($test:ident, $name:literal) => {
        #[test]
        fn $test() {
            let (ours, nikon) = frames_of($name);
            assert_eq!(tops(&ours), tops(&nikon));
            assert_eq!(ours.to_bytes().unwrap(), nikon.to_bytes().unwrap());
        }
    };
}

matches_nikon!(
    a_color_negative_strip_is_framed_as_nikon_frames_it,
    "coolscan_v_thumbnail_preview_scan"
);
matches_nikon!(
    a_slide_strip_is_framed_as_nikon_frames_it,
    "coolscan_v_slide"
);
matches_nikon!(
    a_mono_negative_strip_is_framed_as_nikon_frames_it,
    "coolscan_v_bw"
);
matches_nikon!(
    a_thin_negative_strip_is_framed_as_nikon_frames_it,
    "coolscan_v_thin_neg_redo"
);
matches_nikon!(
    a_strip_starting_on_its_first_frame_is_framed_as_nikon_frames_it,
    "coolscan_v_frame_test"
);

/// A nudge stays after line 0 and before the last record, at most 144 lines
#[test]
fn a_frame_can_be_nudged_within_nikons_range() {
    let (_, kept, starts, _) = found("coolscan_v_thin_neg_redo");
    for (&s, line) in starts.iter().zip(lines(&starts)) {
        let r = nudge_range(kept.len(), s);
        assert_eq!(*r.start(), -line.min(144));
        assert!(*r.end() <= 144, "frame at {s}: {r:?}");
    }
}

/// A nudge moves only that frame, to the record of its new line
#[test]
fn a_nudge_moves_one_frame_onto_its_new_record() {
    let (perfs, kept, starts, _) = found("coolscan_v_thumbnail_preview_scan");
    let before = table(&perfs, 4000, &kept, lines(&starts));
    let nudged = lines(&starts)
        .enumerate()
        .map(|(i, l)| if i == 2 { l + 2 } else { l });
    let after = table(&perfs, 4000, &kept, nudged);
    let moved: Vec<_> = before
        .frames
        .iter()
        .zip(&after.frames)
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(moved.len(), 1);
    assert!(moved[0].1.top > moved[0].0.top);
}
