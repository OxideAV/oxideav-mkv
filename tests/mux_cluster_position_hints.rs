//! Muxer-side Cluster `Position` / `PrevSize` hints (RFC 9559 §5.1.3.2 +
//! §5.1.3.3) — `MkvMuxer::with_cluster_position_hints`.
//!
//! `Position` is the Cluster's own Segment Position ("It might help to
//! resynchronize the offset on damaged streams") and `PrevSize` the
//! previous Cluster's full element size in octets ("Can be useful for
//! backward playing"). Both are optional, so the muxer keeps them off by
//! default (byte-identical output with prior releases) and emits them
//! right after each Cluster's `Timestamp` when the caller opts in.
//!
//! The round-trip assertions verify the *semantics*, not just presence:
//! every written `Position`, offset by the Segment payload start plus the
//! Cluster header length, must land exactly on the Cluster's on-disk ID;
//! every written `PrevSize`, subtracted from a Cluster's start, must land
//! exactly on the previous Cluster's ID.

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, Error, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo,
    TimeBase, WriteSeek,
};

fn pcm_stream() -> StreamInfo {
    let mut ap = CodecParameters::audio(CodecId::new("pcm_s16le"));
    ap.sample_rate = Some(48_000);
    ap.channels = Some(2);
    ap.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: ap,
    }
}

/// Mux 13 one-second-spaced PCM packets (0..=12 s). With the muxer's ~5 s
/// cluster cadence this produces 3 Clusters (t = 0 / 6000 / 12000 ms).
fn mux_file(with_hints: bool) -> Vec<u8> {
    let stream = pcm_stream();
    let streams = vec![stream.clone()];
    let tmp = std::env::temp_dir().join(format!(
        "oxideav-mkv-cluster-position-hints-{with_hints}.mkv"
    ));
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mkv::mux::MkvMuxer::new_matroska(ws, &streams).unwrap();
        if with_hints {
            mux.with_cluster_position_hints().unwrap();
        }
        mux.write_header().unwrap();
        for i in 0..=12i64 {
            let mut p = Packet::new(0, stream.time_base, vec![i as u8; 64]);
            p.pts = Some(i * 1000);
            p.duration = Some(1000);
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
        }
        mux.write_trailer().unwrap();
    }
    let bytes = std::fs::read(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    bytes
}

/// Absolute offset of the Segment payload start: the Segment element ID
/// (0x18538067) plus its 4-byte ID plus the 1-byte unknown-size VINT the
/// muxer writes.
fn segment_payload_start(bytes: &[u8]) -> u64 {
    let idx = bytes
        .windows(4)
        .position(|w| w == [0x18, 0x53, 0x80, 0x67])
        .expect("Segment element present") as u64;
    idx + 5
}

fn drain_count(dmx: &mut oxideav_mkv::demux::MkvDemuxer) -> usize {
    let mut n = 0;
    loop {
        match dmx.next_packet() {
            Ok(_) => n += 1,
            Err(Error::Eof) => return n,
            Err(e) => panic!("demux error: {e}"),
        }
    }
}

#[test]
fn hints_round_trip_with_exact_offsets() {
    let bytes = mux_file(true);
    let sds = segment_payload_start(&bytes);

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let mut dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(drain_count(&mut dmx), 13);

    let records = dmx.cluster_records();
    assert_eq!(records.len(), 3, "13 s of 1 s packets → 3 clusters");

    // The muxer writes each Cluster with a 4-byte ID + 1-byte unknown-size
    // VINT, so a record's Cluster ID sits 5 bytes before its body.
    const CLUSTER_HEADER_LEN: u64 = 5;
    for (i, r) in records.iter().enumerate() {
        let cluster_id_offset = r.body_offset - CLUSTER_HEADER_LEN;
        // §5.1.3.2: Position is the Cluster's Segment Position — its ID
        // offset relative to the Segment payload start (Section 16).
        assert_eq!(
            r.position,
            Some(cluster_id_offset - sds),
            "cluster {i}: Position must be the Cluster's own Segment Position"
        );
        match i {
            0 => assert_eq!(
                r.prev_size, None,
                "first cluster has no previous cluster to size"
            ),
            _ => {
                // §5.1.3.3: PrevSize is the previous Cluster's size in
                // octets — subtracting it from this Cluster's start must
                // land exactly on the previous Cluster's ID.
                let prev_id_offset = records[i - 1].body_offset - CLUSTER_HEADER_LEN;
                assert_eq!(
                    r.prev_size,
                    Some(cluster_id_offset - prev_id_offset),
                    "cluster {i}: PrevSize must span back to the previous Cluster"
                );
            }
        }
    }
}

#[test]
fn hints_are_off_by_default() {
    let bytes = mux_file(false);
    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let mut dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    assert_eq!(drain_count(&mut dmx), 13);
    for r in dmx.cluster_records() {
        assert_eq!(r.position, None, "no Position without the opt-in");
        assert_eq!(r.prev_size, None, "no PrevSize without the opt-in");
    }
}

#[test]
fn with_cluster_position_hints_rejected_after_write_header() {
    let stream = pcm_stream();
    let ws: Box<dyn WriteSeek> = Box::new(std::io::Cursor::new(Vec::new()));
    let mut mux = oxideav_mkv::mux::MkvMuxer::new_matroska(ws, &[stream]).unwrap();
    mux.write_header().unwrap();
    assert!(mux.with_cluster_position_hints().is_err());
}

/// The §5.1.3.2 use case end-to-end: a damaged stream written with
/// Position hints resynchronises, and the recovered Cluster's `Position`
/// still verifies against its actual on-disk offset — the reader can
/// trust the hint survived the damage.
#[test]
fn position_hints_verify_after_damage_resync() {
    let mut bytes = mux_file(true);
    let sds = segment_payload_start(&bytes);

    // Corrupt the 2nd Cluster's ID (leading 0x00 bytes make the element
    // header unparseable at exactly that offset).
    let cluster_needle = [0x1Fu8, 0x43, 0xB6, 0x75];
    let offsets: Vec<usize> = bytes
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == cluster_needle)
        .map(|(i, _)| i)
        .collect();
    assert!(offsets.len() >= 3);
    let second = offsets[1];
    for b in &mut bytes[second..second + 4] {
        *b = 0x00;
    }

    let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_resilient_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
    let n = drain_count(&mut dmx);
    assert!(n > 0 && n < 13, "some packets lost to the damage: {n}");
    assert!(!dmx.damage_events().is_empty());
    // Every Cluster that survived still carries a verifiable Position.
    const CLUSTER_HEADER_LEN: u64 = 5;
    for r in dmx.cluster_records() {
        assert_eq!(r.position, Some(r.body_offset - CLUSTER_HEADER_LEN - sds));
    }
}
