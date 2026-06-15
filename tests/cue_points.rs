//! Typed `Cues > CuePoint` accessor — RFC 9559 §5.1.5.1 and its
//! sub-elements §5.1.5.1.1..§5.1.5.1.2.8 plus the reclaimed
//! Appendix A.37..A.39 `CueReference` children.
//!
//! The denormalised seek index ([`MkvDemuxer::seek_to`]) keeps only
//! (track, time, cluster_offset, relative_position); the typed
//! [`MkvDemuxer::cue_points`] accessor surfaces the full on-disk tree —
//! `CueDuration`, `CueBlockNumber`, `CueCodecState`, and the nested
//! `CueReference` rows — in document order. We hand-build minimal MKV
//! files exercising every sub-element, with `Cues` placed both before the
//! first Cluster and after the last, and assert the typed surface is
//! faithful.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_mkv::demux::{CuePoint, CueReference, CueTrackPositions};
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

fn cluster_body(tc: u64, payload: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_uint(ids::TIMECODE, tc));
    body.extend_from_slice(&simple_block(1, 0, true, payload));
    body
}

/// Build an MKV with the supplied Cues body placed BEFORE the first
/// Cluster.
fn build_cues_first(cues_body: &[u8], clusters: &[Vec<u8>]) -> Vec<u8> {
    let info = elem_master(ids::INFO, &info_body());
    let tracks = elem_master(ids::TRACKS, &tracks_body());
    let cues = elem_master(ids::CUES, cues_body);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&cues);
    for c in clusters {
        seg_body.extend_from_slice(&elem_master(ids::CLUSTER, c));
    }
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

/// Build an MKV with the supplied Cues body placed AFTER the last
/// Cluster (the common single-pass / index-at-end layout). This drives
/// the late best-effort `scan_cues_from` path.
fn build_cues_last(cues_body: &[u8], clusters: &[Vec<u8>]) -> Vec<u8> {
    let info = elem_master(ids::INFO, &info_body());
    let tracks = elem_master(ids::TRACKS, &tracks_body());
    let cues = elem_master(ids::CUES, cues_body);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    for c in clusters {
        seg_body.extend_from_slice(&elem_master(ids::CLUSTER, c));
    }
    seg_body.extend_from_slice(&cues);
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

/// A `CueTrackPositions` carrying CueTrack + CueClusterPosition only — the
/// spec-minimum mandatory pair.
fn ctp_minimal(track: u64, cluster_pos: u64) -> Vec<u8> {
    let mut tp = Vec::new();
    tp.extend_from_slice(&elem_uint(ids::CUE_TRACK, track));
    tp.extend_from_slice(&elem_uint(ids::CUE_CLUSTER_POSITION, cluster_pos));
    elem_master(ids::CUE_TRACK_POSITIONS, &tp)
}

/// A `CuePoint` carrying CueTime + the supplied CueTrackPositions bytes.
fn cue_point(time: u64, track_positions: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&elem_uint(ids::CUE_TIME, time));
    p.extend_from_slice(track_positions);
    elem_master(ids::CUE_POINT, &p)
}

#[test]
fn no_cues_element_empty_slice() {
    // A file with no Cues element at all surfaces an empty cue_points slice.
    let info = elem_master(ids::INFO, &info_body());
    let tracks = elem_master(ids::TRACKS, &tracks_body());
    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&elem_master(ids::CLUSTER, &cluster_body(0, 0x11)));
    let segment = elem_master(ids::SEGMENT, &seg_body);
    let mut bytes = ebml_header();
    bytes.extend_from_slice(&segment);

    let dmx = open(bytes);
    assert!(dmx.cue_points().is_empty());
}

#[test]
fn minimal_cue_point_mandatory_pair_only() {
    // A CuePoint with only CueTime + (CueTrack, CueClusterPosition) — the
    // spec-minimum — surfaces with every optional field at its absence /
    // default state.
    let mut cb = Vec::new();
    cb.extend_from_slice(&cue_point(0, &ctp_minimal(1, 1000)));
    let bytes = build_cues_first(&cb, &[cluster_body(0, 0x11)]);

    let dmx = open(bytes);
    let pts = dmx.cue_points();
    assert_eq!(pts.len(), 1);
    let p = &pts[0];
    assert_eq!(p.time, 0);
    assert_eq!(p.track_positions.len(), 1);
    let tp = &p.track_positions[0];
    assert_eq!(tp.track, 1);
    assert_eq!(tp.cluster_position, Some(1000));
    assert_eq!(tp.relative_position, None);
    assert_eq!(tp.duration, None);
    assert_eq!(tp.block_number, None);
    // CueCodecState default 0 materialised (§5.1.5.1.2.6).
    assert_eq!(tp.codec_state, 0);
    assert!(tp.references.is_empty());
}

#[test]
fn all_sub_elements_round_trip() {
    // A CueTrackPositions exercising every documented sub-element:
    // CueTrack, CueClusterPosition, CueRelativePosition, CueDuration,
    // CueBlockNumber, CueCodecState, and a CueReference.
    let mut tp = Vec::new();
    tp.extend_from_slice(&elem_uint(ids::CUE_TRACK, 1));
    tp.extend_from_slice(&elem_uint(ids::CUE_CLUSTER_POSITION, 0x2000));
    tp.extend_from_slice(&elem_uint(ids::CUE_RELATIVE_POSITION, 0x40));
    tp.extend_from_slice(&elem_uint(ids::CUE_DURATION, 480));
    tp.extend_from_slice(&elem_uint(ids::CUE_BLOCK_NUMBER, 3));
    tp.extend_from_slice(&elem_uint(ids::CUE_CODEC_STATE, 0x1234));
    let mut refbody = Vec::new();
    refbody.extend_from_slice(&elem_uint(ids::CUE_REF_TIME, 200));
    refbody.extend_from_slice(&elem_uint(ids::CUE_REF_CLUSTER, 0x1500));
    refbody.extend_from_slice(&elem_uint(ids::CUE_REF_NUMBER, 2));
    refbody.extend_from_slice(&elem_uint(ids::CUE_REF_CODEC_STATE, 0x99));
    tp.extend_from_slice(&elem_master(ids::CUE_REFERENCE, &refbody));
    let ctp = elem_master(ids::CUE_TRACK_POSITIONS, &tp);

    let cb = cue_point(500, &ctp);
    let bytes = build_cues_first(&cb, &[cluster_body(0, 0x11)]);

    let dmx = open(bytes);
    let pts = dmx.cue_points();
    assert_eq!(pts.len(), 1);
    assert_eq!(pts[0].time, 500);
    let p = &pts[0].track_positions[0];
    assert_eq!(p.track, 1);
    assert_eq!(p.cluster_position, Some(0x2000));
    assert_eq!(p.relative_position, Some(0x40));
    assert_eq!(p.duration, Some(480));
    assert_eq!(p.block_number, Some(3));
    assert_eq!(p.codec_state, 0x1234);
    assert_eq!(p.references.len(), 1);
    let r = &p.references[0];
    assert_eq!(
        r,
        &CueReference {
            ref_time: 200,
            ref_cluster: Some(0x1500),
            ref_number: Some(2),
            ref_codec_state: Some(0x99),
        }
    );
}

#[test]
fn cue_reference_minimal_only_ref_time() {
    // A CueReference carrying only the mandatory CueRefTime surfaces the
    // reclaimed-appendix children as None (they list no default).
    let mut tp = Vec::new();
    tp.extend_from_slice(&elem_uint(ids::CUE_TRACK, 1));
    tp.extend_from_slice(&elem_uint(ids::CUE_CLUSTER_POSITION, 0x10));
    let refbody = elem_uint(ids::CUE_REF_TIME, 42);
    tp.extend_from_slice(&elem_master(ids::CUE_REFERENCE, &refbody));
    let ctp = elem_master(ids::CUE_TRACK_POSITIONS, &tp);

    let dmx = open(build_cues_first(&cue_point(0, &ctp), &[cluster_body(0, 0)]));
    let r = &dmx.cue_points()[0].track_positions[0].references[0];
    assert_eq!(r.ref_time, 42);
    assert_eq!(r.ref_cluster, None);
    assert_eq!(r.ref_number, None);
    assert_eq!(r.ref_codec_state, None);
}

#[test]
fn multiple_references_preserved_in_order() {
    // CueReference has no maxOccurs — several references on one position
    // are preserved in document order.
    let mut tp = Vec::new();
    tp.extend_from_slice(&elem_uint(ids::CUE_TRACK, 1));
    tp.extend_from_slice(&elem_uint(ids::CUE_CLUSTER_POSITION, 0x10));
    for t in [100u64, 200, 300] {
        tp.extend_from_slice(&elem_master(
            ids::CUE_REFERENCE,
            &elem_uint(ids::CUE_REF_TIME, t),
        ));
    }
    let ctp = elem_master(ids::CUE_TRACK_POSITIONS, &tp);

    let dmx = open(build_cues_first(&cue_point(0, &ctp), &[cluster_body(0, 0)]));
    let refs = &dmx.cue_points()[0].track_positions[0].references;
    assert_eq!(refs.len(), 3);
    assert_eq!(refs[0].ref_time, 100);
    assert_eq!(refs[1].ref_time, 200);
    assert_eq!(refs[2].ref_time, 300);
}

#[test]
fn multiple_track_positions_per_cue_point() {
    // §5.1.5.1.2 has no maxOccurs — one CueTime can index several tracks.
    let mut cb = Vec::new();
    let mut pbody = Vec::new();
    pbody.extend_from_slice(&elem_uint(ids::CUE_TIME, 750));
    pbody.extend_from_slice(&ctp_minimal(1, 0x100));
    pbody.extend_from_slice(&ctp_minimal(2, 0x200));
    cb.extend_from_slice(&elem_master(ids::CUE_POINT, &pbody));
    let dmx = open(build_cues_first(&cb, &[cluster_body(0, 0)]));

    let p = &dmx.cue_points()[0];
    assert_eq!(p.time, 750);
    assert_eq!(p.track_positions.len(), 2);
    assert_eq!(p.track_positions[0].track, 1);
    assert_eq!(p.track_positions[0].cluster_position, Some(0x100));
    assert_eq!(p.track_positions[1].track, 2);
    assert_eq!(p.track_positions[1].cluster_position, Some(0x200));
}

#[test]
fn multiple_cue_points_preserve_document_order() {
    // Several CuePoints surface in document order, distinct timestamps.
    let mut cb = Vec::new();
    cb.extend_from_slice(&cue_point(0, &ctp_minimal(1, 0x100)));
    cb.extend_from_slice(&cue_point(1000, &ctp_minimal(1, 0x200)));
    cb.extend_from_slice(&cue_point(2000, &ctp_minimal(1, 0x300)));
    let dmx = open(build_cues_first(&cb, &[cluster_body(0, 0)]));

    let pts = dmx.cue_points();
    assert_eq!(pts.len(), 3);
    assert_eq!(pts[0].time, 0);
    assert_eq!(pts[1].time, 1000);
    assert_eq!(pts[2].time, 2000);
    assert_eq!(pts[2].track_positions[0].cluster_position, Some(0x300));
}

#[test]
fn cues_after_last_cluster_late_scan_path() {
    // The common index-at-end layout drives the best-effort scan_cues_from
    // path; the typed collector is fed there too.
    let mut tp = Vec::new();
    tp.extend_from_slice(&elem_uint(ids::CUE_TRACK, 1));
    tp.extend_from_slice(&elem_uint(ids::CUE_CLUSTER_POSITION, 0x55));
    tp.extend_from_slice(&elem_uint(ids::CUE_DURATION, 960));
    tp.extend_from_slice(&elem_uint(ids::CUE_BLOCK_NUMBER, 1));
    let ctp = elem_master(ids::CUE_TRACK_POSITIONS, &tp);
    let cb = cue_point(123, &ctp);

    let dmx = open(build_cues_last(&cb, &[cluster_body(0, 0x11)]));
    let pts = dmx.cue_points();
    assert_eq!(pts.len(), 1);
    assert_eq!(pts[0].time, 123);
    let p = &pts[0].track_positions[0];
    assert_eq!(p.cluster_position, Some(0x55));
    assert_eq!(p.duration, Some(960));
    assert_eq!(p.block_number, Some(1));
}

#[test]
fn unknown_child_inside_cue_track_positions_skipped() {
    // An unrecognised element inside CueTrackPositions is skipped without
    // disturbing the recognised siblings (forward-compat).
    let mut tp = Vec::new();
    tp.extend_from_slice(&elem_uint(ids::CUE_TRACK, 1));
    // Inject an unknown element id (0x80 is a valid 1-byte VINT id not used
    // inside CueTrackPositions).
    tp.extend_from_slice(&write_element_id(0x80));
    tp.extend_from_slice(&write_vint(2, 0));
    tp.extend_from_slice(&[0xDE, 0xAD]);
    tp.extend_from_slice(&elem_uint(ids::CUE_CLUSTER_POSITION, 0x77));
    let ctp = elem_master(ids::CUE_TRACK_POSITIONS, &tp);

    let dmx = open(build_cues_first(&cue_point(0, &ctp), &[cluster_body(0, 0)]));
    let p = &dmx.cue_points()[0].track_positions[0];
    assert_eq!(p.track, 1);
    assert_eq!(p.cluster_position, Some(0x77));
}

#[test]
fn explicit_codec_state_distinct_from_default() {
    // An explicit non-zero CueCodecState round-trips; absence materialises
    // the spec default 0 (covered by minimal_cue_point_*). Here we pin the
    // explicit-zero case: a written 0 is observationally identical to the
    // default, which is correct per §5.1.5.1.2.6 (default 0).
    let mut tp = Vec::new();
    tp.extend_from_slice(&elem_uint(ids::CUE_TRACK, 1));
    tp.extend_from_slice(&elem_uint(ids::CUE_CLUSTER_POSITION, 0x10));
    tp.extend_from_slice(&elem_uint(ids::CUE_CODEC_STATE, 0));
    let ctp = elem_master(ids::CUE_TRACK_POSITIONS, &tp);
    let dmx = open(build_cues_first(&cue_point(0, &ctp), &[cluster_body(0, 0)]));
    assert_eq!(dmx.cue_points()[0].track_positions[0].codec_state, 0);
}

#[test]
fn typed_view_and_packet_walk_coexist() {
    // The typed Cues view is populated at open time and remains stable while
    // the demuxer walks Clusters — reading packets does not perturb it.
    let mut cb = Vec::new();
    cb.extend_from_slice(&cue_point(0, &ctp_minimal(1, 0x40)));
    let clusters = vec![cluster_body(0, 0xAB)];
    let mut dmx = open(build_cues_first(&cb, &clusters));

    // Typed surface reports the (track, time) pair from the on-disk cue.
    {
        let p = &dmx.cue_points()[0];
        assert_eq!(p.time, 0);
        assert_eq!(p.track_positions[0].track, 1);
    }

    // Draining the first packet leaves the typed cue view unchanged.
    let pkt = dmx.next_packet().expect("first packet");
    assert_eq!(pkt.data, vec![0xAB]);
    let p = &dmx.cue_points()[0];
    assert_eq!(p.time, 0);
    assert_eq!(p.track_positions[0].cluster_position, Some(0x40));
}

#[test]
fn default_cue_point_equals_empty_decode() {
    // CuePoint::default() matches a parsed CuePoint that carried nothing
    // but the structural shell — useful for re-mux comparisons.
    let dflt = CuePoint::default();
    assert_eq!(dflt.time, 0);
    assert!(dflt.track_positions.is_empty());
    let tp = CueTrackPositions::default();
    assert_eq!(tp.codec_state, 0);
    assert_eq!(tp.cluster_position, None);
    assert!(tp.references.is_empty());
}
