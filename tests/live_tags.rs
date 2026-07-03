//! Mid-stream `Tags` — RFC 9559 §23.2 live tagging.
//!
//! §23.2: "In the context of live radio or web TV, it is possible to
//! 'tag' the content while it is playing. The Tags element can be placed
//! between Clusters each time it is necessary. In that case, the new
//! Tags element MUST reset the previously encountered Tags elements and
//! use the new values instead."
//!
//! Write side: `MkvMuxer::write_live_tags` (live-streaming muxers only)
//! emits a `Tags` element between Clusters. Read side: the demuxer
//! parses a `Tags` element it crosses during the Cluster walk and
//! applies the MUST-reset — the typed `tags()` slice is replaced and the
//! tag-derived flat-metadata entries are swapped, leaving Info- /
//! Chapters-derived metadata untouched. The same read path surfaces a
//! trailing `Tags` element placed after the last Cluster, which the
//! single-pass open walk never reaches.

use std::io::Cursor;

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo,
    TimeBase, WriteSeek,
};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;
use oxideav_mkv::mux::{MkvMuxer, MkvTag};

fn pcm_stream() -> StreamInfo {
    let mut ap = CodecParameters::audio(CodecId::new("pcm_s16le"));
    ap.sample_rate = Some(48_000);
    ap.channels = Some(2);
    ap.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: ap,
    }
}

fn packet(stream: &StreamInfo, pts_ms: i64) -> Packet {
    let mut p = Packet::new(0, stream.time_base, vec![pts_ms as u8; 32]);
    p.pts = Some(pts_ms);
    p.duration = Some(1000);
    p.flags.keyframe = true;
    p
}

fn metadata_value<'a>(meta: &'a [(String, String)], key: &str) -> Option<&'a str> {
    meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

#[test]
fn live_tags_reset_previous_tags_on_read() {
    let stream = pcm_stream();
    let tmp = std::env::temp_dir().join("oxideav-mkv-live-tags-reset.mkv");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = MkvMuxer::new_matroska(ws, std::slice::from_ref(&stream)).unwrap();
        mux.with_live_streaming().unwrap();
        // The up-front Tags master carries the initial now-playing value.
        mux.add_tag(MkvTag::global("NOW_PLAYING", "song-one"))
            .unwrap();
        mux.write_header().unwrap();
        // Two clusters' worth of packets, then a live tag update, then more.
        for i in 0..=6i64 {
            mux.write_packet(&packet(&stream, i * 1000)).unwrap();
        }
        mux.write_live_tags(&[MkvTag::global("NOW_PLAYING", "song-two")])
            .unwrap();
        for i in 7..=12i64 {
            mux.write_packet(&packet(&stream, i * 1000)).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();

    // Before the walk crosses the mid-stream Tags: the up-front value.
    assert_eq!(dmx.tags().len(), 1);
    assert_eq!(
        metadata_value(dmx.metadata(), "now_playing"),
        Some("song-one")
    );

    // Drain everything — the walk crosses the live Tags element.
    let mut pts = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => pts.push(p.pts.unwrap_or(-1)),
            Err(Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(pts.len(), 13, "no packet lost around the live Tags element");

    // §23.2 MUST-reset applied: the new value replaced the old one.
    assert_eq!(dmx.tags().len(), 1);
    assert_eq!(
        metadata_value(dmx.metadata(), "now_playing"),
        Some("song-two")
    );
    assert_eq!(
        dmx.metadata()
            .iter()
            .filter(|(k, _)| k == "now_playing")
            .count(),
        1,
        "the old entry was removed, not shadowed"
    );
}

#[test]
fn write_live_tags_validation() {
    let stream = pcm_stream();

    // Non-live muxer: rejected.
    let ws: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    let mut vod = MkvMuxer::new_matroska(ws, std::slice::from_ref(&stream)).unwrap();
    vod.write_header().unwrap();
    assert!(vod.write_live_tags(&[MkvTag::global("A", "b")]).is_err());

    // Live muxer before write_header: rejected.
    let ws: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    let mut live = MkvMuxer::new_matroska(ws, std::slice::from_ref(&stream)).unwrap();
    live.with_live_streaming().unwrap();
    assert!(live.write_live_tags(&[MkvTag::global("A", "b")]).is_err());

    // Malformed tag (no SimpleTag): rejected after header too.
    live.write_header().unwrap();
    let empty = MkvTag {
        simple_tags: Vec::new(),
        ..MkvTag::global("A", "b")
    };
    assert!(live.write_live_tags(&[empty]).is_err());
    // A valid call still works afterwards.
    live.write_live_tags(&[MkvTag::global("A", "b")]).unwrap();
}

// ---------------------------------------------------------------------
// Trailing Tags after the last Cluster (common Writer layout referenced
// from the SeekHead) — the walk-time parse surfaces it once the stream
// is drained. Hand-built, since the in-tree muxer places its Tags
// element up front.
// ---------------------------------------------------------------------

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

#[test]
fn trailing_tags_after_last_cluster_surface_after_drain() {
    let mut header = Vec::new();
    header.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    header.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    let ebml = elem_master(ids::EBML_HEADER, &header);

    let mut track = Vec::new();
    track.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    track.extend_from_slice(&elem_uint(ids::TRACK_UID, 0xA1));
    track.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    track.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let tracks = elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &track));

    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    let mut sb = Vec::new();
    sb.extend_from_slice(&write_vint(1, 0));
    sb.extend_from_slice(&0i16.to_be_bytes());
    sb.push(0x80);
    sb.extend_from_slice(&[0x42; 4]);
    cluster_body.extend_from_slice(&elem_master(ids::SIMPLE_BLOCK, &sb));
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    let mut st = Vec::new();
    st.extend_from_slice(&elem_str(ids::TAG_NAME, "COMMENT"));
    st.extend_from_slice(&elem_str(ids::TAG_STRING, "written-at-the-end"));
    let mut tag = Vec::new();
    tag.extend_from_slice(&elem_master(ids::TARGETS, &[]));
    tag.extend_from_slice(&elem_master(ids::SIMPLE_TAG, &st));
    let tags = elem_master(ids::TAGS, &elem_master(ids::TAG, &tag));

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&cluster);
    seg_body.extend_from_slice(&tags); // AFTER the last cluster
    let mut bytes = ebml;
    bytes.extend_from_slice(&elem_master(ids::SEGMENT, &seg_body));

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert!(
        dmx.tags().is_empty(),
        "open walk stops at the first Cluster"
    );
    loop {
        match dmx.next_packet() {
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        }
    }
    assert_eq!(dmx.tags().len(), 1, "trailing Tags surfaced by the walk");
    assert_eq!(
        metadata_value(dmx.metadata(), "comment"),
        Some("written-at-the-end")
    );
}
