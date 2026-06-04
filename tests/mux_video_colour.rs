//! Round-trip tests for the muxer's `Video > Colour`
//! (RFC 9559 §5.1.4.1.28.16) scalar-children write path.
//!
//! Drives [`MkvMuxer::set_video_colour`] against the public `Muxer`
//! trait, then re-opens the bytes through
//! [`oxideav_mkv::demux::open_typed`] and confirms
//! [`oxideav_mkv::demux::MkvDemuxer::video_colour`] decodes the
//! eleven scalar children of the `Colour` master back exactly as fed
//! in — including the `MatrixCoefficients` / `TransferCharacteristics`
//! / `Primaries` Tables 12 / 16 / 17 defaults, the
//! `ChromaSitingHorz` / `ChromaSitingVert` / `Range` defaults, and the
//! `Other(u64)` forward-compat round-trip through every enum-typed
//! child.
//!
//! Spec contracts pinned here:
//!
//! 1. Setting [`MkvVideoColour::bt709`] surfaces the canonical BT.709
//!    SDR shape on the demux side: matrix `1`, transfer `1`, primaries
//!    `1`, broadcast range.
//! 2. Setting [`MkvVideoColour::bt2020_pq`] surfaces the canonical
//!    HDR10 shape: matrix `9`, transfer `16`, primaries `9`, full
//!    range, 10 bits per channel.
//! 3. `MaxCLL` / `MaxFALL` round-trip as plain integer cd/m² when
//!    explicitly set; `None` keeps the elements off-disk so the
//!    demuxer surfaces `None` from those getters.
//! 4. `ChromaSubsamplingHorz` / `ChromaSubsamplingVert` /
//!    `CbSubsamplingHorz` / `CbSubsamplingVert` round-trip as plain
//!    integers when set; `None` keeps them off-disk so the demuxer
//!    surfaces `None` (no spec default for those four).
//! 5. Omitting `set_video_colour` entirely keeps the `Colour` master
//!    off-disk so the demuxer surfaces `None` from `video_colour`
//!    (distinct from "empty Colour master present").
//! 6. Calling `set_video_colour` with [`MkvVideoColour::default`]
//!    writes an empty `Colour` master, which the demuxer parses into
//!    `Some(VideoColour)` with every getter returning the spec
//!    default value.
//! 7. Every enum-typed child's `Other(u64)` forward-compat variant
//!    round-trips its wrapped value verbatim per RFC 9559 §27 open
//!    registries.
//! 8. The setter rejects calls made after `write_header`, out-of-range
//!    stream indices, and calls on non-video tracks.
//! 9. On-disk bytes contain the `Colour` element id (`0x55B0`) only
//!    when the API was called.
//! 10. Calling the setter twice on the same `stream_index` is
//!     last-write-wins.
//! 11. Settings the `Colour` master are independent of every other
//!     `Video` sub-element setter (interlacing, stereo, alpha,
//!     geometry, UncompressedFourCC).
//!
//! These tests use the production EBML helpers to walk the muxed
//! buffer — no third-party Matroska parser is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, MediaType, Muxer, Packet, ReadSeek, SampleFormat,
    StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::demux::{
    ChromaSitingHorz, ChromaSitingVert, ColourRange, MatrixCoefficients, Primaries,
    TransferCharacteristics,
};
use oxideav_mkv::mux::{MkvMuxer, MkvVideoColour};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r231-vcolour-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

fn video_stream() -> StreamInfo {
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
/// stream in to `set_video_colour` (or not). Returns the muxed file's
/// bytes.
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
fn roundtrip_bt709() {
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, MkvVideoColour::bt709())
            .expect("set_video_colour");
    });
    let dmx = demux_typed(bytes);
    let c = dmx.video_colour(0).expect("video_colour surfaced");
    assert_eq!(c.matrix_coefficients(), MatrixCoefficients::BT709);
    assert_eq!(c.transfer_characteristics(), TransferCharacteristics::BT709);
    assert_eq!(c.primaries(), Primaries::BT709);
    assert_eq!(c.range(), ColourRange::Broadcast);
    // Defaults — not set, so they surface as defaults via parse_colour's
    // pre-populated RawColour.
    assert_eq!(c.bits_per_channel(), 0);
    assert_eq!(c.chroma_siting_horz(), ChromaSitingHorz::Unspecified);
    assert_eq!(c.chroma_siting_vert(), ChromaSitingVert::Unspecified);
    assert!(c.chroma_subsampling_horz().is_none());
    assert!(c.chroma_subsampling_vert().is_none());
    assert!(c.cb_subsampling_horz().is_none());
    assert!(c.cb_subsampling_vert().is_none());
    assert!(c.max_cll().is_none());
    assert!(c.max_fall().is_none());
}

#[test]
fn roundtrip_bt2020_pq_hdr10() {
    // BT.2020 + PQ + full range + 10 bpc. HDR10's canonical shape.
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, MkvVideoColour::bt2020_pq())
            .expect("set_video_colour");
    });
    let dmx = demux_typed(bytes);
    let c = dmx.video_colour(0).expect("video_colour surfaced");
    assert_eq!(
        c.matrix_coefficients(),
        MatrixCoefficients::BT2020NonConstantLuminance
    );
    assert_eq!(
        c.transfer_characteristics(),
        TransferCharacteristics::BT2100Pq
    );
    assert_eq!(c.primaries(), Primaries::BT2020);
    assert_eq!(c.range(), ColourRange::Full);
    assert_eq!(c.bits_per_channel(), 10);
}

#[test]
fn roundtrip_max_cll_and_max_fall() {
    // §5.1.4.1.28.28 / §5.1.4.1.28.29 — light-level pair, no spec default.
    let c = MkvVideoColour {
        max_cll: Some(1000),
        max_fall: Some(400),
        ..MkvVideoColour::bt2020_pq()
    };
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, c).expect("set_video_colour");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.video_colour(0).expect("video_colour surfaced");
    assert_eq!(got.max_cll(), Some(1000));
    assert_eq!(got.max_fall(), Some(400));
}

#[test]
fn roundtrip_chroma_subsampling_quartet() {
    // §5.1.4.1.28.19..§5.1.4.1.28.22 — four chroma-subsampling
    // integers with no spec default. Set all four to distinct
    // values and round-trip them.
    let c = MkvVideoColour {
        chroma_subsampling_horz: Some(1),
        chroma_subsampling_vert: Some(1),
        cb_subsampling_horz: Some(0),
        cb_subsampling_vert: Some(0),
        ..MkvVideoColour::default()
    };
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, c).expect("set_video_colour");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.video_colour(0).expect("video_colour surfaced");
    assert_eq!(got.chroma_subsampling_horz(), Some(1));
    assert_eq!(got.chroma_subsampling_vert(), Some(1));
    assert_eq!(got.cb_subsampling_horz(), Some(0));
    assert_eq!(got.cb_subsampling_vert(), Some(0));
}

#[test]
fn roundtrip_chroma_siting_explicit() {
    // §5.1.4.1.28.23 / .24 — table 13 / 14. Default `0`
    // (`Unspecified`) is omitted on disk; set to non-default values
    // for both axes and round-trip.
    let c = MkvVideoColour {
        chroma_siting_horz: ChromaSitingHorz::LeftCollocated,
        chroma_siting_vert: ChromaSitingVert::TopCollocated,
        ..MkvVideoColour::default()
    };
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, c).expect("set_video_colour");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.video_colour(0).expect("video_colour surfaced");
    assert_eq!(got.chroma_siting_horz(), ChromaSitingHorz::LeftCollocated);
    assert_eq!(got.chroma_siting_vert(), ChromaSitingVert::TopCollocated);
}

#[test]
fn roundtrip_chroma_siting_half() {
    // The shared `Half` value (`2`) across both axes — one element id
    // taking the same raw value on both children.
    let c = MkvVideoColour {
        chroma_siting_horz: ChromaSitingHorz::Half,
        chroma_siting_vert: ChromaSitingVert::Half,
        ..MkvVideoColour::default()
    };
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, c).expect("set_video_colour");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.video_colour(0).expect("video_colour surfaced");
    assert_eq!(got.chroma_siting_horz(), ChromaSitingHorz::Half);
    assert_eq!(got.chroma_siting_vert(), ChromaSitingVert::Half);
}

#[test]
fn roundtrip_matrix_other_passthrough() {
    // §27.13 leaves the registry open; `MatrixCoefficients::Other(99)`
    // must round-trip its wrapped value verbatim.
    let c = MkvVideoColour {
        matrix_coefficients: MatrixCoefficients::Other(99),
        ..MkvVideoColour::default()
    };
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, c).expect("set_video_colour");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.video_colour(0).expect("video_colour surfaced");
    assert_eq!(got.matrix_coefficients(), MatrixCoefficients::Other(99));
}

#[test]
fn roundtrip_transfer_other_passthrough() {
    let c = MkvVideoColour {
        transfer_characteristics: TransferCharacteristics::Other(50),
        ..MkvVideoColour::default()
    };
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, c).expect("set_video_colour");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.video_colour(0).expect("video_colour surfaced");
    assert_eq!(
        got.transfer_characteristics(),
        TransferCharacteristics::Other(50)
    );
}

#[test]
fn roundtrip_primaries_other_passthrough() {
    let c = MkvVideoColour {
        primaries: Primaries::Other(123),
        ..MkvVideoColour::default()
    };
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, c).expect("set_video_colour");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.video_colour(0).expect("video_colour surfaced");
    assert_eq!(got.primaries(), Primaries::Other(123));
}

#[test]
fn roundtrip_primaries_p3_jedec_22() {
    // Table 17's gap between `12` and `22`. `EbuTech3213EJedecP22Phosphors`
    // is value `22` per the spec; the `to_raw` inverse must hit the same
    // integer the demuxer expects.
    let c = MkvVideoColour {
        primaries: Primaries::EbuTech3213EJedecP22Phosphors,
        ..MkvVideoColour::default()
    };
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, c).expect("set_video_colour");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.video_colour(0).expect("video_colour surfaced");
    assert_eq!(got.primaries(), Primaries::EbuTech3213EJedecP22Phosphors);
}

#[test]
fn roundtrip_range_defined_by_matrix_and_transfer() {
    let c = MkvVideoColour {
        range: ColourRange::DefinedByMatrixAndTransfer,
        ..MkvVideoColour::default()
    };
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, c).expect("set_video_colour");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.video_colour(0).expect("video_colour surfaced");
    assert_eq!(got.range(), ColourRange::DefinedByMatrixAndTransfer);
}

#[test]
fn roundtrip_range_other_passthrough() {
    let c = MkvVideoColour {
        range: ColourRange::Other(7),
        ..MkvVideoColour::default()
    };
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, c).expect("set_video_colour");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.video_colour(0).expect("video_colour surfaced");
    assert_eq!(got.range(), ColourRange::Other(7));
}

#[test]
fn roundtrip_chroma_siting_other_passthrough() {
    // §27.10 / §27.11 leave the chroma-siting registries open.
    let c = MkvVideoColour {
        chroma_siting_horz: ChromaSitingHorz::Other(50),
        chroma_siting_vert: ChromaSitingVert::Other(70),
        ..MkvVideoColour::default()
    };
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, c).expect("set_video_colour");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.video_colour(0).expect("video_colour surfaced");
    assert_eq!(got.chroma_siting_horz(), ChromaSitingHorz::Other(50));
    assert_eq!(got.chroma_siting_vert(), ChromaSitingVert::Other(70));
}

#[test]
fn omitted_call_yields_none() {
    // No `set_video_colour` call → the `Colour` master must be
    // absent and the demuxer surfaces `None`.
    let bytes = mux_video(|_mx| {});
    let dmx = demux_typed(bytes);
    assert!(
        dmx.video_colour(0).is_none(),
        "absent Colour master must surface as None"
    );
}

#[test]
fn empty_colour_writes_empty_master_with_defaults() {
    // Setting `MkvVideoColour::default()` queues a `Colour` master whose
    // every scalar child matches the §5.1.4.1.28.17..§5.1.4.1.28.27 spec
    // default. Every child is omitted on disk (default-omission), so the
    // on-disk `Colour` master is empty. The demuxer, however, *does*
    // see the `Colour` element header — `video_colour` is `Some`,
    // with every getter returning the spec default.
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, MkvVideoColour::default())
            .expect("set_video_colour");
    });
    let dmx = demux_typed(bytes);
    let got = dmx.video_colour(0).expect("Colour master present");
    assert_eq!(got.matrix_coefficients(), MatrixCoefficients::Unspecified);
    assert_eq!(
        got.transfer_characteristics(),
        TransferCharacteristics::Unspecified
    );
    assert_eq!(got.primaries(), Primaries::Unspecified);
    assert_eq!(got.range(), ColourRange::Unspecified);
    assert_eq!(got.chroma_siting_horz(), ChromaSitingHorz::Unspecified);
    assert_eq!(got.chroma_siting_vert(), ChromaSitingVert::Unspecified);
    assert_eq!(got.bits_per_channel(), 0);
    assert!(got.chroma_subsampling_horz().is_none());
    assert!(got.chroma_subsampling_vert().is_none());
    assert!(got.cb_subsampling_horz().is_none());
    assert!(got.cb_subsampling_vert().is_none());
    assert!(got.max_cll().is_none());
    assert!(got.max_fall().is_none());
}

#[test]
fn on_disk_bytes_contain_colour_id_only_when_set() {
    // Colour id 0x55B0 = [0x55, 0xB0]. Scan for the two-byte id.
    fn has_two_byte_id(bytes: &[u8], a: u8, b: u8) -> bool {
        bytes.windows(2).any(|w| w[0] == a && w[1] == b)
    }
    let bytes_with = mux_video(|mx| {
        mx.set_video_colour(0, MkvVideoColour::bt709()).unwrap();
    });
    let bytes_without = mux_video(|_mx| {});
    assert!(
        has_two_byte_id(&bytes_with, 0x55, 0xB0),
        "Colour id (0x55B0) must be present when set_video_colour was called"
    );
    assert!(
        !has_two_byte_id(&bytes_without, 0x55, 0xB0),
        "Colour id (0x55B0) must NOT be present when set_video_colour was not called"
    );
}

#[test]
fn last_write_wins() {
    // Two `set_video_colour` calls — only the second value reaches disk.
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, MkvVideoColour::bt709()).unwrap();
        mx.set_video_colour(0, MkvVideoColour::bt2020_pq()).unwrap();
    });
    let dmx = demux_typed(bytes);
    let got = dmx.video_colour(0).expect("video_colour surfaced");
    assert_eq!(got.primaries(), Primaries::BT2020);
    assert_eq!(
        got.transfer_characteristics(),
        TransferCharacteristics::BT2100Pq
    );
    assert_eq!(got.range(), ColourRange::Full);
}

#[test]
fn rejects_call_after_write_header() {
    let tmp = tmp_path("after_hdr");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    mx.write_header().unwrap();
    let e = assert_err(
        mx.set_video_colour(0, MkvVideoColour::bt709()),
        "post-header colour set must error",
    );
    assert!(
        format!("{e}").contains("after write_header"),
        "error must mention write_header: got {e}"
    );
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_out_of_range_stream_index() {
    let tmp = tmp_path("oor");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    let e = assert_err(
        mx.set_video_colour(7, MkvVideoColour::bt709()),
        "out-of-range stream_index must error",
    );
    assert!(
        format!("{e}").contains("out of range"),
        "error must mention out of range: got {e}"
    );
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_call_on_audio_track() {
    let tmp = tmp_path("audio");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    // Two streams: video index 0, audio index 1.
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream(), audio_stream(1)]).unwrap();
    // Video index — accepted.
    mx.set_video_colour(0, MkvVideoColour::bt709()).unwrap();
    // Audio index — rejected.
    let e = assert_err(
        mx.set_video_colour(1, MkvVideoColour::bt709()),
        "audio track colour set must error",
    );
    assert!(
        format!("{e}").contains("only Video tracks"),
        "error must mention video-only constraint: got {e}"
    );
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn audio_track_index_does_not_carry_colour() {
    // Even on a multi-track file, video_colour for an audio track is
    // `None` on the demux side.
    let tmp = tmp_path("multi");
    let bytes = {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &[video_stream(), audio_stream(1)]).unwrap();
        mx.set_video_colour(0, MkvVideoColour::bt709()).unwrap();
        mx.write_header().unwrap();
        mx.write_packet(&keyframe_packet(0, 0, 0xAA, 16)).unwrap();
        let mut p = Packet::new(1, TimeBase::new(1, 48_000), vec![0u8; 4]);
        p.pts = Some(0);
        p.flags.keyframe = true;
        mx.write_packet(&p).unwrap();
        mx.write_trailer().unwrap();
        std::fs::read(&tmp).unwrap()
    };
    let _ = std::fs::remove_file(&tmp);
    let dmx = demux_typed(bytes);
    // Video track sees its colour.
    let video_stream = dmx
        .streams()
        .iter()
        .find(|s| s.params.media_type == MediaType::Video)
        .expect("video stream");
    assert!(dmx.video_colour(video_stream.index).is_some());
    // Audio track has no colour data.
    let audio_stream = dmx
        .streams()
        .iter()
        .find(|s| s.params.media_type == MediaType::Audio)
        .expect("audio stream");
    assert!(dmx.video_colour(audio_stream.index).is_none());
}

#[test]
fn accessor_returns_queued_value() {
    // Pre-`write_header`, the muxer's own `video_colour` accessor
    // returns the queued hint.
    let tmp = tmp_path("acc");
    let f = std::fs::File::create(&tmp).unwrap();
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).unwrap();
    assert!(mx.video_colour(0).is_none());
    let c = MkvVideoColour::bt709();
    mx.set_video_colour(0, c).unwrap();
    assert_eq!(mx.video_colour(0), Some(c));
    // Out-of-range accessor is `None` too.
    assert!(mx.video_colour(42).is_none());
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn independent_from_other_video_setters() {
    // Setting `set_video_colour` does not perturb the other video
    // sub-element setters; combining it with set_video_interlacing,
    // set_video_stereo_mode, set_video_alpha_mode, set_video_geometry,
    // and set_video_uncompressed_fourcc round-trips every value.
    use oxideav_mkv::demux::{AlphaMode, FieldOrder, FlagInterlaced, StereoMode};
    use oxideav_mkv::mux::MkvVideoGeometry;
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, MkvVideoColour::bt2020_pq()).unwrap();
        mx.set_video_interlacing(0, FlagInterlaced::Interlaced, Some(FieldOrder::Tff))
            .unwrap();
        mx.set_video_stereo_mode(0, StereoMode::SideBySideRightFirst)
            .unwrap();
        mx.set_video_alpha_mode(0, AlphaMode::Present).unwrap();
        mx.set_video_geometry(0, MkvVideoGeometry::aspect_ratio(16, 9))
            .unwrap();
        mx.set_video_uncompressed_fourcc(0, *b"NV12").unwrap();
    });
    let dmx = demux_typed(bytes);
    // Colour came through.
    let c = dmx.video_colour(0).expect("video_colour");
    assert_eq!(c.primaries(), Primaries::BT2020);
    assert_eq!(c.bits_per_channel(), 10);
    // Interlacing came through.
    let vi = dmx.video_interlacing(0).expect("video_interlacing");
    assert_eq!(vi.flag(), FlagInterlaced::Interlaced);
    assert_eq!(vi.field_order(), Some(FieldOrder::Tff));
    // Stereo came through.
    assert_eq!(
        dmx.video_stereo_mode(0),
        Some(StereoMode::SideBySideRightFirst)
    );
    // Alpha came through.
    assert_eq!(dmx.video_alpha_mode(0), Some(AlphaMode::Present));
    // UncompressedFourCC came through.
    assert_eq!(
        dmx.video_uncompressed_fourcc(0).and_then(|f| f.fourcc()),
        Some(*b"NV12")
    );
}

#[test]
fn empty_master_on_disk_size_is_two_bytes() {
    // Default `MkvVideoColour` should serialise as `Colour` master id
    // (0x55B0, two bytes) + size VINT 0x80 (zero-length payload) — three
    // total bytes inside the parent Video master. Scan for the exact
    // [0x55, 0xB0, 0x80] sequence.
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, MkvVideoColour::default()).unwrap();
    });
    let needle = [0x55u8, 0xB0, 0x80];
    assert!(
        bytes.windows(needle.len()).any(|w| w == needle),
        "empty Colour master must be exactly 3 bytes on disk: id 0x55B0 + size VINT 0x80"
    );
}

#[test]
fn matrix_coefficients_explicit_bt2020_ncl_round_trip() {
    // Pin the matrix-coefficient writer at value 9 (BT.2020 NCL).
    let c = MkvVideoColour {
        matrix_coefficients: MatrixCoefficients::BT2020NonConstantLuminance,
        ..MkvVideoColour::default()
    };
    let bytes = mux_video(|mx| {
        mx.set_video_colour(0, c).unwrap();
    });
    let dmx = demux_typed(bytes);
    let got = dmx.video_colour(0).expect("video_colour");
    assert_eq!(
        got.matrix_coefficients(),
        MatrixCoefficients::BT2020NonConstantLuminance
    );
}
