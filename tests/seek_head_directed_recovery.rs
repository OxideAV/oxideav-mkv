//! SeekHead-directed recovery of post-Cluster Top-Level masters
//! (RFC 9559 §6.3).
//!
//! The common single-pass-mux layout stores `Tags` / `Chapters` /
//! `Attachments` / `Cues` *after* the Cluster run, where the demuxer's
//! pre-Cluster walk never reaches them. The open path now follows the
//! SeekHead's `SeekPosition` entries to those masters:
//!
//! 1. Late Chapters / Attachments / Tags / Cues all surface at open time
//!    through their typed accessors, the flat metadata view, and the
//!    seek index — including on-demand `attachment_data` reads.
//! 2. Trust-but-verify: a `SeekPosition` whose target does not carry the
//!    promised element ID, or that points outside the Segment, is
//!    ignored wholesale (no partial state, no error).
//! 3. Masters already parsed by the pre-Cluster walk are not chased (no
//!    duplicates from a stale or duplicated SeekHead entry).
//!
//! These tests hand-build files with the production EBML helpers — no
//! third-party Matroska code is consulted.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

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

fn ebml_header() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&uint_element(ids::EBML_VERSION, 1));
    body.extend_from_slice(&uint_element(ids::EBML_READ_VERSION, 1));
    body.extend_from_slice(&uint_element(ids::EBML_MAX_ID_LENGTH, 4));
    body.extend_from_slice(&uint_element(ids::EBML_MAX_SIZE_LENGTH, 8));
    body.extend_from_slice(&string_element(ids::EBML_DOC_TYPE, "matroska"));
    body.extend_from_slice(&uint_element(ids::EBML_DOC_TYPE_VERSION, 4));
    body.extend_from_slice(&uint_element(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    element(ids::EBML_HEADER, &body)
}

fn info_element() -> Vec<u8> {
    element(ids::INFO, &uint_element(ids::TIMECODE_SCALE, 1_000_000))
}

fn tracks_element() -> Vec<u8> {
    let mut entry = Vec::new();
    entry.extend_from_slice(&uint_element(ids::TRACK_NUMBER, 1));
    entry.extend_from_slice(&uint_element(ids::TRACK_UID, 7));
    entry.extend_from_slice(&uint_element(ids::TRACK_TYPE, 2)); // audio
    entry.extend_from_slice(&string_element(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio = Vec::new();
    audio.extend_from_slice(&element(ids::SAMPLING_FREQUENCY, &48000.0f32.to_be_bytes()));
    audio.extend_from_slice(&uint_element(ids::CHANNELS, 1));
    audio.extend_from_slice(&uint_element(ids::BIT_DEPTH, 16));
    entry.extend_from_slice(&element(ids::AUDIO, &audio));
    element(ids::TRACKS, &element(ids::TRACK_ENTRY, &entry))
}

fn cluster_element(timecode_ms: u64) -> Vec<u8> {
    let mut sb = vec![0x81, 0x00, 0x00, 0x80];
    sb.extend_from_slice(&[0xAA; 8]);
    let mut body = uint_element(ids::TIMECODE, timecode_ms);
    body.extend_from_slice(&element(ids::SIMPLE_BLOCK, &sb));
    element(ids::CLUSTER, &body)
}

fn chapters_element() -> Vec<u8> {
    let mut atom = Vec::new();
    atom.extend_from_slice(&uint_element(ids::CHAPTER_UID, 41));
    atom.extend_from_slice(&uint_element(ids::CHAPTER_TIME_START, 0));
    atom.extend_from_slice(&uint_element(ids::CHAPTER_TIME_END, 2_000_000_000));
    let mut disp = string_element(ids::CHAP_STRING, "Late chapter");
    disp.extend_from_slice(&string_element(ids::CHAP_LANGUAGE, "eng"));
    atom.extend_from_slice(&element(ids::CHAPTER_DISPLAY, &disp));
    let edition = element(ids::CHAPTER_ATOM, &atom);
    element(ids::CHAPTERS, &element(ids::EDITION_ENTRY, &edition))
}

fn attachments_element(payload: &[u8]) -> Vec<u8> {
    let mut file = Vec::new();
    file.extend_from_slice(&string_element(ids::FILE_NAME, "late.bin"));
    file.extend_from_slice(&string_element(
        ids::FILE_MIME_TYPE,
        "application/octet-stream",
    ));
    file.extend_from_slice(&element(ids::FILE_DATA, payload));
    file.extend_from_slice(&uint_element(ids::FILE_UID, 99));
    element(ids::ATTACHMENTS, &element(ids::ATTACHED_FILE, &file))
}

fn tags_element() -> Vec<u8> {
    // One track-scoped tag (TagTrackUID = 7, matching tracks_element) and
    // one global tag.
    let mut tag1 = element(ids::TARGETS, &uint_element(ids::TAG_TRACK_UID, 7));
    let mut st = string_element(ids::TAG_NAME, "ARTIST");
    st.extend_from_slice(&string_element(ids::TAG_STRING, "Late Artist"));
    tag1.extend_from_slice(&element(ids::SIMPLE_TAG, &st));
    let mut tag2 = element(ids::TARGETS, &[]);
    let mut st2 = string_element(ids::TAG_NAME, "TITLE");
    st2.extend_from_slice(&string_element(ids::TAG_STRING, "Late Title"));
    tag2.extend_from_slice(&element(ids::SIMPLE_TAG, &st2));
    let mut body = element(ids::TAG, &tag1);
    body.extend_from_slice(&element(ids::TAG, &tag2));
    element(ids::TAGS, &body)
}

fn cues_element(cluster_pos: u64, time_ms: u64) -> Vec<u8> {
    let mut ctp = uint_element(ids::CUE_TRACK, 1);
    ctp.extend_from_slice(&uint_element(ids::CUE_CLUSTER_POSITION, cluster_pos));
    let mut cp = uint_element(ids::CUE_TIME, time_ms);
    cp.extend_from_slice(&element(ids::CUE_TRACK_POSITIONS, &ctp));
    element(ids::CUES, &element(ids::CUE_POINT, &cp))
}

/// One fixed-width `Seek` entry: 4-byte SeekID payload + 8-byte
/// SeekPosition payload, so entry size is position-independent.
fn seek_entry(target_id: u32, position: u64) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_element_id(ids::SEEK_ID));
    body.extend_from_slice(&write_vint(4, 0));
    body.extend_from_slice(&target_id.to_be_bytes());
    body.extend_from_slice(&write_element_id(ids::SEEK_POSITION));
    body.extend_from_slice(&write_vint(8, 0));
    body.extend_from_slice(&position.to_be_bytes());
    element(ids::SEEK, &body)
}

fn seek_head(entries: &[(u32, u64)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, pos) in entries {
        body.extend_from_slice(&seek_entry(*id, *pos));
    }
    element(ids::SEEK_HEAD, &body)
}

/// Assemble: EBML header + known-size Segment whose body is
/// `SeekHead + Info + Tracks + Cluster + <late masters>`, with the
/// SeekHead's positions patched to the real Segment Positions.
fn build_file(late: &[Vec<u8>], seek_ids: &[u32], bogus_positions: Option<u64>) -> Vec<u8> {
    // First pass with zero positions fixes every size.
    let zero_entries: Vec<(u32, u64)> = seek_ids.iter().map(|id| (*id, 0)).collect();
    let sh_len = seek_head(&zero_entries).len() as u64;
    let pre = [info_element(), tracks_element(), cluster_element(0)];
    let pre_len: u64 = pre.iter().map(|e| e.len() as u64).sum();
    // Segment positions of each late master, in order.
    let mut entries = Vec::new();
    let mut cursor = sh_len + pre_len;
    for (i, master) in late.iter().enumerate() {
        let pos = bogus_positions.unwrap_or(cursor);
        entries.push((seek_ids[i], pos));
        cursor += master.len() as u64;
    }
    let sh = seek_head(&entries);
    assert_eq!(sh.len() as u64, sh_len, "fixed-width SeekHead sizing");
    let mut seg_body = sh;
    for e in &pre {
        seg_body.extend_from_slice(e);
    }
    for m in late {
        seg_body.extend_from_slice(m);
    }
    let mut out = ebml_header();
    out.extend_from_slice(&element(ids::SEGMENT, &seg_body));
    out
}

fn open_typed(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

// ---------------------------------------------------------------------------

#[test]
fn late_masters_surface_via_seek_head() {
    let payload = vec![0x5A; 300];
    // Cluster sits right after SeekHead+Info+Tracks; compute its Segment
    // Position for the Cues entry the same way build_file does.
    let sh_len = seek_head(&[
        (ids::CHAPTERS, 0),
        (ids::ATTACHMENTS, 0),
        (ids::TAGS, 0),
        (ids::CUES, 0),
    ])
    .len() as u64;
    let cluster_pos = sh_len + (info_element().len() + tracks_element().len()) as u64;
    let late = vec![
        chapters_element(),
        attachments_element(&payload),
        tags_element(),
        cues_element(cluster_pos, 0),
    ];
    let bytes = build_file(
        &late,
        &[ids::CHAPTERS, ids::ATTACHMENTS, ids::TAGS, ids::CUES],
        None,
    );
    let mut dmx = open_typed(bytes);

    // Chapters.
    assert_eq!(dmx.chapters().len(), 1, "late Chapters must surface");
    let ch = &dmx.chapters()[0].chapters[0];
    assert_eq!(ch.uid, Some(41));
    assert_eq!(ch.displays[0].string, "Late chapter");

    // Attachments, including the on-demand payload read.
    assert_eq!(dmx.attachments().len(), 1, "late Attachments must surface");
    assert_eq!(dmx.attachments()[0].filename, "late.bin");
    assert_eq!(dmx.attachments()[0].uid, 99);
    let data = dmx.attachment_data(1).expect("attachment data");
    assert_eq!(data, payload);

    // Tags: the track-scoped tag resolves against TrackUID 7 → stream 0,
    // the global tag lands on the bare key.
    assert_eq!(dmx.tags().len(), 2, "late Tags must surface");
    let md = dmx.metadata();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    // Track keys are zero-indexed by stream index.
    // Flat keys are lowercased tag names.
    assert_eq!(get("tag:track:0:artist").as_deref(), Some("Late Artist"));
    assert_eq!(get("title").as_deref(), Some("Late Title"));
    assert_eq!(get("chapter:1:title").as_deref(), Some("Late chapter"));
    assert_eq!(get("attachment:1:filename").as_deref(), Some("late.bin"));

    // The Cues index came from the SeekHead follow (not the linear
    // fallback scan — both would find it here, but the typed tree must
    // be populated exactly once).
    assert_eq!(dmx.cue_points().len(), 1);

    // The stream still demuxes.
    let pkt = Demuxer::next_packet(&mut dmx).expect("packet");
    assert_eq!(pkt.data.len(), 8);
}

#[test]
fn bogus_seek_positions_are_ignored() {
    // Entries whose SeekPosition points at the Cluster body (wrong ID at
    // target) — the chase must ignore them without erroring or surfacing
    // partial state.
    let late = vec![chapters_element(), attachments_element(&[1, 2, 3])];
    let bytes = build_file(
        &late,
        &[ids::CHAPTERS, ids::ATTACHMENTS],
        Some(3), // points into the SeekHead itself: wrong ID at target
    );
    let mut dmx = open_typed(bytes);
    assert!(dmx.chapters().is_empty(), "bogus target must be ignored");
    assert!(dmx.attachments().is_empty(), "bogus target must be ignored");
    assert!(Demuxer::next_packet(&mut dmx).is_ok());
}

#[test]
fn out_of_segment_seek_positions_are_ignored() {
    let late = vec![chapters_element()];
    let bytes = build_file(&late, &[ids::CHAPTERS], Some(1 << 40));
    let mut dmx = open_typed(bytes);
    assert!(dmx.chapters().is_empty());
    assert!(Demuxer::next_packet(&mut dmx).is_ok());
}

#[test]
fn pre_cluster_masters_are_not_chased_again() {
    // Chapters BEFORE the Cluster (parsed in-line) plus a SeekHead entry
    // pointing at a second, different Chapters element after the Cluster:
    // the chase must skip it (first element wins, no duplicates).
    let zero = [(ids::CHAPTERS, 0u64)];
    let sh_len = seek_head(&zero).len() as u64;
    let pre = [
        info_element(),
        tracks_element(),
        chapters_element(),
        cluster_element(0),
    ];
    let pre_len: u64 = pre.iter().map(|e| e.len() as u64).sum();
    let late_chapters = chapters_element();
    let sh = seek_head(&[(ids::CHAPTERS, sh_len + pre_len)]);
    assert_eq!(sh.len() as u64, sh_len);
    let mut seg_body = sh;
    for e in &pre {
        seg_body.extend_from_slice(e);
    }
    seg_body.extend_from_slice(&late_chapters);
    let mut bytes = ebml_header();
    bytes.extend_from_slice(&element(ids::SEGMENT, &seg_body));

    let dmx = open_typed(bytes);
    assert_eq!(
        dmx.chapters().len(),
        1,
        "the pre-Cluster Chapters wins; the SeekHead target is not merged on top"
    );
}
