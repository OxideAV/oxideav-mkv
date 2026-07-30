//! Whole-document schema validation (`oxideav_mkv::schema::validate`).
//!
//! Drives the validator over (a) the in-tree muxer's own output —
//! which must validate with zero violations — and (b) hand-built
//! documents exercising every violation kind: wrong parent, bad
//! type/length shape, out-of-range values, occurrence violations,
//! missing mandatory children, misplaced `CRC-32`, unknown-size on an
//! element the schema doesn't allow it on, plus the informational
//! kinds (unknown ID, deprecated element, version mismatch).

use std::io::Cursor;

use oxideav_core::{CodecId, CodecParameters, Muxer, Packet, StreamInfo, TimeBase, WriteSeek};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;
use oxideav_mkv::mux::MkvMuxer;
use oxideav_mkv::schema::{validate, SchemaFindingKind, SchemaReport};

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

fn elem_bin(id: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(data.len() as u64, 0));
    out.extend_from_slice(data);
    out
}

fn elem_master(id: u32, body: &[u8]) -> Vec<u8> {
    elem_bin(id, body)
}

fn elem_float32(id: u32, value: f32) -> Vec<u8> {
    elem_bin(id, &value.to_be_bytes())
}

fn ebml_header(doc_type: &str, version: u64) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    b.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, doc_type));
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, version));
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    elem_master(ids::EBML_HEADER, &b)
}

fn info() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    b.extend_from_slice(&elem_str(ids::MUXING_APP, "t"));
    b.extend_from_slice(&elem_str(ids::WRITING_APP, "t"));
    elem_master(ids::INFO, &b)
}

fn track_entry_body() -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    t.extend_from_slice(&elem_uint(ids::TRACK_UID, 1));
    t.extend_from_slice(&elem_uint(ids::TRACK_TYPE, 2));
    t.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio = Vec::new();
    audio.extend_from_slice(&elem_float32(ids::SAMPLING_FREQUENCY, 48_000.0));
    t.extend_from_slice(&elem_master(ids::AUDIO, &audio));
    t
}

fn tracks() -> Vec<u8> {
    elem_master(
        ids::TRACKS,
        &elem_master(ids::TRACK_ENTRY, &track_entry_body()),
    )
}

fn cluster() -> Vec<u8> {
    let mut sb = Vec::new();
    sb.extend_from_slice(&write_vint(1, 0));
    sb.extend_from_slice(&0i16.to_be_bytes());
    sb.push(0x80);
    sb.push(0xAA);
    let mut c = Vec::new();
    c.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    c.extend_from_slice(&elem_bin(ids::SIMPLE_BLOCK, &sb));
    elem_master(ids::CLUSTER, &c)
}

fn doc(children: &[&[u8]]) -> Vec<u8> {
    let mut seg = Vec::new();
    for c in children {
        seg.extend_from_slice(c);
    }
    let mut out = ebml_header("matroska", 4);
    out.extend_from_slice(&elem_master(ids::SEGMENT, &seg));
    out
}

fn run(bytes: &[u8]) -> SchemaReport {
    validate(&mut Cursor::new(bytes)).expect("validate")
}

fn kinds(report: &SchemaReport) -> Vec<SchemaFindingKind> {
    report.findings.iter().map(|f| f.kind).collect()
}

#[test]
fn well_formed_hand_built_document_is_valid() {
    let report = run(&doc(&[&info(), &tracks(), &cluster()]));
    assert!(
        report.is_valid(),
        "unexpected findings: {:?} (stopped {:?})",
        report.findings,
        report.scan_stopped_at
    );
    assert_eq!(report.violations, 0);
    assert_eq!(report.informational, 0);
    assert_eq!(report.doc_type.as_deref(), Some("matroska"));
    assert_eq!(report.doc_type_version, Some(4));
}

#[test]
fn muxer_output_validates_with_zero_violations() {
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mkv-r434-schemaval-{}.mkv",
        std::process::id()
    ));
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut p = CodecParameters::video(CodecId::new("vp9"));
        p.width = Some(320);
        p.height = Some(240);
        let stream = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 1000),
            duration: None,
            start_time: Some(0),
            params: p,
        };
        let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer");
        mx.with_seek_head_expansion_void(64).expect("knob");
        mx.write_header().expect("write_header");
        for i in 0..3 {
            let mut p = Packet::new(0, TimeBase::new(1, 1000), vec![0x42; 32]);
            p.pts = Some(i * 1000);
            p.duration = Some(1000);
            p.flags.keyframe = true;
            mx.write_packet(&p).expect("packet");
        }
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    let report = run(&bytes);
    assert!(
        report.is_valid(),
        "muxer output must schema-validate: {:?} (stopped {:?})",
        report.findings,
        report.scan_stopped_at
    );
    assert_eq!(report.violations, 0);
    assert_eq!(report.informational, 0, "{:?}", report.findings);
}

#[test]
fn wrong_parent_is_flagged() {
    // Channels (parent: Audio) dropped directly into Info.
    let mut bad_info = Vec::new();
    bad_info.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    bad_info.extend_from_slice(&elem_str(ids::MUXING_APP, "t"));
    bad_info.extend_from_slice(&elem_str(ids::WRITING_APP, "t"));
    bad_info.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    let bad_info = elem_master(ids::INFO, &bad_info);
    let report = run(&doc(&[&bad_info, &tracks(), &cluster()]));
    assert!(!report.is_valid());
    assert!(kinds(&report).iter().any(|k| matches!(
        k,
        SchemaFindingKind::WrongParent {
            expected: Some(id),
            actual: Some(actual)
        } if *id == ids::AUDIO && *actual == ids::INFO
    )));
}

#[test]
fn out_of_range_values_are_flagged() {
    // TrackType 0 violates "not 0"; ChapterFlagHidden-style 0-1 range
    // violated via FlagDefault = 2.
    let mut t = Vec::new();
    t.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    t.extend_from_slice(&elem_uint(ids::TRACK_UID, 1));
    t.extend_from_slice(&elem_uint(ids::TRACK_TYPE, 0)); // range "not 0"
    t.extend_from_slice(&elem_uint(ids::FLAG_DEFAULT, 2)); // range 0-1
    t.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let tracks = elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &t));
    let report = run(&doc(&[&info(), &tracks, &cluster()]));
    assert!(!report.is_valid());
    let flagged: Vec<u32> = report
        .findings
        .iter()
        .filter(|f| f.kind == SchemaFindingKind::OutOfRange)
        .map(|f| f.id)
        .collect();
    assert!(flagged.contains(&ids::TRACK_TYPE));
    assert!(flagged.contains(&ids::FLAG_DEFAULT));
}

#[test]
fn bad_length_shapes_are_flagged() {
    // A 5-byte float, a 3-byte SeekID (length attr 4), and a 15-byte
    // SegmentUUID (length attr 16).
    let mut audio = Vec::new();
    audio.extend_from_slice(&elem_bin(ids::SAMPLING_FREQUENCY, &[0u8; 5]));
    let mut t = track_entry_body();
    t.extend_from_slice(&elem_master(ids::AUDIO, &audio));
    let tracks = elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &t));

    let mut seek = Vec::new();
    seek.extend_from_slice(&elem_bin(ids::SEEK_ID, &[0x1C, 0x53, 0xBB]));
    seek.extend_from_slice(&elem_uint(ids::SEEK_POSITION, 1));
    let seek_head = elem_master(ids::SEEK_HEAD, &elem_master(ids::SEEK, &seek));

    let mut inf = Vec::new();
    inf.extend_from_slice(&elem_bin(ids::SEGMENT_UID, &[0xAB; 15]));
    inf.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    inf.extend_from_slice(&elem_str(ids::MUXING_APP, "t"));
    inf.extend_from_slice(&elem_str(ids::WRITING_APP, "t"));
    let inf = elem_master(ids::INFO, &inf);

    let report = run(&doc(&[&seek_head, &inf, &tracks, &cluster()]));
    assert!(!report.is_valid());
    let flagged: Vec<u32> = report
        .findings
        .iter()
        .filter(|f| f.kind == SchemaFindingKind::BadLength)
        .map(|f| f.id)
        .collect();
    assert!(flagged.contains(&ids::SAMPLING_FREQUENCY));
    assert!(flagged.contains(&ids::SEEK_ID));
    assert!(flagged.contains(&ids::SEGMENT_UID));
}

#[test]
fn occurrence_violations_are_flagged() {
    // Two TimestampScale children (maxOccurs 1) + a TrackEntry missing
    // its mandatory CodecID.
    let mut inf = Vec::new();
    inf.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    inf.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    inf.extend_from_slice(&elem_str(ids::MUXING_APP, "t"));
    inf.extend_from_slice(&elem_str(ids::WRITING_APP, "t"));
    let inf = elem_master(ids::INFO, &inf);

    let mut t = Vec::new();
    t.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    t.extend_from_slice(&elem_uint(ids::TRACK_UID, 1));
    t.extend_from_slice(&elem_uint(ids::TRACK_TYPE, 2));
    // No CodecID (minOccurs 1, no default).
    let tracks = elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &t));

    let report = run(&doc(&[&inf, &tracks, &cluster()]));
    assert!(!report.is_valid());
    assert!(report
        .findings
        .iter()
        .any(|f| f.kind == SchemaFindingKind::TooManyOccurrences && f.id == ids::TIMECODE_SCALE));
    assert!(report
        .findings
        .iter()
        .any(|f| f.kind == SchemaFindingKind::MissingMandatory && f.id == ids::CODEC_ID));
}

#[test]
fn misplaced_crc32_is_flagged() {
    // CRC-32 after Timestamp inside a Cluster (must be first child).
    let mut c = Vec::new();
    c.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    c.extend_from_slice(&elem_bin(ids::CRC32, &[0u8; 4]));
    let cl = elem_master(ids::CLUSTER, &c);
    let report = run(&doc(&[&info(), &tracks(), &cl]));
    assert!(!report.is_valid());
    assert!(report
        .findings
        .iter()
        .any(|f| f.kind == SchemaFindingKind::MisplacedCrc32));
    // A leading CRC-32 is NOT flagged.
    let mut c = Vec::new();
    c.extend_from_slice(&elem_bin(ids::CRC32, &[0u8; 4]));
    c.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    let cl = elem_master(ids::CLUSTER, &c);
    let report = run(&doc(&[&info(), &tracks(), &cl]));
    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.kind == SchemaFindingKind::MisplacedCrc32),
        "{:?}",
        report.findings
    );
}

#[test]
fn unknown_size_where_not_allowed_stops_walk() {
    let mut bad = Vec::new();
    bad.extend_from_slice(&write_element_id(ids::INFO));
    bad.push(0xFF); // unknown-size VINT — Info has no unknownsizeallowed
    let report = run(&doc(&[&bad]));
    assert!(!report.is_valid());
    assert!(report
        .findings
        .iter()
        .any(|f| f.kind == SchemaFindingKind::UnknownSizeNotAllowed && f.id == ids::INFO));
    assert!(report.scan_stopped_at.is_some());
}

#[test]
fn informational_kinds_do_not_fail_validation() {
    // UnknownId: a legacy Signature element (absent from the schema);
    // Deprecated: a SilentTracks master (maxver 0);
    // VersionMismatch: LanguageBCP47 (minver 4) in a DocTypeVersion-2
    // document.
    let sig = elem_bin(ids::SIGNATURE, &[0xDE, 0xAD]);
    let mut c = Vec::new();
    c.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    c.extend_from_slice(&elem_master(
        ids::SILENT_TRACKS,
        &elem_uint(ids::SILENT_TRACK_NUMBER, 1),
    ));
    let cl = elem_master(ids::CLUSTER, &c);
    let mut t = track_entry_body();
    t.extend_from_slice(&elem_str(ids::LANGUAGE_BCP47, "en"));
    let tracks = elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &t));

    let mut seg = Vec::new();
    seg.extend_from_slice(&info());
    seg.extend_from_slice(&tracks);
    seg.extend_from_slice(&sig);
    seg.extend_from_slice(&cl);
    let mut bytes = ebml_header("matroska", 2);
    bytes.extend_from_slice(&elem_master(ids::SEGMENT, &seg));

    let report = run(&bytes);
    assert!(
        report.is_valid(),
        "informational findings must not fail: {:?}",
        report.findings
    );
    assert_eq!(report.violations, 0);
    assert!(report.informational >= 4, "{:?}", report.findings);
    let ks = kinds(&report);
    assert!(ks.contains(&SchemaFindingKind::UnknownId));
    assert!(ks.contains(&SchemaFindingKind::Deprecated));
    assert!(ks.contains(&SchemaFindingKind::VersionMismatch));
    assert!(ks.iter().all(|k| !k.is_violation()));
}

/// Every fuzz corpus seed (valid, malformed, and crash-regression
/// alike) replays through the validator without panicking, and the
/// report invariants the fuzz harness asserts hold here too.
#[test]
fn fuzz_corpus_seeds_replay_through_validator() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/fuzz/corpus/demux");
    let mut seen = 0;
    for entry in std::fs::read_dir(dir).expect("fuzz corpus dir") {
        let path = entry.unwrap().path();
        if !path.is_file() {
            continue;
        }
        seen += 1;
        let data = std::fs::read(&path).unwrap();
        if let Ok(report) = validate(&mut Cursor::new(&data)) {
            if !report.findings_truncated {
                let recorded_violations = report
                    .findings
                    .iter()
                    .filter(|f| f.kind.is_violation())
                    .count() as u64;
                assert_eq!(report.violations, recorded_violations, "{path:?}");
            }
            assert_eq!(
                report.is_valid(),
                report.violations == 0 && report.scan_stopped_at.is_none(),
                "{path:?}"
            );
        }
    }
    assert!(seen >= 8, "corpus dir unexpectedly small: {seen}");
}

#[test]
fn arbitrary_bytes_never_panic_validator() {
    // Deterministic splitmix64-driven byte soup — the validator must
    // never panic, loop, or allocate unboundedly.
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut next = move || {
        state = state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    };
    for len in [0usize, 1, 7, 64, 512, 4096] {
        let mut bytes = Vec::with_capacity(len);
        while bytes.len() < len {
            bytes.extend_from_slice(&next().to_le_bytes());
        }
        bytes.truncate(len);
        let _ = validate(&mut Cursor::new(&bytes));
    }
    // Every prefix of a well-formed document too.
    let good = doc(&[&info(), &tracks(), &cluster()]);
    for cut in 0..good.len() {
        let _ = validate(&mut Cursor::new(&good[..cut]));
    }
}
