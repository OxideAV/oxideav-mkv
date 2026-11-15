//! The I/O the muxer and demuxer issue, and what must not depend on it.
//!
//! * The live layout writes forward only: against a sink whose `seek`
//!   always fails, every packet is on the sink when the call that wrote
//!   it returns, and the bytes equal those written to a seekable sink.
//! * The output buffer size never changes a byte, in any layout.
//! * `set_active_streams` steps over the payload of streams nobody
//!   reads instead of reading it.
//! * A Block whose declared size runs past its Cluster is refused before
//!   anything is allocated or skipped; the Clusters after it survive on
//!   the resilient path.

use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, NullCodecResolver, Packet, ReadSeek,
    SampleFormat, StreamInfo, TimeBase,
};
use oxideav_mkv::demux::DamageKind;
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;
use oxideav_mkv::mux::{LacingMode, MkvMuxer};

fn streams() -> Vec<StreamInfo> {
    let tb = TimeBase::new(1, 1000);
    let mut vp = CodecParameters::video(CodecId::new("ffv1"));
    vp.width = Some(320);
    vp.height = Some(240);
    let mut ap = CodecParameters::audio(CodecId::new("pcm_s16le"));
    ap.sample_rate = Some(8_000);
    ap.channels = Some(1);
    ap.sample_format = Some(SampleFormat::S16);
    [vp, ap]
        .into_iter()
        .enumerate()
        .map(|(i, params)| StreamInfo {
            index: i as u32,
            time_base: tb,
            duration: None,
            start_time: Some(0),
            params,
        })
        .collect()
}

/// 20 video frames of 20 kB (larger than the muxer's direct-write
/// threshold) and 200 audio frames of 100 bytes, interleaved.
fn packets() -> Vec<Packet> {
    let tb = TimeBase::new(1, 1000);
    let mut out = Vec::new();
    for i in 0..200i64 {
        if i % 10 == 0 {
            let v = i / 10;
            let mut p = Packet::new(0, tb, vec![(v * 3) as u8; 20_000 + v as usize]);
            p.pts = Some(v * 100);
            p.flags.keyframe = v % 5 == 0;
            out.push(p);
        }
        let mut p = Packet::new(1, tb, vec![i as u8; 100]);
        p.pts = Some(i * 10);
        p.flags.keyframe = true;
        out.push(p);
    }
    out
}

/// A sink that records what it receives. With `seekable == false` every
/// `seek` — position queries included — is an error, as on a pipe or a
/// socket behind a `Seek` shim.
#[derive(Clone)]
struct Sink {
    data: Arc<Mutex<Cursor<Vec<u8>>>>,
    seekable: bool,
    seek_calls: Arc<AtomicU64>,
}

impl Sink {
    fn new(seekable: bool) -> Self {
        Self {
            data: Arc::new(Mutex::new(Cursor::new(Vec::new()))),
            seekable,
            seek_calls: Arc::new(AtomicU64::new(0)),
        }
    }
    fn len(&self) -> usize {
        self.data.lock().unwrap().get_ref().len()
    }
    fn bytes(&self) -> Vec<u8> {
        self.data.lock().unwrap().get_ref().clone()
    }
}

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.data.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for Sink {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.seek_calls.fetch_add(1, Ordering::Relaxed);
        if !self.seekable {
            return Err(std::io::Error::other("not seekable"));
        }
        self.data.lock().unwrap().seek(p)
    }
}

#[derive(Clone, Copy, Debug)]
enum Layout {
    Default,
    Live,
    FrontCuesAndDuration,
    Laced,
}

fn mux(sink: Sink, layout: Layout, buffer: Option<usize>) -> Vec<usize> {
    let streams = streams();
    let mut m = MkvMuxer::new_matroska(Box::new(sink.clone()), &streams).unwrap();
    m.with_cluster_limits(250, 1 << 20).unwrap();
    match layout {
        Layout::Default => {}
        Layout::Live => {
            m.with_live_streaming().unwrap();
        }
        Layout::FrontCuesAndDuration => {
            m.with_front_cues(4096).unwrap();
            m.with_duration_finalization().unwrap();
            m.with_cluster_position_hints().unwrap();
        }
        Layout::Laced => {
            m.with_block_lacing(LacingMode::Xiph).unwrap();
        }
    }
    if let Some(b) = buffer {
        m.with_output_buffer(b).unwrap();
        assert_eq!(m.output_buffer(), b);
    }
    m.write_header().unwrap();
    let mut seen = vec![sink.len()];
    for p in &packets() {
        m.write_packet(p).unwrap();
        seen.push(sink.len());
    }
    m.write_trailer().unwrap();
    seen
}

#[test]
fn the_live_layout_writes_forward_only_and_delivers_every_packet() {
    let reference = Sink::new(true);
    mux(reference.clone(), Layout::Live, None);

    let pipe = Sink::new(false);
    let seen = mux(pipe.clone(), Layout::Live, None);
    assert!(
        pipe.bytes() == reference.bytes(),
        "same bytes as a seekable sink"
    );
    // One position query before the first byte, answered with an error;
    // nothing after it.
    assert_eq!(pipe.seek_calls.load(Ordering::Relaxed), 1);
    // The header is out before the first packet, and each packet's
    // bytes are on the sink when its `write_packet` returns.
    assert!(seen[0] > 0, "header delivered by write_header");
    let all = packets();
    for (i, p) in all.iter().enumerate() {
        let grew = seen[i + 1] - seen[i];
        assert!(
            grew >= p.data.len(),
            "packet {i}: {grew} bytes reached the sink for a {}-byte frame",
            p.data.len()
        );
    }
    assert_eq!(
        *seen.last().unwrap(),
        pipe.len(),
        "the trailer adds nothing"
    );

    // It reads back whole.
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(pipe.bytes()));
    let mut dmx = oxideav_mkv::demux::open(rs, &NullCodecResolver).unwrap();
    let mut n = 0;
    while let Ok(p) = dmx.next_packet() {
        assert!(p.data == all[n].data && p.pts == all[n].pts, "packet {n}");
        n += 1;
    }
    assert_eq!(n, all.len());
}

#[test]
fn a_finalising_layout_needs_a_seekable_sink() {
    // The default layout patches its SeekHead from `write_trailer`.
    let streams = streams();
    let sink = Sink::new(false);
    let mut m = MkvMuxer::new_matroska(Box::new(sink), &streams).unwrap();
    m.write_header().unwrap();
    for p in &packets() {
        m.write_packet(p).unwrap();
    }
    assert!(m.write_trailer().is_err());
}

#[test]
fn the_output_buffer_size_never_changes_a_byte() {
    for layout in [
        Layout::Default,
        Layout::Live,
        Layout::FrontCuesAndDuration,
        Layout::Laced,
    ] {
        let reference = Sink::new(true);
        mux(reference.clone(), layout, Some(0));
        let want = reference.bytes();
        assert!(want.len() > 400_000);
        for buffer in [None, Some(1), Some(100), Some(4096), Some(1 << 20)] {
            let sink = Sink::new(true);
            mux(sink.clone(), layout, buffer);
            assert!(
                sink.bytes() == want,
                "{layout:?}: buffer {buffer:?} changed the output"
            );
        }
    }
}

#[test]
fn a_muxer_dropped_without_a_trailer_keeps_what_it_accepted() {
    let streams = streams();
    let sink = Sink::new(true);
    let all = packets();
    {
        let mut m = MkvMuxer::new_matroska(Box::new(sink.clone()), &streams).unwrap();
        m.write_header().unwrap();
        for p in &all {
            m.write_packet(p).unwrap();
        }
        // No write_trailer: the capture was cut.
    }
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(sink.bytes()));
    let mut dmx = oxideav_mkv::demux::open(rs, &NullCodecResolver).unwrap();
    let mut n = 0;
    while let Ok(p) = dmx.next_packet() {
        assert!(p.data == all[n].data, "packet {n}");
        n += 1;
    }
    assert_eq!(n, all.len());
}

/// A reader that counts the bytes it hands out.
struct Counting {
    inner: Cursor<Vec<u8>>,
    bytes: Arc<AtomicU64>,
}

impl Read for Counting {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(b)?;
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl Seek for Counting {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(p)
    }
}

#[test]
fn inactive_streams_are_stepped_over_not_read() {
    let sink = Sink::new(true);
    mux(sink.clone(), Layout::Default, None);
    let file = sink.bytes();
    let all = packets();
    let audio: Vec<&Packet> = all.iter().filter(|p| p.stream_index == 1).collect();

    let read_all = Arc::new(AtomicU64::new(0));
    let mut dmx = oxideav_mkv::demux::open(
        Box::new(Counting {
            inner: Cursor::new(file.clone()),
            bytes: read_all.clone(),
        }),
        &NullCodecResolver,
    )
    .unwrap();
    let mut n = 0;
    while dmx.next_packet().is_ok() {
        n += 1;
    }
    assert_eq!(n, all.len());

    let read_audio = Arc::new(AtomicU64::new(0));
    let mut dmx = oxideav_mkv::demux::open(
        Box::new(Counting {
            inner: Cursor::new(file.clone()),
            bytes: read_audio.clone(),
        }),
        &NullCodecResolver,
    )
    .unwrap();
    dmx.set_active_streams(&[1]);
    let mut got = 0;
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                assert_eq!(p.stream_index, 1);
                assert!(p.data == audio[got].data && p.pts == audio[got].pts);
                got += 1;
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(got, audio.len());
    let (whole, audio_only) = (
        read_all.load(Ordering::Relaxed),
        read_audio.load(Ordering::Relaxed),
    );
    // 400 kB of video payload against 20 kB of audio: reading only the
    // audio touches a small fraction of the file.
    assert!(whole as usize >= file.len() - 4096);
    assert!(
        audio_only < whole / 4,
        "{audio_only} bytes read for the audio stream alone, {whole} for everything"
    );
    // The video stream can be switched back on.
    dmx.set_active_streams(&[0, 1]);
    dmx.seek_to(0, 0).unwrap();
    assert_eq!(dmx.next_packet().unwrap().stream_index, 0);
}

fn el(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = write_element_id(id);
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

fn uint(id: u32, v: u64) -> Vec<u8> {
    let n = ((64 - v.leading_zeros()).div_ceil(8) as usize).max(1);
    el(id, &v.to_be_bytes()[8 - n..])
}

#[test]
fn a_block_declared_past_its_cluster_is_refused_before_it_is_read() {
    let block = |track: u8, tag: u8| {
        let mut b = vec![0x80 | track, 0, 0, 0x80];
        b.extend_from_slice(&[tag; 8]);
        b
    };
    let cluster = |ts: u64, tag: u8| {
        el(
            ids::CLUSTER,
            &[
                uint(ids::TIMECODE, ts),
                el(ids::SIMPLE_BLOCK, &block(1, tag)),
            ]
            .concat(),
        )
    };
    // Cluster 2: a SimpleBlock header declaring 2^40 bytes, for the
    // known track and for an unknown one (which is skipped, not read).
    let forged = |track: u8| {
        let mut body = uint(ids::TIMECODE, 1000);
        body.extend(write_element_id(ids::SIMPLE_BLOCK));
        body.extend(write_vint(1 << 40, 0));
        body.extend(block(track, 0xEE));
        el(ids::CLUSTER, &body)
    };
    for track in [1u8, 9] {
        let tracks = el(
            ids::TRACKS,
            &el(
                ids::TRACK_ENTRY,
                &[
                    uint(ids::TRACK_NUMBER, 1),
                    uint(ids::TRACK_UID, 5),
                    uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO),
                    el(ids::CODEC_ID, b"A_PCM/INT/LIT"),
                ]
                .concat(),
            ),
        );
        let segment = el(
            ids::SEGMENT,
            &[
                el(ids::INFO, &uint(ids::TIMECODE_SCALE, 1_000_000)),
                tracks,
                cluster(0, 0x11),
                forged(track),
                cluster(2000, 0x33),
            ]
            .concat(),
        );
        let file = [
            el(ids::EBML_HEADER, &el(ids::EBML_DOC_TYPE, b"matroska")),
            segment,
        ]
        .concat();

        // Strict: the first Cluster reads, the forged Block is an error.
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file.clone()));
        let mut dmx = oxideav_mkv::demux::open_typed(rs, &NullCodecResolver).unwrap();
        assert_eq!(dmx.next_packet().unwrap().data, vec![0x11; 8]);
        assert!(matches!(dmx.next_packet(), Err(e) if !matches!(e, Error::Eof)));

        // Resilient: the Cluster after the forged one is recovered.
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
        let mut dmx = oxideav_mkv::demux::open_resilient_typed(rs, &NullCodecResolver).unwrap();
        let mut tags = Vec::new();
        loop {
            match dmx.next_packet() {
                Ok(p) => tags.push(p.data[0]),
                Err(Error::Eof) => break,
                Err(e) => panic!("track {track}: {e}"),
            }
        }
        assert_eq!(tags, vec![0x11, 0x33], "track {track}");
        assert_eq!(dmx.damage_events().len(), 1);
        assert_eq!(dmx.damage_events()[0].kind(), DamageKind::ClusterStream);
    }
}

fn one_track_file(clusters: Vec<u8>) -> Vec<u8> {
    let tracks = el(
        ids::TRACKS,
        &el(
            ids::TRACK_ENTRY,
            &[
                uint(ids::TRACK_NUMBER, 1),
                uint(ids::TRACK_UID, 5),
                uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO),
                el(ids::CODEC_ID, b"A_PCM/INT/LIT"),
            ]
            .concat(),
        ),
    );
    let mut segment_body = [el(ids::INFO, &uint(ids::TIMECODE_SCALE, 1_000_000)), tracks].concat();
    segment_body.extend(clusters);
    [
        el(ids::EBML_HEADER, &el(ids::EBML_DOC_TYPE, b"matroska")),
        el(ids::SEGMENT, &segment_body),
    ]
    .concat()
}

#[test]
fn per_cluster_bookkeeping_stops_growing_with_the_cluster_count() {
    // 70 000 Clusters of one 1-byte Block each: every packet comes out,
    // while the per-Cluster records stop at their cap.
    const CLUSTERS: usize = 70_000;
    let mut clusters = Vec::new();
    for i in 0..CLUSTERS {
        clusters.extend(el(
            ids::CLUSTER,
            &[
                uint(ids::TIMECODE, i as u64),
                el(ids::SIMPLE_BLOCK, &[0x81, 0, 0, 0x80, i as u8]),
            ]
            .concat(),
        ));
    }
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(one_track_file(clusters)));
    let mut dmx = oxideav_mkv::demux::open_typed(rs, &NullCodecResolver).unwrap();
    let mut n = 0usize;
    while let Ok(p) = dmx.next_packet() {
        assert_eq!(p.pts, Some(n as i64));
        n += 1;
    }
    assert_eq!(n, CLUSTERS);
    assert_eq!(dmx.cluster_records().len(), 65_536);
}

#[test]
fn damage_events_are_counted_past_the_number_kept() {
    // 5000 Clusters, each a good Block followed by one whose declared
    // size runs past the Cluster: 5000 recoveries, 4096 kept.
    const CLUSTERS: usize = 5000;
    let mut clusters = Vec::new();
    for i in 0..CLUSTERS {
        let mut body = [
            uint(ids::TIMECODE, i as u64),
            el(ids::SIMPLE_BLOCK, &[0x81, 0, 0, 0x80, i as u8]),
        ]
        .concat();
        body.extend(write_element_id(ids::SIMPLE_BLOCK));
        body.extend(write_vint(1 << 30, 0));
        body.extend([0x81, 0, 0, 0x80, 0xEE]);
        clusters.extend(el(ids::CLUSTER, &body));
    }
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(one_track_file(clusters)));
    let mut dmx = oxideav_mkv::demux::open_resilient_typed(rs, &NullCodecResolver).unwrap();
    let mut n = 0usize;
    loop {
        match dmx.next_packet() {
            Ok(_) => n += 1,
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(n, CLUSTERS, "every good Block is recovered");
    assert_eq!(dmx.damage_events_total(), CLUSTERS as u64);
    assert_eq!(dmx.damage_events().len(), 4096);
}

#[test]
fn live_muxes_appended_to_one_sink_read_as_one_stream() {
    // RFC 9559 §23.2: a change of resolution mid-stream is made by
    // ending the Segment and starting another. Two WebM live muxes are
    // written one after the other to the same forward-only sink — the
    // second with a different picture size — and read back as one
    // continuous packet sequence.
    let tb = TimeBase::new(1, 1000);
    let video = |w: u32, h: u32| {
        let mut vp = CodecParameters::video(CodecId::new("vp8"));
        vp.width = Some(w);
        vp.height = Some(h);
        vec![StreamInfo {
            index: 0,
            time_base: tb,
            duration: None,
            start_time: Some(0),
            params: vp,
        }]
    };
    let sink = Sink::new(false);
    let mut written = Vec::new();
    for (part, (w, h)) in [(640u32, 360u32), (1280, 720), (1280, 720)]
        .into_iter()
        .enumerate()
    {
        let mut m = MkvMuxer::new_webm(Box::new(sink.clone()), &video(w, h)).unwrap();
        m.with_live_streaming().unwrap();
        m.with_cluster_limits(100, 1 << 20).unwrap();
        m.write_header().unwrap();
        for i in 0..12i64 {
            let mut p = Packet::new(0, tb, vec![(part * 16) as u8 + i as u8; 500 + part]);
            // Each part restarts its own clock, as a fresh encoder does.
            p.pts = Some(i * 40);
            p.duration = Some(40);
            p.flags.keyframe = i % 4 == 0;
            m.write_packet(&p).unwrap();
            written.push((part, i, p.data));
        }
        m.write_trailer().unwrap();
    }
    assert_eq!(
        sink.seek_calls.load(Ordering::Relaxed),
        3,
        "one position query per mux"
    );

    for resilient in [false, true] {
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(sink.bytes()));
        let mut dmx = if resilient {
            oxideav_mkv::demux::open_resilient_typed(rs, &NullCodecResolver).unwrap()
        } else {
            oxideav_mkv::demux::open_typed(rs, &NullCodecResolver).unwrap()
        };
        assert_eq!(dmx.streams()[0].params.width, Some(640));
        let mut n = 0usize;
        let mut generations = Vec::new();
        loop {
            match dmx.next_packet() {
                Ok(p) => {
                    let (part, i, data) = &written[n];
                    assert!(p.data == *data, "packet {n} bytes");
                    // No part declares a Duration, and the last Block of
                    // each carries none on disk: a part starts at the
                    // last timestamp of the one before (11 * 40 ms).
                    assert_eq!(p.pts, Some(*part as i64 * 440 + i * 40), "packet {n}");
                    assert_eq!(p.flags.keyframe, i % 4 == 0);
                    assert_eq!(dmx.current_segment(), *part);
                    generations.push(dmx.stream_params_generation());
                    n += 1;
                }
                Err(Error::Eof) => break,
                Err(e) => panic!("resilient={resilient}: {e}"),
            }
        }
        assert_eq!(n, written.len());
        assert_eq!(dmx.segments().len(), 3);
        assert!(dmx.damage_events().is_empty());
        // The picture size changed once (part 2), not again (part 3).
        assert_eq!(dmx.streams()[0].params.width, Some(1280));
        assert_eq!(dmx.streams()[0].params.height, Some(720));
        assert_eq!(generations[11], 0);
        assert_eq!(generations[12], 1);
        assert_eq!(*generations.last().unwrap(), 1);
    }
}
