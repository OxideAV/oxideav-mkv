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

use oxideav_core::{Demuxer, ReadSeek};
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

fn elem_bin(id: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(data.len() as u64, 0));
    out.extend_from_slice(data);
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

#[test]
fn typed_chapters_match_flat_metadata() {
    // The new typed `chapters()` accessor must surface the same start/end
    // and titles as the legacy flat metadata view, plus the structured
    // edition grouping.
    let bytes = build_mkv_with_chapters();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver)
        .expect("demux open_typed");

    let eds = dmx.chapters();
    assert_eq!(eds.len(), 1, "exactly one EditionEntry");
    let ed = &eds[0];
    // Default edition (flags absent → false).
    assert!(!ed.default);
    assert!(!ed.ordered);
    assert!(ed.uid.is_none(), "no EditionUID was written");
    assert_eq!(ed.chapters.len(), 3, "three top-level atoms");

    let c1 = &ed.chapters[0];
    assert_eq!(c1.index, 1);
    assert_eq!(c1.uid, Some(0xC1));
    assert_eq!(c1.time_start_ns, 0);
    assert_eq!(c1.time_end_ns, Some(1_000_000_000));
    assert!(!c1.hidden);
    assert!(c1.string_uid.is_none());
    assert!(c1.children.is_empty());
    // ChapterFlagEnabled was absent → spec default = 1 → materialised true.
    assert!(c1.enabled);
    assert!(c1.segment_uuid.is_none());
    assert!(c1.segment_edition_uid.is_none());
    assert!(c1.physical_equiv.is_none());
    assert_eq!(c1.displays.len(), 1);
    assert_eq!(c1.displays[0].string, "Intro");
    assert_eq!(c1.displays[0].language, "eng");
    assert!(c1.displays[0].language_bcp47.is_none());
    assert!(c1.displays[0].country.is_none());

    // Atom 3 has no ChapterTimeEnd.
    let c3 = &ed.chapters[2];
    assert_eq!(c3.index, 3);
    assert_eq!(c3.time_end_ns, None);
    assert_eq!(c3.displays[0].string, "Outro");
}

/// Build an EBML header valid for an MKV file.
fn build_ebml_header() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    body.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    body.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    body.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    body.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    elem_master(ids::EBML_HEADER, &body)
}

/// Build the minimum Info + Tracks + one Cluster that the demuxer needs
/// to accept a file. Returns the concatenated bytes ready to drop into
/// a Segment body alongside a `Chapters` element.
fn build_min_segment_skeleton() -> Vec<u8> {
    let mut info_body = Vec::new();
    info_body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info_body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 3000.0));
    let info = elem_master(ids::INFO, &info_body);

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

    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    // Caller appends Chapters between tracks and cluster.
    let mut out = Vec::new();
    out.extend_from_slice(&info);
    out.extend_from_slice(&tracks);
    // Caller inserts Chapters here.
    out.extend_from_slice(&cluster);
    out
}

#[test]
fn typed_chapters_capture_multilingual_displays_and_nesting() {
    // Build a Chapters element with:
    //   * one ordered, default EditionEntry carrying an EditionUID
    //   * one parent ChapterAtom with two ChapterDisplay rows (eng + fra),
    //     a ChapterStringUID, ChapterFlagHidden = 1, and one nested child
    //     atom with a single display in fr-CA via ChapLanguageBCP47.
    let ebml_header = build_ebml_header();

    let mut info_body = Vec::new();
    info_body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info_body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 3000.0));
    let info = elem_master(ids::INFO, &info_body);

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
    let tracks = elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &track_body));

    // Child atom: nested under parent. Single display in fr-CA via BCP-47.
    let mut child_display = Vec::new();
    child_display.extend_from_slice(&elem_str(ids::CHAP_STRING, "Sous-chapitre"));
    // ChapLanguage MUST be ignored when ChapLanguageBCP47 is present, but
    // it's still legal to write — verify the parser preserves both raw.
    child_display.extend_from_slice(&elem_str(ids::CHAP_LANGUAGE, "fre"));
    child_display.extend_from_slice(&elem_str(ids::CHAP_LANGUAGE_BCP47, "fr-CA"));
    let mut child_atom = Vec::new();
    child_atom.extend_from_slice(&elem_uint(ids::CHAPTER_UID, 0x10));
    child_atom.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, 500_000_000));
    child_atom.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_END, 800_000_000));
    child_atom.extend_from_slice(&elem_master(ids::CHAPTER_DISPLAY, &child_display));
    let child_atom = elem_master(ids::CHAPTER_ATOM, &child_atom);

    // Parent atom: two displays (eng + fra+country) + hidden flag + string_uid + nested child.
    let mut display_eng = Vec::new();
    display_eng.extend_from_slice(&elem_str(ids::CHAP_STRING, "Intro"));
    display_eng.extend_from_slice(&elem_str(ids::CHAP_LANGUAGE, "eng"));

    let mut display_fra = Vec::new();
    display_fra.extend_from_slice(&elem_str(ids::CHAP_STRING, "Introduction"));
    display_fra.extend_from_slice(&elem_str(ids::CHAP_LANGUAGE, "fre"));
    display_fra.extend_from_slice(&elem_str(ids::CHAP_COUNTRY, "FR"));

    let mut parent_atom = Vec::new();
    parent_atom.extend_from_slice(&elem_uint(ids::CHAPTER_UID, 0xAA));
    parent_atom.extend_from_slice(&elem_str(ids::CHAPTER_STRING_UID, "cue-intro-1"));
    parent_atom.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, 0));
    parent_atom.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_END, 1_000_000_000));
    parent_atom.extend_from_slice(&elem_uint(ids::CHAPTER_FLAG_HIDDEN, 1));
    parent_atom.extend_from_slice(&elem_master(ids::CHAPTER_DISPLAY, &display_eng));
    parent_atom.extend_from_slice(&elem_master(ids::CHAPTER_DISPLAY, &display_fra));
    parent_atom.extend_from_slice(&child_atom);
    let parent_atom = elem_master(ids::CHAPTER_ATOM, &parent_atom);

    // EditionEntry with both flags set.
    let mut edition_body = Vec::new();
    edition_body.extend_from_slice(&elem_uint(ids::EDITION_UID, 0xBEEF));
    edition_body.extend_from_slice(&elem_uint(ids::EDITION_FLAG_DEFAULT, 1));
    edition_body.extend_from_slice(&elem_uint(ids::EDITION_FLAG_ORDERED, 1));
    edition_body.extend_from_slice(&parent_atom);
    let chapters = elem_master(
        ids::CHAPTERS,
        &elem_master(ids::EDITION_ENTRY, &edition_body),
    );

    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&chapters);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&ebml_header);
    bytes.extend_from_slice(&segment);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver)
        .expect("demux open_typed");

    // Edition.
    let eds = dmx.chapters();
    assert_eq!(eds.len(), 1);
    let ed = &eds[0];
    assert_eq!(ed.uid, Some(0xBEEF));
    assert!(ed.default);
    assert!(ed.ordered);
    assert_eq!(ed.chapters.len(), 1, "one top-level atom (the parent)");

    // Parent atom.
    let parent = &ed.chapters[0];
    assert_eq!(parent.index, 1, "depth-first: parent gets index 1");
    assert_eq!(parent.uid, Some(0xAA));
    assert_eq!(parent.string_uid.as_deref(), Some("cue-intro-1"));
    assert_eq!(parent.time_start_ns, 0);
    assert_eq!(parent.time_end_ns, Some(1_000_000_000));
    assert!(parent.hidden);
    assert_eq!(parent.displays.len(), 2, "eng + fra");

    let eng = &parent.displays[0];
    assert_eq!(eng.string, "Intro");
    assert_eq!(eng.language, "eng");
    assert!(eng.language_bcp47.is_none());
    assert!(eng.country.is_none());

    let fra = &parent.displays[1];
    assert_eq!(fra.string, "Introduction");
    assert_eq!(fra.language, "fre");
    assert!(fra.language_bcp47.is_none());
    assert_eq!(fra.country.as_deref(), Some("FR"));

    // Nested child.
    assert_eq!(parent.children.len(), 1);
    let child = &parent.children[0];
    assert_eq!(child.index, 2, "depth-first: child gets index 2");
    assert_eq!(child.uid, Some(0x10));
    assert_eq!(child.time_start_ns, 500_000_000);
    assert_eq!(child.time_end_ns, Some(800_000_000));
    assert!(!child.hidden);
    assert_eq!(child.displays.len(), 1);
    assert_eq!(child.displays[0].string, "Sous-chapitre");
    // ChapLanguageBCP47 present — parser preserves both raw fields; per
    // RFC 9559 §5.1.7.1.4.12 the consumer ignores `language` when bcp47
    // is set. The typed accessor surfaces both so the consumer can make
    // that decision.
    assert_eq!(child.displays[0].language, "fre");
    assert_eq!(child.displays[0].language_bcp47.as_deref(), Some("fr-CA"));

    // Nested atom also surfaces in the flat metadata view under its
    // depth-first 1-based index.
    let md: Vec<(String, String)> = dmx.metadata().to_vec();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(get("chapter:1:start_ms").as_deref(), Some("0"));
    assert_eq!(get("chapter:1:end_ms").as_deref(), Some("1000"));
    assert_eq!(get("chapter:1:title").as_deref(), Some("Intro"));
    assert_eq!(get("chapter:2:start_ms").as_deref(), Some("500"));
    assert_eq!(get("chapter:2:end_ms").as_deref(), Some("800"));
    assert_eq!(get("chapter:2:title").as_deref(), Some("Sous-chapitre"));
}

#[test]
fn typed_chapters_capture_enabled_flag_segment_link_and_physical_equiv() {
    // Build a Chapters element exercising the rest of RFC 9559 §5.1.7.1.4:
    //   * ChapterFlagEnabled = 0 (default is 1; verify the override sticks
    //     and the spec-default surfaces as `true` on a sibling atom that
    //     omits the element).
    //   * ChapterSegmentUUID (16 raw bytes) + ChapterSegmentEditionUID for
    //     a Medium-Linking atom (§17.2).
    //   * ChapterPhysicalEquiv = 60 ("DVD" per §20.4).
    //
    // The synthetic file carries two top-level atoms:
    //   1. Linked atom: enabled=0, segment_uuid=<16 B>, segment_edition_uid=0xED,
    //      physical_equiv=60, no displays.
    //   2. Vanilla atom: only the mandatory fields — should default to
    //      enabled=true with the three optional fields all None.
    let ebml_header = build_ebml_header();
    let mut info_body = Vec::new();
    info_body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info_body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 3000.0));
    let info = elem_master(ids::INFO, &info_body);

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
    let tracks = elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &track_body));

    // Linked atom (#1).
    let linked_uuid: [u8; 16] = [
        0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x0F, 0xED, 0xCB, 0xA9, 0x87, 0x65, 0x43,
        0x21,
    ];
    let mut linked_atom = Vec::new();
    linked_atom.extend_from_slice(&elem_uint(ids::CHAPTER_UID, 0xA1));
    linked_atom.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, 0));
    linked_atom.extend_from_slice(&elem_uint(ids::CHAPTER_FLAG_ENABLED, 0));
    // Raw 16-byte UUID via elem_master-style header (binary payload, not master).
    let mut uuid_elem = Vec::new();
    uuid_elem.extend_from_slice(&write_element_id(ids::CHAPTER_SEGMENT_UUID));
    uuid_elem.extend_from_slice(&write_vint(linked_uuid.len() as u64, 0));
    uuid_elem.extend_from_slice(&linked_uuid);
    linked_atom.extend_from_slice(&uuid_elem);
    linked_atom.extend_from_slice(&elem_uint(ids::CHAPTER_SEGMENT_EDITION_UID, 0xED));
    linked_atom.extend_from_slice(&elem_uint(ids::CHAPTER_PHYSICAL_EQUIV, 60));
    let linked_atom = elem_master(ids::CHAPTER_ATOM, &linked_atom);

    // Vanilla atom (#2) — only the mandatory fields, plus one display so the
    // flat metadata view still captures something for index 2.
    let mut vanilla_display = Vec::new();
    vanilla_display.extend_from_slice(&elem_str(ids::CHAP_STRING, "Plain"));
    vanilla_display.extend_from_slice(&elem_str(ids::CHAP_LANGUAGE, "eng"));
    let mut vanilla_atom = Vec::new();
    vanilla_atom.extend_from_slice(&elem_uint(ids::CHAPTER_UID, 0xA2));
    vanilla_atom.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, 1_000_000_000));
    vanilla_atom.extend_from_slice(&elem_master(ids::CHAPTER_DISPLAY, &vanilla_display));
    let vanilla_atom = elem_master(ids::CHAPTER_ATOM, &vanilla_atom);

    let mut edition_body = Vec::new();
    edition_body.extend_from_slice(&linked_atom);
    edition_body.extend_from_slice(&vanilla_atom);
    let chapters = elem_master(
        ids::CHAPTERS,
        &elem_master(ids::EDITION_ENTRY, &edition_body),
    );

    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&chapters);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&ebml_header);
    bytes.extend_from_slice(&segment);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver)
        .expect("demux open_typed");

    let eds = dmx.chapters();
    assert_eq!(eds.len(), 1);
    let ed = &eds[0];
    assert_eq!(ed.chapters.len(), 2);

    let linked = &ed.chapters[0];
    assert_eq!(linked.uid, Some(0xA1));
    assert!(!linked.enabled, "ChapterFlagEnabled = 0 must surface");
    assert_eq!(
        linked.segment_uuid.as_deref(),
        Some(&linked_uuid[..]),
        "16-byte ChapterSegmentUUID preserved verbatim"
    );
    assert_eq!(linked.segment_edition_uid, Some(0xED));
    assert_eq!(linked.physical_equiv, Some(60));
    assert!(linked.displays.is_empty());

    let vanilla = &ed.chapters[1];
    assert_eq!(vanilla.uid, Some(0xA2));
    assert!(
        vanilla.enabled,
        "ChapterFlagEnabled spec default = 1 must materialise as true"
    );
    assert!(vanilla.segment_uuid.is_none());
    assert!(vanilla.segment_edition_uid.is_none());
    assert!(vanilla.physical_equiv.is_none());
    assert_eq!(vanilla.displays.len(), 1);
    assert_eq!(vanilla.displays[0].string, "Plain");
}

#[test]
fn typed_chapters_drop_segment_edition_uid_zero() {
    // RFC 9559 §5.1.7.1.4.7 forbids 0 as a value for ChapterSegmentEditionUID
    // ("range: not 0"). A file that nevertheless carries 0 must surface as
    // `None` — never as `Some(0)` — so consumers can use the option as the
    // sole presence check.
    let ebml_header = build_ebml_header();
    let mut info_body = Vec::new();
    info_body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info_body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 3000.0));
    let info = elem_master(ids::INFO, &info_body);

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
    let tracks = elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &track_body));

    let mut atom_body = Vec::new();
    atom_body.extend_from_slice(&elem_uint(ids::CHAPTER_UID, 0xB1));
    atom_body.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, 0));
    atom_body.extend_from_slice(&elem_uint(ids::CHAPTER_SEGMENT_EDITION_UID, 0));
    let atom = elem_master(ids::CHAPTER_ATOM, &atom_body);
    let chapters = elem_master(ids::CHAPTERS, &elem_master(ids::EDITION_ENTRY, &atom));

    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&chapters);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&ebml_header);
    bytes.extend_from_slice(&segment);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver)
        .expect("demux open_typed");

    let eds = dmx.chapters();
    assert_eq!(eds.len(), 1);
    let ed = &eds[0];
    assert_eq!(ed.chapters.len(), 1);
    assert!(
        ed.chapters[0].segment_edition_uid.is_none(),
        "ChapterSegmentEditionUID=0 must surface as None (spec range: not 0)"
    );
}

#[test]
fn typed_chapters_empty_when_no_chapters_element() {
    // A file with no Chapters element at all — `chapters()` must return
    // an empty slice.
    let ebml_header = build_ebml_header();
    // `build_min_segment_skeleton` returns info + tracks + cluster with
    // no Chapters element — exactly the input we need to exercise the
    // "no chapters" branch.
    let segment = elem_master(ids::SEGMENT, &build_min_segment_skeleton());
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&ebml_header);
    bytes.extend_from_slice(&segment);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver)
        .expect("demux open_typed");

    assert!(
        dmx.chapters().is_empty(),
        "file with no Chapters element should expose no editions"
    );
}

#[test]
fn typed_chapters_capture_chap_process_subtree() {
    // Exercise the ChapProcess sub-tree (RFC 9559 §5.1.7.1.4.14–19):
    //   * Atom 1 carries a DVD-menu ChapProcess (ChapProcessCodecID = 1)
    //     with ChapProcessPrivate and two ChapProcessCommands, each with a
    //     distinct ChapProcessTime and binary ChapProcessData payload.
    //   * Atom 2 carries a Matroska-Script ChapProcess that omits the
    //     codec id entirely (must materialise the spec default of 0) and
    //     omits ChapProcessPrivate, with a single command that omits
    //     ChapProcessTime (must materialise the spec default of 0).
    // The roundtrip: synthesize the EBML, demux, and assert the typed
    // accessor surfaces every payload byte-for-byte.
    let ebml_header = build_ebml_header();

    let mut info_body = Vec::new();
    info_body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info_body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 3000.0));
    let info = elem_master(ids::INFO, &info_body);

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
    let tracks = elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &track_body));

    // --- Atom 1: DVD-menu ChapProcess with two commands. ---
    let cmd1_during = {
        let mut b = Vec::new();
        b.extend_from_slice(&elem_uint(
            ids::CHAP_PROCESS_TIME,
            ids::CHAP_PROCESS_TIME_DURING,
        ));
        b.extend_from_slice(&elem_bin(ids::CHAP_PROCESS_DATA, &[0xDE, 0xAD, 0xBE, 0xEF]));
        elem_master(ids::CHAP_PROCESS_COMMAND, &b)
    };
    let cmd1_after = {
        let mut b = Vec::new();
        b.extend_from_slice(&elem_uint(
            ids::CHAP_PROCESS_TIME,
            ids::CHAP_PROCESS_TIME_AFTER,
        ));
        b.extend_from_slice(&elem_bin(ids::CHAP_PROCESS_DATA, &[0x01, 0x02, 0x03]));
        elem_master(ids::CHAP_PROCESS_COMMAND, &b)
    };
    let chap_process1 = {
        let mut b = Vec::new();
        b.extend_from_slice(&elem_uint(
            ids::CHAP_PROCESS_CODEC_ID,
            ids::CHAP_PROCESS_CODEC_DVD_MENU,
        ));
        b.extend_from_slice(&elem_bin(ids::CHAP_PROCESS_PRIVATE, &[0xAA, 0xBB]));
        b.extend_from_slice(&cmd1_during);
        b.extend_from_slice(&cmd1_after);
        elem_master(ids::CHAP_PROCESS, &b)
    };
    let atom1 = {
        let mut a = Vec::new();
        a.extend_from_slice(&elem_uint(ids::CHAPTER_UID, 0xA1));
        a.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, 0));
        a.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_END, 1_000_000_000));
        let mut disp = Vec::new();
        disp.extend_from_slice(&elem_str(ids::CHAP_STRING, "Menu"));
        disp.extend_from_slice(&elem_str(ids::CHAP_LANGUAGE, "eng"));
        a.extend_from_slice(&elem_master(ids::CHAPTER_DISPLAY, &disp));
        a.extend_from_slice(&chap_process1);
        elem_master(ids::CHAPTER_ATOM, &a)
    };

    // --- Atom 2: Matroska-Script ChapProcess relying on defaults. ---
    let cmd2 = {
        // No ChapProcessTime → spec default 0.
        let mut b = Vec::new();
        b.extend_from_slice(&elem_bin(ids::CHAP_PROCESS_DATA, &[0xFF]));
        elem_master(ids::CHAP_PROCESS_COMMAND, &b)
    };
    let chap_process2 = {
        // No ChapProcessCodecID → spec default 0; no ChapProcessPrivate.
        elem_master(ids::CHAP_PROCESS, &cmd2)
    };
    let atom2 = {
        let mut a = Vec::new();
        a.extend_from_slice(&elem_uint(ids::CHAPTER_UID, 0xA2));
        a.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, 1_000_000_000));
        a.extend_from_slice(&chap_process2);
        elem_master(ids::CHAPTER_ATOM, &a)
    };

    let mut edition_body = Vec::new();
    edition_body.extend_from_slice(&atom1);
    edition_body.extend_from_slice(&atom2);
    let chapters = elem_master(
        ids::CHAPTERS,
        &elem_master(ids::EDITION_ENTRY, &edition_body),
    );

    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&chapters);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&ebml_header);
    bytes.extend_from_slice(&segment);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver)
        .expect("demux open_typed");

    let eds = dmx.chapters();
    assert_eq!(eds.len(), 1);
    let ed = &eds[0];
    assert_eq!(ed.chapters.len(), 2, "two top-level atoms");

    // Atom 1: DVD-menu ChapProcess, private + two commands.
    let c1 = &ed.chapters[0];
    assert_eq!(c1.uid, Some(0xA1));
    assert_eq!(c1.chap_processes.len(), 1, "one ChapProcess on atom 1");
    let p1 = &c1.chap_processes[0];
    assert_eq!(p1.codec_id, ids::CHAP_PROCESS_CODEC_DVD_MENU);
    assert_eq!(p1.private.as_deref(), Some(&[0xAA, 0xBB][..]));
    assert_eq!(p1.commands.len(), 2, "two ChapProcessCommands");
    assert_eq!(p1.commands[0].time, ids::CHAP_PROCESS_TIME_DURING);
    assert_eq!(p1.commands[0].data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    assert_eq!(p1.commands[1].time, ids::CHAP_PROCESS_TIME_AFTER);
    assert_eq!(p1.commands[1].data, vec![0x01, 0x02, 0x03]);

    // Atom 2: Matroska-Script ChapProcess relying on spec defaults.
    let c2 = &ed.chapters[1];
    assert_eq!(c2.uid, Some(0xA2));
    assert_eq!(c2.chap_processes.len(), 1, "one ChapProcess on atom 2");
    let p2 = &c2.chap_processes[0];
    // ChapProcessCodecID omitted → spec default 0 (Matroska Script).
    assert_eq!(p2.codec_id, ids::CHAP_PROCESS_CODEC_MATROSKA_SCRIPT);
    assert!(p2.private.is_none(), "no ChapProcessPrivate written");
    assert_eq!(p2.commands.len(), 1);
    // ChapProcessTime omitted → spec default 0 (during the whole chapter).
    assert_eq!(p2.commands[0].time, ids::CHAP_PROCESS_TIME_DURING);
    assert_eq!(p2.commands[0].data, vec![0xFF]);
}
