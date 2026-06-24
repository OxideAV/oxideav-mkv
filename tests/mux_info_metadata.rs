//! Mux round-trip tests for the Segment `Info` metadata write surface —
//! `Title` (RFC 9559 §5.1.2.12) and `DateUTC` (RFC 9559 §5.1.2.11).
//!
//! `MkvMuxer::set_title` / `set_date_utc_ns` / `set_date_utc_unix_secs` queue
//! the two informational `Info` children the demuxer already lifts onto its
//! flat metadata view (`"title"` / `"date"`). The muxer writes them in
//! RFC 9559 §5.1.2 element order (after `TimestampScale`, before `MuxingApp`);
//! re-opening through the production demuxer recovers both.
//!
//! `DateUTC` is the `date` element type: a signed 8-byte big-endian integer
//! counting nanoseconds since the Matroska epoch (2001-01-01T00:00:00 UTC).
//! The Unix-seconds convenience rebases onto that epoch; a pre-2001 timestamp
//! yields a negative `DateUTC`, which still round-trips.
//!
//! No third-party Matroska code is consulted — the production demuxer walks
//! every muxed buffer.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::mux::MkvMuxer;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r364-infometa-{}-{}-{n}.mkv",
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

fn keyframe_packet(stream: u32, pts: i64) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![0xAA; 16]);
    p.pts = Some(pts);
    p.flags.keyframe = true;
    p
}

fn mux_one_track<F>(configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let streams = vec![video_stream(0)];
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer construct");
        configure(&mut mx);
        mx.write_header().expect("write_header");
        mx.write_packet(&keyframe_packet(0, 0)).expect("write 0");
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

fn meta_value<'a>(dmx: &'a oxideav_mkv::demux::MkvDemuxer, key: &str) -> Option<&'a str> {
    dmx.metadata()
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// `set_title` surfaces on the demuxer's `"title"` metadata key.
#[test]
fn roundtrip_title() {
    let bytes = mux_one_track(|mx| {
        mx.set_title("My Movie").expect("set_title");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(meta_value(&dmx, "title"), Some("My Movie"));
}

/// A non-ASCII title round-trips byte-for-byte (UTF-8 element).
#[test]
fn roundtrip_unicode_title() {
    let title = "Café — 日本語 🎬";
    let bytes = mux_one_track(|mx| {
        mx.set_title(title).expect("set_title");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(meta_value(&dmx, "title"), Some(title));
}

/// `set_date_utc_ns` round-trips: the demuxer decodes the same instant onto
/// its `"date"` key. 2001-01-01T00:00:00 (the Matroska epoch, ns = 0) decodes
/// to the ISO-8601 epoch string.
#[test]
fn roundtrip_date_utc_epoch() {
    let bytes = mux_one_track(|mx| {
        mx.set_date_utc_ns(0).expect("set_date_utc_ns");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(meta_value(&dmx, "date"), Some("2001-01-01T00:00:00Z"));
}

/// A specific instant — 2020-02-29T12:34:56 UTC — round-trips through the
/// ns-since-2001 representation. 604_700_096 s after the 2001 epoch.
#[test]
fn roundtrip_date_utc_specific() {
    // 2020-02-29T12:34:56Z is 1_582_979_696 s Unix; minus the 978_307_200 s
    // 2001 epoch = 604_672_496 s after the Matroska epoch.
    let secs_since_2001: i64 = 604_672_496;
    let bytes = mux_one_track(|mx| {
        mx.set_date_utc_ns(secs_since_2001 * 1_000_000_000)
            .expect("set_date_utc_ns");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(meta_value(&dmx, "date"), Some("2020-02-29T12:34:56Z"));
}

/// The Unix-seconds convenience rebases onto the 2001 epoch correctly.
#[test]
fn roundtrip_date_utc_unix_secs() {
    // 2020-02-29T12:34:56Z = 1_582_979_696 Unix seconds.
    let bytes = mux_one_track(|mx| {
        mx.set_date_utc_unix_secs(1_582_979_696)
            .expect("set_date_utc_unix_secs");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(meta_value(&dmx, "date"), Some("2020-02-29T12:34:56Z"));
}

/// A pre-2001 timestamp yields a negative `DateUTC` (signed `date` type) that
/// still round-trips. 2000-01-01T00:00:00Z is one (leap) year before the
/// Matroska epoch.
#[test]
fn roundtrip_date_utc_pre_epoch() {
    // 2000-01-01T00:00:00Z = 946_684_800 Unix seconds (= -31_622_400 s vs 2001).
    let bytes = mux_one_track(|mx| {
        mx.set_date_utc_unix_secs(946_684_800)
            .expect("set_date_utc_unix_secs");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(meta_value(&dmx, "date"), Some("2000-01-01T00:00:00Z"));
}

/// Title + DateUTC together both round-trip when set on the same muxer.
#[test]
fn roundtrip_title_and_date_together() {
    let bytes = mux_one_track(|mx| {
        mx.set_title("Both").expect("set_title");
        mx.set_date_utc_ns(0).expect("set_date_utc_ns");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(meta_value(&dmx, "title"), Some("Both"));
    assert_eq!(meta_value(&dmx, "date"), Some("2001-01-01T00:00:00Z"));
}

/// Omitting the calls writes neither element — the demuxer surfaces no
/// `"title"` / `"date"` keys for a plain Segment.
#[test]
fn absent_when_not_set() {
    let dmx = demux_typed(mux_one_track(|_mx| {}));
    assert!(meta_value(&dmx, "title").is_none());
    assert!(meta_value(&dmx, "date").is_none());
}

/// `set_title` / `set_date_utc_ns` after `write_header` are rejected.
#[test]
fn rejects_after_write_header() {
    let f = std::fs::File::create(tmp_path("late")).expect("create");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("construct");
    mx.write_header().expect("write_header");
    assert!(mx.set_title("x").map(|_| ()).is_err());
    assert!(mx.set_date_utc_ns(0).map(|_| ()).is_err());
    assert!(mx.set_date_utc_unix_secs(0).map(|_| ()).is_err());
}

/// Last-write-wins on the title.
#[test]
fn title_last_write_wins() {
    let bytes = mux_one_track(|mx| {
        mx.set_title("first").expect("set");
        mx.set_title("second").expect("set");
    });
    let dmx = demux_typed(bytes);
    assert_eq!(meta_value(&dmx, "title"), Some("second"));
}
