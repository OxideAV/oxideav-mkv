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
