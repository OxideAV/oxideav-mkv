//! Zero-Cluster (metadata-only) Segment handling.
//!
//! The schema gives `Cluster` no `minOccurs`, so a Segment carrying only
//! Info / Tracks / Chapters / Tags / Attachments is a legal Matroska
//! file — e.g. a chapters-only sidecar. The demuxer opens it (strict
//! *and* resilient), surfaces every non-Cluster master through the usual
//! accessors, reports a clean `Error::Eof` from the first `next_packet`,
//! and refuses `seek_to` with `Error::Unsupported` (nothing to land on).
//! A zero-packet mux from the in-tree muxer round-trips the same way.

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;
use oxideav_mkv::mux::MkvMuxer;

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

fn elem_master(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
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

/// A metadata-only Matroska file: Info + Tracks + Chapters + Tags, no
/// Cluster anywhere.
fn metadata_only_file() -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info.extend_from_slice(&elem_str(ids::TITLE, "Sidecar"));
    let info = elem_master(ids::INFO, &info);

    let mut t = Vec::new();
    t.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    t.extend_from_slice(&elem_uint(ids::TRACK_UID, 0x42));
    t.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_SUBTITLE));
    t.extend_from_slice(&elem_str(ids::CODEC_ID, "S_TEXT/UTF8"));
    let tracks = elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &t));

    let mut atom = Vec::new();
    atom.extend_from_slice(&elem_uint(ids::CHAPTER_UID, 0xC1));
    atom.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, 0));
    let mut disp = Vec::new();
    disp.extend_from_slice(&elem_str(ids::CHAP_STRING, "Intro"));
    atom.extend_from_slice(&elem_master(ids::CHAPTER_DISPLAY, &disp));
    let chapters = elem_master(
        ids::CHAPTERS,
        &elem_master(ids::EDITION_ENTRY, &elem_master(ids::CHAPTER_ATOM, &atom)),
    );

    let mut simple_tag = Vec::new();
    simple_tag.extend_from_slice(&elem_str(ids::TAG_NAME, "COMMENT"));
    simple_tag.extend_from_slice(&elem_str(ids::TAG_STRING, "Zero Clusters"));
    let tag = elem_master(ids::TAG, &elem_master(ids::SIMPLE_TAG, &simple_tag));
    let tags = elem_master(ids::TAGS, &tag);

    let mut seg = Vec::new();
    seg.extend_from_slice(&info);
    seg.extend_from_slice(&tracks);
    seg.extend_from_slice(&chapters);
    seg.extend_from_slice(&tags);
    let mut out = ebml_header();
    out.extend_from_slice(&elem_master(ids::SEGMENT, &seg));
    out
}

fn open_strict(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("strict open")
}

#[test]
fn metadata_only_file_opens_strict_and_eofs() {
    let mut dmx = open_strict(metadata_only_file());
    assert_eq!(dmx.streams().len(), 1);
    // Non-Cluster masters all surfaced.
    let md: Vec<(String, String)> = dmx.metadata().to_vec();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(get("title").as_deref(), Some("Sidecar"));
    assert_eq!(get("chapter:1:title").as_deref(), Some("Intro"));
    assert_eq!(get("comment").as_deref(), Some("Zero Clusters"));
    assert_eq!(dmx.chapters().len(), 1);
    // First and every subsequent next_packet: clean Eof, no error class
    // drift, no panic.
    for _ in 0..3 {
        assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
    }
}

#[test]
fn metadata_only_file_opens_resilient_with_no_damage() {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(metadata_only_file()));
    let mut dmx = oxideav_mkv::demux::open_resilient_typed(rs, &oxideav_core::NullCodecResolver)
        .expect("resilient open");
    assert!(
        dmx.damage_events().is_empty(),
        "a legal zero-Cluster file is not damage: {:?}",
        dmx.damage_events()
    );
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
}

#[test]
fn zero_cluster_seek_is_unsupported_both_modes() {
    let mut strict = open_strict(metadata_only_file());
    assert!(matches!(strict.seek_to(0, 0), Err(Error::Unsupported(_))));

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(metadata_only_file()));
    let mut resilient =
        oxideav_mkv::demux::open_resilient_typed(rs, &oxideav_core::NullCodecResolver)
            .expect("resilient open");
    assert!(matches!(
        resilient.seek_to(0, 0),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn zero_cluster_unknown_size_segment_opens() {
    // Same metadata-only Segment but with the unknown-size VINT on the
    // Segment — the live-ish layout a cut-off recording produces.
    let full = metadata_only_file();
    let header = ebml_header();
    let seg_with_size = &full[header.len()..];
    // Segment id is 4 bytes; its size VINT follows. Rebuild with the
    // 1-byte unknown-size VINT 0xFF.
    let mut cur = Cursor::new(&seg_with_size[4..]);
    let (_, size_len) = oxideav_mkv::ebml::read_vint(&mut cur, false).expect("size vint");
    let body = &seg_with_size[4 + size_len..];
    let mut out = header;
    out.extend_from_slice(&write_element_id(ids::SEGMENT));
    out.push(0xFF);
    out.extend_from_slice(body);

    let mut dmx = open_strict(out);
    assert_eq!(dmx.streams().len(), 1);
    assert_eq!(dmx.chapters().len(), 1);
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
}

#[test]
fn zero_packet_mux_output_round_trips() {
    // The in-tree muxer's zero-packet output is a zero-Cluster file —
    // it must demux back cleanly.
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mkv-r434-zero-cluster-{}.mkv",
        std::process::id()
    ));
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let stream = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 1000),
            duration: None,
            start_time: Some(0),
            params: CodecParameters::audio(CodecId::new("pcm_s16le")),
        };
        let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
        mx.write_header().expect("write_header");
        // No packets.
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);

    let mut dmx = open_strict(bytes);
    assert_eq!(dmx.streams().len(), 1);
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
}
