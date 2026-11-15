//! Container throughput measurements (`cargo bench --bench throughput`).
//!
//! A dependency-free timing harness (no statistics crate): each stage is
//! run several times and the median wall time is reported. Everything is
//! self-muxed, so the bench needs no fixture:
//!
//! * **A/V file** — one video track with large blocks (40 KiB, 30 per
//!   second, a key frame every second) interleaved with one audio track
//!   with small blocks (160 bytes every 20 ms). Size is set with
//!   `OXIDEAV_MKV_BENCH_SECONDS` (default 120 s, about 150 MiB).
//! * **index file** — an audio-only file cut into one Cluster per 20 ms
//!   so the `Cues` table is large (one `CuePoint` per Cluster); muxed
//!   once with the index at the end, once with it at the front, and once
//!   in the live layout (no index at all).
//!
//! * **bounded-Cluster file** — the muxer writes Clusters of unknown
//!   size, most other writers give each Cluster its size, and some add a
//!   `CRC-32` child. This shape is built by hand: 1 s Clusters of the
//!   same video and audio Blocks, sized, with and without the checksum.
//!
//! `OXIDEAV_MKV_BENCH_ONLY=bounded` runs that last shape alone.
//!
//! Reported: mux MiB/s and packets/s (to a file on disk and to memory),
//! demux MiB/s and packets/s (from a file on disk and from memory), and
//! open time for the three index layouts. The muxed bytes are fingerprinted (FNV-1a) so a change that
//! claims identical output can be checked against an earlier run.

use std::io::{Cursor, Write};
use std::time::{Duration, Instant};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, NullCodecResolver, Packet, ReadSeek,
    SampleFormat, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::mux::MkvMuxer;

const VIDEO_BLOCK: usize = 40 * 1024;
const AUDIO_BLOCK: usize = 160;

/// Repetitions per stage (`OXIDEAV_MKV_BENCH_RUNS`, default 5); the
/// median is reported.
fn runs() -> usize {
    std::env::var("OXIDEAV_MKV_BENCH_RUNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(5)
}

fn av_streams() -> Vec<StreamInfo> {
    let mut vp = CodecParameters::video(CodecId::new("ffv1"));
    vp.width = Some(1280);
    vp.height = Some(720);
    let mut ap = CodecParameters::audio(CodecId::new("pcm_s16le"));
    ap.sample_rate = Some(8_000);
    ap.channels = Some(1);
    ap.sample_format = Some(SampleFormat::S16);
    vec![
        StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 1000),
            duration: None,
            start_time: Some(0),
            params: vp,
        },
        StreamInfo {
            index: 1,
            time_base: TimeBase::new(1, 1000),
            duration: None,
            start_time: Some(0),
            params: ap,
        },
    ]
}

/// The interleaved packet list of the A/V file: (stream, pts ms, key
/// frame, payload length). Payloads are filled from a cheap generator so
/// the bytes are not constant.
fn av_schedule(seconds: u64) -> Vec<(u32, i64, bool, usize)> {
    let mut out = Vec::new();
    let mut v = 0u64;
    let mut a = 0u64;
    let end = seconds * 1000;
    loop {
        let vt = v * 1000 / 30;
        let at = a * 20;
        if vt >= end && at >= end {
            return out;
        }
        if vt <= at {
            out.push((0, vt as i64, v.is_multiple_of(30), VIDEO_BLOCK));
            v += 1;
        } else {
            out.push((1, at as i64, true, AUDIO_BLOCK));
            a += 1;
        }
    }
}

fn fill(buf: &mut [u8], seed: u64) {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for chunk in buf.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let b = x.to_le_bytes();
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn mux_av(out: Box<dyn WriteSeek>, packets: &[Packet]) {
    let streams = av_streams();
    let mut mux = MkvMuxer::new_matroska(out, &streams).unwrap();
    mux.write_header().unwrap();
    for p in packets {
        mux.write_packet(p).unwrap();
    }
    mux.write_trailer().unwrap();
}

#[derive(Clone, Copy, PartialEq)]
enum IndexLayout {
    End,
    Front,
    Live,
}

fn mux_index_file(clusters: u64, layout: IndexLayout) -> Vec<u8> {
    let shared = SharedVec::default();
    mux_index_to(Box::new(shared.clone()), clusters, layout);
    shared.take()
}

fn mux_index_to(out: Box<dyn WriteSeek>, clusters: u64, layout: IndexLayout) {
    let streams = vec![av_streams().remove(1)];
    let mut streams = streams;
    streams[0].index = 0;
    let mut mux = MkvMuxer::new_matroska(out, &streams).unwrap();
    mux.with_cluster_limits(19, 1024).unwrap();
    match layout {
        IndexLayout::End => {}
        IndexLayout::Front => {
            mux.with_front_cues((clusters * 24 + 4096) as u32).unwrap();
        }
        IndexLayout::Live => {
            mux.with_live_streaming().unwrap();
        }
    }
    mux.write_header().unwrap();
    let mut payload = vec![0u8; AUDIO_BLOCK];
    for i in 0..clusters {
        fill(&mut payload, i);
        let mut p = Packet::new(0, streams[0].time_base, payload.clone());
        p.pts = Some(i as i64 * 20);
        p.flags.keyframe = true;
        mux.write_packet(&p).unwrap();
    }
    mux.write_trailer().unwrap();
}

/// An in-memory `Write + Seek` sink whose bytes outlive the muxer.
#[derive(Clone, Default)]
struct SharedVec(std::sync::Arc<std::sync::Mutex<Cursor<Vec<u8>>>>);

impl SharedVec {
    fn take(&self) -> Vec<u8> {
        std::mem::take(self.0.lock().unwrap().get_mut())
    }
}

impl Write for SharedVec {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for SharedVec {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(pos)
    }
}

fn el(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = oxideav_mkv::ebml::write_element_id(id);
    out.extend_from_slice(&oxideav_mkv::ebml::write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

fn el_uint(id: u32, v: u64) -> Vec<u8> {
    let n = ((64 - v.leading_zeros()).div_ceil(8) as usize).max(1);
    el(id, &v.to_be_bytes()[8 - n..])
}

/// `seconds` one-second Clusters with known sizes, each holding 30 video
/// Blocks and 50 audio Blocks of the A/V shape; `with_crc` puts a
/// `CRC-32` child (RFC 8794 §11.3.1) at the head of every Cluster.
/// Returns the file and its packet count.
fn bounded_cluster_file(seconds: u64, with_crc: bool) -> (Vec<u8>, u64) {
    use oxideav_mkv::ids;
    let track = |number: u64, kind: u64, codec: &str| {
        el(
            ids::TRACK_ENTRY,
            &[
                el_uint(ids::TRACK_NUMBER, number),
                el_uint(ids::TRACK_UID, number),
                el_uint(ids::TRACK_TYPE, kind),
                el(ids::CODEC_ID, codec.as_bytes()),
            ]
            .concat(),
        )
    };
    let mut segment = el(ids::INFO, &el_uint(ids::TIMECODE_SCALE, 1_000_000));
    segment.extend(el(
        ids::TRACKS,
        &[
            track(1, ids::TRACK_TYPE_VIDEO, "V_FFV1"),
            track(2, ids::TRACK_TYPE_AUDIO, "A_PCM/INT/LIT"),
        ]
        .concat(),
    ));
    let mut packets = 0u64;
    let mut video = vec![0u8; VIDEO_BLOCK];
    let mut audio = vec![0u8; AUDIO_BLOCK];
    for s in 0..seconds {
        let mut rest = el_uint(ids::TIMECODE, s * 1000);
        let (mut v, mut a) = (0u64, 0u64);
        while v < 30 || a < 50 {
            let (track, ms, payload) = if v < 30 && v * 1000 / 30 <= a * 20 {
                fill(&mut video, s * 100 + v);
                v += 1;
                (1u8, (v - 1) * 1000 / 30, &video)
            } else {
                fill(&mut audio, s * 100 + 50 + a);
                a += 1;
                (2u8, (a - 1) * 20, &audio)
            };
            let mut block = vec![0x80 | track, (ms >> 8) as u8, ms as u8, 0x80];
            block.extend_from_slice(payload);
            rest.extend(el(ids::SIMPLE_BLOCK, &block));
            packets += 1;
        }
        let mut body = Vec::new();
        if with_crc {
            body.extend(el(
                ids::CRC32,
                &oxideav_mkv::ebml::crc32_ieee(&rest).to_le_bytes(),
            ));
        }
        body.extend(rest);
        segment.extend(el(ids::CLUSTER, &body));
    }
    let mut file = el(ids::EBML_HEADER, &el(ids::EBML_DOC_TYPE, b"matroska"));
    file.extend(oxideav_mkv::ebml::write_element_id(ids::SEGMENT));
    file.extend(oxideav_mkv::ebml::write_vint(segment.len() as u64, 8));
    file.extend(segment);
    (file, packets)
}

fn bench_bounded(seconds: u64, path: &std::path::Path) {
    for (with_crc, name) in [(false, "sized Clusters"), (true, "sized Clusters + CRC-32")] {
        let (bytes, n_packets) = bounded_cluster_file(seconds, with_crc);
        std::fs::write(path, &bytes).unwrap();
        let mut times = Vec::new();
        for _ in 0..runs() {
            let f = std::fs::File::open(path).unwrap();
            let t = Instant::now();
            let mut dmx = oxideav_mkv::demux::open(Box::new(f), &NullCodecResolver).unwrap();
            let (n, _) = drain(&mut *dmx);
            times.push(t.elapsed());
            assert_eq!(n, n_packets);
        }
        report(
            &format!("demux <- file, {name}"),
            median(times),
            bytes.len() as u64,
            n_packets,
        );
    }
    let _ = std::fs::remove_file(path);
}

fn drain(dmx: &mut dyn Demuxer) -> (u64, u64) {
    let (mut n, mut bytes) = (0u64, 0u64);
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                n += 1;
                bytes += p.data.len() as u64;
            }
            Err(Error::Eof) => return (n, bytes),
            Err(e) => panic!("demux: {e}"),
        }
    }
}

fn report(label: &str, t: Duration, bytes: u64, packets: u64) {
    let s = t.as_secs_f64();
    println!(
        "{label:<44} {:>9.1} ms  {:>9.1} MiB/s  {:>11.0} packets/s",
        s * 1e3,
        bytes as f64 / (1024.0 * 1024.0) / s,
        packets as f64 / s
    );
}

fn main() {
    let seconds: u64 = std::env::var("OXIDEAV_MKV_BENCH_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let schedule = av_schedule(seconds);
    let tb = TimeBase::new(1, 1000);
    let packets: Vec<Packet> = schedule
        .iter()
        .enumerate()
        .map(|(i, &(stream, pts, key, len))| {
            let mut data = vec![0u8; len];
            fill(&mut data, i as u64);
            let mut p = Packet::new(stream, tb, data);
            p.pts = Some(pts);
            p.flags.keyframe = key;
            p
        })
        .collect();
    let n_packets = packets.len() as u64;
    let path = std::env::temp_dir().join(format!("oxideav-mkv-bench-{}.mkv", std::process::id()));
    if std::env::var("OXIDEAV_MKV_BENCH_ONLY").as_deref() == Ok("bounded") {
        bench_bounded(seconds, &path);
        return;
    }

    // --- mux ---------------------------------------------------------
    let mut to_file = Vec::new();
    for _ in 0..runs() {
        let f = std::fs::File::create(&path).unwrap();
        let t = Instant::now();
        mux_av(Box::new(f), &packets);
        to_file.push(t.elapsed());
    }
    let file_bytes = std::fs::read(&path).unwrap();
    let total = file_bytes.len() as u64;
    println!(
        "A/V file: {seconds} s, {n_packets} packets, {:.1} MiB, fnv1a {:016x}",
        total as f64 / (1024.0 * 1024.0),
        fnv1a(&file_bytes)
    );
    report("mux -> file", median(to_file), total, n_packets);
    let mut to_mem = Vec::new();
    for _ in 0..runs() {
        let sink = SharedVec::default();
        let t = Instant::now();
        mux_av(Box::new(sink.clone()), &packets);
        to_mem.push(t.elapsed());
        assert_eq!(sink.take().len() as u64, total);
    }
    report("mux -> memory", median(to_mem), total, n_packets);
    drop(packets);

    // --- demux -------------------------------------------------------
    let mut from_file = Vec::new();
    for _ in 0..runs() {
        let f = std::fs::File::open(&path).unwrap();
        let t = Instant::now();
        let rs: Box<dyn ReadSeek> = Box::new(f);
        let mut dmx = oxideav_mkv::demux::open(rs, &NullCodecResolver).unwrap();
        let (n, _) = drain(&mut *dmx);
        from_file.push(t.elapsed());
        assert_eq!(n, n_packets);
    }
    report("demux <- file", median(from_file), total, n_packets);
    let mut from_mem = Vec::new();
    for _ in 0..runs() {
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file_bytes.clone()));
        let t = Instant::now();
        let mut dmx = oxideav_mkv::demux::open(rs, &NullCodecResolver).unwrap();
        let (n, _) = drain(&mut *dmx);
        from_mem.push(t.elapsed());
        assert_eq!(n, n_packets);
    }
    report("demux <- memory", median(from_mem), total, n_packets);
    drop(file_bytes);
    let _ = std::fs::remove_file(&path);
    bench_bounded(seconds, &path);

    // --- open time vs index layout ------------------------------------
    let clusters: u64 = std::env::var("OXIDEAV_MKV_BENCH_CLUSTERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    for (layout, name) in [
        (IndexLayout::End, "Cues at the end"),
        (IndexLayout::Front, "Cues at the front"),
        (IndexLayout::Live, "no Cues (live layout)"),
    ] {
        let mut muxes = Vec::new();
        for _ in 0..runs() {
            let f = std::fs::File::create(&path).unwrap();
            let t = Instant::now();
            mux_index_to(Box::new(f), clusters, layout);
            muxes.push(t.elapsed());
        }
        let bytes = mux_index_file(clusters, layout);
        assert!(std::fs::read(&path).unwrap() == bytes);
        report(
            "  mux -> file, one block per Cluster",
            median(muxes),
            bytes.len() as u64,
            clusters,
        );
        let mut opens = Vec::new();
        let mut cue_count = 0;
        for _ in 0..runs() {
            let f = std::fs::File::open(&path).unwrap();
            let t = Instant::now();
            let dmx = oxideav_mkv::demux::open_typed(Box::new(f), &NullCodecResolver).unwrap();
            opens.push(t.elapsed());
            cue_count = dmx.cue_points().len();
        }
        println!(
            "open, {clusters} Clusters, {name:<22} {:>9.2} ms  ({cue_count} CuePoints, {:.1} MiB, fnv1a {:016x})",
            median(opens).as_secs_f64() * 1e3,
            bytes.len() as f64 / (1024.0 * 1024.0),
            fnv1a(&bytes)
        );
        // Demux throughput on the one-block-per-Cluster shape.
        let mut drains = Vec::new();
        for _ in 0..runs() {
            let f = std::fs::File::open(&path).unwrap();
            let t = Instant::now();
            let mut dmx = oxideav_mkv::demux::open(Box::new(f), &NullCodecResolver).unwrap();
            let (n, _) = drain(&mut *dmx);
            drains.push(t.elapsed());
            assert_eq!(n, clusters);
        }
        report(
            "  demux <- file, one block per Cluster",
            median(drains),
            bytes.len() as u64,
            clusters,
        );
    }
    let _ = std::fs::remove_file(&path);
}
