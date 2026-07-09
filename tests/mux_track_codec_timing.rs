//! Round-trip tests for the muxer's `TrackEntry` codec-timing write path
//! (RFC 9559 §5.1.4.1.25 / §5.1.4.1.26 — `CodecDelay`, `SeekPreRoll`).
//!
//! Drives [`MkvMuxer::set_track_codec_timing`] against the public Muxer
//! trait, then re-opens the bytes through [`oxideav_mkv::demux::open_typed`]
//! and confirms [`oxideav_mkv::demux::MkvDemuxer::track_codec_timing`]
//! decodes the exact children handed to the muxer.
//!
//! Spec contracts pinned here:
//!
//! 1. Each explicit `Some(v)` child round-trips bit-exactly, including an
//!    explicit `0` (which is byte-distinct from omission — neither element
//!    has a "not 0" range).
//! 2. Omitting the setter on a non-Opus track keeps both elements off-disk;
//!    the demuxer still surfaces a record (the elements sit on `TrackEntry`
//!    directly) with both `_explicit` accessors `None` and `is_empty()`.
//! 3. An Opus track auto-derives `CodecDelay` (pre-skip in ns) and an 80 ms
//!    `SeekPreRoll` with no hint.
//! 4. An explicit hint overrides the Opus auto-derivation *per field* — a
//!    `Some` field replaces the auto value, a `None` field keeps it.
//! 5. The setter rejects calls after `write_header` and out-of-range stream
//!    indices, and there is no track-type restriction.
//!
//! These tests use the production demuxer to walk the muxed buffer — no
//! third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::mux::{MkvMuxer, MkvTrackCodecTiming};

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
        "oxideav-mkv-r404-codectiming-{}-{}-{n}.mkv",
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

/// An Opus stream carrying an `OpusHead` extradata with `pre_skip = 312`
/// samples (48 kHz). `312 * 1e9 / 48000 = 6_500_000` ns of `CodecDelay`.
fn opus_stream() -> StreamInfo {
    let mut head = Vec::new();
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(2); // channel count
    head.extend_from_slice(&312u16.to_le_bytes()); // pre_skip (offset 10..12)
    head.extend_from_slice(&48_000u32.to_le_bytes()); // input sample rate
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family
    let mut p = CodecParameters::audio(CodecId::new("opus"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p.extradata = head;
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

/// Mux a single-track MKV over `stream`. `configure` runs between
/// constructing the muxer and `write_header`. Returns the muxed bytes.
fn mux_one<F>(stream: StreamInfo, configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
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

/// `CodecDelay` id 0x56AA -> [0x56, 0xAA]; `SeekPreRoll` id 0x56BB ->
/// [0x56, 0xBB]. Scan for the two-byte id followed by a one-byte-length VINT.
fn has_id_pair(bytes: &[u8], b0: u8, b1: u8) -> bool {
    bytes.windows(2).any(|w| w[0] == b0 && w[1] == b1)
}

#[test]
fn omitted_call_non_opus_surfaces_empty_record() {
    // A non-Opus track with no hint keeps both elements off-disk; the demuxer
    // still surfaces a record with both `_explicit` accessors None.
    let bytes = mux_one(video_stream(), |_mx| {});
    assert!(
        !has_id_pair(&bytes, 0x56, 0xAA),
        "omitted CodecDelay must be off disk"
    );
    assert!(
        !has_id_pair(&bytes, 0x56, 0xBB),
        "omitted SeekPreRoll must be off disk"
    );
    let dmx = demux_typed(bytes);
    let t = dmx.track_codec_timing(0).expect("record surfaced");
    assert_eq!(t.codec_delay_explicit(), None);
    assert_eq!(t.seek_pre_roll_explicit(), None);
    assert_eq!(t.codec_delay(), 0);
    assert_eq!(t.seek_pre_roll(), 0);
    assert!(t.is_empty());
}

#[test]
fn both_children_roundtrip_on_video() {
    let dmx = demux_typed(mux_one(video_stream(), |mx| {
        mx.set_track_codec_timing(
            0,
            MkvTrackCodecTiming {
                codec_delay: Some(3_250_000),
                seek_pre_roll: Some(80_000_000),
            },
        )
        .expect("set_track_codec_timing");
    }));
    let t = dmx.track_codec_timing(0).expect("record surfaced");
    assert_eq!(t.codec_delay_explicit(), Some(3_250_000));
    assert_eq!(t.seek_pre_roll_explicit(), Some(80_000_000));
    assert!(!t.is_empty());
}

#[test]
fn explicit_zero_distinct_from_absence() {
    // An explicit 0 is on-disk and round-trips as Some(0); absence is None.
    let with_zero = mux_one(video_stream(), |mx| {
        mx.set_track_codec_timing(0, MkvTrackCodecTiming::new(Some(0), None))
            .unwrap();
    });
    assert!(
        has_id_pair(&with_zero, 0x56, 0xAA),
        "explicit CodecDelay=0 must be on disk"
    );
    assert!(
        !has_id_pair(&with_zero, 0x56, 0xBB),
        "unset SeekPreRoll must stay off disk"
    );
    let dmx = demux_typed(with_zero);
    let t = dmx.track_codec_timing(0).expect("surfaced");
    assert_eq!(t.codec_delay_explicit(), Some(0));
    assert_eq!(t.seek_pre_roll_explicit(), None);
    assert!(!t.is_empty());
}

#[test]
fn opus_auto_derivation_without_hint() {
    // pre_skip 312 @ 48 kHz -> CodecDelay 6_500_000 ns; SeekPreRoll 80 ms.
    let dmx = demux_typed(mux_one(opus_stream(), |_mx| {}));
    let t = dmx.track_codec_timing(0).expect("record surfaced");
    assert_eq!(t.codec_delay_explicit(), Some(6_500_000));
    assert_eq!(t.seek_pre_roll_explicit(), Some(80_000_000));
}

#[test]
fn explicit_hint_overrides_opus_per_field() {
    // Override only CodecDelay; SeekPreRoll keeps the auto-derived 80 ms.
    let dmx = demux_typed(mux_one(opus_stream(), |mx| {
        mx.set_track_codec_timing(0, MkvTrackCodecTiming::new(Some(1_000_000), None))
            .unwrap();
    }));
    let t = dmx.track_codec_timing(0).expect("record surfaced");
    assert_eq!(t.codec_delay_explicit(), Some(1_000_000));
    assert_eq!(
        t.seek_pre_roll_explicit(),
        Some(80_000_000),
        "unset field keeps the Opus auto value"
    );

    // Override only SeekPreRoll; CodecDelay keeps the auto-derived pre-skip.
    let dmx = demux_typed(mux_one(opus_stream(), |mx| {
        mx.set_track_codec_timing(0, MkvTrackCodecTiming::new(None, Some(12_500_000)))
            .unwrap();
    }));
    let t = dmx.track_codec_timing(0).expect("record surfaced");
    assert_eq!(t.codec_delay_explicit(), Some(6_500_000));
    assert_eq!(t.seek_pre_roll_explicit(), Some(12_500_000));
}

#[test]
fn accessor_reflects_queued_hint() {
    let tmp = tmp_path("acc");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("construct");
    assert_eq!(mx.track_codec_timing(0), None);
    mx.set_track_codec_timing(0, MkvTrackCodecTiming::new(Some(42), Some(7)))
        .unwrap();
    assert_eq!(
        mx.track_codec_timing(0),
        Some(MkvTrackCodecTiming::new(Some(42), Some(7)))
    );
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn setter_rejects_bad_state_and_index() {
    let tmp = tmp_path("err");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("construct");

    // Out-of-range stream index.
    let e = assert_err(
        mx.set_track_codec_timing(9, MkvTrackCodecTiming::default()),
        "out-of-range index",
    );
    assert!(format!("{e}").contains("out of range"), "got: {e}");

    // After write_header.
    mx.write_header().expect("write_header");
    let e = assert_err(
        mx.set_track_codec_timing(0, MkvTrackCodecTiming::default()),
        "after write_header",
    );
    assert!(format!("{e}").contains("after write_header"), "got: {e}");
    let _ = std::fs::remove_file(&tmp);
}
