//! Round-trip tests for the muxer's `ChapterDisplay > ChapLanguageBCP47`
//! write path (RFC 9559 §5.1.7.1.4.11, id `0x437D`).
//!
//! Drives `MkvMuxer::add_chapter_full` with a `ChapterDisplay` carrying a
//! BCP-47 language tag, then re-opens the bytes through the typed demuxer and
//! confirms the tag round-trips. Per §5.1.7.1.4.11, when `ChapLanguageBCP47`
//! is present the legacy `ChapLanguage` MUST be ignored, so the muxer writes
//! **only** the BCP-47 element in that case — mirroring the `LanguageBCP47` /
//! `TagLanguageBCP47` handling elsewhere in the crate.
//!
//! Contracts pinned here:
//!
//! 1. A `ChapterDisplay` with `language_bcp47 = Some(tag)` round-trips the
//!    tag through the demuxer's typed `ChapterDisplay::language_bcp47`.
//! 2. When `language_bcp47` is set, no `ChapLanguage` element is written.
//! 3. A display without `language_bcp47` still writes `ChapLanguage` and
//!    surfaces `None` for the BCP-47 tag.
//!
//! These tests use the production demuxer to walk the muxed buffer — no
//! third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mkv::mux::{ChapterDisplay, MkvChapter, MkvMuxer};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r404-chapbcp47-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

fn pcm_stream() -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn audio_packet() -> Packet {
    let mut p = Packet::new(0, TimeBase::new(1, 1000), vec![0u8; 64]);
    p.pts = Some(0);
    p.flags.keyframe = true;
    p
}

fn mux_with_display(disp: ChapterDisplay) -> Vec<u8> {
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &[pcm_stream()]).expect("muxer construct");
        mx.add_chapter_full(MkvChapter {
            time_start_ns: 0,
            time_end_ns: Some(1_000_000),
            display: vec![disp],
            ..Default::default()
        })
        .expect("add_chapter_full");
        mx.write_header().expect("write_header");
        mx.write_packet(&audio_packet()).expect("packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn demux(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open_typed")
}

/// `ChapLanguage` id 0x437C -> [0x43, 0x7C]; `ChapLanguageBCP47` id 0x437D
/// -> [0x43, 0x7D].
fn has_pair(bytes: &[u8], b0: u8, b1: u8) -> bool {
    bytes.windows(2).any(|w| w[0] == b0 && w[1] == b1)
}

#[test]
fn bcp47_tag_roundtrips_and_suppresses_chaplanguage() {
    let bytes = mux_with_display(ChapterDisplay {
        title: "Intro".into(),
        language: "eng".into(),
        country: None,
        language_bcp47: Some("en-US".into()),
    });
    assert!(
        has_pair(&bytes, 0x43, 0x7D),
        "ChapLanguageBCP47 must be on disk"
    );
    assert!(
        !has_pair(&bytes, 0x43, 0x7C),
        "ChapLanguage must be suppressed when ChapLanguageBCP47 is present"
    );
    let dmx = demux(bytes);
    let editions = dmx.chapters();
    assert_eq!(editions.len(), 1);
    let disp = &editions[0].chapters[0].displays[0];
    assert_eq!(disp.language_bcp47.as_deref(), Some("en-US"));
    assert_eq!(disp.string, "Intro");
}

#[test]
fn no_bcp47_writes_chaplanguage_and_surfaces_none() {
    let bytes = mux_with_display(ChapterDisplay {
        title: "Verse".into(),
        language: "jpn".into(),
        country: Some("jp".into()),
        language_bcp47: None,
    });
    assert!(
        has_pair(&bytes, 0x43, 0x7C),
        "ChapLanguage must be written when no BCP-47 tag is set"
    );
    assert!(
        !has_pair(&bytes, 0x43, 0x7D),
        "ChapLanguageBCP47 must be off disk when unset"
    );
    let dmx = demux(bytes);
    let disp = &dmx.chapters()[0].chapters[0].displays[0];
    assert_eq!(disp.language, "jpn");
    assert_eq!(disp.language_bcp47, None);
    assert_eq!(disp.country.as_deref(), Some("jp"));
}
