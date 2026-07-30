//! RFC 9559 §25.2 SeekHead-expansion `Void` reservation
//! (`MkvMuxer::with_seek_head_expansion_void`).
//!
//! "It is RECOMMENDED that the first SeekHead element be followed by a
//! Void element to allow for the SeekHead element to be expanded to
//! cover new Top-Level Elements that could be added to the Matroska
//! file, such as Tags, Chapters, and Attachments elements. The size of
//! this Void element should be adjusted depending on the Tags,
//! Chapters, and Attachments elements in the Matroska file."
//!
//! The reservation is opt-in and caller-sized. These tests pin the
//! on-disk layout (a `Void` of exactly the requested size immediately
//! after the SeekHead, before `Info`), that the SeekHead's patched
//! SeekPositions still land on their targets with the Void in between,
//! the demux round-trip, the WebM-profile conformance of the layout,
//! and the knob's conflict/validation surface.

use std::io::{Cursor, Seek, SeekFrom};

use oxideav_core::{CodecId, CodecParameters, Muxer, Packet, StreamInfo, TimeBase};
use oxideav_core::{Demuxer, Error, ReadSeek, WriteSeek};
use oxideav_mkv::ebml::{read_element_header, VINT_UNKNOWN_SIZE};
use oxideav_mkv::ids;
use oxideav_mkv::mux::MkvMuxer;

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

fn video_packet(pts: i64) -> Packet {
    let mut p = Packet::new(0, TimeBase::new(1, 1000), vec![0x42; 64]);
    p.pts = Some(pts);
    p.duration = Some(1000);
    p.flags.keyframe = true;
    p
}

fn tmp_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r434-shexp-{tag}-{}.mkv",
        std::process::id()
    ))
}

fn mux_with<F>(tag: &str, configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path(tag);
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer construct");
        configure(&mut mx);
        mx.write_header().expect("write_header");
        mx.write_packet(&video_packet(0)).expect("packet 0");
        mx.write_packet(&video_packet(1000)).expect("packet 1");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

/// Return `(segment_data_start, children)` where `children` is each
/// Top-Level element's `(relative offset, id, total encoded length)` in
/// document order (Clusters walked with the sibling-termination rule).
fn segment_children(raw: &[u8]) -> (u64, Vec<(u64, u32, u64)>) {
    let mut cur = Cursor::new(raw);
    let ebml_hdr = read_element_header(&mut cur).expect("EBML header");
    assert_eq!(ebml_hdr.id, ids::EBML_HEADER);
    cur.seek(SeekFrom::Current(ebml_hdr.size as i64)).unwrap();
    let seg = read_element_header(&mut cur).expect("Segment header");
    assert_eq!(seg.id, ids::SEGMENT);
    let segment_data_start = cur.position();
    let end = if seg.size == VINT_UNKNOWN_SIZE {
        raw.len() as u64
    } else {
        segment_data_start + seg.size
    };
    let mut out = Vec::new();
    while cur.position() < end {
        let pos = cur.position();
        let e = read_element_header(&mut cur).expect("segment child header");
        let body_start = cur.position();
        if e.id == ids::CLUSTER && e.size == VINT_UNKNOWN_SIZE {
            // Walk children until a sibling Top-Level element appears.
            let mut cend = end;
            while cur.position() < end {
                let cpos = cur.position();
                let ce = match read_element_header(&mut cur) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let is_child = matches!(
                    ce.id,
                    ids::TIMECODE
                        | ids::SIMPLE_BLOCK
                        | ids::BLOCK_GROUP
                        | ids::BLOCK
                        | ids::BLOCK_DURATION
                        | ids::REFERENCE_BLOCK
                        | ids::VOID
                        | ids::CRC32
                        | ids::PREV_SIZE
                        | ids::POSITION
                );
                if !is_child || ce.size == VINT_UNKNOWN_SIZE {
                    cur.seek(SeekFrom::Start(cpos)).unwrap();
                    cend = cpos;
                    break;
                }
                cur.seek(SeekFrom::Current(ce.size as i64)).unwrap();
                cend = cur.position();
            }
            out.push((pos - segment_data_start, e.id, cend - pos));
            continue;
        }
        assert_ne!(e.size, VINT_UNKNOWN_SIZE, "unexpected unknown-size child");
        let total = (body_start - pos) + e.size;
        out.push((pos - segment_data_start, e.id, total));
        cur.seek(SeekFrom::Start(body_start + e.size)).unwrap();
    }
    (segment_data_start, out)
}

#[test]
fn expansion_void_sits_directly_after_seek_head_with_exact_size() {
    const RESERVED: u64 = 128;
    let raw = mux_with("layout", |mx| {
        mx.with_seek_head_expansion_void(RESERVED as u32)
            .expect("knob");
        assert_eq!(mx.seek_head_expansion_void(), Some(RESERVED));
    });
    let (_, children) = segment_children(&raw);
    assert_eq!(children[0].1, ids::SEEK_HEAD, "first child is SeekHead");
    let (void_off, void_id, void_total) = children[1];
    assert_eq!(void_id, ids::VOID, "second child is the §25.2 Void");
    assert_eq!(
        void_off,
        children[0].0 + children[0].2,
        "Void starts immediately after the SeekHead"
    );
    assert_eq!(void_total, RESERVED, "Void spans exactly the reservation");
    assert_eq!(children[2].1, ids::INFO, "Info follows the Void");
}

#[test]
fn default_layout_writes_no_expansion_void() {
    let raw = mux_with("default", |_| {});
    let (_, children) = segment_children(&raw);
    assert_eq!(children[0].1, ids::SEEK_HEAD);
    assert_eq!(
        children[1].1,
        ids::INFO,
        "without the knob Info directly follows the SeekHead"
    );
}

#[test]
fn seek_positions_still_land_on_targets_and_file_demuxes() {
    let raw = mux_with("roundtrip", |mx| {
        mx.with_seek_head_expansion_void(64).expect("knob");
    });
    let (segment_data_start, children) = segment_children(&raw);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(raw.clone()));
    let mut dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver)
        .expect("demux open_typed");
    // Every SeekHead entry that carries a position must land on the
    // matching element header, Void notwithstanding.
    assert!(!dmx.seek_entries().is_empty());
    for entry in dmx.seek_entries().to_vec() {
        let Some(id) = entry.seek_id() else { continue };
        if !entry.has_position() {
            continue;
        }
        let target = children
            .iter()
            .find(|(_, cid, _)| *cid == id)
            .unwrap_or_else(|| panic!("no on-disk element for SeekID 0x{id:X}"));
        assert_eq!(
            segment_data_start + entry.seek_position(),
            segment_data_start + target.0,
            "SeekPosition for 0x{id:X} lands on the element header"
        );
    }
    // Packets flow.
    assert_eq!(dmx.next_packet().expect("packet 0").data, vec![0x42; 64]);
    assert_eq!(dmx.next_packet().expect("packet 1").data, vec![0x42; 64]);
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
}

#[test]
fn strict_webm_layout_stays_conformant() {
    // Void is guidelines-Supported, so the §25.2 reservation is legal
    // under strict WebM gating.
    let tmp = tmp_path("webm");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_webm(ws, &[video_stream()]).expect("webm muxer");
        mx.with_seek_head_expansion_void(48).expect("knob");
        mx.write_header().expect("write_header");
        mx.write_packet(&video_packet(0)).expect("packet");
        mx.write_trailer().expect("write_trailer");
    }
    let raw = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    let report = oxideav_mkv::webm::scan(&mut Cursor::new(&raw)).expect("scan");
    assert!(
        report.is_conformant(),
        "findings: {:?} (stopped: {:?})",
        report.findings,
        report.scan_stopped_at
    );
}

#[test]
fn knob_validation_and_conflicts() {
    let sink: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    let mut mx = MkvMuxer::new_matroska(sink, &[video_stream()]).expect("muxer");
    // Too small for the smallest encodable Void.
    assert!(mx.with_seek_head_expansion_void(1).is_err());
    // Minimum accepted.
    mx.with_seek_head_expansion_void(2).expect("min size");
    // Live streaming conflicts with an installed reservation.
    assert!(mx.with_live_streaming().is_err());

    // Reverse direction: live streaming first, then the knob.
    let sink: Box<dyn WriteSeek> = Box::new(Cursor::new(Vec::new()));
    let mut live = MkvMuxer::new_matroska(sink, &[video_stream()]).expect("muxer");
    live.with_live_streaming().expect("live");
    assert!(live.with_seek_head_expansion_void(64).is_err());

    // After write_header: rejected.
    let tmp = tmp_path("late");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut late = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer");
    late.write_header().expect("write_header");
    assert!(late.with_seek_head_expansion_void(64).is_err());
    let _ = std::fs::remove_file(&tmp);
}
