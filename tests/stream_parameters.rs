//! Stream parameters a demuxed track reports before any packet is
//! decoded: picture size, decoded pixel format, frame rate and colour
//! signal, as stated by the track entry, its codec configuration
//! record and (VP9 without a features list) its first key frame.
//!
//! The fixtures under `tests/fixtures/stream_params/` were made with
//! ffmpeg (`testsrc2`, 66x50 @ 25 fps for H.264 / HEVC / VP9 — a size
//! that is not macroblock- or CTB-aligned, so the SPS crop is
//! exercised — and 64x48 @ 24000/1001 fps for AV1). The expected
//! values are ffprobe's:
//!
//! ```text
//! ffprobe -v error -count_packets -select_streams v:0 \
//!   -show_entries stream=width,height,pix_fmt,r_frame_rate,color_range,nb_read_packets FILE
//! ```

use oxideav_core::Muxer;
use oxideav_core::{ColorRange, Demuxer, Error, NullCodecResolver, PixelFormat, Rational};
use oxideav_mkv::mux::{MkvMuxer, MkvVideoColour};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stream_params/");

struct Expect {
    file: &'static str,
    codec: &'static str,
    size: (u32, u32),
    pixel_format: PixelFormat,
    frame_rate: (i64, i64),
    packets: usize,
}

const fn e(
    file: &'static str,
    codec: &'static str,
    size: (u32, u32),
    pixel_format: PixelFormat,
    frame_rate: (i64, i64),
    packets: usize,
) -> Expect {
    Expect {
        file,
        codec,
        size,
        pixel_format,
        frame_rate,
        packets,
    }
}

use PixelFormat::*;

const CASES: &[Expect] = &[
    e("h264_420p.mkv", "h264", (66, 50), Yuv420P, (25, 1), 2),
    e("h264_420p10.mkv", "h264", (66, 50), Yuv420P10Le, (25, 1), 2),
    e("h264_422p.mkv", "h264", (66, 50), Yuv422P, (25, 1), 2),
    e("h264_444p.mkv", "h264", (66, 50), Yuv444P, (25, 1), 2),
    e("hevc_420p.mkv", "h265", (66, 50), Yuv420P, (25, 1), 2),
    e("hevc_420p10.mkv", "h265", (66, 50), Yuv420P10Le, (25, 1), 2),
    e("hevc_422p10.mkv", "h265", (66, 50), Yuv422P10Le, (25, 1), 2),
    e("hevc_444p.mkv", "h265", (66, 50), Yuv444P, (25, 1), 2),
    e("vp9_420p.mkv", "vp9", (66, 50), Yuv420P, (25, 1), 2),
    e("vp9_420p.webm", "vp9", (66, 50), Yuv420P, (25, 1), 2),
    e("vp9_420p10.webm", "vp9", (66, 50), Yuv420P10Le, (25, 1), 2),
    e("vp9_422p.webm", "vp9", (66, 50), Yuv422P, (25, 1), 2),
    e("vp9_444p.webm", "vp9", (66, 50), Yuv444P, (25, 1), 2),
    e("av1_420p.mkv", "av1", (64, 48), Yuv420P, (24000, 1001), 3),
    e(
        "av1_420p10.webm",
        "av1",
        (64, 48),
        Yuv420P10Le,
        (24000, 1001),
        3,
    ),
];

fn open(file: &str) -> Box<dyn Demuxer> {
    let f = std::fs::File::open(format!("{DIR}{file}")).expect(file);
    oxideav_mkv::demux::open(Box::new(f), &NullCodecResolver).expect(file)
}

#[test]
fn every_field_matches_ffprobe() {
    for c in CASES {
        let d = open(c.file);
        let p = &d.streams()[0].params;
        assert_eq!(p.codec_id.as_str(), c.codec, "{}", c.file);
        assert_eq!(
            (p.width, p.height),
            (Some(c.size.0), Some(c.size.1)),
            "{}",
            c.file
        );
        assert_eq!(p.pixel_format, Some(c.pixel_format), "{}", c.file);
        assert_eq!(
            p.frame_rate,
            Some(Rational::new(c.frame_rate.0, c.frame_rate.1)),
            "{}",
            c.file
        );
        // ffprobe: color_range=tv (the Colour element's Range = 1).
        assert_eq!(p.color_signal.range, ColorRange::Limited, "{}", c.file);
    }
}

#[test]
fn reading_ahead_for_a_vp9_layout_keeps_every_packet() {
    for c in CASES {
        let mut d = open(c.file);
        let mut n = 0;
        let mut last_pts = None;
        loop {
            match d.next_packet() {
                Ok(p) => {
                    if n == 0 {
                        assert!(
                            p.flags.keyframe,
                            "{}: first packet is the key frame",
                            c.file
                        );
                    }
                    assert!(p.pts > last_pts, "{}: packets stay in order", c.file);
                    last_pts = p.pts;
                    n += 1;
                }
                Err(Error::Eof) => break,
                Err(e) => panic!("{}: {e}", c.file),
            }
        }
        assert_eq!(n, c.packets, "{}", c.file);
        // A seek back to the start replays the same packets.
        d.seek_to(0, 0).expect("seek");
        let first = d.next_packet().expect("packet after seek");
        assert_eq!(first.pts, Some(0), "{}", c.file);
    }
}

/// Remux a fixture's first packet into a fresh Matroska file with the
/// given CodecPrivate / Colour, then open it.
fn remux(
    file: &str,
    extradata: Vec<u8>,
    colour: Option<MkvVideoColour>,
    first_packet: Option<Vec<u8>>,
) -> Box<dyn Demuxer> {
    let mut src = open(file);
    let mut stream = src.streams()[0].clone();
    stream.params.extradata = extradata;
    stream.params.pixel_format = None;
    let mut pkt = src.next_packet().expect("packet");
    if let Some(data) = first_packet {
        pkt.data = data;
    }
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mkv-stream-params-{}-{}.mkv",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    {
        let f = std::fs::File::create(&tmp).expect("create");
        let mut mx =
            MkvMuxer::new_matroska(Box::new(f), std::slice::from_ref(&stream)).expect("muxer");
        if let Some(colour) = colour {
            mx.set_video_colour(0, colour).expect("colour");
        }
        mx.write_header().expect("header");
        mx.write_packet(&pkt).expect("packet");
        mx.write_trailer().expect("trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    oxideav_mkv::demux::open(Box::new(std::io::Cursor::new(bytes)), &NullCodecResolver)
        .expect("reopen")
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[test]
fn vp9_features_list_states_the_layout() {
    // WebM "VP9 Codec Feature Metadata": profile 1, bit depth 8,
    // chroma subsampling 2 (4:2:2) — the key frame is never needed.
    let features = vec![1, 1, 1, 3, 1, 8, 4, 1, 2];
    let junk = vec![0u8; 16];
    let d = remux("vp9_420p.webm", features, None, Some(junk));
    assert_eq!(d.streams()[0].params.pixel_format, Some(Yuv422P));
    // Profile 2 fixes 4:2:0; the bit depth feature picks 12 bits.
    let d = remux(
        "vp9_420p.webm",
        vec![1, 1, 2, 3, 1, 12],
        None,
        Some(vec![0; 16]),
    );
    assert_eq!(d.streams()[0].params.pixel_format, Some(Yuv420P12Le));
}

#[test]
fn vp9_colour_element_completes_a_partial_features_list() {
    // Profile 3 alone says neither depth nor subsampling exactly; the
    // Colour element's BitsPerChannel + ChromaSubsampling* do.
    let colour = MkvVideoColour {
        bits_per_channel: 10,
        chroma_subsampling_horz: Some(0),
        chroma_subsampling_vert: Some(1),
        ..MkvVideoColour::default()
    };
    let d = remux(
        "vp9_420p.webm",
        vec![1, 1, 3],
        Some(colour),
        Some(vec![0; 16]),
    );
    assert_eq!(d.streams()[0].params.pixel_format, Some(Yuv440P10Le));
}

#[test]
fn vp9_without_a_statement_or_key_frame_stays_unknown() {
    let d = remux("vp9_420p.webm", Vec::new(), None, Some(vec![0; 16]));
    assert_eq!(d.streams()[0].params.pixel_format, None);
}

#[test]
fn av1c_monochrome_and_444_records() {
    // av1C byte 2: tier | high_bitdepth | twelve_bit | monochrome |
    // subsampling_x | subsampling_y | chroma_sample_position(2).
    for (byte2, expect) in [
        (0b0101_1100u8, Some(Gray10Le)),
        (0b0000_0000, Some(Yuv444P)),
        (0b0110_1000, Some(Yuv422P12Le)),
        (0b0000_0100, None), // 4:4:0 is not an AV1 layout
    ] {
        let rec = vec![0x81, 0x00, byte2, 0x00];
        let d = remux("av1_420p.mkv", rec, None, None);
        assert_eq!(d.streams()[0].params.pixel_format, expect, "{byte2:08b}");
    }
}
