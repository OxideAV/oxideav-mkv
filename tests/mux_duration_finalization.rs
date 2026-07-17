//! Two-pass `Duration` finalization tests
//! (`MkvMuxer::with_duration_finalization`, RFC 9559 §5.1.2.10).
//!
//! 1. The measured duration (max packet end time across streams) lands in
//!    `Info > Duration` and reads back through the demuxer's
//!    `duration_micros()`, and the patched Info still CRC-validates
//!    (RFC 8794 §11.3.1 — the patch rewrites the CRC payload).
//! 2. Packet `duration` fields extend the measured end time beyond the
//!    last pts.
//! 3. A zero-packet mux leaves the reserved `Void` in place — no bogus
//!    `Duration`, demuxer reports no known duration.
//! 4. A crashed producer (header written, trailer never runs) leaves a
//!    structurally-clean file with a `Void` and no duration.
//! 5. Conflict rejections in both directions with `set_duration` and
//!    `with_live_streaming`; post-`write_header` rejection.
//! 6. Strict WebM mode: the patch works without a CRC child and the
//!    output still scans conformant.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo,
    TimeBase, WriteSeek,
};
use oxideav_mkv::mux::MkvMuxer;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r416-durfin-{tag}-{}-{n}.mkv",
        std::process::id()
    ))
}

fn pcm_stream(index: u32) -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
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

fn packet(index: u32, pts_ms: i64, dur_ms: Option<i64>) -> Packet {
    let mut pkt = Packet::new(index, TimeBase::new(1, 1000), vec![0xAB; 32]);
    pkt.pts = Some(pts_ms);
    pkt.duration = dur_ms;
    pkt.flags.keyframe = true;
    pkt
}

fn open_typed(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

#[test]
fn measured_duration_lands_and_info_crc_revalidates() {
    let streams = vec![pcm_stream(0)];
    let tmp = tmp_path("basic");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
        mx.with_duration_finalization().expect("finalization");
        assert!(mx.duration_finalization());
        mx.write_header().expect("header");
        for pts in [0i64, 40, 6_000, 6_040, 12_040] {
            mx.write_packet(&packet(0, pts, None)).expect("packet");
        }
        mx.write_trailer().expect("trailer");
    }
    let bytes = std::fs::read(&tmp).expect("read back");
    let _ = std::fs::remove_file(&tmp);

    let dmx = open_typed(bytes);
    // Last packet pts 12_040 ms with no duration → measured end 12_040 ms.
    assert_eq!(Demuxer::duration_micros(&dmx), Some(12_040_000));
    // The Info CRC was rewritten over the patched body and must validate.
    let info_crcs: Vec<_> = dmx
        .crc_status()
        .iter()
        .filter(|c| c.element_id == oxideav_mkv::ids::INFO)
        .collect();
    assert_eq!(info_crcs.len(), 1, "Info must carry exactly one CRC status");
    assert!(
        info_crcs[0].is_valid(),
        "patched Info CRC must validate (stored {:08X} computed {:08X})",
        info_crcs[0].stored,
        info_crcs[0].computed
    );
    assert!(dmx.crc_status().iter().all(|c| c.is_valid()));
}

#[test]
fn packet_duration_extends_the_measured_end() {
    let streams = vec![vp9_stream(0), pcm_stream(1)];
    let tmp = tmp_path("durext");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
        mx.with_duration_finalization().expect("finalization");
        mx.write_header().expect("header");
        // Audio reaches 1_000 ms; video's last packet starts earlier but
        // its explicit duration pushes the measured end to 1_500 ms.
        mx.write_packet(&packet(1, 960, Some(40))).expect("audio");
        mx.write_packet(&packet(0, 1_100, Some(400)))
            .expect("video");
        mx.write_trailer().expect("trailer");
    }
    let bytes = std::fs::read(&tmp).expect("read back");
    let _ = std::fs::remove_file(&tmp);
    let dmx = open_typed(bytes);
    assert_eq!(Demuxer::duration_micros(&dmx), Some(1_500_000));
}

#[test]
fn zero_packet_mux_leaves_void_and_no_duration() {
    let streams = vec![pcm_stream(0)];
    let tmp = tmp_path("zeropkt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
        mx.with_duration_finalization().expect("finalization");
        mx.write_header().expect("header");
        mx.write_trailer().expect("trailer");
    }
    let bytes = std::fs::read(&tmp).expect("read back");
    let _ = std::fs::remove_file(&tmp);
    // A zero-packet file has no Cluster, which both demux opens reject
    // (pre-existing behaviour), so assert the on-disk layout directly
    // with the EBML walker: the Info body must carry the intact Void
    // (the reserved slot) and no Duration element, and its CRC must
    // still validate over the unpatched body.
    let (info_body, crc_ok) = read_info_body(&bytes);
    assert!(crc_ok, "unpatched Info CRC must validate");
    let kids = child_ids(&info_body);
    assert!(kids.contains(&oxideav_mkv::ids::VOID), "reserved Void kept");
    assert!(
        !kids.contains(&oxideav_mkv::ids::DURATION),
        "no bogus Duration on a zero-packet mux"
    );
}

/// Walk `bytes` to the first `Info` element; return its body with any
/// leading CRC-32 child peeled off, plus whether that CRC validated
/// (`true` when no CRC child is present).
fn read_info_body(bytes: &[u8]) -> (Vec<u8>, bool) {
    use oxideav_mkv::ebml::{crc32_ieee, read_element_header};
    use std::io::{Read, Seek, SeekFrom};
    let mut cur = Cursor::new(bytes);
    let ebml = read_element_header(&mut cur).expect("ebml header");
    assert_eq!(ebml.id, oxideav_mkv::ids::EBML_HEADER);
    cur.seek(SeekFrom::Current(ebml.size as i64))
        .expect("skip ebml");
    let seg = read_element_header(&mut cur).expect("segment header");
    assert_eq!(seg.id, oxideav_mkv::ids::SEGMENT);
    loop {
        let h = read_element_header(&mut cur).expect("segment child");
        if h.id == oxideav_mkv::ids::INFO {
            let mut body = vec![0u8; h.size as usize];
            cur.read_exact(&mut body).expect("info body");
            // Peel a leading CRC-32 child (id 0xBF, 4-byte payload).
            if body.len() >= 6 && body[0] == 0xBF && body[1] == 0x84 {
                let stored = u32::from_le_bytes([body[2], body[3], body[4], body[5]]);
                let rest = body[6..].to_vec();
                let ok = crc32_ieee(&rest) == stored;
                return (rest, ok);
            }
            return (body, true);
        }
        cur.seek(SeekFrom::Current(h.size as i64))
            .expect("skip child");
    }
}

/// The child element IDs of a master body, in order.
fn child_ids(body: &[u8]) -> Vec<u32> {
    use oxideav_mkv::ebml::read_element_header;
    use std::io::{Seek, SeekFrom};
    let mut cur = Cursor::new(body);
    let mut out = Vec::new();
    while (cur.position() as usize) < body.len() {
        let h = read_element_header(&mut cur).expect("child header");
        out.push(h.id);
        cur.seek(SeekFrom::Current(h.size as i64)).expect("skip");
    }
    out
}

#[test]
fn crashed_producer_leaves_clean_void() {
    let streams = vec![pcm_stream(0)];
    let tmp = tmp_path("crash");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
        mx.with_duration_finalization().expect("finalization");
        mx.write_header().expect("header");
        mx.write_packet(&packet(0, 0, Some(40))).expect("packet");
        // No write_trailer: simulate a crash. Drop flushes nothing extra.
    }
    let bytes = std::fs::read(&tmp).expect("read back");
    let _ = std::fs::remove_file(&tmp);
    // The resilient reader sees a truncated stream with a Void where
    // Duration would be — no duration, valid Info CRC, packets intact.
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_mkv::demux::open_resilient_typed(rs, &oxideav_core::NullCodecResolver)
        .expect("resilient open");
    assert_eq!(Demuxer::duration_micros(&dmx), None);
    let mut n = 0;
    while Demuxer::next_packet(&mut dmx).is_ok() {
        n += 1;
    }
    assert_eq!(n, 1);
}

#[test]
fn conflicts_are_rejected_in_both_directions() {
    let streams = vec![pcm_stream(0)];

    // set_duration then with_duration_finalization.
    let tmp = tmp_path("conf1");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
    mx.set_duration(std::time::Duration::from_secs(5))
        .expect("explicit");
    match mx.with_duration_finalization() {
        Err(Error::Other(msg)) => assert!(msg.contains("set_duration"), "{msg}"),
        _ => panic!("must reject finalization after set_duration"),
    }
    drop(mx);
    let _ = std::fs::remove_file(&tmp);

    // with_duration_finalization then set_duration / with_live_streaming.
    let tmp = tmp_path("conf2");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
    mx.with_duration_finalization().expect("finalization");
    match mx.set_duration(std::time::Duration::from_secs(5)) {
        Err(Error::Other(msg)) => assert!(msg.contains("with_duration_finalization"), "{msg}"),
        _ => panic!("must reject set_duration after finalization"),
    }
    match mx.with_live_streaming() {
        Err(Error::Other(msg)) => assert!(msg.contains("with_duration_finalization"), "{msg}"),
        _ => panic!("must reject live streaming after finalization"),
    }
    drop(mx);
    let _ = std::fs::remove_file(&tmp);

    // live streaming then finalization.
    let tmp = tmp_path("conf3");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
    mx.with_live_streaming().expect("live");
    match mx.with_duration_finalization() {
        Err(Error::Other(msg)) => assert!(msg.contains("with_live_streaming"), "{msg}"),
        _ => panic!("must reject finalization on a live muxer"),
    }
    drop(mx);
    let _ = std::fs::remove_file(&tmp);

    // post-write_header rejection.
    let tmp = tmp_path("conf4");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
    mx.write_header().expect("header");
    match mx.with_duration_finalization() {
        Err(Error::Other(msg)) => assert!(msg.contains("write_header"), "{msg}"),
        _ => panic!("must reject finalization after write_header"),
    }
    drop(mx);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn strict_webm_finalization_patches_without_crc_and_scans_conformant() {
    let streams = vec![vp9_stream(0)];
    let tmp = tmp_path("webm");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_webm(ws, &streams).expect("webm muxer");
        mx.with_duration_finalization().expect("finalization");
        mx.write_header().expect("header");
        mx.write_packet(&packet(0, 0, Some(40))).expect("p0");
        mx.write_packet(&packet(0, 40, Some(40))).expect("p1");
        mx.write_trailer().expect("trailer");
    }
    let bytes = std::fs::read(&tmp).expect("read back");
    let _ = std::fs::remove_file(&tmp);

    let report = oxideav_mkv::webm::scan(&mut Cursor::new(&bytes)).expect("scan");
    assert!(
        report.is_conformant(),
        "findings: {:?} stopped: {:?}",
        report.findings,
        report.scan_stopped_at
    );
    let dmx = open_typed(bytes);
    assert_eq!(Demuxer::duration_micros(&dmx), Some(80_000));
    assert!(dmx.crc_status().is_empty(), "strict WebM writes no CRC");
}
