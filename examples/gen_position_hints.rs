//! Dev helper: write a Position/PrevSize-hinted MKV for black-box
//! validation (`cargo run --example gen_position_hints <out.mkv>`).

use oxideav_core::{
    CodecId, CodecParameters, Muxer, Packet, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: gen_position_hints <out.mkv>");
    let mut ap = CodecParameters::audio(CodecId::new("pcm_s16le"));
    ap.sample_rate = Some(48_000);
    ap.channels = Some(2);
    ap.sample_format = Some(SampleFormat::S16);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: ap,
    };
    let f = std::fs::File::create(&path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux =
        oxideav_mkv::mux::MkvMuxer::new_matroska(ws, std::slice::from_ref(&stream)).unwrap();
    mux.with_cluster_position_hints().unwrap();
    mux.write_header().unwrap();
    for i in 0..=12i64 {
        let mut p = Packet::new(0, stream.time_base, vec![i as u8; 192]);
        p.pts = Some(i * 1000);
        p.duration = Some(1000);
        p.flags.keyframe = true;
        mux.write_packet(&p).unwrap();
    }
    mux.write_trailer().unwrap();
    println!("wrote {path}");
}
