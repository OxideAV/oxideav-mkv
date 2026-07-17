#![no_main]

//! Demux arbitrary fuzz-supplied bytes through the Matroska demuxer.
//!
//! The contract under test is purely that the calls *return*: a
//! malformed stream yields `Err(Error::…)`, a well-formed one yields
//! `Ok(_)` packets until `Error::Eof`, and neither path may panic,
//! abort, integer-overflow (in a debug build), index out of bounds,
//! or attempt an attacker-controlled `Vec::with_capacity` / `vec![0;
//! n]` allocation that exceeds what the input could possibly back.
//! Return values are intentionally discarded.
//!
//! The Matroska / EBML attack surface this exercises:
//!   * VINT length parses with crafted widths and "unknown size"
//!     sentinels (RFC 8794 §4).
//!   * Master-element nesting depth (`Segment > Cluster > BlockGroup
//!     > Block > lacing` plus the recursive `ChapterAtom` tree).
//!   * Allocation bounds on every string / bytes read whose size
//!     comes from the file (a u64 VINT can encode up to 2^56 - 2;
//!     a naive `vec![0u8; n]` would attempt gigabytes of allocation
//!     before the truncated read failed).
//!   * Lacing-mode parsers (Xiph 255-additive, fixed-size,
//!     EBML-signed-delta) — each does its own size arithmetic over
//!     attacker-controlled bytes.
//!   * `CRC-32` element validation skip path.
//!   * `seek_to(0)` against any Cues that happen to parse cleanly,
//!     re-exercising the cluster-walk machinery from a random
//!     offset.
//!
//! Open is the only entry point: a successful open hands back a
//! demuxer whose `next_packet` then walks every Cluster in the
//! Segment. We cap the per-input packet count so a pathological
//! valid stream can't dominate fuzz time.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_core::{Demuxer, NullCodecResolver, ReadSeek};

/// Bound on how many packets we drain per fuzz input. A pathological
/// but legitimate stream could otherwise spin the fuzzer on a single
/// many-packet Cluster instead of exploring the input space.
const MAX_PACKETS_PER_INPUT: usize = 256;

fuzz_target!(|data: &[u8]| {
    // Skip empty / trivially-short inputs — the EBML header alone is
    // ~12 bytes, so anything shorter can't even pass `read_element_header`.
    if data.len() < 4 {
        return;
    }
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
    let Ok(mut dmx) = oxideav_mkv::demux::open(rs, &NullCodecResolver) else {
        return;
    };

    // Touch the metadata + streams slices once. These are populated
    // entirely by the open() path but exercising the accessors
    // catches any post-open invariant the parser might have left in
    // an inconsistent state.
    let _ = dmx.streams().len();
    let _ = dmx.metadata().len();
    let _ = dmx.duration_micros();

    // Drain packets up to MAX_PACKETS_PER_INPUT. The loop terminates
    // on the first error (Eof, invalid, …) — fuzz inputs are
    // expected to crash the cluster walker more often than they
    // demux cleanly, so a bounded loop is plenty.
    for _ in 0..MAX_PACKETS_PER_INPUT {
        if dmx.next_packet().is_err() {
            break;
        }
    }

    // Re-exercise the seek path. seek_to(0) is the cheapest possible
    // call — it lands on the first Cues entry for stream 0 (if any)
    // — and runs the CueRelativePosition / cluster pre-open code.
    // If the file had no Cues this returns Err; that's fine.
    let _ = dmx.seek_to(0, 0);

    // Second pass through the *typed* demuxer (`open_typed`), exercising
    // the typed-accessor surface the trait `open` path doesn't reach:
    // the per-Block BlockGroup side channels (`block_additions`,
    // `block_group_meta`), the Cluster records (which now carry
    // `SilentTrackNumber` lists), and the Chapters / SeekHead trees. A
    // crafted BlockGroup / SilentTracks / Chapters subtree must populate
    // these without panicking or leaving an inconsistent invariant.
    let rs2: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
    if let Ok(mut tdmx) = oxideav_mkv::demux::open_typed(rs2, &NullCodecResolver) {
        // Pre-packet typed trees built entirely during open().
        let _ = tdmx.chapters().len();
        let _ = tdmx.seek_entries().len();
        let _ = tdmx.cluster_records().len();
        for _ in 0..MAX_PACKETS_PER_INPUT {
            match tdmx.next_packet() {
                Ok(_) => {
                    // Per-packet side channels: read them while they are
                    // valid (between this packet and the next).
                    let _ = tdmx.block_additions().len();
                    if let Some(meta) = tdmx.block_group_meta() {
                        let _ = meta.reference_blocks().len();
                        let _ = meta.reference_priority();
                        let _ = meta.codec_state().map(|s| s.len());
                        let _ = meta.discard_padding();
                    }
                }
                Err(_) => break,
            }
        }
        // Cluster records accumulate as the walk progresses; touch the
        // SilentTrackNumber lists post-walk.
        for rec in tdmx.cluster_records() {
            let _ = rec.silent_track_numbers.len();
        }
    }

    // Third pass through the *resilient* demuxer (`open_resilient_typed`),
    // exercising the damage-recovery machinery over arbitrary bytes: the
    // Top-Level resync scanner (chunked byte scan + candidate vetting),
    // the open-time master-skip path, the Segment-size clamp, and the
    // Cues-less cluster-scan seek fallback. Two additional contracts on
    // top of "no panic":
    //   * a resilient `next_packet` may only fail with the clean
    //     `Error::Eof` — every other error class must have been recovered
    //     (resynchronised or converted into a dropped tail);
    //   * the recovery loop must terminate (the resync floor guarantees
    //     forward progress, so the bounded drain below completes).
    let rs3: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
    if let Ok(mut rdmx) = oxideav_mkv::demux::open_resilient_typed(rs3, &NullCodecResolver) {
        for _ in 0..MAX_PACKETS_PER_INPUT {
            match rdmx.next_packet() {
                Ok(_) => {}
                Err(oxideav_core::Error::Eof) => break,
                Err(e) => panic!("resilient next_packet leaked a non-Eof error: {e}"),
            }
        }
        // Damage events must be well-formed: recovery never moves backwards.
        for ev in rdmx.damage_events() {
            if let Some(resumed) = ev.resumed_at() {
                assert!(resumed >= ev.offset(), "resync moved backwards");
            }
        }
        // Cues-less seek fallback (or Cues seek when the index parsed).
        let _ = rdmx.seek_to(0, 0);
        let _ = rdmx.seek_to(0, 12_345);
        for _ in 0..8 {
            if rdmx.next_packet().is_err() {
                break;
            }
        }
    }

    // Fourth pass: the WebM conformance scanner (`webm::scan`) — a pure
    // structural walk with its own depth cap, findings cap, and
    // unknown-size sibling-termination rules. Contract: it never panics,
    // never allocates beyond what the input backs (it skips leaf bodies),
    // always terminates (offsets grow strictly monotonically), and its
    // per-status counters always sum to `elements_scanned`.
    let mut cur4 = Cursor::new(data.to_vec());
    if let Ok(report) = oxideav_mkv::webm::scan(&mut cur4) {
        assert_eq!(
            report.supported + report.unsupported + report.deprecated + report.unlisted,
            report.elements_scanned,
            "webm::scan counters must sum to elements_scanned"
        );
        if report.findings_truncated {
            assert!(report.unsupported + report.deprecated > report.findings.len() as u64);
        }
    }
});
