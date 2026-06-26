//! Reclaimed DivX trick-track / old-lacing `BlockGroup` children
//! (RFC 9559 Appendix A.3..A.14), surfaced for a faithful re-mux through
//! [`MkvDemuxer::block_group_meta`].
//!
//! These elements are no longer documented in the RFC 9559 core body but
//! their Element IDs stay reserved in the registry and historical Writers
//! still emit them:
//!
//! * `BlockVirtual` (A.3, binary, id `0xA2`)
//! * `ReferenceVirtual` (A.4, integer, id `0xFD`)
//! * `Slices` (A.5, id `0x8E`) > `TimeSlice` (A.6, id `0xE8`) with
//!   `LaceNumber` (A.7), `FrameNumber` (A.8), `BlockAdditionID` (A.9),
//!   `Delay` (A.10), `SliceDuration` (A.11)
//! * `ReferenceFrame` (A.12, id `0xC8`) > `ReferenceOffset` (A.13) +
//!   `ReferenceTimestamp` (A.14)
//!
//! We hand-build a minimal MKV whose single Cluster carries a
//! `BlockGroup` with each of these children and assert the typed meta
//! surfaces them verbatim.

use std::io::Cursor;

use oxideav_core::{Demuxer, Error, ReadSeek};
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

fn elem_int(id: u32, value: i64) -> Vec<u8> {
    // Minimal-length two's-complement big-endian signed integer.
    let mut n = 1usize;
    while n < 8 {
        let min = -(1i64 << (n * 8 - 1));
        let max = (1i64 << (n * 8 - 1)) - 1;
        if value >= min && value <= max {
            break;
        }
        n += 1;
    }
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(n as u64, 0));
    let be = value.to_be_bytes();
    out.extend_from_slice(&be[8 - n..]);
    out
}

fn elem_bytes(id: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(data.len() as u64, 0));
    out.extend_from_slice(data);
    out
}

fn elem_str(id: u32, s: &str) -> Vec<u8> {
    elem_bytes(id, s.as_bytes())
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

/// A plain `Block` element (no keyframe flag, no lacing) carrying one byte.
fn block(track: u8, tc_offset: i16, payload: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(0x00);
    body.push(payload);
    elem_bytes(ids::BLOCK, &body)
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

fn info_body() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 1000.0));
    body
}

fn tracks_body() -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, 0xA1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio = Vec::new();
    audio.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    tb.extend_from_slice(&elem_master(ids::AUDIO, &audio));
    elem_master(ids::TRACK_ENTRY, &tb)
}

fn build_segment(cluster_body: Vec<u8>) -> Vec<u8> {
    let info = elem_master(ids::INFO, &info_body());
    let tracks = elem_master(ids::TRACKS, &tracks_body());

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&elem_master(ids::CLUSTER, &cluster_body));
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

/// Drain every packet, returning the count.
fn drain(dmx: &mut oxideav_mkv::demux::MkvDemuxer) -> usize {
    let mut n = 0;
    loop {
        match dmx.next_packet() {
            Ok(_) => n += 1,
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected: {e:?}"),
        }
    }
    n
}

#[test]
fn block_group_with_no_legacy_children_has_empty_meta() {
    // A BlockGroup carrying only a Block surfaces a None meta (the common
    // modern case) — the reclaimed surface never synthesises a record.
    let mut cluster = Vec::new();
    cluster.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster.extend_from_slice(&elem_master(ids::BLOCK_GROUP, &block(1, 0, 0xAA)));
    let mut dmx = open(build_segment(cluster));
    assert_eq!(drain(&mut dmx), 1);
    assert!(dmx.block_group_meta().is_none());
}

#[test]
fn block_group_surfaces_block_virtual_and_reference_virtual() {
    // RFC 9559 Appendix A.3 / A.4 — a data-less virtual Block plus the
    // Segment Position of its real data.
    let mut group = block(1, 0, 0xAA);
    group.extend_from_slice(&elem_bytes(ids::BLOCK_VIRTUAL, &[0xDE, 0xAD, 0xBE, 0xEF]));
    group.extend_from_slice(&elem_int(ids::REFERENCE_VIRTUAL, -12_345));

    let mut cluster = Vec::new();
    cluster.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster.extend_from_slice(&elem_master(ids::BLOCK_GROUP, &group));

    let mut dmx = open(build_segment(cluster));
    assert_eq!(drain(&mut dmx), 1);
    let meta = dmx.block_group_meta().expect("legacy meta present");
    assert_eq!(meta.block_virtual(), Some(&[0xDE, 0xAD, 0xBE, 0xEF][..]));
    assert_eq!(meta.reference_virtual(), Some(-12_345));
    assert!(meta.slices().is_empty());
    assert!(meta.reference_frame().is_none());
    assert!(!meta.is_empty());
}

#[test]
fn block_group_surfaces_slices_timeslice_fields() {
    // RFC 9559 Appendix A.5..A.11 — two TimeSlice masters, the first
    // carrying every field, the second empty (preserved for re-mux count).
    let mut ts0 = Vec::new();
    ts0.extend_from_slice(&elem_uint(ids::LACE_NUMBER, 0));
    ts0.extend_from_slice(&elem_uint(ids::FRAME_NUMBER, 7));
    ts0.extend_from_slice(&elem_uint(ids::TIME_SLICE_BLOCK_ADDITION_ID, 3));
    ts0.extend_from_slice(&elem_uint(ids::DELAY, 100));
    ts0.extend_from_slice(&elem_uint(ids::SLICE_DURATION, 200));

    let mut slices = Vec::new();
    slices.extend_from_slice(&elem_master(ids::TIME_SLICE, &ts0));
    slices.extend_from_slice(&elem_master(ids::TIME_SLICE, &[]));

    let mut group = block(1, 0, 0xAA);
    group.extend_from_slice(&elem_master(ids::SLICES, &slices));

    let mut cluster = Vec::new();
    cluster.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster.extend_from_slice(&elem_master(ids::BLOCK_GROUP, &group));

    let mut dmx = open(build_segment(cluster));
    assert_eq!(drain(&mut dmx), 1);
    let meta = dmx.block_group_meta().expect("legacy meta present");
    let slices = meta.slices();
    assert_eq!(slices.len(), 2);

    assert_eq!(slices[0].lace_number(), Some(0));
    assert_eq!(slices[0].frame_number(), Some(7));
    assert_eq!(slices[0].block_addition_id(), Some(3));
    assert_eq!(slices[0].delay(), Some(100));
    assert_eq!(slices[0].slice_duration(), Some(200));
    assert!(!slices[0].is_empty());

    // Second TimeSlice carried no children — every field None, but the
    // record is preserved so a re-mux keeps the element count.
    assert_eq!(slices[1].lace_number(), None);
    assert!(slices[1].is_empty());
}

#[test]
fn block_group_surfaces_reference_frame() {
    // RFC 9559 Appendix A.12..A.14 — Smooth FF/RW trick-track
    // back-reference.
    let mut rf = Vec::new();
    rf.extend_from_slice(&elem_uint(ids::REFERENCE_OFFSET, 4096));
    rf.extend_from_slice(&elem_uint(ids::REFERENCE_TIMESTAMP, 33));

    let mut group = block(1, 0, 0xAA);
    group.extend_from_slice(&elem_master(ids::REFERENCE_FRAME, &rf));

    let mut cluster = Vec::new();
    cluster.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster.extend_from_slice(&elem_master(ids::BLOCK_GROUP, &group));

    let mut dmx = open(build_segment(cluster));
    assert_eq!(drain(&mut dmx), 1);
    let meta = dmx.block_group_meta().expect("legacy meta present");
    let frame = meta.reference_frame().expect("reference_frame present");
    assert_eq!(frame.reference_offset(), Some(4096));
    assert_eq!(frame.reference_timestamp(), Some(33));
    assert!(!frame.is_empty());
}

#[test]
fn reference_frame_partial_children_observable() {
    // Only ReferenceOffset present — ReferenceTimestamp stays None,
    // distinct from a present 0.
    let rf = elem_uint(ids::REFERENCE_OFFSET, 0);
    let mut group = block(1, 0, 0xAA);
    group.extend_from_slice(&elem_master(ids::REFERENCE_FRAME, &rf));

    let mut cluster = Vec::new();
    cluster.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster.extend_from_slice(&elem_master(ids::BLOCK_GROUP, &group));

    let mut dmx = open(build_segment(cluster));
    assert_eq!(drain(&mut dmx), 1);
    let frame = dmx
        .block_group_meta()
        .expect("meta")
        .reference_frame()
        .expect("frame");
    assert_eq!(frame.reference_offset(), Some(0));
    assert_eq!(frame.reference_timestamp(), None);
}
