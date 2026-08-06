//! Dev helper: write a virtual-track MKV for black-box validation
//! (`cargo run --example gen_track_operation <out.mkv> [--stereo]`).
//!
//! Default: three PCM audio tracks where track 3 is a virtual
//! `TrackJoinBlocks` track joining tracks 1 + 2 (RFC 9559 §5.1.4.1.30.5) —
//! the two source tracks stay independently decodable by any reader.
//! With `--stereo`: three video tracks where track 3 is a virtual
//! stereo-3D `TrackCombinePlanes` track (left = track 1, right = track 2,
//! §5.1.4.1.30.1).

use oxideav_core::{
    CodecId, CodecParameters, Muxer, Packet, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::mux::{MkvMuxer, MkvTrackOperation};

fn audio_stream(index: u32) -> StreamInfo {
    let mut ap = CodecParameters::audio(CodecId::new("pcm_s16le"));
    ap.sample_rate = Some(48_000);
    ap.channels = Some(1);
    ap.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: ap,
    }
}

fn video_stream(index: u32) -> StreamInfo {
    let mut vp = CodecParameters::video(CodecId::new("vp9"));
    vp.width = Some(320);
    vp.height = Some(240);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: vp,
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: gen_track_operation <out.mkv> [--stereo]");
    let stereo = std::env::args().any(|a| a == "--stereo");
    let streams: Vec<StreamInfo> = if stereo {
        (0..3).map(video_stream).collect()
    } else {
        (0..3).map(audio_stream).collect()
    };
    let f = std::fs::File::create(&path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = MkvMuxer::new_matroska(ws, &streams).unwrap();
    let op = if stereo {
        MkvTrackOperation::stereo_3d(0, 1)
    } else {
        MkvTrackOperation::join(vec![0, 1])
    };
    mux.set_track_operation(2, op).unwrap();
    mux.write_header().unwrap();
    for i in 0..=12i64 {
        for s in 0..2u32 {
            // 1 ms of mono S16 @48kHz = 96 bytes (audio); tiny opaque
            // payload for the video variant.
            let payload = if stereo {
                vec![(i as u8) ^ (s as u8); 32]
            } else {
                vec![(0x10 + s) as u8; 96]
            };
            let mut p = Packet::new(s, TimeBase::new(1, 1000), payload);
            p.pts = Some(i * 1000);
            p.duration = Some(1000);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
        }
    }
    mux.write_trailer().unwrap();
    println!("wrote {path}");
}
