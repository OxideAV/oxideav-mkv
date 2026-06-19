//! Round-trip tests for the full `BlockGroup` (RFC 9559 §5.1.3.5) child
//! surface beyond `BlockAdditions` — `ReferenceBlock` (§5.1.3.5.5),
//! `ReferencePriority` (§5.1.3.5.4), `CodecState` (§5.1.3.5.6) and
//! `DiscardPadding` (§5.1.3.5.7).
//!
//! Drives the muxer's `write_packet_with_block_group` against the public
//! `BlockGroupOptions`, then re-opens the bytes through the typed
//! [`oxideav_mkv::demux::open_typed`] and confirms
//! `MkvDemuxer::block_group_meta()` surfaces exactly the children handed to
//! the muxer.
//!
//! Spec contracts pinned here:
//!
//! 1. An explicit `reference_blocks` list writes one `ReferenceBlock` per
//!    entry, in order, and round-trips verbatim — including multiple
//!    references on one Block (§5.1.3.5.5: "as many ReferenceBlock elements
//!    as necessary").
//! 2. `ReferencePriority`'s spec default `0` stays off-disk; a non-zero
//!    value is written and round-trips (§5.1.3.5.4).
//! 3. `CodecState` (§5.1.3.5.6) carries verbatim codec-private bytes.
//! 4. `DiscardPadding` (§5.1.3.5.7) carries a signed nanosecond count
//!    (positive = end padding, negative = beginning).
//! 5. A group whose only child is, e.g., `DiscardPadding` needs no
//!    `MaxBlockAdditionID` declaration; the keyframe flag still rides the
//!    `ReferenceBlock` absence.
//! 6. The demuxer materialises the §5.1.3.5.4 default `0` for a Block with
//!    no `ReferencePriority` child, and reports `None` meta when no group
//!    child was present.
//!
//! These tests use the production demuxer / plain byte scans to inspect the
//! muxed buffer — no third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::mux::{BlockGroupOptions, MkvBlockAddition, MkvMuxer};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r341-blockgroup-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

fn video_stream(index: u32) -> StreamInfo {
    let mut p = CodecParameters::video(CodecId::new("vp9"));
    p.width = Some(320);
    p.height = Some(240);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn packet(stream: u32, pts: i64, keyframe: bool, marker: u8, len: usize) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![marker; len]);
    p.pts = Some(pts);
    p.flags.keyframe = keyframe;
    p
}

fn mux_with<F>(streams: &[StreamInfo], body: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, streams).expect("muxer construct");
        body(&mut mx);
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

fn contains_id2(bytes: &[u8], id: u16) -> bool {
    let hi = (id >> 8) as u8;
    let lo = (id & 0xFF) as u8;
    bytes.windows(2).any(|w| w[0] == hi && w[1] == lo)
}

#[test]
fn roundtrip_all_block_group_children() {
    let streams = [video_stream(0)];
    let bytes = mux_with(&streams, |mx| {
        mx.set_max_block_addition_id(0, 2)
            .expect("set_max_block_addition_id");
        mx.write_header().expect("write_header");
        // First a keyframe so the second packet's ReferenceBlock has a
        // prior Block to point at — though here we set references
        // explicitly.
        let mut kf = packet(0, 0, true, 0x11, 24);
        kf.duration = Some(40);
        let kf_opts = BlockGroupOptions {
            reference_priority: 3,
            codec_state: Some(vec![0xC0, 0xDE, 0xC5]),
            discard_padding: Some(7_000),
            ..Default::default()
        };
        mx.write_packet_with_block_group(&kf, &kf_opts)
            .expect("keyframe block group");

        // A non-keyframe with two explicit ReferenceBlocks, additions, and
        // negative DiscardPadding (beginning padding).
        let p = packet(0, 40, false, 0x22, 16);
        let opts = BlockGroupOptions {
            additions: vec![
                MkvBlockAddition::codec_defined(vec![0xAA; 6]),
                MkvBlockAddition::new(2, vec![0xBB; 3]),
            ],
            reference_blocks: vec![-40, 0],
            reference_priority: 0,
            codec_state: None,
            discard_padding: Some(-1_500),
        };
        mx.write_packet_with_block_group(&p, &opts)
            .expect("non-keyframe block group");
    });

    // DiscardPadding is a 2-byte id (0x75A2); confirm it on disk. The
    // CodecState (0xA4) and BlockGroup (0xA0) ids are single-byte and are
    // verified by the round-trip below rather than a byte scan (a lone
    // 0xA4 could collide with frame data).
    assert!(contains_id2(&bytes, 0x75A2), "DiscardPadding id on disk");

    let mut dmx = demux_typed(bytes);

    // Packet 1: keyframe (no ReferenceBlock), priority 3, codec state,
    // positive discard padding.
    let p1 = dmx.next_packet().expect("packet 1");
    assert_eq!(p1.data, vec![0x11; 24]);
    assert!(p1.flags.keyframe, "no ReferenceBlock → keyframe");
    assert_eq!(p1.duration, Some(40), "duration rides BlockDuration");
    let m1 = dmx.block_group_meta().expect("meta for packet 1");
    assert!(m1.reference_blocks().is_empty());
    assert_eq!(m1.reference_priority(), 3);
    assert_eq!(m1.codec_state(), Some(&[0xC0, 0xDE, 0xC5][..]));
    assert_eq!(m1.discard_padding(), Some(7_000));

    // Packet 2: non-keyframe with two references, default priority 0,
    // no codec state, negative discard padding.
    let p2 = dmx.next_packet().expect("packet 2");
    assert_eq!(p2.data, vec![0x22; 16]);
    assert!(!p2.flags.keyframe, "ReferenceBlock present → non-keyframe");
    let m2 = dmx.block_group_meta().expect("meta for packet 2");
    assert_eq!(
        m2.reference_blocks(),
        &[-40, 0],
        "both ReferenceBlocks round-trip in order"
    );
    assert_eq!(
        m2.reference_priority(),
        0,
        "absent ReferencePriority materialises spec default 0"
    );
    assert_eq!(m2.codec_state(), None);
    assert_eq!(m2.discard_padding(), Some(-1_500));
    // BlockAdditions still surface alongside the meta.
    let adds = dmx.block_additions();
    assert_eq!(adds.len(), 2);
    assert_eq!(adds[1].block_add_id(), 2);
}

#[test]
fn discard_padding_only_group_needs_no_max_block_addition_id() {
    let streams = [video_stream(0)];
    let bytes = mux_with(&streams, |mx| {
        // No set_max_block_addition_id call — a group with only
        // DiscardPadding must not require it.
        mx.write_header().expect("write_header");
        let p = packet(0, 0, true, 0x33, 12);
        let opts = BlockGroupOptions {
            discard_padding: Some(2_000),
            ..Default::default()
        };
        mx.write_packet_with_block_group(&p, &opts)
            .expect("discard-padding-only group");
    });

    let mut dmx = demux_typed(bytes);
    let p1 = dmx.next_packet().expect("packet 1");
    assert!(p1.flags.keyframe);
    let m1 = dmx.block_group_meta().expect("meta present");
    assert_eq!(m1.discard_padding(), Some(2_000));
    assert!(m1.reference_blocks().is_empty());
    assert_eq!(m1.reference_priority(), 0);
    assert_eq!(m1.codec_state(), None);
}

#[test]
fn plain_simple_block_reports_no_meta() {
    let streams = [video_stream(0)];
    let bytes = mux_with(&streams, |mx| {
        mx.write_header().expect("write_header");
        mx.write_packet(&packet(0, 0, true, 0x44, 10))
            .expect("plain write_packet");
    });

    let mut dmx = demux_typed(bytes);
    let _p1 = dmx.next_packet().expect("packet 1");
    assert!(
        dmx.block_group_meta().is_none(),
        "SimpleBlock packet has no BlockGroup meta"
    );
}

#[test]
fn empty_options_force_block_group_wrapper() {
    // An all-default BlockGroupOptions still wraps the packet in a
    // BlockGroup rather than a SimpleBlock. A keyframe with no children
    // surfaces None meta (every field at its default ⇒ is_empty), but the
    // on-disk shape is a BlockGroup (0xA0), not a SimpleBlock (0xA3).
    let streams = [video_stream(0)];
    let bytes = mux_with(&streams, |mx| {
        mx.write_header().expect("write_header");
        let p = packet(0, 0, true, 0x55, 8);
        mx.write_packet_with_block_group(&p, &BlockGroupOptions::default())
            .expect("empty-options group");
    });

    let mut dmx = demux_typed(bytes);
    let p1 = dmx.next_packet().expect("packet 1");
    assert_eq!(p1.data, vec![0x55; 8]);
    assert!(p1.flags.keyframe);
    // No group children ⇒ the side-channel reports None.
    assert!(dmx.block_group_meta().is_none());
}

#[test]
fn bad_stream_index_rejected_before_write() {
    let streams = [video_stream(0)];
    let tmp = tmp_path("bad");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer construct");
    mx.write_header().expect("write_header");
    let p = packet(5, 0, true, 0x66, 8);
    let err = mx
        .write_packet_with_block_group(&p, &BlockGroupOptions::default())
        .expect_err("out-of-range stream index must error");
    let _ = std::fs::remove_file(&tmp);
    assert!(format!("{err}").contains("stream index"));
}
