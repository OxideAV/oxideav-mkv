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
use oxideav_core::{NullCodecResolver, ReadSeek};

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
});
