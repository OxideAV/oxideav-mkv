//! RFC 9559 §11.2: "When a CodecDelay element is set, its value MUST be
//! subtracted from each Block timestamp of that track." Packet
//! timestamps are the played time on both sides: the muxer stamps a
//! Block `CodecDelay` after the packet's `pts`, the demuxer hands the
//! `pts` back, and `seek_to` speaks the same terms. Both sides can be
//! switched to the Block timestamps as stored.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, NullCodecResolver, Packet, ReadSeek, SampleFormat,
    StreamInfo, TimeBase,
};
use oxideav_mkv::demux::{MkvDemuxer, open_typed};
use oxideav_mkv::mux::{MkvMuxer, MkvTrackCodecTiming};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Cursor<Vec<u8>>>>);

impl std::io::Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for Sink {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

const TB: TimeBase = TimeBase::new(1, 1000);

/// Two audio tracks, the first with a 6.5 ms `CodecDelay`, the second
/// without; packets every 20 ms, the first track starting one frame
/// before zero.
fn build(as_written: bool) -> Vec<u8> {
    let stream = |index: u32| {
        let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
        p.sample_rate = Some(48_000);
        p.channels = Some(1);
        p.sample_format = Some(SampleFormat::S16);
        StreamInfo {
            index,
            time_base: TB,
            duration: None,
            start_time: Some(0),
            params: p,
        }
    };
    let sink = Sink::default();
    let mut mux = MkvMuxer::new_matroska(Box::new(sink.clone()), &[stream(0), stream(1)]).unwrap();
    mux.set_track_codec_timing(0, MkvTrackCodecTiming::new(Some(6_500_000), None))
        .unwrap();
    mux.set_timestamps_as_written(as_written);
    mux.write_header().unwrap();
    for i in 0..50i64 {
        for s in 0..2u32 {
            let mut p = Packet::new(s, TB, vec![(i as u8) ^ (s as u8 * 0x80); 8]);
            // Track 0 leads by its delay (rounded to the 1 ms tick).
            p.pts = Some(i * 20 - if s == 0 && !as_written { 7 } else { 0 });
            p.duration = Some(20);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
        }
    }
    mux.write_trailer().unwrap();
    drop(mux);
    sink.0.lock().unwrap().get_ref().clone()
}

fn open(bytes: Vec<u8>) -> MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    open_typed(rs, &NullCodecResolver).unwrap()
}

fn pts_of(dmx: &mut MkvDemuxer, stream: u32) -> Vec<i64> {
    let mut out = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        if p.stream_index == stream {
            assert_eq!(p.pts, p.dts);
            out.push(p.pts.unwrap());
        }
    }
    out
}

#[test]
fn packet_timestamps_are_block_timestamps_minus_codec_delay() {
    let bytes = build(false);

    // Default reading: the played time, negative for the frame the
    // delay pushes before zero ("decoded but not played").
    let mut dmx = open(bytes.clone());
    assert!(!dmx.timestamps_as_written());
    let played: Vec<i64> = pts_of(&mut dmx, 0);
    assert_eq!(played.len(), 50);
    assert_eq!(&played[..3], &[-7, 13, 33]);
    assert!(played.windows(2).all(|w| w[1] - w[0] == 20));

    // As stored: 6.5 ms rounds to 7 ticks of the 1 ms time base, so
    // the Blocks sit at 0, 20, 40, ... — never negative (§11.2).
    let mut dmx = open(bytes.clone());
    dmx.set_timestamps_as_written(true);
    let stored = pts_of(&mut dmx, 0);
    assert_eq!(&stored[..3], &[0, 20, 40]);

    // The track without CodecDelay reads the same either way.
    let mut dmx = open(bytes.clone());
    let plain = pts_of(&mut dmx, 1);
    let mut dmx = open(bytes);
    dmx.set_timestamps_as_written(true);
    assert_eq!(pts_of(&mut dmx, 1), plain);
    assert_eq!(&plain[..3], &[0, 20, 40]);
}

#[test]
fn seek_targets_and_landings_are_packet_timestamps() {
    let bytes = build(false);
    for target in [-7i64, 0, 12, 13, 14, 500, 973] {
        let mut dmx = open(bytes.clone());
        let landed = dmx.seek_to(0, target).unwrap();
        // The landing is the last packet of the track at or before the
        // target, in packet timestamps.
        let want = ((target + 7).div_euclid(20)) * 20 - 7;
        assert_eq!(landed, want, "target {target}");
        let first = loop {
            let p = dmx.next_packet().unwrap();
            if p.stream_index == 0 {
                break p;
            }
        };
        assert_eq!(first.pts, Some(want), "target {target}");
    }
    // With the stored reading, the same file seeks in Block timestamps.
    let mut dmx = open(bytes);
    dmx.set_timestamps_as_written(true);
    assert_eq!(dmx.seek_to(0, 20).unwrap(), 20);
    assert_eq!(dmx.seek_to(0, 19).unwrap(), 0);
}

#[test]
fn as_written_muxing_stamps_blocks_with_the_packet_timestamps() {
    // Packets 0, 20, 40 written as stored read back as stored ...
    let bytes = build(true);
    let mut dmx = open(bytes.clone());
    dmx.set_timestamps_as_written(true);
    assert_eq!(&pts_of(&mut dmx, 0)[..3], &[0, 20, 40]);
    // ... and as played time they sit one delay earlier.
    let mut dmx = open(bytes);
    assert_eq!(&pts_of(&mut dmx, 0)[..3], &[-7, 13, 33]);
}

#[test]
fn a_remux_keeps_every_timestamp() {
    let bytes = build(false);
    let mut dmx = open(bytes.clone());
    let streams: Vec<StreamInfo> = dmx.streams().to_vec();
    let sink = Sink::default();
    let mut mux = MkvMuxer::new_matroska(Box::new(sink.clone()), &streams).unwrap();
    let timing = *dmx.track_codec_timing(0).unwrap();
    mux.set_track_codec_timing(
        0,
        MkvTrackCodecTiming::new(
            timing.codec_delay_explicit(),
            timing.seek_pre_roll_explicit(),
        ),
    )
    .unwrap();
    mux.write_header().unwrap();
    let mut first = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        first.push((p.stream_index, p.pts, p.data.clone()));
        mux.write_packet(&p).unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    let again = sink.0.lock().unwrap().get_ref().clone();
    let mut dmx = open(again);
    let mut second = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        second.push((p.stream_index, p.pts, p.data.clone()));
    }
    assert_eq!(first, second);
}
