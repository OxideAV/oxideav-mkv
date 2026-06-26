//! Integration tests for the EBML-header surface — `DocType`, the
//! `DocTypeVersion` / `DocTypeReadVersion` pair, and the `DocTypeExtension`
//! masters (RFC 8794 §11.2, including §11.2.9..§11.2.11).
//!
//! `DocTypeExtension` declares an extra (name, version) tuple that adds
//! Elements to the main `DocType` + `DocTypeVersion` — used to iterate
//! experimental elements before they integrate into a regular DocTypeVersion.
//! The demuxer surfaces the parsed header through `MkvDemuxer::ebml_header`;
//! the muxer writes extensions via `MkvMuxer::set_doc_type_extensions`. Each
//! demux test hand-builds an EBML byte sequence; the round-trip test
//! mux→demux's through the in-tree muxer.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::Muxer;
use oxideav_core::{CodecId, CodecParameters, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek};
use oxideav_mkv::demux::DocTypeExtension;
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;
use oxideav_mkv::mux::MkvMuxer;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path() -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r372-dtext-{}-{n}.mkv",
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

/// Build an EBML header with a configurable body (so tests can add / drop the
/// version elements and DocTypeExtension masters).
fn ebml_header_with(
    extra: &[u8],
    doc_type_version: Option<u64>,
    read_version: Option<u64>,
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    b.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    if let Some(v) = doc_type_version {
        b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, v));
    }
    if let Some(v) = read_version {
        b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, v));
    }
    b.extend_from_slice(extra);
    elem_master(ids::EBML_HEADER, &b)
}

fn doc_type_extension_bytes(name: &str, version: u64) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_str(ids::DOC_TYPE_EXTENSION_NAME, name));
    body.extend_from_slice(&elem_uint(ids::DOC_TYPE_EXTENSION_VERSION, version));
    elem_master(ids::DOC_TYPE_EXTENSION, &body)
}

fn minimal_segment() -> Vec<u8> {
    let mut ib = Vec::new();
    ib.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    ib.extend_from_slice(&elem_float_be_f64(ids::DURATION, 1000.0));
    let info = elem_master(ids::INFO, &ib);

    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, 0xAB));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_OPUS"));
    let track = elem_master(ids::TRACK_ENTRY, &tb);
    let tracks = elem_master(ids::TRACKS, &track);

    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cb.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    let cluster = elem_master(ids::CLUSTER, &cb);

    let mut seg = Vec::new();
    seg.extend_from_slice(&info);
    seg.extend_from_slice(&tracks);
    seg.extend_from_slice(&cluster);
    elem_master(ids::SEGMENT, &seg)
}

fn demux(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

/// A header carrying the version elements + two `DocTypeExtension` masters
/// surfaces them all through `ebml_header`, in document order.
#[test]
fn header_with_two_extensions() {
    let mut extra = Vec::new();
    extra.extend_from_slice(&doc_type_extension_bytes("webmproject.org/mkv/dav1d", 4));
    extra.extend_from_slice(&doc_type_extension_bytes("my-experiment", 1));

    let mut bytes = ebml_header_with(&extra, Some(4), Some(2));
    bytes.extend_from_slice(&minimal_segment());

    let dmx = demux(bytes);
    let h = dmx.ebml_header();
    assert_eq!(h.doc_type, "matroska");
    assert_eq!(h.doc_type_version, 4);
    assert_eq!(h.doc_type_read_version, 2);
    // Version/length quartet was written explicitly (1/1/4/8) in the header.
    assert_eq!(h.ebml_version, 1);
    assert_eq!(h.ebml_read_version, 1);
    assert_eq!(h.ebml_max_id_length, 4);
    assert_eq!(h.ebml_max_size_length, 8);
    assert_eq!(h.doc_type_extensions.len(), 2);
    assert_eq!(
        h.doc_type_extensions[0],
        DocTypeExtension {
            name: "webmproject.org/mkv/dav1d".to_string(),
            version: 4,
        }
    );
    assert_eq!(
        h.doc_type_extensions[1],
        DocTypeExtension {
            name: "my-experiment".to_string(),
            version: 1,
        }
    );
}

/// Absent `DocTypeVersion` / `DocTypeReadVersion` materialise the RFC 8794
/// spec default `1`, and a header with no extension surfaces an empty list.
#[test]
fn header_defaults_when_versions_absent() {
    let mut bytes = ebml_header_with(&[], None, None);
    bytes.extend_from_slice(&minimal_segment());

    let dmx = demux(bytes);
    let h = dmx.ebml_header();
    assert_eq!(h.doc_type_version, 1, "spec default materialised");
    assert_eq!(h.doc_type_read_version, 1, "spec default materialised");
    assert!(h.doc_type_extensions.is_empty());
}

/// A bare EBML header carrying only `DocType` (no EBMLVersion / EBMLReadVersion
/// / EBMLMaxIDLength / EBMLMaxSizeLength elements) materialises every RFC 8794
/// §11.2 spec default: 1 / 1 / 4 / 8.
#[test]
fn version_length_quartet_defaults_when_absent() {
    let mut hb = Vec::new();
    hb.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    let header = elem_master(ids::EBML_HEADER, &hb);
    let mut bytes = header;
    bytes.extend_from_slice(&minimal_segment());

    let dmx = demux(bytes);
    let h = dmx.ebml_header();
    assert_eq!(h.ebml_version, 1, "EBMLVersion default");
    assert_eq!(h.ebml_read_version, 1, "EBMLReadVersion default");
    assert_eq!(h.ebml_max_id_length, 4, "EBMLMaxIDLength default");
    assert_eq!(h.ebml_max_size_length, 8, "EBMLMaxSizeLength default");
}

/// A malformed `DocTypeExtension` is dropped: one missing its mandatory
/// `DocTypeExtensionName`, one with an empty name, one with a zero version.
/// Only the well-formed extension survives.
#[test]
fn malformed_extensions_dropped() {
    // Extension 1: missing name (only a version child).
    let mut e1_body = Vec::new();
    e1_body.extend_from_slice(&elem_uint(ids::DOC_TYPE_EXTENSION_VERSION, 3));
    let e1 = elem_master(ids::DOC_TYPE_EXTENSION, &e1_body);

    // Extension 2: empty name.
    let e2 = doc_type_extension_bytes("", 2);

    // Extension 3: zero version (range "not 0").
    let e3 = doc_type_extension_bytes("zero-ver", 0);

    // Extension 4: well-formed — the only survivor.
    let e4 = doc_type_extension_bytes("good", 7);

    let mut extra = Vec::new();
    extra.extend_from_slice(&e1);
    extra.extend_from_slice(&e2);
    extra.extend_from_slice(&e3);
    extra.extend_from_slice(&e4);

    let mut bytes = ebml_header_with(&extra, Some(4), Some(2));
    bytes.extend_from_slice(&minimal_segment());

    let dmx = demux(bytes);
    let h = dmx.ebml_header();
    assert_eq!(
        h.doc_type_extensions.len(),
        1,
        "only the well-formed extension survives"
    );
    assert_eq!(h.doc_type_extensions[0].name, "good");
    assert_eq!(h.doc_type_extensions[0].version, 7);
}

// ---- Mux write path + mux->demux round-trip ----

fn audio_stream() -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("opus"));
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

/// `set_doc_type_extensions` writes the masters into the EBML header, which
/// the demuxer reads back verbatim through `ebml_header`.
#[test]
fn mux_write_round_trips_through_demux() {
    let exts = vec![
        DocTypeExtension {
            name: "ext-alpha".to_string(),
            version: 2,
        },
        DocTypeExtension {
            name: "ext-beta".to_string(),
            version: 5,
        },
    ];

    let tmp = tmp_path();
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &[audio_stream()]).expect("muxer construct");
        mx.set_doc_type_extensions(exts.clone())
            .expect("set extensions");
        // Read-back accessor returns the queued list pre-write_header.
        assert_eq!(mx.doc_type_extensions(), exts.as_slice());
        mx.write_header().expect("write_header");
        let mut pkt = Packet::new(0, TimeBase::new(1, 48_000), vec![0xAA, 0xBB]);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mx.write_packet(&pkt).expect("write_packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);

    let dmx = demux(bytes);
    let h = dmx.ebml_header();
    assert_eq!(h.doc_type_extensions, exts);
    // Muxer writes the version/length quartet at the canonical 1/1/4/8.
    assert_eq!(h.ebml_version, 1);
    assert_eq!(h.ebml_read_version, 1);
    assert_eq!(h.ebml_max_id_length, 4);
    assert_eq!(h.ebml_max_size_length, 8);
    assert_eq!(h.doc_type, "matroska");
}

/// Omitting the call keeps the header free of `DocTypeExtension` masters.
#[test]
fn mux_omits_extensions_by_default() {
    let tmp = tmp_path();
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &[audio_stream()]).expect("muxer construct");
        assert!(mx.doc_type_extensions().is_empty());
        mx.write_header().expect("write_header");
        let mut pkt = Packet::new(0, TimeBase::new(1, 48_000), vec![0xAA, 0xBB]);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mx.write_packet(&pkt).expect("write_packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);

    let dmx = demux(bytes);
    assert!(dmx.ebml_header().doc_type_extensions.is_empty());
}

/// The setter rejects an empty name, a zero version, a duplicate name, and
/// post-`write_header` use.
#[test]
fn mux_setter_validation() {
    let ws: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::<u8>::new()));
    let mut mx = MkvMuxer::new_matroska(ws, &[audio_stream()]).expect("muxer construct");

    // Empty name rejected.
    assert!(mx
        .set_doc_type_extensions(vec![DocTypeExtension {
            name: String::new(),
            version: 1,
        }])
        .is_err());

    // Zero version rejected.
    assert!(mx
        .set_doc_type_extensions(vec![DocTypeExtension {
            name: "x".to_string(),
            version: 0,
        }])
        .is_err());

    // Duplicate name rejected.
    assert!(mx
        .set_doc_type_extensions(vec![
            DocTypeExtension {
                name: "dup".to_string(),
                version: 1,
            },
            DocTypeExtension {
                name: "dup".to_string(),
                version: 2,
            },
        ])
        .is_err());

    // A valid call still succeeds after the rejected ones.
    assert!(mx
        .set_doc_type_extensions(vec![DocTypeExtension {
            name: "ok".to_string(),
            version: 1,
        }])
        .is_ok());

    // Post-write_header use rejected.
    let ws2: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::<u8>::new()));
    let mut mx2 = MkvMuxer::new_matroska(ws2, &[audio_stream()]).expect("muxer construct");
    mx2.write_header().expect("write_header");
    assert!(mx2
        .set_doc_type_extensions(vec![DocTypeExtension {
            name: "late".to_string(),
            version: 1,
        }])
        .is_err());
}
