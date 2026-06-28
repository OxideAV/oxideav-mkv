//! Per-Cluster typed records — `Position` (RFC 9559 §5.1.3.2) +
//! `PrevSize` (RFC 9559 §5.1.3.3).
//!
//! Both children are optional `uinteger` elements sitting directly on
//! the `Cluster` master. We hand-build a minimal MKV with two Clusters
//! that carry every combination of present / absent fields and assert
//! [`MkvDemuxer::cluster_records`] surfaces them faithfully.

use std::io::Cursor;

use oxideav_core::{Demuxer, Error, ReadSeek};
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
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(ids::SIMPLE_BLOCK));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(&body);
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

fn tracks_body() -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, 0xA1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio = Vec::new();
    audio.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    tb.extend_from_slice(&elem_master(ids::AUDIO, &audio));
    elem_master(ids::TRACK_ENTRY, &tb)
}

/// Build a Cluster body with `Timestamp` first, then optional
/// `Position` / `PrevSize`, then one SimpleBlock.
fn cluster_body_with(
    tc: u64,
    position: Option<u64>,
    prev_size: Option<u64>,
    block_payload: u8,
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_uint(ids::TIMECODE, tc));
    if let Some(p) = position {
        body.extend_from_slice(&elem_uint(ids::POSITION, p));
    }
    if let Some(p) = prev_size {
        body.extend_from_slice(&elem_uint(ids::PREV_SIZE, p));
    }
    body.extend_from_slice(&simple_block(1, 0, true, block_payload));
    body
}

fn build_segment(clusters: &[Vec<u8>]) -> Vec<u8> {
    let info = elem_master(ids::INFO, &info_body());
    let tracks = elem_master(ids::TRACKS, &tracks_body());

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    for c in clusters {
        seg_body.extend_from_slice(&elem_master(ids::CLUSTER, c));
    }
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
fn cluster_records_empty_before_any_walk() {
    // The demuxer opens at the first Cluster but doesn't read into it
    // until `next_packet` is called — so the typed-record slice starts
    // empty.
    let bytes = build_segment(&[cluster_body_with(0, Some(0), None, 0xAA)]);
    let dmx = open(bytes);
    assert!(dmx.cluster_records().is_empty());
}

#[test]
fn cluster_records_capture_position_and_prev_size() {
    // Two Clusters: the first carries `Position` only, the second
    // carries both `Position` and `PrevSize`.
    let bytes = build_segment(&[
        cluster_body_with(0, Some(100), None, 0xAA),
        cluster_body_with(10, Some(1234), Some(42), 0xBB),
    ]);
    let mut dmx = open(bytes);

    // Drain — Eof returns once both clusters are consumed.
    let mut payloads = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => payloads.push(p.data.clone()),
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected: {e:?}"),
        }
    }
    assert_eq!(payloads.len(), 2);

    let recs = dmx.cluster_records();
    assert_eq!(recs.len(), 2, "expected one record per Cluster");

    assert_eq!(recs[0].position, Some(100));
    assert_eq!(recs[0].prev_size, None);

    assert_eq!(recs[1].position, Some(1234));
    assert_eq!(recs[1].prev_size, Some(42));

    // Body offsets are strictly increasing — first Cluster precedes the
    // second on disk.
    assert!(
        recs[0].body_offset < recs[1].body_offset,
        "expected first Cluster's body_offset to precede the second's: {recs:?}"
    );
}

#[test]
fn cluster_records_for_cluster_without_children_left_none() {
    // A Cluster with no `Position` / `PrevSize` children at all — both
    // typed fields stay `None` per the optional-element semantics.
    let bytes = build_segment(&[cluster_body_with(0, None, None, 0xCC)]);
    let mut dmx = open(bytes);
    let _ = dmx.next_packet();
    let _ = dmx.next_packet();

    let recs = dmx.cluster_records();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].position, None);
    assert_eq!(recs[0].prev_size, None);
}

#[test]
fn cluster_records_live_stream_zero_position() {
    // RFC 9559 §5.1.3.2: in a live stream `Position` is conventionally
    // `0`. The demuxer surfaces `Some(0)`, distinct from `None`
    // ("element was absent on disk").
    let bytes = build_segment(&[cluster_body_with(0, Some(0), None, 0xAA)]);
    let mut dmx = open(bytes);
    let _ = dmx.next_packet();
    let _ = dmx.next_packet();

    let recs = dmx.cluster_records();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].position, Some(0));
}

#[test]
fn cluster_records_body_offset_matches_actual_on_disk_position() {
    // The recorded `body_offset` is the absolute file offset of the
    // byte right after the Cluster's id+size header. Verify it lines up
    // with the spec definition: subtracting `segment_data_start` and the
    // Cluster header length yields `Position` (when the writer is
    // truthful).
    //
    // We build the file ourselves so we know the exact offsets.
    let cluster_a_body = cluster_body_with(0, None, None, 0xAA);
    let cluster_b_body = cluster_body_with(10, None, None, 0xBB);
    let cluster_a = elem_master(ids::CLUSTER, &cluster_a_body);
    let cluster_b = elem_master(ids::CLUSTER, &cluster_b_body);

    let info = elem_master(ids::INFO, &info_body());
    let tracks = elem_master(ids::TRACKS, &tracks_body());
    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    let off_a_segment_relative = seg_body.len() as u64;
    seg_body.extend_from_slice(&cluster_a);
    let off_b_segment_relative = seg_body.len() as u64;
    seg_body.extend_from_slice(&cluster_b);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut bytes = ebml_header();
    bytes.extend_from_slice(&segment);

    let mut dmx = open(bytes);
    // Drain all packets so both Clusters get walked.
    loop {
        match dmx.next_packet() {
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected: {e:?}"),
        }
    }

    let recs = dmx.cluster_records();
    assert_eq!(recs.len(), 2);

    // The Cluster element header is `id (1 byte for 0x1F43B675 → 4
    // bytes) + size VINT`. We can recover that by reading the actual
    // Cluster header bytes from `cluster_a` — header_len =
    // cluster_a.len() - cluster_a_body.len().
    let header_len_a = (cluster_a.len() - cluster_a_body.len()) as u64;
    let header_len_b = (cluster_b.len() - cluster_b_body.len()) as u64;

    // body_offset is absolute in the file → re-base to Segment-data-
    // relative by subtracting the Segment header + EBML header length.
    // We know the Segment body starts at `header.len() + segment_header_len`.
    let ebml_header_len = ebml_header().len() as u64;
    let segment_header_len = (segment.len() - seg_body.len()) as u64;
    let segment_data_start = ebml_header_len + segment_header_len;

    // The expected Segment-Position of Cluster A is the offset of the
    // Cluster id within Segment data — i.e. `off_a_segment_relative`.
    let expected_pos_a = off_a_segment_relative;
    let actual_pos_a = recs[0].body_offset - segment_data_start - header_len_a;
    assert_eq!(
        actual_pos_a, expected_pos_a,
        "Cluster A body_offset → Position derivation mismatch"
    );

    let expected_pos_b = off_b_segment_relative;
    let actual_pos_b = recs[1].body_offset - segment_data_start - header_len_b;
    assert_eq!(
        actual_pos_b, expected_pos_b,
        "Cluster B body_offset → Position derivation mismatch"
    );
}

#[test]
fn cluster_records_dedup_across_repeat_walks() {
    // A Cluster opened twice (e.g. through a `next_packet` walk that
    // reaches EOF and then a redundant `next_packet`) registers the
    // record once — body_offset is the dedup key.
    let bytes = build_segment(&[cluster_body_with(0, Some(7), Some(8), 0xAA)]);
    let mut dmx = open(bytes);

    // Drain fully.
    loop {
        match dmx.next_packet() {
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected: {e:?}"),
        }
    }
    // A second drain after EOF — no new clusters arrive.
    loop {
        match dmx.next_packet() {
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected: {e:?}"),
        }
    }

    let recs = dmx.cluster_records();
    assert_eq!(recs.len(), 1, "expected dedup across re-walks: {recs:?}");
    assert_eq!(recs[0].position, Some(7));
    assert_eq!(recs[0].prev_size, Some(8));
}

/// Raw `EncryptedBlock` (RFC 9559 Appendix A.15, id `0xAF`) element with an
/// arbitrary opaque body — the container treats the body as fully Transformed
/// (encrypted/signed) and never inspects it.
fn encrypted_block(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(ids::ENCRYPTED_BLOCK));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

#[test]
fn cluster_records_capture_encrypted_block_payloads() {
    // A Cluster carrying a normal SimpleBlock plus two EncryptedBlocks. The
    // SimpleBlock still yields a packet; the EncryptedBlock bodies surface
    // verbatim on the record in on-disk order.
    let mut cluster = Vec::new();
    cluster.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    cluster.extend_from_slice(&encrypted_block(b"\x81\x00\x00\x80enc-one"));
    cluster.extend_from_slice(&encrypted_block(b"\x81\x00\x10\x80enc-two"));
    let bytes = build_segment(&[cluster]);

    let mut dmx = open(bytes);
    let mut payloads = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => payloads.push(p.data.clone()),
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected: {e:?}"),
        }
    }
    // Only the SimpleBlock produces a packet — EncryptedBlock can't be
    // decoded into one (its track header is inside the Transformed region).
    assert_eq!(payloads.len(), 1, "EncryptedBlock must not become a packet");

    let recs = dmx.cluster_records();
    assert_eq!(recs.len(), 1);
    assert_eq!(
        recs[0].encrypted_blocks,
        vec![
            b"\x81\x00\x00\x80enc-one".to_vec(),
            b"\x81\x00\x10\x80enc-two".to_vec(),
        ],
        "both EncryptedBlock bodies must surface verbatim in on-disk order"
    );
}

#[test]
fn cluster_records_no_encrypted_blocks_when_absent() {
    // A normal Cluster carries no EncryptedBlock — the list stays empty.
    let bytes = build_segment(&[cluster_body_with(0, None, None, 0xCC)]);
    let mut dmx = open(bytes);
    let _ = dmx.next_packet();
    let _ = dmx.next_packet();
    let recs = dmx.cluster_records();
    assert_eq!(recs.len(), 1);
    assert!(recs[0].encrypted_blocks.is_empty());
}
