//! Integration tests for the demuxer's `TrackOperation` parsing
//! (RFC 9559 §5.1.4.1.30).
//!
//! A `TrackOperation` marks a *virtual* track assembled from other tracks:
//!
//! * `TrackCombinePlanes` (§5.1.4.1.30.1) — a list of `TrackPlane` masters,
//!   each naming a video track by `TrackPlaneUID` plus a `TrackPlaneType`
//!   (0 left eye, 1 right eye, 2 background). Used for stereoscopic 3D.
//! * `TrackJoinBlocks` (§5.1.4.1.30.5) — a list of `TrackJoinUID`s naming
//!   tracks whose Blocks are joined into one timeline.
//!
//! The demuxer resolves each `TrackUID` reference back to the matching
//! stream index (`None` for a dangling reference) and exposes the result
//! through `MkvDemuxer::track_operation(stream_index)` /
//! `track_operations()`.

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
use oxideav_mkv::demux::{TrackPlaneType, TrackRef};
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

/// A simple video track: number / uid / pixel dims.
fn video_track(number: u64, uid: u64) -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, number));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, uid));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_VIDEO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "V_VP9"));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::PIXEL_WIDTH, 320));
    v.extend_from_slice(&elem_uint(ids::PIXEL_HEIGHT, 240));
    tb.extend_from_slice(&elem_master(ids::VIDEO, &v));
    tb
}

/// Build one `TrackCombinePlanes` master from `(plane_uid, plane_type)` pairs.
fn combine_planes(planes: &[(u64, u64)]) -> Vec<u8> {
    let mut cp = Vec::new();
    for &(uid, ty) in planes {
        let mut plane = Vec::new();
        plane.extend_from_slice(&elem_uint(ids::TRACK_PLANE_UID, uid));
        plane.extend_from_slice(&elem_uint(ids::TRACK_PLANE_TYPE, ty));
        cp.extend_from_slice(&elem_master(ids::TRACK_PLANE, &plane));
    }
    elem_master(ids::TRACK_COMBINE_PLANES, &cp)
}

/// Build one `TrackJoinBlocks` master from a list of `TrackJoinUID`s.
fn join_blocks(uids: &[u64]) -> Vec<u8> {
    let mut jb = Vec::new();
    for &u in uids {
        jb.extend_from_slice(&elem_uint(ids::TRACK_JOIN_UID, u));
    }
    elem_master(ids::TRACK_JOIN_BLOCKS, &jb)
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

/// A stereoscopic 3D virtual track combining two physical plane tracks:
/// stream 0 = left eye (UID 0xL), stream 1 = right eye (UID 0xR), stream 2
/// = the virtual `TrackCombinePlanes` track.
#[test]
fn combine_planes_resolves_to_stream_indices() {
    const UID_LEFT: u64 = 0x11;
    const UID_RIGHT: u64 = 0x22;
    const UID_VIRTUAL: u64 = 0x33;

    let left = video_track(1, UID_LEFT);
    let right = video_track(2, UID_RIGHT);
    let virt = {
        let mut tb = video_track(3, UID_VIRTUAL);
        // TrackOperation\TrackCombinePlanes: left eye = UID_LEFT,
        // right eye = UID_RIGHT.
        let op = combine_planes(&[
            (UID_LEFT, ids::TRACK_PLANE_TYPE_LEFT_EYE),
            (UID_RIGHT, ids::TRACK_PLANE_TYPE_RIGHT_EYE),
        ]);
        tb.extend_from_slice(&elem_master(ids::TRACK_OPERATION, &op));
        tb
    };

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &left));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &right));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &virt));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    // Ordinary tracks have no operation.
    assert!(
        dmx.track_operation(0).is_none(),
        "left-eye track is not virtual"
    );
    assert!(
        dmx.track_operation(1).is_none(),
        "right-eye track is not virtual"
    );

    // The virtual track (stream 2) carries the combine.
    let op = dmx
        .track_operation(2)
        .expect("virtual track has TrackOperation");
    assert!(op.join_tracks.is_empty(), "no join in this file");
    assert_eq!(op.planes.len(), 2, "two planes");

    assert_eq!(
        op.planes[0].track,
        TrackRef {
            track_uid: UID_LEFT,
            stream_index: Some(0),
        },
        "left plane resolves to stream 0"
    );
    assert_eq!(op.planes[0].plane_type, TrackPlaneType::LeftEye);

    assert_eq!(
        op.planes[1].track,
        TrackRef {
            track_uid: UID_RIGHT,
            stream_index: Some(1),
        },
        "right plane resolves to stream 1"
    );
    assert_eq!(op.planes[1].plane_type, TrackPlaneType::RightEye);

    // The slice view has one entry per stream.
    assert_eq!(dmx.track_operations().len(), dmx.streams().len());
}

/// `TrackJoinBlocks` joins two physical tracks (UID 0xA, 0xB) into a
/// virtual track. The third UID is dangling and must surface with
/// `stream_index == None` rather than being dropped.
#[test]
fn join_blocks_with_dangling_reference() {
    const UID_A: u64 = 0xA;
    const UID_B: u64 = 0xB;
    const UID_DANGLING: u64 = 0xDEAD;

    let ta = video_track(1, UID_A);
    let tb = video_track(2, UID_B);
    let virt = {
        let mut t = video_track(3, 0xC);
        let op = join_blocks(&[UID_A, UID_B, UID_DANGLING]);
        t.extend_from_slice(&elem_master(ids::TRACK_OPERATION, &op));
        t
    };

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &tb));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &virt));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let op = dmx
        .track_operation(2)
        .expect("virtual track has TrackOperation");
    assert!(op.planes.is_empty(), "no planes in this file");
    assert_eq!(op.join_tracks.len(), 3, "three join references kept");

    assert_eq!(op.join_tracks[0].stream_index, Some(0), "UID_A -> stream 0");
    assert_eq!(op.join_tracks[1].stream_index, Some(1), "UID_B -> stream 1");
    assert_eq!(
        op.join_tracks[2],
        TrackRef {
            track_uid: UID_DANGLING,
            stream_index: None,
        },
        "dangling UID kept with stream_index None"
    );
}

/// A `TrackPlaneType` value above the named set surfaces as
/// `TrackPlaneType::Other`. A `TrackPlane` missing its mandatory
/// `TrackPlaneUID` is dropped, and a zero `TrackJoinUID` is dropped.
#[test]
fn unknown_plane_type_and_malformed_refs() {
    const UID_BG: u64 = 0x55;

    let bg = video_track(1, UID_BG);
    let virt = {
        let mut t = video_track(2, 0x66);
        // One valid plane with an unregistered plane type 7, plus a
        // malformed plane carrying only a TrackPlaneType (no UID).
        let mut cp = Vec::new();
        let mut good = Vec::new();
        good.extend_from_slice(&elem_uint(ids::TRACK_PLANE_UID, UID_BG));
        good.extend_from_slice(&elem_uint(ids::TRACK_PLANE_TYPE, 7));
        cp.extend_from_slice(&elem_master(ids::TRACK_PLANE, &good));
        let mut bad = Vec::new();
        bad.extend_from_slice(&elem_uint(
            ids::TRACK_PLANE_TYPE,
            ids::TRACK_PLANE_TYPE_BACKGROUND,
        ));
        cp.extend_from_slice(&elem_master(ids::TRACK_PLANE, &bad));
        let combine = elem_master(ids::TRACK_COMBINE_PLANES, &cp);
        // A TrackJoinBlocks whose only UID is zero (illegal "not 0") —
        // must be dropped, leaving no join references.
        let join = join_blocks(&[0]);
        let mut op = Vec::new();
        op.extend_from_slice(&combine);
        op.extend_from_slice(&join);
        t.extend_from_slice(&elem_master(ids::TRACK_OPERATION, &op));
        t
    };

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &bg));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &virt));

    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let op = dmx
        .track_operation(1)
        .expect("virtual track has TrackOperation");
    assert_eq!(op.planes.len(), 1, "malformed plane (no UID) dropped");
    assert_eq!(op.planes[0].plane_type, TrackPlaneType::Other(7));
    assert_eq!(op.planes[0].track.stream_index, Some(0));
    assert!(op.join_tracks.is_empty(), "zero TrackJoinUID dropped");
    assert!(!op.is_empty(), "operation still has a plane");
}

/// An ordinary file with no `TrackOperation` anywhere reports `None` for
/// every stream.
#[test]
fn no_track_operation_present() {
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &video_track(1, 0x1)));
    let bytes = assemble(&tracks_body);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    assert_eq!(dmx.track_operations().len(), 1);
    assert!(dmx.track_operation(0).is_none());
    // Out-of-range index is None too.
    assert!(dmx.track_operation(99).is_none());
}
