//! Front-`Cues` layout tests (`MkvMuxer::with_front_cues`,
//! RFC 9559 §25.3.3 "Optimum Layout with Cues at the Front").
//!
//! 1. The `Cues` element lands *before* the first Cluster, with a filler
//!    `Void` covering the reservation remainder; nothing Cues-shaped
//!    follows the last Cluster; the SeekHead `Cues` entry points at the
//!    front slot; seeking works off the front index.
//! 2. Exact-fit and off-by-one reservations: remainder 0 needs no
//!    filler; remainder 1 is absorbed by widening the `Cues` size VINT.
//! 3. A too-small reservation falls back to the ordinary end placement
//!    (Void stays, file valid, SeekHead points at the end Cues).
//! 4. Conflicts with `with_live_streaming` (both directions), floor and
//!    post-`write_header` validation.

use std::io::{Cursor, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo,
    TimeBase, WriteSeek,
};
use oxideav_mkv::ebml::read_element_header;
use oxideav_mkv::ids;
use oxideav_mkv::mux::MkvMuxer;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r416-frontcues-{tag}-{}-{n}.mkv",
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

fn packet(pts_ms: i64) -> Packet {
    let mut pkt = Packet::new(0, TimeBase::new(1, 1000), vec![0xAB; 64]);
    pkt.pts = Some(pts_ms);
    pkt.flags.keyframe = true;
    pkt
}

/// Mux a three-cluster file; `reserve` = front-Cues reservation.
fn mux_file(tag: &str, reserve: Option<u32>) -> Vec<u8> {
    let streams = vec![pcm_stream()];
    let tmp = tmp_path(tag);
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
        if let Some(r) = reserve {
            mx.with_front_cues(r).expect("front cues");
            assert_eq!(mx.front_cues_reserved(), Some(r as u64));
        }
        mx.write_header().expect("header");
        for pts in [0i64, 40, 6_000, 6_040, 12_000] {
            mx.write_packet(&packet(pts)).expect("packet");
        }
        mx.write_trailer().expect("trailer");
    }
    let bytes = std::fs::read(&tmp).expect("read back");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

/// Walk the Segment's children, returning `(id, header_offset_abs)` rows
/// (skipping over each element body; unknown-size Clusters are walked by
/// stepping their children, which the fixed layout here never needs past
/// the last cluster because Cues sits in front).
fn top_level_ids(bytes: &[u8]) -> Vec<(u32, u64)> {
    let mut cur = Cursor::new(bytes);
    let ebml = read_element_header(&mut cur).expect("ebml header");
    assert_eq!(ebml.id, ids::EBML_HEADER);
    cur.seek(SeekFrom::Current(ebml.size as i64)).expect("skip");
    let seg = read_element_header(&mut cur).expect("segment");
    assert_eq!(seg.id, ids::SEGMENT);
    let mut out = Vec::new();
    // The muxer's Segment is unknown-size; walk to EOF. Clusters are also
    // unknown-size, so walk *their* children too, attributing them to the
    // cluster (only top-level ids are recorded).
    loop {
        let pos = cur.position();
        let Ok(h) = read_element_header(&mut cur) else {
            break;
        };
        let is_top = matches!(
            h.id,
            x if x == ids::SEEK_HEAD
                || x == ids::INFO
                || x == ids::TRACKS
                || x == ids::CHAPTERS
                || x == ids::ATTACHMENTS
                || x == ids::TAGS
                || x == ids::CUES
                || x == ids::CLUSTER
                || x == ids::VOID
        );
        if is_top {
            out.push((h.id, pos));
        }
        if h.size == oxideav_mkv::ebml::VINT_UNKNOWN_SIZE {
            // Unknown-size Cluster: step into it (children are walked by
            // the same loop; none of them collides with the ids above).
            continue;
        }
        if is_top || h.size > 0 {
            cur.seek(SeekFrom::Current(h.size as i64))
                .expect("skip body");
        }
    }
    out
}

/// The SeekHead's Cues SeekPosition (Segment Position) + segment data
/// start, i.e. the absolute offset the index says Cues lives at.
fn seek_head_cues_target(bytes: Vec<u8>) -> Option<u64> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open");
    let seg_start = {
        // EBML header length + Segment id (4) + size VINT (1 for 0xFF).
        let mut found = None;
        for se in dmx.seek_entries() {
            if se.seek_id() == Some(ids::CUES) && se.has_position() {
                found = Some(se.seek_position());
            }
        }
        found
    };
    seg_start
}

#[test]
fn front_cues_land_before_first_cluster() {
    let bytes = mux_file("front", Some(4096));
    let layout = top_level_ids(&bytes);
    let cues_pos = layout.iter().position(|(id, _)| *id == ids::CUES);
    let first_cluster = layout.iter().position(|(id, _)| *id == ids::CLUSTER);
    let (cues_idx, cluster_idx) = (cues_pos.expect("cues"), first_cluster.expect("cluster"));
    assert!(
        cues_idx < cluster_idx,
        "Cues must precede the first Cluster: {layout:?}"
    );
    // Exactly one Cues element in the file (no duplicate end placement).
    assert_eq!(layout.iter().filter(|(id, _)| *id == ids::CUES).count(), 1);
    // A filler Void directly follows the Cues inside the reservation.
    assert_eq!(layout[cues_idx + 1].0, ids::VOID, "{layout:?}");
    // Nothing after the last Cluster's blocks (the file ends inside the
    // final unknown-size Cluster's children).
    let cues_abs = layout[cues_idx].1;

    // The SeekHead agrees with the on-disk position.
    let target = seek_head_cues_target(bytes.clone()).expect("SeekHead Cues entry");
    // Segment data start = EBML header len + 4 (Segment id) + 1 (0xFF).
    let mut cur = Cursor::new(&bytes);
    let ebml = read_element_header(&mut cur).expect("ebml");
    let seg_data_start = ebml.header_len as u64 + ebml.size + 4 + 1;
    assert_eq!(seg_data_start + target, cues_abs);

    // The index is fully usable: open + seek to the last cluster's time.
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open");
    assert_eq!(dmx.cue_points().len(), 3, "one cue per cluster");
    // `seek_to` takes stream time-base ticks (1 ms at this scale).
    Demuxer::seek_to(&mut dmx, 0, 12_000).expect("seek via front cues");
    let pkt = Demuxer::next_packet(&mut dmx).expect("packet after seek");
    assert_eq!(pkt.pts, Some(12_000));
}

#[test]
fn exact_fit_and_one_byte_remainder() {
    // Measure the natural Cues size with a roomy reservation.
    let bytes = mux_file("measure", Some(4096));
    let layout = top_level_ids(&bytes);
    let cues_idx = layout
        .iter()
        .position(|(id, _)| *id == ids::CUES)
        .expect("cues");
    let cues_abs = layout[cues_idx].1 as usize;
    let mut cur = Cursor::new(&bytes[cues_abs..]);
    let h = read_element_header(&mut cur).expect("cues header");
    let natural_len = (h.header_len as u64 + h.size) as u32;

    // Exact fit: no filler Void inside the reservation.
    let bytes = mux_file("exact", Some(natural_len));
    let layout = top_level_ids(&bytes);
    let cues_idx = layout
        .iter()
        .position(|(id, _)| *id == ids::CUES)
        .expect("cues");
    assert!(
        layout[cues_idx + 1].0 != ids::VOID,
        "exact fit leaves no filler Void: {layout:?}"
    );
    assert_eq!(layout.iter().filter(|(id, _)| *id == ids::CUES).count(), 1);

    // One-byte remainder: the Cues size VINT is widened to fill the slot
    // exactly (no filler Void, still a single front Cues).
    let bytes = mux_file("plus1", Some(natural_len + 1));
    let layout = top_level_ids(&bytes);
    let cues_idx = layout
        .iter()
        .position(|(id, _)| *id == ids::CUES)
        .expect("cues");
    let cluster_idx = layout
        .iter()
        .position(|(id, _)| *id == ids::CLUSTER)
        .expect("cluster");
    assert!(cues_idx < cluster_idx);
    assert!(layout[cues_idx + 1].0 != ids::VOID, "{layout:?}");
    let mut cur = Cursor::new(&bytes[layout[cues_idx].1 as usize..]);
    let h = read_element_header(&mut cur).expect("cues header");
    assert_eq!(
        h.header_len as u64 + h.size,
        natural_len as u64 + 1,
        "the widened size VINT absorbs the extra byte"
    );
    // Still fully readable.
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open");
    assert_eq!(dmx.cue_points().len(), 3);
}

#[test]
fn too_small_reservation_falls_back_to_end_placement() {
    let bytes = mux_file("fallback", Some(32));
    let layout = top_level_ids(&bytes);
    let cues_idx = layout
        .iter()
        .position(|(id, _)| *id == ids::CUES)
        .expect("cues");
    let last_cluster = layout
        .iter()
        .rposition(|(id, _)| *id == ids::CLUSTER)
        .expect("cluster");
    assert!(
        cues_idx > last_cluster,
        "index falls back after the last Cluster: {layout:?}"
    );
    // The unused reservation Void sits before the first Cluster.
    let first_cluster = layout
        .iter()
        .position(|(id, _)| *id == ids::CLUSTER)
        .unwrap();
    assert!(
        layout[..first_cluster]
            .iter()
            .any(|(id, _)| *id == ids::VOID),
        "reservation Void remains: {layout:?}"
    );
    // The SeekHead points at the end-placed Cues and seeking works.
    let target = seek_head_cues_target(bytes.clone()).expect("SeekHead Cues entry");
    let mut cur = Cursor::new(&bytes);
    let ebml = read_element_header(&mut cur).expect("ebml");
    let seg_data_start = ebml.header_len as u64 + ebml.size + 4 + 1;
    assert_eq!(seg_data_start + target, layout[cues_idx].1);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open");
    Demuxer::seek_to(&mut dmx, 0, 6_000).expect("seek via end cues");
    let pkt = Demuxer::next_packet(&mut dmx).expect("packet");
    assert_eq!(pkt.pts, Some(6_000));
}

#[test]
fn validation_and_conflicts() {
    let streams = vec![pcm_stream()];
    let tmp = tmp_path("validate");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
    match mx.with_front_cues(31) {
        Err(Error::InvalidData(msg)) => assert!(msg.contains("reserved_bytes"), "{msg}"),
        _ => panic!("floor must be enforced"),
    }
    mx.with_front_cues(64).expect("valid reserve");
    match mx.with_live_streaming() {
        Err(Error::Other(msg)) => assert!(msg.contains("with_front_cues"), "{msg}"),
        _ => panic!("live after front-cues must be rejected"),
    }
    drop(mx);
    let _ = std::fs::remove_file(&tmp);

    let tmp = tmp_path("validate2");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
    mx.with_live_streaming().expect("live");
    match mx.with_front_cues(4096) {
        Err(Error::Other(msg)) => assert!(msg.contains("with_live_streaming"), "{msg}"),
        _ => panic!("front-cues on a live muxer must be rejected"),
    }
    mx.write_header().expect("header");
    match mx.with_front_cues(4096) {
        Err(Error::Other(msg)) => assert!(msg.contains("write_header"), "{msg}"),
        _ => panic!("post-header must be rejected"),
    }
    drop(mx);
    let _ = std::fs::remove_file(&tmp);
}
