//! Against Nikon Scan's 120 search, emulated on the fixture thumbnails

use super::*;
use crate::scan::boundary::fixtures::read_tiff;
use std::{fs, path::PathBuf};

fn format(folder: u8) -> FilmFormat {
    match folder {
        6 => FilmFormat::F645,
        7 => FilmFormat::F66,
        8 => FilmFormat::F67,
        9 => FilmFormat::F68,
        10 => FilmFormat::F69,
        other => panic!("folder {other}"),
    }
}

#[test]
fn every_120_strip_is_searched_as_nikon_searches_it() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/framing/brownie");
    let expected = fs::read_to_string(dir.join("nikon.txt")).expect("expected frames");
    let mut wrong = Vec::new();
    for line in expected.lines().filter(|l| !l.starts_with('#')) {
        let (head, frames) = line.split_once(':').expect("line");
        let [name, folder, polarity] = head.split_whitespace().collect::<Vec<_>>()[..] else {
            panic!("{head}");
        };
        let nikon: Vec<(i64, i64)> = frames
            .split(',')
            .map(|f| {
                let (s, e) = f.trim().split_once(' ').expect("frame");
                (s.parse().expect("start"), e.parse().expect("end"))
            })
            .collect();
        let (planes, rows, cols) = read_tiff(&dir.join(name));
        let image = Image {
            colors: planes.iter().map(Vec::as_slice).collect(),
            ir: &[],
            rows,
            cols,
            stride: cols,
            bits: 16,
        };
        let film = if polarity == "negative" {
            Film::Negative
        } else {
            Film::Positive
        };
        let ours = search(
            &image,
            film,
            format(folder.parse().expect("folder")),
            Some(STRIP_HOLDER),
        );
        if ours.as_deref() != Some(&nikon[..]) {
            wrong.push(format!("{name}: ours {ours:?}, Nikon {nikon:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
