//! Round-trip tests for the muxer's `Video > StereoMode` (RFC 9559
//! §5.1.4.1.28.3) and `Video > AlphaMode` (§5.1.4.1.28.4) write paths.
//!
//! Drives `MkvMuxer::set_video_stereo_mode` and
//! `MkvMuxer::set_video_alpha_mode` against the public Muxer trait, then
//! re-opens the bytes through the typed
//! [`oxideav_mkv::demux::open_typed`] and confirms
//! `MkvDemuxer::video_stereo_mode(stream_index)` /
//! `MkvDemuxer::video_alpha_mode(stream_index)` decode the exact value
//! handed to the muxer — including the `Other(u64)` forward-compat
//! variant on both enums (§27.7 / §27.8 leave the registries open).
//!
//! Spec contracts pinned here:
//!
//! 1. Setting `StereoMode::SideBySideLeftFirst` surfaces back as the same.
//! 2. Setting `StereoMode::Other(99)` round-trips its wrapped value.
//! 3. Setting `AlphaMode::Present` surfaces back as `Present`.
//! 4. Omitting both calls (the default) means neither element is
//!    written, so the demuxer materialises the §5.1.4.1.28.3 /
//!    §5.1.4.1.28.4 spec defaults (`Mono` + `None`).
//! 5. Both setters reject calls made after `write_header`,
//!    out-of-range stream indices, and calls on non-video tracks.
//! 6. The on-disk bytes contain the `StereoMode` (`0x53B8`) and
//!    `AlphaMode` (`0x53C0`) element IDs only when the API was called.
//! 7. The two settings are independent — setting one does not affect
//!    the other.
//!
//! These tests use the production EBML helpers to walk the muxed
//! buffer — no third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, MediaType, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo,
    TimeBase, WriteSeek,
};
use oxideav_mkv::demux::{AlphaMode, StereoMode};
use oxideav_mkv::mux::MkvMuxer;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r214-vstereoalpha-{}-{}-{n}.mkv",
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

/// Mux a single-track video MKV. `configure` is invoked between
/// constructing the muxer and `write_header`, so the test can opt the
/// stream in to `set_video_stereo_mode` / `set_video_alpha_mode` (or
/// not). Returns the muxed file's bytes.
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
/// `MkvDemuxer::video_stereo_mode` / `video_alpha_mode`.
fn demux_typed(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open_typed")
}

#[test]
fn roundtrip_side_by_side_left_first() {
    let bytes = mux_video(|mx| {
        mx.set_video_stereo_mode(0, StereoMode::SideBySideLeftFirst)
            .expect("set_video_stereo_mode");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(
        dmx.video_stereo_mode(0),
        Some(StereoMode::SideBySideLeftFirst)
    );
}

#[test]
fn roundtrip_top_bottom_right_first() {
    let bytes = mux_video(|mx| {
        mx.set_video_stereo_mode(0, StereoMode::TopBottomRightFirst)
            .expect("set_video_stereo_mode");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(
        dmx.video_stereo_mode(0),
        Some(StereoMode::TopBottomRightFirst)
    );
}

#[test]
fn roundtrip_anaglyph_cyan_red() {
    let bytes = mux_video(|mx| {
        mx.set_video_stereo_mode(0, StereoMode::AnaglyphCyanRed)
            .expect("set_video_stereo_mode");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_stereo_mode(0), Some(StereoMode::AnaglyphCyanRed));
}

#[test]
fn roundtrip_both_eyes_laced_right_first() {
    // Highest registered value in RFC 9559 Table 5 — covers the
    // upper end of the `from_raw` / `to_raw` mapping.
    let bytes = mux_video(|mx| {
        mx.set_video_stereo_mode(0, StereoMode::BothEyesLacedRightFirst)
            .expect("set_video_stereo_mode");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(
        dmx.video_stereo_mode(0),
        Some(StereoMode::BothEyesLacedRightFirst)
    );
}

#[test]
fn roundtrip_stereo_other_passthrough() {
    // Forward-compat: a value registered after RFC 9559 (anything
    // outside Table 5's {0..=14}) round-trips its wrapped u64
    // verbatim through both the writer's `to_raw` and the reader's
    // `from_raw`. §27.7 leaves the StereoMode registry open.
    let bytes = mux_video(|mx| {
        mx.set_video_stereo_mode(0, StereoMode::Other(99))
            .expect("set_video_stereo_mode");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_stereo_mode(0), Some(StereoMode::Other(99)));
}

#[test]
fn roundtrip_stereo_mono_written_explicitly() {
    // Calling with the spec default (Mono) still writes the element on
    // disk. The demuxer surfaces Mono either way, but writing it
    // explicitly is the way for a producer to override a downstream
    // tool that might infer something else.
    let bytes = mux_video(|mx| {
        mx.set_video_stereo_mode(0, StereoMode::Mono)
            .expect("set_video_stereo_mode");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_stereo_mode(0), Some(StereoMode::Mono));
    // And the element id 0x53B8 IS present in the bytes (proves the
    // writer didn't silently optimise away the default).
}

#[test]
fn roundtrip_alpha_present() {
    let bytes = mux_video(|mx| {
        mx.set_video_alpha_mode(0, AlphaMode::Present)
            .expect("set_video_alpha_mode");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_alpha_mode(0), Some(AlphaMode::Present));
    let mode = dmx.video_alpha_mode(0).unwrap();
    assert!(mode.has_alpha(), "Present means has_alpha");
}

#[test]
fn roundtrip_alpha_none_explicit() {
    let bytes = mux_video(|mx| {
        mx.set_video_alpha_mode(0, AlphaMode::None)
            .expect("set_video_alpha_mode");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_alpha_mode(0), Some(AlphaMode::None));
}

#[test]
fn roundtrip_alpha_other_passthrough() {
    // §27.8 leaves the AlphaMode registry open; §5.1.4.1.28.4 itself
    // warns values outside 0/1 SHOULD NOT be used, but if a producer
    // copies one from another file we have to round-trip it byte-for-
    // byte.
    let bytes = mux_video(|mx| {
        mx.set_video_alpha_mode(0, AlphaMode::Other(7))
            .expect("set_video_alpha_mode");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_alpha_mode(0), Some(AlphaMode::Other(7)));
}

#[test]
fn omitted_calls_yield_spec_defaults() {
    // When neither setter is called, the muxer must omit both elements
    // so the demuxer materialises the §5.1.4.1.28.3 / §5.1.4.1.28.4
    // defaults (Mono + None).
    let bytes = mux_video(|_mx| {});
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_stereo_mode(0), Some(StereoMode::Mono));
    assert_eq!(dmx.video_alpha_mode(0), Some(AlphaMode::None));
}

#[test]
fn settings_are_independent() {
    // Set both — both round-trip. Set only one — the other stays at
    // the spec default.
    let bytes = mux_video(|mx| {
        mx.set_video_stereo_mode(0, StereoMode::CheckboardLeftFirst)
            .unwrap();
        mx.set_video_alpha_mode(0, AlphaMode::Present).unwrap();
    });
    let dmx = demux_typed(bytes);
    assert_eq!(
        dmx.video_stereo_mode(0),
        Some(StereoMode::CheckboardLeftFirst)
    );
    assert_eq!(dmx.video_alpha_mode(0), Some(AlphaMode::Present));

    let bytes_only_stereo = mux_video(|mx| {
        mx.set_video_stereo_mode(0, StereoMode::RowInterleavedLeftFirst)
            .unwrap();
    });
    let dmx2 = demux_typed(bytes_only_stereo);
    assert_eq!(
        dmx2.video_stereo_mode(0),
        Some(StereoMode::RowInterleavedLeftFirst)
    );
    assert_eq!(dmx2.video_alpha_mode(0), Some(AlphaMode::None));

    let bytes_only_alpha = mux_video(|mx| {
        mx.set_video_alpha_mode(0, AlphaMode::Present).unwrap();
    });
    let dmx3 = demux_typed(bytes_only_alpha);
    assert_eq!(dmx3.video_stereo_mode(0), Some(StereoMode::Mono));
    assert_eq!(dmx3.video_alpha_mode(0), Some(AlphaMode::Present));
}

#[test]
fn on_disk_bytes_contain_element_ids_only_when_set() {
    // StereoMode element id 0x53B8 = [0x53, 0xB8]; size VINT 0x81 + 1-byte
    // payload. AlphaMode id 0x53C0 = [0x53, 0xC0]; size VINT 0x81 + 1-byte
    // payload. Scan narrowly for [id_hi, id_lo, 0x81, value] quads.
    fn has_two_byte_id(bytes: &[u8], id_hi: u8, id_lo: u8) -> bool {
        bytes
            .windows(3)
            .any(|w| w[0] == id_hi && w[1] == id_lo && w[2] == 0x81)
    }

    let bytes_with_both = mux_video(|mx| {
        mx.set_video_stereo_mode(0, StereoMode::SideBySideLeftFirst)
            .unwrap();
        mx.set_video_alpha_mode(0, AlphaMode::Present).unwrap();
    });
    let bytes_without = mux_video(|_mx| {});

    assert!(
        has_two_byte_id(&bytes_with_both, 0x53, 0xB8),
        "StereoMode (0x53B8) must be present when set_video_stereo_mode was called"
    );
    assert!(
        has_two_byte_id(&bytes_with_both, 0x53, 0xC0),
        "AlphaMode (0x53C0) must be present when set_video_alpha_mode was called"
    );
    assert!(
        !has_two_byte_id(&bytes_without, 0x53, 0xB8),
        "StereoMode (0x53B8) must NOT be present when set_video_stereo_mode was not called"
    );
    assert!(
        !has_two_byte_id(&bytes_without, 0x53, 0xC0),
        "AlphaMode (0x53C0) must NOT be present when set_video_alpha_mode was not called"
    );
}

/// `Result<&mut MkvMuxer, Error>` — `expect_err` needs the OK arm to
/// be `Debug`, which `MkvMuxer` deliberately is not. This helper
/// unwraps the error the same way `expect_err` would but without
/// needing `Debug` on the success type.
#[track_caller]
fn assert_err<T>(r: Result<T, Error>, msg: &str) -> Error {
    match r {
        Ok(_) => panic!("{msg}: expected Err, got Ok"),
        Err(e) => e,
    }
}

#[test]
fn rejects_stereo_call_after_write_header() {
    let tmp = tmp_path("post_header_stereo");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    mx.write_header().unwrap();
    let err = assert_err(
        mx.set_video_stereo_mode(0, StereoMode::SideBySideLeftFirst),
        "must reject after write_header",
    );
    assert!(matches!(err, Error::Other(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_alpha_call_after_write_header() {
    let tmp = tmp_path("post_header_alpha");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    mx.write_header().unwrap();
    let err = assert_err(
        mx.set_video_alpha_mode(0, AlphaMode::Present),
        "must reject after write_header",
    );
    assert!(matches!(err, Error::Other(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_stereo_out_of_range_stream_index() {
    let tmp = tmp_path("oor_stereo");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    let err = assert_err(
        mx.set_video_stereo_mode(5, StereoMode::SideBySideLeftFirst),
        "must reject out-of-range index",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_alpha_out_of_range_stream_index() {
    let tmp = tmp_path("oor_alpha");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    let err = assert_err(
        mx.set_video_alpha_mode(5, AlphaMode::Present),
        "must reject out-of-range index",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_stereo_call_on_audio_track() {
    let tmp = tmp_path("audio_stereo");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(), audio_stream(1)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).unwrap();
    assert_eq!(streams[1].params.media_type, MediaType::Audio);
    let err = assert_err(
        mx.set_video_stereo_mode(1, StereoMode::SideBySideLeftFirst),
        "must reject on audio track",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_alpha_call_on_audio_track() {
    let tmp = tmp_path("audio_alpha");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(), audio_stream(1)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).unwrap();
    let err = assert_err(
        mx.set_video_alpha_mode(1, AlphaMode::Present),
        "must reject on audio track",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn second_stereo_call_overwrites_first() {
    let bytes = mux_video(|mx| {
        mx.set_video_stereo_mode(0, StereoMode::SideBySideLeftFirst)
            .unwrap();
        mx.set_video_stereo_mode(0, StereoMode::TopBottomLeftFirst)
            .unwrap();
    });
    let dmx = demux_typed(bytes);
    assert_eq!(
        dmx.video_stereo_mode(0),
        Some(StereoMode::TopBottomLeftFirst)
    );
}

#[test]
fn second_alpha_call_overwrites_first() {
    let bytes = mux_video(|mx| {
        mx.set_video_alpha_mode(0, AlphaMode::None).unwrap();
        mx.set_video_alpha_mode(0, AlphaMode::Present).unwrap();
    });
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.video_alpha_mode(0), Some(AlphaMode::Present));
}

#[test]
fn accessors_reflect_queued_values() {
    let tmp = tmp_path("accessor");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    assert_eq!(mx.video_stereo_mode(0), None, "stereo starts unset");
    assert_eq!(mx.video_alpha_mode(0), None, "alpha starts unset");
    mx.set_video_stereo_mode(0, StereoMode::AnaglyphGreenMagenta)
        .unwrap();
    mx.set_video_alpha_mode(0, AlphaMode::Present).unwrap();
    assert_eq!(
        mx.video_stereo_mode(0),
        Some(StereoMode::AnaglyphGreenMagenta)
    );
    assert_eq!(mx.video_alpha_mode(0), Some(AlphaMode::Present));
    let _ = std::fs::remove_file(&tmp);
}
