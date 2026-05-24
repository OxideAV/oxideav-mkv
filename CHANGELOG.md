# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Other

- fuzz: **cargo-fuzz `demux` target** under `fuzz/fuzz_targets/demux.rs`,
  driving `demux::open` + `next_packet` + `seek_to(0, 0)` over arbitrary
  bytes from libFuzzer. Mirrors the ico / qoi / bmp harness shape: own
  `[workspace]`, dedicated `fuzz/Cargo.lock`, curated seed corpus in
  `fuzz/corpus/demux/` (minimal valid Matroska + WebM + EBML-header-only
  files, plus two regression seeds for the bugs found below).
  Scheduled daily via `.github/workflows/fuzz.yml` against the OxideAV
  reusable `crate-fuzz.yml` workflow (30-minute budget). The new
  harness drove five defensive demuxer fixes in the same commit:
  (1) `Cursor`-position + element-size arithmetic now uses
  `u64::saturating_add` in every `body_end = pos + e.size` /
  `ebml_end = pos + hdr.size` site so a crafted u64 VINT size cannot
  overflow the loop bound (caught by an EBML header where the declared
  body extended past `u64::MAX`);
  (2) `parse_fixed_lacing` now returns `n_frames` empty sub-frames
  when `frame_size == 0` instead of calling `<[T]>::chunks_exact(0)`,
  which panics — caught by a `SimpleBlock` with the fixed-lacing flag
  set and a one-byte body; (3) `parse_ebml_lacing` and
  `parse_xiph_lacing` now use checked arithmetic for the
  remaining-bytes / cumulative-size computation so neither integer
  overflow nor a debug-build subtract-with-overflow can panic on
  contrived lacing-header bytes; (4) `ebml::read_bytes` /
  `read_string` now use `Read::take(n).read_to_end()` so the
  allocation grows with actually-readable bytes — a 2^56-byte VINT
  size on a 1 KB input now allocates 1 KB and returns
  `UnexpectedEof`, not multi-terabyte vector growth; (5)
  `parse_chapter_atom` recursion is capped at depth 64 to prevent a
  pathologically nested `Chapters` tree from blowing the
  libfuzzer-sized stack. 19 M fuzz iterations clean post-fix
  (3-minute local run).
- demux + mux: **`CueRelativePosition` round-trip** (RFC 9559
  §5.1.5.1.2.3). The demuxer parses `CueRelativePosition` from each
  `CueTrackPositions` and, when present, repositions the input reader
  directly at the referenced `SimpleBlock` / `BlockGroup` inside the
  target Cluster on `seek_to` (instead of returning the first block of
  the Cluster). The Cluster's `Timestamp` (RFC 9559 §5.1.3.1) is
  captured first so block timecodes remain decodable. Out-of-range or
  malformed positions degrade gracefully to the legacy
  scan-from-cluster-start path. The muxer now writes
  `CueRelativePosition` for every Cues entry it emits — measured from
  the first possible element position inside the Cluster (i.e.
  immediately after the Cluster id+size header). Adds 6 integration
  tests covering middle / last / first / out-of-range positions, the
  muxer's on-disk Cues bytes, and a full mux→demux round-trip.
- demux: **typed `Chapters` accessor** (RFC 9559 §5.1.7).
  `MkvDemuxer::chapters() -> &[Edition]` exposes the structured chapter
  tree the flat `chapter:N:*` metadata view collapses. Every
  `EditionEntry` keeps its `EditionUID`, `EditionFlagDefault` and
  `EditionFlagOrdered` flags; every `ChapterAtom` keeps its
  `ChapterUID`, `ChapterStringUID`, full-precision `ChapterTimeStart` /
  `ChapterTimeEnd` nanoseconds, `ChapterFlagHidden`, all multilingual
  `ChapterDisplay` rows (`ChapString` + `ChapLanguage` /
  `ChapLanguageBCP47` + `ChapCountry`), and any nested child atoms
  (the spec marks `ChapterAtom` as recursive). Atoms are 1-indexed
  depth-first in document order — the same index the flat
  `chapter:N:*` keys and `TagChapterUID`-resolved tags use, now extended
  to nested chapters (previously only top-level atoms got an index).
  Adds `EDITION_FLAG_DEFAULT` / `EDITION_FLAG_ORDERED` /
  `CHAPTER_STRING_UID` / `CHAP_LANGUAGE_BCP47` element IDs.
- demux: **apply Header-Stripping on read** (RFC 9559 §5.1.4.1.31.6 algo 3,
  §5.1.4.1.31.7). Header Stripping is the one `ContentEncoding` transform
  the container can reverse without a codec: the `ContentCompSettings` bytes
  were stripped from the front of each frame on write, so the demuxer now
  prepends them back and `next_packet` returns the original frame data.
  Block scope (§5.1.4.1.31.3 bit 0x1, "all frame contents excluding lacing
  data") is honoured per de-laced frame; a chain of multiple Header-
  Stripping steps is combined in decode order (highest `ContentEncodingOrder`
  first, §5.1.4.1.31.2). When the Block-scoped chain contains a step the
  container can't undo (zlib / bzlib / lzo1x compression or encryption) the
  packet passes through encoded rather than being partially stripped, and a
  Private-scope (`CodecPrivate`-only, bit 0x2) Header-Stripping leaves frame
  data untouched. zlib/bzlib/lzo1x decompression and decryption remain out
  of container scope.
- demux: **`ContentEncodings` typed decode** (RFC 9559 §5.1.4.1.31). A
  track's per-frame transformation chain (compression and/or encryption
  applied to frame data / `CodecPrivate` before the bytes hit Blocks) now
  surfaces through `MkvDemuxer::content_encodings(stream_index)` and the
  per-stream `all_content_encodings()` slice. Each `ContentEncoding`
  carries its `ContentEncodingOrder`, a `ContentEncodingScope` bit field
  (`block()` / `private()` / `next()`), and a `ContentEncodingTransform`
  enum — `Compression { algo: ContentCompAlgo, settings }` (§5.1.4.1.31.5,
  algo `Zlib` / `Bzlib` / `Lzo1x` / `HeaderStripping` / `Other`, settings
  carrying the header-stripping bytes per §5.1.4.1.31.7) or `Encryption
  { algo: ContentEncAlgo, key_id, aes_cipher_mode }` (§5.1.4.1.31.8, algo
  `None` / `Des` / `TripleDes` / `Twofish` / `Blowfish` / `Aes` / `Other`,
  `ContentEncKeyID` bytes, and the nested `ContentEncAESSettings`
  `AESSettingsCipherMode` as `Ctr` / `Cbc` / `Other`). The list is
  pre-sorted into decode order (highest `ContentEncodingOrder` first per
  §5.1.4.1.31.2) and element defaults are honoured (order 0, scope 0x1
  Block, type 0 compression, comp-algo 0 zlib). Parse-only: the demuxer
  surfaces the headers and never decompresses or decrypts a frame. New
  public `demux::{ContentEncodings, ContentEncoding, ContentEncodingScope,
  ContentEncodingTransform, ContentCompAlgo, ContentEncAlgo,
  AesCipherMode}` types.
- demux: **`TrackOperation` typed decode** (RFC 9559 §5.1.4.1.30). A
  virtual track assembled from other tracks now surfaces through
  `MkvDemuxer::track_operation(stream_index)` and the per-stream
  `track_operations()` slice. `TrackCombinePlanes` (§5.1.4.1.30.1) decodes
  into a `Vec<TrackPlane>` — each `TrackPlane` pairs a referenced track
  with its `TrackPlaneType` (`LeftEye` / `RightEye` / `Background`, with
  `Other(u64)` preserving FCFS-registry values per §27.17) — and
  `TrackJoinBlocks` (§5.1.4.1.30.5) into a `Vec<TrackRef>`. Every
  `TrackPlaneUID` / `TrackJoinUID` is resolved back to a `TrackRef`
  carrying both the on-disk `TrackUID` and the matching 0-indexed stream
  index (`None` for a dangling reference, kept rather than dropped). A
  `TrackPlane` missing its mandatory `TrackPlaneUID` and a zero
  `TrackJoinUID` ("not 0" per spec) are dropped. New public
  `demux::{TrackOperation, TrackPlane, TrackPlaneType, TrackRef}` types.
- demux: **CRC-32 validation** on Top-Level master elements (RFC 8794
  §11.3.1, RFC 9559 §6.2). When `Info` / `Tracks` / `Tags` / `Cues` /
  `Chapters` / `Attachments` / `SeekHead` carries a leading `CRC-32`
  child, the demuxer recomputes the IEEE CRC-32 (reflected poly
  `0xEDB88320`, init `0xFFFFFFFF`, final ones-complement, little-endian
  storage) over the rest of the element and records a `CrcStatus
  { element_id, stored, computed }`. New `MkvDemuxer::crc_status() ->
  &[CrcStatus]` accessor (with `CrcStatus::is_valid()`) surfaces the
  results. Validation is informational: a mismatch does not abort the
  open (RFC 8794 §12 lets a reader MAY-ignore the data); strict callers
  inspect the slice. New public `ebml::crc32_ieee` helper (table built
  at runtime — no numeric table transcribed). Pinned by the canonical
  `crc32("123456789") == 0xCBF43926` check value.
- mux: opt-in **block lacing** on write (RFC 9559 §5.1.4.5.5,
  §10.3). New `MkvMuxer::with_block_lacing(LacingMode)` aggregates
  same-track, same-keyframe-status consecutive frames into a
  single laced `SimpleBlock` — Xiph (255-additive octets), EBML
  (signed-VINT deltas), or fixed-size (no per-frame header).
  Defaults to `LacingMode::None` (one frame per Block, byte-
  identical with prior versions). Per-Block frame cap is 8;
  cluster boundaries flush. When opted in, the muxer writes
  `TrackEntry.FlagLacing = 1` and sets the LACING bits in the
  SimpleBlock flags byte per §10.2.
- demux: typed `MkvDemuxer::tags() -> &[Tag]` accessor exposes
  `Targets` (`TargetType` string + `TargetTypeValue` + resolved
  `TargetUid` references), per-`SimpleTag` language /
  `TagLanguageBCP47` / `TagDefault` flag, and binary `TagBinary`
  payloads (cover-art bytes etc.) that the legacy flat
  `metadata()` view drops. Multi-UID `Targets` masters preserve
  every resolvable reference; dangling non-zero UIDs are filtered
  out per RFC 9559 §5.1.8.1.1.3..§5.1.8.1.1.6. New
  `demux::open_typed` returns the concrete `MkvDemuxer` so callers
  can reach the new accessor; the trait-returning `demux::open`
  is unchanged.
- mux: add `Chapters` encoding (RFC 9559 §5.1.7). New
  `MkvMuxer::add_chapter(start_ns, end_ns, title)` queues an
  English-language `ChapterAtom`; `add_chapter_full(MkvChapter)`
  supports multilingual `ChapterDisplay` rows with optional
  `ChapCountry`. Chapters are materialised as one `EditionEntry`
  between Tracks and the first Cluster, with the SeekHead
  `Chapters` slot patched to its real offset (or voided if no
  chapters were queued). Unblocks DVD Phase 3b
  (chapter names from the IFO program-chain into MKV output).
- demux: resolve `Tags.Targets.Tag*UID` against track / chapter /
  attachment / edition UIDs and emit scope-prefixed metadata keys
  (`tag:track:N:<name>` etc.); unresolved non-zero UIDs are dropped per
  RFC 9559 §5.1.8.1.1.x

## [0.0.7](https://github.com/OxideAV/oxideav-mkv/compare/v0.0.6...v0.0.7) - 2026-05-06

### Other

- drop stale REGISTRARS / with_all_features intra-doc links
- drop dead `linkme` dep
- registry calls: rename make_decoder/make_encoder → first_decoder/first_encoder
- auto-register via oxideav_core::register! macro (linkme distributed slice)

## [0.0.6](https://github.com/OxideAV/oxideav-mkv/compare/v0.0.5...v0.0.6) - 2026-05-04

### Other

- emit SeekHead at top of Segment for Info / Tracks / Cues
- surface subtitle tracks with MediaType::Subtitle + map S_* codec ids
- surface Matroska Attachments in metadata
- surface Matroska Chapters in metadata
- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows
- pin release-plz to patch-only bumps

### Added

- mux: emit `SeekHead` at the top of the Segment with Seek entries for
  Info, Tracks, and Cues. Players that pre-walk the SeekHead (mpv,
  Chromium) can now jump to Cues without scanning the file. The Cues
  SeekPosition is patched in `write_trailer` once the real Cues offset
  is known; if no packets were written, the Cues slot is rewritten as
  a Void so the SeekHead doesn't point at offset 0.
- demux: parse `Chapters` master element — chapter atoms now surface in
  `Demuxer::metadata()` as `chapter:N:start_ms` / `chapter:N:end_ms` /
  `chapter:N:title` keys (ns→ms, 1-indexed).
- demux: parse `Attachments` — `AttachedFile` entries surface as
  `attachment:N:filename` / `:mime_type` / `:size_bytes`. Payload bytes
  are skipped via seek (no allocation), so a multi-megabyte embedded
  font no longer pulls a copy into RAM just to read its filename.
- codec_id: map Matroska subtitle CodecIDs (`S_TEXT/UTF8`, `S_TEXT/SSA`,
  `S_TEXT/ASS`, `S_TEXT/WEBVTT`, `S_TEXT/USF`, `S_VOBSUB`, `S_HDMV/PGS`,
  `S_HDMV/TEXTST`, `S_DVBSUB`, `S_KATE`) to short oxideav codec ids
  both ways, so subtitle tracks no longer pass through as
  `mkv:S_TEXT/UTF8` opaque ids.

### Fixed

- demux: subtitle tracks (TrackType=17) now surface with
  `MediaType::Subtitle` rather than `MediaType::Data`. Downstream
  filtering / probing tools that key on `media_type == Subtitle` can
  now find them.

## [0.0.5](https://github.com/OxideAV/oxideav-mkv/compare/v0.0.4...v0.0.5) - 2026-04-25

### Other

- drop oxideav-codec/oxideav-container shims, import from oxideav-core
- resolve V_MS/VFW/FOURCC tunnels via the inner FourCC
- bump oxideav-container dep to "0.1"
- drop Cargo.lock — this crate is a library
- bump oxideav-core / oxideav-codec dep examples to "0.1"
- bump to oxideav-core 0.1.1 + codec 0.1.1
- migrate register() to CodecInfo builder
- bump oxideav-core + oxideav-codec deps to "0.1"
- delegate codec-id lookup to CodecResolver (registry-backed)
- thread &dyn CodecResolver through open()

## [0.0.4](https://github.com/OxideAV/oxideav-mkv/compare/v0.0.3...v0.0.4) - 2026-04-18

### Other

- add Matroska V_MPEG1 and V_MPEG2 mappings
- release v0.0.3

## [0.0.3](https://github.com/OxideAV/oxideav-mkv/releases/tag/v0.0.3) - 2026-04-17

### Fixed

- fix all clippy warnings for CI -D warnings gate

### Other

- rewrite README to match what the crate actually does
- add end-to-end demux-then-decode pipeline test
- muxer emits Cues at end; demuxer walks unknown-size Clusters when scanning
- make crate standalone (pin deps, add CI + release-plz + LICENSE)
- move repo to OxideAV/oxideav-workspace
- add publish metadata (readme/homepage/keywords/categories)
- implement seek_to via Cues index
- promote WebM to first-class — separate fourcc + muxer DocType + codec whitelist
- detect format by content probe, not by file extension
- surface metadata + duration_micros across all containers
- scaffold decoder — 3 headers + Huffman trees + packet classify
- RFC 9043 v3 slice layout + CRC-32 parity; ffmpeg→us decodes
- add lossless video codec — bit-exact self-roundtrip, RFC 9043 v3
- add rustfmt + clippy gates; release: macOS universal binary
- add Matroska (MKV) container + Opus crate + proper Ogg header handling
