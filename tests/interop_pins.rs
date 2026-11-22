//! Small, tool-free pins for what the black-box matrix
//! (`tests/foreign_matrix.rs`) found: one test per defect, on files
//! built here.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, CodecTag, Demuxer, Error, Muxer, NullCodecResolver, Packet, ReadSeek,
    StreamInfo, TimeBase,
};
use oxideav_mkv::demux::{MkvDemuxer, open_resilient_typed, open_typed};
use oxideav_mkv::ebml::{VINT_UNKNOWN_SIZE, read_element_header, write_element_id, write_vint};
use oxideav_mkv::ids;
use oxideav_mkv::mux::{
    BlockGroupOptions, LacingMode, MkvAttachment, MkvBlockAddition, MkvMuxer, MkvTrackCodecTiming,
    MkvTrackTiming, MkvVideoColour,
};

const TB: TimeBase = TimeBase::new(1, 1000);

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Cursor<Vec<u8>>>>);

impl std::io::Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for Sink {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

impl Sink {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().get_ref().clone()
    }
}

fn stream(index: u32, params: CodecParameters) -> StreamInfo {
    StreamInfo {
        index,
        time_base: TB,
        duration: None,
        start_time: Some(0),
        params,
    }
}

fn video(codec: &str) -> CodecParameters {
    let mut p = CodecParameters::video(CodecId::new(codec));
    p.width = Some(64);
    p.height = Some(48);
    p
}

fn audio(codec: &str) -> CodecParameters {
    let mut p = CodecParameters::audio(CodecId::new(codec));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p
}

fn packet(stream: u32, pts: i64, duration: Option<i64>, key: bool, data: Vec<u8>) -> Packet {
    let mut p = Packet::new(stream, TB, data);
    p.pts = Some(pts);
    p.duration = duration;
    p.flags.keyframe = key;
    p
}

fn open(bytes: Vec<u8>) -> MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    open_typed(rs, &NullCodecResolver).expect("open")
}

fn drain(dmx: &mut MkvDemuxer) -> Vec<Packet> {
    let mut out = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => out.push(p),
            Err(Error::Eof) => return out,
            Err(e) => panic!("{e}"),
        }
    }
}

/// (id, declared size, body offset) of the Segment and of each of its
/// Top-Level children, in file order.
fn top_level(bytes: &[u8]) -> Vec<(u32, u64, u64)> {
    let mut cur = Cursor::new(bytes);
    let ebml = read_element_header(&mut cur).unwrap();
    cur.set_position(cur.position() + ebml.size);
    let seg = read_element_header(&mut cur).unwrap();
    assert_eq!(seg.id, ids::SEGMENT);
    let mut out = vec![(seg.id, seg.size, cur.position())];
    while (cur.position() as usize) < bytes.len() {
        let e = read_element_header(&mut cur).unwrap();
        out.push((e.id, e.size, cur.position()));
        if e.size == VINT_UNKNOWN_SIZE {
            break;
        }
        cur.set_position(cur.position() + e.size);
    }
    out
}

/// How many `SimpleBlock`s and `BlockGroup`s the sized Clusters hold.
fn block_kinds(bytes: &[u8]) -> (usize, usize) {
    let (mut simple, mut groups) = (0, 0);
    for (id, size, body) in top_level(bytes) {
        if id != ids::CLUSTER {
            continue;
        }
        let mut cur = Cursor::new(&bytes[..(body + size) as usize]);
        cur.set_position(body);
        while cur.position() < body + size {
            let e = read_element_header(&mut cur).unwrap();
            match e.id {
                ids::SIMPLE_BLOCK => simple += 1,
                ids::BLOCK_GROUP => groups += 1,
                _ => {}
            }
            cur.set_position(cur.position() + e.size);
        }
    }
    (simple, groups)
}

fn mux_with(
    streams: &[StreamInfo],
    configure: impl FnOnce(&mut MkvMuxer),
    packets: &[Packet],
) -> Vec<u8> {
    let sink = Sink::default();
    let mut mux = MkvMuxer::new_matroska(Box::new(sink.clone()), streams).unwrap();
    configure(&mut mux);
    mux.write_header().unwrap();
    for p in packets {
        mux.write_packet(p).unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    sink.bytes()
}

// ---------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------

#[test]
fn a_finished_file_states_every_size_and_its_duration() {
    let packets: Vec<Packet> = (0..30)
        .map(|i| packet(0, i * 40, Some(40), i % 10 == 0, vec![i as u8; 100]))
        .collect();
    let bytes = mux_with(
        &[stream(0, video("ffv1"))],
        |m| {
            m.with_cluster_limits(400, 1 << 20).unwrap();
        },
        &packets,
    );
    let layout = top_level(&bytes);
    // The Segment's size is what follows its header, to the last byte.
    let (_, segment_size, segment_body) = layout[0];
    assert_eq!(segment_body + segment_size, bytes.len() as u64);
    let clusters: Vec<_> = layout.iter().filter(|e| e.0 == ids::CLUSTER).collect();
    assert_eq!(clusters.len(), 3);
    assert!(clusters.iter().all(|c| c.1 != VINT_UNKNOWN_SIZE));
    // Both size fields are 8 octets wide (what the unknown-size
    // placeholder was), and the walk above ended exactly at the end.
    let (_, last_size, last_body) = *layout.last().unwrap();
    assert_eq!(last_body + last_size, bytes.len() as u64);
    let dmx = open(bytes);
    assert_eq!(dmx.duration_micros(), Some(1_200_000));
}

#[test]
fn the_live_layout_keeps_eight_octet_unknown_sizes() {
    let packets = [packet(0, 0, Some(40), true, vec![7; 50])];
    let bytes = mux_with(
        &[stream(0, video("ffv1"))],
        |m| {
            m.with_live_streaming().unwrap();
        },
        &packets,
    );
    let unknown = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    for id in [ids::SEGMENT, ids::CLUSTER] {
        let id = write_element_id(id);
        let at = bytes
            .windows(4)
            .position(|w| w == id.as_slice())
            .expect("element present");
        assert_eq!(&bytes[at + 4..at + 12], &unknown);
    }
    let mut dmx = open(bytes);
    assert_eq!(dmx.duration_micros(), None);
    assert_eq!(drain(&mut dmx).len(), 1);
}

// ---------------------------------------------------------------------
// Durations
// ---------------------------------------------------------------------

#[test]
fn subtitle_and_shortened_frames_keep_their_duration() {
    let mut sub = CodecParameters::subtitle(CodecId::new("subrip"));
    sub.language = Some("eng".into());
    let streams = [stream(0, audio("ac3")), stream(1, sub)];
    let packets = [
        packet(0, 0, Some(32), true, vec![1; 40]),
        packet(1, 5, Some(700), true, b"Hello".to_vec()),
        packet(0, 32, Some(32), true, vec![2; 40]),
        // The last frame was cut short.
        packet(0, 64, Some(11), true, vec![3; 40]),
    ];
    let bytes = mux_with(
        &streams,
        |m| {
            m.set_track_timing(
                0,
                MkvTrackTiming::default().with_default_duration(Some(32_000_000)),
            )
            .unwrap();
        },
        &packets,
    );
    // Two frames at DefaultDuration stay SimpleBlocks; the subtitle and
    // the short frame need a BlockDuration.
    assert_eq!(block_kinds(&bytes), (2, 2));
    let got: Vec<(u32, Option<i64>)> = drain(&mut open(bytes))
        .iter()
        .map(|p| (p.stream_index, p.duration))
        .collect();
    assert_eq!(
        got,
        vec![(0, Some(32)), (1, Some(700)), (0, Some(32)), (0, Some(11))]
    );
}

#[test]
fn an_opus_toc_duration_is_converted_to_the_stream_time_base() {
    // OpusHead: version 1, 2 channels, pre-skip 0.
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 2, 0, 0, 0x80, 0xBB, 0, 0, 0, 0, 0]);
    let mut params = audio("opus");
    params.extradata = head;
    // TOC 0xF8: 20 ms, one frame. No packet duration.
    let packets: Vec<Packet> = (0..3)
        .map(|i| packet(0, i * 20, None, true, vec![0xF8, 1, 2, 3]))
        .collect();
    let bytes = mux_with(&[stream(0, params)], |_| {}, &packets);
    // 3 x 20 ms — not 40 + 960 "ms" from 48 kHz samples read as ticks.
    assert_eq!(open(bytes).duration_micros(), Some(60_000));
}

// ---------------------------------------------------------------------
// Lacing
// ---------------------------------------------------------------------

#[test]
fn only_audio_frames_on_the_default_duration_grid_are_laced() {
    let streams = [
        stream(0, video("ffv1")),
        stream(1, audio("ac3")),
        stream(2, audio("aac")),
    ];
    // Video with reordered timestamps, audio on a 32 ms grid except one
    // frame 3 ms late, and an audio track whose DefaultDuration is not
    // a whole number of ticks. A lace forms from consecutive packets of
    // one track, so each track's frames are written in a run.
    let video_pts = [0, 120, 40, 80, 240, 160, 200];
    let grid = [0, 32, 64, 96, 131, 163, 195];
    let mut packets = Vec::new();
    for (i, pts) in video_pts.into_iter().enumerate() {
        packets.push(packet(0, pts, None, i == 0, vec![i as u8; 30]));
    }
    for (i, pts) in grid.into_iter().enumerate() {
        packets.push(packet(1, pts, None, true, vec![0x40 + i as u8; 20 + i]));
    }
    for i in 0..7 {
        packets.push(packet(2, (i * 64) / 3, None, true, vec![0x80 + i as u8; 9]));
    }
    let bytes = mux_with(
        &streams,
        |m| {
            m.with_block_lacing(LacingMode::Xiph).unwrap();
            m.set_track_timing(
                1,
                MkvTrackTiming::default().with_default_duration(Some(32_000_000)),
            )
            .unwrap();
            m.set_track_timing(
                2,
                MkvTrackTiming::default().with_default_duration(Some(21_333_333)),
            )
            .unwrap();
        },
        &packets,
    );
    // 7 video Blocks, 7 Blocks of the off-tick track, and the gridded
    // track in two laces (4 frames, then the 3 after the late one).
    assert_eq!(block_kinds(&bytes), (7 + 7 + 2, 0));
    let got = drain(&mut open(bytes));
    for s in 0..3u32 {
        let want: Vec<(Option<i64>, Vec<u8>)> = packets
            .iter()
            .filter(|p| p.stream_index == s)
            .map(|p| (p.pts, p.data.clone()))
            .collect();
        let have: Vec<(Option<i64>, Vec<u8>)> = got
            .iter()
            .filter(|p| p.stream_index == s)
            .map(|p| (p.pts, p.data.clone()))
            .collect();
        assert_eq!(have, want, "stream {s}");
    }
}

// ---------------------------------------------------------------------
// Track records across a copy
// ---------------------------------------------------------------------

/// A `WAVEFORMATEX` for tag 0x0161 with `nBlockAlign` 1487.
fn wma_header() -> Vec<u8> {
    let mut h = Vec::new();
    h.extend_from_slice(&0x0161u16.to_le_bytes());
    h.extend_from_slice(&2u16.to_le_bytes());
    h.extend_from_slice(&48_000u32.to_le_bytes());
    h.extend_from_slice(&16_000u32.to_le_bytes());
    h.extend_from_slice(&1487u16.to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(&4u16.to_le_bytes());
    h
}

fn codec_private(bytes: &[u8]) -> Vec<u8> {
    let id = write_element_id(ids::CODEC_PRIVATE);
    let at = bytes
        .windows(id.len())
        .position(|w| w == id.as_slice())
        .expect("CodecPrivate present");
    let mut cur = Cursor::new(&bytes[at..]);
    let e = read_element_header(&mut cur).unwrap();
    let start = at + cur.position() as usize;
    bytes[start..start + e.size as usize].to_vec()
}

#[test]
fn a_compatibility_header_survives_a_copy() {
    // First generation: the muxer has only StreamInfo to go on and
    // cannot know nBlockAlign.
    let mut params = audio("mkv:acm:0x0161");
    params.tag = Some(CodecTag::wave_format(0x0161));
    params.extradata = vec![9, 8, 7, 6];
    let first = mux_with(
        &[stream(0, params.clone())],
        |_| {},
        &[packet(0, 0, None, true, vec![1; 8])],
    );
    assert_eq!(&codec_private(&first)[12..14], &0u16.to_le_bytes());

    // A stream read from a file carries the header it was read with ...
    let mut original = wma_header();
    original.extend_from_slice(&[9, 8, 7, 6]);
    params.options.insert(
        oxideav_mkv::codec_id::COMPAT_HEADER_OPTION,
        wma_header()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
    );
    let second = mux_with(
        &[stream(0, params)],
        |_| {},
        &[packet(0, 0, None, true, vec![1; 8])],
    );
    assert_eq!(codec_private(&second), original);

    // ... which is what the demuxer hands out, so a second copy is
    // byte-identical in CodecPrivate.
    let dmx = open(second);
    let read = dmx.streams()[0].clone();
    assert_eq!(read.params.extradata, vec![9, 8, 7, 6]);
    assert!(
        read.params
            .options
            .get(oxideav_mkv::codec_id::COMPAT_HEADER_OPTION)
            .is_some()
    );
    let third = mux_with(&[read], |_| {}, &[packet(0, 0, None, true, vec![1; 8])]);
    assert_eq!(codec_private(&third), original);
}

#[test]
fn uncompressed_video_needs_and_keeps_its_fourcc() {
    let streams = [stream(0, video("rawvideo"))];
    let sink = Sink::default();
    let mut mux = MkvMuxer::new_matroska(Box::new(sink), &streams).unwrap();
    assert!(matches!(mux.write_header(), Err(Error::Unsupported(_))));

    let bytes = mux_with(
        &streams,
        |m| {
            m.set_video_uncompressed_fourcc(0, *b"I420").unwrap();
        },
        &[packet(0, 0, None, true, vec![0; 64 * 48 * 3 / 2])],
    );
    let dmx = open(bytes);
    let read = dmx.streams()[0].clone();
    assert_eq!(read.params.codec_id.as_str(), "rawvideo");
    assert_eq!(
        dmx.video_uncompressed_fourcc(0).and_then(|f| f.fourcc()),
        Some(*b"I420")
    );
    // The stream alone is enough for the next muxer.
    let again = mux_with(&[read], |_| {}, &[packet(0, 0, None, true, vec![0; 16])]);
    let dmx = open(again);
    assert_eq!(
        dmx.video_uncompressed_fourcc(0).and_then(|f| f.fourcc()),
        Some(*b"I420")
    );
}

#[test]
fn flat_metadata_goes_back_into_the_file() {
    let streams = [stream(0, video("ffv1")), stream(1, audio("aac"))];
    let pairs: Vec<(String, String)> = [
        ("title", "A title"),
        ("artist", "Somebody"),
        ("tag:track:1:encoder", "an encoder"),
        ("track:1:codec_delay_ns", "21333333"),
        ("track:1:seek_pre_roll_ns", "80000000"),
        // Not stored: the file states these through other elements.
        ("muxer", "something else"),
        ("track:1:priming_samples", "1024"),
        ("tag:track:0:duration", "00:00:01.000"),
        ("chapter:0:title", "One"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let sink = Sink::default();
    let mut mux = MkvMuxer::new_matroska(Box::new(sink.clone()), &streams).unwrap();
    for (key, carried) in [
        ("title", true),
        ("artist", true),
        ("tag:track:1:encoder", true),
        ("track:1:codec_delay_ns", true),
        ("muxer", false),
        ("track:1:priming_samples", false),
        ("tag:track:7:encoder", false),
        ("chapter:0:title", false),
    ] {
        assert_eq!(mux.carries_metadata_key(key), carried, "{key}");
    }
    mux.set_metadata(&pairs).unwrap();
    mux.write_header().unwrap();
    assert!(mux.set_metadata(&[]).is_err(), "refused after the header");
    mux.write_packet(&packet(0, 0, None, true, vec![1; 8]))
        .unwrap();
    mux.write_packet(&packet(1, -21, None, true, vec![2; 8]))
        .unwrap();
    mux.write_trailer().unwrap();
    drop(mux);

    let mut dmx = open(sink.bytes());
    let meta = |key: &str| {
        dmx.metadata()
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(meta("title").as_deref(), Some("A title"));
    assert_eq!(meta("artist").as_deref(), Some("Somebody"));
    assert_eq!(meta("tag:track:1:encoder").as_deref(), Some("an encoder"));
    assert_eq!(meta("track:1:codec_delay_ns").as_deref(), Some("21333333"));
    assert_eq!(
        meta("track:1:seek_pre_roll_ns").as_deref(),
        Some("80000000")
    );
    assert_eq!(meta("muxer").as_deref(), Some("oxideav"));
    assert_eq!(meta("chapter:0:title"), None);
    // The audio packet written at -21 comes back at -21 (§11.2).
    let audio: Vec<Option<i64>> = drain(&mut dmx)
        .iter()
        .filter(|p| p.stream_index == 1)
        .map(|p| p.pts)
        .collect();
    assert_eq!(audio, vec![Some(-21)]);
}

#[test]
fn a_typed_copy_keeps_headers_side_data_chapters_and_attachments() {
    let streams = [stream(0, video("vp9")), stream(1, audio("opus_like"))];
    let mut a = streams.to_vec();
    a[1].params.tag = Some(CodecTag::matroska("A_EXAMPLE"));
    let sink = Sink::default();
    let mut mux = MkvMuxer::new_matroska(Box::new(sink.clone()), &a).unwrap();
    mux.set_video_colour(0, MkvVideoColour::bt709()).unwrap();
    mux.set_max_block_addition_id(0, 1).unwrap();
    mux.set_track_timing(
        0,
        MkvTrackTiming::default().with_default_duration(Some(40_000_000)),
    )
    .unwrap();
    mux.set_track_codec_timing(
        1,
        MkvTrackCodecTiming::new(Some(6_500_000), Some(80_000_000)),
    )
    .unwrap();
    mux.set_track_timing(
        1,
        MkvTrackTiming::default().with_default_duration(Some(20_000_000)),
    )
    .unwrap();
    mux.set_title("Copy me").unwrap();
    mux.add_chapter(0, Some(1_000_000_000), "One").unwrap();
    mux.add_chapter(1_000_000_000, None, "Two").unwrap();
    mux.add_attachment(
        MkvAttachment::default()
            .with_filename("notes.txt".into())
            .with_mime_type("text/plain".into())
            .with_data(b"attached".to_vec()),
    )
    .unwrap();
    mux.write_header().unwrap();
    for i in 0..4i64 {
        mux.write_packet_with_block_group(
            &packet(0, i * 40, Some(40), i == 0, vec![i as u8; 20]),
            &BlockGroupOptions::default()
                .with_additions(vec![MkvBlockAddition::new(1, vec![0xA0 + i as u8; 5])]),
        )
        .unwrap();
        let opts =
            BlockGroupOptions::default().with_discard_padding((i == 3).then_some(13_500_000));
        mux.write_packet_with_block_group(
            &packet(
                1,
                i * 20 - 7,
                Some(if i == 3 { 7 } else { 20 }),
                true,
                vec![i as u8; 9],
            ),
            &opts,
        )
        .unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    let first = sink.bytes();

    // The copy: streams, flat metadata, typed track headers, chapters,
    // attachments, and each packet with what its Block carried.
    let mut source = open(first.clone());
    let streams: Vec<StreamInfo> = source.streams().to_vec();
    let sink = Sink::default();
    let mut mux = MkvMuxer::new_matroska(Box::new(sink.clone()), &streams).unwrap();
    mux.set_metadata(source.metadata()).unwrap();
    for i in 0..streams.len() {
        mux.copy_track_header(i, &source, i as u32).unwrap();
    }
    assert_eq!(
        mux.copy_chapters_and_attachments(&mut source).unwrap(),
        (2, 1)
    );
    mux.write_header().unwrap();
    while let Ok(p) = source.next_packet() {
        mux.write_packet_from(&p, &source).unwrap();
    }
    mux.write_trailer().unwrap();
    drop(mux);
    let second = sink.bytes();

    type Seen = (
        u32,
        Option<i64>,
        Option<i64>,
        bool,
        Vec<u8>,
        Vec<(u64, Vec<u8>)>,
        Option<i64>,
    );
    let describe = |bytes: Vec<u8>| {
        let mut dmx = open(bytes);
        let mut packets: Vec<Seen> = Vec::new();
        while let Ok(p) = dmx.next_packet() {
            packets.push((
                p.stream_index,
                p.pts,
                p.duration,
                p.flags.keyframe,
                p.data.clone(),
                dmx.block_additions()
                    .iter()
                    .map(|a| (a.block_add_id(), a.data().to_vec()))
                    .collect(),
                dmx.block_group_meta().and_then(|m| m.discard_padding()),
            ));
        }
        let chapters: Vec<(u64, Option<u64>, String)> = dmx.chapters()[0]
            .chapters
            .iter()
            .map(|c| (c.time_start_ns, c.time_end_ns, c.displays[0].string.clone()))
            .collect();
        let attachment = dmx.attachments()[0].clone();
        let data = dmx.attachment_data(attachment.index).unwrap();
        (
            packets,
            chapters,
            (attachment.filename, attachment.mime_type, data),
            dmx.metadata()
                .iter()
                .find(|(k, _)| k == "title")
                .map(|(_, v)| v.clone()),
            dmx.video_colour(0).copied(),
            dmx.track_timing(0).and_then(|t| t.default_duration()),
            dmx.max_block_addition_id(0),
            dmx.track_codec_timing(1)
                .map(|t| (t.codec_delay(), t.seek_pre_roll())),
            dmx.streams()[1].params.codec_id.as_str().to_owned(),
        )
    };
    let (one, two) = (describe(first), describe(second));
    assert_eq!(one.0.len(), 8);
    assert_eq!(one.0[0].5, vec![(1, vec![0xA0; 5])]);
    assert_eq!(one.0[7].6, Some(13_500_000));
    assert_eq!(one.7, Some((6_500_000, 80_000_000)));
    assert_eq!(one.8, "mkv:A_EXAMPLE");
    assert_eq!(one, two);
}

#[test]
fn strict_webm_carries_alpha_additions_without_declaring_a_maximum() {
    let streams = [stream(0, video("vp9"))];
    let sink = Sink::default();
    let mut mux = MkvMuxer::new_webm(Box::new(sink.clone()), &streams).unwrap();
    // The WebM guidelines list MaxBlockAdditionID as unsupported ...
    assert!(mux.set_max_block_addition_id(0, 1).is_err());
    mux.write_header().unwrap();
    // ... and BlockAdditions as supported.
    mux.write_packet_with_additions(
        &packet(0, 0, Some(40), true, vec![1; 10]),
        &[MkvBlockAddition::new(1, vec![2; 6])],
    )
    .unwrap();
    mux.write_trailer().unwrap();
    drop(mux);
    let mut dmx = open(sink.bytes());
    let p = dmx.next_packet().unwrap();
    assert_eq!(p.data, vec![1; 10]);
    assert_eq!(dmx.block_additions()[0].data(), &[2; 6]);

    // A Matroska track still has to declare the maximum.
    let sink = Sink::default();
    let mut mux = MkvMuxer::new_matroska(Box::new(sink), &streams).unwrap();
    mux.write_header().unwrap();
    assert!(
        mux.write_packet_with_additions(
            &packet(0, 0, Some(40), true, vec![1; 10]),
            &[MkvBlockAddition::new(1, vec![2; 6])],
        )
        .is_err()
    );
}

// ---------------------------------------------------------------------
// Seeking a track the index does not list
// ---------------------------------------------------------------------

fn elem(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = write_element_id(id);
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

fn uint(id: u32, value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let skip = bytes.iter().take_while(|b| **b == 0).count().min(7);
    elem(id, &bytes[skip..])
}

fn simple_block(track: u8, offset: i16, key: bool, payload: &[u8]) -> Vec<u8> {
    let mut body = vec![0x80 | track];
    body.extend_from_slice(&offset.to_be_bytes());
    body.push(if key { 0x80 } else { 0 });
    body.extend_from_slice(payload);
    elem(ids::SIMPLE_BLOCK, &body)
}

/// Video on track 1 (a key frame every second), audio on track 2 (a
/// frame every 250 ms), one Cluster per second, and Cues that list the
/// video track only — the index most writers produce.
fn video_indexed_file() -> Vec<u8> {
    let track = |number: u64, kind: u64, codec: &str| {
        let mut t = uint(ids::TRACK_NUMBER, number);
        t.extend(uint(ids::TRACK_UID, number));
        t.extend(uint(ids::TRACK_TYPE, kind));
        t.extend(elem(ids::CODEC_ID, codec.as_bytes()));
        elem(ids::TRACK_ENTRY, &t)
    };
    let mut tracks = track(1, 1, "V_FFV1");
    tracks.extend(track(2, 2, "A_AC3"));
    let info = uint(ids::TIMECODE_SCALE, 1_000_000);
    let clusters: Vec<Vec<u8>> = (0..4u64)
        .map(|c| {
            let mut body = uint(ids::TIMECODE, c * 1000);
            for i in 0..4i16 {
                // The audio frame comes first in each quarter second.
                body.extend(simple_block(2, i * 250, true, &[c as u8, i as u8]));
                body.extend(simple_block(1, i * 250 + 10, i == 0, &[0xF0, i as u8]));
            }
            elem(ids::CLUSTER, &body)
        })
        .collect();
    let head_len = elem(ids::INFO, &info).len() + elem(ids::TRACKS, &tracks).len();
    // Cues sit before the Clusters; their length does not depend on the
    // positions' values as long as those fit the width used here.
    let cues_for = |first_cluster: u64| {
        let mut cues = Vec::new();
        let mut pos = first_cluster;
        for (c, cluster) in clusters.iter().enumerate() {
            let mut positions = uint(ids::CUE_TRACK, 1);
            positions.extend(elem(ids::CUE_CLUSTER_POSITION, &(pos as u32).to_be_bytes()));
            let mut point = uint(ids::CUE_TIME, c as u64 * 1000 + 10);
            point.extend(elem(ids::CUE_TRACK_POSITIONS, &positions));
            cues.extend(elem(ids::CUE_POINT, &point));
            pos += cluster.len() as u64;
        }
        elem(ids::CUES, &cues)
    };
    let cues = cues_for((head_len + cues_for(0).len()) as u64);
    let mut segment = elem(ids::INFO, &info);
    segment.extend(elem(ids::TRACKS, &tracks));
    segment.extend(cues);
    for c in &clusters {
        segment.extend_from_slice(c);
    }
    let mut header = uint(ids::EBML_VERSION, 1);
    header.extend(elem(ids::EBML_DOC_TYPE, b"matroska"));
    let mut file = elem(ids::EBML_HEADER, &header);
    file.extend(elem(ids::SEGMENT, &segment));
    file
}

#[test]
fn a_track_the_index_does_not_list_can_be_sought() {
    let bytes = video_indexed_file();
    for resilient in [false, true] {
        for (target, want) in [
            (-5i64, 0i64),
            (0, 0),
            (249, 0),
            (250, 250),
            (1000, 1000),
            (1740, 1500),
            (2999, 2750),
            (9000, 3750),
        ] {
            let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes.clone()));
            let mut dmx = if resilient {
                open_resilient_typed(rs, &NullCodecResolver).unwrap()
            } else {
                open_typed(rs, &NullCodecResolver).unwrap()
            };
            assert!(
                dmx.cue_points()
                    .iter()
                    .all(|c| c.track_positions.iter().all(|t| t.track == 1))
            );
            let landed = dmx
                .seek_to(1, target)
                .unwrap_or_else(|e| panic!("audio seek to {target}: {e}"));
            assert_eq!(landed, want, "audio seek to {target}");
            let next = loop {
                let p = dmx.next_packet().unwrap();
                if p.stream_index == 1 {
                    break p;
                }
            };
            assert_eq!(next.pts, Some(want), "audio seek to {target}");
        }
    }
}
