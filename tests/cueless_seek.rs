//! Cues-less seeking over an expensive transport (issue #9).
//!
//! A Matroska file whose `Cues` are neither front-stored nor
//! SeekHead-reachable — `mkvmerge --no-cues`-style output, many
//! transcoder outputs — used to cost the demuxer one positioned read per
//! Cluster at `open` (the late-Cues hunt) and a linear Cluster walk per
//! resilient seek, while the strict seek refused outright. Over an HTTP
//! byte-range reader every positioned read is a request, so a 48 MB file
//! with ~540 Clusters paid ~540 requests to open.
//!
//! These tests bill I/O the way such a transport would — a *range
//! request* is a read whose start is not where the previous read ended —
//! and pin the new contract:
//!
//! * `open` no longer hunts for late Cues: its request count is flat in
//!   the Cluster count (identical for a 4.8 MB / 53-Cluster and a 48 MB /
//!   521-Cluster file, the two shapes the issue measured).
//! * A Cues-less `seek_to` bisects the Cluster run on byte offset
//!   (`SeekStrategy::Bisection`), strict and resilient alike, landing on
//!   the last Cluster whose Timestamp is at or before the target (the
//!   first Cluster for an earlier target), monotone in the target, at
//!   ~log2(clusters) requests — flat as the Cluster count grows 10×.
//! * The premise (ascending Cluster Timestamps) is verified per probe: an
//!   unsorted run degrades to the linear walk (`SeekStrategy::LinearScan`)
//!   and still lands correctly.
//! * The old late-Cues hunt survives as the opt-in
//!   `MkvDemuxer::scan_for_cues`.
//!
//! Fixtures are hand-built from RFC 9559 element definitions (known- and
//! unknown-size Clusters, decoy Cluster IDs inside payloads); a final,
//! environment-gated test runs the same harness over external fixtures
//! (`OXIDEAV_MKV_SEEK_FIXTURES=<dir>` holding e.g. ffmpeg / mkvmerge
//! output with the Cues stripped).

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{Demuxer, Error, ReadSeek};
use oxideav_mkv::demux::{DamageKind, MkvDemuxer, SeekStrategy};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

// ---------------------------------------------------------------------
// EBML builders
// ---------------------------------------------------------------------

fn elem_uint(id: u32, value: u64) -> Vec<u8> {
    let n = if value == 0 {
        1
    } else {
        (64 - value.leading_zeros()).div_ceil(8) as usize
    };
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(n as u64, 0));
    for i in (0..n).rev() {
        out.push(((value >> (i * 8)) & 0xFF) as u8);
    }
    out
}

/// Fixed 8-octet unsigned integer — keeps an element's size independent
/// of its value (front-stored Cues whose positions depend on their own
/// size).
fn elem_uint8(id: u32, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(8, 0));
    out.extend_from_slice(&value.to_be_bytes());
    out
}

fn elem_str(id: u32, s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(s.len() as u64, 0));
    out.extend_from_slice(s.as_bytes());
    out
}

fn elem_float_be_f64(id: u32, value: f64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(8, 0));
    out.extend_from_slice(&value.to_be_bytes());
    out
}

fn elem_master(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

fn elem_unknown_size(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.push(0xFF); // 1-byte unknown-size VINT
    out.extend_from_slice(body);
    out
}

fn simple_block(track: u8, tc_offset: i16, keyframe: bool, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(if keyframe { 0x80 } else { 0x00 });
    body.extend_from_slice(payload);
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(ids::SIMPLE_BLOCK));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(&body);
    out
}

fn ebml_header() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    b.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    elem_master(ids::EBML_HEADER, &b)
}

fn info_master(duration_ms: f64) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    body.extend_from_slice(&elem_float_be_f64(ids::DURATION, duration_ms));
    elem_master(ids::INFO, &body)
}

fn tracks_master() -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, 0xA1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio = Vec::new();
    audio.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    tb.extend_from_slice(&elem_master(ids::AUDIO, &audio));
    elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &tb))
}

/// Pseudo-random payload bytes (LCG) — incompressible-looking data in
/// which the 4-byte Cluster ID occurs by chance about once per 4 GiB.
fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 56) as u8
        })
        .collect()
}

/// A Cluster body: `Timestamp` + one keyframe `SimpleBlock` carrying
/// `payload`.
fn cluster_body(tc: u64, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_uint(ids::TIMECODE, tc));
    body.extend_from_slice(&simple_block(1, 0, true, payload));
    body
}

/// Fixture description: Cluster Timestamps (ms) in storage order and the
/// bytes of each Cluster element.
struct Run {
    timestamps: Vec<u64>,
    clusters: Vec<Vec<u8>>,
}

/// `n` known-size Clusters at `i * 1000` ms with payloads jittering
/// between ~60 KB and ~120 KB (so byte offset is not linear in time).
fn ascending_run(n: usize) -> Run {
    let mut timestamps = Vec::with_capacity(n);
    let mut clusters = Vec::with_capacity(n);
    for i in 0..n {
        let tc = i as u64 * 1000;
        let len = 60_000 + ((i as u64 * 7919) % 60_000) as usize;
        timestamps.push(tc);
        clusters.push(elem_master(
            ids::CLUSTER,
            &cluster_body(tc, &payload(len, i as u64)),
        ));
    }
    Run {
        timestamps,
        clusters,
    }
}

/// Segment = Info, Tracks, the Clusters, then `tail` (a trailing
/// Top-Level element, e.g. Cues or Void) — known-size Segment.
fn build(run: &Run, front: &[u8], tail: &[u8]) -> Vec<u8> {
    let last = run.timestamps.iter().copied().max().unwrap_or(0) as f64 + 1000.0;
    let mut seg = Vec::new();
    seg.extend_from_slice(&info_master(last));
    seg.extend_from_slice(&tracks_master());
    seg.extend_from_slice(front);
    for c in &run.clusters {
        seg.extend_from_slice(c);
    }
    seg.extend_from_slice(tail);
    let mut out = ebml_header();
    out.extend_from_slice(&elem_master(ids::SEGMENT, &seg));
    out
}

/// The landing the contract promises for `target`: the last Cluster
/// (storage order) whose Timestamp is at or before it, else the first.
fn expected_landing(timestamps: &[u64], target: i64) -> i64 {
    let mut best = timestamps[0];
    for &t in timestamps {
        if (t as i64) <= target {
            best = t;
        } else {
            break;
        }
    }
    best as i64
}

// ---------------------------------------------------------------------
// Byte-range billing reader
// ---------------------------------------------------------------------

#[derive(Default)]
struct Counters {
    seeks: AtomicU64,
    reads: AtomicU64,
    range_requests: AtomicU64,
    bytes: AtomicU64,
}

impl Counters {
    fn requests(&self) -> u64 {
        self.range_requests.load(Ordering::Relaxed)
    }
    fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
}

struct CountingReader {
    inner: Cursor<Arc<[u8]>>,
    pos: u64,
    next_sequential: Option<u64>,
    counters: Arc<Counters>,
}

impl Read for CountingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.next_sequential != Some(self.pos) {
            self.counters.range_requests.fetch_add(1, Ordering::Relaxed);
        }
        let n = self.inner.read(buf)?;
        self.counters.reads.fetch_add(1, Ordering::Relaxed);
        self.counters.bytes.fetch_add(n as u64, Ordering::Relaxed);
        self.pos += n as u64;
        self.next_sequential = Some(self.pos);
        Ok(n)
    }
}

impl Seek for CountingReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.counters.seeks.fetch_add(1, Ordering::Relaxed);
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }
}

fn open_counting(bytes: &Arc<[u8]>, resilient: bool) -> (MkvDemuxer, Arc<Counters>) {
    let counters = Arc::new(Counters::default());
    let rs: Box<dyn ReadSeek> = Box::new(CountingReader {
        inner: Cursor::new(Arc::clone(bytes)),
        pos: 0,
        next_sequential: None,
        counters: Arc::clone(&counters),
    });
    let dmx = if resilient {
        oxideav_mkv::demux::open_resilient_typed(rs, &oxideav_core::NullCodecResolver)
    } else {
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver)
    }
    .expect("open");
    (dmx, counters)
}

/// Seek sweep: every target in `targets` (in order) must land on the
/// contract's Cluster with the given strategy, the next packet must be
/// that Cluster's first block, and landings must be monotone in the
/// target. Returns the maximum requests one seek cost.
fn sweep(
    dmx: &mut MkvDemuxer,
    counters: &Counters,
    timestamps: &[u64],
    targets: &[i64],
    strategy: SeekStrategy,
) -> u64 {
    let mut max_requests = 0u64;
    let mut last_landed: Option<i64> = None;
    let mut last_target: Option<i64> = None;
    for &t in targets {
        let before = counters.requests();
        let landed = dmx
            .seek_to(0, t)
            .unwrap_or_else(|e| panic!("seek to {t}: {e}"));
        let cost = counters.requests() - before;
        max_requests = max_requests.max(cost);
        assert_eq!(
            landed,
            expected_landing(timestamps, t),
            "landing for target {t} (cost {cost} requests)"
        );
        assert_eq!(
            dmx.seek_strategy(),
            Some(strategy),
            "strategy for target {t}"
        );
        if let (Some(pl), Some(pt)) = (last_landed, last_target)
            && pt <= t
        {
            assert!(
                pl <= landed,
                "monotone: target {pt}->{t} landed {pl}->{landed}"
            );
        }
        last_landed = Some(landed);
        last_target = Some(t);
        let pkt = dmx.next_packet().expect("packet after seek");
        assert_eq!(
            pkt.pts,
            Some(landed),
            "first packet after landing on {landed}"
        );
    }
    max_requests
}

fn sweep_targets(timestamps: &[u64]) -> Vec<i64> {
    let last = *timestamps.last().unwrap() as i64;
    let mut v: Vec<i64> = vec![-5, 0, 1, 999, 1000, 1001];
    let mut t = 0i64;
    while t <= last {
        v.push(t);
        t += 777;
    }
    v.push(last);
    v.push(last + 1);
    v.push(last + 1_000_000);
    v.sort_unstable();
    v
}

/// Request budget for one bisection seek over `n` Clusters: one window
/// per halving, one boundary read per mid-run probe, a few forward steps.
fn seek_budget(n: usize) -> u64 {
    2 * (usize::BITS - (n.max(2) - 1).leading_zeros()) as u64 + 6
}

// ---------------------------------------------------------------------
// 1. open cost is flat in the Cluster count
// ---------------------------------------------------------------------

#[test]
fn open_without_cues_costs_the_same_on_53_and_521_clusters() {
    let small: Arc<[u8]> = build(&ascending_run(53), &[], &[]).into();
    let large: Arc<[u8]> = build(&ascending_run(521), &[], &[]).into();
    assert!(
        small.len() > 4_500_000 && small.len() < 5_500_000,
        "{}",
        small.len()
    );
    assert!(
        large.len() > 45_000_000 && large.len() < 52_000_000,
        "{}",
        large.len()
    );
    for resilient in [false, true] {
        let (dmx_s, c_s) = open_counting(&small, resilient);
        let (dmx_l, c_l) = open_counting(&large, resilient);
        assert!(dmx_s.cue_points().is_empty() && dmx_l.cue_points().is_empty());
        assert_eq!(
            c_s.requests(),
            c_l.requests(),
            "open requests must not grow with the Cluster count (resilient={resilient})"
        );
        assert!(
            c_s.requests() <= 8,
            "open costs {} requests (resilient={resilient})",
            c_s.requests()
        );
        // Not a single Cluster header past the first is touched.
        assert!(c_s.reads() < 200, "{} reads", c_s.reads());
    }
}

// ---------------------------------------------------------------------
// 2. bisection: exact landing, monotone, ~log2 requests, flat at 10x
// ---------------------------------------------------------------------

#[test]
fn cueless_seek_bisects_53_and_521_clusters_strict_and_resilient() {
    let runs = [ascending_run(53), ascending_run(521)];
    let files: Vec<Arc<[u8]>> = runs.iter().map(|r| build(r, &[], &[]).into()).collect();
    let mut max_by_size = [[0u64; 2]; 2];
    for (ri, run) in runs.iter().enumerate() {
        for (mi, resilient) in [false, true].into_iter().enumerate() {
            let (mut dmx, counters) = open_counting(&files[ri], resilient);
            let targets = sweep_targets(&run.timestamps);
            let max = sweep(
                &mut dmx,
                &counters,
                &run.timestamps,
                &targets,
                SeekStrategy::Bisection,
            );
            let budget = seek_budget(run.clusters.len());
            assert!(
                max <= budget,
                "{} clusters resilient={resilient}: worst seek cost {max} requests > budget {budget}",
                run.clusters.len()
            );
            assert!(
                dmx.damage_events().is_empty(),
                "navigation on a clean file records no damage"
            );
            max_by_size[ri][mi] = max;
        }
    }
    // 10x the Clusters costs a handful more probes, not 10x.
    for (small, large) in max_by_size[0].iter().zip(&max_by_size[1]) {
        assert!(
            *large <= *small + 8,
            "53 clusters: {small} requests, 521 clusters: {large} requests"
        );
    }
}

#[test]
fn cueless_seek_backwards_and_repeated_targets() {
    let run = ascending_run(53);
    let bytes: Arc<[u8]> = build(&run, &[], &[]).into();
    let (mut dmx, counters) = open_counting(&bytes, false);
    // Backwards sweep, then the same target thrice: each seek is
    // self-contained (no reliance on the previous landing) and exact.
    let mut targets: Vec<i64> = (0..53).rev().map(|i| i * 1000 + 500).collect();
    targets.extend([30_500, 30_500, 30_500]);
    let max = sweep(
        &mut dmx,
        &counters,
        &run.timestamps,
        &targets,
        SeekStrategy::Bisection,
    );
    assert!(max <= seek_budget(53), "{max}");
}

// ---------------------------------------------------------------------
// 3. unknown-size Clusters (live-style run) bisect too
// ---------------------------------------------------------------------

#[test]
fn cueless_seek_over_unknown_size_clusters() {
    let n = 40usize;
    let mut run = Run {
        timestamps: Vec::new(),
        clusters: Vec::new(),
    };
    for i in 0..n {
        let tc = i as u64 * 1000;
        let len = 50_000 + ((i as u64 * 5557) % 50_000) as usize;
        run.timestamps.push(tc);
        run.clusters.push(elem_unknown_size(
            ids::CLUSTER,
            &cluster_body(tc, &payload(len, 900 + i as u64)),
        ));
    }
    // Known-size Segment: both paths bisect.
    let bytes: Arc<[u8]> = build(&run, &[], &[]).into();
    for resilient in [false, true] {
        let (mut dmx, counters) = open_counting(&bytes, resilient);
        let targets = sweep_targets(&run.timestamps);
        let max = sweep(
            &mut dmx,
            &counters,
            &run.timestamps,
            &targets,
            SeekStrategy::Bisection,
        );
        // Closing the last gap walks through the landing Cluster's body
        // (no declared end), one window per 64 KiB — still far from
        // one request per Cluster.
        assert!(max <= seek_budget(n) + 4, "resilient={resilient}: {max}");
    }

    // Unknown-size Segment with neither SeekHead nor Cues — the RFC 9559
    // §23.2 live-stream shape the spec says SHOULD be considered
    // non-seekable: the strict path honours the signal, the resilient
    // path bisects (the capture has an end: the input's).
    let mut seg = Vec::new();
    seg.extend_from_slice(&info_master(n as f64 * 1000.0));
    seg.extend_from_slice(&tracks_master());
    for c in &run.clusters {
        seg.extend_from_slice(c);
    }
    let mut file = ebml_header();
    file.extend_from_slice(&elem_unknown_size(ids::SEGMENT, &seg));
    let bytes: Arc<[u8]> = file.into();
    let (mut strict, _c) = open_counting(&bytes, false);
    assert!(matches!(
        strict.seek_to(0, 5_000),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(strict.seek_strategy(), None);
    let (mut dmx, counters) = open_counting(&bytes, true);
    let targets = sweep_targets(&run.timestamps);
    let max = sweep(
        &mut dmx,
        &counters,
        &run.timestamps,
        &targets,
        SeekStrategy::Bisection,
    );
    assert!(max <= seek_budget(n) + 4, "{max}");
}

// ---------------------------------------------------------------------
// 4. decoy Cluster headers inside payloads are vetted away
// ---------------------------------------------------------------------

#[test]
fn decoy_cluster_headers_in_payloads_do_not_mislead_the_bisection() {
    let n = 48usize;
    let mut run = Run {
        timestamps: Vec::new(),
        clusters: Vec::new(),
    };
    for i in 0..n {
        let tc = i as u64 * 1000;
        let mut data = payload(80_000, 400 + i as u64);
        // Three decoys per payload: a full fake Cluster header with a
        // plausible small size and an in-bracket-looking Timestamp
        // (the shape that survives the in-window vet and must be
        // rejected by the boundary check), one with the unknown-size
        // marker but a block before its Timestamp, and a bare ID.
        let mut decoy = write_element_id(ids::CLUSTER);
        decoy.push(0x85);
        decoy.extend_from_slice(&elem_uint(ids::TIMECODE, tc + 500));
        data[10_000..10_000 + decoy.len()].copy_from_slice(&decoy);
        let mut decoy2 = write_element_id(ids::CLUSTER);
        decoy2.push(0xFF);
        decoy2.extend_from_slice(&simple_block(1, 0, true, &[1, 2, 3]));
        data[40_000..40_000 + decoy2.len()].copy_from_slice(&decoy2);
        data[70_000..70_004].copy_from_slice(&write_element_id(ids::CLUSTER));
        run.timestamps.push(tc);
        run.clusters
            .push(elem_master(ids::CLUSTER, &cluster_body(tc, &data)));
    }
    let bytes: Arc<[u8]> = build(&run, &[], &[]).into();
    for resilient in [false, true] {
        let (mut dmx, counters) = open_counting(&bytes, resilient);
        let targets = sweep_targets(&run.timestamps);
        let max = sweep(
            &mut dmx,
            &counters,
            &run.timestamps,
            &targets,
            SeekStrategy::Bisection,
        );
        assert!(max <= seek_budget(n) + 4, "resilient={resilient}: {max}");
    }
}

// ---------------------------------------------------------------------
// 5. an unsorted Cluster run is detected and walked linearly
// ---------------------------------------------------------------------

#[test]
fn unsorted_cluster_run_degrades_to_the_linear_walk() {
    let n = 32usize;
    let mut run = Run {
        timestamps: Vec::new(),
        clusters: Vec::new(),
    };
    for i in 0..n {
        // Ascending, except a block of nine Clusters in the second half
        // that jump back in time — a mid-run probe lands on one of them.
        let tc = if (20..29).contains(&i) {
            (i as u64 - 19) * 100
        } else {
            i as u64 * 1000
        };
        run.timestamps.push(tc);
        run.clusters.push(elem_master(
            ids::CLUSTER,
            &cluster_body(tc, &payload(90_000, 700 + i as u64)),
        ));
    }
    let bytes: Arc<[u8]> = build(&run, &[], &[]).into();
    for resilient in [false, true] {
        let (mut dmx, _counters) = open_counting(&bytes, resilient);
        // The linear walk's contract on an unsorted run: the last Cluster
        // at or before the target, stopping at the first Cluster at or
        // past it in storage order.
        let landed = dmx.seek_to(0, 31_000).expect("seek");
        assert_eq!(landed, 31_000);
        assert_eq!(dmx.seek_strategy(), Some(SeekStrategy::LinearScan));
        assert_eq!(dmx.next_packet().unwrap().pts, Some(31_000));
        // A target the bisection resolves without touching the unsorted
        // block still bisects.
        let landed = dmx.seek_to(0, 2_500).expect("seek");
        assert_eq!(landed, 2_000);
        assert_eq!(dmx.seek_strategy(), Some(SeekStrategy::Bisection));
    }
}

// ---------------------------------------------------------------------
// 6. late Cues: not hunted at open, opt-in via scan_for_cues
// ---------------------------------------------------------------------

fn cues_for(run: &Run, cluster_offsets_rel: &[u64]) -> Vec<u8> {
    let mut body = Vec::new();
    for (tc, off) in run.timestamps.iter().zip(cluster_offsets_rel) {
        let mut tp = Vec::new();
        tp.extend_from_slice(&elem_uint(ids::CUE_TRACK, 1));
        tp.extend_from_slice(&elem_uint8(ids::CUE_CLUSTER_POSITION, *off));
        let mut cp = Vec::new();
        cp.extend_from_slice(&elem_uint(ids::CUE_TIME, *tc));
        cp.extend_from_slice(&elem_master(ids::CUE_TRACK_POSITIONS, &tp));
        body.extend_from_slice(&elem_master(ids::CUE_POINT, &cp));
    }
    elem_master(ids::CUES, &body)
}

/// Segment-relative offsets of the Clusters in `build(run, front, _)`.
fn cluster_offsets_rel(run: &Run, front: &[u8]) -> Vec<u64> {
    let last = run.timestamps.iter().copied().max().unwrap_or(0) as f64 + 1000.0;
    let mut pos = (info_master(last).len() + tracks_master().len() + front.len()) as u64;
    let mut out = Vec::new();
    for c in &run.clusters {
        out.push(pos);
        pos += c.len() as u64;
    }
    out
}

#[test]
fn trailing_cues_without_seek_head_are_opt_in() {
    let run = ascending_run(53);
    let cues = cues_for(&run, &cluster_offsets_rel(&run, &[]));
    let bytes: Arc<[u8]> = build(&run, &[], &cues).into();
    for resilient in [false, true] {
        let (mut dmx, counters) = open_counting(&bytes, resilient);
        // Not hunted at open: no index, open as cheap as a Cues-less file.
        assert!(dmx.cue_points().is_empty());
        assert!(counters.requests() <= 8, "{}", counters.requests());
        let landed = dmx.seek_to(0, 20_500).unwrap();
        assert_eq!(landed, 20_000);
        assert_eq!(dmx.seek_strategy(), Some(SeekStrategy::Bisection));
        // Opt in: the hunt finds and loads the index, seeks use it.
        let before = counters.requests();
        assert!(dmx.scan_for_cues().unwrap());
        assert!(
            counters.requests() - before >= 53,
            "the hunt is the per-Cluster walk"
        );
        assert_eq!(dmx.cue_points().len(), 53);
        assert!(dmx.scan_for_cues().unwrap(), "idempotent once loaded");
        let before = counters.requests();
        let landed = dmx.seek_to(0, 20_500).unwrap();
        assert_eq!(landed, 20_000);
        assert_eq!(dmx.seek_strategy(), Some(SeekStrategy::Cues));
        assert!(
            counters.requests() - before <= 3,
            "{}",
            counters.requests() - before
        );
        assert_eq!(dmx.next_packet().unwrap().pts, Some(20_000));
    }
}

#[test]
fn scan_for_cues_on_a_cues_less_file_reports_none() {
    let run = ascending_run(12);
    let bytes: Arc<[u8]> = build(&run, &[], &[]).into();
    let (mut dmx, _c) = open_counting(&bytes, false);
    assert!(!dmx.scan_for_cues().unwrap());
    assert!(dmx.cue_points().is_empty());
    assert_eq!(dmx.seek_to(0, 5_500).unwrap(), 5_000);
    assert_eq!(dmx.seek_strategy(), Some(SeekStrategy::Bisection));
    // Reader position restored: packets still stream from the start
    // after a no-op hunt made before any seek.
    let (mut dmx, _c) = open_counting(&bytes, false);
    assert!(!dmx.scan_for_cues().unwrap());
    assert_eq!(dmx.next_packet().unwrap().pts, Some(0));
}

// ---------------------------------------------------------------------
// 7. the Cues fast path is untouched
// ---------------------------------------------------------------------

#[test]
fn front_cues_keep_the_one_jump_fast_path() {
    let run = ascending_run(53);
    // Front-stored Cues: compute offsets with the Cues' own size in front.
    let probe = cues_for(&run, &[0; 53]);
    let offsets = cluster_offsets_rel(&run, &probe);
    let cues = cues_for(&run, &offsets);
    assert_eq!(
        cues.len(),
        probe.len(),
        "fixed-width offsets keep the Cues size stable"
    );
    let bytes: Arc<[u8]> = build(&run, &cues, &[]).into();
    for resilient in [false, true] {
        let (mut dmx, counters) = open_counting(&bytes, resilient);
        assert_eq!(dmx.cue_points().len(), 53);
        let targets = sweep_targets(&run.timestamps);
        let max = sweep(
            &mut dmx,
            &counters,
            &run.timestamps,
            &targets,
            SeekStrategy::Cues,
        );
        // One jump (+ the resilient landing audit).
        assert!(max <= 3, "{max}");
    }
}

#[test]
fn resilient_cue_lie_falls_back_to_bisection() {
    let run = ascending_run(20);
    let probe = cues_for(&run, &[0; 20]);
    let mut offsets = cluster_offsets_rel(&run, &probe);
    // Cue 10 points into the middle of Cluster 10's payload.
    offsets[10] += 5_000;
    let cues = cues_for(&run, &offsets);
    let bytes: Arc<[u8]> = build(&run, &cues, &[]).into();
    let (mut dmx, _c) = open_counting(&bytes, true);
    let landed = dmx.seek_to(0, 10_500).unwrap();
    assert_eq!(landed, 10_000);
    assert_eq!(dmx.seek_strategy(), Some(SeekStrategy::Bisection));
    assert!(
        dmx.damage_events()
            .iter()
            .any(|e| e.kind() == DamageKind::CueLie)
    );
    assert_eq!(dmx.next_packet().unwrap().pts, Some(10_000));
}

// ---------------------------------------------------------------------
// 8. zero-Cluster Segment: still Unsupported in both modes
// ---------------------------------------------------------------------

#[test]
fn zero_cluster_segment_seek_stays_unsupported() {
    let run = Run {
        timestamps: vec![0],
        clusters: Vec::new(),
    };
    let bytes: Arc<[u8]> = build(&run, &[], &[]).into();
    for resilient in [false, true] {
        let (mut dmx, _c) = open_counting(&bytes, resilient);
        assert!(matches!(dmx.seek_to(0, 0), Err(Error::Unsupported(_))));
        assert_eq!(dmx.seek_strategy(), None);
    }
}

// ---------------------------------------------------------------------
// 9. external fixtures (ffmpeg / mkvmerge output), environment-gated
// ---------------------------------------------------------------------

/// `OXIDEAV_MKV_SEEK_FIXTURES=<dir>`: every `*.mkv` / `*.webm` in it is
/// opened strict with the billing reader; open must cost at most 24
/// requests, a 16-target sweep must land at or before each target
/// (monotone), at most 48 requests per seek, never by linear walk.
#[test]
fn external_fixtures_open_and_seek_within_budget() {
    let Ok(dir) = std::env::var("OXIDEAV_MKV_SEEK_FIXTURES") else {
        eprintln!("OXIDEAV_MKV_SEEK_FIXTURES unset, skipping external-fixture sweep");
        return;
    };
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("fixture dir") {
        let path = entry.expect("entry").path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "mkv" && ext != "webm" {
            continue;
        }
        seen += 1;
        let bytes: Arc<[u8]> = std::fs::read(&path).expect("read fixture").into();
        let (mut dmx, counters) = open_counting(&bytes, false);
        let open_cost = counters.requests();
        assert!(open_cost <= 24, "{}: open cost {open_cost}", path.display());
        // A live-layout file (ffmpeg `-live 1`) declares no Duration:
        // sweep a fixed span instead — targets past the end land on the
        // last Cluster, which the contract covers.
        let dur_ms = match dmx.duration_micros() {
            Some(d) if d > 0 => d / 1000,
            _ => 300_000,
        };
        let mut last_landed = i64::MIN;
        let mut worst = 0u64;
        for i in 0..16 {
            let t = dur_ms * i / 16;
            let before = counters.requests();
            let landed = dmx
                .seek_to(0, t)
                .unwrap_or_else(|e| panic!("{}: seek {t}: {e}", path.display()));
            let cost = counters.requests() - before;
            worst = worst.max(cost);
            assert!(
                landed <= t || i == 0,
                "{}: target {t} landed {landed}",
                path.display()
            );
            assert!(landed >= last_landed, "{}: monotone", path.display());
            assert!(cost <= 48, "{}: seek {t} cost {cost}", path.display());
            assert_ne!(
                dmx.seek_strategy(),
                Some(SeekStrategy::LinearScan),
                "{}",
                path.display()
            );
            let pkt = dmx
                .next_packet()
                .unwrap_or_else(|e| panic!("{}: packet: {e}", path.display()));
            assert!(
                pkt.pts.is_some_and(|p| p >= landed),
                "{}: packet pts after landing",
                path.display()
            );
            last_landed = landed;
        }
        eprintln!(
            "{}: open {open_cost} requests, worst seek {worst} requests, strategy {:?}",
            path.display(),
            dmx.seek_strategy()
        );
    }
    assert!(seen > 0, "no fixtures in {dir}");
}
