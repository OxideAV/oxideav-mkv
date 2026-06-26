//! Integration tests for the demuxer's + muxer's legacy `Video >
//! OldStereoMode` (RFC 9559 §5.1.4.1.28.5, id `0x53B9`) surface.
//!
//! `OldStereoMode` is the "bogus" stereo-3D mode value that libmatroska prior
//! to 0.9.0 wrote at the wrong Element ID (`0x53B9` instead of `0x53B8`,
//! §18.10). The spec marks it `maxver: 2` and says a Writer MUST NOT use it,
//! but a Reader MAY support legacy files by reading it. Its value space
//! (Table 7) is **incompatible** with the modern `StereoMode` (Table 5):
//! only `0` (mono), `1` (right eye), `2` (left eye), `3` (both eyes) appear
//! here, so the typed surface is kept separate from `StereoMode`. Values
//! outside Table 7 pass through `OldStereoMode::Other`.
//!
//! The demuxer exposes the value via
//! `MkvDemuxer::video_old_stereo_mode(stream_index) -> Option<OldStereoMode>`;
//! the muxer writes it via `MkvMuxer::set_video_old_stereo_mode`. Each demux
//! test hand-builds an EBML byte sequence and parses it through the typed open
//! entry; the round-trip test mux→demux's through the in-tree muxer.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::demux::{OldStereoMode, StereoMode};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;
use oxideav_mkv::mux::MkvMuxer;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path() -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r372-oldstereo-{}-{n}.mkv",
        std::process::id()
    ))
}

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
    // OldStereoMode is maxver 2 — declare DocTypeVersion 2 to be faithful.
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 2));
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

/// A video track whose `Video` master carries no `OldStereoMode` child
/// surfaces `None` — unlike the modern `StereoMode`, the legacy element has
/// **no** materialised spec default (a modern file legitimately has none).
#[test]
fn missing_old_stereo_mode_is_none() {
    let v = Vec::new();
    let t = video_track_with_video(1, 0xAB, &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    // Modern StereoMode still materialises Mono (Video master present).
    assert_eq!(dmx.video_stereo_mode(0), Some(StereoMode::Mono));
    // But OldStereoMode is absent on disk -> None, not a synthesised Mono.
    assert_eq!(
        dmx.video_old_stereo_mode(0),
        None,
        "OldStereoMode has no materialised default"
    );
    assert_eq!(dmx.video_old_stereo_modes().len(), dmx.streams().len());
}

/// Every §5.1.4.1.28.5 Table 7 value round-trips through the typed surface,
/// and `is_stereo()` is `false` only for mono. A value outside Table 7 passes
/// through `OldStereoMode::Other`.
#[test]
fn all_table7_values_round_trip() {
    let cases: &[(u64, OldStereoMode)] = &[
        (ids::OLD_STEREO_MODE_MONO, OldStereoMode::Mono),
        (ids::OLD_STEREO_MODE_RIGHT_EYE, OldStereoMode::RightEye),
        (ids::OLD_STEREO_MODE_LEFT_EYE, OldStereoMode::LeftEye),
        (ids::OLD_STEREO_MODE_BOTH_EYES, OldStereoMode::BothEyes),
        (7, OldStereoMode::Other(7)),
    ];

    for (raw, expected) in cases {
        let mut v = Vec::new();
        v.extend_from_slice(&elem_uint(ids::OLD_STEREO_MODE, *raw));
        let t = video_track_with_video(1, 0x11, &v);
        let mut tracks_body = Vec::new();
        tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

        let bytes = assemble(&tracks_body);
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
        let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver)
            .expect("demux open");

        let m = dmx.video_old_stereo_mode(0).expect("video track");
        assert_eq!(m, *expected, "OldStereoMode raw {raw} -> {expected:?}");
        assert_eq!(
            m.is_stereo(),
            *raw != ids::OLD_STEREO_MODE_MONO,
            "is_stereo() should match non-mono for raw {raw}"
        );
        assert_eq!(m.to_raw(), *raw, "to_raw() round-trips raw {raw}");
    }
}

/// A track may legally carry BOTH a modern `StereoMode` and the legacy
/// `OldStereoMode` (a transitional file written by a tool aware of the bug).
/// The two surfaces are independent and report their own values.
#[test]
fn old_and_modern_coexist_independently() {
    let mut v = Vec::new();
    // Modern: side-by-side, left first.
    v.extend_from_slice(&elem_uint(
        ids::STEREO_MODE,
        ids::STEREO_MODE_SIDE_BY_SIDE_LEFT_FIRST,
    ));
    // Legacy: left eye (Table 7 value 2 — different value space).
    v.extend_from_slice(&elem_uint(
        ids::OLD_STEREO_MODE,
        ids::OLD_STEREO_MODE_LEFT_EYE,
    ));
    let t = video_track_with_video(1, 0x22, &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    assert_eq!(
        dmx.video_stereo_mode(0),
        Some(StereoMode::SideBySideLeftFirst)
    );
    assert_eq!(dmx.video_old_stereo_mode(0), Some(OldStereoMode::LeftEye));
}

/// An audio track (no `Video` master) returns `None` from
/// `video_old_stereo_mode`; the slice view still has one entry per stream.
#[test]
fn audio_track_has_no_old_stereo_mode() {
    let ta = audio_track(1, 0x55);
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(
        ids::OLD_STEREO_MODE,
        ids::OLD_STEREO_MODE_BOTH_EYES,
    ));
    let tv = video_track_with_video(2, 0x66, &v);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &tv));

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

    assert_eq!(dmx.video_old_stereo_modes().len(), dmx.streams().len());
    assert!(dmx.video_old_stereo_mode(0).is_none(), "audio -> no Video");
    assert_eq!(dmx.video_old_stereo_mode(1), Some(OldStereoMode::BothEyes));
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

    assert!(dmx.video_old_stereo_mode(99).is_none());
    assert!(dmx.video_old_stereo_mode(u32::MAX).is_none());
}

// ---- Mux write path + mux->demux round-trip ----

fn video_stream() -> StreamInfo {
    let mut p = CodecParameters::video(CodecId::new("vp9"));
    p.width = Some(320);
    p.height = Some(240);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn audio_stream() -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn keyframe_packet(stream: u32, pts: i64, marker: u8, len: usize) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![marker; len]);
    p.pts = Some(pts);
    p.flags.keyframe = true;
    p
}

/// Mux a single-track video MKV, invoking `configure` between construction and
/// `write_header`. Returns the muxed bytes.
fn mux_video<F>(configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path();
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let stream = video_stream();
        let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
        configure(&mut mx);
        mx.write_header().expect("write_header");
        mx.write_packet(&keyframe_packet(0, 0, 0xAA, 32))
            .expect("write_packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn demux_typed(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open_typed")
}

/// `set_video_old_stereo_mode` queues the legacy element, which the muxer
/// emits inside the `Video` master and the demuxer reads back verbatim through
/// `video_old_stereo_mode` — including the `Other(u64)` forward-compat path.
#[test]
fn mux_write_round_trips_through_demux() {
    for mode in [
        OldStereoMode::Mono,
        OldStereoMode::RightEye,
        OldStereoMode::LeftEye,
        OldStereoMode::BothEyes,
        OldStereoMode::Other(9),
    ] {
        let bytes = mux_video(|mx| {
            mx.set_video_old_stereo_mode(0, mode)
                .expect("set_video_old_stereo_mode");
            // Read-back accessor returns the queued hint pre-write_header.
            assert_eq!(mx.video_old_stereo_mode(0), Some(mode));
        });
        let dmx = demux_typed(bytes);
        assert_eq!(
            dmx.video_old_stereo_mode(0),
            Some(mode),
            "mux->demux round-trip for {mode:?}"
        );
    }
}

/// Omitting the call keeps the legacy element off-disk, so the demuxer
/// surfaces `None` (the correct behaviour for a modern file).
#[test]
fn mux_omits_element_by_default() {
    let bytes = mux_video(|mx| {
        assert_eq!(mx.video_old_stereo_mode(0), None);
    });
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_old_stereo_mode(0), None);
}

/// The setter rejects non-video streams, out-of-range indices, and
/// post-`write_header` use.
#[test]
fn mux_setter_validation() {
    // Non-video stream is rejected.
    let ws: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::<u8>::new()));
    let mut mx = MkvMuxer::new_matroska(ws, &[audio_stream()]).expect("muxer construct");
    assert!(
        mx.set_video_old_stereo_mode(0, OldStereoMode::LeftEye)
            .is_err(),
        "audio track has no Video master"
    );
    // Out-of-range index is rejected.
    assert!(mx
        .set_video_old_stereo_mode(99, OldStereoMode::Mono)
        .is_err());

    // Post-write_header use is rejected.
    let ws2: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::<u8>::new()));
    let mut mx2 = MkvMuxer::new_matroska(ws2, &[video_stream()]).expect("muxer construct");
    mx2.write_header().expect("write_header");
    assert!(
        mx2.set_video_old_stereo_mode(0, OldStereoMode::RightEye)
            .is_err(),
        "set after write_header must fail"
    );
}
