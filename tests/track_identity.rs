//! Integration tests for the demuxer's `TrackIdentity` typed decode
//! (RFC 9559 §5.1.4.1.18 / .19 / .20 / .23 / .4 / .5 / .12 / .24 —
//! `Name`, `Language`, `LanguageBCP47`, `CodecName`, `FlagEnabled`,
//! `FlagDefault`, `FlagLacing`, `AttachmentLink`).
//!
//! These eight `TrackEntry`-level identity / selection elements fold into one
//! `TrackIdentity` record per track. The four strings carry no spec default
//! and stay `Option`; the three boolean flags carry the spec default `1`,
//! materialised on the typed accessor while the `*_explicit` accessors
//! preserve the on-disk presence; `AttachmentLink` is a "not 0" uinteger
//! dropped when an illegal `0` is emitted. `LanguageBCP47` supersedes
//! `Language` per §5.1.4.1.20 — `language()` applies that precedence.
//!
//! These tests hand-build Matroska byte streams from the EBML primitives and
//! walk them with the production demuxer — no third-party Matroska code is
//! consulted.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_mkv::demux::TrackIdentity;
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
    elem_master(ids::SIMPLE_BLOCK, &body)
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
    let mut ib = Vec::new();
    ib.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    elem_master(ids::INFO, &ib)
}

fn one_cluster() -> Vec<u8> {
    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cb.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    elem_master(ids::CLUSTER, &cb)
}

/// Build a subtitle TrackEntry carrying `extra` identity children. Subtitle
/// type keeps the entry minimal (no Audio / Video master needed).
fn track_with_identity(number: u64, uid: u64, extra: &[u8]) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_SUBTITLE));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "S_TEXT/UTF8"));
    tb.extend_from_slice(extra);
    tb
}

fn assemble(tracks_body: &[u8]) -> Vec<u8> {
    let tracks = elem_master(ids::TRACKS, tracks_body);
    let mut seg = Vec::new();
    seg.extend_from_slice(&info());
    seg.extend_from_slice(&tracks);
    seg.extend_from_slice(&one_cluster());
    let segment = elem_master(ids::SEGMENT, &seg);
    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

fn open(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

fn identity_of(extra: &[u8]) -> TrackIdentity {
    let t = track_with_identity(1, 0x77, extra);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));
    dmx.track_identity(0).expect("track 0 present").clone()
}

/// A TrackEntry with none of the identity elements surfaces a record whose
/// string fields are all `None`, whose flags all materialise the §default `1`,
/// and which reports `is_default()`.
#[test]
fn empty_identity_materialises_flag_defaults() {
    let id = identity_of(&[]);
    assert_eq!(id.name(), None);
    assert_eq!(id.codec_name(), None);
    assert_eq!(id.language(), None);
    assert_eq!(id.language_matroska(), None);
    assert_eq!(id.language_bcp47(), None);
    assert!(!id.uses_bcp47());
    assert_eq!(id.attachment_link(), None);
    // §5.1.4.1.4 / .5 / .12 default 1 materialised.
    assert!(id.enabled());
    assert!(id.default());
    assert!(id.lacing_allowed());
    // ...but the on-disk presence is observably absent.
    assert_eq!(id.enabled_explicit(), None);
    assert_eq!(id.default_explicit(), None);
    assert_eq!(id.lacing_allowed_explicit(), None);
    assert!(id.is_default());
}

/// `Name` (§5.1.4.1.18) and `CodecName` (§5.1.4.1.23) surface verbatim as
/// human-readable utf-8 strings.
#[test]
fn name_and_codec_name() {
    let mut extra = elem_str(ids::NAME, "Director's commentary");
    extra.extend_from_slice(&elem_str(ids::CODEC_NAME, "SubRip / SRT"));
    let id = identity_of(&extra);
    assert_eq!(id.name(), Some("Director's commentary"));
    assert_eq!(id.codec_name(), Some("SubRip / SRT"));
    assert!(!id.is_default());
}

/// `Language` (§5.1.4.1.19) surfaces in Matroska form and, absent a
/// `LanguageBCP47`, is the effective `language()`.
#[test]
fn language_matroska_only() {
    let id = identity_of(&elem_str(ids::LANGUAGE, "fre"));
    assert_eq!(id.language_matroska(), Some("fre"));
    assert_eq!(id.language_bcp47(), None);
    assert!(!id.uses_bcp47());
    assert_eq!(id.language(), Some("fre"));
}

/// `LanguageBCP47` (§5.1.4.1.20) supersedes `Language` in the same
/// `TrackEntry`: `language()` returns the BCP-47 value, but both raw forms
/// remain inspectable.
#[test]
fn language_bcp47_supersedes_language() {
    let mut extra = elem_str(ids::LANGUAGE, "fre");
    extra.extend_from_slice(&elem_str(ids::LANGUAGE_BCP47, "fr-CA"));
    let id = identity_of(&extra);
    assert_eq!(id.language_matroska(), Some("fre"));
    assert_eq!(id.language_bcp47(), Some("fr-CA"));
    assert!(id.uses_bcp47());
    // §5.1.4.1.20: "any Language elements ... MUST be ignored".
    assert_eq!(id.language(), Some("fr-CA"));
}

/// A `LanguageBCP47` with no `Language` sibling still drives `language()`.
#[test]
fn language_bcp47_only() {
    let id = identity_of(&elem_str(ids::LANGUAGE_BCP47, "de-DE"));
    assert_eq!(id.language(), Some("de-DE"));
    assert!(id.uses_bcp47());
}

/// All three selection flags explicitly cleared (`0`) round-trip as
/// `Some(false)` on the `_explicit` accessors and `false` on the
/// default-materialising ones — byte-distinct from absence.
#[test]
fn flags_explicit_zero() {
    let mut extra = elem_uint(ids::FLAG_ENABLED, 0);
    extra.extend_from_slice(&elem_uint(ids::FLAG_DEFAULT, 0));
    extra.extend_from_slice(&elem_uint(ids::FLAG_LACING, 0));
    let id = identity_of(&extra);
    assert!(!id.enabled());
    assert!(!id.default());
    assert!(!id.lacing_allowed());
    assert_eq!(id.enabled_explicit(), Some(false));
    assert_eq!(id.default_explicit(), Some(false));
    assert_eq!(id.lacing_allowed_explicit(), Some(false));
    assert!(!id.is_default());
}

/// Explicit `1` flags are distinguishable from absence via the `_explicit`
/// accessors even though the materialised value matches the default.
#[test]
fn flags_explicit_one() {
    let mut extra = elem_uint(ids::FLAG_ENABLED, 1);
    extra.extend_from_slice(&elem_uint(ids::FLAG_DEFAULT, 1));
    extra.extend_from_slice(&elem_uint(ids::FLAG_LACING, 1));
    let id = identity_of(&extra);
    assert!(id.enabled());
    assert!(id.default());
    assert!(id.lacing_allowed());
    assert_eq!(id.enabled_explicit(), Some(true));
    assert_eq!(id.default_explicit(), Some(true));
    assert_eq!(id.lacing_allowed_explicit(), Some(true));
    assert!(!id.is_default());
}

/// `AttachmentLink` (§5.1.4.1.24) surfaces its `FileUID` reference; a
/// spec-illegal `0` (range "not 0") is dropped at parse time.
#[test]
fn attachment_link_present_and_zero_dropped() {
    let id = identity_of(&elem_uint(ids::ATTACHMENT_LINK, 0xCAFE));
    assert_eq!(id.attachment_link(), Some(0xCAFE));
    assert!(!id.is_default());

    let zeroed = identity_of(&elem_uint(ids::ATTACHMENT_LINK, 0));
    assert_eq!(zeroed.attachment_link(), None);
    assert!(zeroed.is_default());
}

/// The effective language also lifts onto the flat `StreamInfo` view, with
/// BCP-47 taking precedence over Matroska-form `Language`.
#[test]
fn effective_language_lifts_to_streaminfo() {
    let mut extra = elem_str(ids::LANGUAGE, "spa");
    extra.extend_from_slice(&elem_str(ids::LANGUAGE_BCP47, "es-419"));
    let t = track_with_identity(1, 0x77, &extra);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));
    let stream = &dmx.streams()[0];
    assert_eq!(stream.params.language.as_deref(), Some("es-419"));
}

/// `track_identity` returns `None` only for an out-of-range stream index;
/// every valid track surfaces a record, and `all_track_identity` is indexed
/// by stream index.
#[test]
fn out_of_range_and_all_slice() {
    let id = identity_of(&elem_str(ids::NAME, "Track A"));
    assert_eq!(id.name(), Some("Track A"));

    let t = track_with_identity(1, 0x77, &elem_str(ids::NAME, "Track A"));
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));
    assert!(dmx.track_identity(1).is_none());
    assert_eq!(dmx.all_track_identity().len(), 1);
    assert_eq!(dmx.all_track_identity()[0].name(), Some("Track A"));
}

/// A full identity record round-trips every field at once.
#[test]
fn full_identity_record() {
    let mut extra = elem_str(ids::NAME, "Commentary (English)");
    extra.extend_from_slice(&elem_str(ids::CODEC_NAME, "Advanced SubStation Alpha"));
    extra.extend_from_slice(&elem_str(ids::LANGUAGE, "eng"));
    extra.extend_from_slice(&elem_str(ids::LANGUAGE_BCP47, "en-US"));
    extra.extend_from_slice(&elem_uint(ids::FLAG_ENABLED, 1));
    extra.extend_from_slice(&elem_uint(ids::FLAG_DEFAULT, 0));
    extra.extend_from_slice(&elem_uint(ids::FLAG_LACING, 0));
    extra.extend_from_slice(&elem_uint(ids::ATTACHMENT_LINK, 42));
    let id = identity_of(&extra);
    assert_eq!(id.name(), Some("Commentary (English)"));
    assert_eq!(id.codec_name(), Some("Advanced SubStation Alpha"));
    assert_eq!(id.language(), Some("en-US"));
    assert_eq!(id.language_matroska(), Some("eng"));
    assert_eq!(id.language_bcp47(), Some("en-US"));
    assert!(id.enabled());
    assert!(!id.default());
    assert!(!id.lacing_allowed());
    assert_eq!(id.attachment_link(), Some(42));
    assert!(!id.is_default());
}
