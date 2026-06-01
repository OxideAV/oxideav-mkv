//! Round-trip tests for the muxer's `Video > FlagInterlaced` (RFC 9559
//! §5.1.4.1.28.1) + `FieldOrder` (§5.1.4.1.28.2) write path.
//!
//! Drives `MkvMuxer::set_video_interlacing` against the public Muxer
//! trait, then re-opens the bytes through the typed
//! [`oxideav_mkv::demux::open_typed`] and confirms
//! `MkvDemuxer::video_interlacing(stream_index)` decodes the exact pair
//! handed to the muxer — including the `Other(u64)` forward-compat
//! variant on both enums.
//!
//! Spec contracts pinned here:
//!
//! 1. Setting `FlagInterlaced::Interlaced` + `Some(FieldOrder::Tff)`
//!    on a video track surfaces back as the same pair.
//! 2. Setting `FlagInterlaced::Progressive` with `None` surfaces back
//!    as `flag=Progressive` and `field_order=None` (the spec rule
//!    "If FlagInterlaced is not set to 1, this element MUST be
//!    ignored" applies on both write and read sides).
//! 3. An `Other(u64)` `FieldOrder` round-trips its wrapped value
//!    verbatim (forward-compat with values registered after RFC 9559).
//! 4. Omitting the call entirely (the default) means neither
//!    `FlagInterlaced` nor `FieldOrder` is written, so the demuxer
//!    materialises the §5.1.4.1.28.1 / §5.1.4.1.28.2 spec defaults
//!    (`Undetermined` + `field_order() == None`).
//! 5. `set_video_interlacing` rejects calls made after `write_header`,
//!    out-of-range stream indices, calls on non-video tracks, and
//!    `FieldOrder` paired with anything other than
//!    `FlagInterlaced::Interlaced`.
//! 6. The on-disk bytes contain the `FlagInterlaced` (`0x9A`) and
//!    `FieldOrder` (`0x9D`) element IDs only when the API was called
//!    with the corresponding values — omitted otherwise.
//!
//! These tests use the production EBML helpers to walk the muxed
//! buffer — no third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, MediaType, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo,
    TimeBase, WriteSeek,
};
use oxideav_mkv::demux::{FieldOrder, FlagInterlaced};
use oxideav_mkv::mux::MkvMuxer;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r208-vinterlace-{}-{}-{n}.mkv",
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
/// stream in to `set_video_interlacing` (or not). Returns the muxed
/// file's bytes plus the path it was written to (so the caller can
/// remove it).
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
/// `MkvDemuxer::video_interlacing`.
fn demux_typed(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open_typed")
}

#[test]
fn roundtrip_tff_interlaced() {
    let bytes = mux_video(|mx| {
        mx.set_video_interlacing(0, FlagInterlaced::Interlaced, Some(FieldOrder::Tff))
            .expect("set_video_interlacing");
    });
    let dmx = demux_typed(bytes);
    let vi = dmx
        .video_interlacing(0)
        .expect("video stream surfaces VideoInterlacing");
    assert_eq!(vi.flag(), FlagInterlaced::Interlaced);
    assert_eq!(vi.field_order(), Some(FieldOrder::Tff));
}

#[test]
fn roundtrip_bff_interlaced() {
    let bytes = mux_video(|mx| {
        mx.set_video_interlacing(0, FlagInterlaced::Interlaced, Some(FieldOrder::Bff))
            .expect("set_video_interlacing");
    });
    let dmx = demux_typed(bytes);
    let vi = dmx.video_interlacing(0).expect("VideoInterlacing");
    assert_eq!(vi.flag(), FlagInterlaced::Interlaced);
    assert_eq!(vi.field_order(), Some(FieldOrder::Bff));
}

#[test]
fn roundtrip_progressive_no_field_order() {
    let bytes = mux_video(|mx| {
        mx.set_video_interlacing(0, FlagInterlaced::Progressive, None)
            .expect("set_video_interlacing");
    });
    let dmx = demux_typed(bytes);
    let vi = dmx.video_interlacing(0).expect("VideoInterlacing");
    assert_eq!(vi.flag(), FlagInterlaced::Progressive);
    // §5.1.4.1.28.2: FieldOrder MUST be ignored when FlagInterlaced != 1.
    // The demuxer's typed surface honours this and returns None.
    assert_eq!(vi.field_order(), None);
}

#[test]
fn roundtrip_interlaced_default_field_order() {
    // Interlaced with no explicit FieldOrder — the muxer writes only
    // FlagInterlaced=1, and the demuxer materialises the
    // §5.1.4.1.28.2 default 2 (Undetermined).
    let bytes = mux_video(|mx| {
        mx.set_video_interlacing(0, FlagInterlaced::Interlaced, None)
            .expect("set_video_interlacing");
    });
    let dmx = demux_typed(bytes);
    let vi = dmx.video_interlacing(0).expect("VideoInterlacing");
    assert_eq!(vi.flag(), FlagInterlaced::Interlaced);
    assert_eq!(vi.field_order(), Some(FieldOrder::Undetermined));
}

#[test]
fn roundtrip_other_field_order_passthrough() {
    // Forward-compat: a value registered after RFC 9559 (anything
    // outside Table 4's {0,1,2,6,9,14}) round-trips its wrapped
    // u64 verbatim through both the writer's `to_raw` and the
    // reader's `from_raw`.
    let bytes = mux_video(|mx| {
        mx.set_video_interlacing(0, FlagInterlaced::Interlaced, Some(FieldOrder::Other(42)))
            .expect("set_video_interlacing");
    });
    let dmx = demux_typed(bytes);
    let vi = dmx.video_interlacing(0).expect("VideoInterlacing");
    assert_eq!(vi.flag(), FlagInterlaced::Interlaced);
    assert_eq!(vi.field_order(), Some(FieldOrder::Other(42)));
}

#[test]
fn roundtrip_other_flag_passthrough() {
    // `FlagInterlaced::Other(v)` also round-trips. Per
    // §5.1.4.1.28.2 the typed surface still treats this as
    // "not Interlaced", so `field_order()` returns None even if
    // a writer hypothetically attempted to pair it with one
    // (rejected at queue time — see `field_order_on_non_interlaced_rejected`).
    let bytes = mux_video(|mx| {
        mx.set_video_interlacing(0, FlagInterlaced::Other(99), None)
            .expect("set_video_interlacing");
    });
    let dmx = demux_typed(bytes);
    let vi = dmx.video_interlacing(0).expect("VideoInterlacing");
    assert_eq!(vi.flag(), FlagInterlaced::Other(99));
    assert_eq!(vi.field_order(), None);
}

#[test]
fn omitted_call_yields_spec_default() {
    // When `set_video_interlacing` is NOT called, the muxer must
    // omit both elements so the demuxer materialises the
    // §5.1.4.1.28.1 / §5.1.4.1.28.2 defaults (`Undetermined`).
    let bytes = mux_video(|_mx| {});
    let dmx = demux_typed(bytes);
    let vi = dmx
        .video_interlacing(0)
        .expect("Video master still present (PixelWidth/PixelHeight)");
    assert_eq!(vi.flag(), FlagInterlaced::Undetermined);
    // FlagInterlaced=Undetermined → FieldOrder semantically meaningless.
    assert_eq!(vi.field_order(), None);
}

#[test]
fn on_disk_bytes_contain_element_ids_only_when_set() {
    // FlagInterlaced id = 0x9A, FieldOrder id = 0x9D.
    let bytes_with = mux_video(|mx| {
        mx.set_video_interlacing(0, FlagInterlaced::Interlaced, Some(FieldOrder::Tff))
            .expect("set_video_interlacing");
    });
    let bytes_without = mux_video(|_mx| {});

    // The default-omission path must not contain either id byte
    // followed by the canonical size+payload shape we write (size
    // VINT `0x81` + 1-byte payload). The naive byte search would
    // false-positive on SimpleBlock payload bytes, so we scan
    // narrowly for `[id, 0x81, value]` triples.
    fn has_element_id_byte(bytes: &[u8], id: u8) -> bool {
        bytes.windows(2).any(|w| w[0] == id && w[1] == 0x81)
    }

    assert!(
        has_element_id_byte(&bytes_with, 0x9A),
        "FlagInterlaced (0x9A) must be present when set_video_interlacing was called"
    );
    assert!(
        has_element_id_byte(&bytes_with, 0x9D),
        "FieldOrder (0x9D) must be present when paired with Interlaced"
    );
    assert!(
        !has_element_id_byte(&bytes_without, 0x9A),
        "FlagInterlaced (0x9A) must NOT be present when set_video_interlacing was not called"
    );
    assert!(
        !has_element_id_byte(&bytes_without, 0x9D),
        "FieldOrder (0x9D) must NOT be present when set_video_interlacing was not called"
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
fn rejects_call_after_write_header() {
    // The Video master sits inside Tracks, which is written exactly
    // once at write_header time. set_video_interlacing must reject
    // any post-header call so callers see the misuse synchronously.
    let tmp = tmp_path("post_header");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    mx.write_header().unwrap();
    let err = assert_err(
        mx.set_video_interlacing(0, FlagInterlaced::Interlaced, Some(FieldOrder::Tff)),
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
        mx.set_video_interlacing(5, FlagInterlaced::Interlaced, Some(FieldOrder::Tff)),
        "must reject out-of-range index",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_call_on_audio_track() {
    // The audio stream sits at index 1; calling
    // set_video_interlacing on it must be rejected — non-video
    // tracks have no `Video` master on disk.
    let tmp = tmp_path("audio");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(), audio_stream(1)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).unwrap();
    // Sanity: the second stream is genuinely audio.
    assert_eq!(streams[1].params.media_type, MediaType::Audio);
    let err = assert_err(
        mx.set_video_interlacing(1, FlagInterlaced::Interlaced, Some(FieldOrder::Tff)),
        "must reject on audio track",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn field_order_on_non_interlaced_rejected() {
    // §5.1.4.1.28.2: "If FlagInterlaced is not set to 1, this element
    // MUST be ignored". Writing FieldOrder paired with anything
    // other than FlagInterlaced::Interlaced would be a no-op on every
    // conforming reader, so the muxer surfaces the spec violation up
    // front instead of silently dropping it.
    let tmp = tmp_path("noninterlaced");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    let err = assert_err(
        mx.set_video_interlacing(0, FlagInterlaced::Progressive, Some(FieldOrder::Tff)),
        "FieldOrder on Progressive must be rejected",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    // Same for FlagInterlaced::Undetermined.
    let err = assert_err(
        mx.set_video_interlacing(0, FlagInterlaced::Undetermined, Some(FieldOrder::Tff)),
        "FieldOrder on Undetermined must be rejected",
    );
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn second_call_overwrites_first() {
    // set_video_interlacing is idempotent in the sense that the
    // last-write-wins — useful when callers conditionally derive a
    // pair from upstream metadata that arrives in two passes.
    let bytes = mux_video(|mx| {
        mx.set_video_interlacing(0, FlagInterlaced::Progressive, None)
            .expect("first call");
        mx.set_video_interlacing(0, FlagInterlaced::Interlaced, Some(FieldOrder::Bff))
            .expect("second call");
    });
    let dmx = demux_typed(bytes);
    let vi = dmx.video_interlacing(0).expect("VideoInterlacing");
    assert_eq!(vi.flag(), FlagInterlaced::Interlaced);
    assert_eq!(vi.field_order(), Some(FieldOrder::Bff));
}

#[test]
fn video_interlacing_accessor_reflects_queued_value() {
    let tmp = tmp_path("accessor");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    assert_eq!(mx.video_interlacing(0), None, "starts unset");
    mx.set_video_interlacing(
        0,
        FlagInterlaced::Interlaced,
        Some(FieldOrder::TffInterleaved),
    )
    .unwrap();
    assert_eq!(
        mx.video_interlacing(0),
        Some((FlagInterlaced::Interlaced, Some(FieldOrder::TffInterleaved)))
    );
    let _ = std::fs::remove_file(&tmp);
}
