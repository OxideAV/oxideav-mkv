#![no_main]

//! Mux a fuzz-chosen packet schedule in a fuzz-chosen layout, read it
//! back, and seek in it.
//!
//! The input picks the stream set (1..=3 of video / audio / subtitle),
//! the layout (index at the end, index at the front in reserved space,
//! live with no index), the lacing mode, the Cluster budgets, the output
//! buffer size, and then a run of packets (stream, timestamp step, key
//! flag, payload length). Per-stream timestamps never go backwards and
//! the streams advance on one shared clock, as a real writer's do.
//!
//! Oracles:
//!
//! * the muxer accepts every packet and the output does not depend on
//!   the output buffer size;
//! * a strict demux returns every packet's bytes on the stream it was
//!   written to, in the order written — and, without lacing, with the
//!   timestamp and key flag it was written with;
//! * the resilient demux returns the same and records no damage;
//! * without lacing, `seek_to` on any stream with a key frame returns
//!   the timestamp of that stream's last key frame at or before the
//!   target (its first for an earlier target) and the next packet of the
//!   stream is that key frame.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use libfuzzer_sys::fuzz_target;
use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, NullCodecResolver, Packet, ReadSeek,
    SampleFormat, StreamInfo, TimeBase,
};
use oxideav_mkv::demux::MkvDemuxer;
use oxideav_mkv::mux::{LacingMode, MkvMuxer, MkvTrackTiming};

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

struct Plan {
    kinds: Vec<u8>,
    layout: u8,
    lacing: LacingMode,
    cluster_ms: u32,
    cluster_bytes: u64,
    default_ms: u64,
    packets: Vec<Packet>,
}

fn stream_info(index: u32, kind: u8) -> StreamInfo {
    let params = match kind {
        0 => {
            let mut p = CodecParameters::video(CodecId::new("ffv1"));
            p.width = Some(64);
            p.height = Some(48);
            p
        }
        1 => {
            let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
            p.sample_rate = Some(8_000);
            p.channels = Some(1);
            p.sample_format = Some(SampleFormat::S16);
            p
        }
        _ => CodecParameters::subtitle(CodecId::new("subrip")),
    };
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn plan(data: &[u8]) -> Option<Plan> {
    if data.len() < 8 {
        return None;
    }
    let n_streams = (data[0] % 3) as usize + 1;
    let kinds: Vec<u8> = (0..n_streams).map(|i| (data[1] >> (2 * i)) % 3).collect();
    let layout = data[2] % 3;
    let lacing = match data[3] % 4 {
        0 => LacingMode::None,
        1 => LacingMode::Xiph,
        2 => LacingMode::Ebml,
        _ => LacingMode::FixedSize,
    };
    let cluster_ms = [20u32, 100, 1000, 5000][(data[4] % 4) as usize];
    let cluster_bytes = [1024u64, 4096, 5_000_000][(data[5] % 3) as usize];
    let default_ms = (data[5] >> 4) as u64 % 8;
    let tb = TimeBase::new(1, 1000);
    let mut clock = 0i64;
    let mut packets = Vec::new();
    for (i, rec) in data[8..].chunks_exact(4).take(400).enumerate() {
        let stream = (rec[0] as usize % n_streams) as u32;
        // Half the steps are the DefaultDuration, so laces form.
        clock += if rec[1] & 0x80 != 0 && default_ms > 0 {
            default_ms as i64
        } else {
            (rec[1] % 64) as i64
        };
        let len = 1 + ((rec[2] as usize) << 2 | (rec[3] as usize & 3)) % 700;
        let mut p = Packet::new(stream, tb, vec![(i as u8).wrapping_mul(13) ^ rec[3]; len]);
        p.pts = Some(clock);
        p.flags.keyframe = kinds[stream as usize] != 0 || rec[0] & 0x80 != 0;
        packets.push(p);
    }
    Some(Plan {
        kinds,
        layout,
        lacing,
        cluster_ms,
        cluster_bytes,
        default_ms,
        packets,
    })
}

fn mux(plan: &Plan, buffer: usize) -> Vec<u8> {
    let streams: Vec<StreamInfo> = plan
        .kinds
        .iter()
        .enumerate()
        .map(|(i, &k)| stream_info(i as u32, k))
        .collect();
    let sink = Sink::default();
    let mut m = MkvMuxer::new_matroska(Box::new(sink.clone()), &streams).expect("muxer");
    m.with_cluster_limits(plan.cluster_ms, plan.cluster_bytes)
        .expect("cluster limits");
    match plan.layout {
        0 => {}
        1 => {
            m.with_front_cues(2048).expect("front cues");
        }
        _ => {
            m.with_live_streaming().expect("live");
        }
    }
    m.with_block_lacing(plan.lacing).expect("lacing");
    // Audio tracks state a DefaultDuration (0 = none): what lets the
    // muxer lace frames that are exactly that far apart.
    if plan.default_ms > 0 {
        for (i, &k) in plan.kinds.iter().enumerate() {
            if k == 1 {
                m.set_track_timing(
                    i,
                    MkvTrackTiming::default()
                        .with_default_duration(Some(plan.default_ms * 1_000_000)),
                )
                .expect("track timing");
            }
        }
    }
    m.with_output_buffer(buffer).expect("buffer");
    m.write_header().expect("header");
    for p in &plan.packets {
        m.write_packet(p).expect("packet");
    }
    m.write_trailer().expect("trailer");
    drop(m);
    sink.0.lock().unwrap().get_ref().clone()
}

fn open(bytes: &[u8], resilient: bool) -> MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.to_vec()));
    if resilient {
        oxideav_mkv::demux::open_resilient_typed(rs, &NullCodecResolver).expect("resilient open")
    } else {
        oxideav_mkv::demux::open_typed(rs, &NullCodecResolver).expect("open")
    }
}

fn drain(dmx: &mut MkvDemuxer) -> Vec<Packet> {
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => out.push(p),
            Err(Error::Eof) => return out,
            Err(e) => panic!("demux of own output failed: {e}"),
        }
    }
}

fn check_read_back(plan: &Plan, got: &[Packet], what: &str) {
    assert_eq!(got.len(), plan.packets.len(), "{what}: packet count");
    // A lace holds only frames a reader places where they were
    // written (one DefaultDuration apart), so timestamps and key flags
    // are exact in every lacing mode; only the storage order differs
    // (a lace is written when it is complete).
    let in_order = plan.lacing == LacingMode::None;
    for stream in 0..plan.kinds.len() as u32 {
        let want = plan.packets.iter().filter(|p| p.stream_index == stream);
        let have = got.iter().filter(|p| p.stream_index == stream);
        for (i, (w, h)) in want.zip(have).enumerate() {
            assert!(w.data == h.data, "{what}: stream {stream} packet {i} bytes");
            assert_eq!(w.pts, h.pts, "{what}: stream {stream} packet {i} pts");
            assert_eq!(
                w.flags.keyframe, h.flags.keyframe,
                "{what}: stream {stream} packet {i} key flag"
            );
        }
    }
    if in_order {
        // Storage order is write order.
        for (i, (w, h)) in plan.packets.iter().zip(got).enumerate() {
            assert_eq!(w.stream_index, h.stream_index, "{what}: packet {i} stream");
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Some(plan) = plan(data) else { return };
    let bytes = mux(&plan, 64 * 1024);
    let unbuffered = mux(&plan, 0);
    assert!(bytes == unbuffered, "output depends on the output buffer");

    let mut strict = open(&bytes, false);
    let got = drain(&mut strict);
    check_read_back(&plan, &got, "strict");
    let mut resilient = open(&bytes, true);
    let again = drain(&mut resilient);
    check_read_back(&plan, &again, "resilient");
    assert!(
        resilient.damage_events().is_empty(),
        "own output read as damaged: {:?}",
        resilient.damage_events()
    );

    if plan.lacing != LacingMode::None || plan.packets.is_empty() {
        return;
    }
    // The live layout is non-seekable on the strict path (RFC 9559
    // §23.2).
    let dmx = if plan.layout == 2 {
        &mut resilient
    } else {
        &mut strict
    };
    let last = plan.packets.iter().filter_map(|p| p.pts).max().unwrap_or(0);
    let picks = [data[6] as i64, data[7] as i64];
    for stream in 0..plan.kinds.len() as u32 {
        let keys: Vec<&Packet> = plan
            .packets
            .iter()
            .filter(|p| p.stream_index == stream && p.flags.keyframe)
            .collect();
        if keys.is_empty() {
            continue;
        }
        for pick in picks {
            let target = (last + 20) * pick / 255 - 5;
            let want = keys
                .iter()
                .rev()
                .find(|p| p.pts.unwrap() <= target)
                .or(keys.first())
                .unwrap();
            let landed = dmx.seek_to(stream, target).expect("seek");
            assert_eq!(
                landed,
                want.pts.unwrap(),
                "seek(stream {stream}, {target}) landed timestamp"
            );
            let first = loop {
                match dmx.next_packet() {
                    Ok(p) if p.stream_index == stream => break p,
                    Ok(_) => continue,
                    Err(e) => panic!("no packet of stream {stream} after the seek: {e}"),
                }
            };
            assert_eq!(first.pts, want.pts, "first packet after the seek");
            assert!(
                first.flags.keyframe,
                "first packet after the seek is a key frame"
            );
        }
    }
});
