//! Round-trip tests for the muxer's full `ChapterAtom` surface — the atom
//! fields beyond start/end/display (`ChapterUID`, `ChapterStringUID`,
//! `ChapterFlagHidden`, `ChapterFlagEnabled`, `ChapterSegmentUUID`,
//! `ChapterSegmentEditionUID`, `ChapterPhysicalEquiv`) and the
//! `ChapProcess > ChapProcessCommand` chapter-codec command tree (RFC 9559
//! §5.1.7.1.4.14..§5.1.7.1.4.19).
//!
//! Drives `MkvMuxer::add_chapter_full` with a fully-specified
//! [`MkvChapter`], re-opens the bytes through the typed
//! [`oxideav_mkv::demux::open_typed`], and confirms the typed
//! `MkvDemuxer::chapters()` tree surfaces every field the muxer wrote.
//!
//! These tests use the production demuxer — no third-party Matroska code
//! is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mkv::mux::{
    ChapterDisplay, MkvChapProcess, MkvChapProcessCommand, MkvChapter, MkvMuxer,
};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path() -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r341-chapproc-{}-{n}.mkv",
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

fn mux_chapters(chapters: Vec<MkvChapter>) -> Vec<u8> {
    let tmp = tmp_path();
    {
        let f = std::fs::File::create(&tmp).expect("create");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &[pcm_stream()]).expect("muxer");
        for ch in chapters {
            mx.add_chapter_full(ch).expect("add_chapter_full");
        }
        mx.write_header().expect("write_header");
        let mut pkt = Packet::new(0, TimeBase::new(1, 1000), vec![0xAA; 32]);
        pkt.pts = Some(0);
        pkt.flags.keyframe = true;
        mx.write_packet(&pkt).expect("write_packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("read");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn demux_typed(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open_typed")
}

#[test]
fn full_atom_fields_round_trip() {
    let bytes = mux_chapters(vec![MkvChapter {
        time_start_ns: 1_000_000_000,
        time_end_ns: Some(2_000_000_000),
        uid: Some(0xABCD),
        string_uid: Some("cue-7".into()),
        hidden: true,
        enabled: false,
        segment_uuid: Some(vec![0x11; 16]),
        segment_edition_uid: Some(42),
        physical_equiv: Some(60),
        display: vec![ChapterDisplay {
            title: "Scene".into(),
            language: "eng".into(),
            country: None,
            language_bcp47: None,
        }],
        chap_processes: Vec::new(),
    }]);

    let dmx = demux_typed(bytes);
    let editions = dmx.chapters();
    assert_eq!(editions.len(), 1);
    let chapter = &editions[0].chapters[0];
    assert_eq!(chapter.uid, Some(0xABCD));
    assert_eq!(chapter.string_uid.as_deref(), Some("cue-7"));
    assert_eq!(chapter.time_start_ns, 1_000_000_000);
    assert_eq!(chapter.time_end_ns, Some(2_000_000_000));
    assert!(chapter.hidden, "ChapterFlagHidden round-trips");
    assert!(!chapter.enabled, "ChapterFlagEnabled=0 round-trips");
    assert_eq!(chapter.segment_uuid.as_deref(), Some(&[0x11; 16][..]));
    assert_eq!(chapter.segment_edition_uid, Some(42));
    assert_eq!(chapter.physical_equiv, Some(60));
    assert_eq!(chapter.displays[0].string, "Scene");
}

#[test]
fn chap_process_tree_round_trips() {
    let bytes = mux_chapters(vec![MkvChapter {
        time_start_ns: 0,
        time_end_ns: None,
        display: vec![ChapterDisplay {
            title: "Menu".into(),
            language: "und".into(),
            country: None,
            language_bcp47: None,
        }],
        chap_processes: vec![
            // DVD-menu codec with private data and two timed commands.
            MkvChapProcess {
                codec_id: 1,
                private: Some(vec![0xDE, 0xAD]),
                commands: vec![
                    MkvChapProcessCommand {
                        time: 1,
                        data: vec![0xCA, 0xFE],
                    },
                    MkvChapProcessCommand {
                        time: 2,
                        data: vec![0xBE, 0xEF, 0x00],
                    },
                ],
            },
            // Matroska-Script codec (id 0), no private, no commands.
            MkvChapProcess {
                codec_id: 0,
                private: None,
                commands: Vec::new(),
            },
        ],
        ..Default::default()
    }]);

    let dmx = demux_typed(bytes);
    let chapter = &dmx.chapters()[0].chapters[0];
    assert_eq!(chapter.chap_processes.len(), 2);

    let p0 = &chapter.chap_processes[0];
    assert_eq!(p0.codec_id, 1);
    assert_eq!(p0.private.as_deref(), Some(&[0xDE, 0xAD][..]));
    assert_eq!(p0.commands.len(), 2);
    assert_eq!(p0.commands[0].time, 1);
    assert_eq!(p0.commands[0].data, vec![0xCA, 0xFE]);
    assert_eq!(p0.commands[1].time, 2);
    assert_eq!(p0.commands[1].data, vec![0xBE, 0xEF, 0x00]);

    let p1 = &chapter.chap_processes[1];
    assert_eq!(p1.codec_id, 0);
    assert_eq!(p1.private, None);
    assert!(p1.commands.is_empty());
}

#[test]
fn default_chapter_enabled_and_no_optional_fields() {
    // A default-ish chapter: enabled (default true), not hidden, no
    // optional UIDs. The demuxer materialises the enabled default.
    let bytes = mux_chapters(vec![MkvChapter {
        time_start_ns: 0,
        time_end_ns: Some(500_000_000),
        display: vec![ChapterDisplay {
            title: "Plain".into(),
            language: "eng".into(),
            country: None,
            language_bcp47: None,
        }],
        ..Default::default()
    }]);

    let dmx = demux_typed(bytes);
    let chapter = &dmx.chapters()[0].chapters[0];
    assert!(chapter.enabled, "absent ChapterFlagEnabled ⇒ default true");
    assert!(!chapter.hidden);
    assert!(chapter.string_uid.is_none());
    assert!(chapter.segment_uuid.is_none());
    assert!(chapter.physical_equiv.is_none());
    assert!(chapter.chap_processes.is_empty());
    // UID auto-derived (non-zero).
    assert!(chapter.uid.is_some() && chapter.uid != Some(0));
}

#[test]
fn zero_chapter_uid_is_rejected() {
    let tmp = tmp_path();
    let f = std::fs::File::create(&tmp).expect("create");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[pcm_stream()]).expect("muxer");
    let err = mx
        .add_chapter_full(MkvChapter {
            uid: Some(0),
            ..Default::default()
        })
        .expect_err("ChapterUID 0 must be rejected");
    let _ = std::fs::remove_file(&tmp);
    assert!(format!("{err}").contains("ChapterUID"));
}

#[test]
fn non_16_byte_segment_uuid_is_rejected() {
    let tmp = tmp_path();
    let f = std::fs::File::create(&tmp).expect("create");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[pcm_stream()]).expect("muxer");
    let err = mx
        .add_chapter_full(MkvChapter {
            segment_uuid: Some(vec![0x00; 8]),
            ..Default::default()
        })
        .expect_err("8-byte SegmentUUID must be rejected");
    let _ = std::fs::remove_file(&tmp);
    assert!(format!("{err}").contains("ChapterSegmentUUID"));
}
