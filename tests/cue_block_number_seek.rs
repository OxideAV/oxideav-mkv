//! Demux-side `CueBlockNumber` seek fallback (RFC 9559 §5.1.5.1.2.5).
//!
//! When a Cues entry carries a `CueBlockNumber` ("Number of the Block in
//! the specified Cluster") but NO `CueRelativePosition` (§5.1.5.1.2.3),
//! `seek_to` must still land on the exact Block — it walks the Cluster
//! body counting `SimpleBlock` / `BlockGroup` elements and stops at the
//! n-th one. These tests hand-build a one-Cluster MKV with three blocks
//! and a Cues element whose only positional hint is `CueBlockNumber`.
//!
//! `CueBlockNumber` is 1-based (`range: not 0`): block 1 is the first
//! `SimpleBlock` in the Cluster, regardless of the leading `Timestamp`.

use std::io::Cursor;

use oxideav_core::ReadSeek;

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

fn u64_fixed8(id: u32, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(8, 0));
    out.extend_from_slice(&value.to_be_bytes());
    out
}

fn simple_block(track: u8, tc_offset: i16, keyframe: bool, payload: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(if keyframe { 0x80 } else { 0x00 });
    body.push(payload);
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(ids::SIMPLE_BLOCK));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(&body);
    out
}

/// Build a self-contained MKV with ONE cluster carrying THREE
/// SimpleBlocks (payloads 0xAA/0xBB/0xCC at 0/1/2 ms) plus a Cues
/// element whose single entry carries `CueBlockNumber = block_number`
/// (and CueTime = cue_time) but NO `CueRelativePosition`.
fn build_fixture(block_number: u64, cue_time: u64) -> Vec<u8> {
    let mut ebml_body = Vec::new();
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    ebml_body.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    let ebml_header = elem_master(ids::EBML_HEADER, &ebml_body);

    let mut info_body = Vec::new();
    info_body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info_body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 3.0));
    info_body.extend_from_slice(&elem_str(ids::MUXING_APP, "oxideav-test"));
    info_body.extend_from_slice(&elem_str(ids::WRITING_APP, "oxideav-test"));
    let info = elem_master(ids::INFO, &info_body);

    let mut track_body = Vec::new();
    track_body.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_UID, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    track_body.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio_body.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio_body.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    track_body.extend_from_slice(&elem_master(ids::AUDIO, &audio_body));
    let track_entry = elem_master(ids::TRACK_ENTRY, &track_body);
    let tracks = elem_master(ids::TRACKS, &track_entry);

    let timecode_elem = elem_uint(ids::TIMECODE, 0);
    let block_a = simple_block(1, 0, true, 0xAA);
    let block_b = simple_block(1, 1, true, 0xBB);
    let block_c = simple_block(1, 2, true, 0xCC);
    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&timecode_elem);
    cluster_body.extend_from_slice(&block_a);
    cluster_body.extend_from_slice(&block_b);
    cluster_body.extend_from_slice(&block_c);
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    // Cues: ONE entry, CueBlockNumber only (no CueRelativePosition).
    let build_cues = |cluster_offset: u64| -> Vec<u8> {
        let mut ctp_body = Vec::new();
        ctp_body.extend_from_slice(&elem_uint(ids::CUE_TRACK, 1));
        ctp_body.extend_from_slice(&u64_fixed8(ids::CUE_CLUSTER_POSITION, cluster_offset));
        ctp_body.extend_from_slice(&u64_fixed8(ids::CUE_BLOCK_NUMBER, block_number));
        let ctp = elem_master(ids::CUE_TRACK_POSITIONS, &ctp_body);
        let mut cp_body = Vec::new();
        cp_body.extend_from_slice(&elem_uint(ids::CUE_TIME, cue_time));
        cp_body.extend_from_slice(&ctp);
        let cp = elem_master(ids::CUE_POINT, &cp_body);
        elem_master(ids::CUES, &cp)
    };

    let cues_bytes = build_cues(0);
    let cluster_offset_rel = (info.len() + tracks.len() + cues_bytes.len()) as u64;
    let cues_bytes = build_cues(cluster_offset_rel);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&cues_bytes);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header);
    out.extend_from_slice(&segment);
    out
}

#[test]
fn cue_block_number_lands_on_second_block() {
    // CueBlockNumber = 2 must place us on Block B (0xBB), not Block A.
    let bytes = build_fixture(2, 1);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    let landed = dmx.seek_to(0, 1).expect("seek to t=1");
    assert_eq!(landed, 1, "should land on the cue's CueTime");
    let pkt = dmx.next_packet().expect("packet after seek");
    assert_eq!(
        pkt.data,
        vec![0xBB],
        "CueBlockNumber=2 must place us on Block B, not Block A"
    );
}

#[test]
fn cue_block_number_lands_on_third_block() {
    let bytes = build_fixture(3, 2);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    let landed = dmx.seek_to(0, 2).expect("seek to t=2");
    assert_eq!(landed, 2);
    let pkt = dmx.next_packet().expect("packet after seek");
    assert_eq!(
        pkt.data,
        vec![0xCC],
        "CueBlockNumber=3 should reach Block C"
    );
}

#[test]
fn cue_block_number_one_is_first_block() {
    // CueBlockNumber = 1 is the first SimpleBlock (Block A), even though
    // the Timestamp child precedes it in the Cluster body.
    let bytes = build_fixture(1, 0);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    let landed = dmx.seek_to(0, 0).expect("seek to t=0");
    assert_eq!(landed, 0);
    let pkt = dmx.next_packet().expect("packet after seek");
    assert_eq!(pkt.data, vec![0xAA]);
}

#[test]
fn cue_block_number_out_of_range_falls_back_gracefully() {
    // A block number past the end of the cluster must degrade to the
    // legacy "scan from cluster start" path — no panic, the first packet
    // is just Block A.
    let bytes = build_fixture(99, 0);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open");
    let landed = dmx.seek_to(0, 0).expect("seek to t=0 should still succeed");
    assert_eq!(landed, 0);
    let pkt = dmx.next_packet().expect("packet after seek");
    assert_eq!(pkt.data, vec![0xAA]);
}

#[test]
fn cue_block_number_surfaces_on_typed_cue_points() {
    // The typed cue_points() view must also carry the block_number even
    // when CueRelativePosition is absent.
    let bytes = build_fixture(2, 1);
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("open_typed");
    let cps = dmx.cue_points();
    assert_eq!(cps.len(), 1);
    let ctp = &cps[0].track_positions[0];
    assert_eq!(ctp.block_number, Some(2));
    assert_eq!(ctp.relative_position, None);
}
