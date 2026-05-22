//! Round-trip tests for the muxer's opt-in block-lacing modes
//! (RFC 9559 §5.1.4.5.5, §10.3).
//!
//! Each test:
//!
//! 1. Drives [`MkvMuxer::with_block_lacing`] with one of the three
//!    laced modes (Xiph, fixed-size, EBML) plus the default
//!    [`LacingMode::None`] for a regression anchor.
//! 2. Writes a tiny single-track MKV through the public `Muxer`
//!    trait so the on-disk format matches what real callers see.
//! 3. Re-opens the bytes through the same-crate demuxer and
//!    confirms the original packet payloads survive byte-identical,
//!    in the same order, with their `keyframe` flag preserved.
//! 4. For laced modes, also walks the SimpleBlock manually to
//!    confirm the LACING bits in the flags byte match the
//!    requested mode (RFC 9559 §10.2 Figure 13: bits 1..3 of the
//!    flags byte) and that `FlagLacing = 1` is written on the
//!    `TrackEntry` (`§5.1.4.1.12`).
//!
//! These tests use the production EBML helpers — no third-party
//! Matroska code is consulted.

use std::io::{Cursor, Read, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::ebml::{read_element_header, read_uint, VINT_UNKNOWN_SIZE};
use oxideav_mkv::ids;
use oxideav_mkv::mux::{LacingMode, MkvMuxer};

/// Counter ensures every temp file produced by the parallel test
/// runner gets a unique name.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r95-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

fn pcm_stream() -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p.sample_format = Some(oxideav_core::SampleFormat::S16);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

/// One PCM packet at `pts_ms` carrying `len` bytes of `payload`.
fn pcm_packet(pts_ms: i64, payload: u8, len: usize) -> Packet {
    let mut pkt = Packet::new(0, TimeBase::new(1, 1000), vec![payload; len]);
    pkt.pts = Some(pts_ms);
    pkt.flags.keyframe = true;
    pkt
}

/// Mux a single-track PCM MKV with the given lacing mode and an
/// optional list of pre-built packets. Returns the on-disk bytes.
fn mux_with_lacing(mode: LacingMode, packets: &[Packet]) -> Vec<u8> {
    let tmp = tmp_path("lacing");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let stream = pcm_stream();
        let mut mx = MkvMuxer::new_matroska(ws, &[stream]).expect("muxer construct");
        mx.with_block_lacing(mode).expect("with_block_lacing");
        mx.write_header().expect("write_header");
        for p in packets {
            mx.write_packet(p).expect("write_packet");
        }
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

/// Walk the file to find a top-level child of the Segment master
/// matching `target_id`, returning its absolute file offset.
fn find_top_level(bytes: &[u8], target_id: u32) -> Option<u64> {
    use std::io::Seek;
    let mut cur = Cursor::new(bytes);
    let ebml = read_element_header(&mut cur).ok()?;
    assert_eq!(ebml.id, ids::EBML_HEADER);
    cur.seek(SeekFrom::Current(ebml.size as i64)).ok()?;
    let seg = read_element_header(&mut cur).ok()?;
    assert_eq!(seg.id, ids::SEGMENT);
    let segment_data_start = cur.stream_position().ok()?;
    let segment_end = if seg.size == VINT_UNKNOWN_SIZE {
        bytes.len() as u64
    } else {
        segment_data_start + seg.size
    };
    while cur.stream_position().ok()? < segment_end {
        let elem_start = cur.stream_position().ok()?;
        let e = read_element_header(&mut cur).ok()?;
        if e.id == target_id {
            return Some(elem_start);
        }
        if e.size == VINT_UNKNOWN_SIZE {
            return None;
        }
        cur.seek(SeekFrom::Current(e.size as i64)).ok()?;
    }
    None
}

/// Find the FlagLacing element inside the first TrackEntry of the
/// Tracks master. Returns the raw uint payload, or None if no
/// FlagLacing was written.
fn read_first_track_flag_lacing(bytes: &[u8]) -> Option<u64> {
    use std::io::Seek;
    let tracks_off = find_top_level(bytes, ids::TRACKS)?;
    let mut cur = Cursor::new(bytes);
    cur.seek(SeekFrom::Start(tracks_off)).ok()?;
    let tracks = read_element_header(&mut cur).ok()?;
    let tracks_body_start = cur.stream_position().ok()?;
    let tracks_body_end = tracks_body_start + tracks.size;
    // First child: TrackEntry.
    while cur.stream_position().ok()? < tracks_body_end {
        let e = read_element_header(&mut cur).ok()?;
        if e.id == ids::TRACK_ENTRY {
            let te_body_start = cur.stream_position().ok()?;
            let te_body_end = te_body_start + e.size;
            while cur.stream_position().ok()? < te_body_end {
                let c = read_element_header(&mut cur).ok()?;
                if c.id == ids::FLAG_LACING {
                    return read_uint(&mut cur, c.size as usize).ok();
                }
                cur.seek(SeekFrom::Current(c.size as i64)).ok()?;
            }
            return None;
        }
        cur.seek(SeekFrom::Current(e.size as i64)).ok()?;
    }
    None
}

/// Locate the first SimpleBlock element after Tracks and return
/// its on-disk body bytes (track + tc + flags + payload).
fn read_first_simple_block_body(bytes: &[u8]) -> Vec<u8> {
    use std::io::Seek;
    let cluster_off = find_top_level(bytes, ids::CLUSTER).expect("cluster present");
    let mut cur = Cursor::new(bytes);
    cur.seek(SeekFrom::Start(cluster_off)).unwrap();
    let cluster = read_element_header(&mut cur).unwrap();
    let body_end = if cluster.size == VINT_UNKNOWN_SIZE {
        bytes.len() as u64
    } else {
        cur.stream_position().unwrap() + cluster.size
    };
    while cur.stream_position().unwrap() < body_end {
        let e = read_element_header(&mut cur).unwrap();
        if e.id == ids::SIMPLE_BLOCK {
            let mut buf = vec![0u8; e.size as usize];
            cur.read_exact(&mut buf).unwrap();
            return buf;
        }
        cur.seek(SeekFrom::Current(e.size as i64)).unwrap();
    }
    panic!("no SimpleBlock found after cluster header");
}

/// Decode the LACING bits (positions 1..3) of a SimpleBlock's
/// flags byte. The SimpleBlock body layout: vint TrackNumber, i16
/// timecode, u8 flags, payload.
fn lacing_bits_of_simple_block(body: &[u8]) -> u8 {
    let mut cur = Cursor::new(body);
    let _ = oxideav_mkv::ebml::read_vint(&mut cur, false).unwrap();
    let pos = cur.position() as usize;
    let flags = body[pos + 2];
    (flags >> 1) & 0x03
}

#[test]
fn default_is_no_lacing_and_flag_lacing_stays_zero() {
    // Three identical packets back-to-back. With LacingMode::None
    // each one becomes a standalone SimpleBlock and FlagLacing = 0.
    let packets = vec![
        pcm_packet(0, 0xAA, 32),
        pcm_packet(20, 0xBB, 32),
        pcm_packet(40, 0xCC, 32),
    ];
    let bytes = mux_with_lacing(LacingMode::None, &packets);

    assert_eq!(
        read_first_track_flag_lacing(&bytes),
        Some(0),
        "FlagLacing must stay 0 in LacingMode::None"
    );
    // Round-trip: three packets should come back unchanged.
    let mut dmx = demux_bytes(bytes);
    let got: Vec<Vec<u8>> = std::iter::from_fn(|| dmx.next_packet().ok())
        .map(|p| p.data)
        .collect();
    assert_eq!(got.len(), 3);
    for (i, want) in [0xAAu8, 0xBB, 0xCC].iter().enumerate() {
        assert_eq!(got[i], vec![*want; 32], "frame {i} byte mismatch");
    }
}

#[test]
fn fixed_lacing_round_trips_three_equal_size_frames() {
    let packets = vec![
        pcm_packet(0, 0xAA, 32),
        pcm_packet(20, 0xBB, 32),
        pcm_packet(40, 0xCC, 32),
    ];
    let bytes = mux_with_lacing(LacingMode::FixedSize, &packets);
    assert_eq!(
        read_first_track_flag_lacing(&bytes),
        Some(1),
        "FlagLacing must be 1 once lacing is opted in"
    );
    let body = read_first_simple_block_body(&bytes);
    assert_eq!(
        lacing_bits_of_simple_block(&body),
        0b10,
        "FixedSize lacing must encode flags LACING bits = 10b"
    );
    let mut dmx = demux_bytes(bytes);
    let got: Vec<Vec<u8>> = std::iter::from_fn(|| dmx.next_packet().ok())
        .map(|p| p.data)
        .collect();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], vec![0xAA; 32]);
    assert_eq!(got[1], vec![0xBB; 32]);
    assert_eq!(got[2], vec![0xCC; 32]);
}

#[test]
fn xiph_lacing_round_trips_three_variable_size_frames() {
    let packets = vec![
        pcm_packet(0, 0xAA, 17),
        pcm_packet(20, 0xBB, 23),
        pcm_packet(40, 0xCC, 41),
    ];
    let bytes = mux_with_lacing(LacingMode::Xiph, &packets);
    assert_eq!(read_first_track_flag_lacing(&bytes), Some(1));
    let body = read_first_simple_block_body(&bytes);
    assert_eq!(
        lacing_bits_of_simple_block(&body),
        0b01,
        "Xiph lacing must encode flags LACING bits = 01b"
    );
    let mut dmx = demux_bytes(bytes);
    let got: Vec<Vec<u8>> = std::iter::from_fn(|| dmx.next_packet().ok())
        .map(|p| p.data)
        .collect();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], vec![0xAA; 17]);
    assert_eq!(got[1], vec![0xBB; 23]);
    assert_eq!(got[2], vec![0xCC; 41]);
}

#[test]
fn xiph_lacing_handles_255_octet_run_in_size_encoding() {
    // 260 is encoded `0xFF 0x05` in Xiph; 510 is `0xFF 0xFF 0x00`.
    // Exercise both — the size encoding loop runs through the
    // multi-octet path on the muxer side and the demuxer's loop
    // mirrors it.
    let packets = vec![
        pcm_packet(0, 0xAA, 260),
        pcm_packet(20, 0xBB, 510),
        pcm_packet(40, 0xCC, 99),
    ];
    let bytes = mux_with_lacing(LacingMode::Xiph, &packets);
    let mut dmx = demux_bytes(bytes);
    let got: Vec<Vec<u8>> = std::iter::from_fn(|| dmx.next_packet().ok())
        .map(|p| p.data)
        .collect();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].len(), 260);
    assert_eq!(got[1].len(), 510);
    assert_eq!(got[2].len(), 99);
    assert_eq!(got[0], vec![0xAA; 260]);
    assert_eq!(got[1], vec![0xBB; 510]);
    assert_eq!(got[2], vec![0xCC; 99]);
}

#[test]
fn ebml_lacing_round_trips_three_variable_size_frames() {
    let packets = vec![
        pcm_packet(0, 0xAA, 50),
        pcm_packet(20, 0xBB, 30),
        pcm_packet(40, 0xCC, 80),
    ];
    let bytes = mux_with_lacing(LacingMode::Ebml, &packets);
    assert_eq!(read_first_track_flag_lacing(&bytes), Some(1));
    let body = read_first_simple_block_body(&bytes);
    assert_eq!(
        lacing_bits_of_simple_block(&body),
        0b11,
        "EBML lacing must encode flags LACING bits = 11b"
    );
    let mut dmx = demux_bytes(bytes);
    let got: Vec<Vec<u8>> = std::iter::from_fn(|| dmx.next_packet().ok())
        .map(|p| p.data)
        .collect();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], vec![0xAA; 50]);
    assert_eq!(got[1], vec![0xBB; 30]);
    assert_eq!(got[2], vec![0xCC; 80]);
}

#[test]
fn ebml_lacing_handles_large_signed_delta() {
    // Force the EBML-lacing signed-delta encoder onto a width-2
    // VINT: 800-byte first frame, 500-byte second (delta = -300,
    // requires more than 7 bits of signed range), 1000-byte
    // third. Matches the worked example in RFC 9559 §10.3.3.
    let packets = vec![
        pcm_packet(0, 0xAA, 800),
        pcm_packet(20, 0xBB, 500),
        pcm_packet(40, 0xCC, 1000),
    ];
    let bytes = mux_with_lacing(LacingMode::Ebml, &packets);
    let mut dmx = demux_bytes(bytes);
    let got: Vec<Vec<u8>> = std::iter::from_fn(|| dmx.next_packet().ok())
        .map(|p| p.data)
        .collect();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].len(), 800);
    assert_eq!(got[1].len(), 500);
    assert_eq!(got[2].len(), 1000);
}

#[test]
fn fixed_lacing_flushes_on_size_mismatch_before_appending() {
    // First two packets identical-size → laced. Third packet has
    // a different size → first two get flushed as a 2-frame
    // FixedSize Block; third starts a fresh 1-frame buffer that
    // ends up emitted as a no-lacing Block at write_trailer.
    let packets = vec![
        pcm_packet(0, 0xAA, 32),
        pcm_packet(20, 0xBB, 32),
        pcm_packet(40, 0xCC, 64),
    ];
    let bytes = mux_with_lacing(LacingMode::FixedSize, &packets);
    let mut dmx = demux_bytes(bytes);
    let got: Vec<Vec<u8>> = std::iter::from_fn(|| dmx.next_packet().ok())
        .map(|p| p.data)
        .collect();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].len(), 32);
    assert_eq!(got[1].len(), 32);
    assert_eq!(got[2].len(), 64);
}

#[test]
fn with_block_lacing_after_header_is_rejected() {
    let mem = MemFile {
        inner: Cursor::new(Vec::new()),
    };
    let ws: Box<dyn WriteSeek> = Box::new(mem);
    let mut mx = MkvMuxer::new_matroska(ws, &[pcm_stream()]).expect("muxer construct");
    mx.write_header().expect("write_header");
    let err = match mx.with_block_lacing(LacingMode::Xiph) {
        Ok(_) => panic!("with_block_lacing must reject post-header calls"),
        Err(e) => e,
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("with_block_lacing"),
        "error should mention the offending call: {msg}"
    );
}

#[test]
fn lacing_mode_accessor_reflects_setter() {
    let mem = MemFile {
        inner: Cursor::new(Vec::new()),
    };
    let ws: Box<dyn WriteSeek> = Box::new(mem);
    let mut mx = MkvMuxer::new_matroska(ws, &[pcm_stream()]).expect("muxer construct");
    assert_eq!(mx.block_lacing_mode(), LacingMode::None);
    mx.with_block_lacing(LacingMode::Ebml).unwrap();
    assert_eq!(mx.block_lacing_mode(), LacingMode::Ebml);
    mx.with_block_lacing(LacingMode::None).unwrap();
    assert_eq!(mx.block_lacing_mode(), LacingMode::None);
}

#[test]
fn lacing_respects_max_frames_per_block_cap() {
    // 20 same-size packets in one cluster (each at +5 ms so all
    // sit safely within the same cluster's 5-second window).
    // With the 8-frame cap the muxer should emit at least
    // ceil(20/8) = 3 SimpleBlocks; all 20 frames must still
    // round-trip.
    let mut packets: Vec<Packet> = Vec::new();
    for i in 0..20 {
        packets.push(pcm_packet((i * 5) as i64, 0x10 + i as u8, 32));
    }
    let bytes = mux_with_lacing(LacingMode::FixedSize, &packets);
    let mut dmx = demux_bytes(bytes);
    let got: Vec<Vec<u8>> = std::iter::from_fn(|| dmx.next_packet().ok())
        .map(|p| p.data)
        .collect();
    assert_eq!(got.len(), 20);
    for (i, frame) in got.iter().enumerate() {
        assert_eq!(
            *frame,
            vec![0x10 + i as u8; 32],
            "frame {i} payload byte mismatch"
        );
    }
}

/// In-memory cursor that looks like a file to the Muxer.
struct MemFile {
    inner: Cursor<Vec<u8>>,
}

impl std::io::Write for MemFile {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.inner.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl std::io::Read for MemFile {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(b)
    }
}

impl std::io::Seek for MemFile {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}
