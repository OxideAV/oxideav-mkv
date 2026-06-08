//! Integration tests for the demuxer's `TrackAudienceFlags` typed decode
//! (RFC 9559 §5.1.4.1.6..§5.1.4.1.11).
//!
//! The six audience flags — `FlagForced`, `FlagHearingImpaired`,
//! `FlagVisualImpaired`, `FlagTextDescriptions`, `FlagOriginal`,
//! `FlagCommentary` — describe how a player should present the track to a
//! particular kind of viewer. Each is a `0..=1` uinteger on disk; the
//! typed surface folds all six into one `TrackAudienceFlags` record per
//! stream.
//!
//! The §5.1.4.1.6 spec default `0` for `FlagForced` is materialised on
//! the typed surface; the other five (`minver: 4`) carry no spec default,
//! so absence on disk surfaces as `None` and `Some(false)` exclusively
//! means "the writer emitted an explicit zero." Tests below cover each
//! variant — default materialisation, explicit zero, explicit one — and
//! the convenience predicates [`TrackAudienceFlags::is_default_presentation`]
//! / [`TrackAudienceFlags::is_accessibility`].

use std::io::Cursor;

use oxideav_core::ReadSeek;
use oxideav_mkv::demux::TrackAudienceFlags;
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
    ib.extend_from_slice(&elem_float_be_f64(ids::DURATION, 1000.0));
    elem_master(ids::INFO, &ib)
}

fn one_cluster() -> Vec<u8> {
    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cb.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    elem_master(ids::CLUSTER, &cb)
}

/// Build a single subtitle TrackEntry that carries `flag_body` between
/// `TrackType` and `CodecID`. `flag_body` is a concatenation of
/// `elem_uint(ids::FLAG_*, …)` calls.
fn subtitle_track_with_flags(flag_body: &[u8]) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, 0x71));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_SUBTITLE));
    tb.extend_from_slice(flag_body);
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "S_TEXT/UTF8"));
    tb
}

/// Build a single audio TrackEntry that carries `flag_body`. The audio
/// track is needed for the Cluster to have a packet attached (subtitle
/// tracks don't carry frame payloads in our test cluster).
fn audio_track_with_flags(number: u64, uid: u64, flag_body: &[u8]) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(flag_body);
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_OPUS"));
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

/// `TrackEntry` with no audience-flag children: `FlagForced` materialises
/// the spec default `0` (`false`); the five `minver: 4` flags surface as
/// `None` because the spec gives them no default.
#[test]
fn absent_flags_default_to_forced_false_and_others_none() {
    let t = audio_track_with_flags(1, 0xA1, &[]);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let f = dmx.track_audience_flags(0).expect("track 0 present");
    assert!(
        !f.forced(),
        "FlagForced default 0 must materialise to false"
    );
    assert_eq!(f.hearing_impaired(), None);
    assert_eq!(f.visual_impaired(), None);
    assert_eq!(f.text_descriptions(), None);
    assert_eq!(f.original(), None);
    assert_eq!(f.commentary(), None);

    // Defaults look like a vanilla content track.
    assert!(f.is_default_presentation());
    assert!(!f.is_accessibility());

    // Slice mirror — one entry per stream.
    assert_eq!(dmx.all_track_audience_flags().len(), 1);
    assert_eq!(dmx.all_track_audience_flags()[0], *f);
}

/// `FlagForced=1` on a subtitle track surfaces through `forced()` as
/// `true`. The §5.1.4.1.6 wording is "applies only to subtitles" — the
/// typed surface doesn't second-guess the writer; it just reports the
/// flag.
#[test]
fn explicit_forced_subtitle() {
    let flags = elem_uint(ids::FLAG_FORCED, 1);
    let t = subtitle_track_with_flags(&flags);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let f = dmx.track_audience_flags(0).expect("track 0 present");
    assert!(f.forced());
    assert_eq!(f.hearing_impaired(), None);
    assert!(!f.is_default_presentation());
}

/// An explicit `FlagForced=0` is observationally identical to the spec
/// default — both surface as `false`. The spec gives no Way to distinguish
/// the two cases for `FlagForced` since the default *is* `0`, so the
/// typed surface collapses them.
#[test]
fn explicit_forced_zero_matches_default() {
    let flags = elem_uint(ids::FLAG_FORCED, 0);
    let t = subtitle_track_with_flags(&flags);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let f = dmx.track_audience_flags(0).expect("track 0 present");
    assert!(!f.forced());
}

/// `FlagHearingImpaired=1` surfaces through `hearing_impaired()` as
/// `Some(true)`. The other five flags (still absent) stay at their
/// defaults / `None`.
#[test]
fn explicit_hearing_impaired_one() {
    let flags = elem_uint(ids::FLAG_HEARING_IMPAIRED, 1);
    let t = subtitle_track_with_flags(&flags);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let f = dmx.track_audience_flags(0).expect("track 0 present");
    assert_eq!(f.hearing_impaired(), Some(true));
    assert!(!f.forced());
    assert_eq!(f.visual_impaired(), None);

    // Accessibility detector flags this track.
    assert!(f.is_accessibility());
    assert!(!f.is_default_presentation());
}

/// `FlagHearingImpaired=0` (writer explicitly cleared) is distinct from
/// absent. The typed surface returns `Some(false)`, not `None`.
#[test]
fn explicit_hearing_impaired_zero_distinct_from_absent() {
    let flags = elem_uint(ids::FLAG_HEARING_IMPAIRED, 0);
    let t = audio_track_with_flags(1, 0xA1, &flags);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let f = dmx.track_audience_flags(0).expect("track 0 present");
    assert_eq!(
        f.hearing_impaired(),
        Some(false),
        "explicit 0 must show as Some(false), not None"
    );
    assert!(!f.is_accessibility());
    assert!(f.is_default_presentation());
}

/// Each §minver-4 flag is parsed independently. Setting all five to `1`
/// surfaces each through its own accessor.
#[test]
fn all_minver4_flags_set_independently() {
    let mut flags = Vec::new();
    flags.extend_from_slice(&elem_uint(ids::FLAG_HEARING_IMPAIRED, 1));
    flags.extend_from_slice(&elem_uint(ids::FLAG_VISUAL_IMPAIRED, 1));
    flags.extend_from_slice(&elem_uint(ids::FLAG_TEXT_DESCRIPTIONS, 1));
    flags.extend_from_slice(&elem_uint(ids::FLAG_ORIGINAL, 1));
    flags.extend_from_slice(&elem_uint(ids::FLAG_COMMENTARY, 1));
    let t = audio_track_with_flags(1, 0xA1, &flags);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let f = dmx.track_audience_flags(0).expect("track 0 present");
    assert_eq!(f.hearing_impaired(), Some(true));
    assert_eq!(f.visual_impaired(), Some(true));
    assert_eq!(f.text_descriptions(), Some(true));
    assert_eq!(f.original(), Some(true));
    assert_eq!(f.commentary(), Some(true));
    assert!(!f.forced(), "FlagForced absent → default 0 → false");
    assert!(f.is_accessibility());
    assert!(!f.is_default_presentation());
}

/// Multi-track files surface one record per stream, in stream-index
/// order. A subtitle track with `FlagForced=1` and an audio commentary
/// track with `FlagCommentary=1` produce two records the caller can
/// disambiguate by index.
#[test]
fn multi_track_records_per_stream() {
    let subtitle_flags = elem_uint(ids::FLAG_FORCED, 1);
    let st = subtitle_track_with_flags(&subtitle_flags);

    // Bump the audio track's TrackNumber so SimpleBlock's track-ref (1)
    // still resolves to the first track. Use TrackNumber=2 for audio.
    let commentary_flags = elem_uint(ids::FLAG_COMMENTARY, 1);
    let at = audio_track_with_flags(2, 0xA2, &commentary_flags);

    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &st));
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &at));
    let dmx = open(assemble(&body));

    assert_eq!(dmx.all_track_audience_flags().len(), 2);
    let st_flags = dmx.track_audience_flags(0).expect("track 0");
    assert!(st_flags.forced());
    assert!(!matches!(st_flags.commentary(), Some(true)));

    let at_flags = dmx.track_audience_flags(1).expect("track 1");
    assert!(!at_flags.forced());
    assert_eq!(at_flags.commentary(), Some(true));
}

/// `track_audience_flags(99)` for an out-of-range stream index returns
/// `None` rather than panicking.
#[test]
fn out_of_range_stream_index_returns_none() {
    let t = audio_track_with_flags(1, 0xA1, &[]);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    assert!(dmx.track_audience_flags(99).is_none());
    assert_eq!(dmx.all_track_audience_flags().len(), 1);
}

/// The default-built `TrackAudienceFlags` matches what an empty
/// `TrackEntry` decodes to: `forced == false` plus all-`None` for the
/// `minver: 4` flags.
#[test]
fn default_matches_empty_track_entry() {
    let t = audio_track_with_flags(1, 0xA1, &[]);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let f = dmx.track_audience_flags(0).expect("track 0 present");
    assert_eq!(*f, TrackAudienceFlags::default());
}

/// `is_default_presentation` returns `true` for a track that has no
/// explicit `Some(true)` flag — including ones the writer explicitly
/// cleared with `Some(false)`. `is_accessibility` only fires on
/// `Some(true)` for `hearing_impaired` / `visual_impaired` /
/// `text_descriptions`.
#[test]
fn predicates_distinguish_some_false_from_some_true() {
    let mut flags = Vec::new();
    flags.extend_from_slice(&elem_uint(ids::FLAG_HEARING_IMPAIRED, 0));
    flags.extend_from_slice(&elem_uint(ids::FLAG_ORIGINAL, 0));
    let t = audio_track_with_flags(1, 0xA1, &flags);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let f = dmx.track_audience_flags(0).expect("track 0 present");
    assert_eq!(f.hearing_impaired(), Some(false));
    assert_eq!(f.original(), Some(false));
    // No Some(true) anywhere → default presentation.
    assert!(f.is_default_presentation());
    assert!(!f.is_accessibility());
}
