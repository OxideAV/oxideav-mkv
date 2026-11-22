//! Buffering between the EBML walkers and the caller's reader / writer.
//!
//! An EBML walk is thousands of tiny operations: one octet for the first
//! byte of an ID, a few more for the rest, the same again for the size,
//! "where am I?" before and after each element, a short hop over a
//! `Void`. Issued one by one against a file, each of those is a system
//! call, and the calls — not the parsing — are where the time goes.
//!
//! [`BufInput`] keeps a window of the input in memory and tracks the
//! logical position itself, so reads inside the window, position
//! queries and seeks cost nothing; only a read that leaves the window
//! touches the underlying reader. [`BufOutput`] is the mirror image for
//! the muxer: appended bytes collect in memory and the write position is
//! tracked, so the per-Block "where will this land?" query is free and
//! the sink sees large sequential writes.
//!
//! Neither changes a byte of what is read or written.

use std::io::{Read, Seek, SeekFrom, Write};

use oxideav_core::{ReadSeek, WriteSeek};

/// Largest window [`BufInput`] reads ahead, and the read size from which
/// it stops buffering and reads straight into the caller's buffer.
const INPUT_WINDOW: usize = 64 * 1024;

/// First read-ahead after a jump. A seek is often followed by one small
/// look (an element header, a Cluster Timestamp) and another seek, so
/// the window starts small and doubles for as long as the reads stay
/// sequential.
const INPUT_FIRST_FILL: usize = 4 * 1024;

/// Read-side buffer with a self-tracked position. See the module docs.
pub(crate) struct BufInput {
    inner: Box<dyn ReadSeek>,
    buf: Box<[u8]>,
    /// Absolute offset of `buf[0]`.
    start: u64,
    /// Valid bytes in `buf`.
    len: usize,
    /// Logical position — what `stream_position` answers.
    pos: u64,
    /// Where the underlying reader stands, when known.
    inner_pos: Option<u64>,
    /// Size of the next window fill.
    next_fill: usize,
    /// Input length as last measured (`0` before the first measure).
    known_len: u64,
}

impl BufInput {
    pub(crate) fn new(mut inner: Box<dyn ReadSeek>) -> Self {
        // The logical position starts where the reader was handed over
        // (not necessarily its start).
        let at = inner.stream_position().ok();
        Self {
            inner,
            buf: vec![0u8; INPUT_WINDOW].into_boxed_slice(),
            start: 0,
            len: 0,
            pos: at.unwrap_or(0),
            inner_pos: at,
            next_fill: INPUT_FIRST_FILL,
            known_len: 0,
        }
    }

    /// Whether the input holds bytes up to (not including) offset `end`.
    /// Asks the underlying reader for its length only when `end` is
    /// past what was last measured.
    pub(crate) fn has_bytes_to(&mut self, end: u64) -> std::io::Result<bool> {
        if end > self.known_len {
            let len = self.inner.seek(SeekFrom::End(0))?;
            self.inner_pos = Some(len);
            self.known_len = len;
        }
        Ok(end <= self.known_len)
    }

    /// Read exactly `n` bytes into a fresh buffer.
    ///
    /// The size comes from the input (an element size), so it is checked
    /// against the bytes the input actually has before anything is
    /// allocated: a declared size past the end fails with
    /// `UnexpectedEof` and costs no memory. The part of the range that
    /// is in the window is copied from it; the rest is read from the
    /// underlying reader straight into the buffer, so a large payload
    /// is copied once.
    pub(crate) fn read_vec(&mut self, n: u64) -> std::io::Result<Vec<u8>> {
        let short = || {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EBML: element data runs past the end of the input",
            )
        };
        let fits = |pos: u64, len: u64| pos.checked_add(n).is_some_and(|end| end <= len);
        if !fits(self.pos, self.known_len) {
            // Past what was measured: measure again (the input may have
            // grown) before refusing.
            let end = self.inner.seek(SeekFrom::End(0))?;
            self.inner_pos = Some(end);
            self.known_len = end;
            if !fits(self.pos, end) {
                return Err(short());
            }
        }
        let n = usize::try_from(n).map_err(|_| short())?;
        let mut out = Vec::with_capacity(n);
        let have = self.window();
        let take = have.len().min(n);
        out.extend_from_slice(&have[..take]);
        self.pos += take as u64;
        if take < n {
            self.sync_inner()?;
            let rest = (n - take) as u64;
            let got = (&mut self.inner).take(rest).read_to_end(&mut out)? as u64;
            self.pos += got;
            self.inner_pos = Some(self.pos);
            if got < rest {
                return Err(short());
            }
        }
        if n >= INPUT_FIRST_FILL {
            // Payload-sized data is passing through: keep the window
            // small so the next payload is mostly read directly rather
            // than through it.
            self.next_fill = INPUT_FIRST_FILL / 2;
        }
        Ok(out)
    }

    /// Bytes of the window at and after the logical position.
    fn window(&self) -> &[u8] {
        if self.pos >= self.start && self.pos - self.start < self.len as u64 {
            &self.buf[(self.pos - self.start) as usize..self.len]
        } else {
            &[]
        }
    }

    /// Put the underlying reader at the logical position. `true` when it
    /// already was there (the read that follows continues the previous
    /// one).
    fn sync_inner(&mut self) -> std::io::Result<bool> {
        if self.inner_pos == Some(self.pos) {
            return Ok(true);
        }
        self.inner.seek(SeekFrom::Start(self.pos))?;
        self.inner_pos = Some(self.pos);
        Ok(false)
    }
}

impl Read for BufInput {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let have = self.window();
        if !have.is_empty() {
            let n = have.len().min(out.len());
            out[..n].copy_from_slice(&have[..n]);
            self.pos += n as u64;
            return Ok(n);
        }
        let sequential = self.sync_inner()?;
        if out.len() >= INPUT_WINDOW {
            // A payload-sized read: no point passing it through the
            // window.
            let n = self.inner.read(out)?;
            self.pos += n as u64;
            self.inner_pos = Some(self.pos);
            return Ok(n);
        }
        self.next_fill = if sequential {
            (self.next_fill * 2).min(INPUT_WINDOW)
        } else {
            INPUT_FIRST_FILL
        };
        let fill = self.next_fill.max(out.len()).min(INPUT_WINDOW);
        self.start = self.pos;
        self.len = 0;
        // One read: a short answer is not an end (pipes, sockets and
        // files that are still growing all give short reads), and an
        // empty one is not remembered.
        let got = self.inner.read(&mut self.buf[..fill])?;
        self.len = got;
        self.inner_pos = Some(self.pos + got as u64);
        let n = got.min(out.len());
        out[..n].copy_from_slice(&self.buf[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for BufInput {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        match to {
            SeekFrom::Start(n) => self.pos = n,
            SeekFrom::Current(d) => {
                self.pos = self.pos.checked_add_signed(d).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "seek to a negative or overflowing position",
                    )
                })?;
            }
            SeekFrom::End(d) => {
                // The length is the underlying reader's to know (and may
                // have changed since it was last asked).
                let at = self.inner.seek(to)?;
                self.inner_pos = Some(at);
                self.pos = at;
                if d == 0 {
                    self.known_len = at;
                }
            }
        }
        Ok(self.pos)
    }
}

/// Bytes [`BufOutput`] collects before handing them to the sink.
pub(crate) const OUTPUT_BUFFER: usize = 64 * 1024;

/// A single write of at least this size bypasses the buffer.
const OUTPUT_DIRECT: usize = 8 * 1024;

/// Write-side buffer with a self-tracked position. See the module docs.
pub(crate) struct BufOutput {
    inner: Box<dyn WriteSeek>,
    buf: Vec<u8>,
    /// Buffer size; `0` writes through.
    capacity: usize,
    /// Position of the first buffered byte in the sink — the sink's own
    /// position.
    base: u64,
    /// The sink's position has been asked for (or set).
    base_known: bool,
}

impl BufOutput {
    pub(crate) fn new(inner: Box<dyn WriteSeek>) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            capacity: OUTPUT_BUFFER,
            base: 0,
            base_known: false,
        }
    }

    /// Change the buffer size (`0`: every write goes straight to the
    /// sink). Buffered bytes are written out first.
    pub(crate) fn set_capacity(&mut self, capacity: usize) -> std::io::Result<()> {
        self.write_out()?;
        self.capacity = capacity;
        Ok(())
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Hand the buffered bytes to the sink (without flushing the sink
    /// itself).
    pub(crate) fn write_out(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            self.inner.write_all(&self.buf)?;
            self.base += self.buf.len() as u64;
            self.buf.clear();
        }
        Ok(())
    }

    /// Overwrite `data.len()` bytes at absolute position `pos`, which
    /// must lie wholly before the write position. Bytes still in the
    /// buffer are patched there; bytes already handed to the sink are
    /// rewritten with one positioned write, after which the sink is
    /// back at the write position.
    pub(crate) fn patch_at(&mut self, pos: u64, data: &[u8]) -> std::io::Result<()> {
        self.learn_base();
        if pos >= self.base {
            let start = (pos - self.base) as usize;
            if let Some(slot) = self.buf.get_mut(start..start + data.len()) {
                slot.copy_from_slice(data);
                return Ok(());
            }
        }
        self.write_out()?;
        let end = self.base;
        self.inner.seek(SeekFrom::Start(pos))?;
        self.inner.write_all(data)?;
        self.inner.seek(SeekFrom::Start(end))?;
        Ok(())
    }

    /// Write the buffered bytes followed by `data`.
    fn write_pair(&mut self, data: &[u8]) -> std::io::Result<()> {
        let mut head: &[u8] = &self.buf;
        let mut tail = data;
        let total = (head.len() + tail.len()) as u64;
        while !head.is_empty() {
            let n = self
                .inner
                .write_vectored(&[std::io::IoSlice::new(head), std::io::IoSlice::new(tail)])?;
            if n == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            if n >= head.len() {
                tail = &tail[n - head.len()..];
                head = &[];
            } else {
                head = &head[n..];
            }
        }
        self.inner.write_all(tail)?;
        self.buf.clear();
        self.base += total;
        Ok(())
    }

    /// Learn where the sink stands, once. A sink that cannot say (a
    /// pipe behind a `Seek` shim) is taken to be at offset 0: every
    /// position the muxer derives is then relative to where it started
    /// writing, which is all a forward-only layout needs.
    fn learn_base(&mut self) {
        if !self.base_known {
            self.base = self.inner.stream_position().unwrap_or(0);
            self.base_known = true;
        }
    }
}

impl Write for BufOutput {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.learn_base();
        if self.capacity == 0 || data.len() >= OUTPUT_DIRECT.min(self.capacity) {
            // Payload-sized: not copied into the buffer. What is
            // buffered goes out in front of it, in the same call where
            // the sink can take both at once.
            self.write_pair(data)?;
            return Ok(data.len());
        }
        self.buf.extend_from_slice(data);
        if self.buf.len() >= self.capacity {
            self.write_out()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.write_out()?;
        self.inner.flush()
    }
}

impl Seek for BufOutput {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.learn_base();
        if let SeekFrom::Current(0) = to {
            return Ok(self.base + self.buf.len() as u64);
        }
        self.write_out()?;
        self.base = self.inner.seek(to)?;
        Ok(self.base)
    }
}

impl Drop for BufOutput {
    fn drop(&mut self) {
        // A muxer dropped without `write_trailer` (a capture that was
        // cut) still owes the sink the Blocks it accepted.
        let _ = self.write_out();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    /// A reader that counts the calls it receives.
    struct Counting {
        inner: Cursor<Vec<u8>>,
        calls: Arc<Mutex<(u64, u64)>>,
    }

    impl Read for Counting {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            self.calls.lock().unwrap().0 += 1;
            // Short reads on purpose.
            let n = b.len().min(1000);
            self.inner.read(&mut b[..n])
        }
    }

    impl Seek for Counting {
        fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
            self.calls.lock().unwrap().1 += 1;
            self.inner.seek(p)
        }
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn input_matches_the_plain_reader_under_random_access() {
        let data = pattern(300_000);
        let calls = Arc::new(Mutex::new((0, 0)));
        let mut buffered = BufInput::new(Box::new(Counting {
            inner: Cursor::new(data.clone()),
            calls: calls.clone(),
        }));
        let mut plain = Cursor::new(data.clone());
        let mut x = 0x1234_5678_9ABC_DEF1u64;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut a = vec![0u8; 200_000];
        let mut b = vec![0u8; 200_000];
        for _ in 0..20_000 {
            match rnd() % 10 {
                0 => {
                    let to = rnd() % 310_000;
                    assert_eq!(
                        buffered.seek(SeekFrom::Start(to)).unwrap(),
                        plain.seek(SeekFrom::Start(to)).unwrap()
                    );
                }
                1 => {
                    let d = (rnd() % 2000) as i64 - 300;
                    let (x, y) = (
                        buffered.seek(SeekFrom::Current(d)),
                        plain.seek(SeekFrom::Current(d)),
                    );
                    assert_eq!(x.is_ok(), y.is_ok());
                    if let (Ok(x), Ok(y)) = (x, y) {
                        assert_eq!(x, y);
                    }
                }
                2 => {
                    assert_eq!(
                        buffered.seek(SeekFrom::End(0)).unwrap(),
                        plain.seek(SeekFrom::End(0)).unwrap()
                    );
                }
                k => {
                    let n = match k {
                        3 => (rnd() % 150_000) as usize,
                        4 => (rnd() % 5000) as usize,
                        _ => (rnd() % 12) as usize,
                    };
                    // `read_exact` semantics on both sides.
                    let ra = buffered.read_exact(&mut a[..n]).is_ok();
                    let rb = plain.read_exact(&mut b[..n]).is_ok();
                    assert_eq!(ra, rb);
                    if ra {
                        assert_eq!(a[..n], b[..n]);
                        assert_eq!(
                            buffered.stream_position().unwrap(),
                            plain.stream_position().unwrap()
                        );
                    } else {
                        // A failed exact read leaves the position
                        // unspecified; realign.
                        let to = rnd() % 300_000;
                        buffered.seek(SeekFrom::Start(to)).unwrap();
                        plain.seek(SeekFrom::Start(to)).unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn input_serves_small_sequential_reads_from_few_calls() {
        let data = pattern(200_000);
        let calls = Arc::new(Mutex::new((0, 0)));
        // Unlimited read size for this one.
        struct Whole(Cursor<Vec<u8>>, Arc<Mutex<(u64, u64)>>);
        impl Read for Whole {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.1.lock().unwrap().0 += 1;
                self.0.read(b)
            }
        }
        impl Seek for Whole {
            fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
                self.1.lock().unwrap().1 += 1;
                self.0.seek(p)
            }
        }
        let mut r = BufInput::new(Box::new(Whole(Cursor::new(data.clone()), calls.clone())));
        let mut got = Vec::new();
        let mut one = [0u8; 3];
        while r.read_exact(&mut one).is_ok() {
            got.extend_from_slice(&one);
            // Position queries and short hops inside the window are free.
            let at = r.stream_position().unwrap();
            r.seek(SeekFrom::Start(at)).unwrap();
        }
        assert_eq!(got[..], data[..got.len()]);
        let (reads, seeks) = *calls.lock().unwrap();
        assert!(reads < 16, "{reads} reads for 200 kB of 3-byte reads");
        assert!(seeks <= 1, "{seeks} seeks");
    }

    #[test]
    fn input_sees_bytes_appended_after_an_empty_read() {
        #[derive(Clone)]
        struct Shared(Arc<Mutex<Vec<u8>>>, u64);
        impl Read for Shared {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                let d = self.0.lock().unwrap();
                let s = (self.1 as usize).min(d.len());
                let n = b.len().min(d.len() - s);
                b[..n].copy_from_slice(&d[s..s + n]);
                self.1 += n as u64;
                Ok(n)
            }
        }
        impl Seek for Shared {
            fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
                self.1 = match p {
                    SeekFrom::Start(n) => n,
                    SeekFrom::End(d) => (self.0.lock().unwrap().len() as i64 + d) as u64,
                    SeekFrom::Current(d) => (self.1 as i64 + d) as u64,
                };
                Ok(self.1)
            }
        }
        let shared = Arc::new(Mutex::new(vec![1u8, 2, 3]));
        let mut r = BufInput::new(Box::new(Shared(shared.clone(), 0)));
        let mut b = [0u8; 8];
        assert_eq!(r.read(&mut b).unwrap(), 3);
        assert_eq!(r.read(&mut b).unwrap(), 0);
        shared.lock().unwrap().extend_from_slice(&[4, 5]);
        assert_eq!(r.read(&mut b).unwrap(), 2);
        assert_eq!(b[..2], [4, 5]);
        assert_eq!(r.seek(SeekFrom::End(0)).unwrap(), 5);
    }

    #[derive(Default)]
    struct SinkState {
        data: Cursor<Vec<u8>>,
        writes: u64,
        seeks: u64,
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<SinkState>>);

    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let mut s = self.0.lock().unwrap();
            s.writes += 1;
            s.data.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for Sink {
        fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
            let mut s = self.0.lock().unwrap();
            s.seeks += 1;
            s.data.seek(p)
        }
    }

    #[test]
    fn output_matches_the_plain_writer_including_patches() {
        for capacity in [0usize, 7, 4096, OUTPUT_BUFFER] {
            let sink = Sink::default();
            let mut w = BufOutput::new(Box::new(sink.clone()));
            w.set_capacity(capacity).unwrap();
            assert_eq!(w.capacity(), capacity);
            let mut plain = Cursor::new(Vec::new());
            let data = pattern(50_000);
            let mut at = 0usize;
            let mut k = 1usize;
            while at < data.len() {
                let n = (k * 37 % 3000 + 1).min(data.len() - at);
                w.write_all(&data[at..at + n]).unwrap();
                plain.write_all(&data[at..at + n]).unwrap();
                assert_eq!(
                    w.stream_position().unwrap(),
                    plain.stream_position().unwrap()
                );
                if k.is_multiple_of(9) {
                    // Patch something behind the write position.
                    let resume = w.stream_position().unwrap();
                    let back = resume / 3;
                    w.seek(SeekFrom::Start(back)).unwrap();
                    w.write_all(b"PATCH").unwrap();
                    w.seek(SeekFrom::Start(resume)).unwrap();
                    plain.seek(SeekFrom::Start(back)).unwrap();
                    plain.write_all(b"PATCH").unwrap();
                    plain.seek(SeekFrom::Start(resume)).unwrap();
                }
                at += n;
                k += 1;
            }
            drop(w);
            assert!(sink.0.lock().unwrap().data.get_ref() == plain.get_ref());
        }
    }

    #[test]
    fn output_collects_small_writes_and_answers_position_itself() {
        let sink = Sink::default();
        let mut w = BufOutput::new(Box::new(sink.clone()));
        for i in 0..10_000u32 {
            w.write_all(&i.to_be_bytes()).unwrap();
            assert_eq!(w.stream_position().unwrap(), (i as u64 + 1) * 4);
        }
        w.flush().unwrap();
        let s = sink.0.lock().unwrap();
        assert_eq!(s.data.get_ref().len(), 40_000);
        assert!(s.writes <= 2, "{} writes", s.writes);
        assert_eq!(s.seeks, 1, "one position query");
    }
}
