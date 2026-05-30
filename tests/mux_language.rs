//! Round-trip + omission tests for the `Language` element on
//! `TrackEntry` (RFC 9559 §5.1.4.1.2.1, id `0x22B59C`).
//!
//! 1. **Integration round-trip.** Mux a one-track PCM file whose
//!    [`CodecParameters::language`] is `Some("jpn")` and confirm the
//!    demuxer parses the same string back onto the surfaced
//!    [`StreamInfo::params`].
//!
//! 2. **Omission.** Mux the same one-track file with
//!    [`CodecParameters::language`] left `None` and confirm the
//!    muxer does NOT emit the `Language` element ID anywhere in the
//!    serialized bytes — Matroska parsers fall back to the spec
//!    default (`"eng"`) automatically when the element is absent, so
//!    re-muxing an English-by-default stream must not invent a new
//!    element that wasn't in the source.
//!
//! Both tests run through the public `Muxer` / `Demuxer` traits and
//! the production EBML helpers — no third-party Matroska code is
//! consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mkv::mux::MkvMuxer;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-lang-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

fn pcm_stream_with_language(language: Option<&str>) -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p.sample_format = Some(SampleFormat::S16);
    if let Some(l) = language {
        p = p.with_language(l);
    }
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn pcm_packet(pts_ms: i64, payload: u8, len: usize) -> Packet {
    let mut pkt = Packet::new(0, TimeBase::new(1, 1000), vec![payload; len]);
    pkt.pts = Some(pts_ms);
    pkt.flags.keyframe = true;
    pkt
}

/// Mux a single-track PCM MKV with the supplied per-track language and
/// one packet, then read the file back into memory.
fn mux_with_language(language: Option<&str>) -> Vec<u8> {
    let tmp = tmp_path("roundtrip");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let stream = pcm_stream_with_language(language);
        let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
        mx.write_header().expect("write_header");
        mx.write_packet(&pcm_packet(0, 0xAA, 32))
            .expect("write_packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn demux_bytes(bytes: Vec<u8>) -> Box<dyn Demuxer> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

/// Scan `bytes` for the Language EBML element ID (`0x22B59C`) encoded
/// as the four-byte VINT `[0xE2, 0xB5, 0x9C]` prefixed by the class-D
/// vint marker — i.e. the on-wire encoding written by
/// `write_element_id` for a 4-byte ID. Returns `true` if any
/// occurrence appears anywhere in the byte stream.
fn contains_language_element_id(bytes: &[u8]) -> bool {
    // Per RFC 8794 §4: the ID `0x22B59C` (3 payload bytes) is written
    // verbatim — `[0x22, 0xB5, 0x9C]`. Scan for that triplet.
    bytes.windows(3).any(|w| w == [0x22, 0xB5, 0x9C])
}

#[test]
fn language_some_jpn_round_trips_through_demux() {
    let bytes = mux_with_language(Some("jpn"));

    // Demuxer must surface the language we wrote.
    let dx = demux_bytes(bytes.clone());
    let streams = dx.streams();
    assert_eq!(streams.len(), 1, "expected the single audio track");
    assert_eq!(
        streams[0].params.language.as_deref(),
        Some("jpn"),
        "Language element should round-trip jpn → params.language"
    );

    // Sanity: the Language element ID is on disk.
    assert!(
        contains_language_element_id(&bytes),
        "expected the 0x22B59C element ID to appear in the muxed bytes"
    );
}

#[test]
fn language_none_omits_the_element_on_disk() {
    let bytes = mux_with_language(None);

    // No Language element ID anywhere in the file — Matroska parsers
    // fall back to the spec default `"eng"` automatically when the
    // element is absent.
    assert!(
        !contains_language_element_id(&bytes),
        "muxer must NOT emit 0x22B59C when CodecParameters::language is None"
    );

    // And the demuxer must surface None too — we deliberately do not
    // materialise the spec default so re-mux preserves absence.
    let dx = demux_bytes(bytes);
    let streams = dx.streams();
    assert_eq!(streams.len(), 1);
    assert!(
        streams[0].params.language.is_none(),
        "absent Language must come back as None, not the spec default"
    );
}
