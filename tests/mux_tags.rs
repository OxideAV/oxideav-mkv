//! Round-trip tests for the muxer's `Tags` encoding.
//!
//! Drives `MkvMuxer::add_tag` against the public Muxer trait, then
//! re-opens the bytes through the demuxer and verifies that
//!
//! 1. Every queued tag surfaces through both the flat `metadata()` view
//!    (scope-prefixed `tag:track:N:<name>` / bare `<name>` keys) and the
//!    typed `MkvDemuxer::tags()` accessor with `Targets` UIDs resolved.
//! 2. The `Tags` master sits between `Attachments` (or `Tracks` /
//!    `Chapters` when those precede it) and the first `Cluster`, so the
//!    demuxer's single-pass header walk catches it.
//! 3. The SeekHead `Tags` slot points at the actual element offset, and
//!    the slot is voided when no tags were added.
//! 4. The `Tags` master carries a leading `CRC-32` child that validates
//!    through `crc_status()`.
//! 5. `add_tag` rejects calls made after `write_header`, a `Tag` with no
//!    `SimpleTag`, an empty `TagName`, and a `Some(0)` `TargetTypeValue`.
//! 6. Scope resolution: a `TagTrackUID` matching a track's auto-assigned
//!    `TrackUID` (= 1-based track number) resolves to that stream index;
//!    a global tag (empty `Targets`) lands on the bare key.
//!
//! Reference: RFC 9559 §5.1.8 (Tags / Tag / Targets / SimpleTag, incl.
//! §5.1.8.1.1.1..§5.1.8.1.2.6) and RFC 8794 §11 (EBML element encoding).
//!
//! These tests use the production EBML helpers to walk the muxed buffer —
//! no third-party Matroska code is consulted.

use std::io::{Cursor, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, Packet, ReadSeek, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mkv::demux::{SimpleTagValue, TargetUid};
use oxideav_mkv::ebml::{read_element_header, VINT_UNKNOWN_SIZE};
use oxideav_mkv::ids;
use oxideav_mkv::mux::{MkvMuxer, MkvSimpleTag, MkvSimpleTagValue, MkvTag, MkvTagTargets};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r355-tags-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

fn pcm_stream(index: u32) -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p.sample_format = Some(oxideav_core::SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn pcm_packet(index: u32, pts_ms: i64, payload: u8) -> Packet {
    let mut pkt = Packet::new(index, TimeBase::new(1, 1000), vec![payload; 32]);
    pkt.pts = Some(pts_ms);
    pkt.flags.keyframe = true;
    pkt
}

fn mux_with_tags(streams: &[StreamInfo], tags: &[MkvTag]) -> Vec<u8> {
    let tmp = tmp_path("collect");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, streams).expect("muxer construct");
        for t in tags {
            mx.add_tag(t.clone()).expect("add tag");
        }
        mx.write_header().expect("write_header");
        for s in streams {
            mx.write_packet(&pcm_packet(s.index, 0, 0xAA))
                .expect("write_packet");
        }
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn open_typed(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

fn open_demuxer(bytes: Vec<u8>) -> Box<dyn Demuxer> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

/// Walk the muxed buffer and find the absolute byte offset of the first
/// top-level element with the given id inside the Segment payload.
fn find_top_level(bytes: &[u8], target_id: u32) -> Option<u64> {
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
            return Some(elem_start);
        }
        let body_end = if e.size == VINT_UNKNOWN_SIZE {
            segment_end
        } else {
            body_start + e.size
        };
        cur.seek(SeekFrom::Start(body_end)).ok()?;
    }
    None
}

/// Absolute file offset of the Segment payload start (the byte right
/// after the Segment element id + size header).
fn segment_data_start(bytes: &[u8]) -> u64 {
    let mut cur = Cursor::new(bytes);
    let ebml = read_element_header(&mut cur).expect("ebml header");
    cur.seek(SeekFrom::Current(ebml.size as i64)).expect("skip");
    let seg = read_element_header(&mut cur).expect("segment header");
    assert_eq!(seg.id, ids::SEGMENT);
    cur.stream_position().expect("pos")
}

/// Decode the `SeekPosition` value stored in the SeekHead entry for
/// `target_id` (a Segment Position, relative to the Segment payload
/// start). `None` if the entry was voided.
fn seek_head_position(bytes: &[u8], target_id: u32) -> Option<u64> {
    let dmx = open_typed(bytes.to_vec());
    dmx.seek_entries()
        .iter()
        .find(|e| e.seek_id() == Some(target_id))
        .filter(|e| e.has_position())
        .map(|e| e.seek_position())
}

#[test]
fn global_string_tag_round_trips_flat_and_typed() {
    let streams = [pcm_stream(0)];
    let tags = [MkvTag::global("TITLE", "My Album")];
    let bytes = mux_with_tags(&streams, &tags);

    // Flat view: global scope → bare lower-cased key.
    let dmx = open_demuxer(bytes.clone());
    let md = dmx.metadata().to_vec();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(get("title").as_deref(), Some("My Album"));

    // Typed view: one Tag, global scope, one SimpleTag.
    let typed = open_typed(bytes);
    assert_eq!(typed.tags().len(), 1);
    let t = &typed.tags()[0];
    assert!(t.targets.uids.is_empty(), "global scope has no UIDs");
    assert_eq!(t.targets.target_type_value, None);
    assert_eq!(t.simple_tags.len(), 1);
    assert_eq!(t.simple_tags[0].name, "TITLE");
    assert_eq!(
        t.simple_tags[0].value,
        SimpleTagValue::String("My Album".into())
    );
    assert_eq!(t.simple_tags[0].language, "und");
    assert!(t.simple_tags[0].default);
}

#[test]
fn track_scoped_tag_resolves_to_stream_index() {
    // Two streams: a track-scoped tag on the second one (TrackUID = 2).
    let streams = [pcm_stream(0), pcm_stream(1)];
    let tags = [MkvTag {
        targets: MkvTagTargets::track(2),
        simple_tags: vec![MkvSimpleTag::new("ARTIST", "Track Two Artist")],
    }];
    let bytes = mux_with_tags(&streams, &tags);

    // Flat key uses the 0-indexed stream index (1) for TrackUID 2.
    let dmx = open_demuxer(bytes.clone());
    let md = dmx.metadata().to_vec();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("tag:track:1:artist").as_deref(),
        Some("Track Two Artist")
    );

    // Typed view: UID resolves to stream index 1.
    let typed = open_typed(bytes);
    let t = &typed.tags()[0];
    assert_eq!(t.targets.uids.len(), 1);
    match t.targets.uids[0] {
        TargetUid::Track {
            stream_index,
            track_uid,
        } => {
            assert_eq!(stream_index, 1);
            assert_eq!(track_uid, 2);
        }
        other => panic!("expected Track UID, got {other:?}"),
    }
}

#[test]
fn target_type_and_value_round_trip() {
    let streams = [pcm_stream(0)];
    let tags = [MkvTag {
        targets: MkvTagTargets {
            target_type_value: Some(50),
            target_type: Some("ALBUM".into()),
            ..Default::default()
        },
        simple_tags: vec![MkvSimpleTag::new("GENRE", "Ambient")],
    }];
    let bytes = mux_with_tags(&streams, &tags);

    let typed = open_typed(bytes);
    let t = &typed.tags()[0];
    assert_eq!(t.targets.target_type_value, Some(50));
    assert_eq!(t.targets.target_type.as_deref(), Some("ALBUM"));
    // target_level() resolves the typed hierarchy.
    assert_eq!(
        t.targets.target_level(),
        Some(oxideav_mkv::demux::TargetLevel::Album)
    );
}

#[test]
fn language_and_default_flag_round_trip() {
    let streams = [pcm_stream(0)];
    let tags = [MkvTag {
        targets: MkvTagTargets::default(),
        simple_tags: vec![
            // Non-default language, default flag cleared.
            MkvSimpleTag {
                name: "COMMENT".into(),
                value: MkvSimpleTagValue::String("Kommentar".into()),
                language: "ger".into(),
                language_bcp47: None,
                default: false,
                children: Vec::new(),
            },
            // BCP-47 language wins over TagLanguage per spec.
            MkvSimpleTag {
                name: "COMMENT".into(),
                value: MkvSimpleTagValue::String("Comment".into()),
                language: "und".into(),
                language_bcp47: Some("en-US".into()),
                default: true,
                children: Vec::new(),
            },
        ],
    }];
    let bytes = mux_with_tags(&streams, &tags);

    let typed = open_typed(bytes);
    let t = &typed.tags()[0];
    assert_eq!(t.simple_tags.len(), 2);

    let de = &t.simple_tags[0];
    assert_eq!(de.language, "ger");
    assert!(!de.default, "TagDefault cleared round-trips as false");

    let en = &t.simple_tags[1];
    assert_eq!(en.language_bcp47.as_deref(), Some("en-US"));
    assert!(en.default);
}

#[test]
fn binary_tag_value_round_trips() {
    let streams = [pcm_stream(0)];
    let payload = b"\x89PNG\r\n\x1a\n thumbnail".to_vec();
    let tags = [MkvTag {
        targets: MkvTagTargets::default(),
        simple_tags: vec![MkvSimpleTag::binary("THUMBNAIL", payload.clone())],
    }];
    let bytes = mux_with_tags(&streams, &tags);

    let typed = open_typed(bytes.clone());
    let t = &typed.tags()[0];
    assert_eq!(
        t.simple_tags[0].value,
        SimpleTagValue::Binary(payload.clone())
    );
    // Binary values never project into the flat metadata view.
    let dmx = open_demuxer(bytes);
    assert!(!dmx.metadata().iter().any(|(k, _)| k == "thumbnail"));
}

#[test]
fn tags_master_position_and_seek_head() {
    let streams = [pcm_stream(0)];
    let tags = [MkvTag::global("TITLE", "Positioned")];
    let bytes = mux_with_tags(&streams, &tags);

    let tags_off = find_top_level(&bytes, ids::TAGS).expect("Tags element present");
    let cluster_off = find_top_level(&bytes, ids::CLUSTER).expect("Cluster present");
    assert!(
        tags_off < cluster_off,
        "Tags must sit before the first Cluster (single-pass header walk)"
    );

    // SeekHead Tags slot points at the actual offset (Segment Position).
    let seek_pos = seek_head_position(&bytes, ids::TAGS).expect("Tags SeekHead slot");
    let seg_start = segment_data_start(&bytes);
    assert_eq!(seg_start + seek_pos, tags_off);
}

#[test]
fn no_tags_voids_seek_head_slot() {
    let streams = [pcm_stream(0)];
    let bytes = mux_with_tags(&streams, &[]);
    assert!(
        find_top_level(&bytes, ids::TAGS).is_none(),
        "no Tags element when none queued"
    );
    assert!(
        seek_head_position(&bytes, ids::TAGS).is_none(),
        "Tags SeekHead slot is voided when no tags queued"
    );
}

#[test]
fn tags_master_carries_valid_crc() {
    let streams = [pcm_stream(0)];
    let tags = [MkvTag::global("TITLE", "CRC checked")];
    let bytes = mux_with_tags(&streams, &tags);

    let typed = open_typed(bytes);
    let tags_crc = typed
        .crc_status()
        .iter()
        .find(|c| c.element_id == ids::TAGS)
        .expect("Tags CRC status present");
    assert!(tags_crc.is_valid(), "Tags master CRC must validate");
}

#[test]
fn add_tag_rejects_empty_simple_tags() {
    let f = std::fs::File::create(tmp_path("rej-empty")).expect("create");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[pcm_stream(0)]).expect("construct");
    let err = mx
        .add_tag(MkvTag {
            targets: MkvTagTargets::default(),
            simple_tags: Vec::new(),
        })
        .unwrap_err();
    assert!(matches!(err, Error::InvalidData(_)));
}

#[test]
fn add_tag_rejects_empty_name() {
    let f = std::fs::File::create(tmp_path("rej-name")).expect("create");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[pcm_stream(0)]).expect("construct");
    let err = mx
        .add_tag(MkvTag {
            targets: MkvTagTargets::default(),
            simple_tags: vec![MkvSimpleTag::new("", "value")],
        })
        .unwrap_err();
    assert!(matches!(err, Error::InvalidData(_)));
}

#[test]
fn add_tag_rejects_zero_target_type_value() {
    let f = std::fs::File::create(tmp_path("rej-ttv")).expect("create");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[pcm_stream(0)]).expect("construct");
    let err = mx
        .add_tag(MkvTag {
            targets: MkvTagTargets {
                target_type_value: Some(0),
                ..Default::default()
            },
            simple_tags: vec![MkvSimpleTag::new("X", "y")],
        })
        .unwrap_err();
    assert!(matches!(err, Error::InvalidData(_)));
}

#[test]
fn add_tag_rejected_after_write_header() {
    let f = std::fs::File::create(tmp_path("rej-late")).expect("create");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[pcm_stream(0)]).expect("construct");
    mx.write_header().expect("write_header");
    let err = mx.add_tag(MkvTag::global("TITLE", "late")).unwrap_err();
    assert!(matches!(err, Error::Other(_)));
}

#[test]
fn multiple_tags_round_trip_in_order() {
    let streams = [pcm_stream(0)];
    let tags = [
        MkvTag::global("TITLE", "First"),
        MkvTag::global("ARTIST", "Second"),
        MkvTag::global("GENRE", "Third"),
    ];
    let bytes = mux_with_tags(&streams, &tags);

    let typed = open_typed(bytes);
    assert_eq!(typed.tags().len(), 3);
    assert_eq!(typed.tags()[0].simple_tags[0].name, "TITLE");
    assert_eq!(typed.tags()[1].simple_tags[0].name, "ARTIST");
    assert_eq!(typed.tags()[2].simple_tags[0].name, "GENRE");
}

#[test]
fn nested_simple_tags_round_trip() {
    // RFC 9559 §5.1.8.1.2 `recursive: True`: a SimpleTag carries child
    // SimpleTags. Build a TITLE with a SORT_WITH sub-tag two levels deep.
    let streams = [pcm_stream(0)];
    let parent = MkvSimpleTag {
        name: "TITLE".into(),
        value: MkvSimpleTagValue::String("The Album".into()),
        language: "und".into(),
        language_bcp47: None,
        default: true,
        children: vec![MkvSimpleTag {
            name: "SORT_WITH".into(),
            value: MkvSimpleTagValue::String("Album, The".into()),
            language: "und".into(),
            language_bcp47: None,
            default: true,
            children: vec![MkvSimpleTag::new("DEPTH2", "leaf")],
        }],
    };
    let tags = [MkvTag {
        targets: MkvTagTargets::default(),
        simple_tags: vec![parent],
    }];
    let bytes = mux_with_tags(&streams, &tags);

    let typed = open_typed(bytes);
    let t = &typed.tags()[0];
    assert_eq!(t.simple_tags.len(), 1);
    let title = &t.simple_tags[0];
    assert_eq!(title.name, "TITLE");
    assert_eq!(title.children.len(), 1, "one nested SORT_WITH child");

    let sort = &title.children[0];
    assert_eq!(sort.name, "SORT_WITH");
    assert_eq!(sort.value, SimpleTagValue::String("Album, The".into()));
    assert_eq!(sort.children.len(), 1, "two-level nesting preserved");
    assert_eq!(sort.children[0].name, "DEPTH2");
    assert_eq!(
        sort.children[0].value,
        SimpleTagValue::String("leaf".into())
    );

    // The nested descriptors do not leak into the flat metadata view —
    // only top-level descriptors ever did.
    let dmx = open_demuxer(mux_with_tags(&streams, &tags));
    assert!(dmx.metadata().iter().any(|(k, _)| k == "title"));
    assert!(!dmx.metadata().iter().any(|(k, _)| k == "sort_with"));
}

#[test]
fn name_only_parent_simple_tag_round_trips() {
    // A SimpleTag with no payload (TagString/TagBinary both absent) is
    // legal as a parent node (RFC 9559 §5.1.8.1.2 — neither value element
    // has a minOccurs). It must survive with its children.
    let streams = [pcm_stream(0)];
    let parent = MkvSimpleTag {
        name: "ARTISTS".into(),
        value: MkvSimpleTagValue::None,
        language: "und".into(),
        language_bcp47: None,
        default: true,
        children: vec![
            MkvSimpleTag::new("ARTIST", "Alice"),
            MkvSimpleTag::new("ARTIST", "Bob"),
        ],
    };
    let tags = [MkvTag {
        targets: MkvTagTargets::default(),
        simple_tags: vec![parent],
    }];
    let bytes = mux_with_tags(&streams, &tags);

    let typed = open_typed(bytes);
    let parent = &typed.tags()[0].simple_tags[0];
    assert_eq!(parent.name, "ARTISTS");
    assert_eq!(parent.value, SimpleTagValue::None);
    assert_eq!(parent.children.len(), 2);
    assert_eq!(parent.children[0].name, "ARTIST");
    assert_eq!(
        parent.children[1].value,
        SimpleTagValue::String("Bob".into())
    );
}

#[test]
fn tags_accessor_mirrors_queue() {
    let f = std::fs::File::create(tmp_path("queue")).expect("create");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[pcm_stream(0)]).expect("construct");
    mx.add_tag(MkvTag::global("A", "1")).expect("add");
    mx.add_tag(MkvTag::global("B", "2")).expect("add");
    assert_eq!(mx.tags().len(), 2);
    assert_eq!(mx.tags()[0].simple_tags[0].name, "A");
}
