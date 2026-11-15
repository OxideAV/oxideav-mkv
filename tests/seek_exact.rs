//! Seek exactness, exhaustively, on small self-muxed files.
//!
//! The `Demuxer::seek_to` contract is "the nearest key frame at or before
//! the target". For every layout below, every track and every target
//! tick from before the first packet to past the last one, the test
//! checks that
//!
//! * the returned timestamp is the last key frame of that track at or
//!   before the target (the track's first key frame for an earlier
//!   target),
//! * the first packet of that track read after the seek is that key
//!   frame, and
//! * from the landing on, the packets are exactly the file's packets in
//!   storage order — nothing skipped, repeated or reordered.
//!
//! Layouts: the index at the end (the muxer's default), the index at the
//! front in reserved space, no index at all (the live layout, searched
//! by Cluster Timestamp), each with Clusters cut every few Blocks (so
//! most Clusters start in the middle of a group of pictures), every
//! 100 ms, and at the default five-second budget (one Cluster holding
//! everything); a video track with one key frame in eight and reordered
//! timestamps, an audio track where every frame is a key frame, and a
//! sparse subtitle track.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, NullCodecResolver, Packet, ReadSeek,
    SampleFormat, StreamInfo, TimeBase,
};
use oxideav_mkv::demux::{MkvDemuxer, SeekPrecision, SeekStrategy};
use oxideav_mkv::mux::MkvMuxer;

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Cursor<Vec<u8>>>>);

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for Sink {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Index {
    End,
    Front,
    None,
}

/// (stream, pts, key frame, payload tag)
type Row = (u32, i64, bool, u8);

fn streams() -> Vec<StreamInfo> {
    let tb = TimeBase::new(1, 1000);
    let mut vp = CodecParameters::video(CodecId::new("ffv1"));
    vp.width = Some(32);
    vp.height = Some(16);
    let mut ap = CodecParameters::audio(CodecId::new("pcm_s16le"));
    ap.sample_rate = Some(8_000);
    ap.channels = Some(1);
    ap.sample_format = Some(SampleFormat::S16);
    let sp = CodecParameters::subtitle(CodecId::new("subrip"));
    [vp, ap, sp]
        .into_iter()
        .enumerate()
        .map(|(i, params)| StreamInfo {
            index: i as u32,
            time_base: tb,
            duration: None,
            start_time: Some(0),
            params,
        })
        .collect()
}

/// The packets written, in write order. Video: 40 ms frames, a key
/// frame every eighth, each group stored as I P B B (timestamps 0, 3, 1,
/// 2 frame slots) so storage order is not timestamp order. Audio: a key
/// frame every 25 ms. Subtitles: one every 330 ms.
fn schedule() -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut tag = 0u8;
    let mut next = |rows: &mut Vec<Row>, stream: u32, pts: i64, key: bool| {
        rows.push((stream, pts, key, tag));
        tag = tag.wrapping_add(1);
    };
    let (mut v, mut a, mut s) = (0i64, 0i64, 0i64);
    let slots = [0i64, 3, 1, 2];
    // Video frame `v` is written when the clock reaches its storage
    // slot; the timestamp it carries is the reordered one.
    for clock in (0..1500i64).step_by(5) {
        if clock == v * 40 {
            let group = v / 4;
            let pts = (group * 4 + slots[(v % 4) as usize]) * 40;
            next(&mut rows, 0, pts, v % 8 == 0);
            v += 1;
        }
        if clock == a * 25 {
            next(&mut rows, 1, a * 25, true);
            a += 1;
        }
        if clock == 10 + s * 330 {
            next(&mut rows, 2, 10 + s * 330, true);
            s += 1;
        }
    }
    rows
}

fn mux(index: Index, cluster_ms: u32, cluster_bytes: u64) -> Vec<u8> {
    let streams = streams();
    let sink = Sink::default();
    let mut m = MkvMuxer::new_matroska(Box::new(sink.clone()), &streams).unwrap();
    m.with_cluster_limits(cluster_ms, cluster_bytes).unwrap();
    match index {
        Index::End => {}
        Index::Front => {
            m.with_front_cues(16 * 1024).unwrap();
        }
        Index::None => {
            m.with_live_streaming().unwrap();
        }
    }
    m.write_header().unwrap();
    for &(stream, pts, key, tag) in &schedule() {
        // 300-byte video frames against a 1024-byte Cluster budget cut a
        // Cluster every few Blocks.
        let len = if stream == 0 { 300 } else { 20 };
        let mut p = Packet::new(stream, streams[0].time_base, vec![tag; len]);
        p.pts = Some(pts);
        p.flags.keyframe = key;
        m.write_packet(&p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    sink.0.lock().unwrap().get_ref().clone()
}

fn open(bytes: &[u8], resilient: bool) -> MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    if resilient {
        oxideav_mkv::demux::open_resilient_typed(rs, &NullCodecResolver).unwrap()
    } else {
        oxideav_mkv::demux::open_typed(rs, &NullCodecResolver).unwrap()
    }
}

fn drain(dmx: &mut MkvDemuxer) -> Vec<Row> {
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => out.push((p.stream_index, p.pts.unwrap(), p.flags.keyframe, p.data[0])),
            Err(Error::Eof) => return out,
            Err(e) => panic!("demux: {e}"),
        }
    }
}

/// The key frame `seek_to(stream, target)` owes: index into `rows`.
fn expected(rows: &[Row], stream: u32, target: i64) -> usize {
    let keys = || {
        rows.iter()
            .enumerate()
            .filter(move |(_, r)| r.0 == stream && r.2)
    };
    keys()
        .filter(|(_, r)| r.1 <= target)
        .max_by_key(|(_, r)| r.1)
        .or_else(|| keys().min_by_key(|(_, r)| r.1))
        .map(|(i, _)| i)
        .expect("the track has a key frame")
}

fn check_layout(index: Index, cluster_ms: u32, cluster_bytes: u64, resilient: bool) {
    let label = format!("{index:?} / {cluster_ms} ms / {cluster_bytes} B / resilient={resilient}");
    let bytes = mux(index, cluster_ms, cluster_bytes);
    let mut dmx = open(&bytes, resilient);
    let rows = drain(&mut dmx);
    assert_eq!(rows, schedule(), "{label}: the file holds what was written");
    assert_eq!(dmx.seek_precision(), SeekPrecision::KeyFrame);
    let last = rows.iter().map(|r| r.1).max().unwrap();
    for stream in 0..3u32 {
        for target in -20..=last + 40 {
            let want = expected(&rows, stream, target);
            let landed = dmx
                .seek_to(stream, target)
                .unwrap_or_else(|e| panic!("{label}: seek({stream}, {target}): {e}"));
            assert_eq!(
                landed, rows[want].1,
                "{label}: seek({stream}, {target}) returned timestamp"
            );
            let tail = drain(&mut dmx);
            assert!(
                tail == rows[want..],
                "{label}: seek({stream}, {target}): packets after the landing are not the \
                 file's from row {want} on (got {} rows, first {:?})",
                tail.len(),
                tail.first()
            );
            match index {
                Index::None => assert_ne!(dmx.seek_strategy(), Some(SeekStrategy::Cues)),
                _ => assert_eq!(dmx.seek_strategy(), Some(SeekStrategy::Cues), "{label}"),
            }
        }
    }
}

#[test]
fn every_target_lands_on_the_key_frame_with_cues_at_the_end() {
    for (ms, bytes) in [(5000, 1024), (100, 1 << 20), (5000, 5_000_000)] {
        check_layout(Index::End, ms, bytes, false);
    }
    check_layout(Index::End, 100, 1 << 20, true);
}

#[test]
fn every_target_lands_on_the_key_frame_with_cues_at_the_front() {
    for (ms, bytes) in [(5000, 1024), (100, 1 << 20), (5000, 5_000_000)] {
        check_layout(Index::Front, ms, bytes, false);
    }
}

#[test]
fn every_target_lands_on_the_key_frame_without_cues() {
    // The live layout is non-seekable on the strict path (RFC 9559
    // §23.2); the resilient reader searches the Cluster run.
    for (ms, bytes) in [(5000, 1024), (100, 1 << 20), (5000, 5_000_000)] {
        check_layout(Index::None, ms, bytes, true);
    }
}

#[test]
fn index_precision_stops_where_the_index_points() {
    // With Clusters cut every few Blocks and no Cues, the index-level
    // landing is a Cluster start — for the video track, usually in the
    // middle of a group of pictures.
    let bytes = mux(Index::None, 5000, 1024);
    let mut dmx = open(&bytes, true);
    let rows = drain(&mut dmx);
    dmx.set_seek_precision(SeekPrecision::Index);
    assert_eq!(dmx.seek_precision(), SeekPrecision::Index);
    let last = rows.iter().map(|r| r.1).max().unwrap();
    let mut mid_group = 0usize;
    for target in (0..=last).step_by(7) {
        dmx.seek_to(0, target).unwrap();
        let tail = drain(&mut dmx);
        let start = rows.len() - tail.len();
        assert!(
            tail == rows[start..],
            "target {target}: a suffix of the file"
        );
        if tail.iter().find(|r| r.0 == 0).is_some_and(|r| !r.2) {
            mid_group += 1;
        }
    }
    assert!(
        mid_group > 0,
        "index landings start inside groups of pictures"
    );
}

#[test]
fn clusters_sharing_a_timestamp_do_not_hide_the_key_frame() {
    // Two Clusters at tick 0 — the key frame closes the first, the
    // frames that depend on it open the second. A Cluster search lands
    // on the last Cluster at or before the target, i.e. the second; the
    // key frame is in the one before it, which no earlier Timestamp
    // leads to. Found by the `mux_roundtrip` fuzz target
    // (`fuzz/corpus/mux_roundtrip/regression_seek_equal_cluster_timestamps.bin`).
    let streams: Vec<StreamInfo> = streams().into_iter().take(1).collect();
    let sink = Sink::default();
    let mut m = MkvMuxer::new_matroska(Box::new(sink.clone()), &streams).unwrap();
    m.with_cluster_limits(1000, 1024).unwrap();
    m.with_live_streaming().unwrap();
    m.write_header().unwrap();
    // (pts, key): 700-byte frames against a 1024-byte budget put two
    // Blocks in each Cluster.
    let frames = [
        (0, false),
        (0, true),
        (0, false),
        (0, false),
        (40, false),
        (80, true),
    ];
    for (i, &(pts, key)) in frames.iter().enumerate() {
        let mut p = Packet::new(0, streams[0].time_base, vec![i as u8; 700]);
        p.pts = Some(pts);
        p.flags.keyframe = key;
        m.write_packet(&p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    let bytes = sink.0.lock().unwrap().get_ref().clone();
    let mut dmx = open(&bytes, true);
    assert_eq!(drain(&mut dmx).len(), frames.len());
    for target in [0i64, 11, 40, 79] {
        assert_eq!(dmx.seek_to(0, target).unwrap(), 0, "target {target}");
        let tail = drain(&mut dmx);
        assert_eq!(
            tail[0],
            (0, 0, true, 1),
            "target {target}: lands on the key frame"
        );
        assert_eq!(tail.len(), 5);
    }
    assert_eq!(dmx.seek_to(0, 80).unwrap(), 80);
    assert_eq!(drain(&mut dmx), vec![(0, 80, true, 5)]);
}

#[test]
fn clusters_sharing_a_later_timestamp_do_not_hide_the_key_frame() {
    // The same shape away from tick 0: the Clusters at tick 58 are the
    // first of the file, so "one tick earlier" is answered with the
    // first Cluster and its Timestamp — which is not lower. Progress has
    // to be judged on where the search starts, not on the Timestamp it
    // reports. Second `mux_roundtrip` finding
    // (`regression_seek_equal_cluster_timestamps_nonzero.bin`).
    let streams: Vec<StreamInfo> = streams().into_iter().take(2).collect();
    let sink = Sink::default();
    let mut m = MkvMuxer::new_matroska(Box::new(sink.clone()), &streams).unwrap();
    m.with_cluster_limits(20, 1024).unwrap();
    m.with_live_streaming().unwrap();
    m.write_header().unwrap();
    // (stream, pts, bytes): three audio frames fill the first Cluster,
    // the video frames that follow open a second one at the same tick.
    let frames = [
        (1u32, 58i64, 400usize),
        (1, 58, 300),
        (1, 58, 330),
        (0, 58, 5),
        (0, 75, 600),
        (0, 75, 300),
    ];
    for (i, &(stream, pts, len)) in frames.iter().enumerate() {
        let mut p = Packet::new(stream, streams[0].time_base, vec![i as u8; len]);
        p.pts = Some(pts);
        p.flags.keyframe = true;
        m.write_packet(&p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    let bytes = sink.0.lock().unwrap().get_ref().clone();
    let mut dmx = open(&bytes, true);
    assert_eq!(drain(&mut dmx).len(), frames.len());
    for target in [58i64, 60, 90, 5000] {
        assert_eq!(dmx.seek_to(1, target).unwrap(), 58, "target {target}");
        let tail = drain(&mut dmx);
        assert_eq!(
            tail[0],
            (1, 58, true, 2),
            "target {target}: the last audio frame"
        );
        assert_eq!(tail.len(), 4);
    }
    // Before the first frame: the track's first key frame.
    assert_eq!(dmx.seek_to(1, 3).unwrap(), 58);
    assert_eq!(drain(&mut dmx).len(), frames.len());
}
