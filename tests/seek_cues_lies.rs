//! Seek-pathology depth: a `Cues` index that parses cleanly but LIES.
//!
//! RFC 9559 makes the index pure metadata — every `CueClusterPosition` /
//! `CueTime` / `CueRelativePosition` / `CueBlockNumber` claim restates
//! information about bytes that exist elsewhere in the Segment
//! (§5.1.5.1, Section 16), so every claim can be checked against the
//! Segment itself. The real-world damage class is a *stale* index (the
//! file was edited or truncated after the `Cues` element was written);
//! the hostile class is a forged one. Two surfaces are under test:
//!
//! * `MkvDemuxer::audit_cues()` — the whole-index audit, per-claim typed
//!   findings (`CueLieKind`), available on strict and resilient opens,
//!   read-only with respect to demux state.
//! * the resilient `seek_to` trust-but-verify path — a lying cue records
//!   a `DamageKind::CueLie` event and the seek falls back to the linear
//!   Cluster-`Timestamp` scan (RFC 9559 §26 leaves error handling to the
//!   Reader), landing correctly instead of feeding `next_packet` garbage
//!   or silently overshooting. The strict path stays trusting,
//!   byte-for-byte unchanged.
//!
//! Files are crafted by hand (same approach as `tests/seek_cues.rs`) so
//! the Cues byte layout — including the lies — is controlled exactly.
//! Cluster timestamps sit 100_000 ticks apart: far enough that the §11.2
//! Block-timestamp bound (32768 Track Ticks around a Cluster
//! `Timestamp`) can prove a `CueTime` impossible.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_mkv::demux::{CueLieKind, DamageKind, MkvDemuxer};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

// ---------------------------------------------------------------------
// EBML construction helpers (mirrors tests/seek_cues.rs).
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

/// Encode a uint forced to exactly 8 payload bytes — keeps the Cues
/// element length stable across the two-pass offset fixup.
fn u64_fixed8(id: u32, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(8, 0));
    out.extend_from_slice(&value.to_be_bytes());
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

/// One `CueTrackPositions` row to write into the crafted `Cues`.
#[derive(Clone, Copy)]
struct CueSpec {
    track: u64,
    /// `CueTime`, Segment Ticks (1 ms at the file's TimestampScale).
    time: u64,
    /// `CueClusterPosition` — Segment Position. `None` = the truthful
    /// offset of `cluster_index` below; `Some(v)` = write `v` (the lie).
    offset: Option<u64>,
    /// Which real cluster the truthful offset refers to.
    cluster_index: usize,
    /// Optional `CueRelativePosition` to write verbatim.
    rel: Option<u64>,
    /// Optional `CueBlockNumber` to write verbatim.
    block_number: Option<u64>,
}

impl CueSpec {
    fn truthful(track: u64, time: u64, cluster_index: usize) -> Self {
        CueSpec {
            track,
            time,
            offset: None,
            cluster_index,
            rel: None,
            block_number: None,
        }
    }
}

/// Everything the tests need to reason about the crafted file's layout.
struct Built {
    bytes: Vec<u8>,
    /// Segment-relative offset of each Cluster header.
    cluster_offsets: Vec<u64>,
    /// Absolute file offset of the first Segment payload byte.
    segment_payload_start: u64,
    /// Segment-relative offset of the `Void` element (when requested).
    void_offset: Option<u64>,
    /// Segment-relative offset of each Cluster's first `SimpleBlock`
    /// *within its Cluster body* (i.e. a truthful `CueRelativePosition`).
    rel_of_block: Vec<u64>,
}

/// Build: EBML header ++ Segment(Info, Tracks, Cues, [Void], Clusters).
/// One PCM track (TrackNumber=1), TimestampScale 1 ms, one keyframe
/// SimpleBlock per Cluster with payload byte `0xA0 + index`.
fn build(cluster_times: &[u64], cues: &[CueSpec], tts: Option<f64>, with_void: bool) -> Built {
    let mut ebml_body = Vec::new();
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    ebml_body.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    let ebml_header = elem_master(ids::EBML_HEADER, &ebml_body);

    let mut info_body = Vec::new();
    info_body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info_body.extend_from_slice(&elem_float_be_f64(
        ids::DURATION,
        *cluster_times.last().unwrap_or(&0) as f64 + 1000.0,
    ));
    info_body.extend_from_slice(&elem_str(ids::MUXING_APP, "oxideav-test"));
    info_body.extend_from_slice(&elem_str(ids::WRITING_APP, "oxideav-test"));
    let info = elem_master(ids::INFO, &info_body);

    let mut track_body = Vec::new();
    track_body.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_UID, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    track_body.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    if let Some(tts) = tts {
        track_body.extend_from_slice(&elem_float_be_f64(ids::TRACK_TIMESTAMP_SCALE, tts));
    }
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio_body.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio_body.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    track_body.extend_from_slice(&elem_master(ids::AUDIO, &audio_body));
    let tracks = elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &track_body));

    // Clusters + the truthful CueRelativePosition of each first Block.
    let mut clusters: Vec<Vec<u8>> = Vec::new();
    let mut rel_of_block: Vec<u64> = Vec::new();
    for (i, &t) in cluster_times.iter().enumerate() {
        let tc = elem_uint(ids::TIMECODE, t);
        rel_of_block.push(tc.len() as u64);
        let mut body = tc;
        body.extend_from_slice(&simple_block(1, 0, true, 0xA0 + i as u8));
        clusters.push(elem_master(ids::CLUSTER, &body));
    }
    let void = if with_void {
        // Void, 6-byte body.
        let mut v = Vec::new();
        v.extend_from_slice(&write_element_id(ids::VOID));
        v.extend_from_slice(&write_vint(6, 0));
        v.extend_from_slice(&[0u8; 6]);
        v
    } else {
        Vec::new()
    };

    // Two-pass: Cues length is stable because CueClusterPosition is
    // fixed-8; times / rel / block numbers don't change between passes.
    let build_cues = |cluster_offsets: &[u64]| -> Vec<u8> {
        let mut body = Vec::new();
        for c in cues {
            let mut ctp = Vec::new();
            ctp.extend_from_slice(&elem_uint(ids::CUE_TRACK, c.track));
            let off = c
                .offset
                .unwrap_or_else(|| cluster_offsets.get(c.cluster_index).copied().unwrap_or(0));
            ctp.extend_from_slice(&u64_fixed8(ids::CUE_CLUSTER_POSITION, off));
            if let Some(rel) = c.rel {
                ctp.extend_from_slice(&elem_uint(ids::CUE_RELATIVE_POSITION, rel));
            }
            if let Some(n) = c.block_number {
                ctp.extend_from_slice(&elem_uint(ids::CUE_BLOCK_NUMBER, n));
            }
            let mut cp = Vec::new();
            cp.extend_from_slice(&elem_uint(ids::CUE_TIME, c.time));
            cp.extend_from_slice(&elem_master(ids::CUE_TRACK_POSITIONS, &ctp));
            body.extend_from_slice(&elem_master(ids::CUE_POINT, &cp));
        }
        elem_master(ids::CUES, &body)
    };

    let layout = |cues_len: u64| -> (Vec<u64>, Option<u64>) {
        let mut off = info.len() as u64 + tracks.len() as u64 + cues_len;
        let void_off = if with_void { Some(off) } else { None };
        off += void.len() as u64;
        let mut cluster_offsets = Vec::new();
        for c in &clusters {
            cluster_offsets.push(off);
            off += c.len() as u64;
        }
        (cluster_offsets, void_off)
    };

    let placeholder = build_cues(&vec![0; clusters.len()]);
    let (cluster_offsets, void_offset) = layout(placeholder.len() as u64);
    let cues_bytes = build_cues(&cluster_offsets);
    assert_eq!(cues_bytes.len(), placeholder.len(), "two-pass width drift");

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&cues_bytes);
    seg_body.extend_from_slice(&void);
    for c in &clusters {
        seg_body.extend_from_slice(c);
    }
    let segment = elem_master(ids::SEGMENT, &seg_body);
    let segment_payload_start = (ebml_header.len() + (segment.len() - seg_body.len())) as u64;

    let mut bytes = ebml_header;
    bytes.extend_from_slice(&segment);
    Built {
        bytes,
        cluster_offsets,
        segment_payload_start,
        void_offset,
        rel_of_block,
    }
}

fn open_strict(bytes: &[u8]) -> MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("strict open")
}

fn open_resilient(bytes: &[u8]) -> MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    oxideav_mkv::demux::open_resilient_typed(rs, &oxideav_core::NullCodecResolver)
        .expect("resilient open")
}

/// Three truthful clusters at 0 / 100_000 / 200_000 ticks.
const TIMES: [u64; 3] = [0, 100_000, 200_000];

fn truthful_cues() -> Vec<CueSpec> {
    TIMES
        .iter()
        .enumerate()
        .map(|(i, &t)| CueSpec::truthful(1, t, i))
        .collect()
}

// ---------------------------------------------------------------------
// Truthful baseline.
// ---------------------------------------------------------------------

#[test]
fn truthful_index_audits_clean_in_both_modes() {
    // Give the middle cue a truthful CueRelativePosition and the last a
    // truthful CueBlockNumber so all three fine-grained arms run too.
    let mut cues = truthful_cues();
    let built0 = build(&TIMES, &cues, None, false);
    cues[1].rel = Some(built0.rel_of_block[1]);
    cues[2].block_number = Some(1);
    let built = build(&TIMES, &cues, None, false);

    for mut dmx in [open_strict(&built.bytes), open_resilient(&built.bytes)] {
        let report = dmx.audit_cues().expect("audit");
        assert_eq!(report.entries_checked(), 3);
        assert!(report.is_truthful(), "findings: {:?}", report.findings());
        assert_eq!(report.findings_total(), 0);
        assert!(report.findings().is_empty());
        // Seeks still resolve through the (truthful) index: no fallback,
        // no damage events.
        let landed = dmx.seek_to(0, 200_000).expect("seek");
        assert_eq!(landed, 200_000);
        assert!(dmx.damage_events().is_empty());
        let pkt = dmx.next_packet().expect("packet after seek");
        assert_eq!(pkt.data.as_slice(), &[0xA2]);
    }
}

#[test]
fn audit_does_not_disturb_streaming_state() {
    let built = build(&TIMES, &truthful_cues(), None, false);

    // Reference run: no audit.
    let mut plain = open_strict(&built.bytes);
    let mut expected = Vec::new();
    while let Ok(p) = plain.next_packet() {
        expected.push((p.pts, p.data.as_slice().to_vec()));
    }
    assert_eq!(expected.len(), 3);

    // Audited run: audit before, between, and after packets.
    let mut dmx = open_strict(&built.bytes);
    assert!(dmx.audit_cues().expect("audit pre").is_truthful());
    let mut got = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        got.push((p.pts, p.data.as_slice().to_vec()));
        assert!(dmx.audit_cues().expect("audit mid").is_truthful());
    }
    assert_eq!(got, expected, "audit calls must not perturb the stream");
    assert!(dmx.damage_events().is_empty());
    assert!(dmx.audit_cues().expect("audit post").is_truthful());
}

// ---------------------------------------------------------------------
// CueClusterPosition lies (offset class).
// ---------------------------------------------------------------------

#[test]
fn offset_into_cluster_body_is_target_not_cluster() {
    // The last cue points 3 bytes into cluster A's body — a parseable
    // element header lives there (the Timestamp child), but it is not a
    // Cluster.
    let mut cues = truthful_cues();
    let probe = build(&TIMES, &cues, None, false);
    cues[2].offset = Some(probe.cluster_offsets[0] + 3);
    let built = build(&TIMES, &cues, None, false);

    let mut dmx = open_strict(&built.bytes);
    let report = dmx.audit_cues().expect("audit");
    assert_eq!(report.findings_total(), 1);
    let f = report.findings()[0];
    assert_eq!(f.kind(), CueLieKind::TargetNotCluster);
    assert_eq!(f.entry_index(), 2);
    assert_eq!(f.track(), 1);
    assert_eq!(f.cue_time(), 200_000);
    assert_eq!(f.cluster_offset(), probe.cluster_offsets[0] + 3);
    assert_eq!(
        f.target_offset(),
        built.segment_payload_start + probe.cluster_offsets[0] + 3
    );

    // Resilient seek to the lied-about time recovers via the linear scan.
    let mut rdmx = open_resilient(&built.bytes);
    let landed = rdmx.seek_to(0, 200_000).expect("resilient seek");
    assert_eq!(landed, 200_000, "fallback must land the real cluster C");
    let pkt = rdmx.next_packet().expect("packet");
    assert_eq!(pkt.data.as_slice(), &[0xA2]);
    let events = rdmx.damage_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind(), DamageKind::CueLie);
    assert_eq!(
        events[0].offset(),
        built.segment_payload_start + probe.cluster_offsets[0] + 3
    );
    assert_eq!(
        events[0].resumed_at(),
        Some(built.segment_payload_start + built.cluster_offsets[2]),
        "fallback resumed on the real Cluster C header"
    );
    assert_eq!(events[0].bytes_skipped(), 0);
}

#[test]
fn offset_past_segment_end_is_target_out_of_segment() {
    let mut cues = truthful_cues();
    cues[2].offset = Some(1 << 40);
    let built = build(&TIMES, &cues, None, false);

    let mut dmx = open_strict(&built.bytes);
    let report = dmx.audit_cues().expect("audit");
    assert_eq!(report.findings_total(), 1);
    assert_eq!(report.findings()[0].kind(), CueLieKind::TargetOutOfSegment);
    assert_eq!(report.findings()[0].entry_index(), 2);

    let mut rdmx = open_resilient(&built.bytes);
    let landed = rdmx.seek_to(0, 200_000).expect("resilient seek");
    assert_eq!(landed, 200_000);
    assert_eq!(rdmx.next_packet().expect("pkt").data.as_slice(), &[0xA2]);
    assert_eq!(rdmx.damage_events().len(), 1);
    assert_eq!(rdmx.damage_events()[0].kind(), DamageKind::CueLie);
}

#[test]
fn offset_at_void_element_is_target_not_cluster() {
    let mut cues = truthful_cues();
    let probe = build(&TIMES, &cues, None, true);
    cues[1].offset = Some(probe.void_offset.expect("void"));
    let built = build(&TIMES, &cues, None, true);

    let mut dmx = open_strict(&built.bytes);
    let report = dmx.audit_cues().expect("audit");
    assert_eq!(report.findings_total(), 1);
    assert_eq!(report.findings()[0].kind(), CueLieKind::TargetNotCluster);
    assert_eq!(report.findings()[0].entry_index(), 1);

    let mut rdmx = open_resilient(&built.bytes);
    let landed = rdmx.seek_to(0, 100_000).expect("resilient seek");
    assert_eq!(landed, 100_000);
    assert_eq!(rdmx.next_packet().expect("pkt").data.as_slice(), &[0xA1]);
    assert_eq!(rdmx.damage_events().len(), 1);
    assert_eq!(rdmx.damage_events()[0].kind(), DamageKind::CueLie);
}

#[test]
fn strict_seek_stays_trusting_on_a_lying_offset() {
    // The strict path's documented contract: trust the index, surface
    // whatever the landing yields — no fallback, no DamageEvent, and
    // (critically) NOT the packet the truthful index would have found.
    let mut cues = truthful_cues();
    let probe = build(&TIMES, &cues, None, false);
    cues[2].offset = Some(probe.cluster_offsets[0] + 3);
    let built = build(&TIMES, &cues, None, false);

    let mut dmx = open_strict(&built.bytes);
    let landed = dmx.seek_to(0, 200_000).expect("strict seek trusts the cue");
    assert_eq!(landed, 200_000, "strict returns the cue's own claim");
    assert!(dmx.damage_events().is_empty());
    // An error surface is equally within the strict contract — only a
    // packet claiming to be the truthfully-promised one would be wrong.
    if let Ok(p) = dmx.next_packet() {
        assert_ne!(
            p.data.as_slice(),
            &[0xA2],
            "a lying cue cannot produce the truthfully-promised packet"
        );
    }
}

// ---------------------------------------------------------------------
// CueTime lies (stale index / time class).
// ---------------------------------------------------------------------

#[test]
fn stale_index_timestamp_impossible() {
    // Every cue points at cluster C (timestamp 200_000) — the shape left
    // behind when clusters were rewritten but the index was not. CueTime
    // 0 and 100_000 both sit more than 32768 ticks before 200_000, so
    // the landed Timestamp proves them impossible (§11.2).
    let cues = vec![
        CueSpec::truthful(1, 0, 2),
        CueSpec::truthful(1, 100_000, 2),
        CueSpec::truthful(1, 200_000, 2),
    ];
    let built = build(&TIMES, &cues, None, false);

    let mut dmx = open_strict(&built.bytes);
    let report = dmx.audit_cues().expect("audit");
    assert_eq!(report.entries_checked(), 3);
    assert_eq!(report.findings_total(), 2);
    for (f, expect_time) in report.findings().iter().zip([0u64, 100_000]) {
        assert_eq!(f.kind(), CueLieKind::TimestampImpossible);
        assert_eq!(f.cue_time(), expect_time);
        assert_eq!(f.detail(), Some(200_000), "detail = landed Timestamp");
    }

    // Resilient seek to t=0 must not overshoot to cluster C: the lie is
    // detected and the linear scan lands cluster A.
    let mut rdmx = open_resilient(&built.bytes);
    let landed = rdmx.seek_to(0, 0).expect("resilient seek");
    assert_eq!(landed, 0);
    assert_eq!(rdmx.next_packet().expect("pkt").data.as_slice(), &[0xA0]);
    assert_eq!(rdmx.damage_events().len(), 1);
    assert_eq!(rdmx.damage_events()[0].kind(), DamageKind::CueLie);
    assert_eq!(rdmx.damage_events()[0].bytes_skipped(), 0);
}

#[test]
fn track_timestamp_scale_widens_the_slack() {
    // With TrackTimestampScale = 16.0 the §11.2 bound is 32768 × 16 =
    // 524288 ticks: a Cluster 300_000 ticks past the CueTime could still
    // legally contain the promised Block, so it is NOT flagged...
    let cues = vec![CueSpec::truthful(1, 0, 0)];
    let built = build(&[300_000], &cues, Some(16.0), false);
    let mut dmx = open_strict(&built.bytes);
    let report = dmx.audit_cues().expect("audit");
    assert!(
        report.is_truthful(),
        "300_000 <= 524288 slack: {:?}",
        report.findings()
    );

    // ...while 600_000 ticks past exceeds even the widened bound.
    let built = build(&[600_000], &cues, Some(16.0), false);
    let mut dmx = open_strict(&built.bytes);
    let report = dmx.audit_cues().expect("audit");
    assert_eq!(report.findings_total(), 1);
    assert_eq!(report.findings()[0].kind(), CueLieKind::TimestampImpossible);

    // And the default scale (1.0) flags 300_000 straight away.
    let built = build(&[300_000], &cues, None, false);
    let mut dmx = open_strict(&built.bytes);
    assert_eq!(dmx.audit_cues().expect("audit").findings_total(), 1);
}

// ---------------------------------------------------------------------
// Truncation (stale-after-cut class).
// ---------------------------------------------------------------------

#[test]
fn truncated_file_flags_cluster_truncated_then_out_of_segment() {
    let built = build(&TIMES, &truthful_cues(), None, false);
    let abs_c = (built.segment_payload_start + built.cluster_offsets[2]) as usize;

    // Cut inside cluster C's body (header + Timestamp survive): the
    // index's offset still lands a Cluster, but its declared body runs
    // past the (clamped) Segment end.
    let cut_mid = &built.bytes[..abs_c + 8];
    let mut rdmx = open_resilient(cut_mid);
    assert!(
        rdmx.damage_events()
            .iter()
            .any(|e| e.kind() == DamageKind::SegmentTruncated),
        "resilient open must clamp the declared Segment size"
    );
    let report = rdmx.audit_cues().expect("audit");
    assert_eq!(report.findings_total(), 1);
    assert_eq!(report.findings()[0].kind(), CueLieKind::ClusterTruncated);
    assert_eq!(report.findings()[0].entry_index(), 2);
    // Landing on a truncated Cluster is not a seek lie — the surviving
    // prefix is still the right place.
    let n_before = rdmx.damage_events().len();
    let landed = rdmx.seek_to(0, 200_000).expect("seek");
    assert_eq!(landed, 200_000);
    assert!(
        !rdmx.damage_events()[n_before..]
            .iter()
            .any(|e| e.kind() == DamageKind::CueLie),
        "a truncated-but-present Cluster must not be treated as a lie"
    );

    // Cut *before* cluster C's header: the referenced Cluster is gone.
    let cut_before = &built.bytes[..abs_c];
    let mut rdmx = open_resilient(cut_before);
    let report = rdmx.audit_cues().expect("audit");
    assert_eq!(report.findings_total(), 1);
    assert_eq!(report.findings()[0].kind(), CueLieKind::TargetOutOfSegment);
    // Resilient seek to the vanished time falls back and lands on the
    // last surviving cluster instead of parking at nothing.
    let landed = rdmx.seek_to(0, 200_000).expect("seek");
    assert_eq!(landed, 100_000, "lands the last surviving Cluster");
    assert_eq!(rdmx.next_packet().expect("pkt").data.as_slice(), &[0xA1]);
    assert!(rdmx
        .damage_events()
        .iter()
        .any(|e| e.kind() == DamageKind::CueLie));
}

// ---------------------------------------------------------------------
// Fine-grained lies: CueRelativePosition / CueBlockNumber.
// ---------------------------------------------------------------------

#[test]
fn relative_position_lies_are_flagged_but_not_seek_lies() {
    // rel = 1000 runs past the tiny Cluster body; rel = 0 points at the
    // Timestamp child, which is not a SimpleBlock / BlockGroup.
    let mut cues = truthful_cues();
    cues[1].rel = Some(1000);
    cues[2].rel = Some(0);
    let built = build(&TIMES, &cues, None, false);

    let mut dmx = open_strict(&built.bytes);
    let report = dmx.audit_cues().expect("audit");
    assert_eq!(report.findings_total(), 2);
    assert!(report
        .findings()
        .iter()
        .all(|f| f.kind() == CueLieKind::RelativePositionInvalid));
    assert_eq!(report.findings()[0].entry_index(), 1);
    assert_eq!(report.findings()[1].entry_index(), 2);

    // seek_to already degrades these to a cluster-start walk — correct
    // landing, no CueLie event, in both modes.
    for mut d in [open_strict(&built.bytes), open_resilient(&built.bytes)] {
        let landed = d.seek_to(0, 100_000).expect("seek");
        assert_eq!(landed, 100_000);
        assert_eq!(d.next_packet().expect("pkt").data.as_slice(), &[0xA1]);
        assert!(d.damage_events().is_empty());
    }
}

#[test]
fn block_number_lies_are_flagged_but_not_seek_lies() {
    // Each cluster holds exactly one Block: n=5 is unreachable, n=0 is
    // the spec-illegal value (§5.1.5.1.2.5 range "not 0").
    let mut cues = truthful_cues();
    cues[1].block_number = Some(5);
    cues[2].block_number = Some(0);
    let built = build(&TIMES, &cues, None, false);

    let mut dmx = open_strict(&built.bytes);
    let report = dmx.audit_cues().expect("audit");
    assert_eq!(report.findings_total(), 2);
    assert_eq!(
        report.findings()[0].kind(),
        CueLieKind::BlockNumberOutOfRange
    );
    assert_eq!(report.findings()[0].detail(), Some(1), "one Block found");
    assert_eq!(
        report.findings()[1].kind(),
        CueLieKind::BlockNumberOutOfRange
    );
    assert_eq!(report.findings()[1].detail(), Some(0));

    for mut d in [open_strict(&built.bytes), open_resilient(&built.bytes)] {
        let landed = d.seek_to(0, 100_000).expect("seek");
        assert_eq!(landed, 100_000);
        assert_eq!(d.next_packet().expect("pkt").data.as_slice(), &[0xA1]);
        assert!(d.damage_events().is_empty());
    }
}

#[test]
fn unknown_track_is_flagged_and_never_consulted() {
    let mut cues = truthful_cues();
    cues.push(CueSpec::truthful(9, 100_000, 1)); // no track 9 exists
    let built = build(&TIMES, &cues, None, false);

    let mut dmx = open_strict(&built.bytes);
    let report = dmx.audit_cues().expect("audit");
    assert_eq!(report.entries_checked(), 4);
    assert_eq!(report.findings_total(), 1);
    assert_eq!(report.findings()[0].kind(), CueLieKind::UnknownTrack);
    assert_eq!(report.findings()[0].track(), 9);

    // Seeking the real track never consults the dangling entry.
    let mut rdmx = open_resilient(&built.bytes);
    let landed = rdmx.seek_to(0, 100_000).expect("seek");
    assert_eq!(landed, 100_000);
    assert!(rdmx.damage_events().is_empty());
}

// ---------------------------------------------------------------------
// Hostile depth: findings cap + no-panic sweeps.
// ---------------------------------------------------------------------

#[test]
fn hostile_index_findings_are_capped_with_exact_counter() {
    // 5000 entries, every one lying with the same out-of-segment offset
    // — the probe cache collapses them to a single probe, the findings
    // list caps at 4096, and the counter keeps the exact total.
    let mut cues = vec![CueSpec::truthful(1, 0, 0)];
    for i in 0..5000u64 {
        cues.push(CueSpec {
            track: 1,
            time: i,
            offset: Some(1 << 40),
            cluster_index: 0,
            rel: None,
            block_number: None,
        });
    }
    let built = build(&TIMES, &cues, None, false);
    let mut dmx = open_strict(&built.bytes);
    let report = dmx.audit_cues().expect("audit");
    assert_eq!(report.entries_checked(), 5001);
    assert_eq!(report.findings_total(), 5000);
    assert_eq!(report.findings().len(), 4096);
    assert!(!report.is_truthful());
}

#[test]
fn audit_never_panics_on_fuzz_corpus_and_soup() {
    // Every seed in the fuzz corpus, both open modes.
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/demux");
    let mut seeds = 0;
    for entry in std::fs::read_dir(&corpus).expect("corpus dir") {
        let bytes = std::fs::read(entry.expect("entry").path()).expect("read seed");
        seeds += 1;
        for open in [
            oxideav_mkv::demux::open_typed,
            oxideav_mkv::demux::open_resilient_typed,
        ] {
            let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.clone()));
            if let Ok(mut dmx) = open(rs, &oxideav_core::NullCodecResolver) {
                let report = dmx.audit_cues().expect("audit is IO-error-free in memory");
                assert!(report.findings().len() as u64 <= report.findings_total());
                assert_eq!(report.is_truthful(), report.findings_total() == 0);
            }
        }
    }
    assert!(seeds >= 5, "corpus seeds present");

    // Deterministic byte soup around a valid prefix: splice garbage into
    // a truthful file's Cues region and audit whatever still opens.
    let built = build(&TIMES, &truthful_cues(), None, false);
    let mut state = 0x243F6A8885A308D3u64; // splitmix64 seed
    let mut next = move || {
        state = state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    };
    for _ in 0..200 {
        let mut bytes = built.bytes.clone();
        let n = bytes.len();
        for _ in 0..8 {
            let at = (next() as usize) % n;
            bytes[at] = next() as u8;
        }
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
        if let Ok(mut dmx) =
            oxideav_mkv::demux::open_resilient_typed(rs, &oxideav_core::NullCodecResolver)
        {
            let report = dmx.audit_cues().expect("audit");
            assert!(report.findings().len() <= 4096);
            let _ = dmx.seek_to(0, 100_000);
            let _ = dmx.next_packet();
        }
    }
}

#[test]
fn forged_max_cue_position_never_panics() {
    // A fixed-8 CueClusterPosition of 2^64-1: the strict seek's
    // absolute-offset computation must saturate, not overflow (a
    // debug-build panic fuzz-found 2026-08 the moment a
    // fixed-8-position seed entered the corpus). The staged regression
    // input is the raw fuzz artifact; the hand-built equivalent pins
    // both modes' semantics.
    let mut cues = truthful_cues();
    cues[2].offset = Some(u64::MAX);
    let built = build(&TIMES, &cues, None, false);

    // Strict: trusts, saturates, parks past EoF — Ok seek, then a clean
    // end (or error), never a panic.
    let mut dmx = open_strict(&built.bytes);
    let landed = dmx.seek_to(0, 200_000).expect("strict seek");
    assert_eq!(landed, 200_000);
    assert!(
        dmx.next_packet().is_err(),
        "nothing lives at the fake offset"
    );

    // Resilient: the lie is caught up front and the seek recovers.
    let mut rdmx = open_resilient(&built.bytes);
    assert_eq!(rdmx.seek_to(0, 200_000).expect("resilient seek"), 200_000);
    assert_eq!(rdmx.next_packet().expect("pkt").data.as_slice(), &[0xA2]);
    assert_eq!(rdmx.damage_events().len(), 1);
    assert_eq!(rdmx.damage_events()[0].kind(), DamageKind::CueLie);

    // The staged fuzz artifact replays clean through every entry point
    // the harness drives (the corpus replay test covers the resilient
    // path fleet-wide; this pins the strict seek that crashed).
    let raw = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fuzz/corpus/demux/regression_cue_position_overflow.bin"
    ))
    .expect("regression seed present");
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(raw));
    if let Ok(mut d) = oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver) {
        let _ = d.seek_to(0, 0);
        let _ = d.next_packet();
    }
}

#[test]
fn each_recovered_seek_records_its_own_event() {
    // Every resilient seek is an independent operation: seeking the same
    // lying cue twice performs two recoveries and records two `CueLie`
    // events — the bookkeeping is a log of decisions, not a set.
    let mut cues = truthful_cues();
    cues[2].offset = Some(1 << 40);
    let built = build(&TIMES, &cues, None, false);

    let mut rdmx = open_resilient(&built.bytes);
    assert_eq!(rdmx.seek_to(0, 200_000).expect("first seek"), 200_000);
    assert_eq!(rdmx.seek_to(0, 200_000).expect("second seek"), 200_000);
    let lies: Vec<_> = rdmx
        .damage_events()
        .iter()
        .filter(|e| e.kind() == DamageKind::CueLie)
        .collect();
    assert_eq!(lies.len(), 2);
    assert_eq!(lies[0].offset(), lies[1].offset());
    // A truthful seek in between records nothing.
    assert_eq!(rdmx.seek_to(0, 0).expect("truthful seek"), 0);
    assert_eq!(rdmx.damage_events().len(), 2);
    assert_eq!(rdmx.next_packet().expect("pkt").data.as_slice(), &[0xA0]);
}

// ---------------------------------------------------------------------
// Fuzz corpus seed: a lying index from a well-formed start.
// ---------------------------------------------------------------------

/// The corpus seed's exact bytes: a well-formed document whose `Cues`
/// exercises every audit arm — truthful entries (including truthful
/// `CueRelativePosition` + `CueBlockNumber`) next to one lie of each
/// checkable class — so mutation fuzzing explores the trust-but-verify
/// seek and the audit from a start that reaches all of them.
fn build_cue_lies_seed() -> Vec<u8> {
    let mut cues = truthful_cues();
    let probe = build(&TIMES, &cues, None, false);
    cues[0].rel = Some(probe.rel_of_block[0]); // truthful fine-grained arms
    cues[0].block_number = Some(1);
    cues[1].offset = Some(probe.cluster_offsets[0] + 3); // TargetNotCluster
    cues[2].rel = Some(1000); // RelativePositionInvalid
    cues[2].block_number = Some(9); // BlockNumberOutOfRange
    cues.push(CueSpec::truthful(1, 5, 2)); // TimestampImpossible (200_000 > 5 + 32768)
    cues.push(CueSpec {
        track: 9, // UnknownTrack
        time: 150_000,
        offset: Some(1 << 40), // TargetOutOfSegment
        cluster_index: 0,
        rel: None,
        block_number: None,
    });
    build(&TIMES, &cues, None, false).bytes
}

#[test]
fn corpus_seed_cue_lies_matches_builder_and_pins_findings() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fuzz/corpus/demux/seed_cue_lies.mkv"
    );
    let bytes = build_cue_lies_seed();
    if std::env::var_os("OXIDEAV_MKV_WRITE_SEEDS").is_some() {
        std::fs::write(path, &bytes).expect("write seed");
    }
    let on_disk = std::fs::read(path).expect("cue-lies fuzz seed present");
    assert_eq!(on_disk, bytes, "committed seed must match the builder");

    // The seed must open in both modes and audit to exactly one finding
    // of each lied class (plus zero for the truthful entries).
    for mut dmx in [open_strict(&bytes), open_resilient(&bytes)] {
        let report = dmx.audit_cues().expect("audit");
        assert_eq!(report.entries_checked(), 5);
        let count = |k: CueLieKind| report.findings().iter().filter(|f| f.kind() == k).count();
        assert_eq!(count(CueLieKind::TargetNotCluster), 1);
        assert_eq!(count(CueLieKind::RelativePositionInvalid), 1);
        assert_eq!(count(CueLieKind::BlockNumberOutOfRange), 1);
        assert_eq!(count(CueLieKind::TimestampImpossible), 1);
        assert_eq!(count(CueLieKind::UnknownTrack), 1);
        assert_eq!(count(CueLieKind::TargetOutOfSegment), 1);
        assert_eq!(report.findings_total(), 6);
    }

    // And the resilient seek still reaches every real cluster.
    let mut rdmx = open_resilient(&bytes);
    assert_eq!(rdmx.seek_to(0, 100_000).expect("seek"), 100_000);
    assert_eq!(rdmx.next_packet().expect("pkt").data.as_slice(), &[0xA1]);
}

// ---------------------------------------------------------------------
// Our own muxer's index is truthful.
// ---------------------------------------------------------------------

#[test]
fn in_tree_muxer_output_audits_truthful() {
    use oxideav_core::{CodecId, CodecParameters, Packet, StreamInfo, TimeBase, WriteSeek};

    let mut vp = CodecParameters::video(CodecId::new("vp9"));
    vp.width = Some(320);
    vp.height = Some(240);
    let video = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: vp,
    };
    let streams = vec![video.clone()];
    let tmp = std::env::temp_dir().join("oxideav-mkv-cues-audit.webm");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mkv::mux::open_webm(ws, &streams).unwrap();
        mux.write_header().unwrap();
        // Keyframes every 6 s force multiple clusters + multiple cues.
        for i in 0..13i64 {
            let mut p = Packet::new(0, video.time_base, vec![i as u8; 40]);
            p.pts = Some(i * 1000);
            p.duration = Some(1000);
            p.flags.keyframe = i % 6 == 0;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open");
    let report = dmx.audit_cues().expect("audit");
    assert!(report.entries_checked() >= 2, "muxer emitted multiple cues");
    assert!(
        report.is_truthful(),
        "our own index must audit clean: {:?}",
        report.findings()
    );
    let _ = std::fs::remove_file(&tmp);
}
