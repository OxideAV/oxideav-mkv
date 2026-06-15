//! Round-trip tests for the muxer's `TrackOperation` (RFC 9559
//! §5.1.4.1.30) write path.
//!
//! Drives `MkvMuxer::set_track_operation` against the public Muxer trait,
//! then re-opens the bytes through the typed
//! [`oxideav_mkv::demux::open_typed`] and confirms
//! `MkvDemuxer::track_operation(stream_index)` decodes the exact
//! `TrackCombinePlanes` / `TrackJoinBlocks` structure handed to the muxer,
//! with every plane / join reference resolved back to the source stream
//! index it pointed at.
//!
//! Spec contracts pinned here:
//!
//! 1. A `TrackCombinePlanes` stereoscopic-3D recipe (a left-eye plane and
//!    a right-eye plane, §5.1.4.1.30.1..§5.1.4.1.30.4) round-trips: every
//!    `TrackPlaneUID` resolves back to its source stream index and every
//!    `TrackPlaneType` (including the `Other(u64)` forward-compat variant)
//!    survives verbatim.
//! 2. A `TrackJoinBlocks` recipe (§5.1.4.1.30.5..§5.1.4.1.30.6) round-trips
//!    its joined stream references.
//! 3. The two operation kinds coexist on one track.
//! 4. Omitting the call keeps the `TrackOperation` master off-disk so the
//!    demuxer surfaces `None`.
//! 5. The setter rejects calls after `write_header`, out-of-range stream
//!    indices, empty operations, and out-of-range plane / join references.
//! 6. The on-disk bytes carry the `TrackOperation` (`0xE2`) element id only
//!    when the API was called.
//!
//! These tests use the production demuxer to walk the muxed buffer — no
//! third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, MediaType, Muxer, Packet, ReadSeek, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mkv::demux::TrackPlaneType;
use oxideav_mkv::mux::{MkvMuxer, MkvTrackOperation, MkvTrackPlane};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r314-trackop-{}-{}-{n}.mkv",
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

fn keyframe_packet(stream: u32, pts: i64) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![0xAA; 16]);
    p.pts = Some(pts);
    p.flags.keyframe = true;
    p
}

/// Mux a three-video-track MKV. `configure` runs between construction and
/// `write_header`, so a test can queue `set_track_operation` on stream 0.
fn mux_three_tracks<F>(configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let streams = vec![video_stream(0), video_stream(1), video_stream(2)];
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer construct");
        configure(&mut mx);
        mx.write_header().expect("write_header");
        // Each track produces one packet so the file is well-formed.
        mx.write_packet(&keyframe_packet(0, 0)).expect("write 0");
        mx.write_packet(&keyframe_packet(1, 0)).expect("write 1");
        mx.write_packet(&keyframe_packet(2, 0)).expect("write 2");
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

#[test]
fn roundtrip_combine_planes_stereo_3d() {
    // Stream 0 is a virtual 3D track combining a left-eye plane (stream 1)
    // and a right-eye plane (stream 2).
    let bytes = mux_three_tracks(|mx| {
        mx.set_track_operation(0, MkvTrackOperation::stereo_3d(1, 2))
            .expect("set_track_operation");
    });
    let dmx = demux_typed(bytes);
    let op = dmx
        .track_operation(0)
        .expect("stream 0 carries a TrackOperation");
    assert_eq!(op.planes.len(), 2, "two planes");
    assert!(op.join_tracks.is_empty(), "no join in a pure combine");

    assert_eq!(op.planes[0].plane_type, TrackPlaneType::LeftEye);
    assert_eq!(op.planes[0].track.stream_index, Some(1));
    assert_eq!(op.planes[1].plane_type, TrackPlaneType::RightEye);
    assert_eq!(op.planes[1].track.stream_index, Some(2));

    // The other two tracks are ordinary — no TrackOperation.
    assert!(dmx.track_operation(1).is_none());
    assert!(dmx.track_operation(2).is_none());
}

#[test]
fn roundtrip_combine_planes_with_background_and_other() {
    // Three planes including a `Background` and a forward-compat
    // `Other(u64)` plane type (§27.17 leaves the registry open).
    let op = MkvTrackOperation {
        combine_planes: vec![
            MkvTrackPlane {
                stream_index: 1,
                plane_type: TrackPlaneType::Background,
            },
            MkvTrackPlane {
                stream_index: 2,
                plane_type: TrackPlaneType::Other(42),
            },
        ],
        join_tracks: Vec::new(),
    };
    let bytes = mux_three_tracks(|mx| {
        mx.set_track_operation(0, op).expect("set_track_operation");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.track_operation(0).unwrap();
    assert_eq!(got.planes[0].plane_type, TrackPlaneType::Background);
    assert_eq!(got.planes[0].track.stream_index, Some(1));
    assert_eq!(got.planes[1].plane_type, TrackPlaneType::Other(42));
    assert_eq!(got.planes[1].track.stream_index, Some(2));
}

#[test]
fn roundtrip_join_blocks() {
    // Stream 0 joins the Blocks of streams 1 and 2 onto one timeline.
    let bytes = mux_three_tracks(|mx| {
        mx.set_track_operation(0, MkvTrackOperation::join(vec![1, 2]))
            .expect("set_track_operation");
    });
    let dmx = demux_typed(bytes);
    let op = dmx.track_operation(0).unwrap();
    assert!(op.planes.is_empty(), "no planes in a pure join");
    assert_eq!(op.join_tracks.len(), 2);
    assert_eq!(op.join_tracks[0].stream_index, Some(1));
    assert_eq!(op.join_tracks[1].stream_index, Some(2));
}

#[test]
fn roundtrip_combine_and_join_coexist() {
    // The spec lets a single TrackOperation carry both a
    // TrackCombinePlanes and a TrackJoinBlocks.
    let op = MkvTrackOperation {
        combine_planes: vec![MkvTrackPlane {
            stream_index: 1,
            plane_type: TrackPlaneType::LeftEye,
        }],
        join_tracks: vec![2],
    };
    let bytes = mux_three_tracks(|mx| {
        mx.set_track_operation(0, op).expect("set_track_operation");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.track_operation(0).unwrap();
    assert_eq!(got.planes.len(), 1);
    assert_eq!(got.planes[0].track.stream_index, Some(1));
    assert_eq!(got.join_tracks.len(), 1);
    assert_eq!(got.join_tracks[0].stream_index, Some(2));
}

#[test]
fn omitted_call_yields_none() {
    let bytes = mux_three_tracks(|_mx| {});
    let dmx = demux_typed(bytes);
    assert!(dmx.track_operation(0).is_none());
    assert!(dmx.track_operation(1).is_none());
    assert!(dmx.track_operation(2).is_none());
}

#[test]
fn on_disk_bytes_carry_element_only_when_set() {
    // TrackOperation id 0xE2 is a single byte. Combined with the following
    // size VINT it's unlikely to collide with payload here, but to keep
    // the check robust we look for the TrackPlaneType id 0xE6 which only
    // appears inside a TrackCombinePlanes.
    let with = mux_three_tracks(|mx| {
        mx.set_track_operation(0, MkvTrackOperation::stereo_3d(1, 2))
            .unwrap();
    });
    let without = mux_three_tracks(|_mx| {});
    assert!(
        with.windows(1).any(|w| w[0] == 0xE6),
        "TrackPlaneType id 0xE6 must be present when set_track_operation was called"
    );
    assert!(
        !without.windows(1).any(|w| w[0] == 0xE6),
        "TrackPlaneType id 0xE6 must NOT be present without the call"
    );
}

/// Unwrap an error from a `Result` whose Ok arm (`&mut MkvMuxer`) is not
/// `Debug`.
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
    let streams = vec![video_stream(0), video_stream(1)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).unwrap();
    mx.write_header().unwrap();
    let err = assert_err(
        mx.set_track_operation(0, MkvTrackOperation::join(vec![1])),
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
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream(0)]).unwrap();
    let err = assert_err(
        mx.set_track_operation(5, MkvTrackOperation::join(vec![0])),
        "must reject out-of-range index",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_empty_operation() {
    let tmp = tmp_path("empty");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream(0)]).unwrap();
    let err = assert_err(
        mx.set_track_operation(0, MkvTrackOperation::new()),
        "must reject an empty TrackOperation",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_out_of_range_plane_reference() {
    let tmp = tmp_path("oor_plane");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0), video_stream(1)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).unwrap();
    // Plane references stream 9 which doesn't exist.
    let err = assert_err(
        mx.set_track_operation(0, MkvTrackOperation::stereo_3d(1, 9)),
        "must reject out-of-range plane reference",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_out_of_range_join_reference() {
    let tmp = tmp_path("oor_join");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0), video_stream(1)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).unwrap();
    let err = assert_err(
        mx.set_track_operation(0, MkvTrackOperation::join(vec![1, 7])),
        "must reject out-of-range join reference",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn accessor_reflects_queued_value() {
    let tmp = tmp_path("accessor");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0), video_stream(1), video_stream(2)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).unwrap();
    assert!(mx.track_operation(0).is_none(), "starts unset");
    mx.set_track_operation(0, MkvTrackOperation::stereo_3d(1, 2))
        .unwrap();
    let queued = mx.track_operation(0).expect("now set");
    assert_eq!(queued.combine_planes.len(), 2);
    assert_eq!(queued.combine_planes[0].plane_type, TrackPlaneType::LeftEye);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn second_call_overwrites_first() {
    let bytes = mux_three_tracks(|mx| {
        mx.set_track_operation(0, MkvTrackOperation::join(vec![1]))
            .unwrap();
        mx.set_track_operation(0, MkvTrackOperation::stereo_3d(1, 2))
            .unwrap();
    });
    let dmx = demux_typed(bytes);
    let op = dmx.track_operation(0).unwrap();
    assert!(
        op.join_tracks.is_empty(),
        "join from first call was replaced"
    );
    assert_eq!(op.planes.len(), 2, "planes from second call won");
}

#[test]
fn non_video_track_accepted() {
    // Unlike the set_video_* family, TrackOperation has no track-type
    // restriction in this muxer surface (the spec carries the element on
    // any TrackEntry; a TrackJoinBlocks virtual track is commonly audio).
    let tmp = tmp_path("audio_join");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut audio = CodecParameters::audio(CodecId::new("pcm_s16le"));
    audio.sample_rate = Some(48_000);
    audio.channels = Some(2);
    let a0 = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params: audio.clone(),
    };
    let a1 = StreamInfo {
        index: 1,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params: audio,
    };
    let streams = vec![a0, a1];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).unwrap();
    assert_eq!(streams[0].params.media_type, MediaType::Audio);
    mx.set_track_operation(0, MkvTrackOperation::join(vec![1]))
        .expect("audio track accepts TrackOperation");
    let _ = std::fs::remove_file(&tmp);
}
