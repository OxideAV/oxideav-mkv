//! Round-trip tests for the muxer's `BlockAdditions` write path
//! (RFC 9559 §5.1.3.5.2 — `BlockGroup > BlockAdditions > BlockMore >
//! {BlockAdditional, BlockAddID}`) and the `MaxBlockAdditionID`
//! TrackEntry declaration (§5.1.4.1.16).
//!
//! Drives `MkvMuxer::set_max_block_addition_id` +
//! `MkvMuxer::write_packet_with_additions` against the public Muxer
//! trait, then re-opens the bytes through the typed
//! [`oxideav_mkv::demux::open_typed`] and confirms
//! `MkvDemuxer::block_additions()` surfaces exactly the payloads handed
//! to the muxer.
//!
//! Spec contracts pinned here:
//!
//! 1. Additions round-trip verbatim, in slice order, with the
//!    §5.1.3.5.2.3 default (`BlockAddID == 1` omitted on disk,
//!    materialised back on read) and explicit ids `>= 2` both covered.
//! 2. A keyframe packet's `BlockGroup` carries no `ReferenceBlock`
//!    (§5.1.3.5.5 — keyframe-ness is the element's absence); a
//!    non-keyframe packet writes one, and the demuxer's inferred
//!    keyframe flag round-trips both ways.
//! 3. The packet duration rides `BlockDuration` (§5.1.3.5.3) — a
//!    `SimpleBlock` could not have carried it.
//! 4. `MaxBlockAdditionID` lands in the TrackEntry and gates the write:
//!    an undeclared stream, `BlockAddID == 0`, an id above the declared
//!    maximum, and duplicate ids are all rejected before any byte is
//!    written (§5.1.4.1.16 + §5.1.3.5.2.3).
//! 5. An empty additions slice degrades to plain `write_packet`
//!    behaviour (no `BlockAdditions` master, no declaration needed).
//! 6. With lacing enabled, an additions packet flushes the track's
//!    pending lace first so packet order is preserved end-to-end.
//!
//! These tests use the production demuxer / plain byte scans to inspect
//! the muxed buffer — no third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mkv::mux::{LacingMode, MkvBlockAddition, MkvMuxer};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r283-blockadd-{}-{}-{n}.mkv",
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

fn audio_stream(index: u32) -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p.sample_format = Some(SampleFormat::S16);
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

/// Run `body` against a fresh muxer over the given streams and return
/// the muxed bytes.
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

/// True when the 2-byte element id appears anywhere in `bytes`. Packet
/// payloads in these tests repeat a single marker byte, so a window of
/// two *different* bytes can never originate inside frame data.
fn contains_id2(bytes: &[u8], id: u16) -> bool {
    let hi = (id >> 8) as u8;
    let lo = (id & 0xFF) as u8;
    bytes.windows(2).any(|w| w[0] == hi && w[1] == lo)
}

#[test]
fn roundtrip_additions_duration_and_max_id() {
    let streams = [video_stream(0)];
    let bytes = mux_with(&streams, |mx| {
        mx.set_max_block_addition_id(0, 4)
            .expect("set_max_block_addition_id");
        mx.write_header().expect("write_header");
        let mut p = packet(0, 0, true, 0x11, 32);
        p.duration = Some(40);
        mx.write_packet_with_additions(
            &p,
            &[
                MkvBlockAddition::codec_defined(vec![0xAA; 8]),
                MkvBlockAddition::new(4, vec![0xBB; 4]),
            ],
        )
        .expect("write_packet_with_additions");
        mx.write_packet(&packet(0, 40, false, 0x22, 16))
            .expect("plain write_packet");
    });
    // BlockAdditions (0x75A1) + MaxBlockAdditionID (0x55EE) on disk.
    assert!(contains_id2(&bytes, 0x75A1), "BlockAdditions id on disk");
    assert!(
        contains_id2(&bytes, 0x55EE),
        "MaxBlockAdditionID id on disk"
    );

    let mut dmx = demux_typed(bytes);
    assert_eq!(dmx.max_block_addition_id(0), Some(4));

    let p1 = dmx.next_packet().expect("packet 1");
    assert_eq!(p1.data, vec![0x11; 32]);
    assert!(p1.flags.keyframe, "no ReferenceBlock → keyframe");
    assert_eq!(
        p1.duration,
        Some(40),
        "duration must ride BlockDuration (§5.1.3.5.3)"
    );
    let adds = dmx.block_additions();
    assert_eq!(adds.len(), 2);
    assert_eq!(adds[0].block_add_id(), 1);
    assert!(adds[0].is_codec_defined());
    assert_eq!(adds[0].data(), &[0xAA; 8][..]);
    assert_eq!(adds[1].block_add_id(), 4);
    assert_eq!(adds[1].data(), &[0xBB; 4][..]);

    let p2 = dmx.next_packet().expect("packet 2");
    assert_eq!(p2.data, vec![0x22; 16]);
    assert!(
        dmx.block_additions().is_empty(),
        "plain SimpleBlock packet surfaces no additions"
    );
}

#[test]
fn non_keyframe_round_trips_via_reference_block() {
    // §5.1.3.5.5: a plain Block has no KEY flag bit, so the muxer must
    // write a ReferenceBlock for a non-keyframe — the demuxer infers
    // keyframe-ness from the element's absence/presence.
    let streams = [video_stream(0)];
    let bytes = mux_with(&streams, |mx| {
        mx.set_max_block_addition_id(0, 1).expect("declare");
        mx.write_header().expect("write_header");
        mx.write_packet_with_additions(
            &packet(0, 0, true, 0x11, 8),
            &[MkvBlockAddition::codec_defined(vec![0x33; 2])],
        )
        .expect("keyframe group");
        mx.write_packet_with_additions(
            &packet(0, 40, false, 0x22, 8),
            &[MkvBlockAddition::codec_defined(vec![0x44; 2])],
        )
        .expect("non-keyframe group");
    });
    let mut dmx = demux_typed(bytes);
    let p1 = dmx.next_packet().expect("packet 1");
    assert!(p1.flags.keyframe);
    assert_eq!(dmx.block_additions()[0].data(), &[0x33, 0x33]);
    let p2 = dmx.next_packet().expect("packet 2");
    assert!(
        !p2.flags.keyframe,
        "ReferenceBlock presence must demux as non-keyframe"
    );
    assert_eq!(dmx.block_additions()[0].data(), &[0x44, 0x44]);
}

#[test]
fn empty_additions_slice_degrades_to_plain_write_packet() {
    // No declaration needed, no BlockAdditions master written — the
    // §5.1.3.5.2.1 BlockMore child is mandatory inside the master, so
    // an empty one would be malformed.
    let streams = [video_stream(0)];
    let bytes = mux_with(&streams, |mx| {
        mx.write_header().expect("write_header");
        mx.write_packet_with_additions(&packet(0, 0, true, 0x11, 8), &[])
            .expect("empty additions degrade to write_packet");
    });
    assert!(
        !contains_id2(&bytes, 0x75A1),
        "no BlockAdditions master may reach disk"
    );
    let mut dmx = demux_typed(bytes);
    let p = dmx.next_packet().expect("packet");
    assert_eq!(p.data, vec![0x11; 8]);
    assert!(dmx.block_additions().is_empty());
}

#[test]
fn validation_rejects_spec_violations_before_writing() {
    let streams = [video_stream(0), audio_stream(1)];
    let tmp = tmp_path("rej");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer construct");

    // Setter: out-of-range stream index.
    assert!(mx.set_max_block_addition_id(2, 1).is_err());
    mx.set_max_block_addition_id(0, 2).expect("declare max 2");
    mx.write_header().expect("write_header");

    // Setter after write_header (the TrackEntry is already on disk).
    assert!(mx.set_max_block_addition_id(0, 4).is_err());

    let p = packet(0, 0, true, 0x11, 8);
    // Undeclared stream: §5.1.4.1.16 default 0 = "no BlockAdditions".
    assert!(mx
        .write_packet_with_additions(
            &packet(1, 0, true, 0x22, 8),
            &[MkvBlockAddition::codec_defined(vec![0x01])],
        )
        .is_err());
    // BlockAddID 0: §5.1.3.5.2.3 ranges the element as "not 0".
    assert!(mx
        .write_packet_with_additions(&p, &[MkvBlockAddition::new(0, vec![0x01])])
        .is_err());
    // BlockAddID above the declared MaxBlockAdditionID.
    assert!(mx
        .write_packet_with_additions(&p, &[MkvBlockAddition::new(3, vec![0x01])])
        .is_err());
    // Duplicate BlockAddID within one BlockAdditions (§5.1.3.5.2.3 MUST).
    assert!(mx
        .write_packet_with_additions(
            &p,
            &[
                MkvBlockAddition::new(2, vec![0x01]),
                MkvBlockAddition::new(2, vec![0x02]),
            ],
        )
        .is_err());
    // A spec-conformant write on the declared stream still succeeds.
    mx.write_packet_with_additions(&p, &[MkvBlockAddition::new(2, vec![0x01])])
        .expect("valid additions accepted");
    mx.write_trailer().expect("write_trailer");
    drop(mx);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn additions_packet_flushes_pending_lace_preserving_order() {
    // With Xiph lacing on, three same-size audio packets sit in the
    // lace buffer; the additions packet must flush them first so the
    // file's Block order matches write order.
    let streams = [audio_stream(0)];
    let bytes = mux_with(&streams, |mx| {
        mx.with_block_lacing(LacingMode::Xiph).expect("lacing");
        mx.set_max_block_addition_id(0, 1).expect("declare");
        mx.write_header().expect("write_header");
        for i in 0..3 {
            mx.write_packet(&packet(0, i * 20, true, 0x10 + i as u8, 8))
                .expect("laced packet");
        }
        mx.write_packet_with_additions(
            &packet(0, 60, true, 0x55, 8),
            &[MkvBlockAddition::codec_defined(vec![0x77; 3])],
        )
        .expect("additions packet");
    });
    let mut dmx = demux_typed(bytes);
    for i in 0..3 {
        let p = dmx.next_packet().expect("laced frame");
        assert_eq!(p.data, vec![0x10 + i as u8; 8], "frame {i} order");
        assert!(dmx.block_additions().is_empty());
    }
    let p4 = dmx.next_packet().expect("group frame");
    assert_eq!(p4.data, vec![0x55; 8]);
    assert_eq!(p4.pts, Some(60));
    assert_eq!(dmx.block_additions().len(), 1);
    assert_eq!(dmx.block_additions()[0].data(), &[0x77; 3][..]);
}

#[test]
fn explicit_zero_declaration_is_on_disk_but_still_gates_writes() {
    // set_max_block_addition_id(0, 0) writes the element explicitly
    // (byte-distinct from omission, decoding identically) and still
    // refuses additions — 0 means "no BlockAdditions for this track".
    let streams = [video_stream(0)];
    let tmp = tmp_path("zero");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer construct");
    mx.set_max_block_addition_id(0, 0).expect("explicit zero");
    assert_eq!(mx.max_block_addition_id(0), Some(0));
    mx.write_header().expect("write_header");
    assert!(mx
        .write_packet_with_additions(
            &packet(0, 0, true, 0x11, 8),
            &[MkvBlockAddition::codec_defined(vec![0x01])],
        )
        .is_err());
    mx.write_packet(&packet(0, 0, true, 0x11, 8))
        .expect("plain packet");
    mx.write_trailer().expect("write_trailer");
    drop(mx);
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    assert!(
        contains_id2(&bytes, 0x55EE),
        "explicit zero must reach disk"
    );
    let dmx = demux_typed(bytes);
    assert_eq!(dmx.max_block_addition_id(0), Some(0));
}
