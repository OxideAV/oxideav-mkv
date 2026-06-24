//! Mux round-trip tests for Linked-Segment `Info` metadata
//! (RFC 9559 §5.1.2.1..§5.1.2.8 + Section 17).
//!
//! `MkvMuxer::set_segment_linking` queues a [`SegmentLinking`] record — the
//! mux-side twin of the demux-side `MkvDemuxer::segment_linking()` accessor —
//! and the muxer materialises the `SegmentUUID` / `SegmentFilename` /
//! `PrevUUID` / `PrevFilename` / `NextUUID` / `NextFilename` /
//! `SegmentFamily`(s) / `ChapterTranslate`(s) children into the `Info` master
//! in RFC 9559 §5.1.2 element order. Re-opening through the production
//! demuxer (`open_typed`) recovers every field byte-for-byte.
//!
//! Two surfaces are pinned:
//!
//! * **Mux round-trip** — a fully-populated record (all UIDs + filenames +
//!   two families + two `ChapterTranslate` masters with / without edition
//!   UIDs) survives a mux→demux cycle, plus the standalone-Segment case where
//!   nothing is queued surfaces an empty [`SegmentLinking`].
//! * **Validation** — the setter's spec-rule rejections (post-`write_header`,
//!   off-length UID, `PrevUUID` / `NextUUID` == `SegmentUUID`, a
//!   `ChapterTranslate` without the REQUIRED `SegmentFamily`, an empty
//!   `ChapterTranslateID`) are exercised.
//!
//! No third-party Matroska code is consulted — the production demuxer walks
//! every muxed buffer.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::demux::{ChapterTranslate, SegmentLinking};
use oxideav_mkv::mux::MkvMuxer;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r364-seglink-{}-{}-{n}.mkv",
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

fn keyframe_packet(stream: u32, pts: i64) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![0xAA; 16]);
    p.pts = Some(pts);
    p.flags.keyframe = true;
    p
}

/// Mux a one-video-track MKV. `configure` runs between construction and
/// `write_header`.
fn mux_one_track<F>(configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let streams = vec![video_stream(0)];
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer construct");
        configure(&mut mx);
        mx.write_header().expect("write_header");
        mx.write_packet(&keyframe_packet(0, 0)).expect("write 0");
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

/// 16-byte UID fixtures (RFC 9559 §5.1.2.1 / .3 / .5 / .7 all `length: 16`).
fn uid(seed: u8) -> Vec<u8> {
    (0..16u8).map(|i| i.wrapping_add(seed)).collect()
}

/// A fully-populated `SegmentLinking` survives a mux→demux round-trip with
/// every field byte-for-byte equal, and the §5.1.2 element order is honoured.
#[test]
fn roundtrip_full_segment_linking() {
    let linking = SegmentLinking {
        segment_uuid: Some(uid(0)),
        segment_filename: Some("part2.mkv".into()),
        prev_uuid: Some(uid(0x40)),
        prev_filename: Some("part1.mkv".into()),
        next_uuid: Some(uid(0x80)),
        next_filename: Some("part3.mkv".into()),
        families: vec![uid(0xA0), uid(0xC0)],
        chapter_translates: vec![
            ChapterTranslate {
                id: vec![0xDE, 0xAD, 0xBE, 0xEF],
                codec: 1,
                edition_uids: vec![11, 22],
            },
            ChapterTranslate {
                id: vec![0x01],
                codec: 0,
                edition_uids: vec![],
            },
        ],
    };

    let bytes = mux_one_track(|mx| {
        mx.set_segment_linking(linking.clone())
            .expect("set_segment_linking");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.segment_linking();

    assert_eq!(got.segment_uuid, Some(uid(0)));
    assert_eq!(got.segment_filename.as_deref(), Some("part2.mkv"));
    assert_eq!(got.prev_uuid, Some(uid(0x40)));
    assert_eq!(got.prev_filename.as_deref(), Some("part1.mkv"));
    assert_eq!(got.next_uuid, Some(uid(0x80)));
    assert_eq!(got.next_filename.as_deref(), Some("part3.mkv"));
    assert_eq!(got.families, vec![uid(0xA0), uid(0xC0)]);
    assert_eq!(got.chapter_translates.len(), 2);
    assert_eq!(got.chapter_translates[0].id, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    assert_eq!(got.chapter_translates[0].codec, 1);
    assert_eq!(got.chapter_translates[0].edition_uids, vec![11, 22]);
    assert_eq!(got.chapter_translates[1].id, vec![0x01]);
    assert_eq!(got.chapter_translates[1].codec, 0);
    assert!(got.chapter_translates[1].edition_uids.is_empty());

    // The whole record round-trips structurally equal.
    assert_eq!(got, &linking);
    assert!(got.is_hard_linked());
    assert!(!got.is_empty());
}

/// A Hard-Linked Segment that only names a previous neighbour (the last
/// Segment of a chain, RFC 9559 §5.1.2.3 usage note) round-trips, and writing
/// `PrevUUID` without `SegmentUUID` is legal (no self-reference to check).
#[test]
fn roundtrip_prev_only_no_segment_uuid() {
    let linking = SegmentLinking {
        prev_uuid: Some(uid(7)),
        ..SegmentLinking::default()
    };
    let bytes = mux_one_track(|mx| {
        mx.set_segment_linking(linking.clone()).expect("set");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.segment_linking();
    assert_eq!(got.prev_uuid, Some(uid(7)));
    assert!(got.next_uuid.is_none());
    assert!(got.is_hard_linked());
}

/// A standalone Segment (no `set_segment_linking` call) surfaces an empty
/// `SegmentLinking`, and queuing an all-default record writes nothing.
#[test]
fn standalone_segment_has_empty_linking() {
    let bytes_no_call = mux_one_track(|_mx| {});
    assert!(demux_typed(bytes_no_call).segment_linking().is_empty());

    let bytes_default = mux_one_track(|mx| {
        mx.set_segment_linking(SegmentLinking::default())
            .expect("set default");
    });
    assert!(demux_typed(bytes_default).segment_linking().is_empty());
}

/// `set_segment_linking` after `write_header` is rejected.
#[test]
fn rejects_after_write_header() {
    let f = std::fs::File::create(tmp_path("late")).expect("create");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("construct");
    mx.write_header().expect("write_header");
    let err = mx
        .set_segment_linking(SegmentLinking {
            segment_uuid: Some(uid(0)),
            ..SegmentLinking::default()
        })
        .map(|_| ())
        .expect_err("must reject after write_header");
    assert!(format!("{err}").contains("write_header"), "got: {err}");
}

/// An off-length (not 16-byte) UID is rejected (RFC 9559 §5.1.2, `length: 16`).
#[test]
fn rejects_off_length_uid() {
    let f = std::fs::File::create(tmp_path("len")).expect("create");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("construct");
    let err = mx
        .set_segment_linking(SegmentLinking {
            segment_uuid: Some(vec![1, 2, 3]),
            ..SegmentLinking::default()
        })
        .map(|_| ())
        .expect_err("3-byte SegmentUUID must be rejected");
    assert!(format!("{err}").contains("16 bytes"), "got: {err}");

    // Wrong-length SegmentFamily is rejected too.
    let f2 = std::fs::File::create(tmp_path("fam")).expect("create");
    let ws2: Box<dyn WriteSeek> = Box::new(f2);
    let mut mx2 = MkvMuxer::new_matroska(ws2, &streams).expect("construct");
    let err2 = mx2
        .set_segment_linking(SegmentLinking {
            families: vec![vec![0u8; 8]],
            ..SegmentLinking::default()
        })
        .map(|_| ())
        .expect_err("8-byte SegmentFamily must be rejected");
    assert!(format!("{err2}").contains("SegmentFamily"), "got: {err2}");
}

/// `PrevUUID` / `NextUUID` equal to `SegmentUUID` is rejected
/// (RFC 9559 §5.1.2.3 / §5.1.2.5 "MUST NOT be equal to the SegmentUUID").
#[test]
fn rejects_self_referential_link() {
    let streams = vec![video_stream(0)];

    let f = std::fs::File::create(tmp_path("self_prev")).expect("create");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("construct");
    let err = mx
        .set_segment_linking(SegmentLinking {
            segment_uuid: Some(uid(5)),
            prev_uuid: Some(uid(5)),
            ..SegmentLinking::default()
        })
        .map(|_| ())
        .expect_err("PrevUUID == SegmentUUID must be rejected");
    assert!(format!("{err}").contains("PrevUUID"), "got: {err}");

    let f2 = std::fs::File::create(tmp_path("self_next")).expect("create");
    let ws2: Box<dyn WriteSeek> = Box::new(f2);
    let mut mx2 = MkvMuxer::new_matroska(ws2, &streams).expect("construct");
    let err2 = mx2
        .set_segment_linking(SegmentLinking {
            segment_uuid: Some(uid(5)),
            next_uuid: Some(uid(5)),
            ..SegmentLinking::default()
        })
        .map(|_| ())
        .expect_err("NextUUID == SegmentUUID must be rejected");
    assert!(format!("{err2}").contains("NextUUID"), "got: {err2}");
}

/// A `ChapterTranslate` without the REQUIRED `SegmentFamily` is rejected
/// (RFC 9559 §5.1.2.7 usage note), and an empty `ChapterTranslateID` is
/// rejected (§5.1.2.8.1 `minOccurs: 1`).
#[test]
fn rejects_chapter_translate_rule_violations() {
    let streams = vec![video_stream(0)];

    // ChapterTranslate present but no SegmentFamily.
    let f = std::fs::File::create(tmp_path("ct_nofam")).expect("create");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("construct");
    let err = mx
        .set_segment_linking(SegmentLinking {
            chapter_translates: vec![ChapterTranslate {
                id: vec![0x01],
                codec: 0,
                edition_uids: vec![],
            }],
            ..SegmentLinking::default()
        })
        .map(|_| ())
        .expect_err("ChapterTranslate without SegmentFamily must be rejected");
    assert!(format!("{err}").contains("SegmentFamily"), "got: {err}");

    // Empty ChapterTranslateID, with a SegmentFamily present so only the id
    // rule fires.
    let f2 = std::fs::File::create(tmp_path("ct_noid")).expect("create");
    let ws2: Box<dyn WriteSeek> = Box::new(f2);
    let mut mx2 = MkvMuxer::new_matroska(ws2, &streams).expect("construct");
    let err2 = mx2
        .set_segment_linking(SegmentLinking {
            families: vec![uid(0)],
            chapter_translates: vec![ChapterTranslate {
                id: vec![],
                codec: 0,
                edition_uids: vec![],
            }],
            ..SegmentLinking::default()
        })
        .map(|_| ())
        .expect_err("empty ChapterTranslateID must be rejected");
    assert!(
        format!("{err2}").contains("ChapterTranslateID"),
        "got: {err2}"
    );
}

/// The queued record is visible through the read-only `segment_linking()`
/// accessor before the header is sealed.
#[test]
fn queued_linking_is_introspectable() {
    let f = std::fs::File::create(tmp_path("introspect")).expect("create");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("construct");
    assert!(mx.segment_linking().is_none());
    mx.set_segment_linking(SegmentLinking {
        segment_uuid: Some(uid(3)),
        ..SegmentLinking::default()
    })
    .expect("set");
    assert_eq!(
        mx.segment_linking().expect("queued").segment_uuid,
        Some(uid(3))
    );
}
