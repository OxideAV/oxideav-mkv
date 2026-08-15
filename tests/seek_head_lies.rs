//! Seek-pathology depth, second index: a `SeekHead` that parses cleanly
//! but LIES.
//!
//! The MetaSeek index is the file's other self-referential structure:
//! each `Seek` entry pairs a `SeekID` (RFC 9559 §5.1.1.1.1) with a
//! `SeekPosition` (§5.1.1.1.2, a Segment Position per Section 16) —
//! pure metadata restating where a Top-Level Element lives, so every
//! claim can be checked against the Segment itself.
//! `MkvDemuxer::audit_seek_head()` is the `audit_cues()` counterpart:
//! typed `SeekLieKind` findings, strict and resilient opens alike,
//! read-only with respect to demux state. The open path already
//! trust-but-verifies the entries it *follows* (late post-Cluster
//! masters); the audit checks every entry, including the ones
//! navigation never needed.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_mkv::demux::{MkvDemuxer, SeekLieKind};
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

fn elem_bytes(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

fn elem_master(id: u32, body: &[u8]) -> Vec<u8> {
    elem_bytes(id, body)
}

/// `SeekPosition` forced to exactly 8 payload bytes — keeps the
/// SeekHead length stable across the two-pass offset fixup.
fn u64_fixed8(id: u32, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(8, 0));
    out.extend_from_slice(&value.to_be_bytes());
    out
}

fn simple_block(track: u8, tc_offset: i16, payload: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(0x80);
    body.push(payload);
    elem_bytes(ids::SIMPLE_BLOCK, &body)
}

/// Which Segment Position an entry's `SeekPosition` should carry.
#[derive(Clone, Copy)]
enum Pos {
    /// The truthful offset of the named real element.
    Info,
    Tracks,
    Cluster,
    /// A literal (lying) value.
    Lit(u64),
    /// Omit the mandatory `SeekPosition` child entirely.
    Absent,
}

/// One `Seek` entry: raw `SeekID` payload bytes + position spec.
struct SeekSpec {
    id_bytes: Vec<u8>,
    pos: Pos,
}

impl SeekSpec {
    fn of(id: u32, pos: Pos) -> Self {
        SeekSpec {
            id_bytes: write_element_id(id).to_vec(),
            pos,
        }
    }
}

struct Built {
    bytes: Vec<u8>,
    /// Segment-relative offset of the Tracks element (the one offset a
    /// test asserts against a finding's `seek_position`).
    off_tracks: u64,
}

/// Build: EBML header ++ Segment(SeekHead, Info, Tracks, Cluster).
fn build(entries: &[SeekSpec]) -> Built {
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
    info_body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 1000.0));
    info_body.extend_from_slice(&elem_str(ids::MUXING_APP, "oxideav-test"));
    info_body.extend_from_slice(&elem_str(ids::WRITING_APP, "oxideav-test"));
    let info = elem_master(ids::INFO, &info_body);

    let mut track_body = Vec::new();
    track_body.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_UID, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    track_body.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio_body.extend_from_slice(&elem_uint(ids::CHANNELS, 1));
    track_body.extend_from_slice(&elem_master(ids::AUDIO, &audio_body));
    let tracks = elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &track_body));

    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&simple_block(1, 0, 0xA0));
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    // Two-pass: fixed-8 positions keep the SeekHead length stable.
    let build_seek_head = |off_info: u64, off_tracks: u64, off_cluster: u64| -> Vec<u8> {
        let mut body = Vec::new();
        for e in entries {
            let mut seek = elem_bytes(ids::SEEK_ID, &e.id_bytes);
            match e.pos {
                Pos::Info => seek.extend_from_slice(&u64_fixed8(ids::SEEK_POSITION, off_info)),
                Pos::Tracks => seek.extend_from_slice(&u64_fixed8(ids::SEEK_POSITION, off_tracks)),
                Pos::Cluster => {
                    seek.extend_from_slice(&u64_fixed8(ids::SEEK_POSITION, off_cluster))
                }
                Pos::Lit(v) => seek.extend_from_slice(&u64_fixed8(ids::SEEK_POSITION, v)),
                Pos::Absent => {}
            }
            body.extend_from_slice(&elem_master(ids::SEEK, &seek));
        }
        elem_master(ids::SEEK_HEAD, &body)
    };

    let placeholder = build_seek_head(0, 0, 0);
    let off_info = placeholder.len() as u64;
    let off_tracks = off_info + info.len() as u64;
    let off_cluster = off_tracks + tracks.len() as u64;
    let seek_head = build_seek_head(off_info, off_tracks, off_cluster);
    assert_eq!(seek_head.len(), placeholder.len(), "two-pass width drift");

    let mut seg_body = seek_head;
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut bytes = ebml_header;
    bytes.extend_from_slice(&segment);
    Built { bytes, off_tracks }
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

#[test]
fn truthful_seek_head_audits_clean_in_both_modes() {
    let built = build(&[
        SeekSpec::of(ids::INFO, Pos::Info),
        SeekSpec::of(ids::TRACKS, Pos::Tracks),
        SeekSpec::of(ids::CLUSTER, Pos::Cluster),
    ]);
    for mut dmx in [open_strict(&built.bytes), open_resilient(&built.bytes)] {
        assert_eq!(dmx.seek_entries().len(), 3);
        let report = dmx.audit_seek_head().expect("audit");
        assert_eq!(report.entries_checked(), 3);
        assert!(report.is_truthful(), "findings: {:?}", report.findings());
        // Streaming still works after the audit.
        assert_eq!(dmx.next_packet().expect("pkt").data.as_slice(), &[0xA0]);
    }
}

#[test]
fn mismatched_target_reports_the_found_id() {
    // The SeekID promises Cues; the position lands on Tracks.
    let built = build(&[
        SeekSpec::of(ids::INFO, Pos::Info),
        SeekSpec::of(ids::CUES, Pos::Tracks),
    ]);
    let mut dmx = open_strict(&built.bytes);
    let report = dmx.audit_seek_head().expect("audit");
    assert_eq!(report.entries_checked(), 2);
    assert_eq!(report.findings_total(), 1);
    let f = report.findings()[0];
    assert_eq!(f.kind(), SeekLieKind::TargetMismatch);
    assert_eq!(f.entry_index(), 1);
    assert_eq!(f.seek_id(), Some(ids::CUES));
    assert_eq!(f.found_id(), Some(ids::TRACKS));
    assert_eq!(f.seek_position(), built.off_tracks);
}

#[test]
fn out_of_segment_and_garbage_targets_are_flagged() {
    // Entry 0 points far past the Segment; entry 1 points into the
    // middle of the Info body where no element header parses cleanly —
    // 3 bytes into Info sits the TimestampScale payload.
    let built = build(&[
        SeekSpec::of(ids::CUES, Pos::Lit(1 << 40)),
        SeekSpec::of(ids::TRACKS, Pos::Lit(0)), // points at the SeekHead itself
    ]);
    let mut dmx = open_strict(&built.bytes);
    let report = dmx.audit_seek_head().expect("audit");
    assert_eq!(report.findings_total(), 2);
    assert_eq!(report.findings()[0].kind(), SeekLieKind::TargetOutOfSegment);
    assert_eq!(report.findings()[0].seek_position(), 1 << 40);
    let f1 = report.findings()[1];
    assert_eq!(f1.kind(), SeekLieKind::TargetMismatch);
    assert_eq!(
        f1.found_id(),
        Some(ids::SEEK_HEAD),
        "position 0 is the SeekHead itself, not the promised Tracks"
    );
}

#[test]
fn missing_position_and_malformed_id_are_flagged() {
    let built = build(&[
        SeekSpec::of(ids::INFO, Pos::Absent),
        SeekSpec {
            id_bytes: vec![0xAA; 6], // 6-octet payload: not a 1..=4-octet EBML ID
            pos: Pos::Info,
        },
        SeekSpec::of(ids::TRACKS, Pos::Tracks),
    ]);
    let mut dmx = open_strict(&built.bytes);
    let report = dmx.audit_seek_head().expect("audit");
    assert_eq!(report.entries_checked(), 3);
    assert_eq!(report.findings_total(), 2);
    assert_eq!(report.findings()[0].kind(), SeekLieKind::MissingPosition);
    assert_eq!(report.findings()[0].entry_index(), 0);
    let f1 = report.findings()[1];
    assert_eq!(f1.kind(), SeekLieKind::MalformedId);
    assert_eq!(f1.seek_id(), None);
    assert!(
        report.findings().iter().all(|f| f.entry_index() != 2),
        "the truthful entry stays clean"
    );
}

#[test]
fn no_seek_head_is_trivially_truthful() {
    // Reuse the builder with zero entries: an empty SeekHead master.
    let built = build(&[]);
    let mut dmx = open_strict(&built.bytes);
    let report = dmx.audit_seek_head().expect("audit");
    assert_eq!(report.entries_checked(), 0);
    assert!(report.is_truthful());
}

#[test]
fn audit_does_not_disturb_streaming_state() {
    let built = build(&[
        SeekSpec::of(ids::INFO, Pos::Info),
        SeekSpec::of(ids::CUES, Pos::Lit(1 << 40)),
    ]);
    let mut plain = open_strict(&built.bytes);
    let mut expected = Vec::new();
    while let Ok(p) = plain.next_packet() {
        expected.push((p.pts, p.data.as_slice().to_vec()));
    }
    let mut dmx = open_strict(&built.bytes);
    assert_eq!(dmx.audit_seek_head().expect("audit").findings_total(), 1);
    let mut got = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        got.push((p.pts, p.data.as_slice().to_vec()));
        let _ = dmx.audit_seek_head().expect("audit mid-stream");
    }
    assert_eq!(got, expected, "audit calls must not perturb the stream");
}

#[test]
fn in_tree_muxer_seek_head_audits_truthful() {
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
    let tmp = std::env::temp_dir().join("oxideav-mkv-seekhead-audit.webm");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mkv::mux::open_webm(ws, &streams).unwrap();
        mux.write_header().unwrap();
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
    let report = dmx.audit_seek_head().expect("audit");
    assert!(
        report.entries_checked() >= 3,
        "muxer emits Info / Tracks / Cues rows"
    );
    assert!(
        report.is_truthful(),
        "our own SeekHead must audit clean: {:?}",
        report.findings()
    );
    // The Cues audit stays clean on the same file too — both indices.
    assert!(dmx.audit_cues().expect("cues audit").is_truthful());
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn audit_never_panics_on_fuzz_corpus() {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/demux");
    for entry in std::fs::read_dir(&corpus).expect("corpus dir") {
        let bytes = std::fs::read(entry.expect("entry").path()).expect("read seed");
        for open in [
            oxideav_mkv::demux::open_typed,
            oxideav_mkv::demux::open_resilient_typed,
        ] {
            let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.clone()));
            if let Ok(mut dmx) = open(rs, &oxideav_core::NullCodecResolver) {
                let report = dmx.audit_seek_head().expect("audit");
                assert!(report.findings().len() as u64 <= report.findings_total());
                assert_eq!(report.is_truthful(), report.findings_total() == 0);
            }
        }
    }
}
