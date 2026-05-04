//! Integration tests for the demuxer's `Chapters` parsing.
//!
//! Builds a minimal MKV containing a `Chapters` master with one
//! `EditionEntry` and three `ChapterAtom`s, opens it through the demuxer,
//! and checks that the chapter titles + start/end times surface in the
//! demuxer's `metadata()` view as
//!
//! ```text
//!   chapter:1:start_ms / chapter:1:end_ms / chapter:1:title
//!   chapter:2:start_ms / chapter:2:end_ms / chapter:2:title
//!   chapter:3:start_ms /                  / chapter:3:title
//! ```
//!
//! Chapter atoms with no `ChapterTimeEnd` should still surface their start
//! + title (matches what ffprobe shows for files where end is implicit).
//!
//! Reference: <https://www.matroska.org/technical/chapters.html>

use std::io::Cursor;

use oxideav_core::ReadSeek;
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

fn elem_uint(id: u32, value: u64) -> Vec<u8> {
    let n = if value == 0 {
        1
    } else {
        (64 - value.leading_zeros()).div_ceil(8) as usize
    };
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(n as u64, 0));
    for i in (0..n).rev() {
        out.push(((value >> (i * 8)) & 0xFF) as u8);
    }
    out
}

fn elem_str(id: u32, s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(s.len() as u64, 0));
    out.extend_from_slice(s.as_bytes());
    out
}

fn elem_float_be_f64(id: u32, value: f64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(8, 0));
    out.extend_from_slice(&value.to_be_bytes());
    out
}

fn elem_master(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

fn simple_block(track: u8, tc_offset: i16, keyframe: bool, payload: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(if keyframe { 0x80 } else { 0x00 });
    body.push(payload);
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(ids::SIMPLE_BLOCK));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(&body);
    out
}

/// Build a chapter atom with the given UID, title and start/end ns.
fn chapter_atom(uid: u64, start_ns: u64, end_ns: Option<u64>, title: &str) -> Vec<u8> {
    let mut atom = Vec::new();
    atom.extend_from_slice(&elem_uint(ids::CHAPTER_UID, uid));
    atom.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, start_ns));
    if let Some(e) = end_ns {
        atom.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_END, e));
    }
    let mut display = Vec::new();
    display.extend_from_slice(&elem_str(ids::CHAP_STRING, title));
    display.extend_from_slice(&elem_str(ids::CHAP_LANGUAGE, "eng"));
    atom.extend_from_slice(&elem_master(ids::CHAPTER_DISPLAY, &display));
    elem_master(ids::CHAPTER_ATOM, &atom)
}

/// Build a self-contained MKV with three chapter atoms.
fn build_mkv_with_chapters() -> Vec<u8> {
    // EBML header.
    let mut ebml_body = Vec::new();
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    ebml_body.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    let ebml_header = elem_master(ids::EBML_HEADER, &ebml_body);

    // Info: 1 ms timecode scale, 3 s duration.
    let mut info_body = Vec::new();
    info_body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info_body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 3000.0));
    let info = elem_master(ids::INFO, &info_body);

    // Tracks: a single PCM track so the demuxer accepts the file.
    let mut track_body = Vec::new();
    track_body.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_UID, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    track_body.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio_body.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio_body.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    track_body.extend_from_slice(&elem_master(ids::AUDIO, &audio_body));
    let track_entry = elem_master(ids::TRACK_ENTRY, &track_body);
    let tracks = elem_master(ids::TRACKS, &track_entry);

    // Chapters: one EditionEntry with three atoms.
    //   1: 0 ms .. 1000 ms   "Intro"
    //   2: 1000 ms .. 2000 ms "Verse"
    //   3: 2000 ms .. (no end) "Outro"
    let mut edition_body = Vec::new();
    edition_body.extend_from_slice(&chapter_atom(0xC1, 0, Some(1_000_000_000), "Intro"));
    edition_body.extend_from_slice(&chapter_atom(
        0xC2,
        1_000_000_000,
        Some(2_000_000_000),
        "Verse",
    ));
    edition_body.extend_from_slice(&chapter_atom(0xC3, 2_000_000_000, None, "Outro"));
    let edition = elem_master(ids::EDITION_ENTRY, &edition_body);
    let chapters = elem_master(ids::CHAPTERS, &edition);

    // One cluster so the demuxer is happy (no clusters → "no clusters" error).
    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    // Segment body: Info ++ Tracks ++ Chapters ++ Cluster.
    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&chapters);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header);
    out.extend_from_slice(&segment);
    out
}

#[test]
fn chapters_surface_in_metadata() {
    let bytes = build_mkv_with_chapters();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let md: Vec<(String, String)> = dmx.metadata().to_vec();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());

    assert_eq!(get("chapter:1:title").as_deref(), Some("Intro"));
    assert_eq!(get("chapter:1:start_ms").as_deref(), Some("0"));
    assert_eq!(get("chapter:1:end_ms").as_deref(), Some("1000"));

    assert_eq!(get("chapter:2:title").as_deref(), Some("Verse"));
    assert_eq!(get("chapter:2:start_ms").as_deref(), Some("1000"));
    assert_eq!(get("chapter:2:end_ms").as_deref(), Some("2000"));

    assert_eq!(get("chapter:3:title").as_deref(), Some("Outro"));
    assert_eq!(get("chapter:3:start_ms").as_deref(), Some("2000"));
    // No ChapterTimeEnd → no end_ms key.
    assert!(
        get("chapter:3:end_ms").is_none(),
        "atom without ChapterTimeEnd should not emit end_ms"
    );
}
