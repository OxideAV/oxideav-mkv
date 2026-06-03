//! Round-trip tests for the muxer's `Video > PixelCrop{Top,Bottom,Left,
//! Right}` (RFC 9559 §5.1.4.1.28.8..§5.1.4.1.28.11) + `DisplayWidth`
//! (§5.1.4.1.28.12) / `DisplayHeight` (§5.1.4.1.28.13) / `DisplayUnit`
//! (§5.1.4.1.28.14) write path.
//!
//! Drives `MkvMuxer::set_video_geometry` against the public `Muxer` trait,
//! then re-opens the bytes through [`oxideav_mkv::demux::open_typed`] and
//! confirms `MkvDemuxer::video_geometry(stream_index)` decodes the
//! quartet back exactly as fed in — including the spec-derived display
//! dimension defaults when only the crops were set, the
//! [`DisplayUnit::Other`] forward-compat round-trip per §27.9, and the
//! aspect-ratio shape from RFC 9559 §11.1.
//!
//! Spec contracts pinned here:
//!
//! 1. Pillar-box-style `cropped(0, 0, 240, 240)` round-trips: the four
//!    `PixelCropLeft` / `PixelCropRight` (and zero top / bottom) plus the
//!    demuxer's derived `DisplayWidth` from `PixelWidth - cropLeft -
//!    cropRight` per §5.1.4.1.28.12 default rule. Zero crops are not
//!    written explicitly.
//! 2. Setting `display_unit = DisplayUnit::DisplayAspectRatio` plus an
//!    explicit `display_width` / `display_height` (the "aspect ratio
//!    override" shape) round-trips: the demuxer surfaces the non-Pixels
//!    DisplayUnit and the two numerator/denominator-style integers.
//! 3. Omitting `set_video_geometry` entirely results in the demuxer
//!    materialising the spec defaults (all crops `0`,
//!    `DisplayUnit::Pixels`, display dimensions derived from the
//!    encoded PixelWidth/Height).
//! 4. `DisplayUnit::Other(42)` round-trips its wrapped value verbatim
//!    via the §27.9 forward-compat path.
//! 5. The setter rejects calls made after `write_header`, out-of-range
//!    stream indices, calls on non-video tracks, and `Some(0)` for
//!    either `display_width` / `display_height` per the `range: not 0`
//!    pin on §5.1.4.1.28.12 / .13.
//! 6. The on-disk bytes contain the `PixelCropLeft` (`0x54CC`) and
//!    `PixelCropRight` (`0x54DD`) element IDs in the pillar-box case;
//!    they are *absent* when no crops were set.
//!
//! These tests use the production EBML helpers to walk the muxed
//! buffer — no third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, MediaType, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo,
    TimeBase, WriteSeek,
};
use oxideav_mkv::demux::DisplayUnit;
use oxideav_mkv::mux::{MkvMuxer, MkvVideoGeometry};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r219-vgeo-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

/// 1920x1080 video stream — matches the pillar-box worked example from
/// RFC 9559 §11.1.
fn video_stream_1920x1080() -> StreamInfo {
    let mut p = CodecParameters::video(CodecId::new("vp9"));
    p.width = Some(1920);
    p.height = Some(1080);
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
/// stream in to `set_video_geometry` (or not). Returns the muxed bytes.
fn mux_video<F>(configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let stream = video_stream_1920x1080();
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

#[test]
fn pillar_box_left_right_crops_roundtrip() {
    // RFC 9559 §11.1: a 1440x1080 image padded to 1920x1080 with
    // 240px left + 240px right crops. The demuxer materialises the
    // §5.1.4.1.28.12 default display width as
    // `PixelWidth(1920) - cropLeft(240) - cropRight(240) = 1440`,
    // and the height default as `PixelHeight - 0 - 0 = 1080`.
    let bytes = mux_video(|mx| {
        mx.set_video_geometry(0, MkvVideoGeometry::cropped(0, 0, 240, 240))
            .expect("set_video_geometry");
    });
    // Both PixelCropLeft (0x54CC) and PixelCropRight (0x54DD) must
    // appear on disk; the two zero-axis crops (Top 0x54BB / Bottom
    // 0x54AA) must NOT appear (omitted = spec default 0).
    assert!(contains_id(&bytes, 0x54CC), "PixelCropLeft missing on disk");
    assert!(
        contains_id(&bytes, 0x54DD),
        "PixelCropRight missing on disk"
    );
    assert!(
        !contains_id(&bytes, 0x54BB),
        "PixelCropTop should be omitted (zero crop)"
    );
    assert!(
        !contains_id(&bytes, 0x54AA),
        "PixelCropBottom should be omitted (zero crop)"
    );

    let dmx = demux_typed(bytes);
    let g = dmx.video_geometry(0).expect("VideoGeometry");
    assert_eq!(g.pixel_crop_top(), 0);
    assert_eq!(g.pixel_crop_bottom(), 0);
    assert_eq!(g.pixel_crop_left(), 240);
    assert_eq!(g.pixel_crop_right(), 240);
    // §5.1.4.1.28.12 derived default: 1920 - 240 - 240 = 1440.
    assert_eq!(g.display_width(), Some(1440));
    // §5.1.4.1.28.13 derived default: 1080 - 0 - 0 = 1080.
    assert_eq!(g.display_height(), Some(1080));
    assert_eq!(g.display_unit(), DisplayUnit::Pixels);
}

#[test]
fn four_axis_crops_roundtrip() {
    // Explicit non-zero crops on all four axes — confirms every element
    // ID is written and parsed back faithfully, and the demuxer's
    // derived display dimensions use the four-axis formula.
    let bytes = mux_video(|mx| {
        mx.set_video_geometry(0, MkvVideoGeometry::cropped(8, 16, 32, 64))
            .expect("set_video_geometry");
    });
    let dmx = demux_typed(bytes);
    let g = dmx.video_geometry(0).expect("VideoGeometry");
    assert_eq!(g.pixel_crop_top(), 8);
    assert_eq!(g.pixel_crop_bottom(), 16);
    assert_eq!(g.pixel_crop_left(), 32);
    assert_eq!(g.pixel_crop_right(), 64);
    // 1920 - 32 - 64 = 1824; 1080 - 8 - 16 = 1056.
    assert_eq!(g.display_width(), Some(1824));
    assert_eq!(g.display_height(), Some(1056));
    assert_eq!(g.display_unit(), DisplayUnit::Pixels);
}

#[test]
fn aspect_ratio_override_roundtrip() {
    // RFC 9559 §5.1.4.1.28.14 Table 10 value `3` (DisplayAspectRatio):
    // DisplayWidth / DisplayHeight encode an aspect ratio rather than a
    // physical size. Worked here as 16:9.
    let bytes = mux_video(|mx| {
        mx.set_video_geometry(0, MkvVideoGeometry::aspect_ratio(16, 9))
            .expect("set_video_geometry");
    });
    // DisplayUnit (0x54B2) must appear on disk for any non-Pixels value.
    assert!(
        contains_id(&bytes, 0x54B2),
        "DisplayUnit missing for non-Pixels variant"
    );
    let dmx = demux_typed(bytes);
    let g = dmx.video_geometry(0).expect("VideoGeometry");
    assert_eq!(g.pixel_crop_top(), 0);
    assert_eq!(g.pixel_crop_left(), 0);
    assert_eq!(g.display_width(), Some(16));
    assert_eq!(g.display_height(), Some(9));
    assert_eq!(g.display_unit(), DisplayUnit::DisplayAspectRatio);
}

#[test]
fn omitted_geometry_yields_spec_defaults() {
    // No `set_video_geometry` call: every geometry element is left off
    // disk; the demuxer materialises §5.1.4.1.28.8..14 defaults — zero
    // crops, derived display dimensions from PixelWidth/Height (because
    // DisplayUnit defaults to Pixels), DisplayUnit::Pixels itself.
    let bytes = mux_video(|_| {});
    // None of the seven geometry element IDs should appear.
    for id in [0x54AAu32, 0x54BB, 0x54CC, 0x54DD, 0x54B0, 0x54BA, 0x54B2] {
        assert!(
            !contains_id(&bytes, id),
            "geometry element 0x{id:04X} unexpectedly present on disk for omitted-call case",
        );
    }
    let dmx = demux_typed(bytes);
    let g = dmx.video_geometry(0).expect("VideoGeometry");
    assert_eq!(g.pixel_crop_top(), 0);
    assert_eq!(g.pixel_crop_bottom(), 0);
    assert_eq!(g.pixel_crop_left(), 0);
    assert_eq!(g.pixel_crop_right(), 0);
    assert_eq!(g.display_width(), Some(1920));
    assert_eq!(g.display_height(), Some(1080));
    assert_eq!(g.display_unit(), DisplayUnit::Pixels);
}

#[test]
fn display_unit_other_forward_compat_roundtrip() {
    // §27.9 leaves the "Matroska Display Units" registry open. Setting
    // a never-registered value with DisplayUnit::Other(v) must
    // round-trip verbatim.
    let bytes = mux_video(|mx| {
        let g = MkvVideoGeometry {
            crop_top: 0,
            crop_bottom: 0,
            crop_left: 0,
            crop_right: 0,
            display_width: Some(42),
            display_height: Some(7),
            display_unit: DisplayUnit::Other(42),
        };
        mx.set_video_geometry(0, g).expect("set_video_geometry");
    });
    let dmx = demux_typed(bytes);
    let g = dmx.video_geometry(0).expect("VideoGeometry");
    assert_eq!(g.display_width(), Some(42));
    assert_eq!(g.display_height(), Some(7));
    assert_eq!(g.display_unit(), DisplayUnit::Other(42));
}

#[test]
fn display_unit_centimeters_roundtrip() {
    // DisplayUnit::Centimeters (1) — non-Pixels variant, so the
    // demuxer does not derive defaults for the display dimensions when
    // they are absent. We supply explicit values here.
    let bytes = mux_video(|mx| {
        let g = MkvVideoGeometry {
            crop_top: 0,
            crop_bottom: 0,
            crop_left: 0,
            crop_right: 0,
            display_width: Some(40),
            display_height: Some(22),
            display_unit: DisplayUnit::Centimeters,
        };
        mx.set_video_geometry(0, g).expect("set_video_geometry");
    });
    let dmx = demux_typed(bytes);
    let g = dmx.video_geometry(0).expect("VideoGeometry");
    assert_eq!(g.display_width(), Some(40));
    assert_eq!(g.display_height(), Some(22));
    assert_eq!(g.display_unit(), DisplayUnit::Centimeters);
}

#[test]
fn aspect_ratio_unit_with_absent_dimensions_yields_none() {
    // §5.1.4.1.28.12 / .13: when DisplayUnit is non-Pixels and the
    // dimension element is absent, the spec says "there is no default
    // value" and the demuxer surfaces `None`. Confirms our writer can
    // emit just DisplayUnit (no DisplayWidth / DisplayHeight) — the
    // shape someone configuring DAR without explicit dimensions would
    // use, even though it is rarely useful in practice.
    let bytes = mux_video(|mx| {
        let g = MkvVideoGeometry {
            crop_top: 0,
            crop_bottom: 0,
            crop_left: 0,
            crop_right: 0,
            display_width: None,
            display_height: None,
            display_unit: DisplayUnit::DisplayAspectRatio,
        };
        mx.set_video_geometry(0, g).expect("set_video_geometry");
    });
    let dmx = demux_typed(bytes);
    let g = dmx.video_geometry(0).expect("VideoGeometry");
    assert_eq!(g.display_width(), None);
    assert_eq!(g.display_height(), None);
    assert_eq!(g.display_unit(), DisplayUnit::DisplayAspectRatio);
}

#[test]
fn set_after_write_header_rejected() {
    let tmp = tmp_path("after-header");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream_1920x1080()]).expect("muxer construct");
    mx.write_header().expect("write_header");
    let res = mx.set_video_geometry(0, MkvVideoGeometry::cropped(0, 0, 240, 240));
    let Err(err) = res else {
        panic!("expected set_video_geometry after write_header to be rejected");
    };
    assert!(matches!(err, Error::Other(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn stream_index_out_of_range_rejected() {
    let tmp = tmp_path("oob");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream_1920x1080()]).expect("muxer construct");
    let res = mx.set_video_geometry(7, MkvVideoGeometry::cropped(0, 0, 240, 240));
    let Err(err) = res else {
        panic!("expected set_video_geometry with out-of-range index to be rejected");
    };
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn non_video_stream_rejected() {
    let tmp = tmp_path("non-video");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    // Mixed video + audio; audio stream is index 1.
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream_1920x1080(), audio_stream(1)])
        .expect("muxer construct");
    let res = mx.set_video_geometry(1, MkvVideoGeometry::cropped(0, 0, 240, 240));
    let Err(err) = res else {
        panic!("expected set_video_geometry on non-video stream to be rejected");
    };
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
    // Sanity: the audio track really is non-video.
    assert_ne!(audio_stream(1).params.media_type, MediaType::Video);
}

#[test]
fn display_width_zero_rejected() {
    let tmp = tmp_path("dw-zero");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream_1920x1080()]).expect("muxer construct");
    let g = MkvVideoGeometry {
        crop_top: 0,
        crop_bottom: 0,
        crop_left: 0,
        crop_right: 0,
        display_width: Some(0),
        display_height: Some(1080),
        display_unit: DisplayUnit::Pixels,
    };
    let res = mx.set_video_geometry(0, g);
    let Err(err) = res else {
        panic!("expected display_width Some(0) to be rejected");
    };
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn display_height_zero_rejected() {
    let tmp = tmp_path("dh-zero");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream_1920x1080()]).expect("muxer construct");
    let g = MkvVideoGeometry {
        crop_top: 0,
        crop_bottom: 0,
        crop_left: 0,
        crop_right: 0,
        display_width: Some(1920),
        display_height: Some(0),
        display_unit: DisplayUnit::Pixels,
    };
    let res = mx.set_video_geometry(0, g);
    let Err(err) = res else {
        panic!("expected display_height Some(0) to be rejected");
    };
    assert!(matches!(err, Error::InvalidData(_)));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn overwrite_previous_call() {
    // A second call replaces the first; only the second set of values
    // reaches the on-disk Video master.
    let bytes = mux_video(|mx| {
        mx.set_video_geometry(0, MkvVideoGeometry::cropped(99, 99, 99, 99))
            .expect("set_video_geometry first");
        mx.set_video_geometry(0, MkvVideoGeometry::cropped(0, 0, 240, 240))
            .expect("set_video_geometry second");
    });
    let dmx = demux_typed(bytes);
    let g = dmx.video_geometry(0).expect("VideoGeometry");
    assert_eq!(g.pixel_crop_top(), 0);
    assert_eq!(g.pixel_crop_bottom(), 0);
    assert_eq!(g.pixel_crop_left(), 240);
    assert_eq!(g.pixel_crop_right(), 240);
}

#[test]
fn video_geometry_accessor_returns_queued_hint() {
    let tmp = tmp_path("accessor");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream_1920x1080()]).expect("muxer construct");
    assert_eq!(mx.video_geometry(0), None);
    let g = MkvVideoGeometry::cropped(0, 0, 240, 240);
    mx.set_video_geometry(0, g).expect("set_video_geometry");
    assert_eq!(mx.video_geometry(0), Some(g));
    // Out-of-range stream index returns None rather than panicking.
    assert_eq!(mx.video_geometry(99), None);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn display_unit_pixels_default_keeps_element_off_disk() {
    // Setting DisplayUnit::Pixels (the spec default) explicitly should
    // still keep the element off-disk — re-muxing a file that did not
    // carry an explicit DisplayUnit stays byte-faithful to the common
    // case. The demuxer materialises the default regardless.
    let bytes = mux_video(|mx| {
        let g = MkvVideoGeometry {
            crop_top: 0,
            crop_bottom: 0,
            crop_left: 0,
            crop_right: 0,
            display_width: Some(1280),
            display_height: Some(720),
            display_unit: DisplayUnit::Pixels,
        };
        mx.set_video_geometry(0, g).expect("set_video_geometry");
    });
    // DisplayUnit (0x54B2) is omitted for the Pixels default.
    assert!(
        !contains_id(&bytes, 0x54B2),
        "DisplayUnit should be omitted when set to Pixels (spec default)"
    );
    // But DisplayWidth + DisplayHeight should appear.
    assert!(contains_id(&bytes, 0x54B0), "DisplayWidth missing on disk");
    assert!(contains_id(&bytes, 0x54BA), "DisplayHeight missing on disk");
    let dmx = demux_typed(bytes);
    let g = dmx.video_geometry(0).expect("VideoGeometry");
    assert_eq!(g.display_width(), Some(1280));
    assert_eq!(g.display_height(), Some(720));
    assert_eq!(g.display_unit(), DisplayUnit::Pixels);
}

/// Scan `bytes` for the EBML-encoded element id `id`. EBML 4-byte IDs
/// are written as 4 raw big-endian bytes with the VINT length marker
/// bit set in the leading octet (`0x10` for 4-byte ids). `0x54xx` ids
/// fit in 2 bytes (leading bit `0x40` set). The two `0x54xx` IDs we
/// look for here (`PixelCrop*`, `Display*`) all encode as the literal
/// big-endian byte pair the spec table lists — no extra bit munging
/// needed beyond what `write_element_id` already does. We search for
/// the raw on-disk encoding the muxer would have written.
fn contains_id(bytes: &[u8], id: u32) -> bool {
    let encoded = oxideav_mkv::ebml::write_element_id(id);
    bytes
        .windows(encoded.len())
        .any(|w| w == encoded.as_slice())
}
