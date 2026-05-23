# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Other

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
