//! Integration tests for the demuxer's `TrackTiming` typed decode
//! (RFC 9559 §5.1.4.1.13..§5.1.4.1.15 — `DefaultDuration`,
//! `DefaultDecodedFieldDuration`, `TrackTimestampScale`).
//!
//! The three `TrackEntry`-level timing elements fold into one `TrackTiming`
//! record per track. `DefaultDuration` (§5.1.4.1.13) and
//! `DefaultDecodedFieldDuration` (§5.1.4.1.14) are nanosecond `uinteger`s
//! with a "not 0" range and no default — they stay `Option<u64>`, and a
//! spec-illegal explicit `0` is dropped at parse time. `TrackTimestampScale`
//! (§5.1.4.1.15) is a `float` with a `> 0x0p+0` range and default `1.0`;
//! the typed accessor materialises the default while `_explicit` preserves
//! the on-disk presence, and a non-finite / non-positive payload is dropped.
//!
//! These tests hand-build Matroska byte streams from the EBML primitives and
//! walk them with the production demuxer — no third-party Matroska code is
//! consulted.

use std::io::Cursor;

use oxideav_core::ReadSeek;
use oxideav_mkv::demux::TrackTiming;
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

fn elem_float_be_f32(id: u32, value: f32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(4, 0));
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

/// Build a video TrackEntry carrying `extra` timing children (the
/// concatenation of `elem_uint(ids::DEFAULT_DURATION, ...)`, etc.).
fn video_track_with_timing(number: u64, uid: u64, extra: &[u8]) -> Vec<u8> {
    let mut vb = elem_uint(ids::PIXEL_WIDTH, 320);
    vb.extend_from_slice(&elem_uint(ids::PIXEL_HEIGHT, 240));
    let video_master = elem_master(ids::VIDEO, &vb);
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_VIDEO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "V_VP9"));
    tb.extend_from_slice(extra);
    tb.extend_from_slice(&video_master);
    tb
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

fn timing_of(extra: &[u8]) -> TrackTiming {
    let t = video_track_with_timing(1, 0x77, extra);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));
    *dmx.track_timing(0).expect("track 0 present")
}

/// A TrackEntry with none of the three timing elements surfaces a record
/// with both durations `None` and the materialised §5.1.4.1.15 default 1.0.
#[test]
fn empty_timing_materialises_default_scale() {
    let t = timing_of(&[]);
    assert_eq!(t.default_duration(), None);
    assert_eq!(t.default_decoded_field_duration(), None);
    assert_eq!(t.track_timestamp_scale_explicit(), None);
    assert_eq!(t.track_timestamp_scale(), 1.0);
    assert_eq!(t.nominal_frame_rate(), None);
    assert!(t.is_empty());
}

/// Explicit `DefaultDuration` surfaces unchanged and drives the
/// derived nominal frame rate (1e9 / ns).
#[test]
fn default_duration_and_nominal_frame_rate() {
    // 40_000_000 ns = exactly 25 fps.
    let t = timing_of(&elem_uint(ids::DEFAULT_DURATION, 40_000_000));
    assert_eq!(t.default_duration(), Some(40_000_000));
    let fps = t.nominal_frame_rate().expect("rate derivable");
    assert!((fps - 25.0).abs() < 1e-9, "expected 25 fps, got {fps}");
    assert!(!t.is_empty());
}

/// `DefaultDecodedFieldDuration` (§5.1.4.1.14) surfaces independently.
#[test]
fn default_decoded_field_duration() {
    let t = timing_of(&elem_uint(ids::DEFAULT_DECODED_FIELD_DURATION, 16_683_333));
    assert_eq!(t.default_decoded_field_duration(), Some(16_683_333));
    assert_eq!(t.default_duration(), None);
}

/// `TrackTimestampScale` as an 8-byte f64 round-trips, and the `_explicit`
/// accessor reflects the on-disk presence.
#[test]
fn track_timestamp_scale_f64() {
    let t = timing_of(&elem_float_be_f64(ids::TRACK_TIMESTAMP_SCALE, 2.5));
    assert_eq!(t.track_timestamp_scale_explicit(), Some(2.5));
    assert_eq!(t.track_timestamp_scale(), 2.5);
    assert!(!t.is_empty());
}

/// `TrackTimestampScale` accepts a 4-byte f32 payload (RFC 8794 permits
/// either width); the typed accessor folds it to f64.
#[test]
fn track_timestamp_scale_f32() {
    let t = timing_of(&elem_float_be_f32(ids::TRACK_TIMESTAMP_SCALE, 0.5));
    assert_eq!(t.track_timestamp_scale_explicit(), Some(0.5));
    assert_eq!(t.track_timestamp_scale(), 0.5);
}

/// A spec-illegal explicit `0` for either nanosecond duration is dropped at
/// parse time (range "not 0") and surfaces as `None`.
#[test]
fn zero_durations_dropped() {
    let mut extra = elem_uint(ids::DEFAULT_DURATION, 0);
    extra.extend_from_slice(&elem_uint(ids::DEFAULT_DECODED_FIELD_DURATION, 0));
    let t = timing_of(&extra);
    assert_eq!(t.default_duration(), None);
    assert_eq!(t.default_decoded_field_duration(), None);
    assert!(t.is_empty());
}

/// A non-positive `TrackTimestampScale` (range "> 0x0p+0") is dropped at
/// parse time; the typed accessor falls back to the materialised 1.0.
#[test]
fn non_positive_scale_dropped() {
    let t = timing_of(&elem_float_be_f64(ids::TRACK_TIMESTAMP_SCALE, 0.0));
    assert_eq!(t.track_timestamp_scale_explicit(), None);
    assert_eq!(t.track_timestamp_scale(), 1.0);

    let t = timing_of(&elem_float_be_f64(ids::TRACK_TIMESTAMP_SCALE, -1.0));
    assert_eq!(t.track_timestamp_scale_explicit(), None);

    let t = timing_of(&elem_float_be_f64(ids::TRACK_TIMESTAMP_SCALE, f64::NAN));
    assert_eq!(t.track_timestamp_scale_explicit(), None);
}

/// All three elements present together fold into one record.
#[test]
fn all_three_together() {
    let mut extra = elem_uint(ids::DEFAULT_DURATION, 33_366_666);
    extra.extend_from_slice(&elem_uint(ids::DEFAULT_DECODED_FIELD_DURATION, 16_683_333));
    extra.extend_from_slice(&elem_float_be_f64(ids::TRACK_TIMESTAMP_SCALE, 1.5));
    let t = timing_of(&extra);
    assert_eq!(t.default_duration(), Some(33_366_666));
    assert_eq!(t.default_decoded_field_duration(), Some(16_683_333));
    assert_eq!(t.track_timestamp_scale(), 1.5);
}

/// Every track surfaces a record (the elements sit on `TrackEntry`, with no
/// gating master), and `all_track_timing()` mirrors the per-stream accessor.
#[test]
fn slice_mirror_and_every_track_surfaces() {
    let t0 = video_track_with_timing(1, 0x01, &elem_uint(ids::DEFAULT_DURATION, 20_000_000));
    let t1 = video_track_with_timing(2, 0x02, &[]); // no timing elements
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t0));
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t1));
    let dmx = open(assemble(&body));

    let all = dmx.all_track_timing();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].default_duration(), Some(20_000_000));
    assert!(all[1].is_empty());
    assert_eq!(
        dmx.track_timing(0).map(|t| t.default_duration()),
        Some(Some(20_000_000))
    );
    assert_eq!(dmx.track_timing(1).map(|t| t.is_empty()), Some(true));
    // Out-of-range index returns None.
    assert!(dmx.track_timing(2).is_none());
}
