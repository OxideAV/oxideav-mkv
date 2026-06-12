//! Typed per-Block `BlockAdditions` decode — RFC 9559 §5.1.3.5.2
//! (`BlockMore` §5.1.3.5.2.1, `BlockAdditional` §5.1.3.5.2.2,
//! `BlockAddID` §5.1.3.5.2.3) — plus the per-track `MaxBlockAdditionID`
//! declaration (§5.1.4.1.16).
//!
//! We hand-build minimal MKV byte streams whose `BlockGroup`s carry
//! `BlockAdditions` masters in every interesting shape and assert
//! [`MkvDemuxer::block_additions`] surfaces them faithfully alongside
//! each returned packet:
//!
//! 1. Additions surface in on-disk `BlockMore` order; an omitted
//!    `BlockAddID` materialises the §5.1.3.5.2.3 default `1`.
//! 2. A `SimpleBlock` packet (the element only exists on `BlockGroup`)
//!    and a `BlockGroup` without the master both surface an empty slice
//!    — and a previous packet's additions don't leak forward.
//! 3. Malformed `BlockMore`s are dropped: no `BlockAdditional` child
//!    (mandatory, §5.1.3.5.2.2), `BlockAddID == 0` (range "not 0"),
//!    and a duplicate `BlockAddID` (uniqueness MUST) keeping the first.
//! 4. Every frame de-laced from a laced Block shares the Block's
//!    additions — the spec attaches the master to the Block as a whole.
//! 5. `MaxBlockAdditionID` (§5.1.4.1.16) surfaces per stream with the
//!    spec default `0` materialised on absence.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
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

fn elem_bytes(id: u32, b: &[u8]) -> Vec<u8> {
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

fn info_body() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 1000.0));
    body
}

/// One PCM audio track (TrackNumber 1), optionally carrying an explicit
/// `MaxBlockAdditionID` (RFC 9559 §5.1.4.1.16) element.
fn tracks_body(max_block_addition_id: Option<u64>) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, 0xA1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    if let Some(m) = max_block_addition_id {
        tb.extend_from_slice(&elem_uint(ids::MAX_BLOCK_ADDITION_ID, m));
    }
    let mut audio = Vec::new();
    audio.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    tb.extend_from_slice(&elem_master(ids::AUDIO, &audio));
    elem_master(ids::TRACK_ENTRY, &tb)
}

/// A plain (unlaced) `Block` element (RFC 9559 §5.1.3.5.1) on track 1.
fn block(tc_offset: i16, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(1, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(0x00); // no lacing; plain Block has no keyframe bit
    body.extend_from_slice(payload);
    elem_bytes(ids::BLOCK, &body)
}

/// A Xiph-laced (lacing bits 01) `Block` element with two frames on
/// track 1: `frame_a` (explicit size octet) + `frame_b` (implicit).
fn block_xiph_two_frames(tc_offset: i16, frame_a: &[u8], frame_b: &[u8]) -> Vec<u8> {
    assert!(frame_a.len() < 255);
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(1, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(0x02); // lacing bits (5..6) = 01 → Xiph
    body.push(1); // n_frames - 1
    body.push(frame_a.len() as u8);
    body.extend_from_slice(frame_a);
    body.extend_from_slice(frame_b);
    elem_bytes(ids::BLOCK, &body)
}

/// A `BlockMore` master (RFC 9559 §5.1.3.5.2.1). `id == None` omits the
/// `BlockAddID` child (spec default 1); `data == None` omits the
/// mandatory `BlockAdditional` (a malformed shape the demuxer drops).
fn block_more(id: Option<u64>, data: Option<&[u8]>) -> Vec<u8> {
    let mut body = Vec::new();
    if let Some(d) = data {
        body.extend_from_slice(&elem_bytes(ids::BLOCK_ADDITIONAL, d));
    }
    if let Some(v) = id {
        body.extend_from_slice(&elem_uint(ids::BLOCK_ADD_ID, v));
    }
    elem_master(ids::BLOCK_MORE, &body)
}

fn block_group(block_elem: &[u8], more: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(block_elem);
    if !more.is_empty() {
        let mut additions = Vec::new();
        for m in more {
            additions.extend_from_slice(m);
        }
        body.extend_from_slice(&elem_master(ids::BLOCK_ADDITIONS, &additions));
    }
    elem_master(ids::BLOCK_GROUP, &body)
}

fn simple_block(tc_offset: i16, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(1, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(0x80); // keyframe, no lacing
    body.extend_from_slice(payload);
    elem_bytes(ids::SIMPLE_BLOCK, &body)
}

fn build_segment(max_block_addition_id: Option<u64>, cluster_children: &[Vec<u8>]) -> Vec<u8> {
    let info = elem_master(ids::INFO, &info_body());
    let tracks = elem_master(ids::TRACKS, &tracks_body(max_block_addition_id));

    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    for c in cluster_children {
        cluster_body.extend_from_slice(c);
    }

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&elem_master(ids::CLUSTER, &cluster_body));
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

fn open(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

#[test]
fn additions_surface_in_disk_order_with_default_id_materialised() {
    // First BlockMore omits BlockAddID → §5.1.3.5.2.3 default 1
    // (codec-defined); second carries an explicit BlockAddID 4.
    let bg = block_group(
        &block(0, &[0x10, 0x11]),
        &[
            block_more(None, Some(&[0xA0, 0xA1, 0xA2])),
            block_more(Some(4), Some(&[0xB0])),
        ],
    );
    let mut dmx = open(build_segment(Some(4), &[bg]));
    assert!(
        dmx.block_additions().is_empty(),
        "no packet returned yet → empty surface"
    );
    let p = dmx.next_packet().expect("packet");
    assert_eq!(p.data, vec![0x10, 0x11]);
    let adds = dmx.block_additions();
    assert_eq!(adds.len(), 2);
    assert_eq!(
        adds[0].block_add_id(),
        1,
        "omitted BlockAddID defaults to 1"
    );
    assert!(adds[0].is_codec_defined());
    assert_eq!(adds[0].data(), &[0xA0, 0xA1, 0xA2]);
    assert_eq!(adds[1].block_add_id(), 4);
    assert!(!adds[1].is_codec_defined());
    assert_eq!(adds[1].data(), &[0xB0]);
}

#[test]
fn simple_block_and_plain_block_group_surface_empty_and_do_not_leak() {
    // Packet 1 (BlockGroup with additions) → non-empty; packet 2
    // (SimpleBlock — can never carry the element) → empty again;
    // packet 3 (BlockGroup *without* a BlockAdditions child) → empty.
    let bg_with = block_group(&block(0, &[0x10]), &[block_more(Some(2), Some(&[0xCC]))]);
    let sb = simple_block(5, &[0x20]);
    let bg_without = block_group(&block(10, &[0x30]), &[]);
    let mut dmx = open(build_segment(Some(2), &[bg_with, sb, bg_without]));

    dmx.next_packet().expect("packet 1");
    assert_eq!(dmx.block_additions().len(), 1);

    let p2 = dmx.next_packet().expect("packet 2");
    assert_eq!(p2.data, vec![0x20]);
    assert!(
        dmx.block_additions().is_empty(),
        "SimpleBlock packet must clear the previous Block's additions"
    );

    let p3 = dmx.next_packet().expect("packet 3");
    assert_eq!(p3.data, vec![0x30]);
    assert!(dmx.block_additions().is_empty());
}

#[test]
fn malformed_block_more_shapes_are_dropped() {
    // Four BlockMore children: (a) valid id 2; (b) missing the mandatory
    // BlockAdditional → dropped; (c) BlockAddID 0 (range "not 0") →
    // dropped; (d) duplicate id 2 → dropped, first occurrence kept.
    let bg = block_group(
        &block(0, &[0x10]),
        &[
            block_more(Some(2), Some(&[0x01])),
            block_more(Some(3), None),
            block_more(Some(0), Some(&[0x02])),
            block_more(Some(2), Some(&[0x03])),
        ],
    );
    let mut dmx = open(build_segment(Some(3), &[bg]));
    dmx.next_packet().expect("packet");
    let adds = dmx.block_additions();
    assert_eq!(adds.len(), 1, "only the first valid BlockMore survives");
    assert_eq!(adds[0].block_add_id(), 2);
    assert_eq!(
        adds[0].data(),
        &[0x01],
        "duplicate-id BlockMore must not replace the first occurrence"
    );
}

#[test]
fn laced_frames_share_the_blocks_additions() {
    // A Xiph-laced Block de-laces into two packets; the BlockAdditions
    // master attaches to the Block as a whole, so both report it.
    let bg = block_group(
        &block_xiph_two_frames(0, &[0x51, 0x52], &[0x61]),
        &[block_more(Some(1), Some(&[0xEE, 0xEF]))],
    );
    let mut dmx = open(build_segment(Some(1), &[bg]));

    let p1 = dmx.next_packet().expect("frame 1");
    assert_eq!(p1.data, vec![0x51, 0x52]);
    assert_eq!(dmx.block_additions().len(), 1);
    assert_eq!(dmx.block_additions()[0].data(), &[0xEE, 0xEF]);

    let p2 = dmx.next_packet().expect("frame 2");
    assert_eq!(p2.data, vec![0x61]);
    assert_eq!(
        dmx.block_additions().len(),
        1,
        "second de-laced frame shares the same Block's additions"
    );
    assert_eq!(dmx.block_additions()[0].block_add_id(), 1);
}

#[test]
fn max_block_addition_id_surfaces_with_default_materialised() {
    // Explicit element → its value; absent element → spec default 0
    // (§5.1.4.1.16 "there is no BlockAdditions for this track");
    // out-of-range stream index → None.
    let explicit = open(build_segment(Some(4), &[simple_block(0, &[0x20])]));
    assert_eq!(explicit.max_block_addition_id(0), Some(4));
    assert_eq!(explicit.max_block_addition_id(1), None);

    let absent = open(build_segment(None, &[simple_block(0, &[0x20])]));
    assert_eq!(absent.max_block_addition_id(0), Some(0));
}
