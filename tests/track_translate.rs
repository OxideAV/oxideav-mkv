//! Tests for `TrackTranslate` (RFC 9559 §5.1.4.1.27) — the per-`TrackEntry`
//! chapter-codec track-mapping master.
//!
//! `TrackTranslate` maps a `TrackEntry` to the track value a Chapter Codec
//! (DVD-menu, Matroska Script) uses to name it, so a file can be remuxed
//! (acquiring new `TrackNumber` / `TrackUID` values) without rewriting the
//! opaque chapter-codec command data — only the mapping changes. It is the
//! `TrackEntry`-level twin of `Info\ChapterTranslate` and is **unbounded** (a
//! single `TrackEntry` may carry several mappings).
//!
//! Two surfaces are pinned:
//!
//! * **Demux** — hand-assembled on-disk bytes are walked by the production
//!   demuxer; `MkvDemuxer::track_translates(stream_index)` decodes the exact
//!   mappings, with `TrackTranslateTrackID` surfaced verbatim (it is *not* a
//!   Matroska `TrackUID`), the mandatory `TrackTranslateCodec`, and the
//!   unbounded `TrackTranslateEditionUID` list.
//! * **Mux round-trip** — `MkvMuxer::set_track_translates` writes the master;
//!   re-opening through `open_typed` recovers every field byte-for-byte. The
//!   setter's spec-rule rejections (post-`write_header`, out-of-range stream,
//!   empty `track_id`, zero edition UID) are exercised too.
//!
//! No third-party Matroska code is consulted — the production demuxer walks
//! every muxed buffer.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::demux::TrackTranslate;
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;
use oxideav_mkv::mux::{MkvMuxer, MkvTrackTranslate};

// ---- on-disk fixture helpers (mirrors the other demux integration tests) ----

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
/// `TrackEntry` body so callers can splice in `TrackTranslate` masters.
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

/// Build one `TrackTranslate` master from its three fields.
fn track_translate_master(track_id: &[u8], codec: u64, edition_uids: &[u64]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_bytes(ids::TRACK_TRANSLATE_TRACK_ID, track_id));
    body.extend_from_slice(&elem_uint(ids::TRACK_TRANSLATE_CODEC, codec));
    for &u in edition_uids {
        body.extend_from_slice(&elem_uint(ids::TRACK_TRANSLATE_EDITION_UID, u));
    }
    elem_master(ids::TRACK_TRANSLATE, &body)
}

// ----------------------------- demux tests -----------------------------

/// A `TrackEntry` with no `TrackTranslate` child surfaces as an empty slice —
/// the common case.
#[test]
fn absent_track_translate_is_empty_slice() {
    let tb = video_track(1, 0xB1, &[]);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));
    assert!(
        dmx.track_translates(0).is_empty(),
        "no TrackTranslate → empty slice"
    );
    assert_eq!(dmx.all_track_translates().len(), 1);
    assert!(dmx.all_track_translates()[0].is_empty());
}

/// One `TrackTranslate` with all children present. `TrackTranslateTrackID` is
/// surfaced verbatim (it is the chapter-codec's own opaque value, not a
/// Matroska `TrackUID`).
#[test]
fn single_track_translate_all_children() {
    let master = track_translate_master(&[0x01, 0x02, 0x03], 1, &[0xCAFE, 0xBEEF]);
    let tb = video_track(1, 0xB2, &master);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));

    let tts = dmx.track_translates(0);
    assert_eq!(tts.len(), 1, "one mapping");
    assert_eq!(
        tts[0],
        TrackTranslate {
            track_id: vec![0x01, 0x02, 0x03],
            codec: 1,
            edition_uids: vec![0xCAFE, 0xBEEF],
        }
    );
}

/// A `TrackTranslate` with only the two mandatory children — empty
/// `edition_uids` means "applies to all editions using the codec" per the
/// §5.1.4.1.27.3 usage note.
#[test]
fn track_translate_without_edition_uids() {
    let master = track_translate_master(b"DVD", 0, &[]);
    let tb = video_track(1, 0xB3, &master);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));

    let tts = dmx.track_translates(0);
    assert_eq!(tts.len(), 1);
    assert_eq!(tts[0].track_id, b"DVD");
    assert_eq!(tts[0].codec, 0);
    assert!(
        tts[0].edition_uids.is_empty(),
        "no TrackTranslateEditionUID → empty list (all editions)"
    );
}

/// Multiple `TrackTranslate` masters on one `TrackEntry` are preserved in
/// on-disk order (the element is unbounded).
#[test]
fn multiple_track_translates_preserve_order() {
    let mut extra = Vec::new();
    extra.extend_from_slice(&track_translate_master(&[0xAA], 0, &[1]));
    extra.extend_from_slice(&track_translate_master(&[0xBB], 1, &[2, 3]));
    extra.extend_from_slice(&track_translate_master(&[0xCC], 1, &[]));
    let tb = video_track(1, 0xB4, &extra);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));

    let tts = dmx.track_translates(0);
    assert_eq!(tts.len(), 3);
    assert_eq!(tts[0].track_id, vec![0xAA]);
    assert_eq!(tts[0].edition_uids, vec![1]);
    assert_eq!(tts[1].track_id, vec![0xBB]);
    assert_eq!(tts[1].edition_uids, vec![2, 3]);
    assert_eq!(tts[2].track_id, vec![0xCC]);
    assert!(tts[2].edition_uids.is_empty());
}

/// An out-of-range `stream_index` surfaces as an empty slice rather than a
/// panic.
#[test]
fn out_of_range_stream_index_is_empty() {
    let tb = video_track(1, 0xB5, &[]);
    let dmx = open(assemble(&elem_master(ids::TRACK_ENTRY, &tb)));
    assert!(dmx.track_translates(99).is_empty());
}

// --------------------------- mux round-trip ----------------------------

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r330-tracktrans-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

fn video_stream(index: u32) -> StreamInfo {
    let mut p = CodecParameters::video(CodecId::new("vp9"));
    p.width = Some(320);
    p.height = Some(240);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn keyframe_packet(stream: u32, pts: i64) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![0xAA; 16]);
    p.pts = Some(pts);
    p.flags.keyframe = true;
    p
}

/// Mux a two-video-track MKV. `configure` runs between construction and
/// `write_header`.
fn mux_two_tracks<F>(configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let streams = vec![video_stream(0), video_stream(1)];
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer construct");
        configure(&mut mx);
        mx.write_header().expect("write_header");
        mx.write_packet(&keyframe_packet(0, 0)).expect("write 0");
        mx.write_packet(&keyframe_packet(1, 0)).expect("write 1");
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

/// A mux→demux round-trip preserves every `TrackTranslate` field. Two
/// mappings on stream 0, none on stream 1.
#[test]
fn roundtrip_track_translates() {
    let bytes = mux_two_tracks(|mx| {
        mx.set_track_translates(
            0,
            vec![
                MkvTrackTranslate {
                    track_id: vec![0x10, 0x20],
                    codec: 1,
                    edition_uids: vec![7, 9],
                },
                MkvTrackTranslate::new(b"vts01".to_vec(), 0),
            ],
        )
        .expect("set_track_translates");
    });
    let dmx = demux_typed(bytes);

    let tts = dmx.track_translates(0);
    assert_eq!(tts.len(), 2);
    assert_eq!(tts[0].track_id, vec![0x10, 0x20]);
    assert_eq!(tts[0].codec, 1);
    assert_eq!(tts[0].edition_uids, vec![7, 9]);
    assert_eq!(tts[1].track_id, b"vts01");
    assert_eq!(tts[1].codec, 0);
    assert!(tts[1].edition_uids.is_empty());

    assert!(
        dmx.track_translates(1).is_empty(),
        "stream 1 had no TrackTranslate"
    );
}

/// Omitting the call keeps the master off-disk so the on-disk bytes carry no
/// `TrackTranslate` (`0x6624`) element id and the demuxer surfaces an empty
/// slice.
#[test]
fn omitted_call_writes_nothing() {
    let bytes = mux_two_tracks(|_mx| {});
    // 0x6624 -> big-endian id bytes 0x66 0x24.
    let needle = [0x66u8, 0x24];
    assert!(
        !bytes.windows(2).any(|w| w == needle),
        "no TrackTranslate id on disk when the API was not called"
    );
    let dmx = demux_typed(bytes);
    assert!(dmx.track_translates(0).is_empty());
    assert!(dmx.track_translates(1).is_empty());
}

/// The read-back accessor returns the queued hints pre-`write_header`.
#[test]
fn read_back_queued_hints() {
    let tmp = tmp_path("readback");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0), video_stream(1)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer construct");
    mx.set_track_translates(0, vec![MkvTrackTranslate::new(vec![0x01], 1)])
        .expect("set");
    assert_eq!(mx.track_translates(0).len(), 1);
    assert_eq!(mx.track_translates(0)[0].track_id, vec![0x01]);
    assert!(mx.track_translates(1).is_empty());
    let _ = std::fs::remove_file(&tmp);
}

/// Spec-rule rejections: out-of-range stream, empty `track_id`, zero edition
/// UID, and post-`write_header` use.
#[test]
fn setter_rejects_spec_violations() {
    let tmp = tmp_path("reject");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0), video_stream(1)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer construct");

    // Out-of-range stream index.
    assert!(matches!(
        mx.set_track_translates(9, vec![MkvTrackTranslate::new(vec![0x01], 0)]),
        Err(Error::InvalidData(_))
    ));
    // Empty TrackTranslateTrackID (mandatory child).
    assert!(matches!(
        mx.set_track_translates(0, vec![MkvTrackTranslate::new(Vec::new(), 0)]),
        Err(Error::InvalidData(_))
    ));
    // Zero TrackTranslateEditionUID ("not 0").
    assert!(matches!(
        mx.set_track_translates(
            0,
            vec![MkvTrackTranslate {
                track_id: vec![0x01],
                codec: 0,
                edition_uids: vec![0],
            }]
        ),
        Err(Error::InvalidData(_))
    ));

    // A valid call before write_header is accepted; a clearing empty-vec call
    // is also accepted and removes the queued mapping.
    mx.set_track_translates(0, vec![MkvTrackTranslate::new(vec![0x01], 0)])
        .expect("valid set");
    mx.set_track_translates(0, Vec::new()).expect("clear set");
    assert!(mx.track_translates(0).is_empty());

    mx.write_header().expect("write_header");
    // After write_header the setter rejects.
    assert!(mx
        .set_track_translates(0, vec![MkvTrackTranslate::new(vec![0x01], 0)])
        .is_err());
    let _ = std::fs::remove_file(&tmp);
}
