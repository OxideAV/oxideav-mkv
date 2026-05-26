//! Integration tests for the demuxer's `Video > Colour` typed decode
//! (RFC 9559 §5.1.4.1.28.16 — `Colour` master plus all sub-elements
//! including the SMPTE 2086 / CTA-861.3 HDR `MasteringMetadata` and the
//! `MaxCLL` / `MaxFALL` light-level pair).
//!
//! Each test hand-builds an EBML byte sequence containing a `Video` master
//! with a `Colour` child, parses it through the demuxer's typed open entry,
//! and asserts the typed fields match the spec.

use std::io::Cursor;

use oxideav_core::ReadSeek;
use oxideav_mkv::demux::{
    ChromaSitingHorz, ChromaSitingVert, ColourRange, MatrixCoefficients, Primaries,
    TransferCharacteristics,
};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

fn elem_uint(id: u32, value: u64) -> Vec<u8> {
    let n = if value == 0 {
        1
    } else {
        (64 - value.leading_zeros()).div_ceil(8) as usize
    };
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(n as u64, 0));
    for i in (0..n).rev() {
        out.push(((value >> (i * 8)) & 0xFF) as u8);
    }
    out
}

fn elem_str(id: u32, s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(s.len() as u64, 0));
    out.extend_from_slice(s.as_bytes());
    out
}

fn elem_float_be_f64(id: u32, value: f64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(8, 0));
    out.extend_from_slice(&value.to_be_bytes());
    out
}

fn elem_float_be_f32(id: u32, value: f32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(4, 0));
    out.extend_from_slice(&value.to_be_bytes());
    out
}

fn elem_master(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

fn simple_block(track: u8, tc_offset: i16, keyframe: bool, payload: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(if keyframe { 0x80 } else { 0x00 });
    body.push(payload);
    elem_master(ids::SIMPLE_BLOCK, &body)
}

fn ebml_header() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    b.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    elem_master(ids::EBML_HEADER, &b)
}

fn info() -> Vec<u8> {
    let mut ib = Vec::new();
    ib.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    ib.extend_from_slice(&elem_float_be_f64(ids::DURATION, 1000.0));
    elem_master(ids::INFO, &ib)
}

fn one_cluster() -> Vec<u8> {
    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cb.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    elem_master(ids::CLUSTER, &cb)
}

/// Build a video TrackEntry whose `Video` master has the given
/// PixelWidth/PixelHeight followed by `video_body` (typically a `Colour`
/// master).
fn video_track(number: u64, uid: u64, video_body: &[u8]) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_VIDEO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "V_VP9"));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::PIXEL_WIDTH, 1920));
    v.extend_from_slice(&elem_uint(ids::PIXEL_HEIGHT, 1080));
    v.extend_from_slice(video_body);
    tb.extend_from_slice(&elem_master(ids::VIDEO, &v));
    tb
}

/// Build an audio TrackEntry (no `Video` master at all).
fn audio_track(number: u64, uid: u64) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_OPUS"));
    tb
}

/// Assemble EBML header + Segment(Info, Tracks, Cluster) into a file.
fn assemble(tracks_body: &[u8]) -> Vec<u8> {
    let tracks = elem_master(ids::TRACKS, tracks_body);
    let mut seg = Vec::new();
    seg.extend_from_slice(&info());
    seg.extend_from_slice(&tracks);
    seg.extend_from_slice(&one_cluster());
    let segment = elem_master(ids::SEGMENT, &seg);
    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

/// A `Video` master with no `Colour` child surfaces `None` from
/// `video_colour` — the typed surface is opt-in per the master's presence.
#[test]
fn no_colour_master_returns_none() {
    let t = video_track(1, 0xAA, &[]);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    assert!(dmx.video_colour(0).is_none());
    // Slice view still has one entry per stream.
    assert_eq!(dmx.video_colours().len(), 1);
}

/// An empty `Colour` master (the master exists but carries no children at
/// all) materialises the spec defaults across the board:
/// MatrixCoefficients / TransferCharacteristics / Primaries each default
/// to `2` (*unspecified*); ChromaSiting / Range / BitsPerChannel default
/// to `0`; everything else surfaces as `None`.
#[test]
fn empty_colour_master_materialises_spec_defaults() {
    let colour = elem_master(ids::COLOUR, &[]);
    let t = video_track(1, 0xBB, &colour);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    let c = dmx.video_colour(0).expect("Colour master present");
    assert_eq!(c.matrix_coefficients(), MatrixCoefficients::Unspecified);
    assert_eq!(
        c.transfer_characteristics(),
        TransferCharacteristics::Unspecified
    );
    assert_eq!(c.primaries(), Primaries::Unspecified);
    assert_eq!(c.chroma_siting_horz(), ChromaSitingHorz::Unspecified);
    assert_eq!(c.chroma_siting_vert(), ChromaSitingVert::Unspecified);
    assert_eq!(c.range(), ColourRange::Unspecified);
    assert_eq!(c.bits_per_channel(), 0);
    assert_eq!(c.chroma_subsampling_horz(), None);
    assert_eq!(c.chroma_subsampling_vert(), None);
    assert_eq!(c.cb_subsampling_horz(), None);
    assert_eq!(c.cb_subsampling_vert(), None);
    assert_eq!(c.max_cll(), None);
    assert_eq!(c.max_fall(), None);
    assert!(c.mastering_metadata().is_none());
}

/// A BT.709 SDR 4:2:0 description round-trips: explicit
/// MatrixCoefficients / TransferCharacteristics / Primaries =1 (BT.709),
/// `Range = 1` (broadcast), 8-bit, `ChromaSubsampling{Horz,Vert} = 1`,
/// `ChromaSiting{Horz,Vert} = 1` (left/top collocated).
#[test]
fn bt709_sdr_typed_round_trip() {
    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::MATRIX_COEFFICIENTS, 1));
    cb.extend_from_slice(&elem_uint(ids::BITS_PER_CHANNEL, 8));
    cb.extend_from_slice(&elem_uint(ids::CHROMA_SUBSAMPLING_HORZ, 1));
    cb.extend_from_slice(&elem_uint(ids::CHROMA_SUBSAMPLING_VERT, 1));
    cb.extend_from_slice(&elem_uint(
        ids::CHROMA_SITING_HORZ,
        ids::CHROMA_SITING_HORZ_LEFT_COLLOCATED,
    ));
    cb.extend_from_slice(&elem_uint(
        ids::CHROMA_SITING_VERT,
        ids::CHROMA_SITING_VERT_TOP_COLLOCATED,
    ));
    cb.extend_from_slice(&elem_uint(ids::COLOUR_RANGE, ids::COLOUR_RANGE_BROADCAST));
    cb.extend_from_slice(&elem_uint(ids::TRANSFER_CHARACTERISTICS, 1));
    cb.extend_from_slice(&elem_uint(ids::PRIMARIES, 1));
    let colour = elem_master(ids::COLOUR, &cb);
    let t = video_track(1, 0xCC, &colour);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    let c = dmx.video_colour(0).expect("Colour present");
    assert_eq!(c.matrix_coefficients(), MatrixCoefficients::BT709);
    assert_eq!(c.transfer_characteristics(), TransferCharacteristics::BT709);
    assert_eq!(c.primaries(), Primaries::BT709);
    assert_eq!(c.range(), ColourRange::Broadcast);
    assert_eq!(c.chroma_siting_horz(), ChromaSitingHorz::LeftCollocated);
    assert_eq!(c.chroma_siting_vert(), ChromaSitingVert::TopCollocated);
    assert_eq!(c.bits_per_channel(), 8);
    assert_eq!(c.chroma_subsampling_horz(), Some(1));
    assert_eq!(c.chroma_subsampling_vert(), Some(1));
}

/// A BT.2100 PQ HDR description with MaxCLL / MaxFALL / MasteringMetadata
/// (SMPTE 2086) round-trips. The mastering primaries / white point /
/// luminance values surface through the typed accessor.
///
/// Uses 8-byte floats for the chromaticities and luminances — RFC 9559 lets
/// floats be 4 or 8 byte and `ebml::read_float` accepts either; see the
/// 4-byte test below for the other size.
#[test]
fn bt2100_hdr_with_mastering_metadata() {
    let mut mb = Vec::new();
    // BT.2020 / DCI-P3 reference primaries (placeholder values — only the
    // round-trip matters here, the parser does not validate them).
    mb.extend_from_slice(&elem_float_be_f64(ids::PRIMARY_R_CHROMATICITY_X, 0.708));
    mb.extend_from_slice(&elem_float_be_f64(ids::PRIMARY_R_CHROMATICITY_Y, 0.292));
    mb.extend_from_slice(&elem_float_be_f64(ids::PRIMARY_G_CHROMATICITY_X, 0.170));
    mb.extend_from_slice(&elem_float_be_f64(ids::PRIMARY_G_CHROMATICITY_Y, 0.797));
    mb.extend_from_slice(&elem_float_be_f64(ids::PRIMARY_B_CHROMATICITY_X, 0.131));
    mb.extend_from_slice(&elem_float_be_f64(ids::PRIMARY_B_CHROMATICITY_Y, 0.046));
    mb.extend_from_slice(&elem_float_be_f64(ids::WHITE_POINT_CHROMATICITY_X, 0.3127));
    mb.extend_from_slice(&elem_float_be_f64(ids::WHITE_POINT_CHROMATICITY_Y, 0.3290));
    mb.extend_from_slice(&elem_float_be_f64(ids::LUMINANCE_MAX, 1000.0));
    mb.extend_from_slice(&elem_float_be_f64(ids::LUMINANCE_MIN, 0.005));
    let mastering = elem_master(ids::MASTERING_METADATA, &mb);

    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::MATRIX_COEFFICIENTS, 9)); // BT.2020 NCL
    cb.extend_from_slice(&elem_uint(ids::BITS_PER_CHANNEL, 10));
    cb.extend_from_slice(&elem_uint(ids::COLOUR_RANGE, ids::COLOUR_RANGE_FULL));
    cb.extend_from_slice(&elem_uint(ids::TRANSFER_CHARACTERISTICS, 16)); // BT.2100 PQ
    cb.extend_from_slice(&elem_uint(ids::PRIMARIES, 9)); // BT.2020
    cb.extend_from_slice(&elem_uint(ids::MAX_CLL, 1000));
    cb.extend_from_slice(&elem_uint(ids::MAX_FALL, 400));
    cb.extend_from_slice(&mastering);
    let colour = elem_master(ids::COLOUR, &cb);

    let t = video_track(1, 0xDD, &colour);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let c = dmx.video_colour(0).expect("Colour present");
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
    assert_eq!(c.max_cll(), Some(1000));
    assert_eq!(c.max_fall(), Some(400));

    let m = c.mastering_metadata().expect("MasteringMetadata present");
    fn approx(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|v| (v - b).abs() < 1e-6)
    }
    assert!(approx(m.primary_r_chromaticity_x(), 0.708));
    assert!(approx(m.primary_r_chromaticity_y(), 0.292));
    assert!(approx(m.primary_g_chromaticity_x(), 0.170));
    assert!(approx(m.primary_g_chromaticity_y(), 0.797));
    assert!(approx(m.primary_b_chromaticity_x(), 0.131));
    assert!(approx(m.primary_b_chromaticity_y(), 0.046));
    assert!(approx(m.white_point_chromaticity_x(), 0.3127));
    assert!(approx(m.white_point_chromaticity_y(), 0.3290));
    assert!(approx(m.luminance_max(), 1000.0));
    assert!(approx(m.luminance_min(), 0.005));
}

/// 4-byte (`f32`) MasteringMetadata floats also decode — `ebml::read_float`
/// supports both 4- and 8-byte float widths and the typed surface accepts
/// either. Also exercises a sparse `MasteringMetadata` master: only a
/// subset of the chromaticity / luminance children are present, the rest
/// stay `None`.
#[test]
fn mastering_metadata_f32_and_sparse() {
    let mut mb = Vec::new();
    mb.extend_from_slice(&elem_float_be_f32(ids::LUMINANCE_MAX, 4000.0));
    mb.extend_from_slice(&elem_float_be_f32(ids::LUMINANCE_MIN, 0.0001));
    // No chromaticities — sparse case.
    let mastering = elem_master(ids::MASTERING_METADATA, &mb);

    let cb = mastering; // Colour body is just the MasteringMetadata master.
    let colour = elem_master(ids::COLOUR, &cb);
    let t = video_track(1, 0xEE, &colour);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let c = dmx.video_colour(0).expect("Colour present");
    let m = c.mastering_metadata().expect("MasteringMetadata present");
    assert!(m.luminance_max().is_some_and(|v| (v - 4000.0).abs() < 1e-3));
    assert!(m.luminance_min().is_some_and(|v| (v - 0.0001).abs() < 1e-6));
    // Sparse: nothing else is present.
    assert!(m.primary_r_chromaticity_x().is_none());
    assert!(m.white_point_chromaticity_x().is_none());
}

/// Values outside each table's registered set surface via `Other(raw)` —
/// the spec leaves room for additional registry entries (§27) and the
/// parser preserves them rather than dropping.
#[test]
fn unknown_enum_values_surface_as_other() {
    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::MATRIX_COEFFICIENTS, 99));
    cb.extend_from_slice(&elem_uint(ids::TRANSFER_CHARACTERISTICS, 250));
    cb.extend_from_slice(&elem_uint(ids::PRIMARIES, 77));
    cb.extend_from_slice(&elem_uint(ids::COLOUR_RANGE, 9));
    cb.extend_from_slice(&elem_uint(ids::CHROMA_SITING_HORZ, 7));
    cb.extend_from_slice(&elem_uint(ids::CHROMA_SITING_VERT, 8));
    let colour = elem_master(ids::COLOUR, &cb);
    let t = video_track(1, 0xFF, &colour);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let c = dmx.video_colour(0).expect("Colour present");
    assert_eq!(c.matrix_coefficients(), MatrixCoefficients::Other(99));
    assert_eq!(
        c.transfer_characteristics(),
        TransferCharacteristics::Other(250)
    );
    assert_eq!(c.primaries(), Primaries::Other(77));
    assert_eq!(c.range(), ColourRange::Other(9));
    assert_eq!(c.chroma_siting_horz(), ChromaSitingHorz::Other(7));
    assert_eq!(c.chroma_siting_vert(), ChromaSitingVert::Other(8));
}

/// An audio track (no `Video` master at all) reports `None` from
/// `video_colour`. The slice view keeps one entry per stream so callers
/// can iterate streams uniformly.
#[test]
fn audio_track_has_no_colour() {
    let ta = audio_track(1, 0x11);
    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::TRANSFER_CHARACTERISTICS, 16));
    let colour = elem_master(ids::COLOUR, &cb);
    let tv = video_track(2, 0x22, &colour);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &tv));

    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cb.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    let cluster = elem_master(ids::CLUSTER, &cb);

    let tracks = elem_master(ids::TRACKS, &tracks_body);
    let mut seg = Vec::new();
    seg.extend_from_slice(&info());
    seg.extend_from_slice(&tracks);
    seg.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&ebml_header());
    bytes.extend_from_slice(&segment);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    assert_eq!(dmx.video_colours().len(), 2);
    assert!(dmx.video_colour(0).is_none(), "audio track has no Colour");
    let cv = dmx.video_colour(1).expect("video track Colour");
    assert_eq!(
        cv.transfer_characteristics(),
        TransferCharacteristics::BT2100Pq
    );
}
