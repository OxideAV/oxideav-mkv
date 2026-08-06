//! Integration tests for TrackOperation *application* (RFC 9559
//! §5.1.4.1.30 + §18.8) — the demuxer-side synthesis that turns a virtual
//! track's `TrackOperation` recipe into an actual packet stream, so a
//! reader can open the virtual track like any real one.
//!
//! §18.8: "In the case of TrackJoinBlocks, the Block elements (from
//! BlockGroup and SimpleBlock) of all the tracks SHOULD be used as if they
//! were defined for this new virtual Track." With application enabled
//! (`MkvDemuxer::set_apply_track_operations(true)`), every packet whose
//! stream is referenced by a virtual track's operation is followed by a
//! synthesised copy re-tagged with the virtual track's stream index;
//! `MkvDemuxer::virtual_packet_origin()` reports the provenance of each
//! synthesised packet. Storage order is preserved (§10 keeps Blocks in
//! coding order — a PTS re-sort would break decode).

use std::io::Cursor;

use oxideav_core::{Demuxer, Packet, ReadSeek};
use oxideav_mkv::demux::{MkvDemuxer, VirtualPacketOrigin, VirtualPacketRole};
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

fn elem_master(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

fn simple_block(track: u8, tc_offset: i16, keyframe: bool, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(if keyframe { 0x80 } else { 0x00 });
    body.extend_from_slice(payload);
    elem_master(ids::SIMPLE_BLOCK, &body)
}

/// A Xiph-laced SimpleBlock carrying the given frames.
fn xiph_laced_simple_block(track: u8, tc_offset: i16, frames: &[&[u8]]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(0x80 | 0x02); // keyframe + Xiph lacing
    body.push((frames.len() - 1) as u8);
    for f in &frames[..frames.len() - 1] {
        let mut sz = f.len();
        while sz >= 255 {
            body.push(255);
            sz -= 255;
        }
        body.push(sz as u8);
    }
    for f in frames {
        body.extend_from_slice(f);
    }
    elem_master(ids::SIMPLE_BLOCK, &body)
}

/// A minimal `BlockGroup` (Block + BlockAdditions with one BlockMore).
fn block_group_with_addition(
    track: u8,
    tc_offset: i16,
    payload: &[u8],
    addition: &[u8],
) -> Vec<u8> {
    let mut block = Vec::new();
    block.extend_from_slice(&write_vint(track as u64, 0));
    block.extend_from_slice(&tc_offset.to_be_bytes());
    block.push(0x00);
    block.extend_from_slice(payload);
    let mut more = Vec::new();
    more.extend_from_slice(&elem_uint(ids::BLOCK_ADD_ID, 1));
    more.extend_from_slice(&elem_master(ids::BLOCK_ADDITIONAL, addition));
    let additions = elem_master(ids::BLOCK_ADDITIONS, &elem_master(ids::BLOCK_MORE, &more));
    let mut bg = Vec::new();
    bg.extend_from_slice(&elem_master(ids::BLOCK, &block));
    bg.extend_from_slice(&additions);
    elem_master(ids::BLOCK_GROUP, &bg)
}

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
    elem_master(ids::INFO, &ib)
}

fn cluster(timecode: u64, blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::TIMECODE, timecode));
    for b in blocks {
        cb.extend_from_slice(b);
    }
    elem_master(ids::CLUSTER, &cb)
}

/// Assemble EBML header + Segment(Info, Tracks, clusters...) into a file.
fn assemble(tracks_body: &[u8], clusters: &[Vec<u8>]) -> Vec<u8> {
    let tracks = elem_master(ids::TRACKS, tracks_body);
    let mut seg = Vec::new();
    seg.extend_from_slice(&info());
    seg.extend_from_slice(&tracks);
    for c in clusters {
        seg.extend_from_slice(c);
    }
    let segment = elem_master(ids::SEGMENT, &seg);
    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

fn open(bytes: Vec<u8>) -> MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

/// Drain the demuxer, returning each packet with the origin the demuxer
/// reported for it.
fn drain(dmx: &mut MkvDemuxer) -> Vec<(Packet, Option<VirtualPacketOrigin>)> {
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                let origin = dmx.virtual_packet_origin();
                out.push((p, origin));
            }
            Err(oxideav_core::Error::Eof) => return out,
            Err(e) => panic!("unexpected demux error: {e:?}"),
        }
    }
}

const UID_A: u64 = 0xA1;
const UID_B: u64 = 0xB2;
const UID_V: u64 = 0xC3;

/// Tracks 1 (stream 0) + 2 (stream 1) joined into virtual track 3
/// (stream 2), with source Blocks interleaved across two clusters.
fn join_file() -> Vec<u8> {
    let ta = video_track(1, UID_A);
    let tb = video_track(2, UID_B);
    let virt = {
        let mut t = video_track(3, UID_V);
        t.extend_from_slice(&elem_master(
            ids::TRACK_OPERATION,
            &join_blocks(&[UID_A, UID_B]),
        ));
        t
    };
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &tb));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &virt));
    let c0 = cluster(
        0,
        &[
            simple_block(1, 0, true, &[0x10]),
            simple_block(2, 5, true, &[0x20]),
            simple_block(1, 10, false, &[0x11]),
        ],
    );
    let c1 = cluster(
        20,
        &[
            simple_block(2, 0, false, &[0x21]),
            simple_block(1, 5, false, &[0x12]),
        ],
    );
    assemble(&tracks_body, &[c0, c1])
}

/// With application off (the default), a virtual track yields no packets —
/// it is only described, exactly the pre-application behaviour.
#[test]
fn application_is_off_by_default() {
    let mut dmx = open(join_file());
    assert!(!dmx.applies_track_operations());
    assert!(dmx.virtual_packet_origin().is_none(), "no packet yet");
    let pkts = drain(&mut dmx);
    assert_eq!(pkts.len(), 5, "only the five real source packets");
    assert!(
        pkts.iter().all(|(p, _)| p.stream_index != 2),
        "virtual stream 2 yields nothing while application is off"
    );
    assert!(
        pkts.iter().all(|(_, o)| o.is_none()),
        "no packet reports a virtual origin"
    );
}

/// TrackJoinBlocks application: the virtual stream carries every source
/// Block, in storage order, with bytes / timestamps / keyframe flags
/// preserved, and each synthesised packet reports its provenance.
#[test]
fn join_blocks_synthesises_the_virtual_stream() {
    let mut dmx = open(join_file());
    dmx.set_apply_track_operations(true);
    assert!(dmx.applies_track_operations());
    let pkts = drain(&mut dmx);

    // Every real packet is followed by its virtual copy: 5 sources -> 10.
    assert_eq!(pkts.len(), 10, "five real + five synthesised packets");

    let virt: Vec<_> = pkts.iter().filter(|(p, _)| p.stream_index == 2).collect();
    let real: Vec<_> = pkts.iter().filter(|(p, _)| p.stream_index != 2).collect();
    assert_eq!(virt.len(), 5, "one copy per source Block");
    assert_eq!(real.len(), 5, "source tracks keep their own packets");
    assert!(
        real.iter().all(|(_, o)| o.is_none()),
        "real packets carry no origin"
    );

    // The virtual stream is the storage-order merge of both sources:
    // (pts, payload, source stream). The copy's flags mirror the source
    // packet's flags verbatim (checked against `real` below).
    let expect: [(i64, u8, u32); 5] = [
        (0, 0x10, 0),
        (5, 0x20, 1),
        (10, 0x11, 0),
        (20, 0x21, 1),
        (25, 0x12, 0),
    ];
    for (i, ((p, o), (pts, payload, src))) in virt.iter().zip(expect.iter()).enumerate() {
        assert_eq!(p.pts, Some(*pts), "virtual packet {i} pts");
        assert_eq!(p.data, vec![*payload], "virtual packet {i} bytes");
        assert_eq!(
            p.flags.keyframe, real[i].0.flags.keyframe,
            "virtual packet {i} mirrors its source's keyframe flag"
        );
        assert_eq!(
            *o,
            Some(VirtualPacketOrigin {
                virtual_stream: 2,
                source_stream: *src,
                role: VirtualPacketRole::Joined,
            }),
            "virtual packet {i} origin"
        );
    }

    // Each copy directly follows its source packet, so the interleave is
    // real0, virt0, real1, virt1, ...
    for (i, (p, o)) in pkts.iter().enumerate() {
        if i % 2 == 0 {
            assert!(o.is_none(), "even positions are real packets");
            assert_eq!(pkts[i + 1].0.data, p.data, "copy follows its source");
        } else {
            assert!(o.is_some(), "odd positions are synthesised copies");
        }
    }
}

/// Dangling and self references synthesise nothing; a second virtual track
/// consuming the same source gets its own copies, in ascending
/// virtual-stream order.
#[test]
fn dangling_self_and_multiple_consumers() {
    const UID_W: u64 = 0xD4;
    let ta = video_track(1, UID_A);
    // Virtual track V (stream 1) joins A + a dangling UID + itself.
    let v1 = {
        let mut t = video_track(2, UID_V);
        t.extend_from_slice(&elem_master(
            ids::TRACK_OPERATION,
            &join_blocks(&[UID_A, 0xDEAD, UID_V]),
        ));
        t
    };
    // Virtual track W (stream 2) joins A too.
    let v2 = {
        let mut t = video_track(3, UID_W);
        t.extend_from_slice(&elem_master(ids::TRACK_OPERATION, &join_blocks(&[UID_A])));
        t
    };
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &v1));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &v2));
    let c0 = cluster(0, &[simple_block(1, 0, true, &[0x77])]);
    let bytes = assemble(&tracks_body, &[c0]);

    let mut dmx = open(bytes);
    dmx.set_apply_track_operations(true);
    let pkts = drain(&mut dmx);
    assert_eq!(pkts.len(), 3, "one real + one copy per virtual consumer");
    assert_eq!(pkts[0].0.stream_index, 0);
    assert!(pkts[0].1.is_none());
    // Copies in ascending virtual-stream order.
    assert_eq!(pkts[1].0.stream_index, 1);
    assert_eq!(
        pkts[1].1,
        Some(VirtualPacketOrigin {
            virtual_stream: 1,
            source_stream: 0,
            role: VirtualPacketRole::Joined,
        })
    );
    assert_eq!(pkts[2].0.stream_index, 2);
    assert_eq!(
        pkts[2].1,
        Some(VirtualPacketOrigin {
            virtual_stream: 2,
            source_stream: 0,
            role: VirtualPacketRole::Joined,
        })
    );
    // All three carry the same bytes.
    assert!(pkts.iter().all(|(p, _)| p.data == vec![0x77]));
}

/// A laced source Block synthesises one copy per de-laced frame, each
/// directly after its source frame, timestamps included.
#[test]
fn laced_source_block_copies_every_frame() {
    let ta = video_track(1, UID_A);
    let virt = {
        let mut t = video_track(2, UID_V);
        t.extend_from_slice(&elem_master(ids::TRACK_OPERATION, &join_blocks(&[UID_A])));
        t
    };
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &virt));
    let c0 = cluster(
        0,
        &[xiph_laced_simple_block(
            1,
            0,
            &[&[0x01, 0x02], &[0x03], &[0x04, 0x05, 0x06]],
        )],
    );
    let mut dmx = open(assemble(&tracks_body, &[c0]));
    dmx.set_apply_track_operations(true);
    let pkts = drain(&mut dmx);
    assert_eq!(pkts.len(), 6, "three frames, each with a copy");
    let payloads: Vec<(u32, Vec<u8>)> = pkts
        .iter()
        .map(|(p, _)| (p.stream_index, p.data.clone()))
        .collect();
    assert_eq!(
        payloads,
        vec![
            (0, vec![0x01, 0x02]),
            (1, vec![0x01, 0x02]),
            (0, vec![0x03]),
            (1, vec![0x03]),
            (0, vec![0x04, 0x05, 0x06]),
            (1, vec![0x04, 0x05, 0x06]),
        ],
        "per-frame interleave: real frame then its copy"
    );
}

/// A synthesised copy shares its source Block's `BlockAdditions` side
/// channel — the additions attach to the Block, and the copy *is* that
/// Block used as if it were the virtual track's.
#[test]
fn virtual_copy_shares_block_additions() {
    let ta = video_track(1, UID_A);
    let virt = {
        let mut t = video_track(2, UID_V);
        t.extend_from_slice(&elem_master(ids::TRACK_OPERATION, &join_blocks(&[UID_A])));
        t
    };
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &virt));
    let c0 = cluster(
        0,
        &[block_group_with_addition(1, 0, &[0x42], &[0xAA, 0xBB])],
    );
    let mut dmx = open(assemble(&tracks_body, &[c0]));
    dmx.set_apply_track_operations(true);

    let real = dmx.next_packet().expect("real packet");
    assert_eq!(real.stream_index, 0);
    assert!(dmx.virtual_packet_origin().is_none());
    let adds: Vec<Vec<u8>> = dmx
        .block_additions()
        .iter()
        .map(|a| a.data().to_vec())
        .collect();
    assert_eq!(adds, vec![vec![0xAA, 0xBB]]);

    let copy = dmx.next_packet().expect("synthesised packet");
    assert_eq!(copy.stream_index, 1);
    assert_eq!(copy.data, vec![0x42]);
    assert_eq!(
        dmx.virtual_packet_origin(),
        Some(VirtualPacketOrigin {
            virtual_stream: 1,
            source_stream: 0,
            role: VirtualPacketRole::Joined,
        })
    );
    let copy_adds: Vec<Vec<u8>> = dmx
        .block_additions()
        .iter()
        .map(|a| a.data().to_vec())
        .collect();
    assert_eq!(
        copy_adds,
        vec![vec![0xAA, 0xBB]],
        "the copy shares the Block's additions"
    );
}

/// The toggle can be flipped mid-stream: Blocks de-laced while it was off
/// synthesise nothing; Blocks de-laced after enabling do.
#[test]
fn toggle_mid_stream_affects_later_blocks_only() {
    let mut dmx = open(join_file());
    // First Block read with application off.
    let p0 = dmx.next_packet().expect("first packet");
    assert_eq!(p0.data, vec![0x10]);
    dmx.set_apply_track_operations(true);
    let rest = drain(&mut dmx);
    // Remaining 4 source Blocks each get a copy -> 8 packets; the first
    // Block's copy was never synthesised.
    assert_eq!(rest.len(), 8);
    assert!(
        rest.iter()
            .filter(|(p, _)| p.stream_index == 2)
            .all(|(p, _)| p.data != vec![0x10]),
        "the pre-toggle Block was not retroactively copied"
    );
    // And disabling again stops synthesis (nothing further queued).
    let mut dmx = open(join_file());
    dmx.set_apply_track_operations(true);
    let p0 = dmx.next_packet().expect("first real packet");
    assert_eq!(p0.stream_index, 0);
    let p1 = dmx.next_packet().expect("its copy");
    assert_eq!(p1.stream_index, 2);
    dmx.set_apply_track_operations(false);
    let rest = drain(&mut dmx);
    // The remaining 4 source Blocks arrive without copies... except any
    // packet already de-laced into the queue alongside p0/p1 (none here —
    // each SimpleBlock is unlaced, queued one Block at a time).
    assert_eq!(rest.len(), 4);
    assert!(rest.iter().all(|(p, _)| p.stream_index != 2));
}

/// An empty `TrackOperation` (no planes, no joins) synthesises nothing —
/// the track is surfaced as described but yields no packets.
#[test]
fn empty_operation_synthesises_nothing() {
    let ta = video_track(1, UID_A);
    let virt = {
        let mut t = video_track(2, UID_V);
        t.extend_from_slice(&elem_master(ids::TRACK_OPERATION, &[]));
        t
    };
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &virt));
    let c0 = cluster(0, &[simple_block(1, 0, true, &[0x01])]);
    let mut dmx = open(assemble(&tracks_body, &[c0]));
    dmx.set_apply_track_operations(true);
    let pkts = drain(&mut dmx);
    assert_eq!(pkts.len(), 1, "no synthesis from an empty operation");
    assert_eq!(pkts[0].0.stream_index, 0);
}

// --- TrackCombinePlanes application ---------------------------------------

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

/// Stereo 3D: tracks 1 (left eye) + 2 (right eye) combined into virtual
/// track 3, one frame per eye per time instant, interleaved left-first in
/// storage order.
fn stereo_file() -> Vec<u8> {
    let left = video_track(1, UID_A);
    let right = video_track(2, UID_B);
    let virt = {
        let mut t = video_track(3, UID_V);
        let op = combine_planes(&[
            (UID_A, ids::TRACK_PLANE_TYPE_LEFT_EYE),
            (UID_B, ids::TRACK_PLANE_TYPE_RIGHT_EYE),
        ]);
        t.extend_from_slice(&elem_master(ids::TRACK_OPERATION, &op));
        t
    };
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &left));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &right));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &virt));
    // Two time instants; each stores the left-eye Block first, then the
    // right-eye Block at the same timestamp (the writer's interleave).
    let c0 = cluster(
        0,
        &[
            simple_block(1, 0, true, &[0x4C]),
            simple_block(2, 0, true, &[0x52]),
            simple_block(1, 40, false, &[0x4D]),
            simple_block(2, 40, false, &[0x53]),
        ],
    );
    assemble(&tracks_body, &[c0])
}

/// TrackCombinePlanes application: the virtual 3D stream carries both
/// planes' Blocks in the writer's interleave, each synthesised packet
/// tagged with its plane role so the caller can route it to the right
/// decoder (§18.8: each "sub" track needs its own decoder before the
/// operation is applied).
#[test]
fn combine_planes_synthesises_the_stereo_stream() {
    use oxideav_mkv::demux::TrackPlaneType;

    let mut dmx = open(stereo_file());
    dmx.set_apply_track_operations(true);
    let pkts = drain(&mut dmx);
    assert_eq!(pkts.len(), 8, "four real + four synthesised packets");

    let virt: Vec<_> = pkts.iter().filter(|(p, _)| p.stream_index == 2).collect();
    assert_eq!(virt.len(), 4, "one copy per plane Block");

    // (pts, payload, plane role) in storage order — same-timestamp planes
    // keep the writer's left-first interleave.
    let expect = [
        (0i64, 0x4Cu8, TrackPlaneType::LeftEye),
        (0, 0x52, TrackPlaneType::RightEye),
        (40, 0x4D, TrackPlaneType::LeftEye),
        (40, 0x53, TrackPlaneType::RightEye),
    ];
    for (i, ((p, o), (pts, payload, plane))) in virt.iter().zip(expect.iter()).enumerate() {
        assert_eq!(p.pts, Some(*pts), "plane packet {i} pts");
        assert_eq!(p.data, vec![*payload], "plane packet {i} bytes");
        let o = o.expect("synthesised packet has an origin");
        assert_eq!(o.virtual_stream, 2);
        assert_eq!(
            o.role,
            VirtualPacketRole::Plane(*plane),
            "plane packet {i} role"
        );
    }
    // Left-eye copies come from stream 0, right-eye from stream 1.
    assert_eq!(virt[0].1.unwrap().source_stream, 0);
    assert_eq!(virt[1].1.unwrap().source_stream, 1);
}

/// A background plane and a forward-compat `Other` plane type both apply,
/// and a single `TrackOperation` carrying planes *and* joins queues the
/// plane copies first (on-disk reference order within one operation).
#[test]
fn background_other_planes_and_mixed_operation_order() {
    use oxideav_mkv::demux::TrackPlaneType;

    let src = video_track(1, UID_A);
    let virt = {
        let mut t = video_track(2, UID_V);
        // Planes: background + an unregistered type 7, both naming the
        // same source; plus a TrackJoinBlocks naming it a third time.
        let mut op = Vec::new();
        op.extend_from_slice(&combine_planes(&[
            (UID_A, ids::TRACK_PLANE_TYPE_BACKGROUND),
            (UID_A, 7),
        ]));
        op.extend_from_slice(&join_blocks(&[UID_A]));
        t.extend_from_slice(&elem_master(ids::TRACK_OPERATION, &op));
        t
    };
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &src));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &virt));
    let c0 = cluster(0, &[simple_block(1, 0, true, &[0x99])]);
    let mut dmx = open(assemble(&tracks_body, &[c0]));
    dmx.set_apply_track_operations(true);
    let pkts = drain(&mut dmx);

    // One real packet + one copy per reference (2 planes + 1 join).
    assert_eq!(pkts.len(), 4);
    assert!(pkts[0].1.is_none());
    let roles: Vec<VirtualPacketRole> = pkts[1..].iter().map(|(_, o)| o.unwrap().role).collect();
    assert_eq!(
        roles,
        vec![
            VirtualPacketRole::Plane(TrackPlaneType::Background),
            VirtualPacketRole::Plane(TrackPlaneType::Other(7)),
            VirtualPacketRole::Joined,
        ],
        "planes before joins, each in on-disk order"
    );
    assert!(pkts[1..]
        .iter()
        .all(|(p, _)| p.stream_index == 1 && p.data == vec![0x99]));
}

// --- Virtual-track seek (Cues union, §18.8) -------------------------------

fn cues(entries: &[(u64, u64, u64)]) -> Vec<u8> {
    // entries: (track_number, time_ticks, cluster_offset)
    let mut body = Vec::new();
    for &(track, time, off) in entries {
        let mut ctp = Vec::new();
        ctp.extend_from_slice(&elem_uint(ids::CUE_TRACK, track));
        ctp.extend_from_slice(&elem_uint(ids::CUE_CLUSTER_POSITION, off));
        let mut cp = Vec::new();
        cp.extend_from_slice(&elem_uint(ids::CUE_TIME, time));
        cp.extend_from_slice(&elem_master(ids::CUE_TRACK_POSITIONS, &ctp));
        body.extend_from_slice(&elem_master(ids::CUE_POINT, &cp));
    }
    elem_master(ids::CUES, &body)
}

/// `join_file` plus a trailing Cues element indexing only the two *source*
/// tracks (the common layout — the virtual track has no Cues rows).
fn join_file_with_cues() -> Vec<u8> {
    let ta = video_track(1, UID_A);
    let tb = video_track(2, UID_B);
    let virt = {
        let mut t = video_track(3, UID_V);
        t.extend_from_slice(&elem_master(
            ids::TRACK_OPERATION,
            &join_blocks(&[UID_A, UID_B]),
        ));
        t
    };
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &tb));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &virt));
    let tracks = elem_master(ids::TRACKS, &tracks_body);
    let info = info();
    let c0 = cluster(
        0,
        &[
            simple_block(1, 0, true, &[0x10]),
            simple_block(2, 5, true, &[0x20]),
            simple_block(1, 10, false, &[0x11]),
        ],
    );
    let c1 = cluster(
        20,
        &[
            simple_block(2, 0, false, &[0x21]),
            simple_block(1, 5, false, &[0x12]),
        ],
    );
    // Segment-relative cluster offsets (Cues sit after the Clusters, so
    // the offsets are known when the index is built).
    let off_c0 = (info.len() + tracks.len()) as u64;
    let off_c1 = off_c0 + c0.len() as u64;
    let cues = cues(&[
        (1, 0, off_c0),
        (1, 25, off_c1),
        (2, 5, off_c0),
        (2, 20, off_c1),
    ]);
    let mut seg = Vec::new();
    seg.extend_from_slice(&info);
    seg.extend_from_slice(&tracks);
    seg.extend_from_slice(&c0);
    seg.extend_from_slice(&c1);
    seg.extend_from_slice(&cues);
    let segment = elem_master(ids::SEGMENT, &seg);
    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

/// With application off, seeking the virtual stream keeps the historical
/// behaviour: the file has no Cues rows for the virtual track's number, so
/// the strict path reports Unsupported.
#[test]
fn virtual_seek_without_application_is_unsupported() {
    let mut dmx = open(join_file_with_cues());
    match dmx.seek_to(2, 20) {
        Err(oxideav_core::Error::Unsupported { .. }) => {}
        other => panic!("expected Unsupported, got {other:?}"),
    }
    // The source tracks seek fine either way.
    assert_eq!(dmx.seek_to(0, 25).expect("source seek"), 25);
}

/// §18.8 Cues-union fallback: seeking the virtual stream resolves through
/// the *source* tracks' Cues, landing conservatively on the earliest
/// per-source best cluster so no source's Blocks at/after the target are
/// skipped.
#[test]
fn virtual_seek_unions_the_source_cues() {
    let mut dmx = open(join_file_with_cues());
    dmx.set_apply_track_operations(true);

    // Target 25: track 1's best cue is (25, c1), track 2's is (20, c1) —
    // both in cluster 1; the seek lands there.
    let landed = dmx.seek_to(2, 25).expect("virtual seek");
    assert_eq!(landed, 25, "landed on the first candidate's cue time");
    assert!(
        dmx.virtual_packet_origin().is_none(),
        "seek invalidates the last virtual origin"
    );
    let pkts = drain(&mut dmx);
    let virt: Vec<_> = pkts.iter().filter(|(p, _)| p.stream_index == 2).collect();
    assert_eq!(virt.len(), 2, "cluster 1 holds two source Blocks");
    assert_eq!(virt[0].0.data, vec![0x21]);
    assert_eq!(virt[0].0.pts, Some(20));
    assert_eq!(virt[1].0.data, vec![0x12]);
    assert_eq!(virt[1].0.pts, Some(25));

    // Target 20: track 1's best cue is (0, c0) — earlier cluster than
    // track 2's (20, c1). The union lands on c0 (the conservative pick):
    // every source Block at/after the target is still reachable.
    let landed = dmx.seek_to(2, 20).expect("virtual seek");
    assert_eq!(landed, 0, "conservative landing on the earlier cluster");
    let pkts = drain(&mut dmx);
    let virt: Vec<_> = pkts.iter().filter(|(p, _)| p.stream_index == 2).collect();
    assert_eq!(virt.len(), 5, "the whole merged stream from cluster 0 on");
}

/// A virtual track that *does* carry its own Cues rows seeks through them
/// (the union fallback never engages).
#[test]
fn virtual_track_with_own_cues_uses_them() {
    // Rebuild join_file_with_cues but with a track-3 cue row pointing at
    // cluster 1.
    let ta = video_track(1, UID_A);
    let tb = video_track(2, UID_B);
    let virt = {
        let mut t = video_track(3, UID_V);
        t.extend_from_slice(&elem_master(
            ids::TRACK_OPERATION,
            &join_blocks(&[UID_A, UID_B]),
        ));
        t
    };
    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &ta));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &tb));
    tracks_body.extend_from_slice(&elem_master(ids::TRACK_ENTRY, &virt));
    let tracks = elem_master(ids::TRACKS, &tracks_body);
    let info = info();
    let c0 = cluster(0, &[simple_block(1, 0, true, &[0x10])]);
    let c1 = cluster(
        20,
        &[
            simple_block(2, 0, false, &[0x21]),
            simple_block(1, 5, false, &[0x12]),
        ],
    );
    let off_c0 = (info.len() + tracks.len()) as u64;
    let off_c1 = off_c0 + c0.len() as u64;
    // §18.8: the virtual track's Cues SHOULD be the union of its sources'
    // — this file indexes the virtual track directly.
    let cues = cues(&[(1, 0, off_c0), (3, 20, off_c1)]);
    let mut seg = Vec::new();
    seg.extend_from_slice(&info);
    seg.extend_from_slice(&tracks);
    seg.extend_from_slice(&c0);
    seg.extend_from_slice(&c1);
    seg.extend_from_slice(&cues);
    let segment = elem_master(ids::SEGMENT, &seg);
    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);

    let mut dmx = open(out);
    dmx.set_apply_track_operations(true);
    let landed = dmx.seek_to(2, 30).expect("virtual seek via own cues");
    assert_eq!(landed, 20, "landed on the virtual track's own cue");
    let pkts = drain(&mut dmx);
    let virt: Vec<_> = pkts.iter().filter(|(p, _)| p.stream_index == 2).collect();
    assert_eq!(virt.len(), 2, "cluster 1's two source Blocks");
}
