//! Round-trip tests for the muxer's TrackEntry audience-flag write path
//! (RFC 9559 §5.1.4.1.6..§5.1.4.1.11 — `FlagForced` /
//! `FlagHearingImpaired` / `FlagVisualImpaired` / `FlagTextDescriptions`
//! / `FlagOriginal` / `FlagCommentary`).
//!
//! Drives `MkvMuxer::set_track_audience_flags` against the public Muxer
//! trait, then re-opens the bytes through the typed
//! [`oxideav_mkv::demux::open_typed`] and confirms
//! `MkvDemuxer::track_audience_flags(stream_index)` decodes exactly the
//! record handed to the muxer — including the load-bearing
//! `Some(false)`-vs-absent distinction on the five default-less
//! `minver: 4` flags.
//!
//! Spec contracts pinned here:
//!
//! 1. `forced: Some(true)` on a subtitle track surfaces back as
//!    `forced() == true` (§5.1.4.1.6).
//! 2. `forced: Some(false)` writes the element on disk (id `0x55AA`
//!    present) even though it decodes identically to absence — the
//!    explicit-override path.
//! 3. Omitting the call writes none of the six element ids, so the
//!    demuxer materialises the §5.1.4.1.6 default `false` and `None`
//!    for the five `minver: 4` flags.
//! 4. All five `minver: 4` flags set `Some(true)` round-trip
//!    independently and fire `is_accessibility()` (§5.1.4.1.7..§5.1.4.1.9).
//! 5. An explicit `Some(false)` on a `minver: 4` flag round-trips as
//!    `Some(false)` — distinct from `None` (the §5.1.4.1.7..§5.1.4.1.11
//!    "set to 1 if and only if" wording makes explicit zero a stronger
//!    signal than silence).
//! 6. There is NO track-type restriction: audio and video tracks accept
//!    the call (the spec carries the elements on every `TrackEntry`).
//! 7. The setter rejects calls made after `write_header` and
//!    out-of-range stream indices.
//! 8. Repeated calls are last-write-wins; queueing the all-`None`
//!    default record is a functional no-op.
//!
//! These tests use plain byte scans / the production demuxer to inspect
//! the muxed buffer — no third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mkv::mux::{MkvMuxer, MkvTrackAudienceFlags};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r279-audflags-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

fn video_stream(index: u32) -> StreamInfo {
    let mut p = CodecParameters::video(CodecId::new("vp9"));
    p.width = Some(320);
    p.height = Some(240);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn audio_stream(index: u32) -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn subtitle_stream(index: u32) -> StreamInfo {
    let p = CodecParameters::subtitle(CodecId::new("subrip"));
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn keyframe_packet(stream: u32, pts: i64, marker: u8, len: usize) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![marker; len]);
    p.pts = Some(pts);
    p.flags.keyframe = true;
    p
}

/// Mux the given streams into an MKV. `configure` is invoked between
/// constructing the muxer and `write_header` so the test can queue
/// audience flags (or not). One keyframe packet is written on stream 0.
/// Returns the muxed file's bytes.
fn mux_with<F>(streams: &[StreamInfo], configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, streams).expect("muxer construct");
        configure(&mut mx);
        mx.write_header().expect("write_header");
        mx.write_packet(&keyframe_packet(0, 0, 0x11, 32))
            .expect("write_packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

/// Open `bytes` through the typed demuxer entry point so we can reach
/// `MkvDemuxer::track_audience_flags`.
fn demux_typed(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open_typed")
}

/// True when the 2-byte element id appears anywhere in `bytes`. Audience
/// flag ids are all 2-byte (`0x55AA`..`0x55AF`). Packet payloads in these
/// tests repeat a single marker byte, so a window of two *different*
/// bytes can never originate inside frame data.
fn contains_id2(bytes: &[u8], id: u16) -> bool {
    let hi = (id >> 8) as u8;
    let lo = (id & 0xFF) as u8;
    bytes.windows(2).any(|w| w[0] == hi && w[1] == lo)
}

const ALL_FLAG_IDS: [u16; 6] = [0x55AA, 0x55AB, 0x55AC, 0x55AD, 0x55AE, 0x55AF];

#[test]
fn roundtrip_forced_subtitle() {
    // §5.1.4.1.6: a forced subtitle track. Stream 1 carries the flag;
    // stream 0 is video so the file has a packet-bearing track.
    let streams = [video_stream(0), subtitle_stream(1)];
    let bytes = mux_with(&streams, |mx| {
        mx.set_track_audience_flags(1, MkvTrackAudienceFlags::forced_subtitle())
            .expect("set_track_audience_flags");
    });
    assert!(
        contains_id2(&bytes, 0x55AA),
        "FlagForced id must be on disk"
    );
    let dmx = demux_typed(bytes);
    let af = dmx.track_audience_flags(1).expect("record for stream 1");
    assert!(af.forced(), "forced flag must round-trip true");
    assert!(!af.is_default_presentation());
    // The other five flags were never written → absent → None.
    assert_eq!(af.hearing_impaired(), None);
    assert_eq!(af.commentary(), None);
}

#[test]
fn forced_explicit_zero_writes_element() {
    // §5.1.4.1.6 default is 0, so `Some(false)` decodes the same as
    // absence — but the element must still reach disk (the explicit way
    // to override a downstream tool that might infer something else).
    let streams = [video_stream(0)];
    let bytes = mux_with(&streams, |mx| {
        mx.set_track_audience_flags(
            0,
            MkvTrackAudienceFlags {
                forced: Some(false),
                ..Default::default()
            },
        )
        .expect("set_track_audience_flags");
    });
    assert!(
        contains_id2(&bytes, 0x55AA),
        "explicit FlagForced=0 must still write the element id"
    );
    let dmx = demux_typed(bytes);
    let af = dmx.track_audience_flags(0).expect("record");
    assert!(!af.forced());
    assert!(af.is_default_presentation());
}

#[test]
fn omitted_call_writes_no_flag_ids() {
    // Contract 3: no call → none of the six ids on disk → the demuxer
    // materialises the spec default for FlagForced and None for the
    // minver-4 five.
    let streams = [video_stream(0)];
    let bytes = mux_with(&streams, |_mx| {});
    for id in ALL_FLAG_IDS {
        assert!(
            !contains_id2(&bytes, id),
            "audience flag id {id:#06X} must not appear when the API was never called"
        );
    }
    let dmx = demux_typed(bytes);
    let af = dmx.track_audience_flags(0).expect("record always surfaces");
    assert!(!af.forced(), "§5.1.4.1.6 default 0 materialised");
    assert_eq!(af.hearing_impaired(), None);
    assert_eq!(af.visual_impaired(), None);
    assert_eq!(af.text_descriptions(), None);
    assert_eq!(af.original(), None);
    assert_eq!(af.commentary(), None);
    assert!(af.is_default_presentation());
    assert!(!af.is_accessibility());
}

#[test]
fn roundtrip_all_five_minver4_true() {
    let streams = [video_stream(0), audio_stream(1)];
    let bytes = mux_with(&streams, |mx| {
        mx.set_track_audience_flags(
            1,
            MkvTrackAudienceFlags {
                forced: None,
                hearing_impaired: Some(true),
                visual_impaired: Some(true),
                text_descriptions: Some(true),
                original: Some(true),
                commentary: Some(true),
            },
        )
        .expect("set_track_audience_flags");
    });
    // FlagForced stayed None → its id must NOT be on disk; the five
    // minver-4 ids must all be present.
    assert!(!contains_id2(&bytes, 0x55AA));
    for id in &ALL_FLAG_IDS[1..] {
        assert!(contains_id2(&bytes, *id), "id {id:#06X} must be on disk");
    }
    let dmx = demux_typed(bytes);
    let af = dmx.track_audience_flags(1).expect("record");
    assert!(!af.forced(), "absent FlagForced decodes the spec default");
    assert_eq!(af.hearing_impaired(), Some(true));
    assert_eq!(af.visual_impaired(), Some(true));
    assert_eq!(af.text_descriptions(), Some(true));
    assert_eq!(af.original(), Some(true));
    assert_eq!(af.commentary(), Some(true));
    assert!(af.is_accessibility());
    assert!(!af.is_default_presentation());
}

#[test]
fn explicit_false_distinct_from_absent() {
    // Contract 5: §5.1.4.1.7's "if and only if" makes an explicit 0 a
    // stronger signal than silence. `Some(false)` must round-trip as
    // `Some(false)` while an untouched slot stays `None`.
    let streams = [video_stream(0)];
    let bytes = mux_with(&streams, |mx| {
        mx.set_track_audience_flags(
            0,
            MkvTrackAudienceFlags {
                hearing_impaired: Some(false),
                ..Default::default()
            },
        )
        .expect("set_track_audience_flags");
    });
    assert!(
        contains_id2(&bytes, 0x55AB),
        "explicit 0 still writes the id"
    );
    let dmx = demux_typed(bytes);
    let af = dmx.track_audience_flags(0).expect("record");
    assert_eq!(
        af.hearing_impaired(),
        Some(false),
        "explicit zero must surface as Some(false), not None"
    );
    assert_eq!(af.visual_impaired(), None, "untouched slot stays absent");
    // An explicit false is still a default presentation and not an
    // accessibility track.
    assert!(af.is_default_presentation());
    assert!(!af.is_accessibility());
}

#[test]
fn accepted_on_audio_and_video_tracks() {
    // Contract 6: no media-type restriction — the spec carries the six
    // elements on every TrackEntry. Audio commentary in the content's
    // original language is the canonical audio-track use.
    let streams = [video_stream(0), audio_stream(1)];
    let bytes = mux_with(&streams, |mx| {
        mx.set_track_audience_flags(
            0,
            MkvTrackAudienceFlags {
                original: Some(true),
                ..Default::default()
            },
        )
        .expect("video track accepts audience flags");
        mx.set_track_audience_flags(
            1,
            MkvTrackAudienceFlags {
                original: Some(true),
                commentary: Some(true),
                ..Default::default()
            },
        )
        .expect("audio track accepts audience flags");
    });
    let dmx = demux_typed(bytes);
    let v = dmx.track_audience_flags(0).expect("video record");
    assert_eq!(v.original(), Some(true));
    assert_eq!(v.commentary(), None);
    let a = dmx.track_audience_flags(1).expect("audio record");
    assert_eq!(a.original(), Some(true));
    assert_eq!(a.commentary(), Some(true));
}

#[test]
fn multi_track_records_are_independent() {
    // Flags queued on stream 0 only — stream 1's record must stay at
    // the all-default decode.
    let streams = [video_stream(0), audio_stream(1)];
    let bytes = mux_with(&streams, |mx| {
        mx.set_track_audience_flags(0, MkvTrackAudienceFlags::visual_impaired_track())
            .expect("set_track_audience_flags");
    });
    let dmx = demux_typed(bytes);
    let v = dmx.track_audience_flags(0).expect("stream 0 record");
    assert_eq!(v.visual_impaired(), Some(true));
    let a = dmx.track_audience_flags(1).expect("stream 1 record");
    assert_eq!(a.visual_impaired(), None);
    assert!(a.is_default_presentation());
}

#[test]
fn empty_record_is_a_noop() {
    // Contract 8 (second half): queueing the all-None default record is
    // legal and writes nothing.
    let flags = MkvTrackAudienceFlags::default();
    assert!(flags.is_empty());
    let streams = [video_stream(0)];
    let bytes = mux_with(&streams, |mx| {
        mx.set_track_audience_flags(0, flags)
            .expect("set_track_audience_flags");
    });
    for id in ALL_FLAG_IDS {
        assert!(
            !contains_id2(&bytes, id),
            "all-None record must keep id {id:#06X} off-disk"
        );
    }
    let dmx = demux_typed(bytes);
    let af = dmx.track_audience_flags(0).expect("record");
    assert!(af.is_default_presentation());
}

#[test]
fn last_write_wins() {
    let streams = [video_stream(0)];
    let bytes = mux_with(&streams, |mx| {
        mx.set_track_audience_flags(0, MkvTrackAudienceFlags::commentary_track())
            .expect("first call");
        mx.set_track_audience_flags(0, MkvTrackAudienceFlags::hearing_impaired_track())
            .expect("second call overwrites");
    });
    let dmx = demux_typed(bytes);
    let af = dmx.track_audience_flags(0).expect("record");
    assert_eq!(af.hearing_impaired(), Some(true));
    assert_eq!(
        af.commentary(),
        None,
        "overwritten record must not leak the first call's flag"
    );
}

#[test]
fn accessor_returns_queued_value() {
    let tmp = tmp_path("acc");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = [video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer construct");
    assert_eq!(mx.track_audience_flags(0), None, "nothing queued yet");
    let queued = MkvTrackAudienceFlags {
        forced: Some(true),
        original: Some(false),
        ..Default::default()
    };
    mx.set_track_audience_flags(0, queued).expect("queue");
    assert_eq!(mx.track_audience_flags(0), Some(queued));
    assert_eq!(mx.track_audience_flags(7), None, "out of range → None");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn reject_after_write_header() {
    let tmp = tmp_path("late");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = [video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer construct");
    mx.write_header().expect("write_header");
    let err = mx
        .set_track_audience_flags(0, MkvTrackAudienceFlags::forced_subtitle())
        .map(|_| ())
        .expect_err("must reject post-write_header");
    assert!(matches!(err, Error::Other(_)), "got {err:?}");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn reject_out_of_range_stream_index() {
    let tmp = tmp_path("oor");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = [video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer construct");
    let err = mx
        .set_track_audience_flags(3, MkvTrackAudienceFlags::forced_subtitle())
        .map(|_| ())
        .expect_err("must reject out-of-range index");
    assert!(matches!(err, Error::InvalidData(_)), "got {err:?}");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn convenience_constructor_shapes() {
    let f = MkvTrackAudienceFlags::forced_subtitle();
    assert_eq!(f.forced, Some(true));
    assert_eq!(f.hearing_impaired, None);

    let h = MkvTrackAudienceFlags::hearing_impaired_track();
    assert_eq!(h.hearing_impaired, Some(true));
    assert_eq!(h.forced, None);

    let v = MkvTrackAudienceFlags::visual_impaired_track();
    assert_eq!(v.visual_impaired, Some(true));

    let c = MkvTrackAudienceFlags::commentary_track();
    assert_eq!(c.commentary, Some(true));
    assert!(!c.is_empty());
    assert!(MkvTrackAudienceFlags::default().is_empty());
}
