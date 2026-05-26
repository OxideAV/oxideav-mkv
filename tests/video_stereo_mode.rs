//! Integration tests for the demuxer's `Video > StereoMode` (RFC 9559
//! §5.1.4.1.28.3) typed decode.
//!
//! `StereoMode` describes the single-track packing of stereo-3D video
//! (`0` mono / `1` side-by-side-left-first / … / `14` both-eyes-laced-
//! right-first). §27.7 leaves the registry open for future additions, so
//! the typed surface preserves unknown values via `StereoMode::Other`.
//! The multi-track stereo path (`TrackOperation > TrackCombinePlanes`,
//! §5.1.4.1.30.1) is independent — see `tests/track_operation.rs`.
//!
//! The demuxer exposes the typed value via
//! `MkvDemuxer::video_stereo_mode(stream_index) -> Option<StereoMode>`.
//! Each test hand-builds an EBML byte sequence containing a `Video` master
//! with the elements of interest, parses it through the demuxer's typed
//! open entry, and asserts the typed value matches the spec.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_mkv::demux::StereoMode;
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

/// A video TrackEntry with a `Video` master that carries no `StereoMode`
/// child decodes to the spec default `0` (`Mono`) — distinguishable from
/// `None` (which means "no `Video` master at all").
#[test]
fn missing_stereo_mode_defaults_to_mono() {
    let v = Vec::new(); // empty Video master beyond PixelWidth/PixelHeight
    let t = video_track_with_video(1, 0xAB, &v);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let m = dmx
        .video_stereo_mode(0)
        .expect("Video master present, default materialised");
    assert_eq!(m, StereoMode::Mono);
    assert!(!m.is_stereo(), "Mono is not a stereo packing");
}

/// Every registered §5.1.4.1.28.3 Table 5 value round-trips through the
/// typed surface, and `is_stereo()` returns `true` for everything except
/// `Mono`. The cluster has only one packet so we build a fresh file per
/// case to keep the assertions tight.
#[test]
fn all_registered_stereo_modes_round_trip() {
    let cases: &[(u64, StereoMode)] = &[
        (ids::STEREO_MODE_MONO, StereoMode::Mono),
        (
            ids::STEREO_MODE_SIDE_BY_SIDE_LEFT_FIRST,
            StereoMode::SideBySideLeftFirst,
        ),
        (
            ids::STEREO_MODE_TOP_BOTTOM_RIGHT_FIRST,
            StereoMode::TopBottomRightFirst,
        ),
        (
            ids::STEREO_MODE_TOP_BOTTOM_LEFT_FIRST,
            StereoMode::TopBottomLeftFirst,
        ),
        (
            ids::STEREO_MODE_CHECKBOARD_RIGHT_FIRST,
            StereoMode::CheckboardRightFirst,
        ),
        (
            ids::STEREO_MODE_CHECKBOARD_LEFT_FIRST,
            StereoMode::CheckboardLeftFirst,
        ),
        (
            ids::STEREO_MODE_ROW_INTERLEAVED_RIGHT_FIRST,
            StereoMode::RowInterleavedRightFirst,
        ),
        (
            ids::STEREO_MODE_ROW_INTERLEAVED_LEFT_FIRST,
            StereoMode::RowInterleavedLeftFirst,
        ),
        (
            ids::STEREO_MODE_COLUMN_INTERLEAVED_RIGHT_FIRST,
            StereoMode::ColumnInterleavedRightFirst,
        ),
        (
            ids::STEREO_MODE_COLUMN_INTERLEAVED_LEFT_FIRST,
            StereoMode::ColumnInterleavedLeftFirst,
        ),
        (
            ids::STEREO_MODE_ANAGLYPH_CYAN_RED,
            StereoMode::AnaglyphCyanRed,
        ),
        (
            ids::STEREO_MODE_SIDE_BY_SIDE_RIGHT_FIRST,
            StereoMode::SideBySideRightFirst,
        ),
        (
            ids::STEREO_MODE_ANAGLYPH_GREEN_MAGENTA,
            StereoMode::AnaglyphGreenMagenta,
        ),
        (
            ids::STEREO_MODE_BOTH_EYES_LACED_LEFT_FIRST,
            StereoMode::BothEyesLacedLeftFirst,
        ),
        (
            ids::STEREO_MODE_BOTH_EYES_LACED_RIGHT_FIRST,
            StereoMode::BothEyesLacedRightFirst,
        ),
    ];

    for (raw, expected) in cases {
        let mut v = Vec::new();
        v.extend_from_slice(&elem_uint(ids::STEREO_MODE, *raw));
        let t = video_track_with_video(1, 0x11, &v);
        let mut tracks_body = Vec::new();
        tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

        let bytes = assemble(&tracks_body);
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
        let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver)
            .expect("demux open");

        let m = dmx.video_stereo_mode(0).expect("video track");
        assert_eq!(
            m, *expected,
            "StereoMode raw {raw} should decode to {expected:?}"
        );
        assert_eq!(
            m.is_stereo(),
            *raw != ids::STEREO_MODE_MONO,
            "is_stereo() should match the non-Mono predicate for raw {raw}"
        );
    }
}

/// Two tracks with different StereoMode packings surface independent typed
/// values, and a value outside §5.1.4.1.28.3 Table 5 passes through the
/// `StereoMode::Other` variant rather than being dropped. The slice view
/// holds one entry per stream.
#[test]
fn multiple_tracks_and_unknown_value() {
    // Stream 0: side-by-side (right eye first).
    let mut v0 = Vec::new();
    v0.extend_from_slice(&elem_uint(
        ids::STEREO_MODE,
        ids::STEREO_MODE_SIDE_BY_SIDE_RIGHT_FIRST,
    ));
    let t0 = video_track_with_video(1, 0x33, &v0);

    // Stream 1: unregistered value (42); §27.7 leaves the registry open so
    // it MUST surface rather than be dropped.
    let mut v1 = Vec::new();
    v1.extend_from_slice(&elem_uint(ids::STEREO_MODE, 42));
    let t1 = video_track_with_video(2, 0x44, &v1);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t0));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t1));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let s0 = dmx.video_stereo_mode(0).expect("video track 0");
    assert_eq!(s0, StereoMode::SideBySideRightFirst);
    assert!(s0.is_stereo());

    let s1 = dmx.video_stereo_mode(1).expect("video track 1");
    assert_eq!(s1, StereoMode::Other(42));
    // Other values are treated as stereo packing — anything other than Mono.
    assert!(s1.is_stereo());

    // Slice view has one entry per stream.
    assert_eq!(dmx.video_stereo_modes().len(), dmx.streams().len());
}

/// An audio track (no `Video` master at all) returns `None` from
/// `video_stereo_mode`; the slice view still has one entry per stream.
/// Demonstrates the surface is video-only.
#[test]
fn audio_track_has_no_stereo_mode() {
    let ta = audio_track(1, 0x55);
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(
        ids::STEREO_MODE,
        ids::STEREO_MODE_TOP_BOTTOM_LEFT_FIRST,
    ));
    let tv = video_track_with_video(2, 0x66, &v);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &tv));

    // Cluster references the audio track so the demuxer is happy.
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

    assert_eq!(dmx.video_stereo_modes().len(), dmx.streams().len());
    assert!(
        dmx.video_stereo_mode(0).is_none(),
        "audio track has no Video master -> no StereoMode"
    );
    let sv = dmx.video_stereo_mode(1).expect("video track");
    assert_eq!(sv, StereoMode::TopBottomLeftFirst);
}

/// Out-of-range stream indices yield `None` rather than panicking.
#[test]
fn out_of_range_stream_index() {
    let v = Vec::new();
    let t = video_track_with_video(1, 0x77, &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    assert!(dmx.video_stereo_mode(99).is_none());
    assert!(dmx.video_stereo_mode(u32::MAX).is_none());
}
