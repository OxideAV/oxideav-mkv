//! End-to-end CRC-32 validation on Top-Level master elements.
//!
//! Matroska files SHOULD carry a `CRC-32` element (RFC 8794 §11.3.1, RFC
//! 9559 §6.2) as the first child of each Top-Level master. These tests
//! hand-build a minimal MKV that puts a `CRC-32` child on `Info` and
//! `Tracks`, then assert the demuxer recomputes it and reports a match —
//! and that a deliberately corrupted body is reported as a mismatch
//! while still demuxing.

use std::io::Cursor;

use oxideav_core::ReadSeek;
use oxideav_mkv::ebml::{crc32_ieee, write_element_id, write_vint};
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

/// Build a `CRC-32` element (id 0xBF, 4 bytes, little-endian) over `data`.
fn crc32_elem(data: &[u8]) -> Vec<u8> {
    let crc = crc32_ieee(data);
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(ids::CRC32));
    out.extend_from_slice(&write_vint(4, 0));
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Wrap `body` in a master element whose first child is a CRC-32 element
/// computed over `body` (matching the RFC's "all element data except the
/// CRC-32 element itself" rule). If `corrupt`, flip a value bit of the CRC
/// so it no longer validates.
fn elem_master_crc(id: u32, body: &[u8], corrupt: bool) -> Vec<u8> {
    let mut crc = crc32_elem(body);
    if corrupt {
        // Flip the lowest bit of the stored CRC value (last byte).
        let last = crc.len() - 1;
        crc[last] ^= 0x01;
    }
    let mut full = Vec::new();
    full.extend_from_slice(&crc);
    full.extend_from_slice(body);
    elem_master(id, &full)
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

fn cluster() -> Vec<u8> {
    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    elem_master(ids::CLUSTER, &cluster_body)
}

/// Build an MKV where Info and Tracks each carry a leading CRC-32 child.
/// `corrupt_tracks` flips the Tracks CRC so it no longer validates.
fn build_mkv_with_crc(corrupt_tracks: bool) -> Vec<u8> {
    let info = elem_master_crc(ids::INFO, &info_body(), false);
    let tracks = elem_master_crc(ids::TRACKS, &tracks_body(), corrupt_tracks);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&cluster());
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

/// An MKV with NO CRC-32 children at all (the spec permits omission).
fn build_mkv_without_crc() -> Vec<u8> {
    let info = elem_master(ids::INFO, &info_body());
    let tracks = elem_master(ids::TRACKS, &tracks_body());

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&cluster());
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

#[test]
fn crc_status_reports_valid_for_intact_elements() {
    let bytes = build_mkv_with_crc(false);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let statuses = dmx.crc_status();
    assert_eq!(statuses.len(), 2, "expected CRC status for Info and Tracks");
    // Both elements present and both valid.
    let info = statuses
        .iter()
        .find(|s| s.element_id == ids::INFO)
        .expect("Info CRC status");
    let tracks = statuses
        .iter()
        .find(|s| s.element_id == ids::TRACKS)
        .expect("Tracks CRC status");
    assert!(info.is_valid(), "Info CRC should validate: {info:?}");
    assert!(tracks.is_valid(), "Tracks CRC should validate: {tracks:?}");
    assert_eq!(info.stored, info.computed);
    assert_eq!(tracks.stored, tracks.computed);
}

#[test]
fn crc_status_reports_mismatch_for_corrupted_element() {
    let bytes = build_mkv_with_crc(true);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver)
        .expect("demux open should still succeed despite bad CRC");

    let statuses = dmx.crc_status();
    let info = statuses
        .iter()
        .find(|s| s.element_id == ids::INFO)
        .expect("Info CRC status");
    let tracks = statuses
        .iter()
        .find(|s| s.element_id == ids::TRACKS)
        .expect("Tracks CRC status");
    // Info untouched → valid; Tracks CRC was flipped → invalid.
    assert!(info.is_valid(), "Info CRC should still validate");
    assert!(
        !tracks.is_valid(),
        "Tracks CRC should NOT validate after corruption: {tracks:?}"
    );
    assert_ne!(tracks.stored, tracks.computed);
}

#[test]
fn corrupted_crc_does_not_block_demuxing() {
    use oxideav_core::Demuxer;
    let bytes = build_mkv_with_crc(true);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    // The single SimpleBlock should still come out — a bad CRC is
    // informational only (RFC 8794 §12: a reader MAY ignore data; we
    // surface the status and keep going).
    let pkt = dmx.next_packet().expect("first packet");
    assert_eq!(pkt.data, vec![0xAA]);
}

#[test]
fn no_crc_children_yields_empty_status() {
    let bytes = build_mkv_without_crc();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    assert!(
        dmx.crc_status().is_empty(),
        "no CRC-32 children → no statuses, got {:?}",
        dmx.crc_status()
    );
}

// --- Per-Cluster CRC-32 validation ---------------------------------------
//
// RFC 9559 §6.2 + RFC 8794 §11.3.1: every Top-Level master element of a
// Matroska Segment, *including each Cluster*, SHOULD carry a `CRC-32`
// element as its first child. The demuxer validates Top-Level masters at
// open time and Clusters lazily as it walks them on `next_packet` /
// `seek_to`. The element id on a Cluster status is `ids::CLUSTER`.

/// Build a Cluster with a leading CRC-32 child over the cluster body.
/// `corrupt` flips a bit in the stored CRC so the validator reports a
/// mismatch.
fn cluster_with_crc(corrupt: bool, payload: u8) -> Vec<u8> {
    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&simple_block(1, 0, true, payload));
    elem_master_crc(ids::CLUSTER, &cluster_body, corrupt)
}

fn build_mkv_with_cluster_crc(corrupt_cluster: bool) -> Vec<u8> {
    let info = elem_master(ids::INFO, &info_body());
    let tracks = elem_master(ids::TRACKS, &tracks_body());

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&cluster_with_crc(corrupt_cluster, 0xAA));
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

#[test]
fn cluster_crc_status_reports_valid_after_next_packet() {
    use oxideav_core::Demuxer;
    let bytes = build_mkv_with_cluster_crc(false);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    // Open does NOT walk Clusters, so the Cluster status only lands once
    // the demuxer is asked for a packet.
    let cluster_statuses_pre: Vec<_> = dmx
        .crc_status()
        .iter()
        .filter(|s| s.element_id == ids::CLUSTER)
        .copied()
        .collect();
    assert!(
        cluster_statuses_pre.is_empty(),
        "Cluster CRC should not be validated before next_packet, got {cluster_statuses_pre:?}"
    );

    let pkt = dmx.next_packet().expect("first packet");
    assert_eq!(pkt.data, vec![0xAA]);

    let cluster_statuses: Vec<_> = dmx
        .crc_status()
        .iter()
        .filter(|s| s.element_id == ids::CLUSTER)
        .copied()
        .collect();
    assert_eq!(
        cluster_statuses.len(),
        1,
        "expected one Cluster CRC status, got {cluster_statuses:?}"
    );
    let c = cluster_statuses[0];
    assert!(c.is_valid(), "Cluster CRC should validate: {c:?}");
    assert_eq!(c.stored, c.computed);
}

#[test]
fn cluster_crc_status_reports_mismatch_for_corrupted_cluster() {
    use oxideav_core::Demuxer;
    let bytes = build_mkv_with_cluster_crc(true);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    // The packet still comes out (informational only per RFC 8794 §12).
    let pkt = dmx.next_packet().expect("first packet");
    assert_eq!(pkt.data, vec![0xAA]);

    let cluster_statuses: Vec<_> = dmx
        .crc_status()
        .iter()
        .filter(|s| s.element_id == ids::CLUSTER)
        .copied()
        .collect();
    assert_eq!(cluster_statuses.len(), 1, "expected one Cluster CRC status");
    let c = cluster_statuses[0];
    assert!(
        !c.is_valid(),
        "Cluster CRC should NOT validate after corruption: {c:?}"
    );
    assert_ne!(c.stored, c.computed);
}

#[test]
fn cluster_without_crc_yields_no_cluster_status() {
    use oxideav_core::Demuxer;
    // The original `cluster()` builder produces a Cluster with no CRC-32
    // child. Drain its packets and confirm no Cluster status materialises.
    let bytes = build_mkv_without_crc();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    let _ = dmx.next_packet().expect("first packet");
    // Drive past EoF so every Cluster is walked.
    while dmx.next_packet().is_ok() {}
    let cluster_statuses: Vec<_> = dmx
        .crc_status()
        .iter()
        .filter(|s| s.element_id == ids::CLUSTER)
        .copied()
        .collect();
    assert!(
        cluster_statuses.is_empty(),
        "Cluster without CRC-32 child should produce no status, got {cluster_statuses:?}"
    );
}

/// Build an MKV with two Clusters: the first has a valid CRC-32, the
/// second has a corrupted CRC-32 with a different payload byte. Validates
/// segment-order recording and that mixing valid + invalid Clusters works.
fn build_mkv_two_clusters_mixed_crc() -> Vec<u8> {
    let info = elem_master(ids::INFO, &info_body());
    let tracks = elem_master(ids::TRACKS, &tracks_body());

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&cluster_with_crc(false, 0xAA));
    seg_body.extend_from_slice(&cluster_with_crc(true, 0xBB));
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

#[test]
fn cluster_crc_status_preserves_segment_order_across_multiple_clusters() {
    use oxideav_core::Demuxer;
    let bytes = build_mkv_two_clusters_mixed_crc();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    // Drain both clusters' single packets.
    let p1 = dmx.next_packet().expect("first packet");
    assert_eq!(p1.data, vec![0xAA]);
    let p2 = dmx.next_packet().expect("second packet");
    assert_eq!(p2.data, vec![0xBB]);

    let cluster_statuses: Vec<_> = dmx
        .crc_status()
        .iter()
        .filter(|s| s.element_id == ids::CLUSTER)
        .copied()
        .collect();
    assert_eq!(
        cluster_statuses.len(),
        2,
        "expected two Cluster CRC statuses, got {cluster_statuses:?}"
    );
    assert!(
        cluster_statuses[0].is_valid(),
        "first Cluster CRC should validate"
    );
    assert!(
        !cluster_statuses[1].is_valid(),
        "second Cluster CRC should NOT validate"
    );
}

/// Build a single-Cluster MKV with a Cues index that targets the only
/// Cluster, so the seek path re-opens it. The cluster CRC validation
/// dedup keyed on body_start must keep the recorded status count at 1.
fn build_mkv_one_cluster_with_cues_and_crc() -> Vec<u8> {
    let info = elem_master(ids::INFO, &info_body());
    let tracks = elem_master(ids::TRACKS, &tracks_body());

    // Build the Cluster first so we can compute its byte offset within the
    // Segment body (the Cues index needs CueClusterPosition relative to
    // the Segment payload start).
    let cluster = cluster_with_crc(false, 0xAA);

    // Compute the offset of the Cluster within the Segment body, which is
    // info.len() + tracks.len() + Cues.len() — but Cues is emitted before
    // the Cluster, so the order is info, tracks, Cues, Cluster, and the
    // Cluster sits at info.len() + tracks.len() + cues_len.
    //
    // Construct the cues body referencing a placeholder position, then
    // patch it. The unsigned-int encoder uses a minimal byte count, so to
    // keep the cues body width stable we explicitly pick a value width
    // that won't change when the real offset is written.

    // First pass with a 4-byte sentinel:
    let make_cues = |cluster_position: u64| -> Vec<u8> {
        // CueTrackPosition body.
        let mut cue_track_position = Vec::new();
        cue_track_position.extend_from_slice(&elem_uint(ids::CUE_TRACK, 1));
        // Force CueClusterPosition into a 4-byte slot so the cues body
        // length is identical between the placeholder and the patched
        // value. We do this by manually encoding a 4-byte big-endian uint
        // element.
        let mut ccp = Vec::new();
        ccp.extend_from_slice(&write_element_id(ids::CUE_CLUSTER_POSITION));
        ccp.extend_from_slice(&write_vint(4, 0));
        ccp.extend_from_slice(&(cluster_position as u32).to_be_bytes());
        cue_track_position.extend_from_slice(&ccp);

        // CuePoint body.
        let mut cue_point = Vec::new();
        cue_point.extend_from_slice(&elem_uint(ids::CUE_TIME, 0));
        cue_point.extend_from_slice(&elem_master(ids::CUE_TRACK_POSITIONS, &cue_track_position));
        elem_master(ids::CUES, &elem_master(ids::CUE_POINT, &cue_point))
    };

    let cues_placeholder = make_cues(0);
    let cluster_offset_in_segment: u64 =
        (info.len() + tracks.len() + cues_placeholder.len()) as u64;
    let cues = make_cues(cluster_offset_in_segment);
    assert_eq!(
        cues.len(),
        cues_placeholder.len(),
        "cues body length must be stable across the position patch"
    );

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&cues);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

#[test]
fn cluster_crc_status_not_duplicated_across_seek_and_next_packet() {
    use oxideav_core::Demuxer;
    let bytes = build_mkv_one_cluster_with_cues_and_crc();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    // First reach: forward `next_packet` walk validates the Cluster.
    let _ = dmx.next_packet().expect("first packet");
    let cluster_statuses_first: Vec<_> = dmx
        .crc_status()
        .iter()
        .filter(|s| s.element_id == ids::CLUSTER)
        .copied()
        .collect();
    assert_eq!(
        cluster_statuses_first.len(),
        1,
        "expected exactly one Cluster CRC status after first next_packet, got {cluster_statuses_first:?}"
    );
    assert!(cluster_statuses_first[0].is_valid());

    // Now seek back to the start of the same Cluster. The seek path
    // re-opens the Cluster header but the dedup must prevent a duplicate
    // status from landing.
    let _ = dmx.seek_to(0, 0).expect("seek to start of cluster");
    // Drain a packet so the demuxer actually re-walks the cluster body
    // (seek_to itself can short-circuit, but next_packet will not).
    let _ = dmx.next_packet().expect("packet after seek");

    let cluster_statuses_after: Vec<_> = dmx
        .crc_status()
        .iter()
        .filter(|s| s.element_id == ids::CLUSTER)
        .copied()
        .collect();
    assert_eq!(
        cluster_statuses_after.len(),
        1,
        "Cluster CRC status must not duplicate across seek + next_packet, got {cluster_statuses_after:?}"
    );
}
