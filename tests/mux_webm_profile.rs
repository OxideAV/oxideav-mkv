//! WebM muxer strict-profile gating tests.
//!
//! A WebM muxer defaults to emitting only elements the WebM guidelines
//! list as supported (see `oxideav_mkv::webm`):
//!
//! 1. A fully-featured strict-WebM mux (chapters, tags, colour, geometry,
//!    stereo/alpha, projection, position hints, multiple clusters) scans
//!    conformant via `webm::scan` — the headline round-trip.
//! 2. Every gated setter rejects with `Error::Unsupported` on a strict
//!    WebM muxer, and the same calls succeed on a Matroska muxer.
//! 3. `with_webm_lenient()` restores the full Matroska surface under the
//!    `webm` DocType — and the output then fails the conformance scan
//!    with the expected findings.
//! 4. Emission-level gates: no `CRC-32` children (`crc_status()` empty on
//!    read-back), `Position` hint suppressed while `PrevSize` survives,
//!    chapters `EditionEntry` without `EditionUID`, `SilentTracks`
//!    erroring at packet-write time.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::mux::{
    BlockGroupOptions, MkvAttachment, MkvChapter, MkvMuxer, MkvProjection, MkvSimpleTag, MkvTag,
    MkvTagTargets, MkvTrackIdentity, MkvTrackLegacy, MkvTrackOperation, MkvTrackTiming,
    MkvVideoColour, MkvVideoGeometry,
};
use oxideav_mkv::webm::scan;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r416-webmprofile-{tag}-{}-{n}.webm",
        std::process::id()
    ))
}

fn opus_head() -> Vec<u8> {
    let mut out = Vec::with_capacity(19);
    out.extend_from_slice(b"OpusHead");
    out.push(1);
    out.push(2);
    out.extend_from_slice(&312u16.to_le_bytes());
    out.extend_from_slice(&48_000u32.to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.push(0);
    out
}

fn vp9_stream(index: u32) -> StreamInfo {
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

fn opus_stream(index: u32) -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("opus"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p.extradata = opus_head();
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn video_packet(index: u32, pts_ms: i64, key: bool) -> Packet {
    let mut pkt = Packet::new(index, TimeBase::new(1, 1000), vec![0xC5; 64]);
    pkt.pts = Some(pts_ms);
    pkt.flags.keyframe = key;
    pkt
}

fn webm_muxer(tag: &str, streams: &[StreamInfo]) -> (MkvMuxer, std::path::PathBuf) {
    let tmp = tmp_path(tag);
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    (MkvMuxer::new_webm(ws, streams).expect("webm muxer"), tmp)
}

fn finish(tmp: &std::path::Path) -> Vec<u8> {
    let bytes = std::fs::read(tmp).expect("read back");
    let _ = std::fs::remove_file(tmp);
    bytes
}

fn expect_webm_unsupported(r: Result<&mut MkvMuxer, Error>, what: &str) {
    match r {
        Ok(_) => panic!("{what}: strict WebM muxer must reject"),
        Err(Error::Unsupported(msg)) => {
            assert!(
                msg.contains("WebM") && msg.contains("with_webm_lenient"),
                "{what}: message should explain the profile gate and the opt-out, got: {msg}"
            );
        }
        Err(other) => panic!("{what}: expected Error::Unsupported, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------

#[test]
fn strict_webm_output_scans_conformant() {
    let streams = vec![vp9_stream(0), opus_stream(1)];
    let (mut mx, tmp) = webm_muxer("conformant", &streams);
    assert!(mx.webm_strict());
    // In-profile extras.
    mx.with_cluster_position_hints().expect("hints");
    mx.set_video_colour(0, MkvVideoColour::bt709())
        .expect("colour");
    mx.set_video_geometry(0, MkvVideoGeometry::cropped(2, 2, 0, 0))
        .expect("geometry");
    // Unlisted (post-guidelines) elements must not break conformance.
    mx.set_video_projection(0, MkvProjection::rotated(90.0))
        .expect("projection");
    mx.add_chapter(0, Some(4_000_000_000), "Intro")
        .expect("chapter");
    mx.add_tag(MkvTag::global("TITLE", "Strict"))
        .expect("tag global");
    mx.add_tag(MkvTag {
        targets: MkvTagTargets::track(1),
        simple_tags: vec![MkvSimpleTag::new("ARTIST", "Nobody")],
    })
    .expect("tag track");
    mx.set_duration(std::time::Duration::from_secs(11))
        .expect("duration");
    mx.write_header().expect("header");
    // Three clusters (~5 s budget each).
    for (i, pts) in [0i64, 40, 5100, 5140, 10_200].iter().enumerate() {
        mx.write_packet(&video_packet(0, *pts, i % 2 == 0))
            .expect("packet");
    }
    mx.write_trailer().expect("trailer");
    let bytes = finish(&tmp);

    let report = scan(&mut Cursor::new(&bytes)).expect("scan");
    assert_eq!(report.doc_type.as_deref(), Some("webm"));
    assert!(
        report.is_conformant(),
        "strict WebM output must scan conformant; findings: {:?}, stopped: {:?}",
        report.findings,
        report.scan_stopped_at
    );
    assert_eq!(report.unsupported, 0);
    assert_eq!(report.deprecated, 0);
    // The projection master is there, surfacing as Unlisted.
    assert!(report.unlisted_ids.contains(&oxideav_mkv::ids::PROJECTION));

    // Read-back: no CRC-32 statuses (none written), chapters + tags alive,
    // Position hints suppressed but PrevSize kept.
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux");
    assert!(dmx.crc_status().is_empty(), "strict WebM writes no CRC-32");
    assert_eq!(dmx.chapters().len(), 1);
    assert!(dmx.chapters()[0].uid.is_none(), "EditionUID suppressed");
    assert_eq!(dmx.tags().len(), 2);
    let mut n = 0;
    while let Ok(_p) = oxideav_core::Demuxer::next_packet(&mut dmx) {
        n += 1;
    }
    assert_eq!(n, 5);
    let recs = dmx.cluster_records();
    assert!(
        recs.len() >= 2,
        "expected multiple clusters, got {}",
        recs.len()
    );
    for rec in recs {
        assert_eq!(rec.position, None, "Position must be suppressed");
    }
    assert!(
        recs[1..].iter().all(|r| r.prev_size.is_some()),
        "PrevSize is in-profile and must survive"
    );
}

#[test]
fn strict_webm_rejects_off_profile_setters() {
    let streams = vec![vp9_stream(0), opus_stream(1)];
    let (mut mx, tmp) = webm_muxer("rejections", &streams);

    expect_webm_unsupported(
        mx.set_track_operation(0, MkvTrackOperation::stereo_3d(0, 0)),
        "set_track_operation",
    );
    expect_webm_unsupported(
        mx.set_track_translates(
            0,
            vec![oxideav_mkv::mux::MkvTrackTranslate::new(vec![1], 1)],
        ),
        "set_track_translates",
    );
    expect_webm_unsupported(
        mx.set_track_legacy(
            0,
            MkvTrackLegacy {
                min_cache: Some(1),
                ..Default::default()
            },
        ),
        "set_track_legacy",
    );
    match mx.add_attachment(MkvAttachment::new(
        "f.bin",
        "application/octet-stream",
        vec![1],
    )) {
        Err(Error::Unsupported(msg)) => assert!(msg.contains("Attachments"), "{msg}"),
        other => panic!("add_attachment must be rejected, got {other:?}"),
    }
    expect_webm_unsupported(
        mx.set_segment_linking(oxideav_mkv::demux::SegmentLinking {
            segment_uuid: Some(vec![7u8; 16]),
            ..Default::default()
        }),
        "set_segment_linking",
    );
    expect_webm_unsupported(
        mx.set_max_block_addition_id(0, 2),
        "set_max_block_addition_id",
    );
    expect_webm_unsupported(
        mx.set_track_timing(
            0,
            MkvTrackTiming {
                default_decoded_field_duration: Some(20_833_333),
                ..Default::default()
            },
        ),
        "set_track_timing (DefaultDecodedFieldDuration)",
    );
    expect_webm_unsupported(
        mx.set_track_timing(
            0,
            MkvTrackTiming {
                track_timestamp_scale: Some(2.0),
                ..Default::default()
            },
        ),
        "set_track_timing (TrackTimestampScale)",
    );
    expect_webm_unsupported(
        mx.set_track_identity(
            0,
            MkvTrackIdentity {
                attachment_link: Some(1),
                ..Default::default()
            },
        ),
        "set_track_identity (AttachmentLink)",
    );
    // Off-profile chapter fields.
    for (what, ch) in [
        (
            "ChapterFlagHidden",
            MkvChapter {
                hidden: true,
                ..Default::default()
            },
        ),
        (
            "ChapterFlagEnabled",
            MkvChapter {
                enabled: false,
                ..Default::default()
            },
        ),
        (
            "ChapterSegmentUUID",
            MkvChapter {
                segment_uuid: Some(vec![3u8; 16]),
                ..Default::default()
            },
        ),
        (
            "ChapterPhysicalEquiv",
            MkvChapter {
                physical_equiv: Some(60),
                ..Default::default()
            },
        ),
    ] {
        match mx.add_chapter_full(ch) {
            Err(Error::Unsupported(msg)) => assert!(msg.contains(what), "{what}: {msg}"),
            other => panic!("{what}: expected rejection, got {other:?}"),
        }
    }
    // Off-profile tag scopes.
    for (what, targets) in [
        (
            "TagEditionUID",
            MkvTagTargets {
                edition_uids: vec![1],
                ..Default::default()
            },
        ),
        (
            "TagChapterUID",
            MkvTagTargets {
                chapter_uids: vec![1],
                ..Default::default()
            },
        ),
        (
            "TagAttachmentUID",
            MkvTagTargets {
                attachment_uids: vec![1],
                ..Default::default()
            },
        ),
    ] {
        match mx.add_tag(MkvTag {
            targets,
            simple_tags: vec![MkvSimpleTag::new("K", "V")],
        }) {
            Err(Error::Unsupported(msg)) => assert!(msg.contains(what), "{what}: {msg}"),
            other => panic!("{what}: expected rejection, got {other:?}"),
        }
    }
    // In-profile equivalents still work.
    mx.add_chapter(0, None, "Plain chapter")
        .expect("plain chapter ok");
    mx.add_tag(MkvTag {
        targets: MkvTagTargets::track(1),
        simple_tags: vec![MkvSimpleTag::new("K", "V")],
    })
    .expect("track-scoped tag ok");
    mx.set_track_timing(0, MkvTrackTiming::from_frame_rate(25.0).unwrap())
        .expect("DefaultDuration ok");
    drop(mx);
    let _ = std::fs::remove_file(tmp);
}

#[test]
fn strict_webm_rejects_block_group_extras_and_silent_tracks() {
    let streams = vec![vp9_stream(0)];
    let (mut mx, tmp) = webm_muxer("bg", &streams);
    mx.write_header().expect("header");

    for (what, opts) in [
        (
            "ReferencePriority",
            BlockGroupOptions {
                reference_priority: 5,
                ..Default::default()
            },
        ),
        (
            "CodecState",
            BlockGroupOptions {
                codec_state: Some(vec![1, 2]),
                ..Default::default()
            },
        ),
        (
            "BlockVirtual",
            BlockGroupOptions {
                block_virtual: Some(vec![0x81, 0, 0, 0]),
                ..Default::default()
            },
        ),
    ] {
        match mx.write_packet_with_block_group(&video_packet(0, 0, true), &opts) {
            Err(Error::Unsupported(msg)) => assert!(msg.contains(what), "{what}: {msg}"),
            other => panic!("{what}: expected rejection, got {other:?}"),
        }
    }
    // In-profile group options pass.
    mx.write_packet_with_block_group(
        &video_packet(0, 0, true),
        &BlockGroupOptions {
            discard_padding: Some(1_000_000),
            ..Default::default()
        },
    )
    .expect("DiscardPadding is in-profile");

    // SilentTracks: the setter is infallible, the gate fires at the next
    // Cluster open.
    mx.set_next_cluster_silent_tracks(&[1]);
    match mx.write_packet(&video_packet(0, 6_000, true)) {
        Err(Error::Unsupported(msg)) => assert!(msg.contains("SilentTracks"), "{msg}"),
        other => panic!("SilentTracks: expected rejection at cluster open, got {other:?}"),
    }
    drop(mx);
    let _ = std::fs::remove_file(tmp);
}

#[test]
fn lenient_webm_restores_matroska_surface() {
    let streams = vec![vp9_stream(0), opus_stream(1)];
    let (mut mx, tmp) = webm_muxer("lenient", &streams);
    mx.with_webm_lenient().expect("lenient");
    assert!(!mx.webm_strict());

    mx.add_attachment(MkvAttachment::new(
        "f.bin",
        "application/octet-stream",
        vec![1, 2, 3],
    ))
    .expect("attachment ok when lenient");
    mx.add_chapter_full(MkvChapter {
        hidden: true,
        ..Default::default()
    })
    .expect("hidden chapter ok when lenient");
    mx.set_max_block_addition_id(0, 2)
        .expect("max block addition ok");
    mx.write_header().expect("header");
    mx.write_packet(&video_packet(0, 0, true)).expect("packet");
    mx.write_trailer().expect("trailer");
    let bytes = finish(&tmp);

    let report = scan(&mut Cursor::new(&bytes)).expect("scan");
    assert_eq!(report.doc_type.as_deref(), Some("webm"));
    assert!(!report.is_conformant());
    let flagged: Vec<u32> = report.findings.iter().map(|f| f.id).collect();
    assert!(flagged.contains(&oxideav_mkv::ids::CRC32));
    assert!(flagged.contains(&oxideav_mkv::ids::ATTACHMENTS));
    assert!(flagged.contains(&oxideav_mkv::ids::CHAPTER_FLAG_HIDDEN));
    assert!(flagged.contains(&oxideav_mkv::ids::MAX_BLOCK_ADDITION_ID));

    // The demuxer sees the CRCs again.
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux");
    assert!(!dmx.crc_status().is_empty());
    assert!(dmx.crc_status().iter().all(|c| c.is_valid()));
    assert_eq!(
        dmx.chapters()[0].uid,
        Some(1),
        "EditionUID back when lenient"
    );
}

#[test]
fn matroska_muxer_rejects_webm_lenient_and_keeps_full_surface() {
    let streams = vec![vp9_stream(0)];
    let tmp = tmp_path("matroska");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("matroska muxer");
    assert!(!mx.webm_strict());
    match mx.with_webm_lenient() {
        Err(Error::Other(msg)) => assert!(msg.contains("WebM"), "{msg}"),
        Err(other) => panic!("expected Error::Other on matroska, got {other:?}"),
        Ok(_) => panic!("with_webm_lenient must be rejected on a Matroska muxer"),
    }
    // The full surface stays available.
    mx.add_attachment(MkvAttachment::new(
        "f.bin",
        "application/octet-stream",
        vec![1],
    ))
    .expect("attachment ok on matroska");
    mx.add_chapter_full(MkvChapter {
        hidden: true,
        physical_equiv: Some(60),
        ..Default::default()
    })
    .expect("full chapter ok on matroska");
    drop(mx);
    let _ = std::fs::remove_file(tmp);
}
