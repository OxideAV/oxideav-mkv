//! Round-trip tests for the muxer's `Video > Projection` write path (RFC
//! 9559 §5.1.4.1.28.41, including the §5.1.4.1.28.42..§5.1.4.1.28.46
//! sub-elements).
//!
//! Drives `MkvMuxer::set_video_projection` against the public Muxer trait,
//! then re-opens the bytes through [`oxideav_mkv::demux::open_typed`] and
//! confirms `MkvDemuxer::video_projection(stream_index)` decodes the exact
//! record handed to the muxer — `ProjectionType` (including the
//! `Other(u64)` forward-compat variant, §27.15 leaves the registry open),
//! the verbatim `ProjectionPrivate` payload, and the yaw / pitch / roll
//! pose triple.
//!
//! Spec contracts pinned here:
//!
//! 1. An equirectangular projection with a `ProjectionPrivate` payload and
//!    a pose round-trips type + private + pose bit-exactly.
//! 2. `ProjectionType::Other(99)` round-trips its wrapped value.
//! 3. The §5.1.4.1.28.46 worked example (`ProjectionPoseRoll = 90`, type
//!    stays rectangular) round-trips.
//! 4. Omitting the call (the default) means no `Projection` master is
//!    written, so the demuxer surfaces `None`.
//! 5. Queueing `MkvProjection::default()` writes an empty `Projection`
//!    master that the demuxer parses into `Some(Projection::default())` —
//!    distinguishable on disk from the call-was-omitted case.
//! 6. The setter rejects calls made after `write_header`, out-of-range
//!    stream indices, and calls on non-video tracks.
//! 7. The on-disk bytes contain the `Projection` element id (`0x7670`)
//!    only when the API was called.
//!
//! These tests use the production EBML helpers to walk the muxed buffer —
//! no third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, MediaType, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo,
    TimeBase, WriteSeek,
};
use oxideav_mkv::demux::ProjectionType;
use oxideav_mkv::mux::{MkvMuxer, MkvProjection};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r272-vproj-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

fn video_stream() -> StreamInfo {
    let mut p = CodecParameters::video(CodecId::new("vp9"));
    p.width = Some(320);
    p.height = Some(240);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn audio_stream(index: u32) -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn keyframe_packet(stream: u32, pts: i64, marker: u8, len: usize) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![marker; len]);
    p.pts = Some(pts);
    p.flags.keyframe = true;
    p
}

/// Mux a single-track video MKV. `configure` runs between constructing
/// the muxer and `write_header`, so the test can opt the stream in to
/// `set_video_projection` (or not). Returns the muxed file's bytes.
fn mux_video<F>(configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let stream = video_stream();
        let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
        configure(&mut mx);
        mx.write_header().expect("write_header");
        mx.write_packet(&keyframe_packet(0, 0, 0xAA, 32))
            .expect("write_packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn demux_typed(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open_typed")
}

/// Scan for a two-byte element id followed by a size VINT. The
/// `Projection` master id 0x7670 encodes as the two bytes [0x76, 0x70].
fn has_two_byte_id(bytes: &[u8], id_hi: u8, id_lo: u8) -> bool {
    bytes.windows(2).any(|w| w[0] == id_hi && w[1] == id_lo)
}

#[test]
fn roundtrip_equirectangular_with_private_and_pose() {
    let private = vec![0x00, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44];
    let bytes = mux_video(|mx| {
        let mut p = MkvProjection::equirectangular(private.clone());
        p.pose_yaw = 30.0;
        p.pose_pitch = -15.0;
        p.pose_roll = 90.0;
        mx.set_video_projection(0, p).expect("set_video_projection");
    });
    let dmx = demux_typed(bytes);
    let proj = dmx.video_projection(0).expect("projection surfaces");
    assert_eq!(proj.projection_type(), ProjectionType::Equirectangular);
    assert_eq!(proj.private(), Some(private.as_slice()));
    assert_eq!(proj.pose_yaw(), 30.0);
    assert_eq!(proj.pose_pitch(), -15.0);
    assert_eq!(proj.pose_roll(), 90.0);
    assert!(proj.projection_type().is_spherical());
    assert!(proj.is_rotated());
}

#[test]
fn roundtrip_cubemap() {
    let private = vec![0x01, 0x02, 0x03, 0x04];
    let bytes = mux_video(|mx| {
        mx.set_video_projection(
            0,
            MkvProjection {
                projection_type: ProjectionType::Cubemap,
                private: Some(private.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    });
    let dmx = demux_typed(bytes);
    let proj = dmx.video_projection(0).unwrap();
    assert_eq!(proj.projection_type(), ProjectionType::Cubemap);
    assert_eq!(proj.private(), Some(private.as_slice()));
    // No pose was set — all three components default to 0.0.
    assert_eq!(proj.pose_yaw(), 0.0);
    assert_eq!(proj.pose_pitch(), 0.0);
    assert_eq!(proj.pose_roll(), 0.0);
    assert!(!proj.is_rotated());
}

#[test]
fn roundtrip_projection_type_other_passthrough() {
    // §27.15 leaves the "Matroska Projection Types" registry open; a value
    // outside Table 18's {0..=3} round-trips its wrapped u64 verbatim.
    let bytes = mux_video(|mx| {
        mx.set_video_projection(
            0,
            MkvProjection {
                projection_type: ProjectionType::Other(99),
                ..Default::default()
            },
        )
        .unwrap();
    });
    let dmx = demux_typed(bytes);
    assert_eq!(
        dmx.video_projection(0).unwrap().projection_type(),
        ProjectionType::Other(99)
    );
}

#[test]
fn roundtrip_worked_example_roll_only() {
    // RFC 9559 §5.1.4.1.28.46 worked example: a flat rectangular track
    // signalling a 90° counter-clockwise rotation via ProjectionPoseRoll.
    let bytes = mux_video(|mx| {
        mx.set_video_projection(0, MkvProjection::rotated(90.0))
            .unwrap();
    });
    let dmx = demux_typed(bytes);
    let proj = dmx.video_projection(0).unwrap();
    assert_eq!(proj.projection_type(), ProjectionType::Rectangular);
    assert_eq!(proj.pose_roll(), 90.0);
    assert_eq!(proj.pose_yaw(), 0.0);
    assert_eq!(proj.pose_pitch(), 0.0);
    assert_eq!(proj.private(), None);
    assert!(proj.is_rotated());
    assert!(!proj.projection_type().is_spherical());
}

#[test]
fn omitted_call_yields_none() {
    // No set_video_projection call → no Projection master written → the
    // demuxer surfaces None (the common 2D-video case).
    let bytes = mux_video(|_mx| {});
    let dmx = demux_typed(bytes);
    assert!(dmx.video_projection(0).is_none());
}

#[test]
fn empty_projection_master_round_trips_as_default() {
    // Queueing MkvProjection::default() writes an *empty* Projection master
    // (present-but-childless) which the demuxer parses into
    // Some(Projection::default()) with every getter at its spec default —
    // distinct from the call-omitted case (None).
    let bytes = mux_video(|mx| {
        mx.set_video_projection(0, MkvProjection::default())
            .unwrap();
    });
    // The Projection id 0x7670 IS present on disk.
    assert!(
        has_two_byte_id(&bytes, 0x76, 0x70),
        "empty Projection master must still be written"
    );
    let dmx = demux_typed(bytes);
    let proj = dmx.video_projection(0).expect("present but default");
    assert_eq!(proj.projection_type(), ProjectionType::Rectangular);
    assert_eq!(proj.pose_yaw(), 0.0);
    assert_eq!(proj.pose_pitch(), 0.0);
    assert_eq!(proj.pose_roll(), 0.0);
    assert_eq!(proj.private(), None);
}

#[test]
fn on_disk_bytes_contain_projection_id_only_when_set() {
    let with = mux_video(|mx| {
        mx.set_video_projection(0, MkvProjection::rotated(45.0))
            .unwrap();
    });
    let without = mux_video(|_mx| {});
    assert!(
        has_two_byte_id(&with, 0x76, 0x70),
        "Projection (0x7670) must be present when set_video_projection was called"
    );
    assert!(
        !has_two_byte_id(&without, 0x76, 0x70),
        "Projection (0x7670) must NOT be present when set_video_projection was not called"
    );
}

/// `Result<&mut MkvMuxer, Error>` — `expect_err` needs the OK arm to be
/// `Debug`, which `MkvMuxer` deliberately is not. Unwrap the error without
/// requiring `Debug` on the success type.
#[track_caller]
fn assert_err<T>(r: Result<T, Error>, msg: &str) -> Error {
    match r {
        Ok(_) => panic!("{msg}: expected Err, got Ok"),
        Err(e) => e,
    }
}

#[test]
fn rejects_call_after_write_header() {
    let tmp = tmp_path("post_header");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    mx.write_header().unwrap();
    let err = assert_err(
        mx.set_video_projection(0, MkvProjection::default()),
        "must reject after write_header",
    );
    assert!(matches!(err, Error::Other(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_out_of_range_stream_index() {
    let tmp = tmp_path("oor");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    let err = assert_err(
        mx.set_video_projection(5, MkvProjection::default()),
        "must reject out-of-range index",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_call_on_audio_track() {
    let tmp = tmp_path("audio");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(), audio_stream(1)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).unwrap();
    assert_eq!(streams[1].params.media_type, MediaType::Audio);
    let err = assert_err(
        mx.set_video_projection(1, MkvProjection::default()),
        "must reject on audio track",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn second_call_overwrites_first() {
    let bytes = mux_video(|mx| {
        mx.set_video_projection(0, MkvProjection::rotated(45.0))
            .unwrap();
        mx.set_video_projection(0, MkvProjection::rotated(90.0))
            .unwrap();
    });
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_projection(0).unwrap().pose_roll(), 90.0);
}

#[test]
fn accessor_reflects_queued_value() {
    let tmp = tmp_path("accessor");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    assert_eq!(mx.video_projection(0), None, "projection starts unset");
    mx.set_video_projection(0, MkvProjection::rotated(90.0))
        .unwrap();
    assert_eq!(
        mx.video_projection(0).map(|p| p.pose_roll),
        Some(90.0),
        "queued value reflected"
    );
    let _ = std::fs::remove_file(&tmp);
}
