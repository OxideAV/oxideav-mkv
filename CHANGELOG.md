# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Other

- mux: write-side `Attachments` (RFC 9559 §5.1.6) via `MkvMuxer::add_attachment` + `MkvAttachment`. `AttachedFile` children written in demux parse order (`FileDescription`/`FileName`/`FileMediaType`/`FileData`/`FileUID`); `FileUID` auto-derives from the 1-based attachment index when caller passes `None`, and explicit `Some(0)` is rejected per spec `range: not 0`. SeekHead extended to 5 entries (Info/Tracks/Chapters/Attachments/Cues); the new Attachments slot is either patched to the real element offset or voided when no attachments were queued.

## [0.0.8](https://github.com/OxideAV/oxideav-mkv/compare/v0.0.7...v0.0.8) - 2026-05-29

### Other

- per-Cluster CRC-32 validation (RFC 8794 §11.3.1, RFC 9559 §6.2)
- typed AlphaMode / AspectRatioType / UncompressedFourCC decode (RFC 9559 §5.1.4.1.28.4 / Appendix A.24 / §5.1.4.1.28.15)
- typed Video > Projection decode (RFC 9559 §5.1.4.1.28.41)
- harden ebml::skip + add 16 injection-robustness tests
- typed Video > StereoMode decode (RFC 9559 §5.1.4.1.28.3)
- typed Video > Colour + MasteringMetadata decode (RFC 9559 §5.1.4.1.28.16)
- typed Video geometry quartet decode (RFC 9559 §5.1.4.1.28.8-14)
- typed Video FlagInterlaced + FieldOrder decode (RFC 9559 §5.1.4.1.28.1 / §5.1.4.1.28.2)
- typed decode of ChapProcess sub-tree (RFC 9559 §5.1.7.1.4.14-19)
- extend typed Chapter with ChapterFlagEnabled + Medium-Linking fields (RFC 9559 §5.1.7.1.4.5-8)
- typed Attachments accessor + on-demand payload reader (RFC 9559 §5.1.6)
- cargo-fuzz demux target + 5 defensive demuxer fixes
- demux + mux: CueRelativePosition round-trip (RFC 9559 §5.1.5.1.2.3)
- typed Chapters accessor (RFC 9559 §5.1.7)
- apply Header-Stripping ContentEncoding on read (RFC 9559 §5.1.4.1.31.6 algo 3)
- ContentEncodings typed decode (RFC 9559 §5.1.4.1.31)
- TrackOperation typed decode (RFC 9559 §5.1.4.1.30)
- validate CRC-32 on Top-Level master elements
- opt-in block lacing on write (RFC 9559 §5.1.4.5.5, §10.3)
- typed MkvDemuxer::tags() accessor for RFC 9559 §5.1.8 Tags
- add Chapters encoding (RFC 9559 §5.1.7)
- resolve Tags.Targets.Tag*UID into scope-prefixed metadata keys
- ground the cue-point grouping comment in the EBML spec

### Other

- demux: **per-Cluster CRC-32 validation** (RFC 8794 §11.3.1, RFC 9559
  §6.2). Each `Cluster` carrying a leading `CRC-32` child is now checked
  against its body the first time the demuxer opens it — through the
  legacy `next_packet` walk or through a Cue-driven `seek_to`. Statuses
  surface through the existing `MkvDemuxer::crc_status() -> &[CrcStatus]`
  accessor with `element_id == ids::CLUSTER`. A body-offset dedup keeps a
  back-then-forward seek revisiting the same Cluster from producing two
  statuses for it, and a Cluster declared with the unknown-size VINT
  produces no status (the RFC requires a bounded body). Validation is
  informational — a mismatch never blocks packet emission. Mirrors the
  up-front Top-Level master check that already covered Info / Tracks /
  Tags / Cues / Chapters / Attachments / SeekHead. Closes the
  `What's NOT implemented` "per-Cluster CRC-32 children are not yet
  validated" bullet.
- demux: **typed decode for the three remaining `Video` sub-elements**:
  `AlphaMode` (RFC 9559 §5.1.4.1.28.4), `AspectRatioType` (Appendix
  A.24, reclaimed), and `UncompressedFourCC` (§5.1.4.1.28.15). New
  accessors `MkvDemuxer::video_alpha_mode(stream_index) ->
  Option<AlphaMode>`, `video_aspect_ratio_type(stream_index) ->
  Option<u64>` and `video_uncompressed_fourcc(stream_index) ->
  Option<&UncompressedFourCC>` (plus matching slice accessors). The
  spec default `0` (`AlphaMode::None`) is materialised so an empty
  `Video` master decodes as `Some(AlphaMode::None)`, distinguishable
  from `None` (no `Video` master at all); values outside Table 6 pass
  through `AlphaMode::Other(u64)` per the §27.8 open registry, and a
  convenience `AlphaMode::has_alpha()` returns `true` exactly for
  `Present`. `AspectRatioType` is exposed as the raw `u64` (the
  reclaimed appendix enumerates no values) and has no spec default —
  `None` on absence. `UncompressedFourCC` exposes the verbatim on-disk
  bytes via `as_bytes()` plus `fourcc() -> Option<[u8; 4]>` and
  `as_str() -> Option<String>` (UTF-8 lossy) accessors that return
  `None` whenever the on-disk payload isn't exactly 4 bytes; a
  malformed non-4-byte payload is preserved verbatim so a caller
  debugging a writer issue can still see what was emitted. Pinned by
  12 new `tests/video_alpha_aspect_fourcc.rs` cases covering AlphaMode
  present / default-none / other-passthrough / audio-track-returns-none;
  AspectRatioType present / absent / audio-track; UncompressedFourCC
  present / absent / malformed-length-preserved / audio-track; and a
  combined "three video elements together" round-trip. With this
  change every `Video` sub-element registered under RFC 9559 plus
  Appendix A.24 now surfaces on the demuxer's typed side.

- demux: **typed `Video > Projection` decode** (RFC 9559 §5.1.4.1.28.41,
  including §5.1.4.1.28.42..§5.1.4.1.28.46). New
  `MkvDemuxer::video_projection(stream_index) -> Option<&Projection>`
  (plus the per-stream `video_projections()` slice) folds the
  `Projection` master's children into a single typed `Projection`.
  `ProjectionType` surfaces as a typed enum
  (`Rectangular` / `Equirectangular` / `Cubemap` / `Mesh` /
  `Other(u64)` for values registered after RFC 9559 — §27.15 leaves the
  registry open). `ProjectionPrivate` (the verbatim ISOBMFF box body —
  `equi` / `cbmp` / `mshp` — that pairs with the projection type)
  surfaces verbatim as `Option<&[u8]>`. The yaw / pitch / roll pose
  triple (degrees) surfaces as three `f64`s. Spec defaults are
  materialised: an empty `Projection` master decodes as a fully-typed
  identity projection (rectangular + zero pose), distinguishable from
  `None` (which means "no `Projection` master at all" — the common case
  for ordinary 2D video). Convenience helpers `ProjectionType::is_spherical()`
  and `Projection::is_rotated()` provide the headline yes/no answers.
  Non-video tracks (and video tracks with no `Projection` child) return
  `None`. Pinned by 10 new `tests/video_projection.rs` cases covering:
  the missing-Projection `None` contract; empty-Projection default
  materialisation; round-trip of every Table 18 value plus the
  `Other(u64)` forward-compat slot; the verbatim 12-byte `equi`-shaped
  `ProjectionPrivate` payload; the §5.1.4.1.28.46 worked example
  `<Projection><ProjectionPoseRoll>90</ProjectionPoseRoll></Projection>`
  (signalling a 90° counter-clockwise rotation); 4-byte float pose
  storage as an alternative to 8-byte; the audio-track-has-no-projection
  predicate; forward-compat skip of an unknown sub-element inside
  `Projection`; and the out-of-range `stream_index` `None` contract.

- ebml: **harden `ebml::skip` against forged oversize sizes**. The
  helper now reads `stream_position()`, computes the target with a
  saturating add, and calls `SeekFrom::Start(target)` instead of the
  prior `SeekFrom::Current(n as i64)`. EBML `Size` VINTs can reach
  `2^56 - 2` and the unknown-size sentinel (`VINT_UNKNOWN_SIZE`) is
  `u64::MAX` itself — both wrap `n as i64` to a negative value when
  the old impl cast it for the relative seek, letting a forged size
  rewind the reader and stall the demuxer in a loop. The new impl
  seeks past EOF on an attacker-controlled size (which is fine — the
  next read returns `UnexpectedEof`) and never moves backwards. Pinned
  by sixteen new `tests/injection_robustness.rs` tests covering: the
  bare `skip(huge)` / `skip(u64::MAX)` no-rewind contract; demux-open
  rejection of empty input, EBML-magic-with-truncated-header, an
  oversize EBML-header `Size`, an oversize `DocType` string, an
  oversize `CodecID` on a `TrackEntry`, an oversize `TagString` body,
  and a `Segment` whose declared size extends past EoF;
  `next_packet`-time handling of an oversize-`Size` `SimpleBlock`,
  a Xiph-laced `SimpleBlock` whose declared sub-frame sizes overrun
  the body, and a fixed-laced `SimpleBlock` with `n_frames = 5` over
  an empty payload (zero-frame-size edge case); on-demand
  `attachment_data` short-read on a forged 4 GiB `FileData` size and a
  forged 2 GiB `FileName` string; out-of-range `CueRelativePosition`
  in `seek_to`; and a small inline fuzz-corpus replay of five
  malformed seed shapes. Mirrors the round-162 mov / dds robustness
  pattern. No behaviour change on conforming inputs.
- demux: **typed `Video > StereoMode` decode** (RFC 9559 §5.1.4.1.28.3).
  New `MkvDemuxer::video_stereo_mode(stream_index) -> Option<StereoMode>`
  (and the slice-view `video_stereo_modes()`) returns the typed
  single-track stereo-3D packing for any video TrackEntry — full
  §5.1.4.1.28.3 Table 5 enum coverage (`Mono`, the four
  side-by-side / top-bottom / checkerboard / interleaved / anaglyph /
  both-eyes-laced variants) plus an `Other(u64)` variant for values
  registered after RFC 9559 (§27.7 leaves the registry open). The spec
  default `0` (`Mono`) is materialised, so a `Video` master with no
  explicit `StereoMode` decodes as `Some(StereoMode::Mono)`,
  distinguishable from `None` (track has no `Video` master at all).
  Convenience `StereoMode::is_stereo()` returns `true` for any non-`Mono`
  packing. Adds one element-id constant (`STEREO_MODE`) plus 15 Table-5
  value constants (`STEREO_MODE_*`), and five integration tests covering
  the default-Mono contract, a per-Table-5 round-trip across all 15
  registered values, multi-track + `Other(42)` forward-compat, the
  audio-track / no-`Video`-master contract, and out-of-range
  `stream_index` safety.
- demux: **typed `Video > Colour` decode** (RFC 9559 §5.1.4.1.28.16,
  including §5.1.4.1.28.17..§5.1.4.1.28.40 sub-elements and the
  SMPTE 2086 / CTA-861.3 HDR `MasteringMetadata`). New
  `MkvDemuxer::video_colour(stream_index) -> Option<&VideoColour>` (and
  the slice-view `video_colours()`) folds `MatrixCoefficients`,
  `BitsPerChannel`, `Chroma{Subsampling,Cb}Subsampling{Horz,Vert}`,
  `ChromaSiting{Horz,Vert}`, `Range`, `TransferCharacteristics`,
  `Primaries`, `MaxCLL`, `MaxFALL` and the `MasteringMetadata` master
  (`PrimaryRGBChromaticity{X,Y}` × 6, `WhitePointChromaticity{X,Y}` × 2,
  `Luminance{Max,Min}`) into a single typed record. Enums surface unknown
  values via an `Other(u64)` variant for forward compatibility with §27
  registries; spec defaults are materialised on the typed surface
  (matrix / transfer / primaries default `2` = *unspecified*; chroma
  siting / range / bits-per-channel default `0`). Children with no spec
  default (chroma subsampling, MaxCLL/MaxFALL, every MasteringMetadata
  field) surface as `None` when absent. Adds 25 element-id constants
  (`COLOUR` through `LUMINANCE_MIN`) plus eight Table-13..16 value
  constants, and seven integration tests covering the no-Colour-master
  contract, empty-Colour-master defaults, a BT.709 SDR round-trip, a
  BT.2100 PQ HDR round-trip with full MasteringMetadata (8-byte floats),
  4-byte (f32) MasteringMetadata with a sparse subset of children,
  unknown-enum-value passthrough via `Other(_)`, and the
  audio-track-has-no-Colour contract.
- demux: **typed `Video` geometry quartet decode** (RFC 9559
  §5.1.4.1.28.8..§5.1.4.1.28.14). New
  `MkvDemuxer::video_geometry(stream_index) -> Option<&VideoGeometry>`
  (and the slice-view `video_geometries()`) folds the
  `PixelCrop{Top,Bottom,Left,Right}` hide-window plus the `DisplayWidth` /
  `DisplayHeight` / `DisplayUnit` render-size triple into a single typed
  record. `DisplayUnit` surfaces as the `DisplayUnit` enum (`Pixels` /
  `Centimeters` / `Inches` / `DisplayAspectRatio` / `Unknown` /
  `Other(u64)` for forward-compatibility with the §27.9 "Matroska Display
  Units" registry). `display_width()` / `display_height()` return
  `Option<u64>` — the explicit element when present, otherwise the
  §5.1.4.1.28.12 / §5.1.4.1.28.13 derived default (`PixelWidth -
  PixelCropLeft - PixelCropRight` / `PixelHeight - PixelCropTop -
  PixelCropBottom`) when `DisplayUnit == 0` (pixels), and `None` for any
  other `DisplayUnit` per the spec note "else, there is no default value".
  The §5.1.4.1.28.8..11 PixelCrop defaults (`0`) and §5.1.4.1.28.14
  DisplayUnit default (`0`) are always materialised. Adds seven element-id
  constants (`PIXEL_CROP_{TOP,BOTTOM,LEFT,RIGHT}`, `DISPLAY_WIDTH`,
  `DISPLAY_HEIGHT`, `DISPLAY_UNIT`), five Table 10 value constants, and
  seven integration tests covering an explicit PixelCrop + Display pair,
  the bare-`Video`-master defaults path, derived display sizes after
  PixelCrops, a non-pixel `DisplayUnit` (DAR) with explicit values + a cm
  `DisplayUnit` with no DisplayWidth/Height (no-default path), an
  unregistered DisplayUnit surfacing as `Other(42)`, the
  audio-track-has-no-geometry contract, and a malformed-file
  underflow-returns-`None` case.
- demux: **typed `Video > FlagInterlaced` + `FieldOrder` decode** (RFC 9559
  §5.1.4.1.28.1 + §5.1.4.1.28.2). New
  `MkvDemuxer::video_interlacing(stream_index) -> Option<&VideoInterlacing>`
  (and the slice-view `video_interlacings()`) exposes a track's
  interlacing status — `FlagInterlaced` as the typed `FlagInterlaced` enum
  (`Undetermined` / `Interlaced` / `Progressive` / `Other(u64)`) paired
  with a `field_order()` accessor that returns `Some(FieldOrder)`
  (`Progressive` / `Tff` / `Undetermined` / `Bff` / `TffInterleaved` /
  `BffInterleaved` / `Other(u64)`) only when the track is actually
  interlaced. The §5.1.4.1.28.2 "If FlagInterlaced is not set to 1, this
  element MUST be ignored" rule is honoured by the typed surface: a stray
  `FieldOrder` on a progressive / undetermined track silently resolves to
  `None`. Spec defaults are materialised — a `Video` master with no
  `FlagInterlaced` child decodes as `FlagInterlaced::Undetermined` (the
  §5.1.4.1.28.1 default `0`), an interlaced track with no explicit
  `FieldOrder` decodes as `Some(FieldOrder::Undetermined)` (the
  §5.1.4.1.28.2 default `2`). Adds two element-id constants
  (`FLAG_INTERLACED`, `FIELD_ORDER`), nine value constants from Table 3 +
  Table 4, and four integration tests covering Tff-interlaced +
  progressive siblings, the bare-`Video`-master default path, the
  interlaced-with-default-FieldOrder + unknown-FieldOrder-value cases,
  and the audio-track-has-no-interlacing contract.
- demux: **typed decode of the `ChapProcess` sub-tree** (RFC 9559
  §5.1.7.1.4.14–§5.1.7.1.4.19). The typed `Chapter` now carries a
  `chap_processes: Vec<ChapProcess>` field exposing the chapter-codec
  commands (DVD-menu / Matroska-Script) attached to each `ChapterAtom`.
  New `ChapProcess` struct holds `codec_id` (`ChapProcessCodecID`, spec
  default `0` = Matroska Script materialised), `private`
  (`ChapProcessPrivate`, raw optional bytes) and `commands:
  Vec<ChapProcessCommand>`; each `ChapProcessCommand` carries `time`
  (`ChapProcessTime`, spec default `0` = "during the whole chapter") and
  `data` (`ChapProcessData`, raw command bytes). Payloads are surfaced
  verbatim — the container never executes a chapter command. Adds six
  element-id constants (`CHAP_PROCESS`, `CHAP_PROCESS_CODEC_ID`,
  `CHAP_PROCESS_PRIVATE`, `CHAP_PROCESS_COMMAND`, `CHAP_PROCESS_TIME`,
  `CHAP_PROCESS_DATA`) plus the Table 31 / Table 32 value constants, and
  one roundtrip integration test covering a DVD-menu process with private
  data + two timed commands alongside a Matroska-Script process that
  relies on the codec-id and time spec defaults.
- demux: **extend typed `Chapter` with RFC 9559 §5.1.7.1.4.5–§5.1.7.1.4.8
  sub-elements** previously dropped during the `Chapters` walk.
  `Chapter` now carries `enabled` (`ChapterFlagEnabled`, spec default `1`
  materialised as `true` so consumers don't special-case the absent
  element), `segment_uuid` (`ChapterSegmentUUID`, the raw 16-byte
  SegmentUUID for Medium-Linking Segments per §17.2), `segment_edition_uid`
  (`ChapterSegmentEditionUID`, with `0` suppressed to `None` per the spec's
  "range: not 0"), and `physical_equiv` (`ChapterPhysicalEquiv` —
  DVD/SIDE/etc. physical mapping per §20.4). Adds three element-id
  constants (`CHAPTER_SEGMENT_UUID`, `CHAPTER_SEGMENT_EDITION_UID`,
  `CHAPTER_PHYSICAL_EQUIV`) and a hand-rolled `Default` impl on `Chapter`
  so `Chapter::default().enabled == true` reflects the spec default.
  Adds two integration tests: one exercises every new field against a
  Medium-Linking-style atom alongside a vanilla sibling that verifies
  spec defaults, the other pins the `ChapterSegmentEditionUID = 0 → None`
  contract so the option can be the sole presence check downstream.
- demux: **typed `Attachments` accessor + on-demand payload reader** (RFC
  9559 §5.1.6). `MkvDemuxer::attachments() -> &[Attachment]` exposes the
  structured `AttachedFile` list the flat `attachment:N:*` metadata view
  collapses — every entry carries the 1-based `index` (matching the flat
  metadata keys and `tag:attachment:N:<name>` Tag scopes), `filename`
  (§5.1.6.2), `mime_type` (§5.1.6.3), `description` (§5.1.6.1), `uid`
  (§5.1.6.5), and the on-disk byte range (`data_offset` + `data_size`) of
  the `FileData` payload. The payload itself stays on disk until
  `MkvDemuxer::attachment_data(index)` is called — exactly `data_size`
  bytes are read from `data_offset` and the demuxer's reader position is
  restored across the fetch, so calling it between `next_packet` calls
  (or while mid-cluster) is safe. The flat `metadata()` view also gains
  an `attachment:N:description` key when the source element was present.
  Adds 5 integration tests (typed list fields, on-demand payload read with
  reader-position preservation, invalid-index rejection, FileDescription
  round-trip, empty-attachments case) and one new public type
  `demux::Attachment`.
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
