//! Mux-side CRC-32 round-trip tests (RFC 8794 §11.3.1, RFC 9559 §6.2).
//!
//! The muxer prepends a `CRC-32` child to every Top-Level master it can
//! buffer end-to-end before flushing — currently `Info`, `Tracks`,
//! `Chapters` (when present), `Attachments` (when present), and `Cues`.
//! These tests round-trip a muxed file through the demuxer and assert
//! that every emitted Top-Level master surfaces in `crc_status()` with a
//! matching stored/computed pair (i.e. `is_valid() == true`).
//!
//! Reference: RFC 9559 §6.2 ("In Matroska, all Top-Level Elements of an
//! EBML Document SHOULD include a CRC-32 element as their first Child
//! Element"); RFC 8794 §11.3.1 ("the CRC value MUST be computed on a
//! little-endian bytestream and MUST use little-endian storage").
//!
//! These tests use only the production EBML helpers — no third-party
//! Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, NullCodecResolver, Packet, ReadSeek, StreamInfo,
    TimeBase, WriteSeek,
};
use oxideav_mkv::ids;
use oxideav_mkv::mux::{ChapterDisplay, MkvAttachment, MkvChapter, MkvMuxer};

/// Distinct temp-file path per test instance — cargo's default parallel
/// runner would otherwise share the same path between concurrent tests
/// and cause spurious "file already exists" / "file changed" failures.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r202-mux-crc-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

fn pcm_stream() -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.sample_rate = Some(48_000);
    p.channels = Some(1);
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
    let mut pkt = Packet::new(0, TimeBase::new(1, 1000), vec![payload; 8]);
    pkt.pts = Some(pts_ms);
    pkt.duration = Some(1);
    pkt.flags.keyframe = true;
    pkt
}

/// Mux a minimal MKV with the supplied chapter / attachment lists to a
/// temp file, slurp the bytes back, and return them. The temp file is
/// deleted before return.
fn mux_minimal(chapters: &[MkvChapter], attachments: &[MkvAttachment]) -> Vec<u8> {
    let tmp = tmp_path("min");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let writer: Box<dyn WriteSeek> = Box::new(f);
        let mut mx: Box<MkvMuxer> = Box::new(
            MkvMuxer::new_matroska(writer, std::slice::from_ref(&pcm_stream()))
                .expect("muxer open"),
        );
        for ch in chapters {
            mx.add_chapter_full(ch.clone()).expect("add chapter");
        }
        for att in attachments {
            mx.add_attachment(att.clone()).expect("add attachment");
        }
        mx.write_header().expect("hdr");
        for i in 0..3u8 {
            mx.write_packet(&pcm_packet(i as i64, 0xC0 | i))
                .expect("write packet");
        }
        mx.write_trailer().expect("trailer");
    }
    let bytes = std::fs::read(&tmp).expect("read back");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

/// Open `bytes` through the demuxer and return the `crc_status()`
/// snapshot — drains `next_packet` until EOF so any lazy Cluster CRCs
/// are also recorded, even though Clusters carry no CRC on this side.
fn collect_crc_statuses(bytes: Vec<u8>) -> Vec<(u32, bool, u32, u32)> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_mkv::demux::open_typed(rs, &NullCodecResolver).expect("demux open");
    // Drain packets so cluster CRC statuses get logged (currently the
    // muxer doesn't add a Cluster CRC, but draining keeps the test
    // future-proof against changes there).
    loop {
        match dmx.next_packet() {
            Ok(_) => continue,
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("unexpected demux error: {e}"),
        }
    }
    dmx.crc_status()
        .iter()
        .map(|s| (s.element_id, s.is_valid(), s.stored, s.computed))
        .collect()
}

#[test]
fn info_tracks_cues_carry_valid_crc() {
    let bytes = mux_minimal(&[], &[]);
    let statuses = collect_crc_statuses(bytes);
    let ids_seen: Vec<u32> = statuses.iter().map(|(id, _, _, _)| *id).collect();
    assert!(
        ids_seen.contains(&ids::INFO),
        "Info must carry a CRC-32, statuses={ids_seen:?}"
    );
    assert!(
        ids_seen.contains(&ids::TRACKS),
        "Tracks must carry a CRC-32, statuses={ids_seen:?}"
    );
    assert!(
        ids_seen.contains(&ids::CUES),
        "Cues must carry a CRC-32 (muxer always writes cues for audio), statuses={ids_seen:?}"
    );
    for (id, valid, stored, computed) in &statuses {
        assert!(
            *valid,
            "CRC for element 0x{id:08X} must validate: stored=0x{stored:08X} computed=0x{computed:08X}",
        );
    }
}

#[test]
fn chapters_master_carries_valid_crc_when_present() {
    let chap = MkvChapter {
        time_start_ns: 0,
        time_end_ns: Some(500_000_000),
        display: vec![ChapterDisplay {
            title: "Intro".into(),
            language: "eng".into(),
            country: None,
        }],
        ..Default::default()
    };
    let bytes = mux_minimal(std::slice::from_ref(&chap), &[]);
    let statuses = collect_crc_statuses(bytes);
    let chapters_status = statuses
        .iter()
        .find(|(id, _, _, _)| *id == ids::CHAPTERS)
        .expect("Chapters must surface a CRC-32 status when chapters were added");
    assert!(
        chapters_status.1,
        "Chapters CRC must validate, stored=0x{:08X} computed=0x{:08X}",
        chapters_status.2, chapters_status.3
    );
}

#[test]
fn attachments_master_carries_valid_crc_when_present() {
    let att = MkvAttachment::new(
        "cover.png",
        "image/png",
        vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
    );
    let bytes = mux_minimal(&[], std::slice::from_ref(&att));
    let statuses = collect_crc_statuses(bytes);
    let att_status = statuses
        .iter()
        .find(|(id, _, _, _)| *id == ids::ATTACHMENTS)
        .expect("Attachments must surface a CRC-32 status when attachments were added");
    assert!(
        att_status.1,
        "Attachments CRC must validate, stored=0x{:08X} computed=0x{:08X}",
        att_status.2, att_status.3
    );
}

#[test]
fn no_seek_head_or_cluster_crc_is_emitted() {
    // The muxer deliberately does NOT add a CRC-32 to the SeekHead (the
    // SeekHead Cues entry is patched in `write_trailer`, which would
    // invalidate any CRC computed up front) or to Cluster (Clusters are
    // streamed with the unknown-size VINT and RFC 8794 §11.3.1 requires a
    // bounded body for CRC). This test pins both omissions so a future
    // change that adds either CRC trips the test and triggers a
    // documentation update.
    let bytes = mux_minimal(&[], &[]);
    let statuses = collect_crc_statuses(bytes);
    for (id, _, _, _) in &statuses {
        assert_ne!(
            *id,
            ids::SEEK_HEAD,
            "muxer must not emit a SeekHead CRC (would be invalidated by the trailer-time Cues patch)",
        );
        assert_ne!(
            *id,
            ids::CLUSTER,
            "muxer must not emit a Cluster CRC (unknown-size Clusters have no bounded body to CRC)",
        );
    }
}

#[test]
fn crc_byte_count_matches_six_per_master() {
    // Sanity-check that each CRC-32 child is exactly the 6 bytes the
    // serialiser advertises: 0xBF id + 0x84 size-VINT + 4 payload
    // bytes. We assert by counting `0xBF 0x84` byte pairs in the
    // header bytes — each pair must be followed by exactly 4 bytes
    // of payload, and there must be one per Top-Level master we
    // CRC'd (Info, Tracks, Cues = 3 minimum).
    let bytes = mux_minimal(&[], &[]);
    let mut crc_count = 0usize;
    let mut i = 0usize;
    while i + 6 <= bytes.len() {
        if bytes[i] == 0xBF && bytes[i + 1] == 0x84 {
            crc_count += 1;
        }
        i += 1;
    }
    // A spurious 0xBF 0x84 in the payload (e.g. inside CodecPrivate or a
    // packet) is theoretically possible; for the all-PCM fixture the
    // packet payloads are 0xC0..0xC2 and CodecPrivate is empty, so the
    // count is exact.
    assert!(
        crc_count >= 3,
        "expected at least 3 CRC-32 children for Info/Tracks/Cues, found {crc_count}",
    );
}
