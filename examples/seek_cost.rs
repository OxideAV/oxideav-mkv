//! Dev helper: count the I/O an `open` and a run of seeks cost on a
//! Matroska file, the way an HTTP byte-range backend would bill them
//! (`cargo run --example seek_cost <file.mkv> [--resilient] [target_ms ...]`).
//!
//! The reader wraps the file and counts three things: `seek` calls,
//! `read` calls, and *range requests* — a read whose start offset is not
//! where the previous read ended (what a byte-range transport has to
//! turn into a new HTTP request; sequential reads ride one response).
//! The open-time cost is printed first, then one line per seek target
//! with the landed timestamp and the delta the seek cost.
//!
//! With no targets, a sweep of 16 evenly spaced targets over the
//! Segment duration is run, so the per-seek cost of a cue-less file
//! (bisection, see `MkvDemuxer::seek_strategy`) is visible at a glance.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{Demuxer, NullCodecResolver, ReadSeek};

#[derive(Default)]
struct Counters {
    seeks: AtomicU64,
    reads: AtomicU64,
    range_requests: AtomicU64,
    bytes: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> [u64; 4] {
        [
            self.seeks.load(Ordering::Relaxed),
            self.reads.load(Ordering::Relaxed),
            self.range_requests.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        ]
    }
}

struct CountingReader {
    inner: std::fs::File,
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

fn delta(a: [u64; 4], b: [u64; 4]) -> String {
    format!(
        "range-reqs {:>5}  reads {:>6}  seeks {:>6}  bytes {:>9}",
        b[2] - a[2],
        b[1] - a[1],
        b[0] - a[0],
        b[3] - a[3]
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: seek_cost <file.mkv> [--resilient] [target_ms ...]");
        std::process::exit(2);
    };
    let mut resilient = false;
    let mut targets: Vec<i64> = Vec::new();
    for a in args {
        if a == "--resilient" {
            resilient = true;
        } else if let Ok(v) = a.parse::<i64>() {
            targets.push(v);
        } else {
            eprintln!("unrecognised argument {a:?}");
            std::process::exit(2);
        }
    }
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(2);
        }
    };
    let counters = Arc::new(Counters::default());
    let reader: Box<dyn ReadSeek> = Box::new(CountingReader {
        inner: file,
        pos: 0,
        next_sequential: None,
        counters: Arc::clone(&counters),
    });
    let before = counters.snapshot();
    let opened = if resilient {
        oxideav_mkv::demux::open_resilient_typed(reader, &NullCodecResolver)
    } else {
        oxideav_mkv::demux::open_typed(reader, &NullCodecResolver)
    };
    let mut dmx = match opened {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{path}: open failed: {e}");
            std::process::exit(1);
        }
    };
    let after_open = counters.snapshot();
    println!(
        "open   ({}): {}  cues={}",
        if resilient { "resilient" } else { "strict" },
        delta(before, after_open),
        dmx.cue_points().len()
    );
    if targets.is_empty() {
        let dur_ms = dmx.duration_micros().unwrap_or(0) / 1000;
        if dur_ms > 0 {
            targets = (0..16).map(|i| dur_ms * i / 16).collect();
        } else {
            targets = vec![0];
        }
    }
    let mut failures = 0;
    for t in targets {
        let b = counters.snapshot();
        match dmx.seek_to(0, t) {
            Ok(landed) => {
                let a = counters.snapshot();
                println!(
                    "seek {t:>8} -> {landed:>8}  {}  via {:?}",
                    delta(b, a),
                    dmx.seek_strategy()
                );
            }
            Err(e) => {
                failures += 1;
                let a = counters.snapshot();
                println!("seek {t:>8} -> ERROR {e}  {}", delta(b, a));
            }
        }
    }
    std::process::exit(if failures == 0 { 0 } else { 1 });
}
