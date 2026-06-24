//! Tests for the reclaimed Appendix-A `TrackEntry`-level legacy elements
//! (RFC 9559 Appendix A.19..A.23 + A.28..A.32), surfaced through
//! `MkvDemuxer::track_legacy` as a typed `TrackLegacy` record.
//!
//! These are historical Matroska `TrackEntry` children the RFC 9559 core body
//! no longer documents but whose Element IDs remain reserved in the registry
//! (Section 27.x) and which historical Writers still emit:
//!
//! * **Codec-description metadata** (A.19..A.22): `CodecSettings` (utf-8),
//!   `CodecInfoURL` / `CodecDownloadURL` (string, unbounded), `CodecDecodeAll`
//!   (uinteger).
//! * **`TrackOverlay`** (A.23, uinteger) — the *ordered* overlay-track
//!   fallback list ("the order of multiple TrackOverlay matters").
//! * **DivXTrickTrack pairing** (A.28..A.32): the Smooth FF/RW companion
//!   references (`TrickTrackUID` / `TrickTrackSegmentUID` / `TrickTrackFlag` /
//!   `TrickMasterTrackUID` / `TrickMasterTrackSegmentUID`).
//!
//! The container surfaces every value verbatim for a faithful re-mux and never
//! interprets it. None of the appendix entries carries a spec default or
//! range, so absence is always observable (`None` / empty `Vec` / `None`
//! accessor result).
//!
//! No third-party Matroska code is consulted — the production demuxer walks
//! every hand-assembled buffer.

use std::io::Cursor;

use oxideav_core::ReadSeek;
use oxideav_mkv::demux::TrackLegacy;
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

// ---- on-disk fixture helpers (mirror the other demux integration tests) ----

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

/// A minimal VP9 video track. `extra_body` is appended verbatim to the
/// `TrackEntry` body so callers can splice in the legacy elements.
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

// ----------------------------- demux tests -----------------------------

/// A `TrackEntry` with none of the legacy elements surfaces `None` — the
/// typed accessor never synthesises a hollow record.
#[test]
fn absent_legacy_is_none() {
    let tb = video_track(1, 0xB1, &[]);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));
    assert!(dmx.track_legacy(0).is_none(), "no legacy element → None");
    assert_eq!(dmx.all_track_legacy().len(), 1);
    assert!(dmx.all_track_legacy()[0].is_none());
}

/// The codec-description metadata quartet (A.19..A.22) decodes verbatim.
#[test]
fn codec_description_metadata() {
    let mut extra = Vec::new();
    extra.extend_from_slice(&elem_str(ids::CODEC_SETTINGS, "crf=18 preset=slow"));
    extra.extend_from_slice(&elem_str(ids::CODEC_INFO_URL, "https://example.test/info"));
    extra.extend_from_slice(&elem_str(
        ids::CODEC_DOWNLOAD_URL,
        "https://example.test/dl",
    ));
    extra.extend_from_slice(&elem_uint(ids::CODEC_DECODE_ALL, 1));
    let tb = video_track(1, 0xB2, &extra);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));

    let leg = dmx.track_legacy(0).expect("legacy record present");
    assert_eq!(leg.codec_settings.as_deref(), Some("crf=18 preset=slow"));
    assert_eq!(leg.codec_info_urls, vec!["https://example.test/info"]);
    assert_eq!(leg.codec_download_urls, vec!["https://example.test/dl"]);
    assert_eq!(leg.decode_all, Some(1));
    assert!(leg.can_decode_damaged(), "CodecDecodeAll=1 → true");
    assert!(!leg.is_empty());
}

/// `CodecDecodeAll` with an explicit `0` is observably distinct from absence:
/// `decode_all == Some(0)` while `can_decode_damaged()` is `false`.
#[test]
fn codec_decode_all_explicit_zero() {
    let extra = elem_uint(ids::CODEC_DECODE_ALL, 0);
    let tb = video_track(1, 0xB3, &extra);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));

    let leg = dmx.track_legacy(0).expect("legacy record present");
    assert_eq!(leg.decode_all, Some(0), "explicit 0 preserved");
    assert!(!leg.can_decode_damaged(), "0 → cannot decode damaged");
}

/// `CodecInfoURL` / `CodecDownloadURL` are unbounded and preserve on-disk
/// order.
#[test]
fn multiple_codec_urls_preserve_order() {
    let mut extra = Vec::new();
    extra.extend_from_slice(&elem_str(ids::CODEC_INFO_URL, "i1"));
    extra.extend_from_slice(&elem_str(ids::CODEC_INFO_URL, "i2"));
    extra.extend_from_slice(&elem_str(ids::CODEC_DOWNLOAD_URL, "d1"));
    extra.extend_from_slice(&elem_str(ids::CODEC_DOWNLOAD_URL, "d2"));
    extra.extend_from_slice(&elem_str(ids::CODEC_DOWNLOAD_URL, "d3"));
    let tb = video_track(1, 0xB4, &extra);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));

    let leg = dmx.track_legacy(0).expect("legacy record present");
    assert_eq!(leg.codec_info_urls, vec!["i1", "i2"]);
    assert_eq!(leg.codec_download_urls, vec!["d1", "d2", "d3"]);
}

/// `TrackOverlay` (A.23) is ordered — the first entry is the preferred
/// fallback. On-disk order is preserved exactly.
#[test]
fn track_overlay_order_is_load_bearing() {
    let mut extra = Vec::new();
    extra.extend_from_slice(&elem_uint(ids::TRACK_OVERLAY, 7));
    extra.extend_from_slice(&elem_uint(ids::TRACK_OVERLAY, 3));
    extra.extend_from_slice(&elem_uint(ids::TRACK_OVERLAY, 5));
    let tb = video_track(1, 0xB5, &extra);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));

    let leg = dmx.track_legacy(0).expect("legacy record present");
    assert_eq!(
        leg.track_overlays,
        vec![7, 3, 5],
        "preference order preserved verbatim"
    );
}

/// The DivXTrickTrack pairing quintet (A.28..A.32) decodes verbatim, with the
/// two SegmentUID binaries surfaced as raw bytes.
#[test]
fn divx_trick_track_quintet() {
    let seg_a = [0x11u8; 16];
    let seg_b = [0x22u8; 16];
    let mut extra = Vec::new();
    extra.extend_from_slice(&elem_uint(ids::TRICK_TRACK_UID, 0xDEAD));
    extra.extend_from_slice(&elem_bytes(ids::TRICK_TRACK_SEGMENT_UID, &seg_a));
    extra.extend_from_slice(&elem_uint(ids::TRICK_TRACK_FLAG, 1));
    extra.extend_from_slice(&elem_uint(ids::TRICK_MASTER_TRACK_UID, 0xBEEF));
    extra.extend_from_slice(&elem_bytes(ids::TRICK_MASTER_TRACK_SEGMENT_UID, &seg_b));
    let tb = video_track(1, 0xB6, &extra);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));

    let leg = dmx.track_legacy(0).expect("legacy record present");
    assert_eq!(leg.trick_track_uid, Some(0xDEAD));
    assert_eq!(leg.trick_track_segment_uid.as_deref(), Some(&seg_a[..]));
    assert_eq!(leg.trick_track_flag, Some(1));
    assert!(leg.is_trick_track(), "TrickTrackFlag=1 → trick track");
    assert_eq!(leg.trick_master_track_uid, Some(0xBEEF));
    assert_eq!(
        leg.trick_master_track_segment_uid.as_deref(),
        Some(&seg_b[..])
    );
}

/// `TrickTrackFlag` with an explicit `0` is distinct from absence.
#[test]
fn trick_track_flag_explicit_zero() {
    let extra = elem_uint(ids::TRICK_TRACK_FLAG, 0);
    let tb = video_track(1, 0xB7, &extra);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));

    let leg = dmx.track_legacy(0).expect("legacy record present");
    assert_eq!(leg.trick_track_flag, Some(0));
    assert!(!leg.is_trick_track());
}

/// A `TrackLegacy` carrying exactly one field is `Some(_)` (not all-absent),
/// and `is_empty()` reports `false`.
#[test]
fn single_field_is_not_empty() {
    let extra = elem_str(ids::CODEC_SETTINGS, "x");
    let tb = video_track(1, 0xB8, &extra);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));

    let leg = dmx.track_legacy(0).expect("legacy record present");
    assert!(!leg.is_empty());
    assert_eq!(
        leg,
        &TrackLegacy {
            codec_settings: Some("x".to_string()),
            ..Default::default()
        }
    );
}

/// An out-of-range `stream_index` returns `None` rather than panicking.
#[test]
fn out_of_range_stream_index_is_none() {
    let tb = video_track(1, 0xB9, &[]);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));
    assert!(dmx.track_legacy(99).is_none());
}

/// A `TrickTrackSegmentUID` with a non-16-byte payload is preserved verbatim
/// for inspection rather than truncated or dropped.
#[test]
fn trick_segment_uid_non_canonical_length_preserved() {
    let extra = elem_bytes(ids::TRICK_TRACK_SEGMENT_UID, &[0xAB, 0xCD, 0xEF]);
    let tb = video_track(1, 0xBA, &extra);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));

    let leg = dmx.track_legacy(0).expect("legacy record present");
    assert_eq!(
        leg.trick_track_segment_uid.as_deref(),
        Some(&[0xAB, 0xCD, 0xEF][..]),
        "off-length UID preserved verbatim"
    );
}
