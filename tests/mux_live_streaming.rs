//! Livestreaming muxer layout (RFC 9559 §25.3.4 + §23.2) —
//! `MkvMuxer::with_live_streaming`.
//!
//! §25.3.4: "In livestreaming, only a few elements make sense. For
//! example, SeekHead and Cues are useless. All elements other than the
//! Clusters MUST be placed before the Clusters." §23.2 adds that a
//! stream with neither a SeekHead nor a Cues list at its start SHOULD be
//! considered non-seekable, and that a live Segment's size bits MUST all
//! be 1 (the unknown-size VINT — which this muxer already writes).
//!
//! The tests pin the layout (no SeekHead / no Cues on disk), the
//! §5.1.3.2 `Position = 0` live convention when position hints are also
//! on, and the natural pairing with the resilient demuxer: a live
//! capture cut at an arbitrary point still demuxes its packet prefix and
//! stays seekable through the Cues-less Cluster-Timestamp scan fallback.

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo,
    TimeBase, WriteSeek,
};

fn pcm_stream() -> StreamInfo {
    let mut ap = CodecParameters::audio(CodecId::new("pcm_s16le"));
    ap.sample_rate = Some(48_000);
    ap.channels = Some(2);
    ap.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: ap,
    }
}

/// Mux 13 one-second-spaced PCM packets (3 Clusters at the muxer's ~5 s
/// cadence) with the requested muxer options.
fn mux_file(tag: &str, live: bool, hints: bool) -> Vec<u8> {
    let stream = pcm_stream();
    let streams = vec![stream.clone()];
    let tmp = std::env::temp_dir().join(format!("oxideav-mkv-live-{tag}.mkv"));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mkv::mux::MkvMuxer::new_matroska(ws, &streams).unwrap();
        if live {
            mux.with_live_streaming().unwrap();
        }
        if hints {
            mux.with_cluster_position_hints().unwrap();
        }
        mux.write_header().unwrap();
        for i in 0..=12i64 {
            let mut p = Packet::new(0, stream.time_base, vec![i as u8; 64]);
            p.pts = Some(i * 1000);
            p.duration = Some(1000);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn contains_id(bytes: &[u8], id: [u8; 4]) -> bool {
    bytes.windows(4).any(|w| w == id)
}

const SEEK_HEAD_ID: [u8; 4] = [0x11, 0x4D, 0x9B, 0x74];
const CUES_ID: [u8; 4] = [0x1C, 0x53, 0xBB, 0x6B];

fn drain(dmx: &mut oxideav_mkv::demux::MkvDemuxer) -> Vec<i64> {
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => out.push(p.pts.unwrap_or(-1)),
            Err(Error::Eof) => return out,
            Err(e) => panic!("demux error: {e}"),
        }
    }
}

#[test]
fn live_layout_omits_seek_head_and_cues() {
    let live = mux_file("layout", true, false);
    assert!(
        !contains_id(&live, SEEK_HEAD_ID),
        "§25.3.4: no SeekHead in the live layout"
    );
    assert!(
        !contains_id(&live, CUES_ID),
        "§25.3.4: no Cues in the live layout"
    );

    // The default layout still carries both.
    let vod = mux_file("layout-vod", false, false);
    assert!(contains_id(&vod, SEEK_HEAD_ID));
    assert!(contains_id(&vod, CUES_ID));

    // A live stream still demuxes every packet through the strict path...
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(live.clone()));
    let mut dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert!(dmx.seek_entries().is_empty(), "no SeekHead parsed");
    assert_eq!(drain(&mut dmx).len(), 13);
    // ...but is non-seekable there (§23.2's SHOULD signal in action).
    assert!(dmx.seek_to(0, 0).is_err());

    // The resilient path recovers seekability via the Cluster-Timestamp
    // scan fallback.
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(live));
    let mut dmx =
        oxideav_mkv::demux::open_resilient_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    let landed = dmx.seek_to(0, 7000).expect("resilient live seek");
    assert_eq!(landed, 6000, "snaps to the 6 s cluster");
    assert_eq!(
        drain(&mut dmx),
        vec![6000, 7000, 8000, 9000, 10000, 11000, 12000]
    );
}

#[test]
fn live_position_hint_is_zero() {
    // §5.1.3.2: Position is "0 in live streams".
    let bytes = mux_file("hints", true, true);
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let mut dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(drain(&mut dmx).len(), 13);
    let records = dmx.cluster_records();
    assert_eq!(records.len(), 3);
    for (i, r) in records.iter().enumerate() {
        assert_eq!(
            r.position,
            Some(0),
            "cluster {i}: live Position must be the Some(0) convention"
        );
        if i > 0 {
            // PrevSize stays real — the previous Cluster's size is known
            // even on a live path.
            const CLUSTER_HEADER_LEN: u64 = 5;
            let expect = (records[i].body_offset - CLUSTER_HEADER_LEN)
                - (records[i - 1].body_offset - CLUSTER_HEADER_LEN);
            assert_eq!(r.prev_size, Some(expect));
        }
    }
}

#[test]
fn live_capture_cut_at_any_point_yields_packet_prefix() {
    // A live consumer can lose the connection anywhere. Every cut of the
    // live stream must resiliently demux to a packet prefix — same
    // property as the truncation sweep in damage_resilience.rs, but over
    // the §25.3.4 layout (unknown-size Segment, no trailer elements).
    let bytes = mux_file("cut", true, false);
    let full: Vec<i64> = {
        let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes.clone()));
        let mut dmx =
            oxideav_mkv::demux::open_resilient_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
        drain(&mut dmx)
    };
    assert_eq!(full.len(), 13);
    for cut in (0..bytes.len()).step_by(3) {
        let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes[..cut].to_vec()));
        match oxideav_mkv::demux::open_resilient_typed(rs, &oxideav_core::NullCodecResolver) {
            Err(_) => {} // header / Tracks cut — nothing to demux
            Ok(mut dmx) => {
                let got = drain(&mut dmx);
                assert!(
                    got.len() <= full.len() && got == full[..got.len()],
                    "cut {cut}: not a packet prefix ({got:?})"
                );
            }
        }
    }
}

#[test]
fn with_live_streaming_rejected_after_write_header() {
    let stream = pcm_stream();
    let ws: Box<dyn WriteSeek> = Box::new(std::io::Cursor::new(Vec::new()));
    let mut mux = oxideav_mkv::mux::MkvMuxer::new_matroska(ws, &[stream]).unwrap();
    assert!(!mux.live_streaming());
    mux.write_header().unwrap();
    assert!(mux.with_live_streaming().is_err());
}
