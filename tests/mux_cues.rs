//! Exercises the muxer's on-the-fly Cues generation.
//!
//! Writes a multi-cluster VP9+Opus WebM through `mux::open_webm`, re-reads
//! the file with the demuxer, and verifies that
//!
//! 1. A Cues element was emitted (so `seek_to` must not return
//!    `Error::Unsupported`).
//! 2. Seeking to a keyframe the muxer produced lands on the right cluster —
//!    the very first post-seek packet must have the expected pts.
//! 3. Seeking to a non-indexed timestamp snaps back to the previous cue
//!    without overshooting the target.

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

/// Stereo 10 ms SILK-NB packet (arbitrary payload byte for test identity).
fn opus_packet(payload_byte: u8) -> Vec<u8> {
    let mut out = vec![0x80u8];
    out.extend_from_slice(&[payload_byte; 32]);
    out
}

fn vp9_frame(marker: u8, len: usize) -> Vec<u8> {
    let mut v = vec![marker; len];
    v[0] = marker;
    v
}

/// A VP9+Opus stream description, paired with the expected WebM profile.
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

#[test]
fn muxer_writes_cues_and_demuxer_can_seek() {
    // Build 3 clusters' worth of media. With CLUSTER_DURATION_MS = 5000 the
    // muxer rolls a new cluster on the first keyframe that lands more than
    // 5 s past the open cluster's start. We place keyframes at pts = 0,
    // 6000, 12000 (ms) to force 3 clusters.
    let (video, audio) = build_streams();
    let streams = vec![video.clone(), audio.clone()];
    let tmp = std::env::temp_dir().join("oxideav-mkv-cues-roundtrip.webm");

    // Video: one frame per second. Keyframes at 0, 6, 12 seconds.
    let v_times_ms: Vec<i64> = (0..=12).map(|i| i * 1000).collect();
    let v_frames: Vec<Vec<u8>> = (0..v_times_ms.len())
        .map(|i| vp9_frame(i as u8, 48 + i))
        .collect();
    // Audio: every 100 ms. Audio timebase is 1/48000 so pts is in samples.
    let audio_samples_per_100ms: i64 = 48_000 / 10;
    let a_times: Vec<i64> = (0..130).map(|i| i * audio_samples_per_100ms).collect();
    let a_packets: Vec<Vec<u8>> = (0..a_times.len())
        .map(|i| opus_packet((i as u8).wrapping_add(50)))
        .collect();

    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mkv::mux::open_webm(ws, &streams).unwrap();
        mux.write_header().unwrap();
        for (i, &t_ms) in v_times_ms.iter().enumerate() {
            let mut p = Packet::new(0, video.time_base, v_frames[i].clone());
            p.pts = Some(t_ms);
            p.duration = Some(1000);
            // Keyframes every 6 seconds.
            p.flags.keyframe = t_ms % 6000 == 0;
            mux.write_packet(&p).unwrap();
        }
        for (i, &t_samples) in a_times.iter().enumerate() {
            let mut p = Packet::new(1, audio.time_base, a_packets[i].clone());
            p.pts = Some(t_samples);
            p.duration = Some(audio_samples_per_100ms);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    // The file must contain the Cues element ID (0x1C 0x53 0xBB 0x6B).
    {
        use std::io::Read;
        let raw = std::fs::read(&tmp).unwrap();
        let needle = [0x1Cu8, 0x53, 0xBB, 0x6B];
        assert!(
            raw.windows(4).any(|w| w == needle),
            "written file must contain a Cues element"
        );
        // Sanity: also still a valid WebM header.
        let mut f = std::fs::File::open(&tmp).unwrap();
        let mut head = vec![0u8; 96];
        let n = f.read(&mut head).unwrap();
        head.truncate(n);
        assert!(
            head.windows(4).any(|w| w == b"webm"),
            "WebM DocType must be present at file head"
        );
    }

    // Re-open with the demuxer, find the video stream.
    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux");
    let video_idx = dmx
        .streams()
        .iter()
        .find(|s| s.params.media_type == MediaType::Video)
        .expect("video present")
        .index;

    // Seek to t=6000 ms — a keyframe we wrote. Should land exactly there.
    let landed = dmx.seek_to(video_idx, 6000).expect("seek to 6000");
    assert_eq!(landed, 6000, "must land on the cue we emitted at 6 s");

    // First video packet after the seek must be at pts=6000.
    let pkt = next_stream_packet(&mut dmx, video_idx).expect("post-seek video pkt");
    assert_eq!(
        pkt.pts,
        Some(6000),
        "first post-seek video packet must be the cue target"
    );

    // Seek to 9000 ms (between cues) — should snap to 6000.
    let landed = dmx.seek_to(video_idx, 9000).expect("seek to 9000");
    assert!(
        landed <= 9000,
        "seek must not overshoot target (landed={landed})"
    );
    assert_eq!(landed, 6000, "must snap back to the nearest earlier cue");

    let _ = std::fs::remove_file(&tmp);
}

/// Pull packets until one matches `stream_idx`, discarding others.
fn next_stream_packet(dmx: &mut Box<dyn oxideav_core::Demuxer>, stream_idx: u32) -> Option<Packet> {
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                if p.stream_index == stream_idx {
                    return Some(p);
                }
            }
            Err(_) => return None,
        }
    }
}

/// Regression: an audio-only file must still get a seekable Cues element,
/// since audio packets are self-contained and every cluster-start is a
/// valid random-access point.
#[test]
fn audio_only_mux_emits_cues() {
    let (_, audio) = build_streams();
    let streams = vec![audio.clone()];
    let tmp = std::env::temp_dir().join("oxideav-mkv-cues-audio-only.webm");

    let samples_per_20ms: i64 = 48_000 / 50;
    let n_packets = 500usize; // 10 seconds, crosses the 5 s cluster boundary.
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mkv::mux::open_webm(ws, &streams).unwrap();
        mux.write_header().unwrap();
        for i in 0..n_packets {
            let mut p = Packet::new(0, audio.time_base, opus_packet((i as u8).wrapping_add(1)));
            p.pts = Some(i as i64 * samples_per_20ms);
            p.duration = Some(samples_per_20ms);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux");
    assert_eq!(dmx.streams().len(), 1);
    let audio_idx = dmx.streams()[0].index;
    // Seek into the second cluster — must succeed.
    let target_pts_samples = 7i64 * 48_000; // 7 seconds
    let landed = dmx.seek_to(audio_idx, target_pts_samples).expect("seek");
    assert!(
        landed <= target_pts_samples,
        "seek must not overshoot: landed={landed} target={target_pts_samples}"
    );
    assert!(
        landed > 0,
        "seek should find a cue past t=0 for target at 7 s (landed={landed})"
    );
    let _ = std::fs::remove_file(&tmp);
}

/// Confirm the Cues element is positioned after the last Cluster — so it
/// belongs at end-of-file, not at the start. The demuxer's own
/// `scan_cues_from` handles the end-of-file layout, but we assert it
/// explicitly here to guard against layout regressions.
#[test]
fn cues_are_emitted_at_end() {
    let (video, audio) = build_streams();
    let streams = vec![video.clone(), audio.clone()];
    let tmp = std::env::temp_dir().join("oxideav-mkv-cues-position.webm");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mkv::mux::open_webm(ws, &streams).unwrap();
        mux.write_header().unwrap();
        for i in 0..4 {
            let mut p = Packet::new(0, video.time_base, vec![0xAA; 16]);
            p.pts = Some(i as i64 * 40);
            p.duration = Some(40);
            p.flags.keyframe = i == 0;
            mux.write_packet(&p).unwrap();
            let mut a = Packet::new(1, audio.time_base, opus_packet(i as u8 + 1));
            a.pts = Some(i as i64 * 960);
            a.duration = Some(960);
            a.flags.keyframe = true;
            mux.write_packet(&a).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let raw = std::fs::read(&tmp).unwrap();
    let cues_id = [0x1Cu8, 0x53, 0xBB, 0x6B];
    let cluster_id = [0x1Fu8, 0x43, 0xB6, 0x75];
    let cues_pos = raw
        .windows(4)
        .position(|w| w == cues_id)
        .expect("Cues element present");
    let last_cluster_pos = raw
        .windows(4)
        .enumerate()
        .rev()
        .find(|(_, w)| *w == cluster_id)
        .map(|(i, _)| i)
        .expect("at least one Cluster present");
    assert!(
        cues_pos > last_cluster_pos,
        "Cues ({cues_pos}) must follow the last Cluster ({last_cluster_pos})"
    );
    // Also: the Cues element is the last Segment child — so no Cluster
    // appears after it.
    assert!(
        raw[cues_pos..].windows(4).all(|w| w != cluster_id),
        "no Cluster should appear after the Cues element"
    );
    let _ = std::fs::remove_file(&tmp);
}
