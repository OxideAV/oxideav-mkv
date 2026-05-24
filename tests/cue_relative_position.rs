//! Integration tests for `CueRelativePosition` (RFC 9559 §5.1.5.1.2.3).
//!
//! These hand-build minimal Matroska files where a single Cluster
//! contains *multiple* SimpleBlocks for the same track. Each test
//! exercises a different part of the relative-position path:
//!
//! * Demux: a Cues entry whose `CueRelativePosition` points at the
//!   2nd SimpleBlock — `seek_to` must land the reader exactly on
//!   that block, not the first (which is the legacy "scan from
//!   cluster start" behaviour).
//! * Demux: `CueRelativePosition = 0` (first block) — must keep the
//!   pre-existing behaviour byte-for-byte.
//! * Demux: an out-of-range relative position — must degrade
//!   gracefully (fall back to the cluster start, not panic).
//! * Mux:  the muxer's own Cues element carries
//!   `CueRelativePosition` for every entry.
//! * Roundtrip: mux → demux of a multi-cluster file ends up at the
//!   same packet via either seek (Cues + CueRelativePosition both
//!   make it through).
//!
//! RFC 9559 §5.1.5.1.2.3 ("CueRelativePosition Element"): "The
//! relative position inside the Cluster of the referenced
//! SimpleBlock or BlockGroup with 0 being the first possible
//! position for an element inside that Cluster." — i.e. the offset
//! from the byte immediately after the Cluster element's id+size
//! header.

use std::io::Cursor;

use oxideav_core::ReadSeek;

use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

// ---- EBML helpers (duplicate of seek_cues.rs's helpers; each
//      integration test is its own crate so we can't share them). -----

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

/// Force-8-byte uint encoding so the Cues element's overall length
/// stays stable across rebuilds (the offset patching strategy
/// in `build_fixture` depends on this).
fn u64_fixed8(id: u32, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(8, 0));
    out.extend_from_slice(&value.to_be_bytes());
    out
}

/// Build a SimpleBlock element carrying one raw payload byte.
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

/// Build a self-contained MKV with ONE cluster carrying THREE
/// SimpleBlocks (payloads 0xAA, 0xBB, 0xCC at offsets 0, 1, 2 ms)
/// plus a Cues element whose single entry points at the 2nd block.
///
/// Returns (file_bytes, cluster_body_len, simple_block_len_each,
/// timecode_elem_len) so the test can dial the relative position
/// it wants the Cues entry to encode.
fn build_fixture(
    relative_position: u64,
    cue_time: u64,
) -> (
    Vec<u8>,
    /*timecode_len*/ usize,
    /*block_len*/ usize,
) {
    // --- EBML header ---
    let mut ebml_body = Vec::new();
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    ebml_body.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    let ebml_header = elem_master(ids::EBML_HEADER, &ebml_body);

    // --- Info ---
    let mut info_body = Vec::new();
    info_body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info_body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 3.0));
    info_body.extend_from_slice(&elem_str(ids::MUXING_APP, "oxideav-test"));
    info_body.extend_from_slice(&elem_str(ids::WRITING_APP, "oxideav-test"));
    let info = elem_master(ids::INFO, &info_body);

    // --- Tracks: one PCM track, TrackNumber=1 ---
    let mut track_body = Vec::new();
    track_body.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_UID, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    track_body.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio_body.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio_body.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    track_body.extend_from_slice(&elem_master(ids::AUDIO, &audio_body));
    let track_entry = elem_master(ids::TRACK_ENTRY, &track_body);
    let tracks = elem_master(ids::TRACKS, &track_entry);

    // --- Cluster: timecode=0, three blocks. -----
    let timecode_elem = elem_uint(ids::TIMECODE, 0);
    let block_a = simple_block(1, 0, true, 0xAA);
    let block_b = simple_block(1, 1, true, 0xBB);
    let block_c = simple_block(1, 2, true, 0xCC);
    assert_eq!(
        block_a.len(),
        block_b.len(),
        "blocks must be same size for the test math"
    );
    assert_eq!(block_a.len(), block_c.len());
    let block_len = block_a.len();
    let timecode_len = timecode_elem.len();

    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&timecode_elem);
    cluster_body.extend_from_slice(&block_a);
    cluster_body.extend_from_slice(&block_b);
    cluster_body.extend_from_slice(&block_c);
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    // --- Cues: ONE entry whose CueClusterPosition points at the
    //     cluster header and whose CueRelativePosition is the test's
    //     parameter. ---
    let build_cues = |cluster_offset: u64| -> Vec<u8> {
        let mut ctp_body = Vec::new();
        ctp_body.extend_from_slice(&elem_uint(ids::CUE_TRACK, 1));
        ctp_body.extend_from_slice(&u64_fixed8(ids::CUE_CLUSTER_POSITION, cluster_offset));
        ctp_body.extend_from_slice(&u64_fixed8(ids::CUE_RELATIVE_POSITION, relative_position));
        let ctp = elem_master(ids::CUE_TRACK_POSITIONS, &ctp_body);
        let mut cp_body = Vec::new();
        cp_body.extend_from_slice(&elem_uint(ids::CUE_TIME, cue_time));
        cp_body.extend_from_slice(&ctp);
        let cp = elem_master(ids::CUE_POINT, &cp_body);
        elem_master(ids::CUES, &cp)
    };

    // Placeholder cues (correct length, bogus offset) so we can size
    // the segment.
    let cues_bytes = build_cues(0);

    // Cluster offset = position of cluster header relative to segment
    // payload start = info + tracks + cues.
    let cluster_offset_rel = (info.len() + tracks.len() + cues_bytes.len()) as u64;
    let cues_bytes = build_cues(cluster_offset_rel);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&cues_bytes);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header);
    out.extend_from_slice(&segment);
    (out, timecode_len, block_len)
}

/// Cluster body layout: [Timestamp][Block A][Block B][Block C].
///   relative_position == 0                  → first child (Timestamp,
///                                             which `advance()` parses
///                                             before any block)
///   relative_position == timecode_len       → Block A
///   relative_position == timecode_len + B   → Block B
///   relative_position == timecode_len + 2*B → Block C
fn rel_for_block(index: usize, timecode_len: usize, block_len: usize) -> u64 {
    (timecode_len + index * block_len) as u64
}

#[test]
fn cue_relative_position_lands_on_middle_block() {
    let (_, timecode_len, block_len) = build_fixture(0, 0);
    let rel_b = rel_for_block(1, timecode_len, block_len);
    // Rebuild with the right relative position (the helper takes the
    // offset baked into the Cues entry).
    let (bytes, _, _) = build_fixture(rel_b, 1);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    // Seek to the cue at t=1ms. With CueRelativePosition honoured, the
    // first packet must be Block B (payload 0xBB), NOT Block A (0xAA)
    // — the legacy "scan from cluster start" path would return A first.
    let landed = dmx.seek_to(0, 1).expect("seek to t=1");
    assert_eq!(landed, 1, "should land on the cue's CueTime");
    let pkt = dmx.next_packet().expect("packet after seek");
    assert_eq!(
        pkt.data,
        vec![0xBB],
        "CueRelativePosition must place us on Block B, not Block A"
    );
}

#[test]
fn cue_relative_position_lands_on_last_block() {
    let (_, timecode_len, block_len) = build_fixture(0, 0);
    let rel_c = rel_for_block(2, timecode_len, block_len);
    let (bytes, _, _) = build_fixture(rel_c, 2);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    let landed = dmx.seek_to(0, 2).expect("seek to t=2");
    assert_eq!(landed, 2);
    let pkt = dmx.next_packet().expect("packet after seek");
    assert_eq!(
        pkt.data,
        vec![0xCC],
        "CueRelativePosition should reach Block C"
    );
}

#[test]
fn cue_relative_position_zero_keeps_legacy_behaviour() {
    // CueRelativePosition = 0 means "first possible element position",
    // i.e. the Timestamp. `advance()` walks the Timestamp then the
    // first Block — so the first packet returned must still be A.
    let (bytes, _, _) = build_fixture(0, 0);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    let landed = dmx.seek_to(0, 0).expect("seek to t=0");
    assert_eq!(landed, 0);
    let pkt = dmx.next_packet().expect("packet after seek");
    assert_eq!(pkt.data, vec![0xAA]);
}

#[test]
fn cue_relative_position_out_of_range_falls_back_gracefully() {
    // Pass a deliberately bogus relative position (well past the end
    // of the cluster body). The helper must degrade to the legacy
    // "scan from cluster start" path; no panic, no error, the first
    // packet is just Block A.
    let (bytes, _, _) = build_fixture(1_000_000, 0);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    let landed = dmx.seek_to(0, 0).expect("seek to t=0 should still succeed");
    assert_eq!(landed, 0);
    let pkt = dmx.next_packet().expect("packet after seek");
    assert_eq!(pkt.data, vec![0xAA]);
}

// ---- Muxer side: emitted Cues must carry CueRelativePosition. -----

/// Walk the raw MKV bytes looking for the Cues → CuePoint →
/// CueTrackPositions master and return every (CueClusterPosition,
/// CueRelativePosition) pair it finds. Implemented inline rather than
/// going back through the demuxer because we want to assert on the
/// *written* bytes, not the parsed ones.
fn extract_cue_positions(bytes: &[u8]) -> Vec<(u64, Option<u64>)> {
    use oxideav_mkv::ebml::{read_element_header, VINT_UNKNOWN_SIZE};
    use std::io::{Cursor, Seek, SeekFrom};

    let file_len = bytes.len() as u64;
    let mut r = Cursor::new(bytes);
    // Skip EBML header.
    let hdr = read_element_header(&mut r).expect("EBML header");
    r.seek(SeekFrom::Current(hdr.size as i64)).unwrap();
    // Open Segment.
    let seg = read_element_header(&mut r).expect("Segment");
    // The muxer writes Segment with an unknown-size sentinel — fall
    // back to the file end in that case.
    let seg_end = if seg.size == VINT_UNKNOWN_SIZE {
        file_len
    } else {
        r.position() + seg.size
    };
    // Find Cues inside the segment. Walk segment children, treating
    // unknown-size Clusters as "stop on the next sibling top-level
    // element" (the same rule the demuxer uses).
    let cluster_id = ids::CLUSTER;
    let mut cues_body: Option<(u64, u64)> = None;
    while r.position() < seg_end {
        let e = read_element_header(&mut r).expect("seg child");
        if e.id == ids::CUES {
            let start = r.position();
            let end = if e.size == VINT_UNKNOWN_SIZE {
                seg_end
            } else {
                start + e.size
            };
            cues_body = Some((start, end));
            break;
        }
        if e.size == VINT_UNKNOWN_SIZE {
            // Walk children of an unknown-size Cluster until we hit a
            // sibling element id (Cluster, Cues, Tags...).
            assert_eq!(
                e.id, cluster_id,
                "only Cluster can carry unknown-size at the top level"
            );
            loop {
                let pos = r.position();
                if pos >= seg_end {
                    break;
                }
                let child = match read_element_header(&mut r) {
                    Ok(c) => c,
                    Err(_) => break,
                };
                let sibling = matches!(
                    child.id,
                    id if id == ids::CLUSTER
                        || id == ids::CUES
                        || id == ids::TAGS
                        || id == ids::ATTACHMENTS
                        || id == ids::CHAPTERS
                        || id == ids::SEEK_HEAD
                        || id == ids::INFO
                        || id == ids::TRACKS
                );
                if sibling {
                    // Rewind and let the outer loop dispatch this id.
                    r.seek(SeekFrom::Start(pos)).unwrap();
                    break;
                }
                r.seek(SeekFrom::Current(child.size as i64)).unwrap();
            }
            continue;
        }
        r.seek(SeekFrom::Current(e.size as i64)).unwrap();
    }
    let (cues_start, cues_end) = cues_body.expect("Cues element");
    r.seek(SeekFrom::Start(cues_start)).unwrap();

    let mut found = Vec::new();
    while r.position() < cues_end {
        let cp = read_element_header(&mut r).expect("cue point");
        assert_eq!(cp.id, ids::CUE_POINT);
        let cp_end = r.position() + cp.size;
        while r.position() < cp_end {
            let child = read_element_header(&mut r).expect("cue point child");
            if child.id == ids::CUE_TRACK_POSITIONS {
                let ctp_end = r.position() + child.size;
                let mut cluster_offset = 0u64;
                let mut rel: Option<u64> = None;
                while r.position() < ctp_end {
                    let g = read_element_header(&mut r).expect("ctp child");
                    match g.id {
                        ids::CUE_CLUSTER_POSITION => {
                            cluster_offset = read_u64(&mut r, g.size as usize);
                        }
                        ids::CUE_RELATIVE_POSITION => {
                            rel = Some(read_u64(&mut r, g.size as usize));
                        }
                        _ => {
                            r.seek(SeekFrom::Current(g.size as i64)).unwrap();
                        }
                    }
                }
                found.push((cluster_offset, rel));
            } else {
                r.seek(SeekFrom::Current(child.size as i64)).unwrap();
            }
        }
    }
    found
}

fn read_u64<R: std::io::Read>(r: &mut R, n: usize) -> u64 {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf[8 - n..]).unwrap();
    u64::from_be_bytes(buf)
}

/// One stream description for the muxer-side tests — a single mono PCM
/// audio track running at 1 sample per ms so `pts` (in stream units) is
/// trivially "milliseconds".
fn build_pcm_stream() -> oxideav_core::StreamInfo {
    use oxideav_core::{CodecId, CodecParameters, StreamInfo, TimeBase};
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.sample_rate = Some(48_000);
    p.channels = Some(1);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

#[test]
fn muxer_writes_cue_relative_position_field() {
    use oxideav_core::{Packet, WriteSeek};

    let stream = build_pcm_stream();
    let tmp = std::env::temp_dir().join("oxideav-mkv-r122-cue-rel-field.mkv");
    let _ = std::fs::remove_file(&tmp);

    {
        let f = std::fs::File::create(&tmp).expect("create temp file");
        let writer: Box<dyn WriteSeek> = Box::new(f);
        let mut mx =
            oxideav_mkv::mux::open(writer, std::slice::from_ref(&stream)).expect("mux open");
        mx.write_header().expect("hdr");
        for i in 0..3u32 {
            let mut pkt = Packet::new(0, stream.time_base, vec![0xDD, 0xEE, 0xFF, i as u8]);
            pkt.pts = Some(i as i64);
            pkt.duration = Some(1);
            pkt.flags.keyframe = true;
            mx.write_packet(&pkt).expect("write packet");
        }
        mx.write_trailer().expect("trailer");
    }

    let bytes = std::fs::read(&tmp).expect("read back");
    let _ = std::fs::remove_file(&tmp);
    let positions = extract_cue_positions(&bytes);
    assert!(!positions.is_empty(), "muxer must emit at least one cue");
    // For audio, the first packet of the cluster is the indexed one,
    // and it lands right after the cluster's Timestamp element. So
    // `relative_position` must be a small positive integer (the
    // byte-length of the Timestamp element — a uint master with a
    // 1-byte payload is ~3 bytes), never `None`.
    for (cluster_off, rel) in &positions {
        assert!(*cluster_off > 0, "cue must reference a real cluster");
        let rel = rel.expect("muxer must write CueRelativePosition");
        assert!(
            rel <= 16,
            "first block's relative position should be small, got {rel}"
        );
    }
}

#[test]
fn mux_demux_roundtrip_uses_cue_relative_position() {
    use oxideav_core::{Packet, WriteSeek};

    let stream = build_pcm_stream();
    let tmp = std::env::temp_dir().join("oxideav-mkv-r122-cue-rel-roundtrip.mkv");
    let _ = std::fs::remove_file(&tmp);

    {
        let f = std::fs::File::create(&tmp).expect("create temp file");
        let writer: Box<dyn WriteSeek> = Box::new(f);
        let mut mx =
            oxideav_mkv::mux::open(writer, std::slice::from_ref(&stream)).expect("mux open");
        mx.write_header().expect("hdr");
        for i in 0..4u32 {
            let mut pkt = Packet::new(0, stream.time_base, vec![i as u8 + 0xA0]);
            pkt.pts = Some(i as i64);
            pkt.duration = Some(1);
            pkt.flags.keyframe = true;
            mx.write_packet(&pkt).expect("packet");
        }
        mx.write_trailer().expect("trailer");
    }

    let bytes = std::fs::read(&tmp).expect("read back");
    let _ = std::fs::remove_file(&tmp);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    // The muxer emits one cue per cluster for audio; the cue's
    // CueRelativePosition points at the first SimpleBlock in the
    // cluster. After seeking to t=0 the first packet must be the
    // first one we wrote (0xA0).
    let landed = dmx.seek_to(0, 0).expect("seek");
    assert_eq!(landed, 0);
    let pkt = dmx.next_packet().expect("packet after seek");
    assert_eq!(pkt.data, vec![0xA0]);
}
