//! Integration tests for the demuxer's `ContentEncodings` parsing
//! (RFC 9559 §5.1.4.1.31).
//!
//! A track's `ContentEncodings` describes the chain of transformations —
//! compression and/or encryption — applied to its frame data and/or
//! `CodecPrivate` before the bytes were written into Blocks:
//!
//! * `ContentEncoding` (§5.1.4.1.31.1) — one step, carrying
//!   `ContentEncodingOrder` (§5.1.4.1.31.2), `ContentEncodingScope`
//!   (§5.1.4.1.31.3), and `ContentEncodingType` (§5.1.4.1.31.4) selecting
//!   `ContentCompression` (§5.1.4.1.31.5) vs `ContentEncryption`
//!   (§5.1.4.1.31.8).
//! * Compression carries `ContentCompAlgo` (§5.1.4.1.31.6) +
//!   `ContentCompSettings` (§5.1.4.1.31.7).
//! * Encryption carries `ContentEncAlgo` (§5.1.4.1.31.9),
//!   `ContentEncKeyID` (§5.1.4.1.31.10), and `ContentEncAESSettings`
//!   (§5.1.4.1.31.11) → `AESSettingsCipherMode` (§5.1.4.1.31.12).
//!
//! The container surfaces these *headers* only — it never decompresses or
//! decrypts a frame. Encodings are returned through
//! `MkvDemuxer::content_encodings(stream_index)` /
//! `all_content_encodings()`, sorted into decode order (descending
//! `ContentEncodingOrder`).

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_mkv::demux::{
    AesCipherMode, ContentCompAlgo, ContentEncAlgo, ContentEncodingTransform,
};
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

fn elem_bin(id: u32, bytes: &[u8]) -> Vec<u8> {
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

/// A plain video track header, optionally extended by `extra` (e.g. a
/// `ContentEncodings` master).
fn video_track(number: u64, uid: u64, extra: &[u8]) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_VIDEO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "V_VP9"));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::PIXEL_WIDTH, 320));
    v.extend_from_slice(&elem_uint(ids::PIXEL_HEIGHT, 240));
    tb.extend_from_slice(&elem_master(ids::VIDEO, &v));
    tb.extend_from_slice(extra);
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

/// Build a `ContentEncoding` for header stripping (ContentCompAlgo=3) with
/// the given order, scope and stripped bytes.
fn header_stripping_encoding(order: u64, scope: u64, stripped: &[u8]) -> Vec<u8> {
    let mut comp = Vec::new();
    comp.extend_from_slice(&elem_uint(
        ids::CONTENT_COMP_ALGO,
        ids::CONTENT_COMP_ALGO_HEADER_STRIPPING,
    ));
    comp.extend_from_slice(&elem_bin(ids::CONTENT_COMP_SETTINGS, stripped));
    let mut ce = Vec::new();
    ce.extend_from_slice(&elem_uint(ids::CONTENT_ENCODING_ORDER, order));
    ce.extend_from_slice(&elem_uint(ids::CONTENT_ENCODING_SCOPE, scope));
    ce.extend_from_slice(&elem_uint(
        ids::CONTENT_ENCODING_TYPE,
        ids::CONTENT_ENCODING_TYPE_COMPRESSION,
    ));
    ce.extend_from_slice(&elem_master(ids::CONTENT_COMPRESSION, &comp));
    elem_master(ids::CONTENT_ENCODING, &ce)
}

/// Build an AES-CTR `ContentEncoding` (encryption) with the given order,
/// key id, and cipher mode.
fn aes_encryption_encoding(order: u64, key_id: &[u8], cipher_mode: u64) -> Vec<u8> {
    let mut aes = Vec::new();
    aes.extend_from_slice(&elem_uint(ids::AES_SETTINGS_CIPHER_MODE, cipher_mode));
    let mut encr = Vec::new();
    encr.extend_from_slice(&elem_uint(ids::CONTENT_ENC_ALGO, ids::CONTENT_ENC_ALGO_AES));
    encr.extend_from_slice(&elem_bin(ids::CONTENT_ENC_KEY_ID, key_id));
    encr.extend_from_slice(&elem_master(ids::CONTENT_ENC_AES_SETTINGS, &aes));
    let mut ce = Vec::new();
    ce.extend_from_slice(&elem_uint(ids::CONTENT_ENCODING_ORDER, order));
    ce.extend_from_slice(&elem_uint(
        ids::CONTENT_ENCODING_TYPE,
        ids::CONTENT_ENCODING_TYPE_ENCRYPTION,
    ));
    ce.extend_from_slice(&elem_master(ids::CONTENT_ENCRYPTION, &encr));
    elem_master(ids::CONTENT_ENCODING, &ce)
}

fn open(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

/// A single header-stripping ContentEncoding decodes into a Compression
/// transform carrying the algorithm and the stripped settings bytes.
#[test]
fn header_stripping_decodes() {
    let stripped = [0xAA, 0xBB, 0xCC];
    let enc = header_stripping_encoding(0, ids::CONTENT_ENCODING_SCOPE_BLOCK, &stripped);
    let track = video_track(1, 0x1, &elem_master(ids::CONTENT_ENCODINGS, &enc));

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &track));
    let dmx = open(assemble(&tracks_body));

    let ce = dmx.content_encodings(0).expect("track has encodings");
    assert!(!ce.is_empty());
    assert_eq!(ce.encodings.len(), 1);
    let e = &ce.encodings[0];
    assert_eq!(e.order, 0);
    assert!(e.scope.block(), "scope = Block");
    assert!(!e.scope.private());
    match &e.transform {
        ContentEncodingTransform::Compression { algo, settings } => {
            assert_eq!(*algo, ContentCompAlgo::HeaderStripping);
            assert_eq!(settings, &stripped, "stripped bytes preserved");
        }
        other => panic!("expected Compression, got {other:?}"),
    }
    // Slice view has one entry per stream.
    assert_eq!(dmx.all_content_encodings().len(), dmx.streams().len());
}

/// An AES-CTR ContentEncryption decodes into an Encryption transform with
/// the algorithm, key id, and cipher mode all surfaced.
#[test]
fn aes_encryption_decodes() {
    let key = [0x01, 0x02, 0x03, 0x04];
    let enc = aes_encryption_encoding(0, &key, ids::AES_CIPHER_MODE_CTR);
    let track = video_track(1, 0x1, &elem_master(ids::CONTENT_ENCODINGS, &enc));

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &track));
    let dmx = open(assemble(&tracks_body));

    let ce = dmx.content_encodings(0).expect("track has encodings");
    assert_eq!(ce.encodings.len(), 1);
    match &ce.encodings[0].transform {
        ContentEncodingTransform::Encryption {
            algo,
            key_id,
            aes_cipher_mode,
        } => {
            assert_eq!(*algo, ContentEncAlgo::Aes);
            assert_eq!(key_id, &key);
            assert_eq!(*aes_cipher_mode, Some(AesCipherMode::Ctr));
        }
        other => panic!("expected Encryption, got {other:?}"),
    }
}

/// Two encodings on one track are returned sorted by **descending** order
/// (decode order per §5.1.4.1.31.2: highest order first), regardless of
/// on-disk order. Here a low-order (0) compression and a high-order (1)
/// encryption are written compression-first but must come out
/// encryption-first.
#[test]
fn multiple_encodings_sorted_into_decode_order() {
    let comp = header_stripping_encoding(0, ids::CONTENT_ENCODING_SCOPE_BLOCK, &[0xFF]);
    let encr = aes_encryption_encoding(1, &[0xAB], ids::AES_CIPHER_MODE_CBC);
    let mut body = Vec::new();
    body.extend_from_slice(&comp); // order 0 written first
    body.extend_from_slice(&encr); // order 1 written second
    let track = video_track(1, 0x1, &elem_master(ids::CONTENT_ENCODINGS, &body));

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &track));
    let dmx = open(assemble(&tracks_body));

    let ce = dmx.content_encodings(0).expect("track has encodings");
    assert_eq!(ce.encodings.len(), 2);
    // Highest order (1, encryption) first.
    assert_eq!(ce.encodings[0].order, 1);
    assert!(matches!(
        ce.encodings[0].transform,
        ContentEncodingTransform::Encryption { .. }
    ));
    // Then order 0 (compression).
    assert_eq!(ce.encodings[1].order, 0);
    assert!(matches!(
        ce.encodings[1].transform,
        ContentEncodingTransform::Compression { .. }
    ));
}

/// Element defaults are applied when children are omitted: a
/// `ContentEncoding` with only a `ContentCompression` (no order/scope/type)
/// uses order 0, scope 0x1 (Block), type 0 (compression); a
/// `ContentCompression` with no `ContentCompAlgo` defaults to zlib (0).
#[test]
fn defaults_applied_for_omitted_children() {
    // ContentCompression master with NO ContentCompAlgo child.
    let comp = elem_master(ids::CONTENT_COMPRESSION, &[]);
    let ce = elem_master(ids::CONTENT_ENCODING, &comp);
    let track = video_track(1, 0x1, &elem_master(ids::CONTENT_ENCODINGS, &ce));

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &track));
    let dmx = open(assemble(&tracks_body));

    let ce = dmx.content_encodings(0).expect("track has encodings");
    let e = &ce.encodings[0];
    assert_eq!(e.order, 0, "ContentEncodingOrder default 0");
    assert!(e.scope.block(), "ContentEncodingScope default 0x1 (Block)");
    match &e.transform {
        ContentEncodingTransform::Compression { algo, settings } => {
            assert_eq!(*algo, ContentCompAlgo::Zlib, "ContentCompAlgo default 0");
            assert!(settings.is_empty());
        }
        other => panic!("expected Compression default, got {other:?}"),
    }
}

/// Scope bit field with multiple bits set (Block | Private = 0x3) reports
/// both via the accessors.
#[test]
fn scope_bitfield_multiple_bits() {
    let scope = ids::CONTENT_ENCODING_SCOPE_BLOCK | ids::CONTENT_ENCODING_SCOPE_PRIVATE; // 0x3
    let enc = header_stripping_encoding(0, scope, &[0x10]);
    let track = video_track(1, 0x1, &elem_master(ids::CONTENT_ENCODINGS, &enc));

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &track));
    let dmx = open(assemble(&tracks_body));

    let s = dmx.content_encodings(0).expect("encodings").encodings[0].scope;
    assert!(s.block());
    assert!(s.private());
    assert!(!s.next());
    assert_eq!(s.0, 0x3);
}

/// Unrecognised algorithm / cipher-mode values round-trip through the
/// `Other` variants rather than being lost or mis-mapped.
#[test]
fn unknown_values_preserved_as_other() {
    // Compression with an algo of 99 (unregistered).
    let mut comp = Vec::new();
    comp.extend_from_slice(&elem_uint(ids::CONTENT_COMP_ALGO, 99));
    let ce = elem_master(
        ids::CONTENT_ENCODING,
        &elem_master(ids::CONTENT_COMPRESSION, &comp),
    );
    let track = video_track(1, 0x1, &elem_master(ids::CONTENT_ENCODINGS, &ce));

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &track));
    let dmx = open(assemble(&tracks_body));

    match &dmx.content_encodings(0).expect("encodings").encodings[0].transform {
        ContentEncodingTransform::Compression { algo, .. } => {
            assert_eq!(*algo, ContentCompAlgo::Other(99));
        }
        other => panic!("expected Compression, got {other:?}"),
    }
}

/// A non-AES encryption algorithm has no AESSettings, so the cipher mode is
/// `None` even though the encoding is still surfaced.
#[test]
fn non_aes_encryption_has_no_cipher_mode() {
    let mut encr = Vec::new();
    encr.extend_from_slice(&elem_uint(
        ids::CONTENT_ENC_ALGO,
        ids::CONTENT_ENC_ALGO_TWOFISH,
    ));
    let mut ce = Vec::new();
    ce.extend_from_slice(&elem_uint(
        ids::CONTENT_ENCODING_TYPE,
        ids::CONTENT_ENCODING_TYPE_ENCRYPTION,
    ));
    ce.extend_from_slice(&elem_master(ids::CONTENT_ENCRYPTION, &encr));
    let enc = elem_master(ids::CONTENT_ENCODING, &ce);
    let track = video_track(1, 0x1, &elem_master(ids::CONTENT_ENCODINGS, &enc));

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &track));
    let dmx = open(assemble(&tracks_body));

    match &dmx.content_encodings(0).expect("encodings").encodings[0].transform {
        ContentEncodingTransform::Encryption {
            algo,
            key_id,
            aes_cipher_mode,
        } => {
            assert_eq!(*algo, ContentEncAlgo::Twofish);
            assert!(key_id.is_empty());
            assert_eq!(*aes_cipher_mode, None);
        }
        other => panic!("expected Encryption, got {other:?}"),
    }
}

/// A track with no `ContentEncodings` reports `None`; out-of-range indices
/// also report `None`.
#[test]
fn no_content_encodings_present() {
    let track = video_track(1, 0x1, &[]);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &track));
    let dmx = open(assemble(&tracks_body));

    assert_eq!(dmx.all_content_encodings().len(), 1);
    assert!(dmx.content_encodings(0).is_none());
    assert!(dmx.content_encodings(99).is_none());
}
