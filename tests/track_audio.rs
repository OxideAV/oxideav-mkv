//! Integration tests for the demuxer's `TrackAudio` typed decode
//! (RFC 9559 §5.1.4.1.29.1..§5.1.4.1.29.4).
//!
//! The four `Audio` children — `SamplingFrequency`, `OutputSamplingFrequency`,
//! `Channels`, `BitDepth` — fold into one `TrackAudio` record per audio
//! track. The spec defaults are materialised asymmetrically: `SamplingFrequency`
//! (§5.1.4.1.29.1) and `Channels` (§5.1.4.1.29.3) carry `minOccurs: 1` with
//! concrete defaults (8000.0 Hz and 1 channel respectively), so an `Audio`
//! master with no explicit children still surfaces meaningful numbers;
//! `OutputSamplingFrequency` (§5.1.4.1.29.2) has Table 19's derived default
//! (= `SamplingFrequency`) so the typed accessor folds the derivation while
//! `output_sampling_frequency_explicit` preserves the on-disk presence;
//! `BitDepth` (§5.1.4.1.29.4) has no spec default and stays `Option<u64>`.
//!
//! Tests below cover: default-materialisation when the `Audio` master is
//! empty, each child set independently, the `OutputSamplingFrequency`
//! derived-default path, the `BitDepth`-as-`Option` distinction, the
//! `is_sbr()` predicate, non-audio tracks returning `None`, and the
//! per-stream slice mirror.

use std::io::Cursor;

use oxideav_core::ReadSeek;
use oxideav_mkv::demux::TrackAudio;
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

fn elem_float_be_f32(id: u32, value: f32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(4, 0));
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

/// Build an audio TrackEntry whose `Audio` master holds `audio_body`
/// (a concatenation of `elem_uint(ids::CHANNELS, ...)`, etc., or empty).
fn audio_track_with_audio_master(number: u64, uid: u64, audio_body: &[u8]) -> Vec<u8> {
    let audio_master = elem_master(ids::AUDIO, audio_body);
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_OPUS"));
    tb.extend_from_slice(&audio_master);
    tb
}

/// Build an audio TrackEntry with NO `Audio` master — pathological case
/// per the schema (audio tracks SHOULD have one) but tolerated.
fn audio_track_without_audio_master(number: u64, uid: u64) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_OPUS"));
    tb
}

/// Build a video TrackEntry — used when the test needs a non-audio track
/// to confirm `track_audio()` returns `None`.
fn video_track(number: u64, uid: u64) -> Vec<u8> {
    let video_body = elem_uint(ids::PIXEL_WIDTH, 320);
    let mut vb = video_body.clone();
    vb.extend_from_slice(&elem_uint(ids::PIXEL_HEIGHT, 240));
    let video_master = elem_master(ids::VIDEO, &vb);
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_VIDEO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "V_VP9"));
    tb.extend_from_slice(&video_master);
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

/// An `Audio` master with no explicit children materialises the
/// `SamplingFrequency` (§5.1.4.1.29.1) default `0x1.f4p+12` = `8000.0` and
/// the `Channels` (§5.1.4.1.29.3) default `1`. `OutputSamplingFrequency`
/// (§5.1.4.1.29.2) returns the derived default (= `SamplingFrequency`),
/// and `BitDepth` (§5.1.4.1.29.4) stays `None`.
#[test]
fn empty_audio_master_materialises_spec_defaults() {
    let t = audio_track_with_audio_master(1, 0xA1, &[]);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let a = dmx.track_audio(0).expect("track 0 present");
    assert_eq!(a.sampling_frequency(), 8000.0);
    assert_eq!(a.output_sampling_frequency(), 8000.0); // derived default
    assert_eq!(a.output_sampling_frequency_explicit(), None);
    assert_eq!(a.channels(), 1);
    assert_eq!(a.bit_depth(), None);
    assert!(!a.is_sbr());
}

/// Explicit `SamplingFrequency` of 48000.0 surfaces unchanged through
/// `sampling_frequency()`. `OutputSamplingFrequency` is absent, so its
/// typed accessor returns the derived default (= 48000.0) and the
/// `_explicit` accessor returns `None`.
#[test]
fn explicit_sampling_frequency() {
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48000.0));
    let t = audio_track_with_audio_master(1, 0xA1, &audio_body);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let a = dmx.track_audio(0).expect("track 0 present");
    assert_eq!(a.sampling_frequency(), 48000.0);
    assert_eq!(a.output_sampling_frequency(), 48000.0);
    assert_eq!(a.output_sampling_frequency_explicit(), None);
}

/// `SamplingFrequency` accepts an f32 payload (4-byte float) — the spec
/// permits 4-byte or 8-byte floats per RFC 8794. The typed accessor
/// folds both byte widths into f64.
#[test]
fn sampling_frequency_f32_payload() {
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f32(ids::SAMPLING_FREQUENCY, 44100.0));
    let t = audio_track_with_audio_master(1, 0xA1, &audio_body);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let a = dmx.track_audio(0).expect("track 0 present");
    assert!((a.sampling_frequency() - 44100.0).abs() < 1.0);
}

/// Explicit `OutputSamplingFrequency` strictly greater than
/// `SamplingFrequency` is the canonical SBR signal. `is_sbr()` fires;
/// the `_explicit` accessor records the on-disk value.
#[test]
fn explicit_output_sampling_frequency_signals_sbr() {
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 22050.0));
    audio_body.extend_from_slice(&elem_float_be_f64(ids::OUTPUT_SAMPLING_FREQUENCY, 44100.0));
    let t = audio_track_with_audio_master(1, 0xA1, &audio_body);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let a = dmx.track_audio(0).expect("track 0 present");
    assert_eq!(a.sampling_frequency(), 22050.0);
    assert_eq!(a.output_sampling_frequency(), 44100.0);
    assert_eq!(a.output_sampling_frequency_explicit(), Some(44100.0));
    assert!(a.is_sbr());
}

/// Explicit `OutputSamplingFrequency` equal to `SamplingFrequency` does
/// NOT signal SBR. The `_explicit` accessor still records the on-disk
/// value (distinguishable from absence).
#[test]
fn explicit_output_sampling_frequency_equal_is_not_sbr() {
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48000.0));
    audio_body.extend_from_slice(&elem_float_be_f64(ids::OUTPUT_SAMPLING_FREQUENCY, 48000.0));
    let t = audio_track_with_audio_master(1, 0xA1, &audio_body);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let a = dmx.track_audio(0).expect("track 0 present");
    assert_eq!(a.output_sampling_frequency(), 48000.0);
    assert_eq!(a.output_sampling_frequency_explicit(), Some(48000.0));
    assert!(!a.is_sbr());
}

/// Explicit `Channels=6` (5.1) surfaces through `channels()` unchanged.
/// `SamplingFrequency` keeps its default 8000.0 when absent.
#[test]
fn explicit_channels_six() {
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_uint(ids::CHANNELS, 6));
    let t = audio_track_with_audio_master(1, 0xA1, &audio_body);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let a = dmx.track_audio(0).expect("track 0 present");
    assert_eq!(a.channels(), 6);
    assert_eq!(a.sampling_frequency(), 8000.0); // default
}

/// Explicit `BitDepth=24` is the typical 24-bit PCM signal. The accessor
/// returns `Some(24)`; without the element it returns `None`.
#[test]
fn explicit_bit_depth() {
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 24));
    let t = audio_track_with_audio_master(1, 0xA1, &audio_body);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let a = dmx.track_audio(0).expect("track 0 present");
    assert_eq!(a.bit_depth(), Some(24));
}

/// All four children set together — full 48 kHz, stereo, 16-bit, no SBR.
/// Every field surfaces explicitly without folding any default.
#[test]
fn all_four_children_set() {
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48000.0));
    audio_body.extend_from_slice(&elem_float_be_f64(ids::OUTPUT_SAMPLING_FREQUENCY, 48000.0));
    audio_body.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio_body.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    let t = audio_track_with_audio_master(1, 0xA1, &audio_body);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let a = dmx.track_audio(0).expect("track 0 present");
    assert_eq!(a.sampling_frequency(), 48000.0);
    assert_eq!(a.output_sampling_frequency(), 48000.0);
    assert_eq!(a.output_sampling_frequency_explicit(), Some(48000.0));
    assert_eq!(a.channels(), 2);
    assert_eq!(a.bit_depth(), Some(16));
    assert!(!a.is_sbr());
}

/// A `TrackEntry` without an `Audio` sub-master surfaces `None` from
/// `track_audio()`. The schema mandates `Audio` on audio tracks, but the
/// typed surface tolerates the absence and never synthesises a record.
#[test]
fn audio_track_without_audio_master_surfaces_none() {
    let t = audio_track_without_audio_master(1, 0xA1);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    assert!(dmx.track_audio(0).is_none());
    assert_eq!(dmx.all_track_audio().len(), 1);
    assert!(dmx.all_track_audio()[0].is_none());
}

/// Video tracks return `None` from `track_audio()`. The `Audio` master
/// belongs on audio tracks only.
#[test]
fn video_track_surfaces_none() {
    let t = video_track(1, 0x71);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    assert!(dmx.track_audio(0).is_none());
    assert_eq!(dmx.all_track_audio().len(), 1);
    assert!(dmx.all_track_audio()[0].is_none());
}

/// A multi-track file mixes audio + video. Each stream index gets its own
/// `Option<TrackAudio>`; the audio track surfaces a record while the video
/// track surfaces `None`.
#[test]
fn mixed_audio_video_tracks() {
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 44100.0));
    audio_body.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    let a = audio_track_with_audio_master(1, 0xA1, &audio_body);
    let v = video_track(2, 0x71);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &a));
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &v));
    let dmx = open(assemble(&body));

    let a = dmx.track_audio(0).expect("audio track present");
    assert_eq!(a.sampling_frequency(), 44100.0);
    assert_eq!(a.channels(), 2);

    assert!(dmx.track_audio(1).is_none());
    assert_eq!(dmx.all_track_audio().len(), 2);
}

/// `track_audio()` returns `None` for an out-of-range `stream_index`
/// without panicking. The `all_track_audio()` slice length always
/// matches the number of streams.
#[test]
fn out_of_range_stream_index_returns_none() {
    let t = audio_track_with_audio_master(1, 0xA1, &[]);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    assert!(dmx.track_audio(5).is_none());
    assert_eq!(dmx.all_track_audio().len(), 1);
}

/// Slice mirror — `all_track_audio()` holds the same record returned by
/// `track_audio(idx)` for every in-range index. The structural equality
/// makes downstream re-muxing safe to lift the slice once and iterate.
#[test]
fn slice_mirror_matches_per_track_accessor() {
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48000.0));
    audio_body.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio_body.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 24));
    let t = audio_track_with_audio_master(1, 0xA1, &audio_body);
    let mut body = Vec::new();
    body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&body));

    let direct: &TrackAudio = dmx.track_audio(0).expect("track 0 present");
    let slice: &Option<TrackAudio> = &dmx.all_track_audio()[0];
    assert_eq!(slice.as_ref(), Some(direct));
}
