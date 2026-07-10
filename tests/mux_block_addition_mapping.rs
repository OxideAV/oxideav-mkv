//! Round-trip tests for the muxer's track-level `BlockAdditionMapping` write
//! path (RFC 9559 §5.1.4.1.17 — `BlockAddIDValue` / `BlockAddIDName` /
//! `BlockAddIDType` / `BlockAddIDExtraData`).
//!
//! Drives [`MkvMuxer::set_block_addition_mappings`] against the public Muxer
//! trait, then re-opens the bytes through [`oxideav_mkv::demux::open_typed`]
//! and confirms [`oxideav_mkv::demux::MkvDemuxer::block_addition_mappings`]
//! decodes every mapping the muxer was handed, element-for-element.
//!
//! Contracts pinned here:
//!
//! 1. A mapping with all four children round-trips bit-exactly.
//! 2. Multiple mappings on one `TrackEntry` preserve on-disk order.
//! 3. The §5.1.4.1.17.3 default `BlockAddIDType == 0` stays off-disk yet
//!    round-trips as `0`; `value` / `name` / `extra_data` write only when
//!    `Some`.
//! 4. Omitting the call keeps the `TrackEntry` free of any
//!    `BlockAdditionMapping` (the demuxer surfaces an empty slice).
//! 5. The setter rejects calls after `write_header` and out-of-range indices,
//!    and an empty slice clears a previously-queued list.
//!
//! These tests use the production demuxer to walk the muxed buffer — no
//! third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::demux::BlockAdditionMapping;
use oxideav_mkv::mux::MkvMuxer;

fn assert_err<T>(r: Result<T, Error>, msg: &str) -> Error {
    match r {
        Ok(_) => panic!("{msg}: expected Err, got Ok"),
        Err(e) => e,
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r404-bamap-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

fn video_stream() -> StreamInfo {
    let mut p = CodecParameters::video(CodecId::new("vp9"));
    p.width = Some(320);
    p.height = Some(240);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn video_packet() -> Packet {
    let mut p = Packet::new(0, TimeBase::new(1, 1000), vec![0x42; 64]);
    p.pts = Some(0);
    p.flags.keyframe = true;
    p
}

fn mux_video<F>(configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("muxer construct");
        configure(&mut mx);
        mx.write_header().expect("write_header");
        mx.write_packet(&video_packet()).expect("packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn demux(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open_typed")
}

#[test]
fn full_mapping_roundtrips() {
    let mapping = BlockAdditionMapping {
        value: Some(4),
        name: Some("dolby-vision-rpu".to_string()),
        addid_type: 0x1_2345,
        extra_data: Some(vec![0xDE, 0xAD, 0xBE, 0xEF]),
    };
    let dmx = demux(mux_video(|mx| {
        mx.set_block_addition_mappings(0, vec![mapping.clone()])
            .expect("set");
    }));
    let got = dmx.block_addition_mappings(0);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0], mapping);
    assert!(!got[0].is_codec_defined());
}

#[test]
fn multiple_mappings_preserve_order() {
    let m1 = BlockAdditionMapping {
        value: Some(2),
        name: None,
        addid_type: 5,
        extra_data: None,
    };
    let m2 = BlockAdditionMapping {
        value: Some(3),
        name: Some("second".to_string()),
        addid_type: 7,
        extra_data: Some(vec![0x01]),
    };
    let dmx = demux(mux_video(|mx| {
        mx.set_block_addition_mappings(0, vec![m1.clone(), m2.clone()])
            .expect("set");
    }));
    let got = dmx.block_addition_mappings(0);
    assert_eq!(got, [m1, m2].as_slice());
}

#[test]
fn default_type_zero_stays_off_disk_but_roundtrips() {
    // addid_type 0 is the §5.1.4.1.17.3 default; per the usage note a
    // codec-defined mapping pairs with BlockAddIDValue 1. The BlockAddIDType
    // child stays off-disk, yet decodes back to 0.
    let mapping = BlockAdditionMapping {
        value: Some(1),
        name: None,
        addid_type: 0,
        extra_data: None,
    };
    let bytes = mux_video(|mx| {
        mx.set_block_addition_mappings(0, vec![mapping.clone()])
            .expect("set");
    });
    // BlockAddIDType id 0x41E7 -> [0x41, 0xE7]; must NOT appear on disk.
    assert!(
        !bytes.windows(2).any(|w| w[0] == 0x41 && w[1] == 0xE7),
        "default BlockAddIDType=0 must stay off-disk"
    );
    let dmx = demux(bytes);
    let got = dmx.block_addition_mappings(0);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0], mapping);
    assert!(got[0].is_codec_defined());
}

#[test]
fn omitted_call_surfaces_empty_slice() {
    let bytes = mux_video(|_mx| {});
    // BlockAdditionMapping id 0x41E4 -> [0x41, 0xE4]; absent.
    assert!(
        !bytes.windows(2).any(|w| w[0] == 0x41 && w[1] == 0xE4),
        "no BlockAdditionMapping master when the setter is not called"
    );
    let dmx = demux(bytes);
    assert!(dmx.block_addition_mappings(0).is_empty());
}

#[test]
fn empty_slice_clears_and_accessor_reflects() {
    let tmp = tmp_path("clr");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("construct");
    assert!(mx.block_addition_mappings(0).is_empty());
    let m = BlockAdditionMapping {
        value: Some(2),
        name: None,
        addid_type: 3,
        extra_data: None,
    };
    mx.set_block_addition_mappings(0, vec![m.clone()]).unwrap();
    assert_eq!(mx.block_addition_mappings(0), [m].as_slice());
    // Clear.
    mx.set_block_addition_mappings(0, vec![]).unwrap();
    assert!(mx.block_addition_mappings(0).is_empty());
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn setter_rejects_bad_state_and_index() {
    let tmp = tmp_path("err");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("construct");

    let e = assert_err(
        mx.set_block_addition_mappings(9, vec![]),
        "out-of-range index",
    );
    assert!(format!("{e}").contains("out of range"), "got: {e}");

    mx.write_header().expect("write_header");
    let e = assert_err(
        mx.set_block_addition_mappings(0, vec![]),
        "after write_header",
    );
    assert!(format!("{e}").contains("after write_header"), "got: {e}");
    let _ = std::fs::remove_file(&tmp);
}
