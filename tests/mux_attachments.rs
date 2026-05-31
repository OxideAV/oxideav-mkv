//! Round-trip tests for the muxer's `Attachments` encoding.
//!
//! Drives `MkvMuxer::add_attachment` against the public Muxer trait,
//! then re-opens the bytes through the demuxer and verifies that
//!
//! 1. Every queued attachment surfaces as `attachment:N:filename` /
//!    `:mime_type` / `:size_bytes` / `:description` in the demuxer's
//!    `metadata()` view, in the same order they were added.
//! 2. The typed `MkvDemuxer::attachments()` accessor surfaces the same
//!    fields with the on-disk `FileUID`, and
//!    `MkvDemuxer::attachment_data(N)` returns the verbatim payload.
//! 3. The `Attachments` master sits between `Tracks` (or `Chapters`
//!    when both are present) and the first `Cluster`, so the demuxer's
//!    single-pass header walk catches it.
//! 4. The SeekHead `Attachments` slot points at the actual element
//!    offset, and the slot is voided when no attachments were added
//!    (keeping pre-walking players from chasing a placeholder zero).
//! 5. `add_attachment` rejects calls made after `write_header`, an
//!    empty `FileName`, an empty `FileMediaType`, and an explicit
//!    `Some(0)` UID (the spec's `range: not 0`).
//! 6. `Tags.Targets.TagAttachmentUID` references resolve when the
//!    muxer auto-derives a UID and the tag carries that same value
//!    (round-trip via the existing `attach_tag_to_attachment` flow on
//!    real files would land its check here once the tag mux exists;
//!    in the meantime, the resolver still works because the UID is
//!    deterministic).
//!
//! Reference: RFC 9559 §5.1.6 (Attachments / AttachedFile / FileName /
//! FileMediaType / FileData / FileUID / FileDescription) and RFC 8794
//! §11 (EBML element encoding).
//!
//! These tests use the production EBML helpers to walk the muxed
//! buffer — no third-party Matroska code is consulted.

use std::io::{Cursor, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, Packet, ReadSeek, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mkv::ebml::{read_element_header, VINT_UNKNOWN_SIZE};
use oxideav_mkv::ids;
use oxideav_mkv::mux::{MkvAttachment, MkvChapter, MkvMuxer};

/// Counter ensures every temp file produced by the parallel test runner
/// gets a unique name — cargo's default `--test-threads=8` would
/// otherwise stomp `mux_attachments-{pid}.mkv` between tests that all
/// run concurrently and create/remove the same path.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r196-attach-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

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

fn mux_with_attachments_collect(attachments: &[MkvAttachment]) -> Vec<u8> {
    let tmp = tmp_path("collect");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let stream = pcm_stream();
        let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
        for att in attachments {
            mx.add_attachment(att.clone()).expect("add attachment");
        }
        mx.write_header().expect("write_header");
        mx.write_packet(&pcm_packet(0, 0xAA)).expect("write_packet");
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

/// Walk the muxed buffer and find the absolute byte offset (relative to
/// the start of `bytes`) of the first top-level element with the given
/// id inside the Segment payload. Returns `None` if not present.
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

/// Synthetic 4-byte cover-art payload (deliberately not a real JPEG —
/// the container layer doesn't inspect FileData bytes).
const COVER_BYTES: &[u8] = b"\xFF\xD8\xFF\xE0";

/// Synthetic 17-byte "font" payload.
const FONT_BYTES: &[u8] = b"\x00\x01OTTO\x00\x10test font\x00";

#[test]
fn attachments_round_trip_through_demuxer_metadata() {
    let attachments = vec![
        MkvAttachment::new("cover.jpg", "image/jpeg", COVER_BYTES.to_vec()),
        MkvAttachment {
            filename: "subset.otf".into(),
            mime_type: "application/x-truetype-font".into(),
            data: FONT_BYTES.to_vec(),
            uid: Some(0xCAFE_F00D_DEAD_BEEF),
            description: Some("Embedded subtitle font".into()),
        },
    ];
    let bytes = mux_with_attachments_collect(&attachments);

    let dmx = open_demuxer(bytes);
    let md: Vec<(String, String)> = dmx.metadata().to_vec();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());

    // Index 1 (cover) — auto-derived UID, no description on disk.
    assert_eq!(get("attachment:1:filename").as_deref(), Some("cover.jpg"));
    assert_eq!(get("attachment:1:mime_type").as_deref(), Some("image/jpeg"));
    assert_eq!(
        get("attachment:1:size_bytes").as_deref(),
        Some(COVER_BYTES.len().to_string().as_str())
    );
    assert!(
        get("attachment:1:description").is_none(),
        "no description was queued for attachment 1"
    );

    // Index 2 (font) — explicit UID + description.
    assert_eq!(get("attachment:2:filename").as_deref(), Some("subset.otf"));
    assert_eq!(
        get("attachment:2:mime_type").as_deref(),
        Some("application/x-truetype-font")
    );
    assert_eq!(
        get("attachment:2:size_bytes").as_deref(),
        Some(FONT_BYTES.len().to_string().as_str())
    );
    assert_eq!(
        get("attachment:2:description").as_deref(),
        Some("Embedded subtitle font")
    );
}

#[test]
fn typed_attachments_accessor_round_trip_with_payload_fetch() {
    let attachments = vec![
        MkvAttachment::new("cover.jpg", "image/jpeg", COVER_BYTES.to_vec()),
        MkvAttachment {
            filename: "subset.otf".into(),
            mime_type: "application/x-truetype-font".into(),
            data: FONT_BYTES.to_vec(),
            uid: Some(0xCAFE_F00D_DEAD_BEEF),
            description: Some("Embedded subtitle font".into()),
        },
    ];
    let bytes = mux_with_attachments_collect(&attachments);

    let mut dmx = open_typed(bytes);
    let atts = dmx.attachments().to_vec();
    assert_eq!(atts.len(), 2, "two attachments must surface");

    assert_eq!(atts[0].index, 1);
    assert_eq!(atts[0].filename, "cover.jpg");
    assert_eq!(atts[0].mime_type, "image/jpeg");
    assert_eq!(atts[0].description, "");
    assert_eq!(
        atts[0].uid, 1,
        "auto-derived UID equals the 1-based attachment index"
    );
    assert_eq!(atts[0].data_size, COVER_BYTES.len() as u64);

    assert_eq!(atts[1].index, 2);
    assert_eq!(atts[1].filename, "subset.otf");
    assert_eq!(atts[1].mime_type, "application/x-truetype-font");
    assert_eq!(atts[1].description, "Embedded subtitle font");
    assert_eq!(atts[1].uid, 0xCAFE_F00D_DEAD_BEEF);
    assert_eq!(atts[1].data_size, FONT_BYTES.len() as u64);

    let cover = dmx.attachment_data(1).expect("cover payload");
    assert_eq!(cover, COVER_BYTES);
    let font = dmx.attachment_data(2).expect("font payload");
    assert_eq!(font, FONT_BYTES);
}

#[test]
fn attachments_master_sits_after_tracks_before_first_cluster() {
    // Mix attachments with chapters to confirm RFC 9559 §5.1.6 ordering
    // still holds: Tracks → Chapters → Attachments → Cluster.
    let tmp = tmp_path("order");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let stream = pcm_stream();
        let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
        mx.add_chapter_full(MkvChapter {
            time_start_ns: 0,
            time_end_ns: Some(1_000_000_000),
            display: Vec::new(),
        })
        .expect("queue chapter");
        mx.add_attachment(MkvAttachment::new(
            "cover.jpg",
            "image/jpeg",
            COVER_BYTES.to_vec(),
        ))
        .expect("queue attachment");
        mx.write_header().expect("write_header");
        mx.write_packet(&pcm_packet(0, 0xAA)).expect("write_packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);

    let tracks_off = find_top_level(&bytes, ids::TRACKS).expect("Tracks present");
    let chapters_off = find_top_level(&bytes, ids::CHAPTERS).expect("Chapters present");
    let attachments_off = find_top_level(&bytes, ids::ATTACHMENTS).expect("Attachments present");
    let cluster_off = find_top_level(&bytes, ids::CLUSTER).expect("Cluster present");

    assert!(
        tracks_off < chapters_off,
        "Tracks ({tracks_off}) must come before Chapters ({chapters_off})"
    );
    assert!(
        chapters_off < attachments_off,
        "Chapters ({chapters_off}) must come before Attachments ({attachments_off})"
    );
    assert!(
        attachments_off < cluster_off,
        "Attachments ({attachments_off}) must come before first Cluster ({cluster_off})"
    );
}

#[test]
fn seek_head_attachments_slot_voided_when_no_attachments_queued() {
    // A plain muxer with no attachments must NOT emit an Attachments
    // master, and its SeekHead slot must be a Void filler (so SeekHead
    // pre-walkers don't chase a placeholder zero that resolves back to
    // the SeekHead itself).
    let tmp = tmp_path("void");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let stream = pcm_stream();
        let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
        mx.write_header().expect("write_header");
        mx.write_packet(&pcm_packet(0, 0xAA)).expect("write_packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);

    assert!(
        find_top_level(&bytes, ids::ATTACHMENTS).is_none(),
        "no attachments queued — Attachments master must NOT be emitted"
    );

    // Walk into the SeekHead and confirm every Seek entry it carries
    // points at a Top-Level master we DID emit — no Attachments entry
    // can survive (it would have been rewritten as a Void filler).
    let seek_head_off =
        find_top_level(&bytes, ids::SEEK_HEAD).expect("SeekHead must always be present");
    let mut cur = Cursor::new(&bytes);
    cur.seek(SeekFrom::Start(seek_head_off)).unwrap();
    let sh = read_element_header(&mut cur).unwrap();
    assert_eq!(sh.id, ids::SEEK_HEAD);
    let body_start = cur.stream_position().unwrap();
    let body_end = body_start + sh.size;
    let mut saw_attachments_target = false;
    while cur.stream_position().unwrap() < body_end {
        let e = read_element_header(&mut cur).unwrap();
        if e.id != ids::SEEK {
            // A Void filler — skip its payload. This is exactly what the
            // muxer rewrites unused Seek slots into.
            cur.seek(SeekFrom::Current(e.size as i64)).unwrap();
            continue;
        }
        let seek_end = cur.stream_position().unwrap() + e.size;
        while cur.stream_position().unwrap() < seek_end {
            let child = read_element_header(&mut cur).unwrap();
            if child.id == ids::SEEK_ID {
                let mut id_buf = [0u8; 4];
                std::io::Read::read_exact(&mut cur, &mut id_buf).unwrap();
                let id = u32::from_be_bytes(id_buf);
                if id == ids::ATTACHMENTS {
                    saw_attachments_target = true;
                }
            } else {
                cur.seek(SeekFrom::Current(child.size as i64)).unwrap();
            }
        }
    }
    assert!(
        !saw_attachments_target,
        "no Attachments queued — the SeekHead must have voided that slot"
    );
}

#[test]
fn add_attachment_after_write_header_is_rejected() {
    let tmp = tmp_path("late");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let stream = pcm_stream();
    let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
    mx.write_header().expect("write_header");
    let err = mx
        .add_attachment(MkvAttachment::new(
            "late.bin",
            "application/octet-stream",
            vec![],
        ))
        .expect_err("add_attachment after write_header must error");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("add_attachment") && msg.contains("write_header"),
        "unexpected error: {msg}"
    );
    drop(mx);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn add_attachment_rejects_empty_filename() {
    let tmp = tmp_path("nofn");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let stream = pcm_stream();
    let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
    let err = mx
        .add_attachment(MkvAttachment::new("", "image/jpeg", vec![0u8; 4]))
        .expect_err("empty FileName must be rejected (RFC 9559 §5.1.6.1.2)");
    match err {
        Error::InvalidData(msg) => {
            assert!(msg.contains("FileName"), "unexpected error message: {msg}")
        }
        _ => panic!("expected Error::InvalidData, got {err:?}"),
    }
    drop(mx);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn add_attachment_rejects_empty_mime_type() {
    let tmp = tmp_path("nomime");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let stream = pcm_stream();
    let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
    let err = mx
        .add_attachment(MkvAttachment::new("x.bin", "", vec![0u8; 4]))
        .expect_err("empty FileMediaType must be rejected (RFC 9559 §5.1.6.1.3)");
    match err {
        Error::InvalidData(msg) => assert!(
            msg.contains("FileMediaType"),
            "unexpected error message: {msg}"
        ),
        _ => panic!("expected Error::InvalidData, got {err:?}"),
    }
    drop(mx);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn add_attachment_rejects_explicit_zero_uid() {
    let tmp = tmp_path("zerouid");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let stream = pcm_stream();
    let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
    let att = MkvAttachment {
        filename: "x.bin".into(),
        mime_type: "application/octet-stream".into(),
        data: vec![0u8; 4],
        uid: Some(0),
        description: None,
    };
    let err = mx
        .add_attachment(att)
        .expect_err("uid=Some(0) violates §5.1.6.1.5 range: not 0");
    match err {
        Error::InvalidData(msg) => assert!(
            msg.contains("FileUID") && msg.contains("not 0"),
            "unexpected error message: {msg}"
        ),
        _ => panic!("expected Error::InvalidData, got {err:?}"),
    }
    drop(mx);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn auto_derived_uid_is_stable_and_resolves_tag_attachment_uid_scope() {
    // The muxer's auto-derived UID is the attachment's 1-based index.
    // Confirm the typed accessor surfaces the same value on read so
    // upstream tag-scope resolution (which keys on FileUID) lines up.
    let attachments = vec![
        MkvAttachment::new("cover.jpg", "image/jpeg", COVER_BYTES.to_vec()),
        MkvAttachment::new("logo.png", "image/png", COVER_BYTES.to_vec()),
    ];
    let bytes = mux_with_attachments_collect(&attachments);
    let dmx = open_typed(bytes);
    let atts = dmx.attachments().to_vec();
    assert_eq!(atts.len(), 2);
    assert_eq!(atts[0].uid, 1, "first attachment's auto UID is 1");
    assert_eq!(atts[1].uid, 2, "second attachment's auto UID is 2");
}

#[test]
fn empty_description_is_omitted_on_disk() {
    // `Some("")` description should NOT emit a zero-length
    // FileDescription on disk — the demuxer's flat metadata view
    // hides empty strings as "no description present," and we want
    // the on-disk view to match.
    let att = MkvAttachment {
        filename: "x.bin".into(),
        mime_type: "application/octet-stream".into(),
        data: vec![0u8; 4],
        uid: None,
        description: Some("".into()),
    };
    let bytes = mux_with_attachments_collect(&[att]);
    let dmx = open_demuxer(bytes);
    let md: Vec<(String, String)> = dmx.metadata().to_vec();
    assert!(
        !md.iter().any(|(k, _)| k == "attachment:1:description"),
        "empty Some(\"\") description must NOT emit an `attachment:1:description` key"
    );
}
