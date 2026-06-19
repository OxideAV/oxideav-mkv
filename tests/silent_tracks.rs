//! Round-trip tests for `SilentTracks > SilentTrackNumber` (RFC 9559
//! Appendix A.1 / A.2, ids `0x5854` / `0x58D7`) — the Cluster-level list of
//! track numbers not used in that part of the stream.
//!
//! Drives `MkvMuxer::set_next_cluster_silent_tracks` against the public
//! Muxer trait, then re-opens the bytes and confirms the demuxer surfaces
//! the list on the matching `ClusterRecord::silent_track_numbers`.
//!
//! Spec contracts pinned here:
//!
//! 1. The list lands on the Cluster opened by the next packet, in on-disk
//!    order (A.2), and round-trips verbatim.
//! 2. The list applies to exactly one Cluster — it is drained, so a later
//!    Cluster with no fresh call carries no `SilentTracks` (A.2: a track
//!    silent here MAY be active again later).
//! 3. A Cluster with no queued list carries no `SilentTracks` master.
//! 4. `MkvMuxer::track_number` maps a stream index to the on-wire
//!    TrackNumber the muxer assigned, so a caller can build the list.
//!
//! These tests use the production demuxer / plain byte scans — no
//! third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mkv::mux::MkvMuxer;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path() -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r341-silent-{}-{n}.mkv",
        std::process::id()
    ))
}

fn video_stream(index: u32) -> StreamInfo {
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

fn audio_stream(index: u32) -> StreamInfo {
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

fn packet(stream: u32, pts: i64, marker: u8) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![marker; 12]);
    p.pts = Some(pts);
    p.flags.keyframe = true;
    p
}

fn demux_typed(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open_typed")
}

fn contains_id2(bytes: &[u8], id: u16) -> bool {
    let hi = (id >> 8) as u8;
    let lo = (id & 0xFF) as u8;
    bytes.windows(2).any(|w| w[0] == hi && w[1] == lo)
}

/// Drain every packet so the demuxer walks all Clusters and records them.
fn drain(dmx: &mut oxideav_mkv::demux::MkvDemuxer) {
    while dmx.next_packet().is_ok() {}
}

#[test]
fn silent_tracks_round_trip_on_first_cluster() {
    let streams = [video_stream(0), audio_stream(1)];
    let tmp = tmp_path();
    {
        let f = std::fs::File::create(&tmp).expect("create");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
        mx.write_header().expect("write_header");
        // Map stream 1 → its on-wire TrackNumber, then mark it silent for
        // the first Cluster.
        let audio_tn = mx.track_number(1).expect("track number for stream 1");
        mx.set_next_cluster_silent_tracks(&[audio_tn]);
        // First packet opens the first Cluster (carrying SilentTracks).
        mx.write_packet(&packet(0, 0, 0x11)).expect("p0");
        // A far-future packet forces a second Cluster with no queued list.
        mx.write_packet(&packet(0, 60_000, 0x22)).expect("p1");
        mx.write_trailer().expect("trailer");
    }
    let bytes = std::fs::read(&tmp).expect("read");
    let _ = std::fs::remove_file(&tmp);

    assert!(contains_id2(&bytes, 0x5854), "SilentTracks id on disk");
    assert!(contains_id2(&bytes, 0x58D7), "SilentTrackNumber id on disk");

    let mut dmx = demux_typed(bytes);
    let audio_tn = 2u64; // stream 1 → TrackNumber 2 (1-based assignment).
    drain(&mut dmx);
    let recs = dmx.cluster_records();
    assert!(recs.len() >= 2, "two clusters expected, got {}", recs.len());
    assert_eq!(
        recs[0].silent_track_numbers,
        vec![audio_tn],
        "first cluster carries the queued silent track"
    );
    assert!(
        recs[1].silent_track_numbers.is_empty(),
        "second cluster carries no SilentTracks (list was drained)"
    );
}

#[test]
fn multiple_silent_track_numbers_preserve_order() {
    let streams = [video_stream(0)];
    let tmp = tmp_path();
    {
        let f = std::fs::File::create(&tmp).expect("create");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
        mx.write_header().expect("write_header");
        mx.set_next_cluster_silent_tracks(&[7, 3, 9]);
        mx.write_packet(&packet(0, 0, 0x11)).expect("p0");
        mx.write_trailer().expect("trailer");
    }
    let bytes = std::fs::read(&tmp).expect("read");
    let _ = std::fs::remove_file(&tmp);

    let mut dmx = demux_typed(bytes);
    drain(&mut dmx);
    let recs = dmx.cluster_records();
    assert_eq!(recs[0].silent_track_numbers, vec![7, 3, 9]);
}

#[test]
fn no_silent_tracks_when_unset() {
    let streams = [video_stream(0)];
    let tmp = tmp_path();
    {
        let f = std::fs::File::create(&tmp).expect("create");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
        mx.write_header().expect("write_header");
        mx.write_packet(&packet(0, 0, 0x11)).expect("p0");
        mx.write_trailer().expect("trailer");
    }
    let bytes = std::fs::read(&tmp).expect("read");
    let _ = std::fs::remove_file(&tmp);

    assert!(
        !contains_id2(&bytes, 0x5854),
        "no SilentTracks master when none queued"
    );
    let mut dmx = demux_typed(bytes);
    drain(&mut dmx);
    assert!(dmx.cluster_records()[0].silent_track_numbers.is_empty());
}

#[test]
fn empty_slice_clears_queued_list() {
    let streams = [video_stream(0)];
    let tmp = tmp_path();
    {
        let f = std::fs::File::create(&tmp).expect("create");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
        mx.write_header().expect("write_header");
        mx.set_next_cluster_silent_tracks(&[1]);
        mx.set_next_cluster_silent_tracks(&[]); // clears it
        mx.write_packet(&packet(0, 0, 0x11)).expect("p0");
        mx.write_trailer().expect("trailer");
    }
    let bytes = std::fs::read(&tmp).expect("read");
    let _ = std::fs::remove_file(&tmp);
    assert!(!contains_id2(&bytes, 0x5854), "queued list was cleared");
}
