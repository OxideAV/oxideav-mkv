//! Streaming shapes: the layouts a live or still-running writer
//! produces, and the ones RFC 9559 allows but a finalised file rarely
//! shows.
//!
//! * `Void` (RFC 8794 §11.3.2) and stray `CRC-32` elements at every
//!   level of the tree;
//! * an unknown-size Cluster inside a bounded Segment;
//! * concatenated Segments (RFC 9559 §4.5 / §23.2, RFC 8794 §9 EBML
//!   Stream) with a different `TimestampScale`, renumbered tracks, a
//!   changed track shape and a track the first Segment did not have;
//! * the three lacing kinds (§10.3) — frame timestamps from
//!   `DefaultDuration` and from `BlockDuration`, a one-frame lace, and a
//!   sweep of hostile lace bodies;
//! * explicit Block timestamps next to a `DefaultDuration`;
//! * a file that is still being written: fed to the demuxer a few bytes
//!   at a time, every packet comes out exactly once and nothing but a
//!   clean end is ever reported.
//!
//! Every fixture is built in the test from the element definitions.

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, NullCodecResolver, Packet, ReadSeek,
    SampleFormat, StreamInfo, TimeBase,
};
use oxideav_mkv::demux::{MkvDemuxer, open_resilient_typed, open_typed};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

// ---------------------------------------------------------------------
// EBML builders
// ---------------------------------------------------------------------

fn el(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = write_element_id(id);
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

/// Master with the 1-octet unknown-size VINT.
fn el_unknown(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = write_element_id(id);
    out.push(0xFF);
    out.extend_from_slice(body);
    out
}

fn uint(id: u32, value: u64) -> Vec<u8> {
    let n = ((64 - value.leading_zeros()).div_ceil(8) as usize).max(1);
    el(id, &value.to_be_bytes()[8 - n..])
}

fn string(id: u32, s: &str) -> Vec<u8> {
    el(id, s.as_bytes())
}

fn float(id: u32, v: f64) -> Vec<u8> {
    el(id, &v.to_be_bytes())
}

fn void(payload: usize) -> Vec<u8> {
    el(ids::VOID, &vec![0u8; payload])
}

/// A `CRC-32` element holding a value that matches nothing. It is only
/// meaningful as the first child of a master; anywhere else (and with a
/// wrong value) a reader has to step over it.
fn stray_crc() -> Vec<u8> {
    el(ids::CRC32, &[0xDE, 0xAD, 0xBE, 0xEF])
}

fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

fn ebml_header(doc_type: &str) -> Vec<u8> {
    el(
        ids::EBML_HEADER,
        &cat(&[
            uint(ids::EBML_VERSION, 1),
            uint(ids::EBML_READ_VERSION, 1),
            string(ids::EBML_DOC_TYPE, doc_type),
            uint(ids::EBML_DOC_TYPE_VERSION, 4),
            uint(ids::EBML_DOC_TYPE_READ_VERSION, 2),
        ]),
    )
}

fn info(scale_ns: u64, duration_ticks: Option<f64>) -> Vec<u8> {
    let mut body = uint(ids::TIMECODE_SCALE, scale_ns);
    if let Some(d) = duration_ticks {
        body.extend(float(ids::DURATION, d));
    }
    el(ids::INFO, &body)
}

fn audio_track(number: u64, codec: &str, rate: f64, default_duration_ns: Option<u64>) -> Vec<u8> {
    let mut body = cat(&[
        uint(ids::TRACK_NUMBER, number),
        uint(ids::TRACK_UID, 0x1000 + number),
        uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO),
        string(ids::CODEC_ID, codec),
    ]);
    if let Some(d) = default_duration_ns {
        body.extend(uint(ids::DEFAULT_DURATION, d));
    }
    body.extend(el(
        ids::AUDIO,
        &cat(&[float(ids::SAMPLING_FREQUENCY, rate), uint(ids::CHANNELS, 2)]),
    ));
    el(ids::TRACK_ENTRY, &body)
}

fn video_track(number: u64, codec: &str, w: u64, h: u64) -> Vec<u8> {
    el(
        ids::TRACK_ENTRY,
        &cat(&[
            uint(ids::TRACK_NUMBER, number),
            uint(ids::TRACK_UID, 0x2000 + number),
            uint(ids::TRACK_TYPE, ids::TRACK_TYPE_VIDEO),
            string(ids::CODEC_ID, codec),
            el(
                ids::VIDEO,
                &cat(&[uint(ids::PIXEL_WIDTH, w), uint(ids::PIXEL_HEIGHT, h)]),
            ),
        ]),
    )
}

/// Block / SimpleBlock body: track VINT, relative timestamp, flags, data.
fn block_body(track: u64, rel: i16, flags: u8, data: &[u8]) -> Vec<u8> {
    let mut out = write_vint(track, 0);
    out.extend_from_slice(&rel.to_be_bytes());
    out.push(flags);
    out.extend_from_slice(data);
    out
}

fn simple_block(track: u64, rel: i16, flags: u8, data: &[u8]) -> Vec<u8> {
    el(ids::SIMPLE_BLOCK, &block_body(track, rel, flags, data))
}

const KEY: u8 = 0x80;
const LACE_XIPH: u8 = 0x02;
const LACE_FIXED: u8 = 0x04;
const LACE_EBML: u8 = 0x06;

fn open_strict(bytes: Vec<u8>) -> MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    open_typed(rs, &NullCodecResolver).expect("open")
}

/// (stream, pts, duration, key, data) of every packet up to the end.
type Row = (u32, i64, Option<i64>, bool, Vec<u8>);

fn drain(dmx: &mut MkvDemuxer) -> Vec<Row> {
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => out.push((
                p.stream_index,
                p.pts.unwrap(),
                p.duration,
                p.flags.keyframe,
                p.data,
            )),
            Err(Error::Eof) => return out,
            Err(e) => panic!("demux error: {e}"),
        }
    }
}

// ---------------------------------------------------------------------
// Void / CRC-32 anywhere
// ---------------------------------------------------------------------

#[test]
fn void_and_stray_crc_at_every_level() {
    let mut ebml = uint(ids::EBML_VERSION, 1);
    ebml.extend(void(3));
    ebml.extend(string(ids::EBML_DOC_TYPE, "matroska"));
    ebml.extend(void(0));
    let header = el(ids::EBML_HEADER, &ebml);

    let track_entry = el(
        ids::TRACK_ENTRY,
        &cat(&[
            void(1),
            uint(ids::TRACK_NUMBER, 1),
            stray_crc(),
            uint(ids::TRACK_UID, 77),
            void(9),
            uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO),
            string(ids::CODEC_ID, "A_PCM/INT/LIT"),
            el(
                ids::AUDIO,
                &cat(&[
                    void(2),
                    float(ids::SAMPLING_FREQUENCY, 48_000.0),
                    stray_crc(),
                    uint(ids::CHANNELS, 2),
                    void(0),
                ]),
            ),
            void(4),
        ]),
    );
    let tracks = el(ids::TRACKS, &cat(&[void(5), track_entry, void(5)]));
    let info = el(
        ids::INFO,
        &cat(&[void(7), uint(ids::TIMECODE_SCALE, 1_000_000), void(1)]),
    );
    let group = el(
        ids::BLOCK_GROUP,
        &cat(&[
            void(3),
            el(ids::BLOCK, &block_body(1, 20, 0, b"group")),
            stray_crc(),
            uint(ids::BLOCK_DURATION, 15),
            void(11),
        ]),
    );
    let cluster_a = el(
        ids::CLUSTER,
        &cat(&[
            void(2),
            uint(ids::TIMECODE, 100),
            void(300),
            simple_block(1, 0, KEY, b"first"),
            stray_crc(),
            void(1),
            group,
            void(0),
        ]),
    );
    let cluster_b = el_unknown(
        ids::CLUSTER,
        &cat(&[
            uint(ids::TIMECODE, 200),
            void(40),
            simple_block(1, 5, KEY, b"third"),
            void(6),
        ]),
    );
    let segment = el_unknown(
        ids::SEGMENT,
        &cat(&[
            void(10),
            info,
            void(2),
            stray_crc(),
            tracks,
            void(100),
            cluster_a,
            void(17),
            stray_crc(),
            cluster_b,
        ]),
    );
    let mut dmx = open_strict(cat(&[header, segment]));
    let rows = drain(&mut dmx);
    assert_eq!(
        rows,
        vec![
            (0, 100, None, true, b"first".to_vec()),
            (0, 120, Some(15), true, b"group".to_vec()),
            (0, 205, None, true, b"third".to_vec()),
        ]
    );
}

// ---------------------------------------------------------------------
// Unknown-size Cluster inside a bounded Segment
// ---------------------------------------------------------------------

#[test]
fn unknown_size_clusters_in_a_bounded_segment() {
    let tracks = el(
        ids::TRACKS,
        &audio_track(1, "A_PCM/INT/LIT", 48_000.0, None),
    );
    let c = |ts: u64, tag: &[u8]| {
        el_unknown(
            ids::CLUSTER,
            &cat(&[uint(ids::TIMECODE, ts), simple_block(1, 0, KEY, tag)]),
        )
    };
    let segment = el(
        ids::SEGMENT,
        &cat(&[
            info(1_000_000, None),
            tracks,
            c(0, b"a"),
            c(10, b"b"),
            c(20, b"c"),
        ]),
    );
    // Bytes after the Segment that are not an EBML document are not read.
    let mut file = cat(&[ebml_header("matroska"), segment]);
    file.extend_from_slice(&[0x55; 64]);
    let mut dmx = open_strict(file);
    let pts: Vec<i64> = drain(&mut dmx).iter().map(|r| r.1).collect();
    assert_eq!(pts, vec![0, 10, 20]);
    assert_eq!(dmx.segments().len(), 1);
    // The end stays an end.
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
}

// ---------------------------------------------------------------------
// Concatenated Segments
// ---------------------------------------------------------------------

/// Three documents:
///
/// 1. scale 1 ms, audio track 1 (48 kHz, DefaultDuration 20 ms) and
///    video track 2 (640x360), bounded Segment with `Duration` 60;
/// 2. scale 0.5 ms, the same tracks renumbered (audio 7, video 3), the
///    video now 1280x720, bounded Segment without `Duration`;
/// 3. scale 2 ms, unknown-size Segment and Clusters, audio track 1 at
///    44.1 kHz plus a Vorbis track the first Segment never had.
fn chained_file() -> Vec<u8> {
    let doc1 = {
        let tracks = el(
            ids::TRACKS,
            &cat(&[
                audio_track(1, "A_PCM/INT/LIT", 48_000.0, Some(20_000_000)),
                video_track(2, "V_FFV1", 640, 360),
            ]),
        );
        let cluster = el(
            ids::CLUSTER,
            &cat(&[
                uint(ids::TIMECODE, 0),
                simple_block(2, 0, KEY, b"v1-0"),
                simple_block(1, 0, KEY, b"a1-0"),
                simple_block(1, 20, KEY, b"a1-1"),
                simple_block(2, 33, 0, b"v1-1"),
                simple_block(1, 40, KEY, b"a1-2"),
            ]),
        );
        cat(&[
            ebml_header("matroska"),
            el(
                ids::SEGMENT,
                &cat(&[info(1_000_000, Some(60.0)), tracks, cluster]),
            ),
        ])
    };
    let doc2 = {
        let tracks = el(
            ids::TRACKS,
            &cat(&[
                video_track(3, "V_FFV1", 1280, 720),
                audio_track(7, "A_PCM/INT/LIT", 48_000.0, Some(20_000_000)),
            ]),
        );
        let cluster = el(
            ids::CLUSTER,
            &cat(&[
                uint(ids::TIMECODE, 10),
                simple_block(3, -10, KEY, b"v2-0"),
                simple_block(7, -10, KEY, b"a2-0"),
                simple_block(7, 30, KEY, b"a2-1"),
                simple_block(7, 70, KEY, b"a2-2"),
            ]),
        );
        cat(&[
            ebml_header("matroska"),
            void(5),
            el(ids::SEGMENT, &cat(&[info(500_000, None), tracks, cluster])),
        ])
    };
    let doc3 = {
        let tracks = el(
            ids::TRACKS,
            &cat(&[
                audio_track(4, "A_VORBIS", 44_100.0, None),
                audio_track(1, "A_PCM/INT/LIT", 44_100.0, None),
            ]),
        );
        let cluster = |ts: u64, a: &[u8], b: &[u8]| {
            el_unknown(
                ids::CLUSTER,
                &cat(&[
                    uint(ids::TIMECODE, ts),
                    simple_block(4, 0, KEY, b"vorbis"),
                    simple_block(1, 0, KEY, a),
                    simple_block(1, 5, KEY, b),
                ]),
            )
        };
        cat(&[
            ebml_header("webm"),
            el_unknown(
                ids::SEGMENT,
                &cat(&[
                    info(2_000_000, None),
                    tracks,
                    cluster(0, b"a3-0", b"a3-1"),
                    cluster(10, b"a3-2", b"a3-3"),
                ]),
            ),
        ])
    };
    cat(&[doc1, doc2, doc3])
}

fn tags(rows: &[Row]) -> Vec<(u32, i64, String)> {
    rows.iter()
        .map(|r| (r.0, r.1, String::from_utf8(r.4.clone()).unwrap()))
        .collect()
}

#[test]
fn concatenated_segments_read_as_one_timeline() {
    let mut dmx = open_strict(chained_file());
    assert_eq!(dmx.streams()[1].params.width, Some(640));
    assert_eq!(dmx.streams()[0].params.sample_rate, Some(48_000));
    let rows = drain(&mut dmx);
    let got = tags(&rows);
    let want: Vec<(u32, i64, &str)> = vec![
        // Segment 1, ticks are milliseconds.
        (1, 0, "v1-0"),
        (0, 0, "a1-0"),
        (0, 20, "a1-1"),
        (1, 33, "v1-1"),
        (0, 40, "a1-2"),
        // Segment 2 starts at 60 ms (the first Segment's Duration, which
        // is also where its last audio frame ends); its ticks are half
        // milliseconds.
        (1, 60, "v2-0"),
        (0, 60, "a2-0"),
        (0, 80, "a2-1"),
        (0, 100, "a2-2"),
        // Segment 3 starts where Segment 2's last frame ends (100 + 20);
        // its ticks are 2 ms. The Vorbis track maps to no stream.
        (0, 120, "a3-0"),
        (0, 130, "a3-1"),
        (0, 140, "a3-2"),
        (0, 150, "a3-3"),
    ];
    let want: Vec<(u32, i64, String)> = want
        .into_iter()
        .map(|(s, p, t)| (s, p, t.to_string()))
        .collect();
    assert_eq!(got, want);
    // DefaultDuration (20 ms) reaches the packets of Segment 2 in stream
    // ticks even though that Segment counts in half milliseconds.
    assert_eq!(rows[6].2, Some(20));
    // Segment 3 declares none.
    assert_eq!(rows[9].2, None);

    let segs = dmx.segments();
    assert_eq!(segs.len(), 3);
    assert_eq!(dmx.current_segment(), 2);
    assert_eq!(
        segs.iter()
            .map(|s| s.timestamp_scale_ns())
            .collect::<Vec<_>>(),
        vec![1_000_000, 500_000, 2_000_000]
    );
    assert_eq!(
        segs.iter().map(|s| s.start_ns()).collect::<Vec<_>>(),
        vec![0, 60_000_000, 120_000_000]
    );
    assert_eq!(segs[0].duration_ns(), Some(60_000_000));
    assert_eq!(
        segs.iter().map(|s| s.unknown_size()).collect::<Vec<_>>(),
        vec![false, false, true]
    );
    assert_eq!(segs[0].track_map(), &[(1, Some(0)), (2, Some(1))]);
    assert_eq!(segs[1].track_map(), &[(3, Some(1)), (7, Some(0))]);
    assert_eq!(segs[2].track_map(), &[(4, None), (1, Some(0))]);
    assert!(segs[1].document_offset() > 0 && segs[2].document_offset() > segs[1].data_offset());

    // The streams describe the Segment being read; the counter moved
    // once per Segment that changed something.
    assert_eq!(dmx.streams()[1].params.width, Some(1280));
    assert_eq!(dmx.streams()[1].params.height, Some(720));
    assert_eq!(dmx.streams()[0].params.sample_rate, Some(44_100));
    assert_eq!(dmx.stream_params_generation(), 2);

    // A seek goes back to the first Segment; reading on from there walks
    // the same chain again and keeps every Segment where it was.
    dmx.seek_to(1, 0).expect("seek to the start");
    assert_eq!(dmx.current_segment(), 0);
    let again = tags(&drain(&mut dmx));
    assert_eq!(again, want);
    assert_eq!(dmx.segments().len(), 3);
    assert_eq!(dmx.segments()[2].start_ns(), 120_000_000);
}

#[test]
fn concatenated_segments_after_an_unknown_size_segment() {
    // The live shape: every Segment has the unknown size, so the next
    // document's EBML Header is what ends the previous Cluster.
    let doc = |scale: u64, tag: &[u8]| {
        let tracks = el(
            ids::TRACKS,
            &audio_track(1, "A_PCM/INT/LIT", 48_000.0, None),
        );
        let cluster = el_unknown(
            ids::CLUSTER,
            &cat(&[
                uint(ids::TIMECODE, 0),
                simple_block(1, 0, KEY, tag),
                simple_block(1, 10, KEY, tag),
            ]),
        );
        cat(&[
            ebml_header("matroska"),
            el_unknown(ids::SEGMENT, &cat(&[info(scale, None), tracks, cluster])),
        ])
    };
    let file = cat(&[
        doc(1_000_000, b"x"),
        doc(1_000_000, b"y"),
        doc(10_000_000, b"z"),
    ]);
    for resilient in [false, true] {
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file.clone()));
        let mut dmx = if resilient {
            open_resilient_typed(rs, &NullCodecResolver).unwrap()
        } else {
            open_typed(rs, &NullCodecResolver).unwrap()
        };
        let got = tags(&drain(&mut dmx));
        let pts: Vec<i64> = got.iter().map(|g| g.1).collect();
        // No durations anywhere: each Segment starts at the last
        // timestamp of the one before.
        assert_eq!(pts, vec![0, 10, 10, 20, 20, 120], "resilient={resilient}");
        assert_eq!(dmx.segments().len(), 3);
        assert!(dmx.damage_events().is_empty());
    }
}

#[test]
fn a_foreign_document_after_the_segment_ends_the_stream() {
    let tracks = el(
        ids::TRACKS,
        &audio_track(1, "A_PCM/INT/LIT", 48_000.0, None),
    );
    let cluster = el(
        ids::CLUSTER,
        &cat(&[uint(ids::TIMECODE, 0), simple_block(1, 0, KEY, b"only")]),
    );
    let mut file = cat(&[
        ebml_header("matroska"),
        el(
            ids::SEGMENT,
            &cat(&[info(1_000_000, None), tracks, cluster]),
        ),
    ]);
    // Another EBML document, of a DocType this crate does not read.
    file.extend(ebml_header("somethingelse"));
    file.extend(el(ids::SEGMENT, b"opaque"));
    let mut dmx = open_strict(file);
    assert_eq!(drain(&mut dmx).len(), 1);
    assert_eq!(dmx.segments().len(), 1);
    assert!(matches!(dmx.next_packet(), Err(Error::Eof)));
}

// ---------------------------------------------------------------------
// Lacing
// ---------------------------------------------------------------------

fn lace_file(default_duration_ns: Option<u64>, cluster_children: Vec<u8>) -> Vec<u8> {
    let tracks = el(
        ids::TRACKS,
        &audio_track(1, "A_PCM/INT/LIT", 48_000.0, default_duration_ns),
    );
    let mut body = uint(ids::TIMECODE, 1000);
    body.extend(cluster_children);
    cat(&[
        ebml_header("matroska"),
        el(
            ids::SEGMENT,
            &cat(&[info(1_000_000, None), tracks, el(ids::CLUSTER, &body)]),
        ),
    ])
}

/// Three frames of 300 / 2 / 5 bytes laced each way.
fn three_frames() -> [Vec<u8>; 3] {
    [vec![0xA1; 300], vec![0xB2; 2], vec![0xC3; 5]]
}

fn xiph_lace(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![(frames.len() - 1) as u8];
    for f in &frames[..frames.len() - 1] {
        let mut n = f.len();
        while n >= 255 {
            out.push(255);
            n -= 255;
        }
        out.push(n as u8);
    }
    for f in frames {
        out.extend_from_slice(f);
    }
    out
}

fn ebml_lace(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![(frames.len() - 1) as u8];
    out.extend(write_vint(frames[0].len() as u64, 0));
    for w in frames[..frames.len() - 1].windows(2) {
        // Signed difference in a 2-octet VINT: bias 2^13 - 1.
        let diff = w[1].len() as i64 - w[0].len() as i64;
        out.extend(write_vint((diff + 8191) as u64, 2));
    }
    for f in frames {
        out.extend_from_slice(f);
    }
    out
}

fn fixed_lace(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![(frames.len() - 1) as u8];
    for f in frames {
        out.extend_from_slice(f);
    }
    out
}

fn fixed_frames() -> [Vec<u8>; 3] {
    [vec![1; 4], vec![2; 4], vec![3; 4]]
}

/// One Block of each lacing kind, plus a laced Block in a BlockGroup
/// with a BlockDuration.
fn laced_cluster_children() -> Vec<u8> {
    let frames = three_frames();
    cat(&[
        simple_block(1, 0, KEY | LACE_XIPH, &xiph_lace(&frames)),
        simple_block(1, 60, KEY | LACE_EBML, &ebml_lace(&frames)),
        simple_block(1, 120, KEY | LACE_FIXED, &fixed_lace(&fixed_frames())),
        // BlockDuration covers the whole Block: 30 ticks over 3 frames.
        el(
            ids::BLOCK_GROUP,
            &cat(&[
                el(
                    ids::BLOCK,
                    &block_body(1, 180, LACE_XIPH, &xiph_lace(&frames)),
                ),
                uint(ids::BLOCK_DURATION, 30),
            ]),
        ),
    ])
}

#[test]
fn laced_frames_are_spaced_by_default_duration_or_block_duration() {
    let frames = three_frames();
    let fixed = fixed_frames();
    let children = laced_cluster_children();
    // DefaultDuration 20 ms.
    let mut dmx = open_strict(lace_file(Some(20_000_000), children.clone()));
    let rows = drain(&mut dmx);
    let pts: Vec<i64> = rows.iter().map(|r| r.1).collect();
    assert_eq!(
        pts,
        vec![
            1000, 1020, 1040, 1060, 1080, 1100, 1120, 1140, 1160, 1180, 1190, 1200
        ]
    );
    for (i, r) in rows.iter().enumerate() {
        let want = if i < 9 { 20 } else { 10 };
        assert_eq!(r.2, Some(want), "frame {i} duration");
    }
    for chunk in [&rows[0..3], &rows[3..6], &rows[9..12]] {
        let data: Vec<&[u8]> = chunk.iter().map(|r| r.4.as_slice()).collect();
        assert_eq!(data, vec![&frames[0][..], &frames[1][..], &frames[2][..]]);
    }
    assert_eq!(rows[7].4, fixed[1]);

    // Without a DefaultDuration the frames of a plain lace share the
    // Block's timestamp (the lace does not say more); the BlockGroup's
    // are still spread over its BlockDuration.
    let mut dmx = open_strict(lace_file(None, children));
    let rows = drain(&mut dmx);
    let pts: Vec<i64> = rows.iter().map(|r| r.1).collect();
    assert_eq!(
        pts,
        vec![
            1000, 1000, 1000, 1060, 1060, 1060, 1120, 1120, 1120, 1180, 1190, 1200
        ]
    );
    assert_eq!(rows[0].2, None);
    assert_eq!(rows[11].2, Some(10));
}

#[test]
fn ebml_lace_sizes_at_the_top_of_a_width_round_trip() {
    use oxideav_mkv::mux::{LacingMode, MkvMuxer};
    // Table 37: each width's signed range ends on the all-ones pattern
    // (+2^6 in one octet, +2^13 in two). A reader that takes all-ones
    // for the Element Data Size "unknown" marker cannot read a frame 64
    // (or 8192) bytes longer than its predecessor. Found by the
    // `mux_roundtrip` fuzz target; `fuzz/corpus/mux_roundtrip/
    // regression_ebml_lace_delta_all_ones.bin` is the minimised input.
    let tb = TimeBase::new(1, 1000);
    let mut ap = CodecParameters::audio(CodecId::new("pcm_s16le"));
    ap.sample_rate = Some(48_000);
    ap.channels = Some(2);
    ap.sample_format = Some(SampleFormat::S16);
    let streams = vec![StreamInfo {
        index: 0,
        time_base: tb,
        duration: None,
        start_time: Some(0),
        params: ap,
    }];
    // Deltas: +64, -63 (the bottom of one octet), +8192, -8191, +63, +65.
    let sizes = [10usize, 74, 11, 8203, 12, 75, 140, 3];
    let path = std::env::temp_dir().join(format!("oxideav-mkv-lace-{}.mkv", std::process::id()));
    {
        let f = std::fs::File::create(&path).unwrap();
        let mut mux = MkvMuxer::new_matroska(Box::new(f), &streams).unwrap();
        mux.with_block_lacing(LacingMode::Ebml).unwrap();
        mux.write_header().unwrap();
        for (i, &n) in sizes.iter().enumerate() {
            let mut p = Packet::new(0, tb, vec![i as u8 + 1; n]);
            p.pts = Some(i as i64);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    // The lace is on disk the way the table says: a one-octet all-ones
    // delta follows the first size.
    assert!(
        bytes.windows(3).any(|w| w == [0x8A, 0xFF, 0x80]),
        "10, +64, -63"
    );
    let mut dmx = open_strict(bytes);
    let rows = drain(&mut dmx);
    assert_eq!(
        rows.iter().map(|r| r.4.len()).collect::<Vec<_>>(),
        sizes.to_vec()
    );
    for (i, r) in rows.iter().enumerate() {
        assert!(r.4.iter().all(|&b| b == i as u8 + 1), "frame {i} bytes");
    }
}

#[test]
fn a_one_frame_lace_is_one_frame() {
    // §10.3 forbids writing it, but the bytes are unambiguous: the lace
    // head says one frame, so there are no sizes and the rest is data.
    for flag in [LACE_XIPH, LACE_EBML, LACE_FIXED] {
        let mut body = vec![0u8];
        body.extend_from_slice(b"\x83payload");
        let mut dmx = open_strict(lace_file(None, simple_block(1, 0, KEY | flag, &body)));
        let rows = drain(&mut dmx);
        assert_eq!(rows.len(), 1, "lacing bits {flag:#x}");
        assert_eq!(rows[0].4, b"\x83payload");
    }
}

#[test]
fn explicit_block_timestamps_win_over_default_duration() {
    // DefaultDuration says 20 ms per frame; the Blocks are 20, 20, 23
    // and 17 ms apart. Each packet keeps the timestamp its Block states.
    let children = cat(&[
        simple_block(1, 0, KEY, b"a"),
        simple_block(1, 20, KEY, b"b"),
        simple_block(1, 43, KEY, b"c"),
        simple_block(1, 60, KEY, b"d"),
        // A Block before the Cluster Timestamp (negative offset).
        simple_block(1, -5, KEY, b"e"),
    ]);
    let mut dmx = open_strict(lace_file(Some(20_000_000), children));
    let rows = drain(&mut dmx);
    assert_eq!(
        rows.iter().map(|r| r.1).collect::<Vec<_>>(),
        vec![1000, 1020, 1043, 1060, 995]
    );
    assert!(rows.iter().all(|r| r.2 == Some(20)));
}

/// Deterministic generator (xorshift) for the hostile sweeps.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn hostile_lace_bodies_never_panic_or_invent_bytes() {
    let mut rng = Rng(0x5EED_1ACE_D00D_F00D);
    for case in 0..6000u32 {
        let flag = [LACE_XIPH, LACE_EBML, LACE_FIXED][(case % 3) as usize];
        let len = (rng.next() % 40) as usize;
        let mut body: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        // Bias towards the interesting values.
        if !body.is_empty() && case % 5 == 0 {
            body[0] = 0xFF;
        }
        if body.len() > 2 && case % 7 == 0 {
            body[1] = 0xFF;
            body[2] = 0xFF;
        }
        if body.len() > 9 && case % 11 == 0 {
            // An 8-octet VINT holding the largest value.
            body[1..10].copy_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE]);
        }
        let file = lace_file(
            Some(20_000_000),
            cat(&[
                simple_block(1, 0, KEY | flag, &body),
                simple_block(1, 100, KEY, b"after"),
            ]),
        );
        // Strict: the Block either de-laces or is refused.
        let mut dmx = open_strict(file.clone());
        let mut total = 0usize;
        while let Ok(p) = dmx.next_packet() {
            total += p.data.len();
        }
        assert!(
            total <= body.len() + 5,
            "case {case}: more bytes out than in"
        );
        // Resilient: a refused Block costs that Block, not the stream.
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
        let mut dmx = open_resilient_typed(rs, &NullCodecResolver).unwrap();
        let mut last = Vec::new();
        loop {
            match dmx.next_packet() {
                Ok(p) => last = p.data,
                Err(Error::Eof) => break,
                Err(e) => panic!("case {case}: resilient error {e}"),
            }
        }
        assert_eq!(last, b"after", "case {case}: the next Block survives");
    }
}

// ---------------------------------------------------------------------
// A file that is still being written
// ---------------------------------------------------------------------

/// A reader over a buffer another handle keeps appending to.
#[derive(Clone)]
struct Growing {
    data: Arc<Mutex<Vec<u8>>>,
    pos: u64,
}

impl Read for Growing {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let data = self.data.lock().unwrap();
        let start = (self.pos as usize).min(data.len());
        let n = buf.len().min(data.len() - start);
        buf[..n].copy_from_slice(&data[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Growing {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let len = self.data.lock().unwrap().len() as i64;
        let next = match to {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(d) => len + d,
            SeekFrom::Current(d) => self.pos as i64 + d,
        };
        if next < 0 {
            return Err(std::io::Error::other("seek before start"));
        }
        self.pos = next as u64;
        Ok(self.pos)
    }
}

/// A live-layout mux (unknown-size Segment and Clusters, nothing written
/// behind the write position) of two tracks, with laced and unlaced
/// Blocks.
/// (stream, pts, data) of a packet.
type Sent = (u32, i64, Vec<u8>);

fn live_capture() -> (Vec<u8>, Vec<Sent>) {
    let tb = TimeBase::new(1, 1000);
    let mut ap = CodecParameters::audio(CodecId::new("pcm_s16le"));
    ap.sample_rate = Some(48_000);
    ap.channels = Some(2);
    ap.sample_format = Some(SampleFormat::S16);
    let mut vp = CodecParameters::video(CodecId::new("ffv1"));
    vp.width = Some(64);
    vp.height = Some(48);
    let streams = vec![
        StreamInfo {
            index: 0,
            time_base: tb,
            duration: None,
            start_time: Some(0),
            params: vp,
        },
        StreamInfo {
            index: 1,
            time_base: tb,
            duration: None,
            start_time: Some(0),
            params: ap,
        },
    ];
    let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
    struct Sink(Arc<Mutex<Cursor<Vec<u8>>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for Sink {
        fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
            self.0.lock().unwrap().seek(p)
        }
    }
    let mut mux =
        oxideav_mkv::mux::MkvMuxer::new_matroska(Box::new(Sink(sink.clone())), &streams).unwrap();
    mux.with_live_streaming().unwrap();
    mux.with_cluster_limits(100, 1024).unwrap();
    mux.write_header().unwrap();
    let mut expect = Vec::new();
    for i in 0..40i64 {
        let (stream, len) = if i % 4 == 0 {
            (0u32, 300usize)
        } else {
            (1u32, 24usize)
        };
        let data: Vec<u8> = (0..len).map(|k| (i as usize * 7 + k) as u8).collect();
        let mut p = Packet::new(stream, tb, data.clone());
        p.pts = Some(i * 10);
        p.flags.keyframe = stream == 1 || i % 8 == 0;
        mux.write_packet(&p).unwrap();
        expect.push((stream, i * 10, data));
    }
    mux.write_trailer().unwrap();
    drop(mux);
    let bytes = sink.lock().unwrap().get_ref().clone();
    (bytes, expect)
}

#[test]
fn a_growing_file_yields_every_packet_exactly_once() {
    let (bytes, expect) = live_capture();
    // The writer has got as far as the first Cluster's first Block
    // before the reader opens the file.
    let first_cluster = bytes
        .windows(4)
        .position(|w| w == [0x1F, 0x43, 0xB6, 0x75])
        .unwrap();
    for step in [1usize, 3, 7, 64] {
        let shared = Arc::new(Mutex::new(bytes[..first_cluster].to_vec()));
        let rs: Box<dyn ReadSeek> = Box::new(Growing {
            data: shared.clone(),
            pos: 0,
        });
        let mut dmx = open_typed(rs, &NullCodecResolver).expect("open on headers only");
        let mut got = Vec::new();
        let mut written = first_cluster;
        let mut stalls = 0usize;
        loop {
            loop {
                match dmx.next_packet() {
                    Ok(p) => got.push((p.stream_index, p.pts.unwrap(), p.data)),
                    Err(Error::Eof) => break,
                    Err(e) => panic!("step {step}: at {written} bytes: {e}"),
                }
            }
            if written == bytes.len() {
                break;
            }
            // Everything read so far is a prefix of what was written.
            assert!(got[..] == expect[..got.len()], "step {step}: not a prefix");
            if dmx.incomplete_tail().is_some() {
                stalls += 1;
            }
            let next = (written + step).min(bytes.len());
            shared
                .lock()
                .unwrap()
                .extend_from_slice(&bytes[written..next]);
            written = next;
        }
        assert_eq!(got.len(), expect.len(), "step {step}");
        assert!(got == expect, "step {step}: packets differ");
        assert_eq!(dmx.incomplete_tail(), None);
        assert!(stalls > 0, "the reader did stop inside elements");
    }
}

#[test]
fn a_capture_cut_anywhere_ends_cleanly_in_strict_mode() {
    let (bytes, expect) = live_capture();
    let first_cluster = bytes
        .windows(4)
        .position(|w| w == [0x1F, 0x43, 0xB6, 0x75])
        .unwrap();
    for cut in first_cluster..=bytes.len() {
        let mut dmx = open_strict(bytes[..cut].to_vec());
        let mut got = Vec::new();
        loop {
            match dmx.next_packet() {
                Ok(p) => got.push((p.stream_index, p.pts.unwrap(), p.data)),
                Err(Error::Eof) => break,
                Err(e) => panic!("cut {cut}: {e}"),
            }
        }
        assert!(got[..] == expect[..got.len()], "cut {cut}: not a prefix");
        if cut == bytes.len() {
            assert_eq!(got.len(), expect.len());
        }
    }
}

// ---------------------------------------------------------------------
// Priming, pre-roll and end trim
// ---------------------------------------------------------------------

#[test]
fn codec_delay_pre_roll_and_discard_padding_reach_the_consumer() {
    use oxideav_mkv::mux::{BlockGroupOptions, MkvMuxer, MkvTrackCodecTiming};

    let tb = TimeBase::new(1, 1000);
    let mut ap = CodecParameters::audio(CodecId::new("pcm_s16le"));
    ap.sample_rate = Some(48_000);
    ap.channels = Some(2);
    ap.sample_format = Some(SampleFormat::S16);
    let streams = vec![StreamInfo {
        index: 0,
        time_base: tb,
        duration: None,
        start_time: Some(0),
        params: ap,
    }];
    let path = std::env::temp_dir().join(format!("oxideav-mkv-priming-{}.mkv", std::process::id()));
    {
        let f = std::fs::File::create(&path).unwrap();
        let mut mux = MkvMuxer::new_matroska(Box::new(f), &streams).unwrap();
        // 312 samples of priming at 48 kHz, 80 ms of pre-roll.
        mux.set_track_codec_timing(
            0,
            MkvTrackCodecTiming::new(Some(6_500_000), Some(80_000_000)),
        )
        .unwrap();
        mux.write_header().unwrap();
        let packet = |i: i64| {
            let mut p = Packet::new(0, tb, vec![i as u8; 16]);
            p.pts = Some(i * 20);
            p.duration = Some(20);
            p.flags.keyframe = true;
            p
        };
        mux.write_packet(&packet(0)).unwrap();
        mux.write_packet(&packet(1)).unwrap();
        // The last frame carrying sound: 13.5 ms of it is padding.
        mux.write_packet_with_block_group(
            &packet(2),
            &BlockGroupOptions::default().with_discard_padding(Some(13_500_000)),
        )
        .unwrap();
        // Padding as long as the stated duration: §5.1.3.5.3 does not
        // say whether the duration counts the padding, so the frame is
        // not declared silent.
        mux.write_packet_with_block_group(
            &packet(3),
            &BlockGroupOptions::default().with_discard_padding(Some(20_000_000)),
        )
        .unwrap();
        // A frame that is padding from end to end: nothing of it lasts.
        let mut silent = packet(4);
        silent.duration = Some(0);
        mux.write_packet_with_block_group(
            &silent,
            &BlockGroupOptions::default().with_discard_padding(Some(20_000_000)),
        )
        .unwrap();
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let mut dmx = open_strict(bytes);

    let meta = |key: &str| {
        dmx.metadata()
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(meta("track:0:codec_delay_ns").as_deref(), Some("6500000"));
    assert_eq!(meta("track:0:priming_samples").as_deref(), Some("312"));
    assert_eq!(
        meta("track:0:seek_pre_roll_ns").as_deref(),
        Some("80000000")
    );
    let timing = *dmx.track_codec_timing(0).unwrap();
    assert_eq!(timing.priming_samples(48_000), 312);
    assert_eq!(timing.seek_pre_roll_samples(48_000), 3840);

    let mut seen = Vec::new();
    while let Ok(p) = dmx.next_packet() {
        let trim = dmx
            .block_group_meta()
            .map_or((0, 0), |m| m.trim_samples(48_000));
        seen.push((p.pts.unwrap(), p.duration, p.flags.discard, trim));
    }
    assert_eq!(
        seen,
        vec![
            (0, None, false, (0, 0)),
            (20, None, false, (0, 0)),
            (40, Some(20), false, (0, 648)),
            (60, Some(20), false, (0, 960)),
            (80, Some(0), true, (0, 960)),
        ]
    );
}

// ---------------------------------------------------------------------
// Fuzz seeds
// ---------------------------------------------------------------------

/// The streaming shapes above are also the demux fuzz target's seeds, so
/// mutation starts from concatenated Segments, laces of every kind and a
/// capture cut inside its last Block. The files under
/// `fuzz/corpus/demux/` must be what the builders here produce (run with
/// `OXIDEAV_MKV_WRITE_SEEDS=1` to regenerate them after changing a
/// builder).
#[test]
fn fuzz_seeds_match_their_builders() {
    let cut = {
        let mut live = chained_file();
        live.truncate(live.len() - 7);
        live
    };
    let seeds: [(&str, Vec<u8>); 3] = [
        ("seed_chained_segments.mkv", chained_file()),
        (
            "seed_laces.mkv",
            lace_file(Some(20_000_000), laced_cluster_children()),
        ),
        ("seed_live_cut.mkv", cut),
    ];
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/demux");
    for (name, bytes) in seeds {
        let path = dir.join(name);
        if std::env::var_os("OXIDEAV_MKV_WRITE_SEEDS").is_some() {
            std::fs::write(&path, &bytes).unwrap();
        }
        let on_disk = std::fs::read(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(on_disk == bytes, "{name} differs from its builder");
    }
}
