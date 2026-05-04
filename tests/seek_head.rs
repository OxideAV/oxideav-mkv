//! Verifies the muxer emits a SeekHead at the top of the Segment with
//! correct SeekPosition values for Info, Tracks, and Cues.
//!
//! Players that prefer up-front index lookup (mpv, Chromium) walk the
//! SeekHead first and jump directly to Cues / Tracks without scanning the
//! whole file. The on-disk layout written by the muxer is:
//!
//! ```text
//! Segment
//!   SeekHead
//!     Seek { id: Info,   position: <patched at write_header>   }
//!     Seek { id: Tracks, position: <patched at write_header>   }
//!     Seek { id: Cues,   position: <patched at write_trailer>  }
//!   Info
//!   Tracks
//!   Cluster…
//!   Cues
//! ```
//!
//! These tests parse the file with the production EBML helpers and assert
//! that each Seek entry's SeekPosition (a byte offset relative to the
//! Segment payload start) lands on the matching top-level element.

use std::io::{Cursor, Read, Seek, SeekFrom};

use oxideav_core::{CodecId, CodecParameters, Packet, StreamInfo, TimeBase};
use oxideav_core::{ReadSeek, WriteSeek};
use oxideav_mkv::ebml::{read_element_header, read_uint, VINT_UNKNOWN_SIZE};
use oxideav_mkv::ids;

fn opus_head(channels: u8, sample_rate: u32, pre_skip: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(19);
    out.extend_from_slice(b"OpusHead");
    out.push(1);
    out.push(channels);
    out.extend_from_slice(&pre_skip.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.push(0);
    out
}

fn opus_packet(payload_byte: u8) -> Vec<u8> {
    let mut out = vec![0x80u8];
    out.extend_from_slice(&[payload_byte; 32]);
    out
}

fn vp9_frame(marker: u8, len: usize) -> Vec<u8> {
    let mut v = vec![marker; len];
    v[0] = marker;
    v
}

fn build_streams() -> (StreamInfo, StreamInfo) {
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
    let mut op = CodecParameters::audio(CodecId::new("opus"));
    op.sample_rate = Some(48_000);
    op.channels = Some(2);
    op.extradata = opus_head(2, 48_000, 312);
    let audio = StreamInfo {
        index: 1,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params: op,
    };
    (video, audio)
}

/// One Seek child of a SeekHead, denormalised for assertions.
#[derive(Debug, PartialEq, Eq)]
struct SeekChild {
    seek_id: u32,
    seek_position: u64,
}

/// Parse a SeekHead body and return its Seek entries. Skips Void elements
/// so callers can assert "no Seek for Cues" by absence.
fn parse_seek_head(body: &[u8]) -> Vec<SeekChild> {
    let mut out = Vec::new();
    let mut cur = Cursor::new(body);
    while (cur.position() as usize) < body.len() {
        let e = read_element_header(&mut cur).expect("seek head child header");
        if e.id != ids::SEEK {
            // Skip Voids and any unknowns inside the SeekHead body.
            cur.seek(SeekFrom::Current(e.size as i64))
                .expect("skip non-seek child");
            continue;
        }
        let body_end = cur.position() + e.size;
        let mut seek_id: Option<u32> = None;
        let mut seek_position: Option<u64> = None;
        while cur.position() < body_end {
            let c = read_element_header(&mut cur).expect("seek field header");
            match c.id {
                ids::SEEK_ID => {
                    let raw = read_uint(&mut cur, c.size as usize).expect("SeekID");
                    seek_id = Some(raw as u32);
                }
                ids::SEEK_POSITION => {
                    seek_position = Some(read_uint(&mut cur, c.size as usize).expect("SeekPos"));
                }
                _ => {
                    cur.seek(SeekFrom::Current(c.size as i64))
                        .expect("skip seek field");
                }
            }
        }
        if let (Some(id), Some(pos)) = (seek_id, seek_position) {
            out.push(SeekChild {
                seek_id: id,
                seek_position: pos,
            });
        }
    }
    out
}

/// Walk a muxed file and return:
///   (segment_data_start, top-level offsets keyed by element id, seek_head_body)
///
/// `top_level_offsets[id]` is the byte offset of the element header,
/// **relative to the Segment payload start** — same convention as
/// SeekHead's SeekPosition.
fn parse_segment_layout(
    raw: &[u8],
) -> (
    u64,
    std::collections::HashMap<u32, u64>,
    Vec<u8>,
    Option<u64>,
) {
    let mut cur = Cursor::new(raw);
    let ebml_hdr = read_element_header(&mut cur).expect("EBML header");
    assert_eq!(ebml_hdr.id, ids::EBML_HEADER);
    cur.seek(SeekFrom::Current(ebml_hdr.size as i64)).unwrap();
    let seg = read_element_header(&mut cur).expect("Segment header");
    assert_eq!(seg.id, ids::SEGMENT);
    let segment_data_start = cur.position();
    let segment_data_end = if seg.size == VINT_UNKNOWN_SIZE {
        raw.len() as u64
    } else {
        segment_data_start + seg.size
    };
    let mut offsets = std::collections::HashMap::new();
    let mut seek_head_body = Vec::new();
    let mut first_cluster: Option<u64> = None;
    while cur.position() < segment_data_end {
        let pos = cur.position();
        let e = read_element_header(&mut cur).expect("segment child header");
        let body_start = cur.position();
        let rel = pos - segment_data_start;
        offsets.entry(e.id).or_insert(rel);
        if e.id == ids::SEEK_HEAD {
            let mut buf = vec![0u8; e.size as usize];
            cur.read_exact(&mut buf).expect("read SeekHead body");
            seek_head_body = buf;
            continue;
        }
        if e.id == ids::CLUSTER {
            if first_cluster.is_none() {
                first_cluster = Some(rel);
            }
            // Cluster has unknown size — walk children until a sibling
            // top-level id appears.
            if e.size == VINT_UNKNOWN_SIZE {
                while cur.position() < segment_data_end {
                    let cpos = cur.position();
                    let ce = match read_element_header(&mut cur) {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let is_cluster_child = matches!(
                        ce.id,
                        ids::TIMECODE
                            | ids::SIMPLE_BLOCK
                            | ids::BLOCK_GROUP
                            | ids::BLOCK
                            | ids::BLOCK_DURATION
                            | ids::REFERENCE_BLOCK
                            | ids::VOID
                            | ids::CRC32
                    );
                    if !is_cluster_child {
                        cur.seek(SeekFrom::Start(cpos)).unwrap();
                        break;
                    }
                    if ce.size == VINT_UNKNOWN_SIZE {
                        break;
                    }
                    cur.seek(SeekFrom::Current(ce.size as i64)).unwrap();
                }
                continue;
            }
            cur.seek(SeekFrom::Start(body_start + e.size)).unwrap();
            continue;
        }
        if e.size == VINT_UNKNOWN_SIZE {
            break;
        }
        cur.seek(SeekFrom::Start(body_start + e.size)).unwrap();
    }
    (segment_data_start, offsets, seek_head_body, first_cluster)
}

/// Mux a small VP9+Opus file with a few keyframes so Cues is non-empty,
/// then re-read it and check the SeekHead points at Info, Tracks, Cues.
#[test]
fn seek_head_points_at_info_tracks_cues() {
    let (video, audio) = build_streams();
    let streams = vec![video.clone(), audio.clone()];
    let tmp = std::env::temp_dir().join("oxideav-mkv-seekhead-roundtrip.webm");

    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mkv::mux::open_webm(ws, &streams).unwrap();
        mux.write_header().unwrap();
        // Two keyframes 6 s apart → forces 2 clusters → ensures a Cues
        // element with at least one entry.
        for i in 0..=12 {
            let t_ms = i as i64 * 1000;
            let mut p = Packet::new(0, video.time_base, vp9_frame(i as u8, 48 + i));
            p.pts = Some(t_ms);
            p.duration = Some(1000);
            p.flags.keyframe = t_ms % 6000 == 0;
            mux.write_packet(&p).unwrap();
        }
        let samples_per_100ms: i64 = 48_000 / 10;
        for i in 0..130 {
            let mut p = Packet::new(1, audio.time_base, opus_packet((i as u8).wrapping_add(50)));
            p.pts = Some(i as i64 * samples_per_100ms);
            p.duration = Some(samples_per_100ms);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let raw = std::fs::read(&tmp).unwrap();
    let (segment_data_start, offsets, seek_head_body, _first_cluster) = parse_segment_layout(&raw);

    // Sanity: SeekHead must be the first child of the Segment.
    assert_eq!(
        offsets.get(&ids::SEEK_HEAD),
        Some(&0),
        "SeekHead must be the first Segment child"
    );
    let entries = parse_seek_head(&seek_head_body);
    assert_eq!(
        entries.len(),
        3,
        "expected 3 Seek entries (Info, Tracks, Cues), got {entries:?}"
    );

    let info_off = *offsets.get(&ids::INFO).expect("Info element present");
    let tracks_off = *offsets.get(&ids::TRACKS).expect("Tracks element present");
    let cues_off = *offsets.get(&ids::CUES).expect("Cues element present");

    let by_id: std::collections::HashMap<u32, u64> = entries
        .iter()
        .map(|e| (e.seek_id, e.seek_position))
        .collect();
    assert_eq!(
        by_id.get(&ids::INFO),
        Some(&info_off),
        "Info Seek must point at the Info element"
    );
    assert_eq!(
        by_id.get(&ids::TRACKS),
        Some(&tracks_off),
        "Tracks Seek must point at the Tracks element"
    );
    assert_eq!(
        by_id.get(&ids::CUES),
        Some(&cues_off),
        "Cues Seek must point at the Cues element"
    );

    // Spot-check: SeekPosition values must put the byte at
    // (segment_data_start + seek_position) on the matching element id.
    for entry in &entries {
        let abs = segment_data_start + entry.seek_position;
        let id_bytes = (entry.seek_id).to_be_bytes();
        let n = match entry.seek_id {
            x if x < 0x100 => 1,
            x if x < 0x10000 => 2,
            x if x < 0x1000000 => 3,
            _ => 4,
        };
        let slice = &raw[abs as usize..abs as usize + n];
        assert_eq!(
            slice,
            &id_bytes[4 - n..],
            "SeekPosition for id 0x{:X} must land on the matching element id",
            entry.seek_id
        );
    }

    // The demuxer must still round-trip after the SeekHead patching — the
    // patched bytes need to leave Cues fully readable. A full seek into the
    // second cluster verifies the on-disk Cues + the demuxer agree.
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux");
    let landed = dmx.seek_to(0, 6000).expect("post-seekhead seek");
    assert_eq!(landed, 6000);

    let _ = std::fs::remove_file(&tmp);
}

/// A muxer that never sees `write_packet` produces zero clusters and zero
/// cues. The Cues Seek slot must therefore become a Void element, not a
/// stale placeholder pointing at offset 0 (which is the SeekHead itself).
#[test]
fn seek_head_voids_cues_slot_when_no_cues_emitted() {
    let (_, audio) = build_streams();
    let streams = vec![audio.clone()];
    let tmp = std::env::temp_dir().join("oxideav-mkv-seekhead-empty.webm");

    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mkv::mux::open_webm(ws, &streams).unwrap();
        mux.write_header().unwrap();
        // No write_packet calls — cluster + cues stay empty.
        mux.write_trailer().unwrap();
    }

    let raw = std::fs::read(&tmp).unwrap();
    let (_segment_data_start, offsets, seek_head_body, _first_cluster) = parse_segment_layout(&raw);
    assert_eq!(offsets.get(&ids::SEEK_HEAD), Some(&0));
    let entries = parse_seek_head(&seek_head_body);
    // Info + Tracks survive; Cues was rewritten as a Void and is therefore
    // skipped by `parse_seek_head`.
    let ids_present: std::collections::HashSet<u32> = entries.iter().map(|e| e.seek_id).collect();
    assert!(
        ids_present.contains(&ids::INFO),
        "Info Seek must remain after the Cues void"
    );
    assert!(
        ids_present.contains(&ids::TRACKS),
        "Tracks Seek must remain after the Cues void"
    );
    assert!(
        !ids_present.contains(&ids::CUES),
        "Cues Seek must be voided when no Cues element was emitted (got {entries:?})"
    );

    // The SeekHead body must still be 63 bytes long (3 × 21-byte entries),
    // even with one entry rewritten as a Void — we never resize the
    // SeekHead, so the on-disk size is always identical regardless of
    // whether Cues was emitted.
    assert_eq!(
        seek_head_body.len(),
        3 * 21,
        "SeekHead body size must stay constant whether or not Cues exists"
    );

    let _ = std::fs::remove_file(&tmp);
}

/// Matroska muxer (not WebM) must also emit the SeekHead — the muxer
/// shares a single MkvMuxer impl, but a regression that gates SeekHead
/// behind the WebM flavour would slip past the WebM-only tests above.
#[test]
fn matroska_muxer_also_emits_seek_head() {
    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.sample_rate = Some(44_100);
    params.channels = Some(2);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 44_100),
        duration: None,
        start_time: Some(0),
        params,
    };
    let tmp = std::env::temp_dir().join("oxideav-mkv-seekhead-matroska.mka");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mkv::mux::open(ws, &[stream]).unwrap();
        mux.write_header().unwrap();
        // No packets — exercise the void-cues path on the Matroska muxer.
        mux.write_trailer().unwrap();
    }
    let raw = std::fs::read(&tmp).unwrap();
    let (_seg_start, offsets, body, _first_cluster) = parse_segment_layout(&raw);
    assert_eq!(
        offsets.get(&ids::SEEK_HEAD),
        Some(&0),
        "matroska muxer must also emit a SeekHead"
    );
    let entries = parse_seek_head(&body);
    assert!(
        entries.iter().any(|e| e.seek_id == ids::INFO),
        "matroska SeekHead must contain an Info entry"
    );
    assert!(
        entries.iter().any(|e| e.seek_id == ids::TRACKS),
        "matroska SeekHead must contain a Tracks entry"
    );
    let _ = std::fs::remove_file(&tmp);
}
