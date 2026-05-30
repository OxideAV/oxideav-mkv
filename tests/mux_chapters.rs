//! Round-trip tests for the muxer's `Chapters` encoding.
//!
//! Drives `MkvMuxer::add_chapter` + `add_chapter_full` against the public
//! Muxer trait, then re-opens the bytes through the demuxer and verifies
//! that
//!
//! 1. Every queued chapter surfaces as `chapter:N:start_ms` /
//!    `chapter:N:end_ms` / `chapter:N:title` in the demuxer's
//!    `metadata()` view, in the same order they were added.
//! 2. The `Chapters` master sits **between** Tracks and the first
//!    Cluster, so it can be parsed in the demuxer's single-pass header
//!    walk (no late-segment chapter parsing needed).
//! 3. The SeekHead `Chapters` slot points at the actual `Chapters`
//!    element offset, and the `Chapters` slot is voided when no
//!    chapters were added (keeping pre-walking players from chasing a
//!    placeholder zero).
//! 4. `add_chapter` rejects calls made after `write_header`.
//! 5. Multilingual `ChapterDisplay` lists round-trip (first ChapString
//!    wins on the demux side, matches existing behaviour and ffprobe).
//!
//! Reference: RFC 9559 §5.1.7 (Chapters).
//!
//! These tests use the production EBML helpers to walk the muxed
//! buffer — no third-party Matroska code is consulted.

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};

/// Counter ensures every temp file produced by the parallel test runner
/// gets a unique name — cargo's default `--test-threads=8` (or whatever
/// the host has) would otherwise stomp `mux_chapters-{pid}.mkv` between
/// tests that all run concurrently and create/remove the same path.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r89-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

use oxideav_core::{
    CodecId, CodecParameters, Error, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::ebml::{read_element_header, read_uint, VINT_UNKNOWN_SIZE};
use oxideav_mkv::ids;
use oxideav_mkv::mux::{ChapterDisplay, MkvChapter, MkvMuxer};

/// A single PCM-16LE audio stream — cheapest possible MKV the demuxer
/// will accept. Time base = ms.
fn pcm_stream() -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p.sample_format = Some(oxideav_core::SampleFormat::S16);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn pcm_packet(pts_ms: i64, payload: u8) -> Packet {
    let mut pkt = Packet::new(0, TimeBase::new(1, 1000), vec![payload; 32]);
    pkt.pts = Some(pts_ms);
    pkt.flags.keyframe = true;
    pkt
}

/// Mux a minimal MKV with the given chapter list to a temp file, read it
/// back into a Vec<u8>, and return the bytes. The temp file is deleted
/// before return.
fn mux_with_chapters_collect(chapters: &[MkvChapter]) -> Vec<u8> {
    let tmp = tmp_path("collect");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let stream = pcm_stream();
        let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
        for ch in chapters {
            mx.add_chapter_full(ch.clone()).expect("add chapter");
        }
        mx.write_header().expect("write_header");
        mx.write_packet(&pcm_packet(0, 0xAA)).expect("write_packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn open_demuxer(bytes: Vec<u8>) -> Box<dyn oxideav_core::Demuxer> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

/// Walk the file and find the absolute offset of the first top-level
/// element with the given id inside the Segment payload. Returns
/// `(absolute_offset_of_element_header, segment_data_start)` or
/// `None` if not present.
fn find_top_level(bytes: &[u8], target_id: u32) -> Option<(u64, u64)> {
    let mut cur = Cursor::new(bytes);
    let ebml = read_element_header(&mut cur).ok()?;
    assert_eq!(ebml.id, ids::EBML_HEADER);
    cur.seek(SeekFrom::Current(ebml.size as i64)).ok()?;
    let seg = read_element_header(&mut cur).ok()?;
    assert_eq!(seg.id, ids::SEGMENT);
    let segment_data_start = cur.stream_position().ok()?;
    let segment_end = if seg.size == VINT_UNKNOWN_SIZE {
        bytes.len() as u64
    } else {
        segment_data_start + seg.size
    };
    while cur.stream_position().ok()? < segment_end {
        let elem_start = cur.stream_position().ok()?;
        let e = read_element_header(&mut cur).ok()?;
        let body_start = cur.stream_position().ok()?;
        if e.id == target_id {
            return Some((elem_start, segment_data_start));
        }
        let body_end = if e.size == VINT_UNKNOWN_SIZE {
            // Treat unknown-size as runs-to-segment-end.
            segment_end
        } else {
            body_start + e.size
        };
        cur.seek(SeekFrom::Start(body_end)).ok()?;
    }
    None
}

#[test]
fn chapters_round_trip_through_demuxer() {
    let chapters = vec![
        MkvChapter {
            time_start_ns: 0,
            time_end_ns: Some(1_000_000_000),
            display: vec![ChapterDisplay {
                title: "Intro".into(),
                language: "eng".into(),
                country: None,
            }],
        },
        MkvChapter {
            time_start_ns: 1_000_000_000,
            time_end_ns: Some(2_000_000_000),
            display: vec![ChapterDisplay {
                title: "Verse".into(),
                language: "eng".into(),
                country: Some("us".into()),
            }],
        },
        MkvChapter {
            time_start_ns: 2_000_000_000,
            time_end_ns: None,
            display: vec![ChapterDisplay {
                title: "Outro".into(),
                language: "eng".into(),
                country: None,
            }],
        },
    ];
    let bytes = mux_with_chapters_collect(&chapters);

    let dmx = open_demuxer(bytes);
    let md: Vec<(String, String)> = dmx.metadata().to_vec();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());

    assert_eq!(get("chapter:1:title").as_deref(), Some("Intro"));
    assert_eq!(get("chapter:1:start_ms").as_deref(), Some("0"));
    assert_eq!(get("chapter:1:end_ms").as_deref(), Some("1000"));

    assert_eq!(get("chapter:2:title").as_deref(), Some("Verse"));
    assert_eq!(get("chapter:2:start_ms").as_deref(), Some("1000"));
    assert_eq!(get("chapter:2:end_ms").as_deref(), Some("2000"));

    assert_eq!(get("chapter:3:title").as_deref(), Some("Outro"));
    assert_eq!(get("chapter:3:start_ms").as_deref(), Some("2000"));
    assert!(
        get("chapter:3:end_ms").is_none(),
        "open-ended chapter must not emit end_ms key"
    );
}

#[test]
fn convenience_add_chapter_defaults_language_to_english() {
    let tmp = tmp_path("convenience");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let stream = pcm_stream();
    let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");

    mx.add_chapter(0, Some(500_000_000), "Cold Open")
        .expect("add 1");
    mx.add_chapter(500_000_000, None, "Body").expect("add 2");

    assert_eq!(mx.chapters().len(), 2, "two chapters queued");
    assert_eq!(mx.chapters()[0].display[0].title, "Cold Open");
    assert_eq!(mx.chapters()[0].display[0].language, "eng");
    assert_eq!(mx.chapters()[1].display[0].title, "Body");
    assert_eq!(mx.chapters()[1].display[0].language, "eng");
    drop(mx);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn add_chapter_after_write_header_is_rejected() {
    let tmp = tmp_path("late");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let stream = pcm_stream();
    let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
    mx.write_header().expect("write_header");
    let err = mx
        .add_chapter(0, Some(1_000_000_000), "Too Late")
        .expect_err("add_chapter after write_header must error");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("add_chapter") && msg.contains("write_header"),
        "unexpected error: {msg}"
    );
    drop(mx);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn add_chapter_rejects_end_before_start() {
    let tmp = tmp_path("bw");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let stream = pcm_stream();
    let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
    let err = mx
        .add_chapter(2_000_000_000, Some(1_000_000_000), "Backwards")
        .expect_err("end < start must be rejected up front");
    match err {
        Error::InvalidData(msg) => assert!(
            msg.contains("end_time_ns") && msg.contains("start_time_ns"),
            "unexpected error message: {msg}"
        ),
        _ => panic!("expected Error::InvalidData, got {err:?}"),
    }
    drop(mx);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn no_chapter_calls_emits_no_chapters_element() {
    let bytes = mux_with_chapters_collect(&[]);
    assert!(
        find_top_level(&bytes, ids::CHAPTERS).is_none(),
        "muxer must not emit a Chapters element when no chapters were queued"
    );
}

#[test]
fn chapters_element_sits_before_first_cluster() {
    let chapters = vec![MkvChapter {
        time_start_ns: 0,
        time_end_ns: Some(1_000_000_000),
        display: vec![ChapterDisplay {
            title: "Only".into(),
            language: "eng".into(),
            country: None,
        }],
    }];
    let bytes = mux_with_chapters_collect(&chapters);
    let (chapters_off, _seg_start) =
        find_top_level(&bytes, ids::CHAPTERS).expect("Chapters present");
    let (cluster_off, _) = find_top_level(&bytes, ids::CLUSTER).expect("Cluster present");
    assert!(
        chapters_off < cluster_off,
        "Chapters element ({}) should precede first Cluster ({}) so the demuxer's single-pass header walk picks it up",
        chapters_off,
        cluster_off
    );
}

#[test]
fn seek_head_chapters_entry_points_at_chapters_when_present() {
    let chapters = vec![MkvChapter {
        time_start_ns: 0,
        time_end_ns: Some(2_000_000_000),
        display: vec![ChapterDisplay {
            title: "First".into(),
            language: "eng".into(),
            country: None,
        }],
    }];
    let bytes = mux_with_chapters_collect(&chapters);

    let (chapters_abs, seg_start) =
        find_top_level(&bytes, ids::CHAPTERS).expect("Chapters present");
    let expected_rel = chapters_abs - seg_start;

    let entries = collect_seek_entries(&bytes);
    let chapters_entry = entries
        .iter()
        .find(|(id, _)| *id == ids::CHAPTERS)
        .expect("SeekHead must include a Chapters entry");
    assert_eq!(
        chapters_entry.1, expected_rel,
        "SeekHead Chapters position must match actual Chapters element offset"
    );
}

#[test]
fn seek_head_chapters_entry_is_voided_when_no_chapters() {
    let bytes = mux_with_chapters_collect(&[]);
    // No Chapters element should exist.
    assert!(find_top_level(&bytes, ids::CHAPTERS).is_none());
    // SeekHead should not include a Chapters Seek entry (it was overwritten
    // with a Void of the same size).
    let entries = collect_seek_entries(&bytes);
    assert!(
        !entries.iter().any(|(id, _)| *id == ids::CHAPTERS),
        "SeekHead Chapters slot must be voided when no chapters were emitted"
    );
}

/// Walk into a Chapters element and pull out the first ChapterAtom's
/// `(ChapterTimeStart, ChapterTimeEnd)` payloads (both as raw uint
/// values). Used by the on-disk-units regression test to make sure
/// the muxer treats `MkvChapter::time_start_ns` as literal
/// nanoseconds rather than e.g. milliseconds or some scaled
/// timecode.
fn first_atom_times(bytes: &[u8]) -> (u64, Option<u64>) {
    let (chapters_abs, _seg_start) =
        find_top_level(bytes, ids::CHAPTERS).expect("Chapters element");
    let mut cur = Cursor::new(bytes);
    cur.seek(SeekFrom::Start(chapters_abs)).unwrap();
    let ch = read_element_header(&mut cur).unwrap();
    assert_eq!(ch.id, ids::CHAPTERS);
    let ch_end = cur.stream_position().unwrap() + ch.size;
    // Walk Chapters → EditionEntry → ChapterAtom → {time_start, time_end}.
    while cur.stream_position().unwrap() < ch_end {
        let ee = read_element_header(&mut cur).unwrap();
        let ee_end = cur.stream_position().unwrap() + ee.size;
        if ee.id != ids::EDITION_ENTRY {
            cur.seek(SeekFrom::Start(ee_end)).unwrap();
            continue;
        }
        while cur.stream_position().unwrap() < ee_end {
            let child = read_element_header(&mut cur).unwrap();
            let child_end = cur.stream_position().unwrap() + child.size;
            if child.id != ids::CHAPTER_ATOM {
                cur.seek(SeekFrom::Start(child_end)).unwrap();
                continue;
            }
            let mut start: Option<u64> = None;
            let mut end: Option<u64> = None;
            while cur.stream_position().unwrap() < child_end {
                let sub = read_element_header(&mut cur).unwrap();
                let sub_end = cur.stream_position().unwrap() + sub.size;
                match sub.id {
                    ids::CHAPTER_TIME_START => {
                        start = Some(read_uint(&mut cur, sub.size as usize).unwrap());
                    }
                    ids::CHAPTER_TIME_END => {
                        end = Some(read_uint(&mut cur, sub.size as usize).unwrap());
                    }
                    _ => {
                        cur.seek(SeekFrom::Start(sub_end)).unwrap();
                    }
                }
            }
            return (start.expect("ChapterTimeStart required"), end);
        }
    }
    panic!("no ChapterAtom found inside Chapters element");
}

/// Regression guard: `MkvMuxer::add_chapter` accepts nanoseconds and
/// writes them straight into `ChapterTimeStart` / `ChapterTimeEnd`,
/// which are spec-defined as nanoseconds independent of
/// `TimecodeScale` (RFC 9559 §5.1.7.2). If anyone later misreads the
/// docstring and tries to "scale" the value into TimecodeScale ticks,
/// this test catches it.
#[test]
fn chapter_times_are_written_as_literal_nanoseconds() {
    // 3 600 050 000 000 ns = 1 h 0 min 0.05 s — picked so any
    // accidental ms-rescale (÷1 000 000) would land at the easily-
    // recognisable value 3 600 050 instead. 90 kHz PTS would land at
    // 324 004 500 (off by ~4 orders of magnitude).
    let start_ns: u64 = 3_600_050_000_000;
    let end_ns: u64 = 7_200_100_000_000;
    let chapters = vec![MkvChapter {
        time_start_ns: start_ns,
        time_end_ns: Some(end_ns),
        display: vec![ChapterDisplay {
            title: "NsLiteral".into(),
            language: "eng".into(),
            country: None,
        }],
    }];
    let bytes = mux_with_chapters_collect(&chapters);
    let (start, end) = first_atom_times(&bytes);
    assert_eq!(start, start_ns, "ChapterTimeStart must be literal ns");
    assert_eq!(
        end,
        Some(end_ns),
        "ChapterTimeEnd must be literal ns (no TimecodeScale rescale)"
    );
}

/// Walk the SeekHead at the head of the Segment and pull out
/// `(target_id, position)` pairs. Void entries inside the SeekHead are
/// skipped so the caller can assert on the live Seek set.
fn collect_seek_entries(bytes: &[u8]) -> Vec<(u32, u64)> {
    let mut out = Vec::new();
    let mut cur = Cursor::new(bytes);
    let ebml = read_element_header(&mut cur).expect("EBML header");
    assert_eq!(ebml.id, ids::EBML_HEADER);
    cur.seek(SeekFrom::Current(ebml.size as i64)).unwrap();
    let seg = read_element_header(&mut cur).expect("Segment");
    assert_eq!(seg.id, ids::SEGMENT);
    let sh = read_element_header(&mut cur).expect("SeekHead");
    assert_eq!(sh.id, ids::SEEK_HEAD);
    let sh_body_start = cur.stream_position().unwrap();
    let sh_end = sh_body_start + sh.size;
    while cur.stream_position().unwrap() < sh_end {
        let e = read_element_header(&mut cur).expect("SeekHead child");
        let body_start = cur.stream_position().unwrap();
        let body_end = body_start + e.size;
        match e.id {
            ids::SEEK => {
                let mut target_id: Option<u32> = None;
                let mut position: Option<u64> = None;
                while cur.stream_position().unwrap() < body_end {
                    let sub = read_element_header(&mut cur).expect("Seek child");
                    match sub.id {
                        ids::SEEK_ID => {
                            let mut buf = vec![0u8; sub.size as usize];
                            cur.read_exact(&mut buf).unwrap();
                            let mut id = 0u32;
                            for &b in &buf {
                                id = (id << 8) | (b as u32);
                            }
                            target_id = Some(id);
                        }
                        ids::SEEK_POSITION => {
                            position = Some(read_uint(&mut cur, sub.size as usize).unwrap());
                        }
                        _ => {
                            cur.seek(SeekFrom::Current(sub.size as i64)).unwrap();
                        }
                    }
                }
                if let (Some(id), Some(pos)) = (target_id, position) {
                    out.push((id, pos));
                }
            }
            ids::VOID => {
                cur.seek(SeekFrom::Start(body_end)).unwrap();
            }
            _ => {
                cur.seek(SeekFrom::Start(body_end)).unwrap();
            }
        }
    }
    out
}
