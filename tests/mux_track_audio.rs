//! Round-trip tests for the muxer's `Audio` master write path
//! (RFC 9559 §5.1.4.1.29, including §5.1.4.1.29.1..§5.1.4.1.29.4).
//!
//! Drives [`MkvMuxer::set_track_audio`] against the public Muxer trait,
//! then re-opens the bytes through [`oxideav_mkv::demux::open_typed`] and
//! confirms [`oxideav_mkv::demux::MkvDemuxer::track_audio`] decodes the
//! exact children handed to the muxer — most importantly the
//! `OutputSamplingFrequency` SBR signal (§5.1.4.1.29.2) which the
//! `StreamInfo`-derived write path cannot express.
//!
//! Spec contracts pinned here:
//!
//! 1. An explicit hint overrides the `StreamInfo`-derived
//!    `SamplingFrequency` / `Channels` / `BitDepth` children.
//! 2. `OutputSamplingFrequency` round-trips and the demuxer's
//!    `is_sbr()` predicate fires for the SBR-doubling shape.
//! 3. Omitting `set_track_audio` keeps the existing `StreamInfo`-derived
//!    `Audio` master (back-compat), with no `OutputSamplingFrequency`
//!    element on disk (demuxer applies the Table 19 derived default).
//! 4. The setter rejects calls after `write_header`, out-of-range stream
//!    indices, non-audio tracks, and spec-range violations (sampling
//!    freqs `<= 0`, zero `Channels`, zero `BitDepth`).
//! 5. The `MkvTrackAudio::sbr` convenience constructor produces the
//!    canonical HE-AAC core-rate / doubled-output-rate pair.
//!
//! These tests use the production demuxer + EBML helpers to walk the
//! muxed buffer — no third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mkv::mux::{MkvMuxer, MkvTrackAudio};

/// Unwrap a `Result` expecting an `Err`, returning the error. Mirrors the
/// helper in the other mux test modules — the muxer's `Ok` type
/// (`&mut MkvMuxer`) is not `Debug`, so `Result::expect_err` won't compile.
fn assert_err<T>(r: Result<T, Error>, msg: &str) -> Error {
    match r {
        Ok(_) => panic!("{msg}: expected Err, got Ok"),
        Err(e) => e,
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r291-trackaudio-{}-{}-{n}.mka",
        tag,
        std::process::id()
    ))
}

/// A stereo 48 kHz S16 PCM audio stream. `StreamInfo` carries
/// `sample_rate` / `channels` / `sample_format`, so the muxer derives a
/// non-empty `Audio` master even with no `set_track_audio` hint.
fn audio_stream() -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn video_stream() -> StreamInfo {
    let mut p = CodecParameters::video(CodecId::new("vp9"));
    p.width = Some(320);
    p.height = Some(240);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn audio_packet(stream: u32, pts: i64, len: usize) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![0x5A; len]);
    p.pts = Some(pts);
    p.flags.keyframe = true;
    p
}

/// Mux a single-track audio MKA. `configure` runs between constructing
/// the muxer and `write_header`. Returns the muxed bytes.
fn mux_audio<F>(configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let stream = audio_stream();
        let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
        configure(&mut mx);
        mx.write_header().expect("write_header");
        mx.write_packet(&audio_packet(0, 0, 64)).expect("packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn demux_typed(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open_typed")
}

#[test]
fn omitted_call_keeps_streaminfo_audio() {
    // Back-compat: with no hint the muxer derives the Audio master from
    // StreamInfo alone. SamplingFrequency = 48000, Channels = 2,
    // BitDepth = 16 (S16). No OutputSamplingFrequency on disk, so the
    // demuxer applies the §Table 19 derived default (= SamplingFrequency)
    // and is_sbr() is false.
    let dmx = demux_typed(mux_audio(|_mx| {}));
    let a = dmx.track_audio(0).expect("track_audio surfaced");
    assert_eq!(a.sampling_frequency(), 48_000.0);
    assert_eq!(a.channels(), 2);
    assert_eq!(a.bit_depth(), Some(16));
    assert_eq!(a.output_sampling_frequency_explicit(), None);
    assert_eq!(a.output_sampling_frequency(), 48_000.0);
    assert!(!a.is_sbr());
}

#[test]
fn explicit_hint_overrides_streaminfo_children() {
    // A hint with every field Some overrides the StreamInfo-derived
    // children. Use values distinct from the StreamInfo defaults so the
    // override is observable.
    let dmx = demux_typed(mux_audio(|mx| {
        mx.set_track_audio(
            0,
            MkvTrackAudio {
                sampling_frequency: Some(44_100.0),
                output_sampling_frequency: None,
                channels: Some(6),
                bit_depth: Some(24),
            },
        )
        .expect("set_track_audio");
    }));
    let a = dmx.track_audio(0).expect("track_audio surfaced");
    assert_eq!(a.sampling_frequency(), 44_100.0);
    assert_eq!(a.channels(), 6);
    assert_eq!(a.bit_depth(), Some(24));
}

#[test]
fn output_sampling_frequency_sbr_roundtrip() {
    // The headline feature: OutputSamplingFrequency (§5.1.4.1.29.2), the
    // SBR output rate the StreamInfo-derived path cannot express. A
    // 22050 Hz core with a 44100 Hz output rate is the canonical HE-AAC
    // SBR-doubling shape — is_sbr() must fire.
    let dmx = demux_typed(mux_audio(|mx| {
        mx.set_track_audio(
            0,
            MkvTrackAudio {
                sampling_frequency: Some(22_050.0),
                output_sampling_frequency: Some(44_100.0),
                channels: None,
                bit_depth: None,
            },
        )
        .expect("set_track_audio");
    }));
    let a = dmx.track_audio(0).expect("track_audio surfaced");
    assert_eq!(a.sampling_frequency(), 22_050.0);
    assert_eq!(a.output_sampling_frequency_explicit(), Some(44_100.0));
    assert_eq!(a.output_sampling_frequency(), 44_100.0);
    assert!(
        a.is_sbr(),
        "explicit OutputSamplingFrequency > core must signal SBR"
    );
    // channels / bit_depth deferred to StreamInfo.
    assert_eq!(a.channels(), 2);
    assert_eq!(a.bit_depth(), Some(16));
}

#[test]
fn sbr_convenience_constructor() {
    // MkvTrackAudio::sbr(core) produces (core, 2*core) — the canonical
    // HE-AAC pair.
    let hint = MkvTrackAudio::sbr(24_000.0);
    assert_eq!(hint.sampling_frequency, Some(24_000.0));
    assert_eq!(hint.output_sampling_frequency, Some(48_000.0));
    assert_eq!(hint.channels, None);
    assert_eq!(hint.bit_depth, None);

    let dmx = demux_typed(mux_audio(|mx| {
        mx.set_track_audio(0, MkvTrackAudio::sbr(24_000.0))
            .expect("set_track_audio");
    }));
    let a = dmx.track_audio(0).expect("track_audio surfaced");
    assert_eq!(a.sampling_frequency(), 24_000.0);
    assert_eq!(a.output_sampling_frequency(), 48_000.0);
    assert!(a.is_sbr());
}

#[test]
fn output_sampling_frequency_element_only_when_set() {
    // OutputSamplingFrequency id 0x78B5 = [0x78, 0xB5]; the element is an
    // 8-byte big-endian f64 so the on-disk header is [0x78, 0xB5, 0x88]
    // (size VINT 0x88 = marker | 8). Confirm it appears only when the
    // hint supplied output_sampling_frequency.
    fn has_output_sf(bytes: &[u8]) -> bool {
        bytes
            .windows(3)
            .any(|w| w[0] == 0x78 && w[1] == 0xB5 && w[2] == 0x88)
    }
    let with = mux_audio(|mx| {
        mx.set_track_audio(0, MkvTrackAudio::sbr(22_050.0)).unwrap();
    });
    let without_hint = mux_audio(|_mx| {});
    let with_hint_no_osf = mux_audio(|mx| {
        mx.set_track_audio(
            0,
            MkvTrackAudio {
                sampling_frequency: Some(44_100.0),
                output_sampling_frequency: None,
                channels: None,
                bit_depth: None,
            },
        )
        .unwrap();
    });
    assert!(
        has_output_sf(&with),
        "OutputSamplingFrequency must be present when set"
    );
    assert!(
        !has_output_sf(&without_hint),
        "OutputSamplingFrequency must be absent with no hint"
    );
    assert!(
        !has_output_sf(&with_hint_no_osf),
        "OutputSamplingFrequency must be absent when the hint left it None"
    );
}

#[test]
fn unset_streaminfo_fields_fall_back_to_spec_defaults() {
    // A hint that only sets OutputSamplingFrequency, on a StreamInfo with
    // no sample_rate / channels, must omit SamplingFrequency / Channels
    // so the demuxer materialises the §5.1.4.1.29.1 / .3 spec defaults
    // (8000.0 / 1).
    let tmp = tmp_path("bare");
    let bytes = {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        // Bare audio StreamInfo with no audio descriptors.
        let mut p = CodecParameters::audio(CodecId::new("opus"));
        p.sample_rate = None;
        p.channels = None;
        p.sample_format = None;
        let stream = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 48_000),
            duration: None,
            start_time: Some(0),
            params: p,
        };
        let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
        mx.set_track_audio(
            0,
            MkvTrackAudio {
                sampling_frequency: None,
                output_sampling_frequency: Some(96_000.0),
                channels: None,
                bit_depth: None,
            },
        )
        .expect("set_track_audio");
        mx.write_header().expect("write_header");
        mx.write_packet(&audio_packet(0, 0, 32)).expect("packet");
        mx.write_trailer().expect("write_trailer");
        drop(mx);
        let b = std::fs::read(&tmp).expect("re-read");
        let _ = std::fs::remove_file(&tmp);
        b
    };
    let dmx = demux_typed(bytes);
    let a = dmx.track_audio(0).expect("track_audio surfaced");
    // SamplingFrequency omitted -> spec default 8000.0.
    assert_eq!(a.sampling_frequency(), 8_000.0);
    // Channels omitted -> spec default 1 (mono).
    assert_eq!(a.channels(), 1);
    // BitDepth omitted -> None (no spec default).
    assert_eq!(a.bit_depth(), None);
    // OutputSamplingFrequency was set explicitly.
    assert_eq!(a.output_sampling_frequency_explicit(), Some(96_000.0));
}

#[test]
fn last_write_wins() {
    let dmx = demux_typed(mux_audio(|mx| {
        mx.set_track_audio(0, MkvTrackAudio::sbr(22_050.0)).unwrap();
        mx.set_track_audio(
            0,
            MkvTrackAudio {
                sampling_frequency: Some(96_000.0),
                output_sampling_frequency: None,
                channels: Some(8),
                bit_depth: None,
            },
        )
        .unwrap();
    }));
    let a = dmx.track_audio(0).expect("track_audio surfaced");
    assert_eq!(a.sampling_frequency(), 96_000.0);
    assert_eq!(a.channels(), 8);
    // The second hint left OutputSamplingFrequency None -> absent.
    assert_eq!(a.output_sampling_frequency_explicit(), None);
}

#[test]
fn rejects_after_write_header() {
    let f = std::fs::File::create(tmp_path("post")).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[audio_stream()]).expect("muxer construct");
    mx.write_header().expect("write_header");
    let err = assert_err(
        mx.set_track_audio(0, MkvTrackAudio::sbr(22_050.0)),
        "must reject post-write_header",
    );
    assert!(
        matches!(err, Error::Other(_)),
        "expected Error::Other, got {err:?}"
    );
}

#[test]
fn rejects_out_of_range_stream() {
    let f = std::fs::File::create(tmp_path("oor")).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[audio_stream()]).expect("muxer construct");
    let err = assert_err(
        mx.set_track_audio(5, MkvTrackAudio::sbr(22_050.0)),
        "must reject out-of-range index",
    );
    assert!(
        matches!(err, Error::InvalidData(_)),
        "expected InvalidData, got {err:?}"
    );
}

#[test]
fn rejects_non_audio_track() {
    let f = std::fs::File::create(tmp_path("vid")).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer construct");
    let err = assert_err(
        mx.set_track_audio(0, MkvTrackAudio::sbr(22_050.0)),
        "must reject non-audio track",
    );
    assert!(
        matches!(err, Error::InvalidData(_)),
        "expected InvalidData, got {err:?}"
    );
}

#[test]
fn rejects_out_of_range_values() {
    let make = || {
        let f = std::fs::File::create(tmp_path("rng")).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        MkvMuxer::new_matroska(ws, &[audio_stream()]).expect("muxer construct")
    };

    // SamplingFrequency must be > 0.
    let mut mx = make();
    assert!(mx
        .set_track_audio(
            0,
            MkvTrackAudio {
                sampling_frequency: Some(0.0),
                ..Default::default()
            }
        )
        .is_err());

    // OutputSamplingFrequency must be > 0 (and finite).
    let mut mx = make();
    assert!(mx
        .set_track_audio(
            0,
            MkvTrackAudio {
                output_sampling_frequency: Some(-1.0),
                ..Default::default()
            }
        )
        .is_err());
    let mut mx = make();
    assert!(mx
        .set_track_audio(
            0,
            MkvTrackAudio {
                output_sampling_frequency: Some(f64::NAN),
                ..Default::default()
            }
        )
        .is_err());

    // Channels must be != 0.
    let mut mx = make();
    assert!(mx
        .set_track_audio(
            0,
            MkvTrackAudio {
                channels: Some(0),
                ..Default::default()
            }
        )
        .is_err());

    // BitDepth must be != 0.
    let mut mx = make();
    assert!(mx
        .set_track_audio(
            0,
            MkvTrackAudio {
                bit_depth: Some(0),
                ..Default::default()
            }
        )
        .is_err());
}

#[test]
fn read_back_queued_hint() {
    let f = std::fs::File::create(tmp_path("acc")).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[audio_stream()]).expect("muxer construct");
    assert_eq!(mx.track_audio(0), None);
    let hint = MkvTrackAudio::sbr(22_050.0);
    mx.set_track_audio(0, hint).unwrap();
    assert_eq!(mx.track_audio(0), Some(hint));
}
