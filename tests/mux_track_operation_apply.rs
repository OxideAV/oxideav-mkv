//! Mux→demux round-trip for TrackOperation *application* (RFC 9559
//! §5.1.4.1.30 + §18.8): the muxer writes `TrackOperation` structures the
//! demuxer can itself re-apply.
//!
//! Each test muxes a file whose virtual track carries a
//! `TrackCombinePlanes` / `TrackJoinBlocks` recipe over real source
//! tracks, re-opens the bytes with
//! `MkvDemuxer::set_apply_track_operations(true)`, and checks the virtual
//! stream is synthesised exactly as the sources were written — the
//! round-trip validation the write path was still missing. The muxed
//! bytes are also run through `schema::validate` to pin that a
//! TrackOperation-carrying file stays schema-clean.
//!
//! These tests use the production demuxer and validator to walk the muxed
//! buffer — no third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::demux::{MkvDemuxer, TrackPlaneType, VirtualPacketOrigin, VirtualPacketRole};
use oxideav_mkv::mux::{MkvMuxer, MkvTrackOperation};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r438-trackop-apply-{}-{}-{n}.mkv",
        tag,
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

fn packet(stream: u32, pts: i64, payload: u8, keyframe: bool) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![payload; 4]);
    p.pts = Some(pts);
    p.flags.keyframe = keyframe;
    p
}

/// Mux a three-video-track MKV (streams 0/1 real, stream 2 virtual) with
/// the given `TrackOperation` on stream 2 and the given packet sequence.
fn mux_with_operation(op: MkvTrackOperation, packets: &[Packet]) -> Vec<u8> {
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let streams = vec![video_stream(0), video_stream(1), video_stream(2)];
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer construct");
        mx.set_track_operation(2, op).expect("set_track_operation");
        mx.write_header().expect("write_header");
        for p in packets {
            mx.write_packet(p).expect("write_packet");
        }
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn demux_applying(bytes: Vec<u8>) -> MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver)
        .expect("demux open_typed");
    dmx.set_apply_track_operations(true);
    dmx
}

fn drain(dmx: &mut MkvDemuxer) -> Vec<(Packet, Option<VirtualPacketOrigin>)> {
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                let origin = dmx.virtual_packet_origin();
                out.push((p, origin));
            }
            Err(oxideav_core::Error::Eof) => return out,
            Err(e) => panic!("unexpected demux error: {e:?}"),
        }
    }
}

/// The canonical stereo-3D recipe (left = stream 0, right = stream 1)
/// written by our own muxer re-applies: the virtual stream carries every
/// plane frame in write order with the correct plane role per packet.
#[test]
fn muxed_stereo_3d_reapplies() {
    let packets = vec![
        packet(0, 0, 0x4C, true),
        packet(1, 0, 0x52, true),
        packet(0, 40, 0x4D, false),
        packet(1, 40, 0x53, false),
    ];
    let bytes = mux_with_operation(MkvTrackOperation::stereo_3d(0, 1), &packets);

    // The muxed file stays schema-clean with the TrackOperation present.
    let report = oxideav_mkv::schema::validate(&mut Cursor::new(&bytes)).expect("schema walk");
    assert!(report.is_valid(), "violations: {:?}", report.findings);
    assert_eq!(report.informational, 0, "{:?}", report.findings);

    let mut dmx = demux_applying(bytes);
    let pkts = drain(&mut dmx);
    let virt: Vec<_> = pkts.iter().filter(|(p, _)| p.stream_index == 2).collect();
    assert_eq!(virt.len(), 4, "one copy per plane frame");
    let expect = [
        (0i64, 0x4Cu8, 0u32, TrackPlaneType::LeftEye),
        (0, 0x52, 1, TrackPlaneType::RightEye),
        (40, 0x4D, 0, TrackPlaneType::LeftEye),
        (40, 0x53, 1, TrackPlaneType::RightEye),
    ];
    for (i, ((p, o), (pts, payload, src, plane))) in virt.iter().zip(expect.iter()).enumerate() {
        assert_eq!(p.pts, Some(*pts), "virtual packet {i} pts");
        assert_eq!(p.data, vec![*payload; 4], "virtual packet {i} bytes");
        assert_eq!(
            *o,
            Some(VirtualPacketOrigin {
                virtual_stream: 2,
                source_stream: *src,
                role: VirtualPacketRole::Plane(*plane),
            }),
            "virtual packet {i} origin"
        );
    }
    // Source tracks kept their own packets.
    assert_eq!(
        pkts.iter().filter(|(p, _)| p.stream_index != 2).count(),
        4,
        "sources unchanged"
    );
}

/// A `TrackJoinBlocks` recipe written by our own muxer re-applies: the
/// virtual stream is the merged (write-order = timestamp-order) stream of
/// both sources.
#[test]
fn muxed_join_reapplies() {
    let packets = vec![
        packet(0, 0, 0x10, true),
        packet(1, 5, 0x20, true),
        packet(0, 10, 0x11, false),
        packet(1, 20, 0x21, false),
    ];
    let bytes = mux_with_operation(MkvTrackOperation::join(vec![0, 1]), &packets);

    let report = oxideav_mkv::schema::validate(&mut Cursor::new(&bytes)).expect("schema walk");
    assert!(report.is_valid(), "violations: {:?}", report.findings);
    assert_eq!(report.informational, 0, "{:?}", report.findings);

    let mut dmx = demux_applying(bytes);
    let pkts = drain(&mut dmx);
    let virt: Vec<_> = pkts.iter().filter(|(p, _)| p.stream_index == 2).collect();
    let got: Vec<(i64, u8, u32)> = virt
        .iter()
        .map(|(p, o)| (p.pts.unwrap(), p.data[0], o.unwrap().source_stream))
        .collect();
    assert_eq!(
        got,
        vec![(0, 0x10, 0), (5, 0x20, 1), (10, 0x11, 0), (20, 0x21, 1)],
        "timestamp-ordered merge of both sources"
    );
    assert!(virt
        .iter()
        .all(|(_, o)| o.unwrap().role == VirtualPacketRole::Joined));
}

/// Seeking the muxed virtual track goes through the §18.8 Cues union over
/// the muxer's own emitted Cues (which index only the real tracks).
#[test]
fn muxed_virtual_track_seeks_via_source_cues() {
    // Two "GOPs": keyframes at 0 and 200 on both sources so the muxer
    // emits cue points for both clusters.
    let packets = vec![
        packet(0, 0, 0x10, true),
        packet(1, 0, 0x20, true),
        packet(0, 200, 0x11, true),
        packet(1, 200, 0x21, true),
    ];
    let bytes = mux_with_operation(MkvTrackOperation::join(vec![0, 1]), &packets);
    let mut dmx = demux_applying(bytes);

    let landed = dmx.seek_to(2, 200).expect("virtual seek via source cues");
    assert!(landed <= 200, "landed at or before the target");
    let pkts = drain(&mut dmx);
    let virt: Vec<_> = pkts.iter().filter(|(p, _)| p.stream_index == 2).collect();
    assert!(
        virt.iter().any(|(p, _)| p.pts == Some(200)),
        "the target frames are reachable from the landing point"
    );
}
