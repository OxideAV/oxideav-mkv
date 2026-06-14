//! Integration tests for the demuxer's `TrackCodecTiming` typed decode
//! (RFC 9559 §5.1.4.1.25 + §5.1.4.1.26 — `CodecDelay`, `SeekPreRoll`).
//!
//! The two `TrackEntry`-level codec-timing elements fold into one
//! `TrackCodecTiming` record per track. Both are nanosecond (Matroska Tick)
//! `uinteger`s with the spec default `0` and — unlike the
//! `DefaultDuration` / `DefaultDecodedFieldDuration` pair — *no* "not 0"
//! range, so an explicit on-disk `0` is a legal value distinct from "absent."
//! The plain accessors materialise the `0` default; the `_explicit` accessors
//! preserve the on-disk presence.
//!
//! These tests hand-build Matroska byte streams from the EBML primitives and
//! walk them with the production demuxer — no third-party Matroska code is
//! consulted.

use std::io::Cursor;

use oxideav_core::ReadSeek;
use oxideav_mkv::demux::TrackCodecTiming;
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

/// Build an audio TrackEntry carrying `extra` codec-timing children
/// (the concatenation of `elem_uint(ids::CODEC_DELAY, ...)`, etc.).
fn audio_track_with_codec_timing(number: u64, uid: u64, extra: &[u8]) -> Vec<u8> {
    let mut ab = elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0);
    ab.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    let audio_master = elem_master(ids::AUDIO, &ab);
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_OPUS"));
    tb.extend_from_slice(extra);
    tb.extend_from_slice(&audio_master);
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

fn codec_timing_of(extra: &[u8]) -> TrackCodecTiming {
    let t = audio_track_with_codec_timing(1, 0x77, extra);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));
    *dmx.track_codec_timing(0).expect("track 0 present")
}

/// A TrackEntry with neither element surfaces a record with both fields at
/// the materialised `0` default and `is_empty()` true.
#[test]
fn empty_codec_timing_materialises_zero_defaults() {
    let t = codec_timing_of(&[]);
    assert_eq!(t.codec_delay_explicit(), None);
    assert_eq!(t.seek_pre_roll_explicit(), None);
    assert_eq!(t.codec_delay(), 0);
    assert_eq!(t.seek_pre_roll(), 0);
    assert!(t.is_empty());
}

/// Explicit `CodecDelay` (§5.1.4.1.25) surfaces unchanged through both the
/// default-materialising and the `_explicit` accessors.
#[test]
fn codec_delay_explicit() {
    // 3_840_000 ns = an Opus pre-skip of 312 samples at 48 kHz, ~6.5 ms.
    let t = codec_timing_of(&elem_uint(ids::CODEC_DELAY, 3_840_000));
    assert_eq!(t.codec_delay_explicit(), Some(3_840_000));
    assert_eq!(t.codec_delay(), 3_840_000);
    assert_eq!(t.seek_pre_roll_explicit(), None);
    assert_eq!(t.seek_pre_roll(), 0);
    assert!(!t.is_empty());
}

/// `SeekPreRoll` (§5.1.4.1.26) surfaces independently of `CodecDelay`.
#[test]
fn seek_pre_roll_explicit() {
    // 80_000_000 ns = the conventional 80 ms Opus seek pre-roll.
    let t = codec_timing_of(&elem_uint(ids::SEEK_PRE_ROLL, 80_000_000));
    assert_eq!(t.seek_pre_roll_explicit(), Some(80_000_000));
    assert_eq!(t.seek_pre_roll(), 80_000_000);
    assert_eq!(t.codec_delay_explicit(), None);
    assert_eq!(t.codec_delay(), 0);
    assert!(!t.is_empty());
}

/// Both elements present together — the canonical Opus shape.
#[test]
fn both_present() {
    let mut extra = elem_uint(ids::CODEC_DELAY, 6_500_000);
    extra.extend_from_slice(&elem_uint(ids::SEEK_PRE_ROLL, 80_000_000));
    let t = codec_timing_of(&extra);
    assert_eq!(t.codec_delay(), 6_500_000);
    assert_eq!(t.seek_pre_roll(), 80_000_000);
    assert_eq!(t.codec_delay_explicit(), Some(6_500_000));
    assert_eq!(t.seek_pre_roll_explicit(), Some(80_000_000));
    assert!(!t.is_empty());
}

/// An explicit on-disk `0` is distinct from "absent": both elements have no
/// "not 0" range, so a writer-emitted `0` must survive as `Some(0)` and the
/// record is *not* empty (RFC 9559 §5.1.4.1.25 / §5.1.4.1.26, default `0`).
#[test]
fn explicit_zero_distinct_from_absent() {
    let mut extra = elem_uint(ids::CODEC_DELAY, 0);
    extra.extend_from_slice(&elem_uint(ids::SEEK_PRE_ROLL, 0));
    let t = codec_timing_of(&extra);
    assert_eq!(t.codec_delay_explicit(), Some(0));
    assert_eq!(t.seek_pre_roll_explicit(), Some(0));
    assert_eq!(t.codec_delay(), 0);
    assert_eq!(t.seek_pre_roll(), 0);
    assert!(
        !t.is_empty(),
        "an explicit on-disk 0 must not read as empty"
    );
}

/// A record surfaces for every track and the `all_*` slice is indexed by
/// stream index; an out-of-range index returns `None`.
#[test]
fn surfaces_for_every_track_and_slice_indexing() {
    let t0 = audio_track_with_codec_timing(1, 0x10, &elem_uint(ids::CODEC_DELAY, 1_000_000));
    let t1 = audio_track_with_codec_timing(2, 0x20, &[]);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t0));
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t1));
    let dmx = open(assemble(&body));

    let all = dmx.all_track_codec_timing();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].codec_delay(), 1_000_000);
    assert!(!all[0].is_empty());
    assert!(all[1].is_empty());

    assert!(dmx.track_codec_timing(0).is_some());
    assert!(dmx.track_codec_timing(1).is_some());
    assert!(dmx.track_codec_timing(2).is_none());
}
