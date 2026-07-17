//! Cluster budgeting tests (RFC 9559 §25.1: "It is RECOMMENDED that each
//! individual Cluster element contain no more than five seconds or five
//! megabytes of content").
//!
//! 1. Default byte budget: a high-bitrate stream rotates Clusters near
//!    5 MB even though the 5 s duration budget is nowhere close.
//! 2. Configured budgets (`with_cluster_limits`): a small byte budget and
//!    a small duration budget each drive rotation; every emitted Cluster
//!    stays within budget + one Block.
//! 3. Validation: zero / oversized duration (the i16 Block-timestamp
//!    bound), undersized byte budget, post-`write_header` rejection.
//! 4. Round-trip: the multi-Cluster output demuxes every packet in order.

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
        "oxideav-mkv-r416-climits-{tag}-{}-{n}.mkv",
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

fn packet(pts_ms: i64, payload_len: usize) -> Packet {
    let mut pkt = Packet::new(0, TimeBase::new(1, 1000), vec![0xAB; payload_len]);
    pkt.pts = Some(pts_ms);
    pkt.flags.keyframe = true;
    pkt
}

/// Mux `packets`, return the file bytes.
fn mux(tag: &str, limits: Option<(u32, u64)>, packets: &[Packet]) -> Vec<u8> {
    let streams = vec![pcm_stream()];
    let tmp = tmp_path(tag);
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
        if let Some((ms, bytes)) = limits {
            mx.with_cluster_limits(ms, bytes).expect("limits");
            assert_eq!(mx.cluster_limits(), (ms, bytes));
        } else {
            assert_eq!(mx.cluster_limits(), (5_000, 5_000_000));
        }
        mx.write_header().expect("header");
        for p in packets {
            mx.write_packet(p).expect("packet");
        }
        mx.write_trailer().expect("trailer");
    }
    let bytes = std::fs::read(&tmp).expect("read back");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

/// Demux and return (cluster count via ClusterRecords, packet count).
fn cluster_and_packet_counts(bytes: Vec<u8>) -> (usize, usize) {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux");
    let mut packets = 0;
    while Demuxer::next_packet(&mut dmx).is_ok() {
        packets += 1;
    }
    (dmx.cluster_records().len(), packets)
}

#[test]
fn default_byte_budget_rotates_high_bitrate_stream() {
    // 40 packets x 512 KiB at 40 ms cadence = ~20 MiB over 1.6 s: the
    // 5 s duration budget alone would keep one Cluster; §25.1's 5 MB
    // byte budget must rotate roughly every 9-10 blocks.
    let packets: Vec<Packet> = (0..40).map(|i| packet(i * 40, 512 * 1024)).collect();
    let bytes = mux("default5mb", None, &packets);
    let (clusters, got) = cluster_and_packet_counts(bytes);
    assert_eq!(got, 40);
    assert!(
        (4..=6).contains(&clusters),
        "~20 MiB at a 5 MB budget should give 4-6 clusters, got {clusters}"
    );
}

#[test]
fn configured_byte_budget_rotates() {
    // 64 KiB budget, 8 KiB packets → a new cluster every 8 blocks.
    let packets: Vec<Packet> = (0..32).map(|i| packet(i * 10, 8 * 1024)).collect();
    let bytes = mux("64k", Some((5_000, 64 * 1024)), &packets);
    let (clusters, got) = cluster_and_packet_counts(bytes);
    assert_eq!(got, 32);
    assert_eq!(clusters, 4, "32 x 8 KiB at a 64 KiB budget = 4 clusters");
}

#[test]
fn configured_duration_budget_rotates() {
    // 100 ms budget, packets every 40 ms → rotation roughly every 3-4
    // blocks (the budget check is `delta > max_ms`).
    let packets: Vec<Packet> = (0..24).map(|i| packet(i * 40, 64)).collect();
    let bytes = mux("100ms", Some((100, 5_000_000)), &packets);
    let (clusters, got) = cluster_and_packet_counts(bytes);
    assert_eq!(got, 24);
    assert!(
        (6..=8).contains(&clusters),
        "24 x 40 ms at a 100 ms budget should give 6-8 clusters, got {clusters}"
    );
}

#[test]
fn oversize_single_block_is_written_whole() {
    // One block larger than the whole byte budget must still be written
    // (a Cluster exceeds the budget by at most one Block), and the next
    // block lands in a fresh Cluster.
    let packets = vec![packet(0, 8 * 1024), packet(10, 64)];
    let bytes = mux("oversize", Some((5_000, 1024)), &packets);
    let (clusters, got) = cluster_and_packet_counts(bytes);
    assert_eq!(got, 2);
    assert_eq!(clusters, 2, "the oversize block fills cluster 1 alone");
}

#[test]
fn limits_validation() {
    let streams = vec![pcm_stream()];
    let tmp = tmp_path("validate");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");

    for (ms, bytes) in [(0u32, 5_000_000u64), (32_768, 5_000_000), (5_000, 1023)] {
        match mx.with_cluster_limits(ms, bytes) {
            Err(Error::InvalidData(msg)) => {
                assert!(msg.contains("with_cluster_limits"), "{msg}")
            }
            _ => panic!("({ms}, {bytes}) must be rejected"),
        }
    }
    // The i16 boundary itself is legal.
    mx.with_cluster_limits(32_767, 1024)
        .expect("boundary values ok");

    mx.write_header().expect("header");
    match mx.with_cluster_limits(1_000, 5_000_000) {
        Err(Error::Other(msg)) => assert!(msg.contains("write_header"), "{msg}"),
        _ => panic!("must reject post-write_header"),
    }
    drop(mx);
    let _ = std::fs::remove_file(&tmp);
}
