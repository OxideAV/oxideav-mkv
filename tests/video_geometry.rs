//! Integration tests for the demuxer's `Video > PixelCrop{Top,Bottom,Left,
//! Right}` (RFC 9559 §5.1.4.1.28.8..§5.1.4.1.28.11) + `DisplayWidth` /
//! `DisplayHeight` (§5.1.4.1.28.12 / .13) + `DisplayUnit` (§5.1.4.1.28.14)
//! typed decode.
//!
//! `PixelCrop*` carve a visible rectangle out of the encoded buffer (RFC 9559
//! §5.1.4.1.28.8..11). `DisplayWidth` / `DisplayHeight` describe the
//! rendered size of that cropped image, in units selected by `DisplayUnit`
//! (Table 10: `0` pixels / `1` cm / `2` in / `3` DAR / `4` unknown). Per
//! §5.1.4.1.28.12 / .13 the spec defaults for `DisplayWidth` and
//! `DisplayHeight` (encoded size minus crops on each axis) apply only when
//! `DisplayUnit == 0` (pixels); for every other DisplayUnit the spec is
//! explicit: "there is no default value", which the typed surface
//! materialises as `None`.
//!
//! Each test hand-builds an EBML byte sequence containing a `Video` master
//! with the elements of interest, parses it through the demuxer's typed
//! open entry, and asserts the typed fields match the spec.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_mkv::demux::DisplayUnit;
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

/// Build a video TrackEntry whose `Video` master starts with the given
/// PixelWidth/PixelHeight and is then extended by `video_body`.
fn video_track_with_size(
    number: u64,
    uid: u64,
    pixel_width: u64,
    pixel_height: u64,
    video_body: &[u8],
) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_VIDEO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "V_VP9"));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::PIXEL_WIDTH, pixel_width));
    v.extend_from_slice(&elem_uint(ids::PIXEL_HEIGHT, pixel_height));
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

/// A track that explicitly sets all four PixelCrop fields plus DisplayWidth
/// and DisplayHeight surfaces every value verbatim through the typed
/// accessor. `DisplayUnit` defaults to `0` (pixels) per §5.1.4.1.28.14.
#[test]
fn pixel_crop_and_display_explicit() {
    // 1920x1080 stored, 240px pillarboxed on each side, 16:9 rendered out.
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::PIXEL_CROP_LEFT, 240));
    v.extend_from_slice(&elem_uint(ids::PIXEL_CROP_RIGHT, 240));
    v.extend_from_slice(&elem_uint(ids::PIXEL_CROP_TOP, 0));
    v.extend_from_slice(&elem_uint(ids::PIXEL_CROP_BOTTOM, 0));
    v.extend_from_slice(&elem_uint(ids::DISPLAY_WIDTH, 1440));
    v.extend_from_slice(&elem_uint(ids::DISPLAY_HEIGHT, 1080));
    let t = video_track_with_size(1, 0xAB, 1920, 1080, &v);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let g = dmx.video_geometry(0).expect("video track has geometry");
    assert_eq!(g.pixel_crop_left(), 240);
    assert_eq!(g.pixel_crop_right(), 240);
    assert_eq!(g.pixel_crop_top(), 0);
    assert_eq!(g.pixel_crop_bottom(), 0);
    assert_eq!(g.display_width(), Some(1440));
    assert_eq!(g.display_height(), Some(1080));
    assert_eq!(g.display_unit(), DisplayUnit::Pixels);

    // The slice view has one entry per stream.
    assert_eq!(dmx.video_geometries().len(), dmx.streams().len());
}

/// A bare `Video` master with PixelWidth/PixelHeight only (no `PixelCrop*`,
/// no `DisplayWidth/Height/Unit`) materialises the spec defaults:
/// every `PixelCrop*` resolves to `0` (§5.1.4.1.28.8..11 default),
/// `DisplayUnit` resolves to `Pixels` (§5.1.4.1.28.14 default `0`), and
/// `DisplayWidth` / `DisplayHeight` derive from
/// `PixelWidth - PixelCropLeft - PixelCropRight` / `PixelHeight - top -
/// bottom` per the §5.1.4.1.28.12 / .13 default note (which applies only
/// when DisplayUnit == 0).
#[test]
fn defaults_when_no_geometry_present() {
    let v = Vec::new(); // No PixelCrop, no Display elements.
    let t = video_track_with_size(1, 0xCD, 1280, 720, &v);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let g = dmx.video_geometry(0).expect("Video master present");
    assert_eq!(g.pixel_crop_top(), 0);
    assert_eq!(g.pixel_crop_bottom(), 0);
    assert_eq!(g.pixel_crop_left(), 0);
    assert_eq!(g.pixel_crop_right(), 0);
    assert_eq!(g.display_unit(), DisplayUnit::Pixels);
    // Derived default: 1280 - 0 - 0 = 1280, 720 - 0 - 0 = 720.
    assert_eq!(g.display_width(), Some(1280));
    assert_eq!(g.display_height(), Some(720));
}

/// A track that omits explicit `DisplayWidth` / `DisplayHeight` but does set
/// PixelCrops still materialises the spec derivation (subtracting crops from
/// PixelWidth / PixelHeight) — but only because DisplayUnit defaults to `0`
/// (pixels). The derivation tracks the §5.1.4.1.28.12 / .13 default rule.
#[test]
fn derived_display_after_crops() {
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::PIXEL_CROP_TOP, 4));
    v.extend_from_slice(&elem_uint(ids::PIXEL_CROP_BOTTOM, 4));
    v.extend_from_slice(&elem_uint(ids::PIXEL_CROP_LEFT, 8));
    v.extend_from_slice(&elem_uint(ids::PIXEL_CROP_RIGHT, 8));
    // No DisplayWidth/Height/Unit: defaults to pixels-with-derivation.
    let t = video_track_with_size(1, 0xEF, 640, 480, &v);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let g = dmx.video_geometry(0).expect("video track has geometry");
    assert_eq!(g.display_unit(), DisplayUnit::Pixels);
    // Derived defaults: PixelWidth - left - right = 640 - 8 - 8 = 624.
    assert_eq!(g.display_width(), Some(624));
    // PixelHeight - top - bottom = 480 - 4 - 4 = 472.
    assert_eq!(g.display_height(), Some(472));
}

/// When `DisplayUnit` is non-zero (e.g. `3` = display aspect ratio) the spec
/// is explicit that there is no default value for `DisplayWidth` /
/// `DisplayHeight`: §5.1.4.1.28.12 "If the DisplayUnit of the same TrackEntry
/// is 0, then the default value for DisplayWidth is ...; else, there is no
/// default value." Absent elements therefore surface as `None`.
///
/// Also exercises the explicit-DisplayWidth-with-non-pixel-unit path: a 16:9
/// DAR encoded as `DisplayWidth=16` / `DisplayHeight=9` round-trips.
#[test]
fn display_unit_dar_no_derivation() {
    // Track 0: DAR unit, explicit 16:9.
    let mut v0 = Vec::new();
    v0.extend_from_slice(&elem_uint(ids::DISPLAY_UNIT, ids::DISPLAY_UNIT_DAR));
    v0.extend_from_slice(&elem_uint(ids::DISPLAY_WIDTH, 16));
    v0.extend_from_slice(&elem_uint(ids::DISPLAY_HEIGHT, 9));
    let t0 = video_track_with_size(1, 0x11, 1920, 1080, &v0);

    // Track 1: cm unit but no DisplayWidth/Height -> no default applies.
    let mut v1 = Vec::new();
    v1.extend_from_slice(&elem_uint(ids::DISPLAY_UNIT, ids::DISPLAY_UNIT_CENTIMETERS));
    let t1 = video_track_with_size(2, 0x22, 1920, 1080, &v1);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t0));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t1));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let g0 = dmx.video_geometry(0).expect("DAR track");
    assert_eq!(g0.display_unit(), DisplayUnit::DisplayAspectRatio);
    assert_eq!(g0.display_width(), Some(16));
    assert_eq!(g0.display_height(), Some(9));

    let g1 = dmx.video_geometry(1).expect("cm track");
    assert_eq!(g1.display_unit(), DisplayUnit::Centimeters);
    assert_eq!(
        g1.display_width(),
        None,
        "non-pixel DisplayUnit: no default for absent DisplayWidth"
    );
    assert_eq!(
        g1.display_height(),
        None,
        "non-pixel DisplayUnit: no default for absent DisplayHeight"
    );
}

/// An unrecognised `DisplayUnit` value passes through `DisplayUnit::Other`
/// rather than being dropped — §5.1.4.1.28.14 notes that additional values
/// can be registered in the "Matroska Display Units" registry (§27.9).
/// A non-pixel `DisplayUnit::Other` also disables the §5.1.4.1.28.12 / .13
/// derivation default.
#[test]
fn unknown_display_unit_other_and_no_derivation() {
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::DISPLAY_UNIT, 42));
    let t = video_track_with_size(1, 0x33, 1280, 720, &v);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let g = dmx.video_geometry(0).expect("video track has geometry");
    assert_eq!(g.display_unit(), DisplayUnit::Other(42));
    assert_eq!(g.display_width(), None);
    assert_eq!(g.display_height(), None);
}

/// A track with no `Video` master at all (an audio track) reports `None`
/// from `video_geometry` — the typed surface is video-only. Also confirms
/// `video_geometries()` keeps one entry per stream, paired with a video
/// track that does carry geometry.
#[test]
fn audio_track_has_no_geometry() {
    let ta = audio_track(1, 0x55);
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::PIXEL_CROP_TOP, 1));
    v.extend_from_slice(&elem_uint(ids::PIXEL_CROP_BOTTOM, 1));
    let tv = video_track_with_size(2, 0x66, 320, 240, &v);

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

    assert_eq!(dmx.video_geometries().len(), dmx.streams().len());
    assert!(
        dmx.video_geometry(0).is_none(),
        "audio track has no Video master -> no geometry record"
    );
    let gv = dmx.video_geometry(1).expect("video track");
    assert_eq!(gv.pixel_crop_top(), 1);
    assert_eq!(gv.pixel_crop_bottom(), 1);
    // Derived default: 320 - 0 - 0 = 320, 240 - 1 - 1 = 238.
    assert_eq!(gv.display_width(), Some(320));
    assert_eq!(gv.display_height(), Some(238));
}

/// A malformed file where the PixelCrop sum exceeds the encoded PixelWidth /
/// PixelHeight on either axis would underflow the §5.1.4.1.28.12 / .13
/// derivation. The typed surface returns `None` rather than wrapping
/// arithmetic on such files.
#[test]
fn underflowing_derivation_returns_none() {
    let mut v = Vec::new();
    // 100x100 encoded, but 60 + 60 = 120 worth of horizontal crop > 100.
    v.extend_from_slice(&elem_uint(ids::PIXEL_CROP_LEFT, 60));
    v.extend_from_slice(&elem_uint(ids::PIXEL_CROP_RIGHT, 60));
    v.extend_from_slice(&elem_uint(ids::PIXEL_CROP_TOP, 10));
    v.extend_from_slice(&elem_uint(ids::PIXEL_CROP_BOTTOM, 10));
    // No explicit DisplayWidth/Height/Unit -> falls into pixel derivation.
    let t = video_track_with_size(1, 0x77, 100, 100, &v);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let g = dmx.video_geometry(0).expect("video track has geometry");
    // Width derivation underflows -> None.
    assert_eq!(g.display_width(), None);
    // Height derivation: 100 - 10 - 10 = 80 -> still valid.
    assert_eq!(g.display_height(), Some(80));
}
