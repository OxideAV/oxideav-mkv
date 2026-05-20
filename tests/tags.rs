//! Integration tests for the demuxer's `Tags` parsing + target resolution.
//!
//! `Segment\Tags\Tag` carries a `Targets` master that scopes each SimpleTag
//! to one or more tracks / editions / chapters / attachments via UID
//! references (RFC 9559 §5.1.8.1.1.x). The demuxer surfaces resolved tags
//! into the metadata vector with the scope encoded in the key:
//!
//! * Global (all UIDs zero):                          `<name>`
//! * Track UID matched stream index N:                `tag:track:<N>:<name>`
//! * Chapter UID matched chapter index N:             `tag:chapter:<N>:<name>`
//! * Attachment UID matched attachment index N:       `tag:attachment:<N>:<name>`
//! * Edition UID matched edition index N:             `tag:edition:<N>:<name>`
//!
//! Tags whose UID is non-zero but doesn't match any known target MUST be
//! dropped (RFC 9559 §5.1.8.1.1.3..§5.1.8.1.1.6 use "MUST match" phrasing
//! for non-zero UIDs). Names are lower-cased on emit.

use std::io::Cursor;

use oxideav_core::ReadSeek;
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

fn simple_tag(name: &str, value: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_str(ids::TAG_NAME, name));
    body.extend_from_slice(&elem_str(ids::TAG_LANGUAGE, "und"));
    body.extend_from_slice(&elem_str(ids::TAG_STRING, value));
    elem_master(ids::SIMPLE_TAG, &body)
}

/// Build a `Tag` element with the given Targets UIDs and SimpleTag pairs.
fn tag_with(
    track_uid: u64,
    edition_uid: u64,
    chapter_uid: u64,
    attachment_uid: u64,
    target_type_value: Option<u64>,
    target_type: Option<&str>,
    simple_tags: &[(&str, &str)],
) -> Vec<u8> {
    let mut targets = Vec::new();
    if let Some(v) = target_type_value {
        targets.extend_from_slice(&elem_uint(ids::TARGET_TYPE_VALUE, v));
    }
    if let Some(s) = target_type {
        targets.extend_from_slice(&elem_str(ids::TARGET_TYPE, s));
    }
    if track_uid != 0 {
        targets.extend_from_slice(&elem_uint(ids::TAG_TRACK_UID, track_uid));
    }
    if edition_uid != 0 {
        targets.extend_from_slice(&elem_uint(ids::TAG_EDITION_UID, edition_uid));
    }
    if chapter_uid != 0 {
        targets.extend_from_slice(&elem_uint(ids::TAG_CHAPTER_UID, chapter_uid));
    }
    if attachment_uid != 0 {
        targets.extend_from_slice(&elem_uint(ids::TAG_ATTACHMENT_UID, attachment_uid));
    }
    let targets_master = elem_master(ids::TARGETS, &targets);

    let mut tag_body = Vec::new();
    tag_body.extend_from_slice(&targets_master);
    for (n, v) in simple_tags {
        tag_body.extend_from_slice(&simple_tag(n, v));
    }
    elem_master(ids::TAG, &tag_body)
}

/// Build a self-contained MKV with two tracks, one edition (with one
/// chapter), one attachment, and a `Tags` block exercising every scope.
fn build_mkv_with_tags() -> Vec<u8> {
    // EBML header.
    let mut ebml_body = Vec::new();
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    ebml_body.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    let ebml_header = elem_master(ids::EBML_HEADER, &ebml_body);

    // Info.
    let mut info_body = Vec::new();
    info_body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info_body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 1000.0));
    let info = elem_master(ids::INFO, &info_body);

    // Two PCM tracks so we can scope a tag to track 2 specifically.
    let track1 = {
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
    };
    let track2 = {
        let mut tb = Vec::new();
        tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 2));
        tb.extend_from_slice(&elem_uint(ids::TRACK_UID, 0xA2));
        tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
        tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
        let mut audio = Vec::new();
        audio.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
        audio.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
        audio.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
        tb.extend_from_slice(&elem_master(ids::AUDIO, &audio));
        elem_master(ids::TRACK_ENTRY, &tb)
    };
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&track1);
    tracks_body.extend_from_slice(&track2);
    let tracks = elem_master(ids::TRACKS, &tracks_body);

    // Chapters: one EditionEntry (UID 0xE1) holding one ChapterAtom (UID 0xC1).
    let chapter_atom = {
        let mut ca = Vec::new();
        ca.extend_from_slice(&elem_uint(ids::CHAPTER_UID, 0xC1));
        ca.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, 0));
        ca.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_END, 500_000_000));
        let mut disp = Vec::new();
        disp.extend_from_slice(&elem_str(ids::CHAP_STRING, "Opening"));
        ca.extend_from_slice(&elem_master(ids::CHAPTER_DISPLAY, &disp));
        elem_master(ids::CHAPTER_ATOM, &ca)
    };
    let edition = {
        let mut eb = Vec::new();
        eb.extend_from_slice(&elem_uint(ids::EDITION_UID, 0xE1));
        eb.extend_from_slice(&chapter_atom);
        elem_master(ids::EDITION_ENTRY, &eb)
    };
    let chapters = elem_master(ids::CHAPTERS, &edition);

    // Attachments: one AttachedFile with UID 0xF1.
    let attachment = {
        let mut af = Vec::new();
        af.extend_from_slice(&elem_str(ids::FILE_NAME, "cover.jpg"));
        af.extend_from_slice(&elem_str(ids::FILE_MIME_TYPE, "image/jpeg"));
        af.extend_from_slice(&elem_uint(ids::FILE_UID, 0xF1));
        // Tiny 3-byte file payload — we won't read it, we just need the
        // header to round-trip through the FILE_DATA size accounting.
        af.extend_from_slice(&write_element_id(ids::FILE_DATA));
        af.extend_from_slice(&write_vint(3, 0));
        af.extend_from_slice(&[0x00, 0x01, 0x02]);
        elem_master(ids::ATTACHED_FILE, &af)
    };
    let attachments = elem_master(ids::ATTACHMENTS, &attachment);

    // Tags block.
    let tags_body = {
        let mut tb = Vec::new();
        // 1. Global TITLE (all UIDs zero).
        tb.extend_from_slice(&tag_with(
            0,
            0,
            0,
            0,
            Some(50),
            Some("ALBUM"),
            &[("TITLE", "My Movie")],
        ));
        // 2. Track-scoped: ARTIST + LANGUAGE on TrackUID 0xA2 (stream idx 1).
        tb.extend_from_slice(&tag_with(
            0xA2,
            0,
            0,
            0,
            Some(30),
            Some("TRACK"),
            &[("ARTIST", "Track Two Performer"), ("LANGUAGE", "eng")],
        ));
        // 3. Chapter-scoped on CHAPTER_UID 0xC1.
        tb.extend_from_slice(&tag_with(
            0,
            0,
            0xC1,
            0,
            Some(30),
            Some("CHAPTER"),
            &[("DESCRIPTION", "Big bang")],
        ));
        // 4. Attachment-scoped on FILE_UID 0xF1.
        tb.extend_from_slice(&tag_with(
            0,
            0,
            0,
            0xF1,
            None,
            None,
            &[("DESCRIPTION", "Front cover art")],
        ));
        // 5. Edition-scoped on EDITION_UID 0xE1.
        tb.extend_from_slice(&tag_with(
            0,
            0xE1,
            0,
            0,
            Some(60),
            Some("EDITION"),
            &[("TITLE", "Director's Cut")],
        ));
        // 6. Unresolved TrackUID — MUST be dropped per RFC 9559 §5.1.8.1.1.3.
        tb.extend_from_slice(&tag_with(
            0xDEAD_BEEF,
            0,
            0,
            0,
            None,
            None,
            &[("ARTIST", "Ghost")],
        ));
        tb
    };
    let tags = elem_master(ids::TAGS, &tags_body);

    // One cluster so the demuxer accepts the file.
    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    // Order: Tags before Tracks / Chapters / Attachments to verify deferred
    // resolution. RFC 9559 doesn't fix an ordering; both arrangements are
    // valid.
    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tags);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&chapters);
    seg_body.extend_from_slice(&attachments);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header);
    out.extend_from_slice(&segment);
    out
}

#[test]
fn tags_resolve_to_track_chapter_attachment_edition_scopes() {
    let bytes = build_mkv_with_tags();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let md: Vec<(String, String)> = dmx.metadata().to_vec();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());

    // Global tag — bare key.
    assert_eq!(
        get("title").as_deref(),
        Some("My Movie"),
        "global TITLE should surface as bare 'title' key"
    );

    // Track-scoped tag — stream index 1 (the second TrackEntry, TrackUID 0xA2).
    assert_eq!(
        get("tag:track:1:artist").as_deref(),
        Some("Track Two Performer"),
        "track-2 ARTIST should be scoped to stream index 1"
    );
    assert_eq!(
        get("tag:track:1:language").as_deref(),
        Some("eng"),
        "track-2 LANGUAGE should also be scoped to stream index 1"
    );
    // The other stream (index 0) MUST NOT see the track-2 tag bleed through.
    assert!(
        get("tag:track:0:artist").is_none(),
        "track-2 ARTIST must not leak onto stream index 0"
    );

    // Chapter-scoped tag.
    assert_eq!(
        get("tag:chapter:1:description").as_deref(),
        Some("Big bang"),
        "ChapterUID 0xC1 should resolve to chapter index 1"
    );

    // Attachment-scoped tag.
    assert_eq!(
        get("tag:attachment:1:description").as_deref(),
        Some("Front cover art"),
        "FileUID 0xF1 should resolve to attachment index 1"
    );

    // Edition-scoped tag.
    assert_eq!(
        get("tag:edition:1:title").as_deref(),
        Some("Director's Cut"),
        "EditionUID 0xE1 should resolve to edition index 1"
    );

    // Unresolved TagTrackUID — MUST be dropped per RFC 9559 §5.1.8.1.1.3.
    let ghost_hits: Vec<&(String, String)> = md.iter().filter(|(_, v)| v == "Ghost").collect();
    assert!(
        ghost_hits.is_empty(),
        "tag with non-zero unresolved TagTrackUID must not surface (found {ghost_hits:?})"
    );
}

/// Tags with only `TargetType` / `TargetTypeValue` (informational per RFC
/// 9559 §5.1.8.1.1.1..2) and all UIDs zero are global tags — the
/// `TargetType` string is purely a display hint and the demuxer treats them
/// the same as a Tag with no Targets master at all.
#[test]
fn target_type_string_does_not_change_scope() {
    let bytes = build_mkv_with_tags();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    // Tag 1 in the fixture has TargetTypeValue=50, TargetType="ALBUM" but
    // every UID is zero — it must surface as a global "title" key, not
    // shadowed by the ALBUM hint.
    let md = dmx.metadata();
    assert!(
        md.iter().any(|(k, v)| k == "title" && v == "My Movie"),
        "TargetType='ALBUM' on a UID-less tag must still emit a global 'title' key (metadata: {md:?})"
    );
    // And the demuxer must not have invented a 'tag:album:*' key from the
    // informational TargetType string.
    assert!(
        !md.iter().any(|(k, _)| k.starts_with("tag:album:")),
        "TargetType string is informational only — must NOT spawn a 'tag:album:*' key"
    );
}
