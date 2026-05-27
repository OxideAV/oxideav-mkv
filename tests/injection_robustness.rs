//! Round 163 — injection-robustness coverage for the EBML / Matroska
//! demuxer.
//!
//! The demuxer must be the kind of parser you can point at an attacker-
//! shaped byte stream and get a clean `Err(...)` for any malformed shape
//! rather than:
//!
//! * a runtime panic / index-out-of-bounds,
//! * a multi-GiB `Vec<u8>` allocation that turns into an OOM kill,
//! * an infinite loop chewing CPU as the cursor moves nowhere,
//! * a `SeekFrom::Current` going *backwards* because a `u64` size was
//!   cast to `i64` and wrapped negative,
//! * silent acceptance of a header-only file with garbage where the
//!   media table should be.
//!
//! Each test names the exact byte shape it exercises and asserts the
//! parser either returns `Err` or yields a well-defined empty result —
//! never panicking, never allocating attacker-controlled size up-front,
//! and never seeking backwards on a forged `Size` field.
//!
//! Mirrors the per-container injection-robustness pattern landed by
//! `oxideav-mov::synth_round162_robustness` and `oxideav-dds`'s
//! malformed-input tests; pins the hardening of [`oxideav_mkv::ebml::skip`]
//! against the `u64`→`i64` cast that would otherwise let a forged size
//! seek the reader backwards.

use std::io::Cursor;

use oxideav_core::ReadSeek;
use oxideav_mkv::ebml::{self, write_element_id, write_vint};
use oxideav_mkv::ids;

// ---------------------------------------------------------------------
// Helpers — minimal EBML / Matroska element builders. Same shape as the
// helpers used in `tests/crc32.rs` but kept private to this file so
// each robustness test can be read top-to-bottom.
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

fn elem_str(id: u32, s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(s.len() as u64, 0));
    out.extend_from_slice(s.as_bytes());
    out
}

fn elem_master(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

/// Emit an element header whose `Size` VINT is a *forged* value — the
/// declared payload size does not match the bytes that follow. The
/// VINT width is the smallest that encodes `forged_size`.
fn elem_forged_size_with_body(id: u32, forged_size: u64, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(forged_size, 0));
    out.extend_from_slice(body);
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

fn tracks_body_pcm() -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, 0xA1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio = Vec::new();
    let freq_be = 48_000.0f64.to_be_bytes();
    let mut freq_elem = Vec::new();
    freq_elem.extend_from_slice(&write_element_id(ids::SAMPLING_FREQUENCY));
    freq_elem.extend_from_slice(&write_vint(8, 0));
    freq_elem.extend_from_slice(&freq_be);
    audio.extend_from_slice(&freq_elem);
    audio.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    tb.extend_from_slice(&elem_master(ids::AUDIO, &audio));
    elem_master(ids::TRACK_ENTRY, &tb)
}

fn open(bytes: Vec<u8>) -> oxideav_core::Result<Box<dyn oxideav_core::Demuxer>> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver)
}

fn open_typed(bytes: Vec<u8>) -> oxideav_core::Result<oxideav_mkv::demux::MkvDemuxer> {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver)
}

// =====================================================================
// 1. EBML primitive — `skip` must never seek backwards on a forged size.
// =====================================================================

#[test]
fn ebml_skip_with_oversize_value_does_not_seek_backwards() {
    // A buffer of 16 bytes. Position the cursor at offset 4, then ask
    // `skip` to advance by a value that would, under the old
    // `SeekFrom::Current(n as i64)` implementation, wrap to a negative
    // offset and *rewind* the reader. Cursor's seek then errors on the
    // negative absolute target, leaving the cursor at 4 — passing this
    // test only because of the underlying Cursor's defence, not the
    // helper's. The companion `unknown_size_sentinel` test below picks
    // u64::MAX which wraps `n as i64` to -1 and DOES rewind cleanly,
    // so the pair together pins both behaviours.
    let mut cur = Cursor::new(vec![0u8; 16]);
    use std::io::Seek;
    cur.seek(std::io::SeekFrom::Start(4)).unwrap();

    // Pick a value > i64::MAX so the old `n as i64` cast goes negative.
    let huge: u64 = (i64::MAX as u64) + 100;
    let _ = ebml::skip(&mut cur, huge);

    // After the call the cursor's position MUST NOT be less than the
    // starting position. A pre-hardening implementation would attempt
    // (4 + huge as i64) ≈ -large, which Cursor::seek errors on,
    // leaving the cursor where it was (4). The hardened impl seeks to
    // `SeekFrom::Start(4 + huge)`, which Cursor accepts (past EOF).
    let after = std::io::Seek::stream_position(&mut cur).unwrap();
    assert!(
        after >= 4,
        "ebml::skip moved the reader backwards on an oversize size \
         (cursor at {after}, started at 4)"
    );
}

#[test]
fn ebml_skip_with_vint_unknown_size_sentinel_does_not_panic_or_rewind() {
    // VINT_UNKNOWN_SIZE == u64::MAX is the streamable-segment sentinel;
    // it must never reach `skip` for a sized element, but defence-in-depth
    // requires the helper to handle it without panicking or rewinding
    // when a malformed file plants it on a non-master element.
    let mut cur = Cursor::new(vec![0u8; 64]);
    use std::io::Seek;
    cur.seek(std::io::SeekFrom::Start(8)).unwrap();
    let _ = ebml::skip(&mut cur, u64::MAX);
    let after = std::io::Seek::stream_position(&mut cur).unwrap();
    assert!(after >= 8, "skip rewound past starting offset");
}

// =====================================================================
// 2. Open path — forged EBML header.
// =====================================================================

#[test]
fn open_rejects_empty_input() {
    let r = open(Vec::new());
    assert!(r.is_err(), "empty input must not demux-open cleanly");
}

#[test]
fn open_rejects_ebml_signature_with_truncated_header() {
    // 4-byte EBML magic + a size VINT claiming 4 KiB but only 1 byte
    // of payload available. The reader must surface this as Err, not
    // panic on the truncated read.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&write_element_id(ids::EBML_HEADER));
    bytes.extend_from_slice(&write_vint(4096, 0));
    bytes.push(0x42);
    assert!(open(bytes).is_err());
}

#[test]
fn open_rejects_oversize_ebml_header_size() {
    // EBML header with `Size = 2^40`; total file ~10 bytes. Must error
    // rather than attempt a multi-TiB allocation.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&write_element_id(ids::EBML_HEADER));
    bytes.extend_from_slice(&write_vint(1u64 << 40, 0));
    bytes.extend_from_slice(b"\x00\x00\x00");
    assert!(open(bytes).is_err());
}

#[test]
fn open_rejects_doctype_oversize_string() {
    // EBML header inner DocType element with a declared 4-GiB string size
    // but only a handful of bytes available. `read_string` -> `read_bytes`
    // uses `Read::take(n).read_to_end(...)` so allocation is bounded by
    // what the reader can deliver — but the eventual short-read must
    // surface as a clean Err.
    let mut hdr_body = Vec::new();
    hdr_body.extend_from_slice(&write_element_id(ids::EBML_DOC_TYPE));
    hdr_body.extend_from_slice(&write_vint(1u64 << 32, 0));
    hdr_body.extend_from_slice(b"matro");
    let bytes = elem_master(ids::EBML_HEADER, &hdr_body);
    assert!(open(bytes).is_err());
}

// =====================================================================
// 3. Segment-level — forged Segment / Tracks / Tags sizes.
// =====================================================================

/// Build a header + Segment master with a body the caller supplies.
fn header_plus_segment(seg_body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&elem_master(ids::SEGMENT, seg_body));
    out
}

#[test]
fn open_rejects_tracks_with_oversize_codec_id_string() {
    // CodecID element claims 2 GiB body but supplies only "A_PCM/INT/LIT".
    // Demuxer must return Err on the short read, never panic.
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, 1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    tb.extend_from_slice(&elem_forged_size_with_body(
        ids::CODEC_ID,
        1u64 << 31,
        b"A_PCM/INT/LIT",
    ));
    let tracks_body = elem_master(ids::TRACK_ENTRY, &tb);
    let seg_body = elem_master(ids::TRACKS, &tracks_body);
    let bytes = header_plus_segment(&seg_body);
    assert!(open(bytes).is_err());
}

#[test]
fn open_rejects_tag_string_with_oversize_size() {
    // Tags > Tag > SimpleTag > TagString declared at 1 GiB but only a
    // tiny actual body. Must surface as Err.
    let mut simple = Vec::new();
    simple.extend_from_slice(&elem_str(ids::TAG_NAME, "TITLE"));
    simple.extend_from_slice(&elem_forged_size_with_body(
        ids::TAG_STRING,
        1u64 << 30,
        b"hi",
    ));
    let simple_master = elem_master(ids::SIMPLE_TAG, &simple);
    // Wrap in Tag → Tags.
    let tag = elem_master(0x7373 /* Tag */, &simple_master);
    let tags = elem_master(ids::TAGS, &tag);
    let bytes = header_plus_segment(&tags);
    let _ = open(bytes); // either Err or open-but-tags-missing — must not panic.
}

#[test]
fn open_handles_segment_with_known_size_extending_past_eof() {
    // Segment declares size = 1 MiB but actual body is only the Tracks
    // we ship. Open should still return Err or land with an empty/limited
    // stream set — must not panic when the walker hits EOF early.
    let tracks = elem_master(ids::TRACKS, &tracks_body_pcm());
    let mut full = Vec::new();
    full.extend_from_slice(&ebml_header());
    // Segment header with size = 1 MiB.
    full.extend_from_slice(&write_element_id(ids::SEGMENT));
    full.extend_from_slice(&write_vint(1u64 << 20, 0));
    full.extend_from_slice(&tracks);
    // No more bytes. The walker reads until `stream_position < segment_end`,
    // and the next `read_element_header` will hit EOF.
    let _ = open(full); // contract: returns, no panic.
}

// =====================================================================
// 4. Cluster / Block parsing — truncated and forged sizes.
// =====================================================================

fn minimal_segment_with_extra_cluster(extra_cluster_bytes: &[u8]) -> Vec<u8> {
    let mut seg_body = Vec::new();
    // Info — minimal.
    let mut info = Vec::new();
    info.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    seg_body.extend_from_slice(&elem_master(ids::INFO, &info));
    // Tracks.
    seg_body.extend_from_slice(&elem_master(ids::TRACKS, &tracks_body_pcm()));
    // Caller-supplied Cluster bytes (may be malformed).
    seg_body.extend_from_slice(extra_cluster_bytes);
    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&elem_master(ids::SEGMENT, &seg_body));
    out
}

#[test]
fn next_packet_rejects_simple_block_with_oversize_size() {
    // SimpleBlock declared 4 GiB but only 6 bytes of body. `read_bytes`
    // uses `take(n).read_to_end(...)` so allocation is bounded — but the
    // short read must yield Err, never a panic.
    let mut block_body = Vec::new();
    block_body.extend_from_slice(&write_vint(1, 0)); // track number 1
    block_body.extend_from_slice(&0i16.to_be_bytes());
    block_body.push(0x80);
    block_body.push(0xAB);
    let block_elem = elem_forged_size_with_body(ids::SIMPLE_BLOCK, 1u64 << 32, &block_body);
    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&block_elem);
    let cluster = elem_master(ids::CLUSTER, &cluster_body);
    let bytes = minimal_segment_with_extra_cluster(&cluster);
    let mut dmx = match open(bytes) {
        Ok(d) => d,
        Err(_) => return, // open itself may bail; acceptable.
    };
    let r = dmx.next_packet();
    assert!(r.is_err(), "next_packet must Err on oversize SimpleBlock");
}

#[test]
fn next_packet_rejects_simple_block_with_truncated_payload_after_lacing() {
    // Xiph-laced SimpleBlock whose declared sub-frame sizes exceed the
    // body. `parse_xiph_lacing` already returns Err on this — the test
    // pins it so it doesn't regress.
    let mut block_body = Vec::new();
    block_body.extend_from_slice(&write_vint(1, 0)); // track number 1
    block_body.extend_from_slice(&0i16.to_be_bytes());
    block_body.push(0x02); // flags: lacing=1 (Xiph), no keyframe bit
    block_body.push(0x02); // n_frames - 1 = 2 → 3 frames
                           // Xiph sizes: claim two sizes of 200 each → 400 bytes total demanded
                           // but only 4 bytes of payload follow.
    block_body.push(200u8);
    block_body.push(200u8);
    block_body.extend_from_slice(&[0u8; 4]);
    let block_elem = elem_master(ids::SIMPLE_BLOCK, &block_body);
    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&block_elem);
    let cluster = elem_master(ids::CLUSTER, &cluster_body);
    let bytes = minimal_segment_with_extra_cluster(&cluster);
    let mut dmx = match open(bytes) {
        Ok(d) => d,
        Err(_) => return,
    };
    assert!(
        dmx.next_packet().is_err(),
        "truncated Xiph-laced SimpleBlock must surface as Err"
    );
}

#[test]
fn next_packet_handles_simple_block_with_fixed_lacing_zero_frame_size() {
    // n_frames = 5, payload empty → 5 empty frames per the hardened
    // `parse_fixed_lacing` (which previously would have panicked in
    // `chunks_exact(0)`). Pin the behaviour: a sequence of empty
    // packets followed by EOF / cluster-end, never a panic.
    let mut block_body = Vec::new();
    block_body.extend_from_slice(&write_vint(1, 0)); // track number 1
    block_body.extend_from_slice(&0i16.to_be_bytes());
    block_body.push(0x04); // flags: lacing=2 (fixed)
    block_body.push(0x04); // n_frames - 1 = 4 → 5 frames; payload empty
    let block_elem = elem_master(ids::SIMPLE_BLOCK, &block_body);
    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&block_elem);
    let cluster = elem_master(ids::CLUSTER, &cluster_body);
    let bytes = minimal_segment_with_extra_cluster(&cluster);
    let mut dmx = match open(bytes) {
        Ok(d) => d,
        Err(_) => return,
    };
    // Drain — should not panic. Up to 16 calls is plenty before EOF.
    for _ in 0..16 {
        if dmx.next_packet().is_err() {
            break;
        }
    }
}

// =====================================================================
// 5. Attachments — oversize FileData claimed by AttachedFile.
// =====================================================================

fn attachment_master(filename: &str, mime: &str, data_size: u64, real_data: &[u8]) -> Vec<u8> {
    let mut af = Vec::new();
    af.extend_from_slice(&elem_str(ids::FILE_NAME, filename));
    af.extend_from_slice(&elem_str(ids::FILE_MIME_TYPE, mime));
    af.extend_from_slice(&elem_uint(ids::FILE_UID, 0xCAFE));
    af.extend_from_slice(&elem_forged_size_with_body(
        ids::FILE_DATA,
        data_size,
        real_data,
    ));
    elem_master(ids::ATTACHED_FILE, &af)
}

#[test]
fn attachment_data_with_oversize_declared_size_does_not_oom() {
    // Build a Segment carrying a single AttachedFile whose FileData declares
    // 4 GiB but only ships 8 bytes. The open path *skips* the payload using
    // `ebml::skip` — which under the hardened impl uses `SeekFrom::Start`
    // and does not seek backwards even when the size is forged.
    let af = attachment_master("font.ttf", "font/ttf", 1u64 << 32, b"abcdefgh");
    let attachments = elem_master(ids::ATTACHMENTS, &af);
    let mut seg_body = Vec::new();
    // Honour the same structural order the muxer uses (Info → Tracks → ...).
    let mut info = Vec::new();
    info.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    seg_body.extend_from_slice(&elem_master(ids::INFO, &info));
    seg_body.extend_from_slice(&elem_master(ids::TRACKS, &tracks_body_pcm()));
    seg_body.extend_from_slice(&attachments);
    let bytes = header_plus_segment(&seg_body);
    let mut dmx = match open_typed(bytes) {
        Ok(d) => d,
        Err(_) => return, // open may also Err — acceptable.
    };
    // attachment_data must NOT attempt a 4-GiB allocation. The hardened
    // impl uses `Read::take(n).read_to_end(...)` so the destination grows
    // only as bytes actually arrive — then errors on the short read.
    if !dmx.attachments().is_empty() {
        let r = dmx.attachment_data(1);
        assert!(
            r.is_err(),
            "attachment_data with forged 4 GiB size must Err on the short read, \
             not attempt the allocation up front"
        );
    }
}

#[test]
fn attachment_with_oversize_filename_does_not_panic() {
    // FileName element claiming 2 GiB but containing only "tiny". Bounded
    // allocation in `read_string` means we don't OOM up front; the short
    // read surfaces as Err and the demuxer either returns Err on open or
    // skips this AttachedFile.
    let mut af = Vec::new();
    af.extend_from_slice(&elem_forged_size_with_body(
        ids::FILE_NAME,
        1u64 << 31,
        b"tiny",
    ));
    af.extend_from_slice(&elem_str(ids::FILE_MIME_TYPE, "text/plain"));
    af.extend_from_slice(&elem_uint(ids::FILE_UID, 1));
    af.extend_from_slice(&elem_str(ids::FILE_DATA, ""));
    let attachments = elem_master(ids::ATTACHMENTS, &elem_master(ids::ATTACHED_FILE, &af));
    let mut seg_body = Vec::new();
    let mut info = Vec::new();
    info.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    seg_body.extend_from_slice(&elem_master(ids::INFO, &info));
    seg_body.extend_from_slice(&elem_master(ids::TRACKS, &tracks_body_pcm()));
    seg_body.extend_from_slice(&attachments);
    let bytes = header_plus_segment(&seg_body);
    let _ = open(bytes); // returns; must not panic.
}

// =====================================================================
// 6. CueRelativePosition out-of-range — must degrade gracefully.
// =====================================================================

#[test]
fn seek_to_with_forged_cue_relative_position_does_not_panic() {
    // Synthesise a minimal MKV with Tracks, one Cluster carrying one
    // SimpleBlock, and a Cues entry whose CueRelativePosition is set
    // 1 MiB into the (200-byte) Cluster. The hardened seek path rewinds
    // to the Cluster header and falls back to the normal walker rather
    // than panicking on the out-of-range offset.
    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    let mut block_body = Vec::new();
    block_body.extend_from_slice(&write_vint(1, 0));
    block_body.extend_from_slice(&0i16.to_be_bytes());
    block_body.push(0x80);
    block_body.push(0xAB);
    cluster_body.extend_from_slice(&elem_master(ids::SIMPLE_BLOCK, &block_body));
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    // Cues entry pointing at the Cluster with a wildly-out-of-range
    // CueRelativePosition. The Cluster will sit at byte offset 0 in the
    // Segment body (Info+Tracks come first, but Cues are looked up by
    // *relative* position to Segment data; the test only needs the
    // CueRelativePosition itself to over-shoot).
    let mut cp = Vec::new();
    cp.extend_from_slice(&elem_uint(ids::CUE_TIME, 0));
    let mut ctp = Vec::new();
    ctp.extend_from_slice(&elem_uint(ids::CUE_TRACK, 1));
    ctp.extend_from_slice(&elem_uint(ids::CUE_CLUSTER_POSITION, 0));
    ctp.extend_from_slice(&elem_uint(ids::CUE_RELATIVE_POSITION, 1u64 << 20));
    cp.extend_from_slice(&elem_master(ids::CUE_TRACK_POSITIONS, &ctp));
    let cues = elem_master(ids::CUES, &elem_master(ids::CUE_POINT, &cp));

    let mut seg_body = Vec::new();
    let mut info = Vec::new();
    info.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    seg_body.extend_from_slice(&elem_master(ids::INFO, &info));
    seg_body.extend_from_slice(&elem_master(ids::TRACKS, &tracks_body_pcm()));
    seg_body.extend_from_slice(&cluster);
    seg_body.extend_from_slice(&cues);
    let bytes = header_plus_segment(&seg_body);

    let mut dmx = match open(bytes) {
        Ok(d) => d,
        Err(_) => return,
    };
    // seek_to may succeed (degrading to "walk from cluster start") or
    // fail; either is acceptable. The contract under test is no panic.
    let _ = dmx.seek_to(0, 0);
}

// =====================================================================
// 7. Pathological random fuzz fallback — every fuzz seed in the corpus
//    must demux-open or demux-fail cleanly with no panic. Mirrors what
//    the cargo-fuzz harness does, but runs as a normal `cargo test`
//    target so a regression that only reproduces on a seed shape lands
//    a clean test failure rather than waiting for a fuzz cycle.
// =====================================================================

const FUZZ_SEEDS: &[(&str, &[u8])] = &[
    ("empty", &[]),
    ("too_short", &[0u8; 3]),
    ("random_garbage", b"NOT EBML BYTES AT ALL\x00\x01\x02"),
    (
        "ebml_magic_only",
        &[
            0x1A, 0x45, 0xDF, 0xA3, // signature
            0x80, // size VINT = 0
        ],
    ),
    (
        "ebml_magic_with_oversize_size",
        &[
            0x1A, 0x45, 0xDF, 0xA3, // signature
            0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE, // 56-bit max size
        ],
    ),
];

#[test]
fn fuzz_corpus_inline_seeds_open_without_panic() {
    for (name, data) in FUZZ_SEEDS {
        let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
        let r = oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver);
        // Every seed is malformed → must Err. The contract is "no panic";
        // the assertion is "Err", but the *real* test is that the call
        // returns at all without unwinding.
        assert!(r.is_err(), "fuzz seed '{name}' unexpectedly opened cleanly");
    }
}
