//! Round-trip tests for the muxer's `Video > AspectRatioType`
//! (RFC 9559 Appendix A.24, reclaimed, id `0x54B3`) write path.
//!
//! Drives [`MkvMuxer::set_video_aspect_ratio_type`] against the public
//! Muxer trait, then re-opens the bytes through the typed
//! [`oxideav_mkv::demux::open_typed`] and confirms
//! [`MkvDemuxer::video_aspect_ratio_type`] decodes the exact raw `u64`
//! handed to the muxer.
//!
//! Spec contracts pinned here:
//!
//! 1. Setting a value surfaces back as the same raw `u64`.
//! 2. The value 0 round-trips as `Some(0)` (the reclaimed appendix
//!    defines no default, so an explicit 0 is observable and distinct
//!    from absence).
//! 3. A large value round-trips verbatim — the element is a `uinteger`
//!    with no enumerated range.
//! 4. Omitting the call (the default) means the element is not written,
//!    and the demuxer surfaces `None` — Appendix A.24 defines no
//!    default, so absence is not materialised on either side.
//! 5. The setter rejects calls made after `write_header`, out-of-range
//!    stream indices, and calls on non-video tracks.
//! 6. The on-disk bytes contain the `AspectRatioType` element id
//!    `0x54B3` only when the API was called.
//! 7. Calling the setter twice on the same `stream_index` is
//!    last-write-wins.
//! 8. The setter is independent of the other `Video` hints.
//!
//! These tests use the production EBML helpers to walk the muxed
//! buffer — no third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, MediaType, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo,
    TimeBase, WriteSeek,
};
use oxideav_mkv::mux::MkvMuxer;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r294-vart-{}-{}-{n}.mkv",
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
/// `set_video_aspect_ratio_type` (or not). Returns the muxed bytes.
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

/// Open `bytes` through the typed demuxer entry point so we can reach
/// [`oxideav_mkv::demux::MkvDemuxer::video_aspect_ratio_type`].
fn demux_typed(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open_typed")
}

#[test]
fn roundtrip_nonzero_value() {
    let bytes = mux_video(|mx| {
        mx.set_video_aspect_ratio_type(0, 3)
            .expect("set_video_aspect_ratio_type");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_aspect_ratio_type(0), Some(3));
}

#[test]
fn roundtrip_explicit_zero_is_observable() {
    // Appendix A.24 defines no default — an explicit 0 must round-trip
    // as `Some(0)`, distinct from absence (`None`).
    let bytes = mux_video(|mx| {
        mx.set_video_aspect_ratio_type(0, 0)
            .expect("set_video_aspect_ratio_type");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_aspect_ratio_type(0), Some(0));
}

#[test]
fn roundtrip_large_value() {
    // No enumerated range — a large `uinteger` round-trips verbatim.
    let v = 0x0123_4567_89AB_CDEF;
    let bytes = mux_video(|mx| {
        mx.set_video_aspect_ratio_type(0, v)
            .expect("set_video_aspect_ratio_type");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_aspect_ratio_type(0), Some(v));
}

#[test]
fn omitted_call_yields_none() {
    // Appendix A.24 defines no default. Omitting the setter must keep
    // the element off-disk so the typed demuxer surfaces `None`.
    let bytes = mux_video(|_mx| {});
    let dmx = demux_typed(bytes);
    assert!(
        dmx.video_aspect_ratio_type(0).is_none(),
        "absent AspectRatioType must surface as None"
    );
}

#[test]
fn on_disk_bytes_contain_element_id_only_when_set() {
    // AspectRatioType id 0x54B3 = [0x54, 0xB3]. Scan for the two-byte
    // id prefix in the muxed buffer.
    fn has_two_byte_id(bytes: &[u8], id0: u8, id1: u8) -> bool {
        bytes.windows(2).any(|w| w[0] == id0 && w[1] == id1)
    }

    let bytes_with = mux_video(|mx| {
        mx.set_video_aspect_ratio_type(0, 3).unwrap();
    });
    let bytes_without = mux_video(|_mx| {});

    assert!(
        has_two_byte_id(&bytes_with, 0x54, 0xB3),
        "AspectRatioType (0x54B3) must be present when set_video_aspect_ratio_type was called"
    );
    assert!(
        !has_two_byte_id(&bytes_without, 0x54, 0xB3),
        "AspectRatioType (0x54B3) must NOT be present when the setter was not called"
    );
}

#[test]
fn last_write_wins() {
    let bytes = mux_video(|mx| {
        mx.set_video_aspect_ratio_type(0, 1).unwrap();
        mx.set_video_aspect_ratio_type(0, 7).unwrap();
    });
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_aspect_ratio_type(0), Some(7));
}

#[test]
fn read_back_accessor_returns_queued_value() {
    // Pre-`write_header`, the muxer's own accessor surfaces the queued
    // hint.
    let tmp = tmp_path("accessor");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    assert!(mx.video_aspect_ratio_type(0).is_none());
    mx.set_video_aspect_ratio_type(0, 5).unwrap();
    assert_eq!(mx.video_aspect_ratio_type(0), Some(5));
    let _ = std::fs::remove_file(&tmp);
}

/// `Result<&mut MkvMuxer, Error>` — `expect_err` needs the OK arm to be
/// `Debug`, which `MkvMuxer` deliberately is not. This helper unwraps
/// the error the same way `expect_err` would but without needing
/// `Debug` on the success type.
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
        mx.set_video_aspect_ratio_type(0, 3),
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
        mx.set_video_aspect_ratio_type(5, 3),
        "must reject out-of-range index",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_call_on_audio_track() {
    let tmp = tmp_path("audio_track");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(), audio_stream(1)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).unwrap();
    assert_eq!(streams[1].params.media_type, MediaType::Audio);
    let err = assert_err(
        mx.set_video_aspect_ratio_type(1, 3),
        "must reject on audio track",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn independent_of_other_video_setters() {
    // Setting `set_video_aspect_ratio_type` must not affect any other
    // queued Video hint — exercising the FourCC setter alongside this
    // one keeps both round-tripping independently.
    let bytes = mux_video(|mx| {
        mx.set_video_aspect_ratio_type(0, 2).unwrap();
        mx.set_video_uncompressed_fourcc(0, *b"YUY2").unwrap();
    });
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_aspect_ratio_type(0), Some(2));
    assert_eq!(
        dmx.video_uncompressed_fourcc(0).and_then(|f| f.fourcc()),
        Some(*b"YUY2")
    );
}
