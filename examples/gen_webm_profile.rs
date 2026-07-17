//! Dev helper: write a strict-profile WebM file for black-box validation
//! (`cargo run --example gen_webm_profile <out.webm> [--lenient]`).
//! Emits a VP9-shaped video track with chapters, tags, colour metadata,
//! and cluster hints; with `--lenient` the full Matroska surface is
//! restored under the `webm` DocType (CRC-32 children, EditionUID,
//! Position hints) so the two outputs can be diffed by external tools.

use oxideav_core::{CodecId, CodecParameters, Muxer, Packet, StreamInfo, TimeBase, WriteSeek};
use oxideav_mkv::mux::{MkvMuxer, MkvSimpleTag, MkvTag, MkvTagTargets, MkvVideoColour};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: gen_webm_profile <out.webm> [--lenient]");
    let lenient = std::env::args().any(|a| a == "--lenient");
    let mut vp = CodecParameters::video(CodecId::new("vp9"));
    vp.width = Some(320);
    vp.height = Some(240);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: vp,
    };
    let f = std::fs::File::create(&path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = MkvMuxer::new_webm(ws, std::slice::from_ref(&stream)).unwrap();
    if lenient {
        mux.with_webm_lenient().unwrap();
    }
    mux.with_cluster_position_hints().unwrap();
    mux.set_video_colour(0, MkvVideoColour::bt709()).unwrap();
    mux.add_chapter(0, Some(4_000_000_000), "Intro").unwrap();
    mux.add_tag(MkvTag::global("TITLE", "webm profile demo"))
        .unwrap();
    mux.add_tag(MkvTag {
        targets: MkvTagTargets::track(1),
        simple_tags: vec![MkvSimpleTag::new("ENCODER_SETTINGS", "none")],
    })
    .unwrap();
    mux.set_duration(std::time::Duration::from_secs(12))
        .unwrap();
    mux.write_header().unwrap();
    for i in 0..300i64 {
        let mut pkt = Packet::new(0, TimeBase::new(1, 1000), vec![0xC5; 64]);
        pkt.pts = Some(i * 40);
        pkt.flags.keyframe = i % 25 == 0;
        mux.write_packet(&pkt).unwrap();
    }
    mux.write_trailer().unwrap();
    println!(
        "wrote {path} ({})",
        if lenient { "lenient" } else { "strict" }
    );
}
