//! Round-trip tests for the muxer's `Video > UncompressedFourCC`
//! (RFC 9559 §5.1.4.1.28.15) write path.
//!
//! Drives [`MkvMuxer::set_video_uncompressed_fourcc`] against the
//! public Muxer trait, then re-opens the bytes through the typed
//! [`oxideav_mkv::demux::open_typed`] and confirms
//! [`MkvDemuxer::video_uncompressed_fourcc`] decodes the exact value
//! handed to the muxer (including the four-byte preview via
//! [`UncompressedFourCC::fourcc`] and the UTF-8 lossy
//! [`UncompressedFourCC::as_str`] convenience).
//!
//! Spec contracts pinned here:
//!
//! 1. Setting `*b"YUY2"` surfaces back as the same four-byte FourCC.
//! 2. Setting `*b"BGRA"` round-trips.
//! 3. Omitting the call (the default) means the element is not
//!    written, and the demuxer surfaces `None` per §5.1.4.1.28.15
//!    (the spec defines no default — Table 11 only pins
//!    `minOccurs=1` for `CodecID == "V_UNCOMPRESSED"`).
//! 4. The setter rejects calls made after `write_header`,
//!    out-of-range stream indices, and calls on non-video tracks.
//! 5. The on-disk bytes contain the `UncompressedFourCC` element id
//!    `0x2EB524` only when the API was called.
//! 6. Calling the setter twice on the same `stream_index` is
//!    last-write-wins.
//! 7. The element honours the spec's fixed `length: 4` (the on-disk
//!    payload is exactly four bytes).
//! 8. A non-printable / high-byte FourCC (`[0xFF, 0x00, 0x12, 0xAB]`)
//!    round-trips verbatim — the schema is `binary`, not `string`.
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
        "oxideav-mkv-r225-vufourcc-{}-{}-{n}.mkv",
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
/// stream in to `set_video_uncompressed_fourcc` (or not). Returns the
/// muxed file's bytes.
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

/// Open `bytes` through the typed demuxer entry point so we can
/// reach [`oxideav_mkv::demux::MkvDemuxer::video_uncompressed_fourcc`].
fn demux_typed(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open_typed")
}

#[test]
fn roundtrip_yuy2_printable() {
    // The canonical 4:2:2-packed YUV FourCC used by countless
    // raw-video files. Round-trips verbatim.
    let bytes = mux_video(|mx| {
        mx.set_video_uncompressed_fourcc(0, *b"YUY2")
            .expect("set_video_uncompressed_fourcc");
    });
    let dmx = demux_typed(bytes);
    let fourcc = dmx
        .video_uncompressed_fourcc(0)
        .expect("UncompressedFourCC surfaced");
    assert_eq!(fourcc.fourcc(), Some(*b"YUY2"));
    assert_eq!(fourcc.as_str().as_deref(), Some("YUY2"));
}

#[test]
fn roundtrip_bgra_alpha() {
    // 32-bit BGRA — a common four-channel uncompressed pixel layout.
    let bytes = mux_video(|mx| {
        mx.set_video_uncompressed_fourcc(0, *b"BGRA")
            .expect("set_video_uncompressed_fourcc");
    });
    let dmx = demux_typed(bytes);
    let fourcc = dmx
        .video_uncompressed_fourcc(0)
        .expect("UncompressedFourCC surfaced");
    assert_eq!(fourcc.fourcc(), Some(*b"BGRA"));
}

#[test]
fn roundtrip_nv12_planar() {
    // 8-bit 4:2:0 planar, the most common GPU-decoder output FourCC.
    let bytes = mux_video(|mx| {
        mx.set_video_uncompressed_fourcc(0, *b"NV12")
            .expect("set_video_uncompressed_fourcc");
    });
    let dmx = demux_typed(bytes);
    let fourcc = dmx
        .video_uncompressed_fourcc(0)
        .expect("UncompressedFourCC surfaced");
    assert_eq!(fourcc.fourcc(), Some(*b"NV12"));
    assert_eq!(fourcc.as_bytes(), b"NV12");
}

#[test]
fn roundtrip_high_byte_passthrough() {
    // §5.1.4.1.28.15 declares the element `binary`, not `string`.
    // A four-byte payload mixing a control byte (0x00), a high byte
    // (0xFF), and two printable bytes must round-trip verbatim — the
    // muxer must not treat the value as UTF-8.
    let payload: [u8; 4] = [0xFF, 0x00, 0x12, 0xAB];
    let bytes = mux_video(|mx| {
        mx.set_video_uncompressed_fourcc(0, payload)
            .expect("set_video_uncompressed_fourcc");
    });
    let dmx = demux_typed(bytes);
    let fourcc = dmx
        .video_uncompressed_fourcc(0)
        .expect("UncompressedFourCC surfaced");
    assert_eq!(fourcc.fourcc(), Some(payload));
    assert_eq!(fourcc.as_bytes(), &payload);
}

#[test]
fn omitted_call_yields_none() {
    // The element has no spec default per §5.1.4.1.28.15 — only
    // Table 11's `minOccurs=1` for `CodecID == "V_UNCOMPRESSED"`,
    // which this VP9 track isn't. Omitting the setter must keep the
    // element off-disk so the typed demuxer surfaces `None`.
    let bytes = mux_video(|_mx| {});
    let dmx = demux_typed(bytes);
    assert!(
        dmx.video_uncompressed_fourcc(0).is_none(),
        "absent UncompressedFourCC must surface as None"
    );
}

#[test]
fn on_disk_bytes_contain_element_id_only_when_set() {
    // UncompressedFourCC id 0x2EB524 = [0x2E, 0xB5, 0x24]; size VINT
    // 0x84 + 4-byte payload. Scan for [id0, id1, id2, 0x84] quads.
    fn has_three_byte_id(bytes: &[u8], id0: u8, id1: u8, id2: u8) -> bool {
        bytes
            .windows(4)
            .any(|w| w[0] == id0 && w[1] == id1 && w[2] == id2 && w[3] == 0x84)
    }

    let bytes_with = mux_video(|mx| {
        mx.set_video_uncompressed_fourcc(0, *b"YUY2").unwrap();
    });
    let bytes_without = mux_video(|_mx| {});

    assert!(
        has_three_byte_id(&bytes_with, 0x2E, 0xB5, 0x24),
        "UncompressedFourCC (0x2EB524) must be present when set_video_uncompressed_fourcc was called"
    );
    assert!(
        !has_three_byte_id(&bytes_without, 0x2E, 0xB5, 0x24),
        "UncompressedFourCC (0x2EB524) must NOT be present when set_video_uncompressed_fourcc was not called"
    );
}

#[test]
fn on_disk_payload_is_exactly_four_bytes() {
    // §5.1.4.1.28.15 pins `length: 4`. Locate the element header on
    // disk and confirm the payload sandwiched between the size VINT
    // and the next element is exactly four bytes long.
    let payload: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
    let bytes = mux_video(|mx| {
        mx.set_video_uncompressed_fourcc(0, payload).unwrap();
    });
    // The on-disk sequence is [0x2E, 0xB5, 0x24, 0x84, 0xDE, 0xAD,
    // 0xBE, 0xEF]. Look it up directly to confirm both the size VINT
    // declares 4 and the payload is exactly four bytes wide.
    let needle = [0x2E, 0xB5, 0x24, 0x84, 0xDE, 0xAD, 0xBE, 0xEF];
    assert!(
        bytes.windows(needle.len()).any(|w| w == needle),
        "on-disk element must carry size VINT 0x84 (four-byte length) followed by the exact payload"
    );
}

#[test]
fn last_write_wins() {
    // Calling the setter twice overwrites the previously queued
    // value — the file ends up with only the second FourCC.
    let bytes = mux_video(|mx| {
        mx.set_video_uncompressed_fourcc(0, *b"YUY2").unwrap();
        mx.set_video_uncompressed_fourcc(0, *b"NV12").unwrap();
    });
    let dmx = demux_typed(bytes);
    assert_eq!(
        dmx.video_uncompressed_fourcc(0).and_then(|f| f.fourcc()),
        Some(*b"NV12")
    );
}

#[test]
fn read_back_accessor_returns_queued_value() {
    // Pre-`write_header`, the muxer's own `video_uncompressed_fourcc`
    // accessor surfaces the queued hint. Useful for tests inspecting
    // the muxer state without round-tripping.
    let tmp = tmp_path("accessor");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    assert!(mx.video_uncompressed_fourcc(0).is_none());
    mx.set_video_uncompressed_fourcc(0, *b"AYUV").unwrap();
    assert_eq!(mx.video_uncompressed_fourcc(0), Some(*b"AYUV"));
    let _ = std::fs::remove_file(&tmp);
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
fn rejects_call_after_write_header() {
    let tmp = tmp_path("post_header");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    mx.write_header().unwrap();
    let err = assert_err(
        mx.set_video_uncompressed_fourcc(0, *b"YUY2"),
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
        mx.set_video_uncompressed_fourcc(5, *b"YUY2"),
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
        mx.set_video_uncompressed_fourcc(1, *b"YUY2"),
        "must reject on audio track",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn independent_of_other_video_setters() {
    // Setting `set_video_uncompressed_fourcc` must not affect any
    // other queued Video hint — exercising the geometry setter
    // alongside this one keeps both round-tripping independently.
    use oxideav_mkv::mux::MkvVideoGeometry;
    let bytes = mux_video(|mx| {
        mx.set_video_uncompressed_fourcc(0, *b"YUY2").unwrap();
        mx.set_video_geometry(0, MkvVideoGeometry::cropped(2, 4, 6, 8))
            .unwrap();
    });
    let dmx = demux_typed(bytes);
    assert_eq!(
        dmx.video_uncompressed_fourcc(0).and_then(|f| f.fourcc()),
        Some(*b"YUY2")
    );
    let g = dmx.video_geometry(0).expect("video_geometry surfaced");
    assert_eq!(g.pixel_crop_top(), 2);
    assert_eq!(g.pixel_crop_bottom(), 4);
    assert_eq!(g.pixel_crop_left(), 6);
    assert_eq!(g.pixel_crop_right(), 8);
}
