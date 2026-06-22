//! Round-trip tests for the muxer's `TrackEntry` identity / selection write
//! path (RFC 9559 §5.1.4.1.18 / .19 / .20 / .23 / .4 / .5 / .12 / .24 —
//! `Name`, `Language`, `LanguageBCP47`, `CodecName`, `FlagEnabled`,
//! `FlagDefault`, `FlagLacing`, `AttachmentLink`).
//!
//! Drives [`MkvMuxer::set_track_identity`] against the public Muxer trait,
//! then re-opens the bytes through [`oxideav_mkv::demux::open_typed`] and
//! confirms [`oxideav_mkv::demux::MkvDemuxer::track_identity`] decodes the
//! exact children handed to the muxer.
//!
//! Spec contracts pinned here:
//!
//! 1. Each explicit child round-trips through the demux-side `TrackIdentity`.
//! 2. Omitting `set_track_identity` keeps the optional strings / link
//!    off-disk and materialises the §default `1` selection flags.
//! 3. `LanguageBCP47` supersedes `Language` — when both are set the muxer
//!    writes only `LanguageBCP47` and the demuxer's `language()` returns it
//!    (§5.1.4.1.20).
//! 4. A hint `language` overrides the `StreamInfo`-derived `Language`.
//! 5. The setter rejects calls after `write_header`, out-of-range stream
//!    indices, empty strings, and `attachment_link == Some(0)`.
//!
//! These tests use the production demuxer to walk the muxed buffer — no
//! third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::mux::{MkvMuxer, MkvTrackIdentity};

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
        "oxideav-mkv-r360-trackident-{}-{}-{n}.mkv",
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

/// A video stream pre-loaded with a `StreamInfo`-level Language so the
/// override / precedence behaviour can be exercised.
fn video_stream_with_language(lang: &str) -> StreamInfo {
    let mut s = video_stream();
    s.params.language = Some(lang.to_string());
    s
}

fn video_packet(stream: u32, pts: i64, len: usize) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![0x42; len]);
    p.pts = Some(pts);
    p.flags.keyframe = true;
    p
}

/// Mux a single-track video MKV. `configure` runs between constructing the
/// muxer and `write_header`. Returns the muxed bytes.
fn mux_with<F>(stream: StreamInfo, configure: F) -> Vec<u8>
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

#[test]
fn omitted_call_surfaces_default_record() {
    // With no hint, the optional strings / link stay off-disk and the demuxer
    // materialises the §default `1` for the three selection flags.
    let dmx = demux_typed(mux_with(video_stream(), |_mx| {}));
    let id = dmx.track_identity(0).expect("track_identity surfaced");
    assert_eq!(id.name(), None);
    assert_eq!(id.codec_name(), None);
    assert_eq!(id.language(), None);
    assert_eq!(id.attachment_link(), None);
    assert!(id.enabled());
    assert!(id.default());
    // The muxer is constructed with LacingMode::None, so FlagLacing is written
    // as an explicit 0 — observable on the typed surface.
    assert_eq!(id.lacing_allowed_explicit(), Some(false));
    assert!(!id.lacing_allowed());
}

#[test]
fn name_and_codec_name_roundtrip() {
    let dmx = demux_typed(mux_with(video_stream(), |mx| {
        mx.set_track_identity(
            0,
            MkvTrackIdentity {
                name: Some("Main video".to_string()),
                codec_name: Some("VP9".to_string()),
                ..Default::default()
            },
        )
        .expect("set_track_identity");
    }));
    let id = dmx.track_identity(0).expect("surfaced");
    assert_eq!(id.name(), Some("Main video"));
    assert_eq!(id.codec_name(), Some("VP9"));
}

#[test]
fn flags_roundtrip() {
    let dmx = demux_typed(mux_with(video_stream(), |mx| {
        mx.set_track_identity(
            0,
            MkvTrackIdentity {
                flag_enabled: Some(false),
                flag_default: Some(false),
                flag_lacing: Some(true),
                ..Default::default()
            },
        )
        .expect("set_track_identity");
    }));
    let id = dmx.track_identity(0).expect("surfaced");
    assert_eq!(id.enabled_explicit(), Some(false));
    assert_eq!(id.default_explicit(), Some(false));
    // The hint's flag_lacing overrides the auto-derived LacingMode::None 0.
    assert_eq!(id.lacing_allowed_explicit(), Some(true));
    assert!(id.lacing_allowed());
}

#[test]
fn language_matroska_override() {
    // The StreamInfo carries "eng"; the hint overrides it to "fre".
    let dmx = demux_typed(mux_with(video_stream_with_language("eng"), |mx| {
        mx.set_track_identity(
            0,
            MkvTrackIdentity {
                language: Some("fre".to_string()),
                ..Default::default()
            },
        )
        .expect("set_track_identity");
    }));
    let id = dmx.track_identity(0).expect("surfaced");
    assert_eq!(id.language_matroska(), Some("fre"));
    assert_eq!(id.language_bcp47(), None);
    assert_eq!(id.language(), Some("fre"));
}

#[test]
fn language_bcp47_supersedes() {
    // Both Language (from StreamInfo) and the hint's LanguageBCP47 are present;
    // the muxer must write only LanguageBCP47 (§5.1.4.1.20).
    let dmx = demux_typed(mux_with(video_stream_with_language("ger"), |mx| {
        mx.set_track_identity(
            0,
            MkvTrackIdentity {
                language: Some("ger".to_string()),
                language_bcp47: Some("de-AT".to_string()),
                ..Default::default()
            },
        )
        .expect("set_track_identity");
    }));
    let id = dmx.track_identity(0).expect("surfaced");
    // Language element MUST have been suppressed.
    assert_eq!(id.language_matroska(), None);
    assert_eq!(id.language_bcp47(), Some("de-AT"));
    assert!(id.uses_bcp47());
    assert_eq!(id.language(), Some("de-AT"));
}

#[test]
fn attachment_link_roundtrip() {
    let dmx = demux_typed(mux_with(video_stream(), |mx| {
        mx.set_track_identity(
            0,
            MkvTrackIdentity {
                attachment_link: Some(0x1234),
                ..Default::default()
            },
        )
        .expect("set_track_identity");
    }));
    let id = dmx.track_identity(0).expect("surfaced");
    assert_eq!(id.attachment_link(), Some(0x1234));
}

#[test]
fn full_record_roundtrip() {
    let dmx = demux_typed(mux_with(video_stream(), |mx| {
        mx.set_track_identity(
            0,
            MkvTrackIdentity {
                name: Some("Commentary".to_string()),
                codec_name: Some("VP9 / WebM".to_string()),
                language: Some("eng".to_string()),
                language_bcp47: Some("en-GB".to_string()),
                flag_enabled: Some(true),
                flag_default: Some(false),
                flag_lacing: Some(false),
                attachment_link: Some(7),
            },
        )
        .expect("set_track_identity");
    }));
    let id = dmx.track_identity(0).expect("surfaced");
    assert_eq!(id.name(), Some("Commentary"));
    assert_eq!(id.codec_name(), Some("VP9 / WebM"));
    assert_eq!(id.language(), Some("en-GB"));
    assert_eq!(id.language_matroska(), None); // suppressed by BCP-47
    assert_eq!(id.language_bcp47(), Some("en-GB"));
    assert_eq!(id.enabled_explicit(), Some(true));
    assert_eq!(id.default_explicit(), Some(false));
    assert_eq!(id.lacing_allowed_explicit(), Some(false));
    assert_eq!(id.attachment_link(), Some(7));
}

#[test]
fn constructors() {
    assert_eq!(MkvTrackIdentity::named("foo").name.as_deref(), Some("foo"));
    assert_eq!(
        MkvTrackIdentity::language_bcp47("pt-BR")
            .language_bcp47
            .as_deref(),
        Some("pt-BR")
    );
    assert_eq!(MkvTrackIdentity::non_default().flag_default, Some(false));
}

#[test]
fn rejects_empty_strings_and_zero_link() {
    let tmp = tmp_path("err");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer construct");

    let e = assert_err(
        mx.set_track_identity(
            0,
            MkvTrackIdentity {
                name: Some(String::new()),
                ..Default::default()
            },
        ),
        "empty name",
    );
    assert!(format!("{e}").contains("empty string"), "got: {e}");

    let e = assert_err(
        mx.set_track_identity(
            0,
            MkvTrackIdentity {
                attachment_link: Some(0),
                ..Default::default()
            },
        ),
        "zero attachment_link",
    );
    assert!(format!("{e}").contains("not 0"), "got: {e}");

    let e = assert_err(
        mx.set_track_identity(1, MkvTrackIdentity::named("x")),
        "out of range",
    );
    assert!(format!("{e}").contains("out of range"), "got: {e}");

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_call_after_write_header() {
    let tmp = tmp_path("posthdr");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer construct");
    mx.write_header().expect("write_header");
    let e = assert_err(
        mx.set_track_identity(0, MkvTrackIdentity::named("late")),
        "after write_header",
    );
    assert!(format!("{e}").contains("write_header"), "got: {e}");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn readback_accessor() {
    let tmp = tmp_path("readback");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer construct");
    assert!(mx.track_identity(0).is_none());
    mx.set_track_identity(0, MkvTrackIdentity::named("hello"))
        .expect("set");
    assert_eq!(
        mx.track_identity(0).and_then(|id| id.name.as_deref()),
        Some("hello")
    );
    let _ = std::fs::remove_file(&tmp);
}
