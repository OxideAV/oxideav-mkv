//! Round-trips the muxer's newly-completed `Cues` write surface —
//! `CueBlockNumber` (RFC 9559 §5.1.5.1.2.5) and `CueDuration`
//! (§5.1.5.1.2.4) — through the typed demuxer `cue_points()` view.
//!
//! Before this round the muxer emitted only `CueTime` / `CueTrack` /
//! `CueClusterPosition` / `CueRelativePosition`; the demuxer already read
//! the full `CueTrackPositions` sub-tree, so the two sides were
//! asymmetric. These tests pin the now-symmetric behaviour:
//!
//! 1. Every emitted `CueTrackPositions` carries a 1-based
//!    `CueBlockNumber` (`range: not 0`, §5.1.5.1.2.5) — the first block of
//!    a cluster is number `1`, and a cue on a later block in the same
//!    cluster reports the right ordinal across tracks.
//! 2. A non-laced packet carrying a duration round-trips its
//!    `CueDuration`.
//! 3. The cue still drives a working `seek_to` (no regression).

use oxideav_core::{CodecId, CodecParameters, MediaType, Packet, StreamInfo, TimeBase};
use oxideav_core::{ReadSeek, WriteSeek};

fn opus_head(channels: u8, sample_rate: u32, pre_skip: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(19);
    out.extend_from_slice(b"OpusHead");
    out.push(1);
    out.push(channels);
    out.extend_from_slice(&pre_skip.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.push(0);
    out
}

fn opus_packet(payload_byte: u8) -> Vec<u8> {
    let mut out = vec![0x80u8];
    out.extend_from_slice(&[payload_byte; 32]);
    out
}

fn vp9_frame(marker: u8, len: usize) -> Vec<u8> {
    let mut v = vec![marker; len.max(1)];
    v[0] = marker;
    v
}

fn build_streams() -> (StreamInfo, StreamInfo) {
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

    let mut op = CodecParameters::audio(CodecId::new("opus"));
    op.sample_rate = Some(48_000);
    op.channels = Some(2);
    op.extradata = opus_head(2, 48_000, 312);
    let audio = StreamInfo {
        index: 1,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params: op,
    };

    (video, audio)
}

/// Build a 3-cluster VP9+Opus WebM, returning its path. Video keyframes
/// at 0/6/12 s force three clusters (`CLUSTER_DURATION_MS = 5000`). Each
/// video packet carries an explicit `duration` so the cue's
/// `CueDuration` (§5.1.5.1.2.4) has a value to round-trip.
fn write_fixture(path: &std::path::Path) {
    let (video, audio) = build_streams();
    let streams = vec![video.clone(), audio.clone()];

    let v_times_ms: Vec<i64> = (0..=12).map(|i| i * 1000).collect();
    let audio_samples_per_100ms: i64 = 48_000 / 10;
    let a_times: Vec<i64> = (0..130).map(|i| i * audio_samples_per_100ms).collect();

    let f = std::fs::File::create(path).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mux = oxideav_mkv::mux::open_webm(ws, &streams).unwrap();
    mux.write_header().unwrap();
    for (i, &t_ms) in v_times_ms.iter().enumerate() {
        let mut p = Packet::new(0, video.time_base, vp9_frame(i as u8, 48 + i));
        p.pts = Some(t_ms);
        p.duration = Some(1000);
        p.flags.keyframe = t_ms % 6000 == 0;
        mux.write_packet(&p).unwrap();
    }
    for (i, &t_samples) in a_times.iter().enumerate() {
        let mut p = Packet::new(1, audio.time_base, opus_packet((i as u8).wrapping_add(50)));
        p.pts = Some(t_samples);
        p.duration = Some(audio_samples_per_100ms);
        p.flags.keyframe = true;
        mux.write_packet(&p).unwrap();
    }
    mux.write_trailer().unwrap();
}

#[test]
fn cue_block_number_round_trips_and_is_never_zero() {
    let tmp = std::env::temp_dir().join("oxideav-mkv-cue-block-number.webm");
    write_fixture(&tmp);

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open_typed");

    let cue_points = dmx.cue_points();
    assert!(
        !cue_points.is_empty(),
        "muxer must have emitted at least one CuePoint"
    );

    let mut saw_block_number = false;
    for cp in cue_points {
        assert!(
            !cp.track_positions.is_empty(),
            "every CuePoint must carry at least one CueTrackPositions"
        );
        for ctp in &cp.track_positions {
            // §5.1.5.1.2.5 ranges CueBlockNumber as "not 0"; the muxer
            // now writes it on every entry, so it must be present and >= 1.
            let bn = ctp
                .block_number
                .expect("muxer must write a CueBlockNumber on every CueTrackPositions");
            assert!(
                bn >= 1,
                "CueBlockNumber is 1-based (range: not 0), got {bn}"
            );
            saw_block_number = true;
        }
    }
    assert!(
        saw_block_number,
        "at least one CueBlockNumber must round-trip"
    );

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn cue_duration_round_trips_for_video_keyframe_cues() {
    let tmp = std::env::temp_dir().join("oxideav-mkv-cue-duration.webm");
    write_fixture(&tmp);

    let (video, _) = build_streams();
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open_typed");

    // The video track is track number 1 (stream index 0 -> track 1). Each
    // video packet had duration = 1000 ms; the cue for the first keyframe
    // in each cluster must carry CueDuration == 1000.
    let video_track = 1u64;
    let mut checked = 0usize;
    for cp in dmx.cue_points() {
        for ctp in &cp.track_positions {
            if ctp.track == video_track {
                assert_eq!(
                    ctp.duration,
                    Some(1000),
                    "video cue at time {} must carry CueDuration=1000",
                    cp.time
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked >= 1,
        "expected at least one video-track cue carrying CueDuration"
    );

    // Sanity: the cues still seek. Re-open via the trait surface.
    let rs2: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut tdmx = oxideav_mkv::demux::open(rs2, &oxideav_core::NullCodecResolver).expect("open");
    let vidx = tdmx
        .streams()
        .iter()
        .find(|s| s.params.media_type == MediaType::Video)
        .unwrap()
        .index;
    let landed = tdmx.seek_to(vidx, 6000).expect("seek to 6000");
    assert_eq!(
        landed, 6000,
        "cue-driven seek must still land on the keyframe"
    );
    let _ = (video, std::fs::remove_file(&tmp));
}

/// When audio and video share a Cluster, the audio cue references a Block
/// that is *not* the first one in the Cluster — so its `CueBlockNumber`
/// (RFC 9559 §5.1.5.1.2.5) must be > 1. This is the case the prior
/// minimal Cues surface could not express at all.
#[test]
fn cue_block_number_exceeds_one_when_track_shares_cluster() {
    let (video, audio) = build_streams();
    let streams = vec![video.clone(), audio.clone()];
    let tmp = std::env::temp_dir().join("oxideav-mkv-cue-block-shared.webm");

    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mkv::mux::open_webm(ws, &streams).unwrap();
        mux.write_header().unwrap();
        // Interleave within a single cluster: a video keyframe at t=0
        // (Block 1 of the cluster), then several audio frames (Blocks
        // 2, 3, ...). The first audio frame of the cluster is the one
        // the audio cue references, so its CueBlockNumber must be 2.
        let mut vk = Packet::new(0, video.time_base, vp9_frame(7, 64));
        vk.pts = Some(0);
        vk.duration = Some(40);
        vk.flags.keyframe = true;
        mux.write_packet(&vk).unwrap();

        let samples_per_20ms: i64 = 48_000 / 50;
        for i in 0..5i64 {
            let mut ap = Packet::new(1, audio.time_base, opus_packet((i as u8).wrapping_add(1)));
            ap.pts = Some(i * samples_per_20ms);
            ap.duration = Some(samples_per_20ms);
            ap.flags.keyframe = true;
            mux.write_packet(&ap).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open_typed");

    let mut video_bn = None;
    let mut audio_bn = None;
    for cp in dmx.cue_points() {
        for ctp in &cp.track_positions {
            match ctp.track {
                1 => video_bn = ctp.block_number,
                2 => audio_bn = ctp.block_number,
                _ => {}
            }
        }
    }
    assert_eq!(
        video_bn,
        Some(1),
        "the cluster-opening video keyframe must be Block 1"
    );
    assert_eq!(
        audio_bn,
        Some(2),
        "the first audio Block follows the video keyframe -> Block 2"
    );

    let _ = std::fs::remove_file(&tmp);
}
