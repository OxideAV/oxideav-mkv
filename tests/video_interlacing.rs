//! Integration tests for the demuxer's `Video > FlagInterlaced` (RFC 9559
//! §5.1.4.1.28.1) + `FieldOrder` (§5.1.4.1.28.2) typed decode.
//!
//! `FlagInterlaced` marks whether a video track's frames are interlaced
//! (`0` undetermined / `1` interlaced / `2` progressive). `FieldOrder`
//! reports the field order of an interlaced track — and MUST be ignored
//! when `FlagInterlaced != 1` per §5.1.4.1.28.2's "If FlagInterlaced is
//! not set to 1, this element MUST be ignored". The demuxer exposes both
//! via `MkvDemuxer::video_interlacing(stream_index) -> &VideoInterlacing`.
//!
//! Each test hand-builds an EBML byte sequence containing a `Video`
//! master with the elements of interest, parses it through the demuxer's
//! typed open entry, and asserts the typed fields match the spec.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_mkv::demux::{FieldOrder, FlagInterlaced};
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

/// Build a video TrackEntry whose `Video` master is built from `video_body`.
/// `number` is the on-disk `TrackNumber`, `uid` the `TrackUID`.
fn video_track_with_video(number: u64, uid: u64, video_body: &[u8]) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_VIDEO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "V_VP9"));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::PIXEL_WIDTH, 320));
    v.extend_from_slice(&elem_uint(ids::PIXEL_HEIGHT, 240));
    v.extend_from_slice(video_body);
    tb.extend_from_slice(&elem_master(ids::VIDEO, &v));
    tb
}

/// Build an audio TrackEntry (no `Video` master at all).
fn audio_track(number: u64, uid: u64) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_OPUS"));
    tb
}

/// Assemble EBML header + Segment(Info, Tracks, Cluster) into a file.
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

/// An interlaced track that carries `FlagInterlaced=1` and `FieldOrder=1`
/// (Tff) surfaces both fields. A progressive sibling whose `Video` master
/// carries `FlagInterlaced=2` and no `FieldOrder` reports `Progressive`
/// with `field_order() == None` (the spec forbids honouring `FieldOrder`
/// for non-interlaced tracks).
#[test]
fn flag_interlaced_with_field_order() {
    // Stream 0: interlaced, top-field-first.
    let mut v0 = Vec::new();
    v0.extend_from_slice(&elem_uint(
        ids::FLAG_INTERLACED,
        ids::FLAG_INTERLACED_INTERLACED,
    ));
    v0.extend_from_slice(&elem_uint(ids::FIELD_ORDER, ids::FIELD_ORDER_TFF));
    let t0 = video_track_with_video(1, 0x11, &v0);

    // Stream 1: progressive, no FieldOrder child.
    let mut v1 = Vec::new();
    v1.extend_from_slice(&elem_uint(
        ids::FLAG_INTERLACED,
        ids::FLAG_INTERLACED_PROGRESSIVE,
    ));
    let t1 = video_track_with_video(2, 0x22, &v1);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t0));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t1));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let s0 = dmx
        .video_interlacing(0)
        .expect("video track has interlacing");
    assert_eq!(s0.flag(), FlagInterlaced::Interlaced);
    assert_eq!(s0.field_order(), Some(FieldOrder::Tff));

    let s1 = dmx
        .video_interlacing(1)
        .expect("progressive video has interlacing record");
    assert_eq!(s1.flag(), FlagInterlaced::Progressive);
    assert_eq!(
        s1.field_order(),
        None,
        "FieldOrder MUST be ignored when FlagInterlaced != 1"
    );

    // The slice view has one entry per stream.
    assert_eq!(dmx.video_interlacings().len(), dmx.streams().len());
}

/// A video track whose `Video` master omits `FlagInterlaced` decodes to the
/// spec default `0` (`Undetermined`); for that flag value `FieldOrder` is
/// `None` because the §5.1.4.1.28.2 "MUST be ignored" rule covers
/// undetermined as well.
#[test]
fn missing_flag_interlaced_defaults_to_undetermined() {
    let v = Vec::new(); // no FlagInterlaced, no FieldOrder
    let t = video_track_with_video(1, 0xAB, &v);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let s = dmx
        .video_interlacing(0)
        .expect("Video master present, defaults materialised");
    assert_eq!(s.flag(), FlagInterlaced::Undetermined);
    assert_eq!(s.field_order(), None);
}

/// An interlaced track with no explicit `FieldOrder` reports the spec
/// default `2` (`Undetermined`) — that is, `Some(FieldOrder::Undetermined)`
/// rather than `None`. Track-on-the-other-side: an unrecognised
/// `FieldOrder` value (e.g. `42`) on an interlaced track surfaces as
/// `Some(FieldOrder::Other(42))` rather than being dropped.
#[test]
fn interlaced_field_order_default_and_unknown() {
    // Stream 0: interlaced, no FieldOrder -> default 2 = Undetermined.
    let mut v0 = Vec::new();
    v0.extend_from_slice(&elem_uint(
        ids::FLAG_INTERLACED,
        ids::FLAG_INTERLACED_INTERLACED,
    ));
    let t0 = video_track_with_video(1, 0x33, &v0);

    // Stream 1: interlaced, FieldOrder=42 (unregistered).
    let mut v1 = Vec::new();
    v1.extend_from_slice(&elem_uint(
        ids::FLAG_INTERLACED,
        ids::FLAG_INTERLACED_INTERLACED,
    ));
    v1.extend_from_slice(&elem_uint(ids::FIELD_ORDER, 42));
    let t1 = video_track_with_video(2, 0x44, &v1);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t0));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t1));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let s0 = dmx.video_interlacing(0).expect("interlaced track");
    assert_eq!(s0.flag(), FlagInterlaced::Interlaced);
    assert_eq!(s0.field_order(), Some(FieldOrder::Undetermined));

    let s1 = dmx.video_interlacing(1).expect("interlaced track");
    assert_eq!(s1.flag(), FlagInterlaced::Interlaced);
    assert_eq!(s1.field_order(), Some(FieldOrder::Other(42)));
}

/// A track with no `Video` master at all (an audio track) reports `None`
/// from `video_interlacing` — the typed surface is video-only.
/// Demonstrates that `video_interlacings()` still has one entry per stream
/// (here: two streams, both with `None`-ish state for audio).
#[test]
fn audio_track_has_no_interlacing() {
    // Stream 0: audio. Stream 1: video with FlagInterlaced=1.
    let ta = audio_track(1, 0x55);
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(
        ids::FLAG_INTERLACED,
        ids::FLAG_INTERLACED_INTERLACED,
    ));
    v.extend_from_slice(&elem_uint(
        ids::FIELD_ORDER,
        ids::FIELD_ORDER_BFF_INTERLEAVED,
    ));
    let tv = video_track_with_video(2, 0x66, &v);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &tv));

    // Use a cluster that names the audio TrackNumber so demux doesn't
    // complain about the cluster being empty for the streams it found.
    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cb.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    let cluster = elem_master(ids::CLUSTER, &cb);

    let tracks = elem_master(ids::TRACKS, &tracks_body);
    let mut seg = Vec::new();
    seg.extend_from_slice(&info());
    seg.extend_from_slice(&tracks);
    seg.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&ebml_header());
    bytes.extend_from_slice(&segment);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    assert_eq!(dmx.video_interlacings().len(), dmx.streams().len());
    assert!(
        dmx.video_interlacing(0).is_none(),
        "audio track has no Video master -> no interlacing record"
    );
    let sv = dmx.video_interlacing(1).expect("video track");
    assert_eq!(sv.flag(), FlagInterlaced::Interlaced);
    assert_eq!(sv.field_order(), Some(FieldOrder::BffInterleaved));
}
