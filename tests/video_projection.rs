//! Integration tests for the demuxer's `Video > Projection` (RFC 9559
//! §5.1.4.1.28.41..§5.1.4.1.28.46) typed decode.
//!
//! `Projection` carries the per-track projection (rectangular /
//! equirectangular / cubemap / mesh) plus an ISOBMFF-mirrored
//! `ProjectionPrivate` payload and a yaw/pitch/roll pose triple. §27.15
//! leaves the projection-type registry open, so the typed surface preserves
//! unknown values via `ProjectionType::Other`. The demuxer exposes the
//! typed value via
//! `MkvDemuxer::video_projection(stream_index) -> Option<&Projection>`.
//!
//! Each test hand-builds an EBML byte sequence containing a `Video` master
//! with the elements of interest, parses it through the typed open entry,
//! and asserts the typed value matches the spec — including the §27 spec
//! defaults and the §5.1.4.1.28.46 worked example.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_mkv::demux::ProjectionType;
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

fn elem_binary(id: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(bytes.len() as u64, 0));
    out.extend_from_slice(bytes);
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

/// Build a video TrackEntry whose `Video` master is built from `video_body`.
fn video_track_with_video(number: u64, uid: u64, video_body: &[u8]) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_VIDEO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "V_VP9"));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::PIXEL_WIDTH, 320));
    v.extend_from_slice(&elem_uint(ids::PIXEL_HEIGHT, 240));
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

fn open(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

/// A video track with no `Projection` element at all returns `None` from
/// `video_projection`. The slice view still has one entry per stream.
#[test]
fn missing_projection_returns_none() {
    let v: Vec<u8> = Vec::new();
    let t = video_track_with_video(1, 0xAB, &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&tracks_body));

    assert_eq!(
        dmx.video_projection(0),
        None,
        "no `Projection` master => `None`"
    );
    assert_eq!(dmx.video_projections().len(), dmx.streams().len());
}

/// An empty `Projection` master decodes as a fully-typed identity rectangular
/// projection: `ProjectionType::Rectangular`, no `ProjectionPrivate`, zero
/// pose. Distinguishable from `None` (which means "no `Projection` master at
/// all").
#[test]
fn empty_projection_materialises_defaults() {
    let mut v = Vec::new();
    v.extend_from_slice(&elem_master(ids::PROJECTION, &[]));
    let t = video_track_with_video(1, 0x11, &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&tracks_body));

    let p = dmx.video_projection(0).expect("Projection present");
    assert_eq!(p.projection_type(), ProjectionType::Rectangular);
    assert_eq!(p.private(), None);
    assert_eq!(p.pose_yaw(), 0.0);
    assert_eq!(p.pose_pitch(), 0.0);
    assert_eq!(p.pose_roll(), 0.0);
    assert!(!p.is_rotated());
    assert!(!p.projection_type().is_spherical());
}

/// Every registered §5.1.4.1.28.42 Table 18 value round-trips through the
/// typed surface, and `is_spherical()` returns `true` for everything except
/// `Rectangular`.
#[test]
fn all_registered_projection_types_round_trip() {
    let cases: &[(u64, ProjectionType)] = &[
        (
            ids::PROJECTION_TYPE_RECTANGULAR,
            ProjectionType::Rectangular,
        ),
        (
            ids::PROJECTION_TYPE_EQUIRECTANGULAR,
            ProjectionType::Equirectangular,
        ),
        (ids::PROJECTION_TYPE_CUBEMAP, ProjectionType::Cubemap),
        (ids::PROJECTION_TYPE_MESH, ProjectionType::Mesh),
    ];
    for (raw, expected) in cases {
        let mut pb = Vec::new();
        pb.extend_from_slice(&elem_uint(ids::PROJECTION_TYPE, *raw));
        let mut v = Vec::new();
        v.extend_from_slice(&elem_master(ids::PROJECTION, &pb));
        let t = video_track_with_video(1, 0x22, &v);
        let mut tracks_body = Vec::new();
        tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
        let dmx = open(assemble(&tracks_body));

        let p = dmx.video_projection(0).expect("video track");
        assert_eq!(
            p.projection_type(),
            *expected,
            "ProjectionType raw {raw} should decode to {expected:?}"
        );
        assert_eq!(
            p.projection_type().is_spherical(),
            *raw != ids::PROJECTION_TYPE_RECTANGULAR,
            "is_spherical() should match the non-Rectangular predicate for raw {raw}"
        );
    }
}

/// A value outside §5.1.4.1.28.42 Table 18 passes through the
/// `ProjectionType::Other` variant rather than being dropped. §27.15 leaves
/// the registry open for future additions.
#[test]
fn unregistered_projection_type_is_other() {
    let mut pb = Vec::new();
    pb.extend_from_slice(&elem_uint(ids::PROJECTION_TYPE, 99));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_master(ids::PROJECTION, &pb));
    let t = video_track_with_video(1, 0x33, &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&tracks_body));

    let p = dmx.video_projection(0).expect("video track");
    assert_eq!(p.projection_type(), ProjectionType::Other(99));
    // Unknown values count as non-rectangular ("is this spherical?" => yes).
    assert!(p.projection_type().is_spherical());
}

/// `ProjectionPrivate` surfaces verbatim as an `Option<&[u8]>`. The
/// container never parses or validates the payload — it's the FullBox body
/// of the ISOBMFF box that pairs with the projection type.
#[test]
fn projection_private_round_trips_verbatim() {
    // A made-up 12-byte equirectangular "equi" body (FullBox version=0 +
    // flags=0x000000, then four 32-bit ints filling the projection_bounds
    // area). The actual byte-meaning lives in the ISOBMFF spec, not RFC 9559
    // — we just preserve the bytes.
    let private_bytes: &[u8] = &[
        0x00, 0x00, 0x00, 0x00, // FullBox version+flags
        0x00, 0x00, 0x00, 0x10, // projection_bounds_top
        0x00, 0x00, 0x00, 0x20, // projection_bounds_bottom
    ];
    let mut pb = Vec::new();
    pb.extend_from_slice(&elem_uint(
        ids::PROJECTION_TYPE,
        ids::PROJECTION_TYPE_EQUIRECTANGULAR,
    ));
    pb.extend_from_slice(&elem_binary(ids::PROJECTION_PRIVATE, private_bytes));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_master(ids::PROJECTION, &pb));
    let t = video_track_with_video(1, 0x44, &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&tracks_body));

    let p = dmx.video_projection(0).expect("video track");
    assert_eq!(p.projection_type(), ProjectionType::Equirectangular);
    assert_eq!(p.private(), Some(private_bytes));
}

/// The §5.1.4.1.28.46 worked example:
/// `<Projection><ProjectionPoseRoll>90</ProjectionPoseRoll></Projection>`
/// signals a 90° counter-clockwise rotation. Round-trips through the typed
/// surface with `projection_type == Rectangular`, `pose_roll == 90.0`, and
/// the other components at `0.0`. `is_rotated()` flips to `true`.
#[test]
fn rfc_pose_roll_worked_example() {
    let mut pb = Vec::new();
    pb.extend_from_slice(&elem_float_be_f64(ids::PROJECTION_POSE_ROLL, 90.0));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_master(ids::PROJECTION, &pb));
    let t = video_track_with_video(1, 0x55, &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&tracks_body));

    let p = dmx.video_projection(0).expect("video track");
    assert_eq!(p.projection_type(), ProjectionType::Rectangular);
    assert_eq!(p.pose_yaw(), 0.0);
    assert_eq!(p.pose_pitch(), 0.0);
    assert_eq!(p.pose_roll(), 90.0);
    assert!(p.is_rotated());
}

/// All three pose components round-trip through the typed surface (4-byte
/// float storage too, since §5.1.4.1.28.44..46 typed them as float without
/// forcing 8-byte storage).
#[test]
fn pose_triple_round_trips_through_f32_storage() {
    let mut pb = Vec::new();
    pb.extend_from_slice(&elem_float_be_f32(ids::PROJECTION_POSE_YAW, -45.5));
    pb.extend_from_slice(&elem_float_be_f32(ids::PROJECTION_POSE_PITCH, 30.25));
    pb.extend_from_slice(&elem_float_be_f32(ids::PROJECTION_POSE_ROLL, -90.0));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_master(ids::PROJECTION, &pb));
    let t = video_track_with_video(1, 0x66, &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&tracks_body));

    let p = dmx.video_projection(0).expect("video track");
    assert_eq!(p.pose_yaw(), -45.5);
    assert_eq!(p.pose_pitch(), 30.25);
    assert_eq!(p.pose_roll(), -90.0);
    assert!(p.is_rotated());
}

/// An audio track (no `Video` master at all) returns `None` from
/// `video_projection`; the slice view still has one entry per stream.
#[test]
fn audio_track_has_no_projection() {
    let ta = audio_track(1, 0x77);
    let mut pb = Vec::new();
    pb.extend_from_slice(&elem_uint(
        ids::PROJECTION_TYPE,
        ids::PROJECTION_TYPE_CUBEMAP,
    ));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_master(ids::PROJECTION, &pb));
    let tv = video_track_with_video(2, 0x88, &v);

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
    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);

    let dmx = open(out);

    assert_eq!(dmx.video_projection(0), None);
    assert_eq!(
        dmx.video_projection(1).map(|p| p.projection_type()),
        Some(ProjectionType::Cubemap),
    );
    assert_eq!(dmx.video_projections().len(), dmx.streams().len());
}

/// Forward-compat: a `Projection` master containing an unknown sub-element
/// is skipped cleanly without breaking the typed decode of the known
/// children.
#[test]
fn unknown_projection_subelement_is_skipped() {
    // ProjectionType + a synthetic 0x7799 element ("not in the spec") +
    // ProjectionPoseRoll, in that order. The skip must consume the unknown
    // element exactly so the pose-roll that follows still decodes.
    let mut pb = Vec::new();
    pb.extend_from_slice(&elem_uint(
        ids::PROJECTION_TYPE,
        ids::PROJECTION_TYPE_EQUIRECTANGULAR,
    ));
    pb.extend_from_slice(&elem_binary(0x7799, &[0x01, 0x02, 0x03, 0x04]));
    pb.extend_from_slice(&elem_float_be_f64(ids::PROJECTION_POSE_ROLL, 45.0));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_master(ids::PROJECTION, &pb));
    let t = video_track_with_video(1, 0x99, &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&tracks_body));

    let p = dmx.video_projection(0).expect("video track");
    assert_eq!(p.projection_type(), ProjectionType::Equirectangular);
    assert_eq!(p.pose_roll(), 45.0);
}

/// Returns `None` for an out-of-range `stream_index`.
#[test]
fn out_of_range_stream_index_returns_none() {
    let mut pb = Vec::new();
    pb.extend_from_slice(&elem_uint(ids::PROJECTION_TYPE, ids::PROJECTION_TYPE_MESH));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_master(ids::PROJECTION, &pb));
    let t = video_track_with_video(1, 0xAA, &v);
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &t));
    let dmx = open(assemble(&tracks_body));

    assert!(dmx.video_projection(0).is_some());
    assert_eq!(dmx.video_projection(99), None);
}
