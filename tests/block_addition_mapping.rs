//! Integration tests for the demuxer's `BlockAdditionMapping` parsing
//! (RFC 9559 §5.1.4.1.17).
//!
//! A `BlockAdditionMapping` master on a `TrackEntry` links a `BlockAddID`
//! value (§5.1.3.5.2.3) — the per-frame side-channel selector inside a
//! `BlockGroup > BlockAdditions > BlockMore` — to a registered
//! `BlockAddIDType`. The element is unbounded on the parent: a single
//! `TrackEntry` may carry several mappings, one per (`BlockAddIDType`,
//! `BlockAddIDValue`) pair the track intends to emit.
//!
//! These tests exercise:
//! * an absent `BlockAdditionMapping` master (the common case → empty slice);
//! * a single mapping with all four children present;
//! * the §5.1.4.1.17.3 default `0` (codec-defined) materialised when the
//!   master has no `BlockAddIDType` child;
//! * multiple mappings on one `TrackEntry` preserved in on-disk order;
//! * an out-of-range `stream_index` surfaced as an empty slice.

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

fn elem_bytes(id: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(bytes.len() as u64, 0));
    out.extend_from_slice(bytes);
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
    elem_master(ids::SIMPLE_BLOCK, &body)
}

/// A minimal VP9 video track: number / uid / dims / codec id. `extra_body`
/// is appended verbatim to the `TrackEntry` body, so callers can splice in
/// one or more `BlockAdditionMapping` masters.
fn video_track(number: u64, uid: u64, extra_body: &[u8]) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_VIDEO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "V_VP9"));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::PIXEL_WIDTH, 320));
    v.extend_from_slice(&elem_uint(ids::PIXEL_HEIGHT, 240));
    tb.extend_from_slice(&elem_master(ids::VIDEO, &v));
    tb.extend_from_slice(extra_body);
    tb
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

fn info() -> Vec<u8> {
    let mut ib = Vec::new();
    ib.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    ib.extend_from_slice(&elem_float_be_f64(ids::DURATION, 1000.0));
    elem_master(ids::INFO, &ib)
}

fn one_cluster() -> Vec<u8> {
    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cb.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    elem_master(ids::CLUSTER, &cb)
}

fn assemble(tracks_body: &[u8]) -> Vec<u8> {
    let tracks = elem_master(ids::TRACKS, tracks_body);
    let mut seg = Vec::new();
    seg.extend_from_slice(&info());
    seg.extend_from_slice(&tracks);
    seg.extend_from_slice(&one_cluster());
    let segment = elem_master(ids::SEGMENT, &seg);
    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

fn open(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

/// A `TrackEntry` with no `BlockAdditionMapping` child surfaces as an
/// empty slice — the common case.
#[test]
fn absent_block_addition_mapping_is_empty_slice() {
    let tb = video_track(1, 0xA1, &[]);
    let bytes = assemble(&elem_master(ids::TRACK_ENTRY, &tb));
    let dmx = open(bytes);
    assert!(
        dmx.block_addition_mappings(0).is_empty(),
        "no BlockAdditionMapping → empty slice"
    );
    // The aggregate accessor returns one entry per stream too.
    assert_eq!(dmx.all_block_addition_mappings().len(), 1);
    assert!(dmx.all_block_addition_mappings()[0].is_empty());
}

/// One `BlockAdditionMapping` master with all four children present.
/// Verifies each field is captured verbatim and `is_codec_defined()`
/// reflects a non-zero `BlockAddIDType`.
#[test]
fn single_mapping_with_all_children() {
    // BlockAdditionMapping(value=4, name="HDR10+", type=4, extra_data=[0xDE,0xAD]).
    let mut bam = Vec::new();
    bam.extend_from_slice(&elem_uint(ids::BLOCK_ADD_ID_VALUE, 4));
    bam.extend_from_slice(&elem_str(ids::BLOCK_ADD_ID_NAME, "HDR10+"));
    bam.extend_from_slice(&elem_uint(ids::BLOCK_ADD_ID_TYPE, 4));
    bam.extend_from_slice(&elem_bytes(ids::BLOCK_ADD_ID_EXTRA_DATA, &[0xDE, 0xAD]));
    let mapping_master = elem_master(ids::BLOCK_ADDITION_MAPPING, &bam);

    let tb = video_track(1, 0xA2, &mapping_master);
    let bytes = assemble(&elem_master(ids::TRACK_ENTRY, &tb));
    let dmx = open(bytes);

    let mappings = dmx.block_addition_mappings(0);
    assert_eq!(mappings.len(), 1, "one mapping");
    let m = &mappings[0];
    assert_eq!(m.value, Some(4));
    assert_eq!(m.name.as_deref(), Some("HDR10+"));
    assert_eq!(m.addid_type, 4);
    assert_eq!(m.extra_data.as_deref(), Some(&[0xDE, 0xAD][..]));
    assert!(!m.is_codec_defined(), "addid_type != 0 → not codec-defined");
}

/// A `BlockAdditionMapping` master with no `BlockAddIDType` child decodes
/// as `addid_type == 0` per §5.1.4.1.17.3's default — and
/// `is_codec_defined()` reports true. The absent `BlockAddIDValue` /
/// `BlockAddIDName` / `BlockAddIDExtraData` children all surface as
/// `None` (no spec default for any of them).
#[test]
fn empty_mapping_materialises_codec_defined_default() {
    let mapping_master = elem_master(ids::BLOCK_ADDITION_MAPPING, &[]);
    let tb = video_track(1, 0xA3, &mapping_master);
    let bytes = assemble(&elem_master(ids::TRACK_ENTRY, &tb));
    let dmx = open(bytes);

    let mappings = dmx.block_addition_mappings(0);
    assert_eq!(mappings.len(), 1);
    let m = &mappings[0];
    assert_eq!(m.value, None, "no BlockAddIDValue → None");
    assert_eq!(m.name, None, "no BlockAddIDName → None");
    assert_eq!(m.addid_type, 0, "default 0 materialised");
    assert_eq!(m.extra_data, None, "no BlockAddIDExtraData → None");
    assert!(m.is_codec_defined(), "addid_type 0 → codec-defined");
}

/// Multiple `BlockAdditionMapping` masters on one `TrackEntry` preserve
/// on-disk order in the returned slice.
#[test]
fn multiple_mappings_preserve_document_order() {
    let mut bam1 = Vec::new();
    bam1.extend_from_slice(&elem_uint(ids::BLOCK_ADD_ID_VALUE, 2));
    bam1.extend_from_slice(&elem_uint(ids::BLOCK_ADD_ID_TYPE, 0x100));
    let mut bam2 = Vec::new();
    bam2.extend_from_slice(&elem_uint(ids::BLOCK_ADD_ID_VALUE, 3));
    bam2.extend_from_slice(&elem_uint(ids::BLOCK_ADD_ID_TYPE, 0x200));
    let mut bam3 = Vec::new();
    bam3.extend_from_slice(&elem_uint(ids::BLOCK_ADD_ID_VALUE, 4));
    bam3.extend_from_slice(&elem_uint(ids::BLOCK_ADD_ID_TYPE, 0x300));

    let mut extra = Vec::new();
    extra.extend_from_slice(&elem_master(ids::BLOCK_ADDITION_MAPPING, &bam1));
    extra.extend_from_slice(&elem_master(ids::BLOCK_ADDITION_MAPPING, &bam2));
    extra.extend_from_slice(&elem_master(ids::BLOCK_ADDITION_MAPPING, &bam3));

    let tb = video_track(1, 0xA4, &extra);
    let bytes = assemble(&elem_master(ids::TRACK_ENTRY, &tb));
    let dmx = open(bytes);

    let mappings = dmx.block_addition_mappings(0);
    assert_eq!(mappings.len(), 3, "three mappings");
    assert_eq!(mappings[0].value, Some(2));
    assert_eq!(mappings[0].addid_type, 0x100);
    assert_eq!(mappings[1].value, Some(3));
    assert_eq!(mappings[1].addid_type, 0x200);
    assert_eq!(mappings[2].value, Some(4));
    assert_eq!(mappings[2].addid_type, 0x300);
}

/// Out-of-range `stream_index` surfaces as an empty slice rather than
/// panicking — matches the slice-returning shape advertised in the
/// accessor's doc comment.
#[test]
fn out_of_range_stream_index_is_empty_slice() {
    let tb = video_track(1, 0xA5, &[]);
    let bytes = assemble(&elem_master(ids::TRACK_ENTRY, &tb));
    let dmx = open(bytes);
    assert!(dmx.block_addition_mappings(99).is_empty());
}

/// Unknown child elements inside the `BlockAdditionMapping` master are
/// skipped — forward-compat with any future schema extension on the
/// element. We splice an `EBML Void` between the known children to
/// exercise the unknown-id branch.
#[test]
fn unknown_children_are_skipped() {
    let mut bam = Vec::new();
    bam.extend_from_slice(&elem_uint(ids::BLOCK_ADD_ID_VALUE, 7));
    // EBML Void with a 3-byte payload.
    bam.extend_from_slice(&elem_bytes(ids::VOID, &[0, 0, 0]));
    bam.extend_from_slice(&elem_uint(ids::BLOCK_ADD_ID_TYPE, 9));
    let mapping_master = elem_master(ids::BLOCK_ADDITION_MAPPING, &bam);

    let tb = video_track(1, 0xA6, &mapping_master);
    let bytes = assemble(&elem_master(ids::TRACK_ENTRY, &tb));
    let dmx = open(bytes);

    let mappings = dmx.block_addition_mappings(0);
    assert_eq!(mappings.len(), 1);
    assert_eq!(mappings[0].value, Some(7));
    assert_eq!(mappings[0].addid_type, 9);
    assert!(mappings[0].name.is_none());
    assert!(mappings[0].extra_data.is_none());
}
