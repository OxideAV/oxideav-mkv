//! Round-trip tests for the muxer's Segment `Duration` write path
//! (RFC 9559 §5.1.2.10, id `0x4489`).
//!
//! `Duration` is a `float` in `TimestampScale` ticks giving the total length
//! of the Segment. The muxer streams `Cluster`s with the unknown-size VINT
//! and seals the CRC-validated `Info` master at header time, so it never
//! auto-derives the value — a caller supplies it via
//! [`MkvMuxer::set_duration`] and it is written into the `Info` body before
//! `DateUTC`.
//!
//! Contracts pinned here:
//!
//! 1. A queued duration round-trips through the demuxer's `duration_micros`.
//! 2. Omitting the call keeps `Duration` off-disk (demuxer surfaces `None`).
//! 3. The setter enforces the §5.1.2.10 `> 0x0p+0` range and rejects calls
//!    after `write_header`.
//! 4. `Duration` lands inside the `Info` element (before `DateUTC`) and the
//!    `Info` `CRC-32` still validates on read (no `crc_status` mismatch).
//!
//! These tests use the production demuxer to walk the muxed buffer — no
//! third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, Packet, ReadSeek, StreamInfo, TimeBase,
    WriteSeek,
};
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
        "oxideav-mkv-r404-duration-{}-{}-{n}.mkv",
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

fn video_packet(pts: i64) -> Packet {
    let mut p = Packet::new(0, TimeBase::new(1, 1000), vec![0x42; 64]);
    p.pts = Some(pts);
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
        mx.write_packet(&video_packet(0)).expect("packet 0");
        mx.write_packet(&video_packet(1000)).expect("packet 1");
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

/// `Duration` id 0x4489 -> [0x44, 0x89]. Confirm on-disk presence.
fn has_duration(bytes: &[u8]) -> bool {
    bytes.windows(2).any(|w| w[0] == 0x44 && w[1] == 0x89)
}

#[test]
fn duration_roundtrips_through_demuxer() {
    // 3.5 s -> 3500 ms ticks -> 3_500_000 micros surfaced by the demuxer.
    let bytes = mux_video(|mx| {
        mx.set_duration(Duration::from_millis(3500))
            .expect("set_duration");
    });
    assert!(has_duration(&bytes), "Duration must be on disk");
    let dmx = demux(bytes);
    assert_eq!(dmx.duration_micros(), Some(3_500_000));
}

#[test]
fn omitted_duration_stays_off_disk() {
    let bytes = mux_video(|_mx| {});
    assert!(
        !has_duration(&bytes),
        "omitted Duration must not be on disk"
    );
    let dmx = demux(bytes);
    assert_eq!(dmx.duration_micros(), None);
}

#[test]
fn accessor_reflects_queued_value() {
    let tmp = tmp_path("acc");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("construct");
    assert_eq!(mx.duration_ticks(), None);
    mx.set_duration(Duration::from_millis(1234)).unwrap();
    assert_eq!(mx.duration_ticks(), Some(1234.0));
    // Last-write-wins.
    mx.set_duration(Duration::from_secs(2)).unwrap();
    assert_eq!(mx.duration_ticks(), Some(2000.0));
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn setter_rejects_zero_and_bad_state() {
    let tmp = tmp_path("err");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let mut mx = MkvMuxer::new_matroska(ws, &[video_stream()]).expect("construct");

    // Zero duration violates the §5.1.2.10 "> 0x0p+0" range.
    let e = assert_err(mx.set_duration(Duration::ZERO), "zero duration");
    assert!(format!("{e}").contains("out of range"), "got: {e}");

    // After write_header.
    mx.write_header().expect("write_header");
    let e = assert_err(
        mx.set_duration(Duration::from_millis(10)),
        "after write_header",
    );
    assert!(format!("{e}").contains("after write_header"), "got: {e}");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn duration_inside_info_crc_still_validates() {
    // Duration sits inside the CRC-validated Info body. Re-opening must not
    // report a CRC mismatch on any Top-Level master.
    let bytes = mux_video(|mx| {
        mx.set_duration(Duration::from_millis(500)).unwrap();
    });
    let dmx = demux(bytes);
    // Every Top-Level master that carried a CRC-32 (Info included) must
    // still validate with Duration written inside the Info body.
    let statuses = dmx.crc_status();
    assert!(
        statuses
            .iter()
            .any(|s| s.element_id == oxideav_mkv::ids::INFO),
        "Info CRC status must be present"
    );
    for s in statuses {
        assert!(
            s.is_valid(),
            "CRC for element 0x{:X} must validate (stored 0x{:X} vs computed 0x{:X})",
            s.element_id,
            s.stored,
            s.computed
        );
    }
    assert_eq!(dmx.duration_micros(), Some(500_000));
}
