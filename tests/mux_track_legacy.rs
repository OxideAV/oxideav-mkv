//! Round-trip tests for the muxer's reclaimed Appendix-A `TrackEntry`-level
//! legacy write path (RFC 9559 Appendix A.19..A.23 + A.28..A.32 —
//! `CodecSettings`, `CodecInfoURL`, `CodecDownloadURL`, `CodecDecodeAll`,
//! the ordered `TrackOverlay` list, and the DivXTrickTrack pairing quintet).
//!
//! Drives [`MkvMuxer::set_track_legacy`] against the public Muxer trait, then
//! re-opens the bytes through [`oxideav_mkv::demux::open_typed`] and confirms
//! [`oxideav_mkv::demux::MkvDemuxer::track_legacy`] decodes the exact fields
//! handed to the muxer.
//!
//! Spec contracts pinned here:
//!
//! 1. Every populated field round-trips through the demux-side `TrackLegacy`.
//! 2. Omitting `set_track_legacy` keeps all legacy elements off-disk —
//!    `track_legacy` surfaces `None`.
//! 3. The appendix carries no defaults, so a `Some(0)` `decode_all` /
//!    `trick_track_flag` round-trips as an explicit `0`, distinct from absence.
//! 4. `CodecInfoURL` / `CodecDownloadURL` / `TrackOverlay` preserve on-disk
//!    order (TrackOverlay's order is load-bearing).
//! 5. The setter rejects calls after `write_header`, out-of-range stream
//!    indices, and non-16-byte SegmentUID binaries; an all-absent record
//!    clears the queue.
//!
//! These tests use the production demuxer to walk the muxed buffer — no
//! third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::mux::{MkvMuxer, MkvTrackLegacy};

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
        "oxideav-mkv-r368-tracklegacy-{}-{}-{n}.mkv",
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

fn video_packet(stream: u32, pts: i64, len: usize) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![0x42; len]);
    p.pts = Some(pts);
    p.flags.keyframe = true;
    p
}

/// Mux a single-track video MKV. `configure` runs between constructing the
/// muxer and `write_header`. Returns the muxed bytes.
fn mux_with<F>(configure: F) -> Vec<u8>
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
fn omitted_call_surfaces_no_legacy() {
    // With no hint, no legacy element reaches the disk → track_legacy is None.
    let dmx = demux_typed(mux_with(|_mx| {}));
    assert!(
        dmx.track_legacy(0).is_none(),
        "no set_track_legacy → None on demux"
    );
}

#[test]
fn full_legacy_record_round_trips() {
    let seg_a = vec![0x11u8; 16];
    let seg_b = vec![0x22u8; 16];
    let leg = MkvTrackLegacy {
        codec_settings: Some("crf=18 preset=slow".to_string()),
        codec_info_urls: vec![
            "https://x.test/i1".to_string(),
            "https://x.test/i2".to_string(),
        ],
        codec_download_urls: vec!["https://x.test/d1".to_string()],
        decode_all: Some(1),
        min_cache: Some(2),
        max_cache: Some(10),
        track_offset: Some(-1_000_000),
        gamma_value: Some(2.2),
        frame_rate: Some(23.976),
        // ChannelPositions is Audio-only; this is a video track, so leave it
        // unset (covered by its own test).
        channel_positions: None,
        track_overlays: vec![7, 3, 5],
        trick_track_uid: Some(0xDEAD),
        trick_track_segment_uid: Some(seg_a.clone()),
        trick_track_flag: Some(1),
        trick_master_track_uid: Some(0xBEEF),
        trick_master_track_segment_uid: Some(seg_b.clone()),
    };
    let dmx = demux_typed(mux_with(move |mx| {
        mx.set_track_legacy(0, leg).expect("set_track_legacy");
    }));

    let got = dmx.track_legacy(0).expect("legacy surfaced");
    assert_eq!(got.codec_settings.as_deref(), Some("crf=18 preset=slow"));
    assert_eq!(
        got.codec_info_urls,
        vec!["https://x.test/i1", "https://x.test/i2"]
    );
    assert_eq!(got.codec_download_urls, vec!["https://x.test/d1"]);
    assert_eq!(got.decode_all, Some(1));
    assert!(got.can_decode_damaged());
    assert_eq!(got.min_cache, Some(2));
    assert_eq!(got.max_cache, Some(10));
    assert_eq!(got.track_offset, Some(-1_000_000));
    assert_eq!(
        got.gamma_value,
        Some(2.2),
        "GammaValue round-trips in Video"
    );
    assert_eq!(
        got.frame_rate,
        Some(23.976),
        "FrameRate round-trips in Video"
    );
    assert_eq!(
        got.track_overlays,
        vec![7, 3, 5],
        "TrackOverlay order preserved"
    );
    assert_eq!(got.trick_track_uid, Some(0xDEAD));
    assert_eq!(got.trick_track_segment_uid.as_deref(), Some(&seg_a[..]));
    assert_eq!(got.trick_track_flag, Some(1));
    assert!(got.is_trick_track());
    assert_eq!(got.trick_master_track_uid, Some(0xBEEF));
    assert_eq!(
        got.trick_master_track_segment_uid.as_deref(),
        Some(&seg_b[..])
    );
}

#[test]
fn explicit_zero_flags_round_trip_distinct_from_absence() {
    let leg = MkvTrackLegacy {
        decode_all: Some(0),
        trick_track_flag: Some(0),
        ..Default::default()
    };
    let dmx = demux_typed(mux_with(move |mx| {
        mx.set_track_legacy(0, leg).expect("set_track_legacy");
    }));

    let got = dmx.track_legacy(0).expect("legacy surfaced");
    assert_eq!(got.decode_all, Some(0), "explicit 0 round-trips");
    assert!(!got.can_decode_damaged());
    assert_eq!(got.trick_track_flag, Some(0));
    assert!(!got.is_trick_track());
}

#[test]
fn cache_and_offset_round_trip_including_signs_and_zero() {
    // RFC 9559 Appendix A.16 (MinCache) / A.17 (MaxCache) / A.18
    // (TrackOffset). MinCache=0 and a positive TrackOffset round-trip
    // distinct from absence; MaxCache stays absent.
    let leg = MkvTrackLegacy {
        min_cache: Some(0),
        track_offset: Some(2_500_000),
        ..Default::default()
    };
    let dmx = demux_typed(mux_with(move |mx| {
        mx.set_track_legacy(0, leg).expect("set_track_legacy");
    }));

    let got = dmx.track_legacy(0).expect("legacy surfaced");
    assert_eq!(got.min_cache, Some(0), "explicit MinCache 0 round-trips");
    assert_eq!(got.max_cache, None, "absent MaxCache stays None");
    assert_eq!(got.track_offset, Some(2_500_000), "positive TrackOffset");
    assert!(got.decode_all.is_none());
}

#[test]
fn partial_record_only_writes_populated_fields() {
    let leg = MkvTrackLegacy {
        codec_settings: Some("x264 default".to_string()),
        ..Default::default()
    };
    let dmx = demux_typed(mux_with(move |mx| {
        mx.set_track_legacy(0, leg).expect("set_track_legacy");
    }));

    let got = dmx.track_legacy(0).expect("legacy surfaced");
    assert_eq!(got.codec_settings.as_deref(), Some("x264 default"));
    assert!(got.codec_info_urls.is_empty());
    assert!(got.decode_all.is_none());
    assert!(got.track_overlays.is_empty());
    assert!(got.trick_track_uid.is_none());
}

#[test]
fn empty_record_clears_queue_and_writes_nothing() {
    // An all-absent record queues nothing — track_legacy() is None on the
    // muxer and the demuxer surfaces None.
    let dmx = demux_typed(mux_with(|mx| {
        mx.set_track_legacy(0, MkvTrackLegacy::default())
            .expect("set_track_legacy");
        assert!(
            mx.track_legacy(0).is_none(),
            "all-absent record clears the queue"
        );
    }));
    assert!(dmx.track_legacy(0).is_none());
}

#[test]
fn setter_rejects_after_write_header() {
    let tmp = tmp_path("post");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer construct");
    mx.write_header().expect("write_header");
    let leg = MkvTrackLegacy {
        decode_all: Some(1),
        ..Default::default()
    };
    let e = assert_err(
        mx.set_track_legacy(0, leg),
        "set_track_legacy after write_header",
    );
    assert!(format!("{e}").contains("after write_header"));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn setter_rejects_out_of_range_stream() {
    let tmp = tmp_path("oor");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer construct");
    let leg = MkvTrackLegacy {
        decode_all: Some(1),
        ..Default::default()
    };
    let e = assert_err(
        mx.set_track_legacy(9, leg),
        "set_track_legacy out-of-range stream",
    );
    assert!(format!("{e}").contains("out of range"));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn setter_rejects_non_16_byte_segment_uid() {
    let tmp = tmp_path("uid");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer construct");
    let leg = MkvTrackLegacy {
        trick_track_segment_uid: Some(vec![0xAB, 0xCD, 0xEF]),
        ..Default::default()
    };
    let e = assert_err(
        mx.set_track_legacy(0, leg),
        "set_track_legacy short SegmentUID",
    );
    let msg = format!("{e}");
    assert!(msg.contains("TrickTrackSegmentUID"), "msg: {msg}");
    assert!(msg.contains("16-byte"), "msg: {msg}");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn channel_positions_round_trips_in_audio_master() {
    // RFC 9559 Appendix A.27 — ChannelPositions is nested in the Audio
    // master, so it needs an audio track to land. Drive a one-track audio
    // MKV and confirm the binary table round-trips through TrackLegacy.
    let mut p = CodecParameters::audio(CodecId::new("A_PCM/INT/LIT"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    let audio = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    };

    let tmp = tmp_path("chanpos");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx =
            MkvMuxer::new_matroska(ws, std::slice::from_ref(&audio)).expect("muxer construct");
        mx.set_track_legacy(
            0,
            MkvTrackLegacy {
                channel_positions: Some(vec![0x00, 0x5A, 0xB4]),
                ..Default::default()
            },
        )
        .expect("set_track_legacy");
        mx.write_header().expect("write_header");
        let mut pkt = Packet::new(0, TimeBase::new(1, 1000), vec![0x01; 32]);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mx.write_packet(&pkt).expect("packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);

    let dmx = demux_typed(bytes);
    let got = dmx.track_legacy(0).expect("legacy surfaced");
    assert_eq!(
        got.channel_positions.as_deref(),
        Some(&[0x00, 0x5A, 0xB4][..]),
        "ChannelPositions round-trips through the Audio master"
    );
    // Gamma / FrameRate are Video-only and stay absent on an audio track.
    assert!(got.gamma_value.is_none());
    assert!(got.frame_rate.is_none());
}
