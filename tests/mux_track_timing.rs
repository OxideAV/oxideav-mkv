//! Round-trip tests for the muxer's `TrackEntry` timing write path
//! (RFC 9559 §5.1.4.1.13..§5.1.4.1.15 — `DefaultDuration`,
//! `DefaultDecodedFieldDuration`, `TrackTimestampScale`).
//!
//! Drives [`MkvMuxer::set_track_timing`] against the public Muxer trait,
//! then re-opens the bytes through [`oxideav_mkv::demux::open_typed`] and
//! confirms [`oxideav_mkv::demux::MkvDemuxer::track_timing`] decodes the
//! exact children handed to the muxer.
//!
//! Spec contracts pinned here:
//!
//! 1. Each explicit `Some(v)` child round-trips bit-exactly.
//! 2. Omitting `set_track_timing` keeps all three elements off-disk —
//!    the demuxer surfaces `None` for the two durations and materialises
//!    the §5.1.4.1.15 `TrackTimestampScale` default `1.0`
//!    (`TrackTiming::is_empty()` is true).
//! 3. `nominal_frame_rate()` derives fps from `DefaultDuration` (ns/frame).
//! 4. `MkvTrackTiming::from_frame_rate` produces the rounded ns interval
//!    and rejects non-finite / non-positive fps.
//! 5. The setter rejects calls after `write_header`, out-of-range stream
//!    indices, and spec-range violations (`0` durations,
//!    `<= 0` / non-finite `TrackTimestampScale`).
//! 6. There is no track-type restriction — the elements sit on
//!    `TrackEntry` and the setter accepts audio tracks too.
//!
//! These tests use the production demuxer + EBML helpers to walk the
//! muxed buffer — no third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::mux::{MkvMuxer, MkvTrackTiming};

/// Unwrap a `Result` expecting an `Err`. The muxer's `Ok` type
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
        "oxideav-mkv-r301-tracktiming-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
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

fn audio_stream() -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("opus"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn video_packet(stream: u32, pts: i64, len: usize) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![0x42; len]);
    p.pts = Some(pts);
    p.flags.keyframe = true;
    p
}

/// Mux a single-track video MKV. `configure` runs between constructing the
/// muxer and `write_header`. Returns the muxed bytes.
fn mux_video<F>(configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer construct");
        configure(&mut mx);
        mx.write_header().expect("write_header");
        mx.write_packet(&video_packet(0, 0, 64)).expect("packet");
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
fn omitted_call_surfaces_empty_record() {
    // With no hint, all three elements stay off-disk. The demuxer still
    // surfaces a record (the elements sit on TrackEntry directly), with both
    // durations None and the §5.1.4.1.15 default TrackTimestampScale 1.0.
    let dmx = demux_typed(mux_video(|_mx| {}));
    let t = dmx.track_timing(0).expect("track_timing surfaced");
    assert_eq!(t.default_duration(), None);
    assert_eq!(t.default_decoded_field_duration(), None);
    assert_eq!(t.track_timestamp_scale_explicit(), None);
    assert_eq!(t.track_timestamp_scale(), 1.0);
    assert_eq!(t.nominal_frame_rate(), None);
    assert!(t.is_empty());
}

#[test]
fn all_three_children_roundtrip() {
    let dmx = demux_typed(mux_video(|mx| {
        mx.set_track_timing(
            0,
            MkvTrackTiming {
                default_duration: Some(33_366_666),
                default_decoded_field_duration: Some(16_683_333),
                track_timestamp_scale: Some(2.0),
            },
        )
        .expect("set_track_timing");
    }));
    let t = dmx.track_timing(0).expect("track_timing surfaced");
    assert_eq!(t.default_duration(), Some(33_366_666));
    assert_eq!(t.default_decoded_field_duration(), Some(16_683_333));
    assert_eq!(t.track_timestamp_scale_explicit(), Some(2.0));
    assert_eq!(t.track_timestamp_scale(), 2.0);
    assert!(!t.is_empty());
}

#[test]
fn nominal_frame_rate_from_default_duration() {
    // 24000/1001 fps ("23.976") stored as the canonical 41708333 ns interval.
    let dmx = demux_typed(mux_video(|mx| {
        mx.set_track_timing(
            0,
            MkvTrackTiming {
                default_duration: Some(41_708_333),
                ..Default::default()
            },
        )
        .expect("set_track_timing");
    }));
    let t = dmx.track_timing(0).expect("track_timing surfaced");
    let fps = t.nominal_frame_rate().expect("rate derivable");
    assert!(
        (fps - 23.976).abs() < 0.001,
        "expected ~23.976 fps, got {fps}"
    );
}

#[test]
fn from_frame_rate_constructor() {
    // 25 fps -> exactly 40_000_000 ns. 30 fps -> 33_333_333 ns (rounded).
    let t25 = MkvTrackTiming::from_frame_rate(25.0).expect("25 fps ok");
    assert_eq!(t25.default_duration, Some(40_000_000));
    assert_eq!(t25.default_decoded_field_duration, None);
    assert_eq!(t25.track_timestamp_scale, None);

    let t30 = MkvTrackTiming::from_frame_rate(30.0).expect("30 fps ok");
    assert_eq!(t30.default_duration, Some(33_333_333));

    // Round-trips through the mux->demux pipeline.
    let dmx = demux_typed(mux_video(|mx| {
        mx.set_track_timing(0, MkvTrackTiming::from_frame_rate(50.0).unwrap())
            .unwrap();
    }));
    let t = dmx.track_timing(0).expect("track_timing surfaced");
    assert_eq!(t.default_duration(), Some(20_000_000));
    let fps = t.nominal_frame_rate().expect("rate derivable");
    assert!((fps - 50.0).abs() < 1e-9, "expected 50 fps, got {fps}");

    // Invalid frame rates rejected.
    assert!(MkvTrackTiming::from_frame_rate(0.0).is_err());
    assert!(MkvTrackTiming::from_frame_rate(-30.0).is_err());
    assert!(MkvTrackTiming::from_frame_rate(f64::NAN).is_err());
    assert!(MkvTrackTiming::from_frame_rate(f64::INFINITY).is_err());
}

#[test]
fn timestamp_scale_default_materialised_distinct_from_explicit() {
    // Explicit TrackTimestampScale 1.0 is byte-distinct from absence on disk
    // but decodes to the same materialised value. The explicit path is the
    // producer-override.
    let with = mux_video(|mx| {
        mx.set_track_timing(
            0,
            MkvTrackTiming {
                track_timestamp_scale: Some(1.0),
                ..Default::default()
            },
        )
        .unwrap();
    });
    let without = mux_video(|_mx| {});
    // TrackTimestampScale id 0x23314F -> [0x23, 0x31, 0x4F]; 8-byte f64 body
    // means size VINT 0x88. Confirm the element is present only when set.
    fn has_tts(bytes: &[u8]) -> bool {
        bytes
            .windows(4)
            .any(|w| w[0] == 0x23 && w[1] == 0x31 && w[2] == 0x4F && w[3] == 0x88)
    }
    assert!(
        has_tts(&with),
        "explicit TrackTimestampScale must be on disk"
    );
    assert!(
        !has_tts(&without),
        "omitted TrackTimestampScale must be off disk"
    );

    let dmx_with = demux_typed(with);
    let t = dmx_with.track_timing(0).expect("surfaced");
    assert_eq!(t.track_timestamp_scale_explicit(), Some(1.0));
    assert_eq!(t.track_timestamp_scale(), 1.0);
    assert!(!t.is_empty());

    let dmx_without = demux_typed(without);
    let t = dmx_without.track_timing(0).expect("surfaced");
    assert_eq!(t.track_timestamp_scale_explicit(), None);
    assert_eq!(t.track_timestamp_scale(), 1.0);
    assert!(t.is_empty());
}

#[test]
fn no_track_type_restriction() {
    // The three elements sit on TrackEntry; the setter accepts audio tracks.
    let tmp = tmp_path("aud");
    let bytes = {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &[audio_stream()]).expect("muxer construct");
        mx.set_track_timing(
            0,
            MkvTrackTiming {
                default_duration: Some(20_000_000),
                ..Default::default()
            },
        )
        .expect("set_track_timing on audio track must succeed");
        mx.write_header().expect("write_header");
        let mut p = Packet::new(0, TimeBase::new(1, 1000), vec![0x5A; 32]);
        p.pts = Some(0);
        p.flags.keyframe = true;
        mx.write_packet(&p).expect("packet");
        mx.write_trailer().expect("write_trailer");
        drop(mx);
        let b = std::fs::read(&tmp).expect("re-read");
        let _ = std::fs::remove_file(&tmp);
        b
    };
    let dmx = demux_typed(bytes);
    let t = dmx.track_timing(0).expect("surfaced");
    assert_eq!(t.default_duration(), Some(20_000_000));
}

#[test]
fn rejects_after_write_header() {
    let f = std::fs::File::create(tmp_path("post")).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer construct");
    mx.write_header().expect("write_header");
    let err = assert_err(
        mx.set_track_timing(0, MkvTrackTiming::from_frame_rate(25.0).unwrap()),
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
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer construct");
    let err = assert_err(
        mx.set_track_timing(5, MkvTrackTiming::from_frame_rate(25.0).unwrap()),
        "must reject out-of-range index",
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
        MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer construct")
    };

    // DefaultDuration must be != 0.
    let mut mx = make();
    assert!(mx
        .set_track_timing(
            0,
            MkvTrackTiming {
                default_duration: Some(0),
                ..Default::default()
            }
        )
        .is_err());

    // DefaultDecodedFieldDuration must be != 0.
    let mut mx = make();
    assert!(mx
        .set_track_timing(
            0,
            MkvTrackTiming {
                default_decoded_field_duration: Some(0),
                ..Default::default()
            }
        )
        .is_err());

    // TrackTimestampScale must be > 0 and finite.
    let mut mx = make();
    assert!(mx
        .set_track_timing(
            0,
            MkvTrackTiming {
                track_timestamp_scale: Some(0.0),
                ..Default::default()
            }
        )
        .is_err());
    let mut mx = make();
    assert!(mx
        .set_track_timing(
            0,
            MkvTrackTiming {
                track_timestamp_scale: Some(-1.0),
                ..Default::default()
            }
        )
        .is_err());
    let mut mx = make();
    assert!(mx
        .set_track_timing(
            0,
            MkvTrackTiming {
                track_timestamp_scale: Some(f64::NAN),
                ..Default::default()
            }
        )
        .is_err());
}

#[test]
fn read_back_queued_hint() {
    let f = std::fs::File::create(tmp_path("acc")).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer construct");
    assert_eq!(mx.track_timing(0), None);
    let hint = MkvTrackTiming::from_frame_rate(60.0).unwrap();
    mx.set_track_timing(0, hint).unwrap();
    assert_eq!(mx.track_timing(0), Some(hint));
}

#[test]
fn last_write_wins() {
    let dmx = demux_typed(mux_video(|mx| {
        mx.set_track_timing(0, MkvTrackTiming::from_frame_rate(25.0).unwrap())
            .unwrap();
        mx.set_track_timing(
            0,
            MkvTrackTiming {
                default_duration: Some(16_666_666),
                track_timestamp_scale: Some(0.5),
                ..Default::default()
            },
        )
        .unwrap();
    }));
    let t = dmx.track_timing(0).expect("surfaced");
    assert_eq!(t.default_duration(), Some(16_666_666));
    assert_eq!(t.track_timestamp_scale_explicit(), Some(0.5));
}
