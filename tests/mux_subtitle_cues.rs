//! Subtitle cue emission (RFC 9559 §22.1).
//!
//! §22.1 RECOMMENDS: "For each subtitle track present, each subtitle frame
//! SHOULD be referenced by a CuePoint element with a CueDuration element."
//!
//! The muxer indexes only the first audio/video frame per cluster, but for
//! SUBTITLE tracks it now indexes *every* frame, each carrying its
//! `CueDuration` (§5.1.5.1.2.4). These tests build a single-cluster MKV
//! whose subtitle track has several frames and assert one cue per frame,
//! each with the right duration and a distinct `CueBlockNumber`
//! (§5.1.5.1.2.5).

use oxideav_core::{CodecId, CodecParameters, Packet, StreamInfo, TimeBase};
use oxideav_core::{ReadSeek, WriteSeek};

fn build_streams() -> (StreamInfo, StreamInfo) {
    // Track 1: VP9 video (forces a normal cluster layout / keyframe cue).
    let mut vp = CodecParameters::video(CodecId::new("vp9"));
    vp.width = Some(320);
    vp.height = Some(240);
    let video = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: vp,
    };

    // Track 2: SubRip (S_TEXT/UTF8) subtitle, ms time base.
    let sub_params = CodecParameters::subtitle(CodecId::new("subrip"));
    let subtitle = StreamInfo {
        index: 1,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: sub_params,
    };

    (video, subtitle)
}

/// Build a one-cluster MKV: a video keyframe at t=0 then four subtitle
/// frames at t = 100/600/1100/1600 ms, each with a 400 ms duration. All
/// land inside the first cluster (`CLUSTER_DURATION_MS = 5000`).
fn write_fixture(path: &std::path::Path) -> Vec<(u64, u64)> {
    let (video, subtitle) = build_streams();
    let streams = vec![video.clone(), subtitle.clone()];

    // (pts_ms, duration_ms) for each subtitle cue we expect to round-trip.
    let sub_events: Vec<(u64, u64)> = vec![(100, 400), (600, 400), (1100, 400), (1600, 400)];

    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = oxideav_mkv::mux::open(ws, &streams).unwrap();
    mux.write_header().unwrap();

    // Video keyframe at t=0 opens the cluster (Block 1).
    let mut vk = Packet::new(0, video.time_base, vec![0x10u8; 64]);
    vk.pts = Some(0);
    vk.duration = Some(2000);
    vk.flags.keyframe = true;
    mux.write_packet(&vk).unwrap();

    // Subtitle frames — each its own cue.
    for (i, &(pts, dur)) in sub_events.iter().enumerate() {
        let text = format!("subtitle cue {i}");
        let mut sp = Packet::new(1, subtitle.time_base, text.into_bytes());
        sp.pts = Some(pts as i64);
        sp.duration = Some(dur as i64);
        sp.flags.keyframe = true;
        mux.write_packet(&sp).unwrap();
    }
    mux.write_trailer().unwrap();

    sub_events
}

#[test]
fn every_subtitle_frame_gets_a_cue_with_duration() {
    let tmp = std::env::temp_dir().join("oxideav-mkv-subtitle-cues.mkv");
    let expected = write_fixture(&tmp);

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open_typed");

    // Subtitle is track number 2 (stream index 1 -> track 2). Collect the
    // (time, duration, block_number) of every subtitle-track cue.
    let sub_track = 2u64;
    let mut got: Vec<(u64, Option<u64>, Option<u64>)> = Vec::new();
    for cp in dmx.cue_points() {
        for ctp in &cp.track_positions {
            if ctp.track == sub_track {
                got.push((cp.time, ctp.duration, ctp.block_number));
            }
        }
    }

    assert_eq!(
        got.len(),
        expected.len(),
        "§22.1: each subtitle frame must get its own CuePoint (got {got:?})"
    );

    // Cues are stored sorted by CueTime, matching our event order.
    for ((exp_pts, exp_dur), (time, dur, bn)) in expected.iter().zip(got.iter()) {
        assert_eq!(*time, *exp_pts, "subtitle cue time mismatch");
        assert_eq!(
            *dur,
            Some(*exp_dur),
            "§5.1.5.1.2.4: subtitle cue at {time} ms must carry CueDuration={exp_dur}"
        );
        let n = bn.expect("subtitle cue must carry a CueBlockNumber");
        assert!(n >= 1, "CueBlockNumber is 1-based, got {n}");
    }

    // The four subtitle blocks share the cluster with the leading video
    // keyframe, so their block numbers must be 2, 3, 4, 5 (the video
    // keyframe is Block 1).
    let block_numbers: Vec<u64> = got.iter().map(|(_, _, bn)| bn.unwrap()).collect();
    assert_eq!(
        block_numbers,
        vec![2, 3, 4, 5],
        "subtitle blocks follow the cluster-opening video keyframe"
    );

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn audio_still_indexed_once_per_cluster_not_per_frame() {
    // Regression guard: the per-frame indexing is subtitle-only. An audio
    // track must still get exactly one cue per cluster (the cluster-start),
    // not one per frame — otherwise the Cues element bloats and violates
    // §22.1's "at most once every 500 ms" audio guidance.
    let mut ap = CodecParameters::audio(CodecId::new("opus"));
    ap.sample_rate = Some(48_000);
    ap.channels = Some(2);
    let audio = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: ap,
    };
    let tmp = std::env::temp_dir().join("oxideav-mkv-audio-once-per-cluster.mkv");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mkv::mux::open(ws, std::slice::from_ref(&audio)).unwrap();
        mux.write_header().unwrap();
        // 10 audio frames, all in one cluster (well within 5 s).
        for i in 0..10i64 {
            let mut p = Packet::new(0, audio.time_base, vec![(i as u8).wrapping_add(1); 24]);
            p.pts = Some(i * 20);
            p.duration = Some(20);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open_typed");
    let audio_cues = dmx
        .cue_points()
        .iter()
        .flat_map(|cp| cp.track_positions.iter().filter(|c| c.track == 1))
        .count();
    assert_eq!(
        audio_cues, 1,
        "audio track must get exactly one cue per cluster, got {audio_cues}"
    );

    let _ = std::fs::remove_file(&tmp);
}
