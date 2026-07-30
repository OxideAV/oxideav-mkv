//! Legacy pre-registry element handling (staged mapping doc
//! `docs/container/matroska/legacy-element-ids.md`).
//!
//! Matroska dropped three element groups from the specification before
//! the IETF work began, so RFC 9559 Table 53 and the IANA registry leave
//! their IDs formally unassigned:
//!
//! * the Signature family — global elements headed by `SignatureSlot`
//!   (`0x1B538667`, a four-octet ID in the same class as the Top-Level
//!   elements) — which a Reader must recognise and *skip*;
//! * `EditionFlagHidden` (`0x45BD`), read onto [`Edition::hidden`];
//! * `ChapterTrack` (`0x8F`) / `ChapterTrackUID` (`0x89` — the element
//!   the pre-2016 schema and the WebM guidelines spell
//!   `ChapterTrackNumber`), read onto `Chapter::track_uids`.
//!
//! The muxer never emits any of them (an unassigned ID could collide
//! with a future registry assignment), so this is read-side coverage:
//! strict and resilient opens both handle the elements, and the
//! resilient resync scanner can re-anchor on a `SignatureSlot`.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_mkv::demux::DamageKind;
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

fn elem_bin(id: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(data.len() as u64, 0));
    out.extend_from_slice(data);
    out
}

fn simple_block(track: u8, tc_offset: i16, keyframe: bool, payload: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(if keyframe { 0x80 } else { 0x00 });
    body.push(payload);
    elem_bin(ids::SIMPLE_BLOCK, &body)
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

fn info() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    elem_master(ids::INFO, &b)
}

fn tracks() -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    t.extend_from_slice(&elem_uint(ids::TRACK_UID, 0x1111));
    t.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    t.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio = Vec::new();
    audio.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio.extend_from_slice(&elem_uint(ids::CHANNELS, 1));
    audio.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    t.extend_from_slice(&elem_master(ids::AUDIO, &audio));
    elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &t))
}

fn cluster() -> Vec<u8> {
    let mut c = Vec::new();
    c.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    c.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    elem_master(ids::CLUSTER, &c)
}

/// A fully populated legacy `SignatureSlot` per the mapping doc's nesting:
/// SignatureSlot > { SignatureAlgo, SignatureHash, SignaturePublicKey,
/// Signature, SignatureElements > SignatureElementList+ > SignedElement+ }.
fn signature_slot() -> Vec<u8> {
    let list = elem_master(
        ids::SIGNATURE_ELEMENT_LIST,
        &elem_bin(ids::SIGNED_ELEMENT, &[0xA3]),
    );
    let elements = elem_master(ids::SIGNATURE_ELEMENTS, &list);
    let mut slot = Vec::new();
    slot.extend_from_slice(&elem_uint(ids::SIGNATURE_ALGO, 1)); // RSA
    slot.extend_from_slice(&elem_uint(ids::SIGNATURE_HASH, 1)); // SHA1-160
    slot.extend_from_slice(&elem_bin(ids::SIGNATURE_PUBLIC_KEY, &[0x01, 0x02]));
    slot.extend_from_slice(&elem_bin(ids::SIGNATURE, &[0xDE, 0xAD, 0xBE, 0xEF]));
    slot.extend_from_slice(&elements);
    elem_master(ids::SIGNATURE_SLOT, &slot)
}

fn wrap(segment_children: &[&[u8]]) -> Vec<u8> {
    let mut seg = Vec::new();
    for c in segment_children {
        seg.extend_from_slice(c);
    }
    let mut out = ebml_header();
    out.extend_from_slice(&elem_master(ids::SEGMENT, &seg));
    out
}

// ---------------------------------------------------------------------------
// Signature family: recognise and skip.

#[test]
fn signature_slot_between_top_level_masters_is_skipped_strict() {
    // SignatureSlot sits between Tracks and the Cluster — the strict open
    // must step over it and demux the stream normally.
    let bytes = wrap(&[&info(), &tracks(), &signature_slot(), &cluster()]);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("strict open");
    assert_eq!(dmx.streams().len(), 1);
    let p = dmx.next_packet().expect("packet");
    assert_eq!(p.data, vec![0xAA]);
    assert!(matches!(dmx.next_packet(), Err(oxideav_core::Error::Eof)));
}

#[test]
fn signature_slot_yields_no_damage_events_resilient() {
    // The legacy element is *known*, not damage: a resilient open must
    // record zero DamageEvents for a file whose only oddity is a
    // SignatureSlot.
    let bytes = wrap(&[&info(), &tracks(), &signature_slot(), &cluster()]);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_mkv::demux::open_resilient_typed(rs, &oxideav_core::NullCodecResolver)
        .expect("resilient open");
    assert!(
        dmx.damage_events().is_empty(),
        "known-legacy SignatureSlot must not be reported as damage: {:?}",
        dmx.damage_events()
    );
    let p = dmx.next_packet().expect("packet");
    assert_eq!(p.data, vec![0xAA]);
}

#[test]
fn resilient_resync_anchors_on_signature_slot() {
    // Garbage bytes followed by a SignatureSlot, then the Cluster. The
    // resync scanner re-anchors on the SignatureSlot's 4-byte ID (it is
    // a recognised legacy element in the Top-Level ID class), so the
    // recovery resumes there instead of scanning past it to the Cluster.
    let garbage = [0x00u8; 32];
    let slot = signature_slot();
    let bytes = wrap(&[&info(), &tracks(), &garbage, &slot, &cluster()]);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.clone()));
    let mut dmx = oxideav_mkv::demux::open_resilient_typed(rs, &oxideav_core::NullCodecResolver)
        .expect("resilient open");
    let ev = *dmx
        .damage_events()
        .iter()
        .find(|e| e.kind() == DamageKind::GarbageData)
        .expect("garbage damage event");
    // The resume offset is the SignatureSlot's ID byte: locate the slot's
    // serialised bytes in the file to compute the expected offset.
    let expected = bytes
        .windows(slot.len())
        .position(|w| w == slot.as_slice())
        .expect("slot bytes present") as u64;
    assert_eq!(
        ev.resumed_at(),
        Some(expected),
        "resync should re-anchor on the SignatureSlot ID"
    );
    // And the stream still demuxes.
    let p = dmx.next_packet().expect("packet");
    assert_eq!(p.data, vec![0xAA]);
}

// ---------------------------------------------------------------------------
// EditionFlagHidden.

#[test]
fn edition_flag_hidden_parses_and_defaults() {
    // Two editions: one explicitly hidden, one silent (legacy default 0).
    let mut ed1 = Vec::new();
    ed1.extend_from_slice(&elem_uint(ids::EDITION_UID, 0xE1));
    ed1.extend_from_slice(&elem_uint(ids::EDITION_FLAG_HIDDEN, 1));
    let mut atom1 = Vec::new();
    atom1.extend_from_slice(&elem_uint(ids::CHAPTER_UID, 0xC1));
    atom1.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, 0));
    ed1.extend_from_slice(&elem_master(ids::CHAPTER_ATOM, &atom1));

    let mut ed2 = Vec::new();
    ed2.extend_from_slice(&elem_uint(ids::EDITION_UID, 0xE2));
    let mut atom2 = Vec::new();
    atom2.extend_from_slice(&elem_uint(ids::CHAPTER_UID, 0xC2));
    atom2.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, 0));
    ed2.extend_from_slice(&elem_master(ids::CHAPTER_ATOM, &atom2));

    let mut chapters = Vec::new();
    chapters.extend_from_slice(&elem_master(ids::EDITION_ENTRY, &ed1));
    chapters.extend_from_slice(&elem_master(ids::EDITION_ENTRY, &ed2));
    let chapters = elem_master(ids::CHAPTERS, &chapters);

    let bytes = wrap(&[&info(), &tracks(), &chapters, &cluster()]);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open");
    let eds = dmx.chapters();
    assert_eq!(eds.len(), 2);
    assert!(eds[0].hidden, "explicit EditionFlagHidden=1");
    assert!(!eds[1].hidden, "absent element materialises the default 0");
}

// ---------------------------------------------------------------------------
// ChapterTrack / ChapterTrackUID.

#[test]
fn chapter_track_uids_parse_in_order_zeros_dropped() {
    let mut ct = Vec::new();
    ct.extend_from_slice(&elem_uint(ids::CHAPTER_TRACK_UID, 0x1111));
    ct.extend_from_slice(&elem_uint(ids::CHAPTER_TRACK_UID, 0)); // spec-illegal, dropped
    ct.extend_from_slice(&elem_uint(ids::CHAPTER_TRACK_UID, 0x2222));
    let mut atom = Vec::new();
    atom.extend_from_slice(&elem_uint(ids::CHAPTER_UID, 0xC1));
    atom.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, 0));
    atom.extend_from_slice(&elem_master(ids::CHAPTER_TRACK, &ct));
    let edition = elem_master(ids::EDITION_ENTRY, &elem_master(ids::CHAPTER_ATOM, &atom));
    let chapters = elem_master(ids::CHAPTERS, &edition);

    let bytes = wrap(&[&info(), &tracks(), &chapters, &cluster()]);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open");
    let eds = dmx.chapters();
    assert_eq!(eds.len(), 1);
    let ch = &eds[0].chapters[0];
    assert_eq!(
        ch.track_uids,
        vec![0x1111, 0x2222],
        "on-disk order, zero dropped"
    );
}

#[test]
fn chapter_without_chapter_track_applies_to_all_tracks() {
    // Absent ChapterTrack master → empty list ("all tracks apply").
    let mut atom = Vec::new();
    atom.extend_from_slice(&elem_uint(ids::CHAPTER_UID, 0xC1));
    atom.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, 0));
    let edition = elem_master(ids::EDITION_ENTRY, &elem_master(ids::CHAPTER_ATOM, &atom));
    let chapters = elem_master(ids::CHAPTERS, &edition);

    let bytes = wrap(&[&info(), &tracks(), &chapters, &cluster()]);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open");
    assert!(dmx.chapters()[0].chapters[0].track_uids.is_empty());
}
