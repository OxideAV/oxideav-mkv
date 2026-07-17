//! WebM-profile conformance scanner tests (`oxideav_mkv::webm`).
//!
//! Exercises the guidelines support table (`webm_element_support`) and the
//! whole-file scanner (`webm::scan`):
//!
//! 1. Table shape: sorted by ID, expected per-status row counts, spot
//!    checks for headline elements.
//! 2. A hand-built minimal WebM document scans conformant.
//! 3. The in-crate Matroska muxer's output (which legitimately uses
//!    Matroska-only elements like `CRC-32`) is *not* WebM-conformant, and
//!    the findings name the exact off-profile IDs.
//! 4. Damage / hostile shapes: truncation, unknown-size on a non-
//!    Segment/Cluster master, deep nesting, and a findings-cap flood all
//!    scan without panicking, without unbounded allocation, and with the
//!    documented report semantics.
//!
//! These tests use the production EBML helpers to build byte streams —
//! no third-party Matroska code is consulted.

use std::io::Cursor;

use oxideav_core::{CodecId, CodecParameters, Muxer, Packet, StreamInfo, TimeBase, WriteSeek};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;
use oxideav_mkv::webm::{scan, webm_element_support, WebmSupport};

// ---------------------------------------------------------------------------
// EBML byte-building helpers.

fn element(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = write_element_id(id);
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

fn uint_element(id: u32, v: u64) -> Vec<u8> {
    let mut body = Vec::new();
    let mut started = false;
    for shift in (0..8).rev() {
        let b = ((v >> (shift * 8)) & 0xFF) as u8;
        if b != 0 || started || shift == 0 {
            body.push(b);
            started = true;
        }
    }
    element(id, &body)
}

fn string_element(id: u32, s: &str) -> Vec<u8> {
    element(id, s.as_bytes())
}

/// EBML header declaring the given DocType.
fn ebml_header(doc_type: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&uint_element(ids::EBML_VERSION, 1));
    body.extend_from_slice(&uint_element(ids::EBML_READ_VERSION, 1));
    body.extend_from_slice(&uint_element(ids::EBML_MAX_ID_LENGTH, 4));
    body.extend_from_slice(&uint_element(ids::EBML_MAX_SIZE_LENGTH, 8));
    body.extend_from_slice(&string_element(ids::EBML_DOC_TYPE, doc_type));
    body.extend_from_slice(&uint_element(ids::EBML_DOC_TYPE_VERSION, 4));
    body.extend_from_slice(&uint_element(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    element(ids::EBML_HEADER, &body)
}

/// A minimal all-Supported Segment: Info(TimestampScale) + Tracks(one
/// audio TrackEntry) + one Cluster with one SimpleBlock.
fn minimal_segment_body() -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(&uint_element(ids::TIMECODE_SCALE, 1_000_000));
    let info = element(ids::INFO, &info);

    let mut entry = Vec::new();
    entry.extend_from_slice(&uint_element(ids::TRACK_NUMBER, 1));
    entry.extend_from_slice(&uint_element(ids::TRACK_UID, 1));
    entry.extend_from_slice(&uint_element(ids::TRACK_TYPE, 2)); // audio
    entry.extend_from_slice(&string_element(ids::CODEC_ID, "A_OPUS"));
    let mut audio = Vec::new();
    audio.extend_from_slice(&element(ids::SAMPLING_FREQUENCY, &48000.0f32.to_be_bytes()));
    audio.extend_from_slice(&uint_element(ids::CHANNELS, 2));
    entry.extend_from_slice(&element(ids::AUDIO, &audio));
    let tracks = element(ids::TRACKS, &element(ids::TRACK_ENTRY, &entry));

    // SimpleBlock: track 1 (VINT 0x81), timestamp 0, flags keyframe.
    let mut sb = vec![0x81, 0x00, 0x00, 0x80];
    sb.extend_from_slice(&[0xAA; 8]);
    let mut cluster = Vec::new();
    cluster.extend_from_slice(&uint_element(ids::TIMECODE, 0));
    cluster.extend_from_slice(&element(ids::SIMPLE_BLOCK, &sb));
    let cluster = element(ids::CLUSTER, &cluster);

    let mut seg = Vec::new();
    seg.extend_from_slice(&info);
    seg.extend_from_slice(&tracks);
    seg.extend_from_slice(&cluster);
    seg
}

fn minimal_webm() -> Vec<u8> {
    let mut out = ebml_header("webm");
    out.extend_from_slice(&element(ids::SEGMENT, &minimal_segment_body()));
    out
}

// ---------------------------------------------------------------------------
// Support-table shape.

#[test]
fn support_table_spot_checks() {
    use WebmSupport::*;
    // Headline Supported elements.
    for id in [
        ids::EBML_HEADER,
        ids::SEGMENT,
        ids::SEEK_HEAD,
        ids::INFO,
        ids::TRACKS,
        ids::CLUSTER,
        ids::SIMPLE_BLOCK,
        ids::CUES,
        ids::COLOUR,
        ids::MASTERING_METADATA,
        ids::CHAPTERS,
        ids::TAGS,
        ids::VOID,
        ids::DISCARD_PADDING,
        ids::CODEC_DELAY,
        ids::SEEK_PRE_ROLL,
        ids::ALPHA_MODE,
        ids::STEREO_MODE,
    ] {
        assert_eq!(webm_element_support(id), Supported, "id 0x{id:X}");
    }
    // Headline Unsupported elements.
    for id in [
        ids::CRC32,
        ids::ATTACHMENTS,
        ids::ATTACHED_FILE,
        ids::FILE_NAME,
        ids::TRACK_OPERATION,
        ids::TRACK_TRANSLATE,
        ids::SEGMENT_UID,
        ids::CHAPTER_TRANSLATE,
        ids::SILENT_TRACKS,
        ids::POSITION,
        ids::CONTENT_COMPRESSION,
        ids::CONTENT_COMP_ALGO,
        ids::CODEC_STATE,
        ids::REFERENCE_PRIORITY,
        ids::CUE_CODEC_STATE,
        ids::CUE_REFERENCE,
        ids::TAG_EDITION_UID,
        ids::TAG_CHAPTER_UID,
        ids::TAG_ATTACHMENT_UID,
        ids::CHAPTER_FLAG_HIDDEN,
        ids::CHAPTER_FLAG_ENABLED,
        ids::CHAP_PROCESS,
        ids::ENCRYPTED_BLOCK,
        ids::ATTACHMENT_LINK,
        ids::DEFAULT_DECODED_FIELD_DURATION,
        ids::TRACK_TIMESTAMP_SCALE,
        ids::MIN_CACHE,
        ids::MAX_BLOCK_ADDITION_ID,
    ] {
        assert_eq!(webm_element_support(id), Unsupported, "id 0x{id:X}");
    }
    // Encryption is in-profile even though compression is not.
    for id in [
        ids::CONTENT_ENCODINGS,
        ids::CONTENT_ENCODING,
        ids::CONTENT_ENCRYPTION,
        ids::CONTENT_ENC_ALGO,
        ids::CONTENT_ENC_KEY_ID,
        ids::CONTENT_ENC_AES_SETTINGS,
        ids::AES_SETTINGS_CIPHER_MODE,
    ] {
        assert_eq!(webm_element_support(id), Supported, "id 0x{id:X}");
    }
    // The four Deprecated rows.
    for id in [
        ids::BLOCK_VIRTUAL,
        ids::TIME_SLICE,
        ids::LACE_NUMBER,
        ids::FRAME_RATE,
    ] {
        assert_eq!(webm_element_support(id), Deprecated, "id 0x{id:X}");
    }
    // Elements newer than the guidelines table are Unlisted.
    for id in [
        ids::PROJECTION,
        ids::PROJECTION_TYPE,
        ids::LANGUAGE_BCP47,
        ids::BLOCK_ADDITION_MAPPING,
        ids::FLAG_HEARING_IMPAIRED,
        ids::OLD_STEREO_MODE,
    ] {
        assert_eq!(webm_element_support(id), Unlisted, "id 0x{id:X}");
    }
}

// ---------------------------------------------------------------------------
// Scanner: conformant + off-profile documents.

#[test]
fn minimal_webm_document_is_conformant() {
    let bytes = minimal_webm();
    let report = scan(&mut Cursor::new(&bytes)).expect("scan");
    assert_eq!(report.doc_type.as_deref(), Some("webm"));
    assert!(report.doc_type_is_webm());
    assert!(
        report.is_conformant(),
        "unexpected findings: {:?} (stopped: {:?})",
        report.findings,
        report.scan_stopped_at
    );
    assert_eq!(report.unsupported, 0);
    assert_eq!(report.deprecated, 0);
    assert_eq!(report.unlisted, 0);
    assert!(report.findings.is_empty());
    assert!(report.scan_stopped_at.is_none());
    // EBML(8) + Segment + Info(2) + Tracks(2 + 5 entry children + Audio(2))
    // + Cluster(2 children) + the masters themselves.
    assert!(report.elements_scanned >= 20, "{}", report.elements_scanned);
    assert_eq!(report.supported, report.elements_scanned);
}

#[test]
fn matroska_doc_type_alone_fails_conformance() {
    let mut bytes = ebml_header("matroska");
    bytes.extend_from_slice(&element(ids::SEGMENT, &minimal_segment_body()));
    let report = scan(&mut Cursor::new(&bytes)).expect("scan");
    assert_eq!(report.doc_type.as_deref(), Some("matroska"));
    assert!(!report.doc_type_is_webm());
    assert!(!report.is_conformant());
    // Every element is still individually in-profile.
    assert_eq!(report.unsupported, 0);
}

#[test]
fn off_profile_elements_are_flagged_with_offsets() {
    // A webm-DocType document carrying an Attachments master and a
    // Cluster Position hint — both listed Unsupported.
    let mut seg_body = minimal_segment_body();
    let mut attached = Vec::new();
    attached.extend_from_slice(&string_element(ids::FILE_NAME, "f.bin"));
    attached.extend_from_slice(&string_element(
        ids::FILE_MIME_TYPE,
        "application/octet-stream",
    ));
    attached.extend_from_slice(&element(ids::FILE_DATA, &[1, 2, 3]));
    attached.extend_from_slice(&uint_element(ids::FILE_UID, 1));
    let attachments = element(ids::ATTACHMENTS, &element(ids::ATTACHED_FILE, &attached));
    seg_body.extend_from_slice(&attachments);

    let mut bytes = ebml_header("webm");
    bytes.extend_from_slice(&element(ids::SEGMENT, &seg_body));
    let report = scan(&mut Cursor::new(&bytes)).expect("scan");
    assert!(report.doc_type_is_webm());
    assert!(!report.is_conformant());
    // Attachments + AttachedFile + FileName + FileMimeType + FileData +
    // FileUID = 6 Unsupported occurrences.
    assert_eq!(report.unsupported, 6, "findings: {:?}", report.findings);
    let flagged: Vec<u32> = report.findings.iter().map(|f| f.id).collect();
    assert!(flagged.contains(&ids::ATTACHMENTS));
    assert!(flagged.contains(&ids::ATTACHED_FILE));
    assert!(flagged.contains(&ids::FILE_NAME));
    // Findings are in document order with real offsets: each finding's
    // offset must point at the first ID byte of that element.
    for f in &report.findings {
        let id_bytes = write_element_id(f.id);
        let at = f.offset as usize;
        assert_eq!(
            &bytes[at..at + id_bytes.len()],
            &id_bytes[..],
            "finding offset mismatch for id 0x{:X}",
            f.id
        );
    }
}

#[test]
fn matroska_muxer_output_is_not_webm_conformant() {
    // The in-crate Matroska muxer writes CRC-32 children on Top-Level
    // masters — in-profile for Matroska (RFC 9559 §6.2 SHOULD), listed
    // Unsupported by the WebM guidelines.
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p.sample_format = Some(oxideav_core::SampleFormat::S16);
    let streams = vec![StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }];
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mkv-r416-webmscan-{}.mkv",
        std::process::id()
    ));
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = oxideav_mkv::mux::MkvMuxer::new_matroska(ws, &streams).expect("muxer");
        mx.write_header().expect("header");
        let mut pkt = Packet::new(0, TimeBase::new(1, 1000), vec![0xAA; 32]);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mx.write_packet(&pkt).expect("packet");
        mx.write_trailer().expect("trailer");
    }
    let bytes = std::fs::read(&tmp).expect("read back");
    let _ = std::fs::remove_file(&tmp);

    let report = scan(&mut Cursor::new(&bytes)).expect("scan");
    assert_eq!(report.doc_type.as_deref(), Some("matroska"));
    assert!(!report.is_conformant());
    assert!(
        report.scan_stopped_at.is_none(),
        "muxer output must walk cleanly"
    );
    // Every flagged occurrence in the plain muxer output is a CRC-32.
    assert!(report.unsupported >= 3, "Info/Tracks/Cues CRCs at minimum");
    for f in &report.findings {
        assert_eq!(f.id, ids::CRC32, "unexpected off-profile id 0x{:X}", f.id);
    }
}

// ---------------------------------------------------------------------------
// Scanner: damage + hostile shapes.

#[test]
fn truncated_document_reports_stop_offset() {
    let bytes = minimal_webm();
    // Cut mid-Tracks: past the EBML header, inside the Segment.
    let cut = bytes.len() * 2 / 3;
    let report = scan(&mut Cursor::new(&bytes[..cut])).expect("scan");
    assert_eq!(report.doc_type.as_deref(), Some("webm"));
    assert!(report.scan_stopped_at.is_some());
    assert!(!report.is_conformant());
}

#[test]
fn every_truncation_point_scans_without_panicking() {
    let bytes = minimal_webm();
    for cut in 0..bytes.len() {
        let report = scan(&mut Cursor::new(&bytes[..cut])).expect("scan");
        // Counters never lie about what was walked.
        assert!(
            report.supported + report.unsupported + report.deprecated + report.unlisted
                == report.elements_scanned
        );
    }
}

#[test]
fn unknown_size_on_non_segment_cluster_master_stops_scan() {
    // An Info master with the unknown-size VINT — RFC 9559 §6.2 allows
    // unknown size only on Segment and Cluster.
    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&write_element_id(ids::INFO));
    seg_body.push(0xFF); // unknown-size VINT
    seg_body.extend_from_slice(&uint_element(ids::TIMECODE_SCALE, 1_000_000));
    let mut bytes = ebml_header("webm");
    let seg_start = bytes.len() as u64;
    bytes.extend_from_slice(&element(ids::SEGMENT, &seg_body));
    let report = scan(&mut Cursor::new(&bytes)).expect("scan");
    // The Info element itself is where the walk stops.
    let expect_stop = seg_start + (element(ids::SEGMENT, &seg_body).len() - seg_body.len()) as u64;
    assert_eq!(report.scan_stopped_at, Some(expect_stop));
    assert!(!report.is_conformant());
}

#[test]
fn unknown_size_segment_and_cluster_walk_to_eof() {
    // Streaming layout: unknown-size Segment, unknown-size Cluster, a
    // trailing Cues sibling terminating the Cluster.
    let mut bytes = ebml_header("webm");
    bytes.extend_from_slice(&write_element_id(ids::SEGMENT));
    bytes.push(0xFF);
    let mut info = Vec::new();
    info.extend_from_slice(&uint_element(ids::TIMECODE_SCALE, 1_000_000));
    bytes.extend_from_slice(&element(ids::INFO, &info));
    bytes.extend_from_slice(&write_element_id(ids::CLUSTER));
    bytes.push(0xFF);
    bytes.extend_from_slice(&uint_element(ids::TIMECODE, 0));
    let mut sb = vec![0x81, 0x00, 0x00, 0x80];
    sb.extend_from_slice(&[0xAA; 4]);
    bytes.extend_from_slice(&element(ids::SIMPLE_BLOCK, &sb));
    // Sibling Cues ends the unknown-size Cluster.
    let cues = element(
        ids::CUES,
        &element(ids::CUE_POINT, &{
            let mut cp = uint_element(ids::CUE_TIME, 0);
            let mut ctp = uint_element(ids::CUE_TRACK, 1);
            ctp.extend_from_slice(&uint_element(ids::CUE_CLUSTER_POSITION, 0));
            cp.extend_from_slice(&element(ids::CUE_TRACK_POSITIONS, &ctp));
            cp
        }),
    );
    bytes.extend_from_slice(&cues);
    let report = scan(&mut Cursor::new(&bytes)).expect("scan");
    assert!(
        report.is_conformant(),
        "findings: {:?} stopped: {:?}",
        report.findings,
        report.scan_stopped_at
    );
    // The Cues children were all reached (CueTime/CueTrack/...).
    assert!(report.elements_scanned >= 18);
}

#[test]
fn deeply_nested_masters_scan_without_stack_overflow() {
    // 4000 nested SimpleTag masters. Beyond MAX_DEPTH the scanner skips
    // bodies instead of descending, so this must complete.
    let mut innermost = string_element(ids::TAG_NAME, "X");
    for _ in 0..4000 {
        innermost = element(ids::SIMPLE_TAG, &innermost);
    }
    let mut tag = element(ids::TARGETS, &[]);
    tag.extend_from_slice(&innermost);
    let tags = element(ids::TAGS, &element(ids::TAG, &tag));
    let mut seg_body = minimal_segment_body();
    seg_body.extend_from_slice(&tags);
    let mut bytes = ebml_header("webm");
    bytes.extend_from_slice(&element(ids::SEGMENT, &seg_body));
    let report = scan(&mut Cursor::new(&bytes)).expect("scan");
    // No panic, no stop: the deep chain is structurally clean.
    assert!(report.scan_stopped_at.is_none());
    // Only the masters above the depth cap were descended.
    assert!(report.elements_scanned < 200, "{}", report.elements_scanned);
}

#[test]
fn findings_flood_is_capped_but_counted() {
    // 5000 ReferencePriority elements (Unsupported) inside a BlockGroup.
    let mut bg = Vec::new();
    for _ in 0..5000 {
        bg.extend_from_slice(&uint_element(ids::REFERENCE_PRIORITY, 1));
    }
    let mut cluster = uint_element(ids::TIMECODE, 0);
    cluster.extend_from_slice(&element(ids::BLOCK_GROUP, &bg));
    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&element(ids::CLUSTER, &cluster));
    let mut bytes = ebml_header("webm");
    bytes.extend_from_slice(&element(ids::SEGMENT, &seg_body));
    let report = scan(&mut Cursor::new(&bytes)).expect("scan");
    assert_eq!(report.unsupported, 5000);
    assert_eq!(report.findings.len(), 4096);
    assert!(report.findings_truncated);
    assert!(!report.is_conformant());
}

#[test]
fn arbitrary_bytes_never_panic() {
    // Deterministic splitmix64-driven byte soup, same PRNG style as the
    // EBML walker property tests.
    let mut state = 0x9E3779B97F4A7C15u64;
    let mut next = move || {
        state = state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    };
    for case in 0..200 {
        let len = (next() % 512) as usize;
        let mut buf = Vec::with_capacity(len);
        for _ in 0..len {
            buf.push((next() & 0xFF) as u8);
        }
        let _ = scan(&mut Cursor::new(&buf));
        let _ = case;
    }
}

#[test]
fn fuzz_corpus_seeds_replay_through_scan() {
    // Every seed in the fuzz corpus must scan without panicking, with
    // counters that sum to elements_scanned — the same invariant the
    // fuzz harness asserts, pinned as a deterministic test.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/demux");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("corpus dir") {
        let path = entry.expect("dir entry").path();
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path).expect("read seed");
        let report = scan(&mut Cursor::new(&bytes)).expect("scan never errors");
        assert_eq!(
            report.supported + report.unsupported + report.deprecated + report.unlisted,
            report.elements_scanned,
            "counter sum mismatch on {}",
            path.display()
        );
        seen += 1;
    }
    assert!(seen >= 8, "expected the seed corpus, found {seen} files");
}
