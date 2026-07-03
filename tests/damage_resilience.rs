//! Damage-resilient demuxing — `demux::open_resilient` /
//! `demux::open_resilient_typed`.
//!
//! RFC 9559 §26 leaves error handling to the Reader ("Matroska Readers
//! decide how to handle the errors whether or not they are recoverable in
//! their code"), and §5.1.3.2 explicitly anticipates resynchronising the
//! offset on damaged streams at the Cluster level. These tests pin our
//! Reader's recovery decisions:
//!
//! * a clean file demuxes identically through the strict and resilient
//!   paths, with zero [`DamageEvent`]s;
//! * a corrupt element between Clusters resynchronises on the next
//!   Cluster, losing only the damaged bytes;
//! * a corrupt `SimpleBlock` size inside a Cluster loses that Cluster's
//!   tail but recovers every later Cluster;
//! * a truncated file yields the packet prefix that physically fits, then
//!   a clean `Error::Eof` — including a known-size Segment whose declared
//!   size now runs past the input end;
//! * a damaged Top-Level master (`Tags`) fails the strict open but is
//!   skipped by the resilient open, keeping tracks and packets intact;
//! * damage with no later recovery point drops the tail and ends the
//!   stream cleanly.

use std::io::Cursor;

use oxideav_core::{Demuxer, Error, ReadSeek};
use oxideav_mkv::demux::{DamageKind, MkvDemuxer};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

// ---------------------------------------------------------------------
// Minimal EBML builders (same shape as tests/cluster_records.rs).
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

/// Element header declaring `forged_size`, followed by `body` bytes that
/// do NOT match it — the corruption shape for oversize-size damage.
fn elem_forged_size(id: u32, forged_size: u64, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(forged_size, 0));
    out.extend_from_slice(body);
    out
}

fn simple_block(track: u8, tc_offset: i16, keyframe: bool, payload: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(if keyframe { 0x80 } else { 0x00 });
    body.extend_from_slice(&[payload; 8]);
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

fn info_master() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 10_000.0));
    elem_master(ids::INFO, &body)
}

fn tracks_master() -> Vec<u8> {
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
    elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &tb))
}

/// One bounded Cluster: `Timestamp` + one keyframe `SimpleBlock` whose
/// payload is 8 copies of `payload`.
fn cluster(tc: u64, payload: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_uint(ids::TIMECODE, tc));
    body.extend_from_slice(&simple_block(1, 0, true, payload));
    elem_master(ids::CLUSTER, &body)
}

/// EBML header + known-size Segment around `seg_body`.
fn file_with_segment_body(seg_body: &[u8]) -> Vec<u8> {
    let mut out = ebml_header();
    out.extend_from_slice(&elem_master(ids::SEGMENT, seg_body));
    out
}

/// The canonical 3-cluster test file: Info, Tracks, then Clusters at
/// t = 0 / 1000 / 2000 ms with payload bytes 0x11 / 0x22 / 0x33.
fn three_cluster_file() -> Vec<u8> {
    let mut seg = Vec::new();
    seg.extend_from_slice(&info_master());
    seg.extend_from_slice(&tracks_master());
    seg.extend_from_slice(&cluster(0, 0x11));
    seg.extend_from_slice(&cluster(1000, 0x22));
    seg.extend_from_slice(&cluster(2000, 0x33));
    file_with_segment_body(&seg)
}

/// Byte offset of the `n`-th (0-based) Cluster ID in `bytes`. Scans past
/// the EBML header so a coincidental match inside it can't confuse the
/// tests (the hand-built files carry no SeekHead / Cues whose payloads
/// could embed the pattern).
fn nth_cluster_offset(bytes: &[u8], n: usize) -> usize {
    let needle = write_element_id(ids::CLUSTER);
    bytes
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == needle.as_slice())
        .map(|(i, _)| i)
        .nth(n)
        .expect("cluster present")
}

fn open_strict(bytes: Vec<u8>) -> oxideav_core::Result<MkvDemuxer> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver)
}

fn open_resilient(bytes: Vec<u8>) -> oxideav_core::Result<MkvDemuxer> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_resilient_typed(rs, &oxideav_core::NullCodecResolver)
}

/// Drain every packet, returning `(pts, first_payload_byte)` pairs.
/// Panics on any error other than the clean `Error::Eof`.
fn drain(dmx: &mut MkvDemuxer) -> Vec<(i64, u8)> {
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => out.push((p.pts.unwrap_or(-1), p.data.first().copied().unwrap_or(0))),
            Err(Error::Eof) => return out,
            Err(e) => panic!("unexpected demux error: {e}"),
        }
    }
}

// =====================================================================
// 1. Clean file — resilient path is a byte-for-byte no-op.
// =====================================================================

#[test]
fn resilient_open_matches_strict_on_clean_file() {
    let bytes = three_cluster_file();
    let mut strict = open_strict(bytes.clone()).expect("strict open");
    let mut resilient = open_resilient(bytes).expect("resilient open");
    assert!(!strict.is_resilient());
    assert!(resilient.is_resilient());
    let a = drain(&mut strict);
    let b = drain(&mut resilient);
    assert_eq!(a, b, "clean file must demux identically on both paths");
    assert_eq!(a, vec![(0, 0x11), (1000, 0x22), (2000, 0x33)]);
    assert!(
        resilient.damage_events().is_empty(),
        "a clean file must record no damage events"
    );
}

// =====================================================================
// 2. Corrupt bytes where a Cluster header should be — resync on the
//    next Cluster.
// =====================================================================

#[test]
fn resync_recovers_clusters_after_corrupt_cluster_header() {
    let mut bytes = three_cluster_file();
    // Zero out the 2nd Cluster's 4-byte ID: `read_vint` rejects a leading
    // 0x00 byte, so the walk errors exactly at the damage.
    let c1 = nth_cluster_offset(&bytes, 1);
    for b in &mut bytes[c1..c1 + 4] {
        *b = 0x00;
    }

    // Strict path: packets stop at the damage.
    let mut strict = open_strict(bytes.clone()).expect("open");
    let mut got = Vec::new();
    while let Ok(p) = strict.next_packet() {
        got.push(p.pts.unwrap_or(-1));
    }
    assert_eq!(got, vec![0], "strict path must stop at the damage");

    // Resilient path: cluster 3 comes back.
    let mut dmx = open_resilient(bytes).expect("open");
    let packets = drain(&mut dmx);
    assert_eq!(
        packets,
        vec![(0, 0x11), (2000, 0x33)],
        "resync must recover the cluster after the damaged one"
    );
    let events = dmx.damage_events();
    assert_eq!(events.len(), 1, "one resync event");
    assert_eq!(events[0].kind(), DamageKind::ClusterStream);
    assert!(events[0].resumed_at().is_some());
    assert!(events[0].bytes_skipped() > 0);
}

// =====================================================================
// 3. Corrupt SimpleBlock size inside a Cluster — the Cluster's tail is
//    lost, later Clusters recover.
// =====================================================================

#[test]
fn resync_recovers_after_forged_block_size_mid_cluster() {
    // Cluster 2 carries a SimpleBlock whose declared size (1 MiB) overruns
    // everything — reading it must fail, then resync lands on Cluster 3.
    let mut c2_body = Vec::new();
    c2_body.extend_from_slice(&elem_uint(ids::TIMECODE, 1000));
    c2_body.extend_from_slice(&elem_forged_size(ids::SIMPLE_BLOCK, 1 << 20, &[0xAA; 16]));
    let mut seg = Vec::new();
    seg.extend_from_slice(&info_master());
    seg.extend_from_slice(&tracks_master());
    seg.extend_from_slice(&cluster(0, 0x11));
    seg.extend_from_slice(&elem_master(ids::CLUSTER, &c2_body));
    seg.extend_from_slice(&cluster(2000, 0x33));
    let bytes = file_with_segment_body(&seg);

    let mut dmx = open_resilient(bytes).expect("open");
    let packets = drain(&mut dmx);
    assert_eq!(
        packets,
        vec![(0, 0x11), (2000, 0x33)],
        "the damaged cluster's block is lost; the next cluster recovers"
    );
    assert_eq!(dmx.damage_events().len(), 1);
    assert_eq!(dmx.damage_events()[0].kind(), DamageKind::ClusterStream);
}

// =====================================================================
// 4. Truncation — every cut point yields a packet prefix + clean Eof,
//    never a panic.
// =====================================================================

#[test]
fn every_truncation_point_yields_packet_prefix_without_panic() {
    let bytes = three_cluster_file();
    let mut full = open_resilient(bytes.clone()).expect("open full");
    let all_packets = drain(&mut full);
    assert_eq!(all_packets.len(), 3);

    for cut in 0..bytes.len() {
        let truncated = bytes[..cut].to_vec();
        match open_resilient(truncated) {
            Err(_) => {} // structurally unusable (header / Tracks cut) — fine
            Ok(mut dmx) => {
                let mut got = Vec::new();
                loop {
                    match dmx.next_packet() {
                        Ok(p) => {
                            got.push((p.pts.unwrap_or(-1), p.data.first().copied().unwrap_or(0)))
                        }
                        Err(Error::Eof) => break,
                        Err(e) => panic!("cut {cut}: non-Eof error {e}"),
                    }
                }
                assert!(
                    got.len() <= all_packets.len() && got == all_packets[..got.len()],
                    "cut {cut}: packets must be a prefix of the full stream (got {got:?})"
                );
            }
        }
    }
}

// =====================================================================
// 5. Known-size Segment truncated on disk — the declared size is clamped
//    and recorded as SegmentTruncated.
// =====================================================================

#[test]
fn known_size_segment_past_eof_is_clamped_with_event() {
    let bytes = three_cluster_file();
    // Chop the last cluster's final 5 bytes without touching the Segment's
    // declared size.
    let cut = bytes.len() - 5;
    let truncated = bytes[..cut].to_vec();

    let mut dmx = open_resilient(truncated).expect("resilient open of truncated file");
    assert!(
        dmx.damage_events()
            .iter()
            .any(|e| e.kind() == DamageKind::SegmentTruncated && e.bytes_skipped() == 5),
        "SegmentTruncated event with the missing-byte count: {:?}",
        dmx.damage_events()
    );
    let packets = drain(&mut dmx);
    // The final SimpleBlock no longer fits — the first two clusters do.
    assert_eq!(packets, vec![(0, 0x11), (1000, 0x22)]);
}

// =====================================================================
// 6. Damaged Top-Level master — strict open fails, resilient open skips
//    the master and keeps demuxing.
// =====================================================================

#[test]
fn resilient_open_skips_damaged_tags_master() {
    // A Tags master whose inner TagString declares a 1 GiB size.
    let mut tag_body = Vec::new();
    tag_body.extend_from_slice(&elem_master(ids::TARGETS, &[]));
    let mut st = Vec::new();
    st.extend_from_slice(&elem_str(ids::TAG_NAME, "TITLE"));
    st.extend_from_slice(&elem_forged_size(ids::TAG_STRING, 1 << 30, &[0x41; 4]));
    tag_body.extend_from_slice(&elem_master(ids::SIMPLE_TAG, &st));
    let damaged_tags = elem_master(ids::TAGS, &elem_master(ids::TAG, &tag_body));

    let mut seg = Vec::new();
    seg.extend_from_slice(&info_master());
    seg.extend_from_slice(&tracks_master());
    seg.extend_from_slice(&damaged_tags);
    seg.extend_from_slice(&cluster(0, 0x11));
    let bytes = file_with_segment_body(&seg);

    assert!(
        open_strict(bytes.clone()).is_err(),
        "strict open must reject the forged TagString size"
    );

    let mut dmx = open_resilient(bytes).expect("resilient open");
    assert_eq!(dmx.streams().len(), 1, "tracks survive the damaged Tags");
    assert!(
        dmx.damage_events()
            .iter()
            .any(|e| e.kind() == DamageKind::DamagedMaster(ids::TAGS)),
        "DamagedMaster(Tags) event: {:?}",
        dmx.damage_events()
    );
    let packets = drain(&mut dmx);
    assert_eq!(packets, vec![(0, 0x11)]);
}

// =====================================================================
// 7. Damage with no recovery point — tail dropped, clean Eof.
// =====================================================================

#[test]
fn unrecoverable_tail_ends_stream_cleanly() {
    let mut seg = Vec::new();
    seg.extend_from_slice(&info_master());
    seg.extend_from_slice(&tracks_master());
    seg.extend_from_slice(&cluster(0, 0x11));
    // 64 bytes of 0x00 garbage with no later element to re-anchor on.
    seg.extend_from_slice(&[0x00; 64]);
    let bytes = file_with_segment_body(&seg);

    let mut dmx = open_resilient(bytes).expect("open");
    let packets = drain(&mut dmx);
    assert_eq!(packets, vec![(0, 0x11)]);
    let events = dmx.damage_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind(), DamageKind::UnrecoverableTail);
    assert_eq!(events[0].resumed_at(), None);
    assert_eq!(events[0].bytes_skipped(), 64);
}
