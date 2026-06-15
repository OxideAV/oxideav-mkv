//! Linked-Segment Info elements — `SegmentUUID` / `SegmentFilename` /
//! `PrevUUID` / `PrevFilename` / `NextUUID` / `NextFilename` /
//! `SegmentFamily` / `ChapterTranslate` (RFC 9559 §5.1.2.1..§5.1.2.8 +
//! Section 17).
//!
//! These sit directly on the `Segment\Info` master and tie a Segment to
//! the other Segments of a Linked Segment. We hand-build minimal MKV
//! buffers exercising the standalone case, a Hard-Linked chain member,
//! multiple SegmentFamily UIDs, and the `ChapterTranslate` sub-tree, then
//! assert [`MkvDemuxer::segment_linking`] surfaces them faithfully.

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

fn elem_bin(id: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(bytes.len() as u64, 0));
    out.extend_from_slice(bytes);
    out
}

fn elem_master(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

fn ebml_header() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    b.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    elem_master(ids::EBML_HEADER, &b)
}

fn tracks_body() -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, 0xA1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio = Vec::new();
    audio.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    tb.extend_from_slice(&elem_master(ids::AUDIO, &audio));
    elem_master(ids::TRACK_ENTRY, &tb)
}

fn simple_block(track: u8, payload: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&0i16.to_be_bytes());
    body.push(0x80); // keyframe
    body.push(payload);
    elem_master(ids::SIMPLE_BLOCK, &body)
}

fn cluster_body() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    body.extend_from_slice(&simple_block(1, 0xAA));
    body
}

/// Wrap an Info body in a full file (EBML header + Segment{Info, Tracks,
/// Cluster}).
fn build_file(info_body: &[u8]) -> Vec<u8> {
    let info = elem_master(ids::INFO, info_body);
    let tracks = elem_master(ids::TRACKS, &tracks_body());
    let cluster = elem_master(ids::CLUSTER, &cluster_body());

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

fn open(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

const UID_SELF: [u8; 16] = [
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x01,
];
const UID_PREV: [u8; 16] = [0xA0; 16];
const UID_NEXT: [u8; 16] = [0xB0; 16];
const UID_FAM1: [u8; 16] = [0xC0; 16];
const UID_FAM2: [u8; 16] = [0xD0; 16];

#[test]
fn standalone_segment_has_empty_linking() {
    // An Info with only TimecodeScale — no linking elements at all.
    let mut info = Vec::new();
    info.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    let dmx = open(build_file(&info));

    let link = dmx.segment_linking();
    assert!(
        link.is_empty(),
        "standalone Segment must report empty linking"
    );
    assert!(!link.is_hard_linked());
    assert!(link.segment_uuid.is_none());
    assert!(link.families.is_empty());
    assert!(link.chapter_translates.is_empty());
}

#[test]
fn hard_linked_chain_member_surfaces_neighbours() {
    // A middle-of-chain Segment: declares its own UID, both neighbour
    // UIDs, and display filenames for all three.
    let mut info = Vec::new();
    info.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info.extend_from_slice(&elem_bin(ids::SEGMENT_UID, &UID_SELF));
    info.extend_from_slice(&elem_str(ids::SEGMENT_FILENAME, "part2.mkv"));
    info.extend_from_slice(&elem_bin(ids::PREV_UID, &UID_PREV));
    info.extend_from_slice(&elem_str(ids::PREV_FILENAME, "part1.mkv"));
    info.extend_from_slice(&elem_bin(ids::NEXT_UID, &UID_NEXT));
    info.extend_from_slice(&elem_str(ids::NEXT_FILENAME, "part3.mkv"));
    let dmx = open(build_file(&info));

    let link = dmx.segment_linking();
    assert!(!link.is_empty());
    assert!(link.is_hard_linked());
    assert_eq!(link.segment_uuid.as_deref(), Some(&UID_SELF[..]));
    assert_eq!(link.segment_filename.as_deref(), Some("part2.mkv"));
    assert_eq!(link.prev_uuid.as_deref(), Some(&UID_PREV[..]));
    assert_eq!(link.prev_filename.as_deref(), Some("part1.mkv"));
    assert_eq!(link.next_uuid.as_deref(), Some(&UID_NEXT[..]));
    assert_eq!(link.next_filename.as_deref(), Some("part3.mkv"));
}

#[test]
fn multiple_segment_families_preserved_in_order() {
    // SegmentFamily is unbounded — a Segment may belong to several.
    let mut info = Vec::new();
    info.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info.extend_from_slice(&elem_bin(ids::SEGMENT_FAMILY, &UID_FAM1));
    info.extend_from_slice(&elem_bin(ids::SEGMENT_FAMILY, &UID_FAM2));
    let dmx = open(build_file(&info));

    let link = dmx.segment_linking();
    assert!(!link.is_empty());
    assert!(
        !link.is_hard_linked(),
        "family membership is not hard linking"
    );
    assert_eq!(link.families.len(), 2);
    assert_eq!(link.families[0], UID_FAM1);
    assert_eq!(link.families[1], UID_FAM2);
}

#[test]
fn chapter_translate_subtree_parsed() {
    // One ChapterTranslate master with the mandatory ID + Codec and two
    // edition UIDs, plus a second master with no edition UID (applies to
    // all editions).
    let mut t1 = Vec::new();
    t1.extend_from_slice(&elem_bin(
        ids::CHAPTER_TRANSLATE_ID,
        &[0xDE, 0xAD, 0xBE, 0xEF],
    ));
    t1.extend_from_slice(&elem_uint(
        ids::CHAPTER_TRANSLATE_CODEC,
        ids::CHAP_PROCESS_CODEC_DVD_MENU,
    ));
    t1.extend_from_slice(&elem_uint(ids::CHAPTER_TRANSLATE_EDITION_UID, 7));
    t1.extend_from_slice(&elem_uint(ids::CHAPTER_TRANSLATE_EDITION_UID, 9));

    let mut t2 = Vec::new();
    t2.extend_from_slice(&elem_bin(ids::CHAPTER_TRANSLATE_ID, &[0x01]));
    t2.extend_from_slice(&elem_uint(
        ids::CHAPTER_TRANSLATE_CODEC,
        ids::CHAP_PROCESS_CODEC_MATROSKA_SCRIPT,
    ));

    let mut info = Vec::new();
    info.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info.extend_from_slice(&elem_master(ids::CHAPTER_TRANSLATE, &t1));
    info.extend_from_slice(&elem_master(ids::CHAPTER_TRANSLATE, &t2));
    let dmx = open(build_file(&info));

    let ct = &dmx.segment_linking().chapter_translates;
    assert_eq!(ct.len(), 2);

    assert_eq!(ct[0].id, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    assert_eq!(ct[0].codec, ids::CHAP_PROCESS_CODEC_DVD_MENU);
    assert_eq!(ct[0].edition_uids, vec![7, 9]);

    assert_eq!(ct[1].id, vec![0x01]);
    assert_eq!(ct[1].codec, ids::CHAP_PROCESS_CODEC_MATROSKA_SCRIPT);
    assert!(
        ct[1].edition_uids.is_empty(),
        "no ChapterTranslateEditionUID means it applies to all editions"
    );
}

#[test]
fn off_length_uid_round_trips_verbatim() {
    // A malformed SegmentUUID of the wrong length must still round-trip
    // verbatim for inspection rather than being silently dropped.
    let stub = [0xAB, 0xCD];
    let mut info = Vec::new();
    info.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info.extend_from_slice(&elem_bin(ids::SEGMENT_UID, &stub));
    let dmx = open(build_file(&info));

    assert_eq!(
        dmx.segment_linking().segment_uuid.as_deref(),
        Some(&stub[..])
    );
}
