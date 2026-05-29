//! Integration tests for the demuxer's typed decode of the three
//! per-track `Video` sub-elements that were previously skipped:
//!
//! * `AlphaMode` (RFC 9559 §5.1.4.1.28.4, id `0x53C0`).
//! * `AspectRatioType` (RFC 9559 Appendix A.24, reclaimed, id `0x54B3`).
//! * `UncompressedFourCC` (RFC 9559 §5.1.4.1.28.15, id `0x2EB524`).
//!
//! Each test hand-builds an EBML byte sequence containing a `Video`
//! master with the elements of interest, parses it through the
//! demuxer's typed open entry, and asserts the typed fields match the
//! spec.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_mkv::demux::AlphaMode;
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

fn elem_bin(id: u32, b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(b.len() as u64, 0));
    out.extend_from_slice(b);
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

fn one_cluster_track1() -> Vec<u8> {
    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cb.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    elem_master(ids::CLUSTER, &cb)
}

/// Build a video TrackEntry whose `Video` master is built from `video_body`.
fn video_track_with_video(number: u64, uid: u64, codec_id: &str, video_body: &[u8]) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_VIDEO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, codec_id));
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

fn assemble(tracks_body: &[u8]) -> Vec<u8> {
    let tracks = elem_master(ids::TRACKS, tracks_body);
    let mut seg = Vec::new();
    seg.extend_from_slice(&info());
    seg.extend_from_slice(&tracks);
    seg.extend_from_slice(&one_cluster_track1());
    let segment = elem_master(ids::SEGMENT, &seg);
    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

// -- AlphaMode (RFC 9559 §5.1.4.1.28.4) -------------------------------------

/// A WebM-alpha track that carries `AlphaMode = 1` decodes to
/// `AlphaMode::Present` and `has_alpha()` reports true.
#[test]
fn alpha_mode_present() {
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::ALPHA_MODE, ids::ALPHA_MODE_PRESENT));
    let t = video_track_with_video(1, 0x11, "V_VP9", &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let am = dmx.video_alpha_mode(0).expect("video track");
    assert_eq!(am, AlphaMode::Present);
    assert!(am.has_alpha(), "AlphaMode::Present must report has_alpha");
    assert_eq!(dmx.video_alpha_modes().len(), dmx.streams().len());
}

/// A `Video` master with no explicit `AlphaMode` decodes to the spec
/// default `0` (`AlphaMode::None`); `has_alpha()` reports false.
#[test]
fn alpha_mode_default_none() {
    let v = Vec::new();
    let t = video_track_with_video(1, 0x22, "V_VP9", &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let am = dmx.video_alpha_mode(0).expect("video track");
    assert_eq!(am, AlphaMode::None);
    assert!(!am.has_alpha());
}

/// A track that carries an unregistered `AlphaMode` value (e.g. `7`)
/// surfaces via `AlphaMode::Other(7)` rather than being dropped — §27.8
/// leaves the registry open.
#[test]
fn alpha_mode_other_passthrough() {
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::ALPHA_MODE, 7));
    let t = video_track_with_video(1, 0x33, "V_VP9", &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let am = dmx.video_alpha_mode(0).expect("video track");
    assert_eq!(am, AlphaMode::Other(7));
    assert!(!am.has_alpha(), "Other must not report has_alpha");
}

/// An audio track (no `Video` master) returns `None` from
/// `video_alpha_mode` — the typed surface is video-only.
#[test]
fn alpha_mode_audio_track_returns_none() {
    let ta = audio_track(1, 0x55);
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::ALPHA_MODE, ids::ALPHA_MODE_PRESENT));
    let tv = video_track_with_video(2, 0x66, "V_VP9", &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &tv));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    assert!(
        dmx.video_alpha_mode(0).is_none(),
        "audio has no Video master"
    );
    assert_eq!(dmx.video_alpha_mode(1), Some(AlphaMode::Present));
}

// -- AspectRatioType (RFC 9559 Appendix A.24) -------------------------------

/// A track that carries `AspectRatioType = 2` surfaces the value verbatim
/// — the reclaimed appendix enumerates no values, so the demuxer exposes
/// the raw u64.
#[test]
fn aspect_ratio_type_present() {
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::ASPECT_RATIO_TYPE, 2));
    let t = video_track_with_video(1, 0x11, "V_VP9", &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    assert_eq!(dmx.video_aspect_ratio_type(0), Some(2));
    assert_eq!(dmx.video_aspect_ratio_types().len(), dmx.streams().len());
}

/// A `Video` master with no `AspectRatioType` returns `None` — the
/// reclaimed appendix specifies no default, so absence is *not* synthesised.
#[test]
fn aspect_ratio_type_absent_no_default() {
    let v = Vec::new();
    let t = video_track_with_video(1, 0x22, "V_VP9", &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    assert_eq!(dmx.video_aspect_ratio_type(0), None);
}

/// Audio tracks have no `Video` master and return `None`.
#[test]
fn aspect_ratio_type_audio_returns_none() {
    let ta = audio_track(1, 0xAA);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    assert_eq!(dmx.video_aspect_ratio_type(0), None);
}

// -- UncompressedFourCC (RFC 9559 §5.1.4.1.28.15) ---------------------------

/// A `V_UNCOMPRESSED` track that carries `UncompressedFourCC = b"YUY2"`
/// surfaces both the raw bytes and the 4-character string preview.
#[test]
fn uncompressed_fourcc_present() {
    let mut v = Vec::new();
    v.extend_from_slice(&elem_bin(ids::UNCOMPRESSED_FOURCC, b"YUY2"));
    let t = video_track_with_video(1, 0x11, "V_UNCOMPRESSED", &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let fcc = dmx
        .video_uncompressed_fourcc(0)
        .expect("video track carried UncompressedFourCC");
    assert_eq!(fcc.as_bytes(), b"YUY2");
    assert_eq!(fcc.fourcc(), Some(*b"YUY2"));
    assert_eq!(fcc.as_str().as_deref(), Some("YUY2"));
    assert_eq!(dmx.video_uncompressed_fourccs().len(), dmx.streams().len());
}

/// A `Video` master with no `UncompressedFourCC` returns `None` — the
/// spec specifies no default; the element is only mandatory when the
/// CodecID is `V_UNCOMPRESSED`, and absence on any track is legal.
#[test]
fn uncompressed_fourcc_absent_returns_none() {
    let v = Vec::new();
    let t = video_track_with_video(1, 0x22, "V_VP9", &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    assert!(dmx.video_uncompressed_fourcc(0).is_none());
}

/// A malformed non-4-byte payload (e.g. 3 bytes — the schema pins the
/// length at exactly 4) is preserved verbatim on `as_bytes()` so callers
/// can debug the deviation, while `fourcc()` and `as_str()` correctly
/// return `None`.
#[test]
fn uncompressed_fourcc_malformed_length_preserved() {
    let mut v = Vec::new();
    v.extend_from_slice(&elem_bin(ids::UNCOMPRESSED_FOURCC, b"YUV"));
    let t = video_track_with_video(1, 0x33, "V_UNCOMPRESSED", &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let fcc = dmx
        .video_uncompressed_fourcc(0)
        .expect("3-byte payload still surfaced verbatim");
    assert_eq!(fcc.as_bytes(), b"YUV");
    assert_eq!(fcc.fourcc(), None);
    assert_eq!(fcc.as_str(), None);
}

/// Audio tracks have no `Video` master and return `None`.
#[test]
fn uncompressed_fourcc_audio_returns_none() {
    let ta = audio_track(1, 0xAB);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    assert!(dmx.video_uncompressed_fourcc(0).is_none());
}

// -- Combined: all three elements on one track ------------------------------

/// One track carrying all three elements at once decodes each
/// independently — used to pin that the parser's match arms don't shadow
/// each other or share state.
#[test]
fn three_video_elements_together() {
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::ALPHA_MODE, ids::ALPHA_MODE_PRESENT));
    v.extend_from_slice(&elem_uint(ids::ASPECT_RATIO_TYPE, 1));
    v.extend_from_slice(&elem_bin(ids::UNCOMPRESSED_FOURCC, b"RGB\0"));
    let t = video_track_with_video(1, 0x77, "V_UNCOMPRESSED", &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    assert_eq!(dmx.video_alpha_mode(0), Some(AlphaMode::Present));
    assert_eq!(dmx.video_aspect_ratio_type(0), Some(1));
    let fcc = dmx.video_uncompressed_fourcc(0).expect("present");
    assert_eq!(fcc.fourcc(), Some(*b"RGB\0"));
}
