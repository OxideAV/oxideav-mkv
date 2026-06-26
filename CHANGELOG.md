# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Demuxer: reclaimed DivX trick-track / old-lacing `BlockGroup` children
  (RFC 9559 Appendix A.3..A.14) on `BlockGroupMeta`, surfaced for a faithful
  re-mux. `block_group_meta()` now also exposes `block_virtual()`
  (`BlockVirtual`, A.3 binary), `reference_virtual()` (`ReferenceVirtual`, A.4
  integer — Segment Position of a virtual Block's data), `slices()` (every
  `Slices > TimeSlice` master, A.5..A.11, each a new `TimeSlice` folding
  `LaceNumber` / `FrameNumber` / `BlockAdditionID` / `Delay` / `SliceDuration`),
  and `reference_frame()` (`ReferenceFrame`, A.12..A.14 — `ReferenceOffset` +
  `ReferenceTimestamp` for a Smooth FF/RW trick track). Every field is a pure
  on-disk projection (`None`/empty = absent, present `0` = `Some(0)`); none is
  interpreted by the container. New public types `TimeSlice` + `ReferenceFrame`.

- Demuxer + Muxer: EBML-header `DocTypeExtension` surface (RFC 8794 §11.2,
  including §11.2.9..§11.2.11). `MkvDemuxer::ebml_header() -> &EbmlHeader`
  surfaces the full parsed header — the `EBMLVersion` / `EBMLReadVersion` /
  `EBMLMaxIDLength` / `EBMLMaxSizeLength` quartet (§11.2.2..§11.2.5, spec
  defaults `1` / `1` / `4` / `8` materialised when absent), `doc_type`, the
  `doc_type_version` /
  `doc_type_read_version` pair (spec default `1` materialised when absent), and
  every well-formed `DocTypeExtension` (name + version) in document order; a
  malformed extension missing either mandatory child is dropped at parse time.
  `MkvMuxer::set_doc_type_extensions(Vec<DocTypeExtension>)` writes the
  declarations into the EBML header, with queue-time validation for empty
  names, zero versions, duplicate names, and post-`write_header` use. A
  header→header copy round-trips every extension verbatim. New public types
  `EbmlHeader` + `DocTypeExtension`.

- Demuxer + Muxer: legacy `Video > OldStereoMode` (RFC 9559 §5.1.4.1.28.5, id
  `0x53B9`, `maxver 2`) — the "bogus" stereo-3D mode value libmatroska prior to
  0.9.0 wrote at the wrong Element ID (`0x53B9` instead of `0x53B8`, §18.10).
  `MkvDemuxer::video_old_stereo_mode(stream_index) -> Option<OldStereoMode>`
  (+ the `video_old_stereo_modes()` slice) surfaces it through a new typed enum
  (`Mono` / `RightEye` / `LeftEye` / `BothEyes` / `Other(u64)`) kept separate
  from the modern `StereoMode` because their value spaces (Table 7 vs Table 5)
  are incompatible; no spec default is materialised, so absence reads `None`.
  `MkvMuxer::set_video_old_stereo_mode(stream_index, OldStereoMode)` writes it
  inside the `Video` master as a legacy / re-mux-only surface (omitted by
  default, since a Writer MUST NOT emit it for new files). A mux→demux pipeline
  round-trips the value bit-exactly. This closes the **last** RFC 9559
  element-ID-registry entry the crate had not yet read or written — every
  registry element is now both decoded and writable.

- Muxer: Segment `Info` metadata write surface — `Title` (RFC 9559 §5.1.2.12)
  and `DateUTC` (§5.1.2.11). `MkvMuxer::set_title(impl Into<String>)` queues
  the Segment's general name; `set_date_utc_ns(i64)` queues the creation date
  as signed nanoseconds since the Matroska epoch (2001-01-01T00:00:00 UTC, the
  `date` element type), with `set_date_utc_unix_secs(i64)` rebasing a Unix
  timestamp onto that epoch (pre-2001 instants yield a negative, still-valid
  `DateUTC`). Both land in the `Info` master in §5.1.2 element order (after
  `TimestampScale`, before `MuxingApp`) and round-trip onto the demuxer's flat
  metadata view (`"title"` / `"date"`). The `date` writer fixes the on-disk
  width at 8 bytes (the only legal `date` length). All three setters reject
  post-`write_header` use; omitting them writes neither element.

- Muxer: Linked-Segment `Info` write surface (RFC 9559 §5.1.2.1..§5.1.2.8 +
  Section 17) — `MkvMuxer::set_segment_linking(SegmentLinking)` queues the
  `SegmentUUID` / `SegmentFilename` / `PrevUUID` / `PrevFilename` / `NextUUID`
  / `NextFilename` / `SegmentFamily`(s) / `ChapterTranslate`(s) children, and
  the muxer materialises them into the `Info` master in §5.1.2 element order
  (before `TimestampScale`). The mux-side mirror of the existing demux-side
  `MkvDemuxer::segment_linking()` accessor, so a demuxed `SegmentLinking`
  round-trips through the muxer byte-for-byte. The setter enforces the §5.1.2
  spec rules: the 128-bit UID elements are `length: 16` (§5.1.2.1 / .3 / .5 /
  .7 — an off-length UID is rejected); `PrevUUID` / `NextUUID` MUST NOT equal
  `SegmentUUID` (§5.1.2.3 / .5); a `SegmentFamily` is REQUIRED when a
  `ChapterTranslate` is present (§5.1.2.7 usage note); each `ChapterTranslate`
  carries a non-empty `ChapterTranslateID` (§5.1.2.8.1, `minOccurs: 1`). The
  read-only `segment_linking()` accessor exposes the queued record before the
  header is sealed. An all-default record writes nothing (standalone Segment).
- Demuxer: typed `TrackIdentity` accessor (RFC 9559 §5.1.4.1.18 / .19 / .20 /
  .23 / .4 / .5 / .12 / .24). `MkvDemuxer::track_identity(stream_index)` (and
  the per-stream `all_track_identity()` slice) folds eight `TrackEntry`-level
  identity / selection elements — `Name` (§5.1.4.1.18), `Language`
  (§5.1.4.1.19), `LanguageBCP47` (§5.1.4.1.20), `CodecName` (§5.1.4.1.23),
  `FlagEnabled` (§5.1.4.1.4), `FlagDefault` (§5.1.4.1.5), `FlagLacing`
  (§5.1.4.1.12), and `AttachmentLink` (§5.1.4.1.24) — into one record per
  track. The four strings carry no spec default and stay `Option`; the three
  selection flags carry the spec default `1`, materialised on
  `enabled()` / `default()` / `lacing_allowed()` while the `*_explicit`
  accessors preserve the on-disk presence; `AttachmentLink` is a "not 0"
  uinteger (a spec-illegal `0` is dropped). `language()` honours the
  §5.1.4.1.20 precedence — `LanguageBCP47` supersedes `Language` — while
  `language_matroska()` / `language_bcp47()` expose each raw form; `uses_bcp47()`
  reports the precedence. The effective language now also lifts onto the flat
  `StreamInfo` view with BCP-47 taking precedence. `is_default()` reports the
  all-absent state.
- Muxer: `TrackEntry` identity / selection writing (RFC 9559 §5.1.4.1.18 /
  .19 / .20 / .23 / .4 / .5 / .12 / .24). New
  `MkvMuxer::set_track_identity(stream_index, MkvTrackIdentity)` queues a
  per-track hint whose eight `Option` slots — `name` (`Name`), `codec_name`
  (`CodecName`), `language` (`Language`), `language_bcp47` (`LanguageBCP47`),
  `flag_enabled` (`FlagEnabled`), `flag_default` (`FlagDefault`),
  `flag_lacing` (`FlagLacing`), `attachment_link` (`AttachmentLink`) — land
  directly inside the `TrackEntry` at `write_header` time. Per-field omission
  rule: each `Some(v)` writes the element, each `None` stays off-disk. The
  `language` field overrides the `StreamInfo`-derived `Language`; the
  `flag_lacing` field overrides the auto-derived value. Per §5.1.4.1.20, when
  both `language` and `language_bcp47` are `Some` the muxer writes only
  `LanguageBCP47` (`Language` MUST be ignored when BCP-47 is present).
  Queue-time validation rejects an empty `Name` / `CodecName` / `Language` /
  `LanguageBCP47` string, `attachment_link == Some(0)` (§5.1.4.1.24 "not 0"),
  an out-of-range `stream_index`, and any call after `write_header`.
  Convenience constructors `MkvTrackIdentity::named` / `::language_bcp47` /
  `::non_default`, plus a read-back `MkvMuxer::track_identity` accessor. Pairs
  symmetrically with the demux-side `MkvDemuxer::track_identity` — a mux→demux
  pipeline round-trips every element.
- Muxer: `Tags` writing (RFC 9559 §5.1.8). New `MkvMuxer::add_tag(MkvTag)`
  queues metadata descriptors emitted as the file's single `Tags` master
  before the first `Cluster`, symmetric with the long-standing demux-side
  `tags()` read surface. `MkvTag` pairs an `MkvTagTargets` scope
  (`TargetTypeValue` §5.1.8.1.1.1, `TargetType` §5.1.8.1.1.2, and the four
  `TagTrackUID` / `TagEditionUID` / `TagChapterUID` / `TagAttachmentUID`
  lists §5.1.8.1.1.3..§5.1.8.1.1.6 — multi-UID scoping supported) with one
  or more `MkvSimpleTag` `(name, value)` descriptors carrying `TagName`
  (§5.1.8.1.2.1), `TagLanguage` (§5.1.8.1.2.2, default `und` omitted),
  `TagLanguageBCP47` (§5.1.8.1.2.3, wins over `TagLanguage`), `TagDefault`
  (§5.1.8.1.2.4, default `1` written only when cleared), and a
  `TagString` (§5.1.8.1.2.5) / `TagBinary` (§5.1.8.1.2.6) payload enum.
  `MkvSimpleTag` supports the spec's `recursive: True` nesting via a
  `children` list. Convenience constructors `MkvTag::global`,
  `MkvTagTargets::track`, `MkvSimpleTag::new` / `::binary`. Queue-time
  validation rejects an empty `simple_tags` list (§5.1.8.1.2 `minOccurs:
  1`), an empty `TagName` at any nesting depth (§5.1.8.1.2.1), a
  `TargetTypeValue == 0` (`range: not 0`), and any call after
  `write_header`. The `Tags` master carries a leading `CRC-32` child
  (§6.2) and is reachable from a new `SeekHead` `Tags` slot (voided when
  no tags are queued). Covered by `tests/mux_tags.rs` (16 round-trip
  cases through the demuxer's flat `metadata()` and typed `tags()` views).
- Demuxer: nested `SimpleTag` parsing (RFC 9559 §5.1.8.1.2 `recursive:
  True`). `SimpleTag` gains a `children: Vec<SimpleTag>` field; the
  `tags()` accessor now surfaces child `SimpleTag`s instead of silently
  dropping them, so a hierarchical tag (e.g. a `TITLE` carrying a
  `SORT_WITH` sub-tag, or a name-only `ARTISTS` parent with `ARTIST`
  leaves) round-trips through a mux→demux pipeline. Parsed up to a fixed
  16-level depth cap; name-less children are dropped per the
  §5.1.8.1.2.1 `minOccurs: 1` rule. Nested descriptors stay out of the
  flat `metadata()` view (which only ever surfaced top-level
  descriptors). Two `tests/mux_tags.rs` cases cover the round-trip.
- Tests: two `tests/injection_robustness.rs` cases for the nested-tag
  parser — a 4000-level-deep nested `SimpleTag` chain that must parse
  without a stack overflow (proving the §5.1.8.1.2 `recursive` depth
  cap), and a name-less nested `SimpleTag` that must be dropped
  (§5.1.8.1.2.1 `minOccurs: 1`) while its named parent survives.
- Demuxer: `CueBlockNumber` seek fallback (RFC 9559 §5.1.5.1.2.5). When a
  Cues entry carries a `CueBlockNumber` ("Number of the Block in the
  specified Cluster") but no `CueRelativePosition` (§5.1.5.1.2.3) — common
  for files indexed by older tools — `seek_to` now walks the Cluster body
  counting `SimpleBlock` / `BlockGroup` elements and lands the reader on
  the exact 1-based n-th Block instead of falling back to a cluster-start
  scan. Out-of-range / malformed block numbers degrade gracefully to the
  cluster-start walk (no panic). The flat seek index and the typed
  `cue_points()` view both carry `CueBlockNumber` now. Covered by
  `tests/cue_block_number_seek.rs`.
- Muxer: per-frame subtitle cue emission (RFC 9559 §22.1 — "each subtitle
  frame SHOULD be referenced by a CuePoint element with a CueDuration
  element"). A `MediaType::Subtitle` track is now indexed once per *frame*
  rather than once per *cluster*: every subtitle Block gets its own
  `CuePoint` carrying the frame's `CueDuration` (§5.1.5.1.2.4) and a
  distinct `CueBlockNumber` (§5.1.5.1.2.5). Audio/video indexing is
  unchanged (one cue per cluster — §22.1's "at most once every 500 ms"
  guidance for audio, keyframes for video). Covered by
  `tests/mux_subtitle_cues.rs`, including a regression guard that audio is
  still indexed once per cluster.
- Muxer: complete the `Cues` write surface to match the demuxer's read
  surface. Every `CueTrackPositions` the muxer emits now carries
  `CueBlockNumber` (RFC 9559 §5.1.5.1.2.5) — the 1-based ordinal of the
  indexed Block within its Cluster, counting every `SimpleBlock` /
  `BlockGroup` across all tracks in write order, so a cue on a Block that
  is not first in its Cluster is now expressible (`range: not 0` honoured —
  the first Block is number 1). When the indexed packet carried a usable
  duration, `CueDuration` (§5.1.5.1.2.4, in Segment Ticks) is written too;
  §22.1 specifically recommends it for subtitle cues. Both values
  round-trip through the typed `demux::MkvDemuxer::cue_points()` view
  (`tests/mux_cue_block_number.rs`). Previously the muxer wrote only
  `CueTime` / `CueTrack` / `CueClusterPosition` / `CueRelativePosition`.
- Property-style test coverage for the EBML element walker (RFC 8794) in
  `tests/ebml_walker_property.rs` — a seeded splitmix64 PRNG drives ~100k
  generated cases per run (no `proptest`/`quickcheck` dependency, keeping
  the clean-room surface minimal). Pins: VINT `write_vint`↔`read_vint`
  round-trip with `min_width` honoured across the full 56-bit range; the
  unknown-size sentinel at every width (1..8); `read_element_header`
  round-trip with `header_len` accounting; a sequential `read_element_header`
  + `skip` walk recovering every element id and landing exactly on the
  buffer end; and no-panic / no-backward-seek on arbitrary byte streams and
  every prefix of a well-formed tree. Complements the existing
  demux-only libFuzzer target in `fuzz/`.
- Muxer: complete `ChapterAtom` write surface (RFC 9559 §5.1.7.1.4). The
  `mux::MkvChapter` struct now carries the atom fields the writer previously
  dropped — `uid` (§5.1.7.1.4.1, auto-derived non-zero when `None`),
  `string_uid` (§5.1.7.1.4.2), `hidden` (§5.1.7.1.4.5),
  `enabled` (§5.1.7.1.4, default `1` materialised, written only when
  cleared), `segment_uuid` (§5.1.7.1.4.6, 16-byte), `segment_edition_uid`
  (§5.1.7.1.4.7), `physical_equiv` (§5.1.7.1.4.8) — plus the
  `ChapProcess > ChapProcessCommand` chapter-codec command tree
  (§5.1.7.1.4.14..§5.1.7.1.4.19) via new `mux::MkvChapProcess` /
  `mux::MkvChapProcessCommand`, the write-side mirror of the existing
  `demux::ChapProcess` / `demux::ChapProcessCommand` read surface. The full
  atom + process tree round-trips through the typed `MkvDemuxer::chapters()`.
  `add_chapter_full` rejects `ChapterUID 0` / `ChapterSegmentEditionUID 0`
  (range "not 0") and a non-16-byte `ChapterSegmentUUID`. New
  `tests/mux_chapter_process.rs` (5 cases). `MkvChapter` gained a manual
  `Default` (so `..Default::default()` works while keeping `enabled = true`).
- `SilentTracks` (RFC 9559 Appendix A.1 / A.2, ids `0x5854` / `0x58D7`),
  read **and** write. Demuxer: a new `ClusterRecord::silent_track_numbers`
  field surfaces the per-Cluster list of `SilentTrackNumber` values (the
  track numbers "not used in that part of the stream") in on-disk order,
  empty for the common case of a Cluster without the element. Muxer:
  `MkvMuxer::set_next_cluster_silent_tracks(&[u64])` queues the list for the
  next Cluster the muxer opens, draining it after one Cluster to match the
  element's per-Cluster scope (A.2: a track silent here MAY be active again
  later); `MkvMuxer::track_number(stream_index)` maps a stream index to the
  assigned on-wire `TrackNumber` so callers can build the list. The element
  is deprecated (`maxver: 0`) but emitted by historical Writers, so both
  paths exist for faithful inspection / re-mux. Covered by
  `tests/silent_tracks.rs` (4 round-trip cases). `ClusterRecord` is no longer
  `Copy` (it now owns a `Vec`); it stays `Clone`.
- BlockGroup meta surface (RFC 9559 §5.1.3.5.4..§5.1.3.5.7), read **and**
  write. Demuxer: `MkvDemuxer::block_group_meta() -> Option<&demux::BlockGroupMeta>`
  folds the four non-`Block`, non-`BlockAdditions` `BlockGroup` children the
  `Packet` type has no slot for — `ReferenceBlock` (§5.1.3.5.5, every value, in
  on-disk order; was previously read only to flip the keyframe flag and then
  discarded), `ReferencePriority` (§5.1.3.5.4, spec default `0` materialised),
  `CodecState` (§5.1.3.5.6, verbatim codec-private bytes), `DiscardPadding`
  (§5.1.3.5.7, signed Matroska Ticks). Same call discipline as
  `block_additions()` (read after `next_packet()`, invalidated by `seek_to`).
  Muxer: `MkvMuxer::write_packet_with_block_group(&Packet, &mux::BlockGroupOptions)`
  writes the full group child set in §5.1.3.5 order, deriving a single
  `ReferenceBlock` from the previous same-track Block when the caller leaves the
  list empty for a non-keyframe (the prior `write_packet_with_additions`
  behaviour) and writing the caller's explicit references verbatim otherwise.
  An empty `BlockAdditions` no longer emits a malformed empty master, and a
  group whose only child is e.g. `DiscardPadding` needs no `MaxBlockAdditionID`
  declaration. New `ids::{REFERENCE_PRIORITY, CODEC_STATE, DISCARD_PADDING,
  SILENT_TRACKS, SILENT_TRACK_NUMBER}`. Covered by `tests/mux_block_group.rs`
  (5 round-trip cases).
- Demuxer: typed `SeekHead` accessor (RFC 9559 §5.1.1, including
  §5.1.1.1..§5.1.1.1.2). `MkvDemuxer::seek_entries() -> &[demux::SeekEntry]`
  surfaces the MetaSeek index — the `SeekHead > Seek` rows that point each
  Top-Level Element to its Segment Position — in document order. Each
  `SeekEntry` pairs a `SeekID` (§5.1.1.1.1, the 4-byte binary EBML ID of the
  referenced element, decoded via `seek_id() -> Option<u32>` and preserved
  verbatim via `seek_id_bytes()`) with a `SeekPosition` (§5.1.1.1.2, a Segment
  Position per Section 16, via `seek_position()` + `has_position()`). The
  demuxer doesn't navigate by the SeekHead — it walks Segment children directly
  and seeks via `Cues` — so this is a pure inspection / re-mux surface, the one
  Top-Level master that was CRC-validated but never read back. A malformed
  `Seek` missing its mandatory `SeekPosition` is surfaced for inspection
  (`seek_position() == 0`, `has_position() == false`) rather than dropped;
  a `SeekID` referencing an unrecognised element round-trips verbatim; the §6.3
  two-`SeekHead` layout (`maxOccurs: 2`) accumulates both SeekHeads' entries
  onto one slice. The in-tree muxer's emitted SeekHead (Info / Tracks / Cues)
  reads back through this accessor with every `SeekPosition + segment_data_start`
  landing on the matching on-disk element header. Returns an empty slice when
  the file carries no `SeekHead`.

- Demuxer + Muxer: `TrackTranslate` (RFC 9559 §5.1.4.1.27, id `0x6624`) — the
  per-`TrackEntry` chapter-codec track-mapping master, the `TrackEntry`-level
  twin of `Info > ChapterTranslate`. The demuxer surfaces each mapping through
  `MkvDemuxer::track_translates(stream_index) -> &[demux::TrackTranslate]` (and
  the per-stream `all_track_translates()` slice) — `track_id`
  (`TrackTranslateTrackID`, binary, verbatim), `codec` (`TrackTranslateCodec`),
  and the unbounded `edition_uids` (`TrackTranslateEditionUID`) list, in on-disk
  order, empty for tracks with no mapping. The muxer writes the masters via
  `MkvMuxer::set_track_translates(stream_index, Vec<mux::MkvTrackTranslate>)`
  (convenience constructor `MkvTrackTranslate::new(track_id, codec)`), enforcing
  the spec rules at queue time (mandatory non-empty `track_id`, "not 0" edition
  UIDs, pre-`write_header` only, in-range stream). A mux→demux pipeline
  round-trips every mapping field-for-field.

- Demuxer + Muxer: the reclaimed content-signing quartet inside
  `ContentEncryption` (RFC 9559 Appendix A.33..A.36 — `ContentSignature`
  `0x47E3`, `ContentSigKeyID` `0x47E4`, `ContentSigAlgo` `0x47E5`,
  `ContentSigHashAlgo` `0x47E6`). The `demux::ContentEncodingTransform::Encryption`
  variant gains a `signing: ContentSigning` field surfacing the four elements
  verbatim — `signature` / `key_id` as `Option<Vec<u8>>`, `algo` / `hash_algo`
  as `Option<u64>` — where `None` means the element was absent on disk (the
  appendix defines no values and no defaults, so a present `0` round-trips as
  `Some(0)` distinct from absence). `ContentSigning::is_empty()` reports the
  all-absent state. The muxer writes each child only when its `Option` slot is
  `Some`, so an empty `ContentSigning` adds no bytes and round-trips to `None`
  on every field. The container is a pure carrier: it never computes or
  verifies a signature.
- Muxer: `ContentEncodings` (RFC 9559 §5.1.4.1.31) write path via
  `MkvMuxer::set_track_content_encodings(stream_index, ContentEncodings)`.
  Serialises the per-track compression / encryption chain
  (`ContentEncoding` → `ContentEncodingOrder` / `ContentEncodingScope` /
  `ContentEncodingType` + `ContentCompression` / `ContentEncryption`
  sub-masters) as a `ContentEncodings` master inside the `TrackEntry`,
  taking the same `demux::ContentEncodings` record the demuxer produces
  for a byte-exact round-trip. Adds `to_raw()` inverses on the
  `demux::ContentCompAlgo` / `ContentEncAlgo` / `AesCipherMode` enums
  (Table 23 / 24 / 26, including each `Other(u64)` forward-compat value).
  Queue-time validation enforces order uniqueness (§5.1.4.1.31.2), non-zero
  scope (§5.1.4.1.31.3), the AES-only / non-zero `AESSettingsCipherMode`
  rules (Table 25 / §5.1.4.1.31.12), a non-empty chain, in-range stream
  index, and pre-`write_header` use. The container carries the declared
  chain but does not compress or encrypt the frame bytes. Pairs
  symmetrically with `MkvDemuxer::content_encodings`.
- Demuxer: Linked-Segment `Info` metadata (RFC 9559 §5.1.2.1..§5.1.2.8 +
  Section 17), exposed via `MkvDemuxer::segment_linking()` returning a typed
  `demux::SegmentLinking`. Parses `SegmentUUID`, `SegmentFilename`,
  `PrevUUID`/`PrevFilename`, `NextUUID`/`NextFilename`, the unbounded
  `SegmentFamily` UID list, and the `ChapterTranslate` sub-tree
  (`ChapterTranslateID` / `ChapterTranslateCodec` / `ChapterTranslateEditionUID`)
  as `demux::ChapterTranslate`. UID binaries are kept verbatim (off-length
  values round-trip for inspection); `is_empty()` / `is_hard_linked()`
  convenience predicates added.

## [0.0.9](https://github.com/OxideAV/oxideav-mkv/compare/v0.0.8...v0.0.9) - 2026-06-15

### Other

- typed Cues accessor (RFC 9559 §5.1.5.1)
- scrub decorative external-implementation references from comments
- demux TrackCodecTiming — CodecDelay / SeekPreRoll typed accessor (RFC 9559 §5.1.4.1.25..§5.1.4.1.26)
- TrackEntry timing trio — DefaultDuration / DefaultDecodedFieldDuration / TrackTimestampScale (RFC 9559 §5.1.4.1.13..§5.1.4.1.15)
- scrub pre-existing decorative impl-attribution from codec_id test comment
- write-side Video > AspectRatioType (RFC 9559 Appendix A.24)
- write-side Audio master children (RFC 9559 §5.1.4.1.29)
- Per-Block BlockAdditions typed views, read + write (RFC 9559 §5.1.3.5.2) + MaxBlockAdditionID (§5.1.4.1.16)
- write-side TrackEntry audience flags (RFC 9559 §5.1.4.1.6..§5.1.4.1.11)
- write-side Video > Projection master (RFC 9559 §5.1.4.1.28.41)
- typed TrackAudio accessor (RFC 9559 §5.1.4.1.29.1..§5.1.4.1.29.4)
- typed TrackAudienceFlags accessor (RFC 9559 §5.1.4.1.6..§5.1.4.1.11)
- typed per-Cluster Position / PrevSize records (RFC 9559 §5.1.3.2 / §5.1.3.3)
- typed Targets::target_level() hierarchy resolver (RFC 9559 §5.1.8.1.1.1)
- drop release-plz.toml — use release-plz defaults across the workspace
- write Colour > MasteringMetadata sub-master (RFC 9559 §5.1.4.1.28.30..§5.1.4.1.28.40)
- typed BlockAdditionMapping decode (RFC 9559 §5.1.4.1.17)
- write Video > Colour scalar children (RFC 9559 §5.1.4.1.28.16, §5.1.4.1.28.17..§5.1.4.1.28.29)
- write Video > UncompressedFourCC (RFC 9559 §5.1.4.1.28.15)
- write Video > PixelCrop quartet + Display{Width,Height,Unit} (RFC 9559 §5.1.4.1.28.8..§5.1.4.1.28.14)
- write Video > StereoMode + AlphaMode (RFC 9559 §5.1.4.1.28.3 + §5.1.4.1.28.4)
- write Video > FlagInterlaced + FieldOrder (RFC 9559 §5.1.4.1.28.1 + §5.1.4.1.28.2)
- write CRC-32 on every buffered Top-Level master (RFC 9559 §6.2)
- write-side Attachments (RFC 9559 §5.1.6)
- emit per-track Language element from CodecParameters::language
- add Annex-B → AVCC repack helper for V_MPEG4/ISO/AVC passthrough

### Other

- mux: **`TrackOperation` on write** (RFC 9559 §5.1.4.1.30). New `MkvMuxer::set_track_operation(stream_index, MkvTrackOperation)` queues a per-track virtual-track recipe that lands as a `TrackOperation` master (id `0xE2`) directly inside the carrying `TrackEntry` (sibling to `Video` / `Audio`) at `write_header` time. `MkvTrackOperation` carries a `combine_planes: Vec<MkvTrackPlane>` (`TrackCombinePlanes`, §5.1.4.1.30.1 — each `MkvTrackPlane` pairs a 0-indexed source `stream_index` with a `TrackPlaneType`, §5.1.4.1.30.4) and a `join_tracks: Vec<usize>` (`TrackJoinBlocks`, §5.1.4.1.30.5). Each plane / join reference's stream index is resolved to the source track's on-disk `TrackUID` at write time — the symmetric inverse of the demux side, which resolves each `TrackPlaneUID` (§5.1.4.1.30.3) / `TrackJoinUID` (§5.1.4.1.30.6) back to a stream index. The demux-side `TrackPlaneType` enum gained a `to_raw()` inverse so every Table 20 value (`LeftEye` / `RightEye` / `Background`) round-trips, including the `Other(u64)` forward-compat variant (§27.17 leaves the "Matroska Track Plane Types" registry open). Both operation kinds may coexist on one track. Convenience constructors `MkvTrackOperation::stereo_3d(left, right)` (the canonical left/right-eye 3D recipe) and `MkvTrackOperation::join(streams)` cover the two common shapes. Spec rules enforced at queue time: rejects post-`write_header` use, out-of-range `stream_index`, an empty operation (`TrackCombinePlanes` / `TrackJoinBlocks` exist only to carry references), and any plane / join reference pointing at a non-existent stream (the `TrackPlaneUID` / `TrackJoinUID` "not 0" pins, §5.1.4.1.30.3 / §5.1.4.1.30.6, fall out of the stream-index→`TrackUID` mapping). Unlike the `set_video_*` family there is no track-type restriction — the spec carries `TrackOperation` on every `TrackEntry`, so a `TrackJoinBlocks` audio virtual track is accepted. Omitting the call keeps the master off-disk so the demuxer surfaces `None` from `track_operation`. The muxer pins `DocTypeVersion` to `4`, comfortably above §5.1.4.1.30's `minver: 3`. Pairs symmetrically with the existing `MkvDemuxer::track_operation` typed accessor — a mux→demux pipeline preserves every plane (with its type) and every join reference. Covered by `tests/mux_track_operation.rs` (14 cases: combine-planes 3D, background + `Other` plane type, join-blocks, coexisting combine+join, omitted-call `None`, on-disk-id presence, all five queue-time rejections, accessor read-back, last-write-wins, non-video acceptance).

- demux: **typed `Cues` accessor** (RFC 9559 §5.1.5.1, including §5.1.5.1.1..§5.1.5.1.2.8 and the reclaimed Appendix A.37..A.39 `CueReference` children). New `MkvDemuxer::cue_points() -> &[CuePoint]` surfaces the full on-disk seek-index tree in document order — the structure the denormalised `seek_to` index (track / time / cluster offset / relative position) collapses. Each `CuePoint` pairs `CueTime` (id `0xB3`, §5.1.5.1.1 — absolute timestamp in Segment Ticks, not microseconds) with one or more `CueTrackPositions` (id `0xB7`, §5.1.5.1.2 — `minOccurs: 1`, no `maxOccurs`, so a single timestamp can index blocks on several tracks). The new typed `CueTrackPositions` record exposes `track` (`CueTrack`, id `0xF7`, §5.1.5.1.2.1), `cluster_position` (`CueClusterPosition`, id `0xF1`, §5.1.5.1.2.2, `Option<u64>`), `relative_position` (`CueRelativePosition`, id `0xF0`, §5.1.5.1.2.3, `Option<u64>`), `duration` (`CueDuration`, id `0xB2`, §5.1.5.1.2.4, `Option<u64>`), `block_number` (`CueBlockNumber`, id `0x5378`, §5.1.5.1.2.5, `Option<u64>`), `codec_state` (`CueCodecState`, id `0xEA`, §5.1.5.1.2.6, `u64` with spec default `0` materialised — `0` meaning "taken from the initial `TrackEntry`"), and `references` (`CueReference`, id `0xDB`, §5.1.5.1.2.7, `Vec<CueReference>`). The new typed `CueReference` record exposes `ref_time` (`CueRefTime`, id `0x96`, §5.1.5.1.2.8) plus the reclaimed-appendix `ref_cluster` (`CueRefCluster`, id `0x97`, A.37), `ref_number` (`CueRefNumber`, id `0x535F`, A.38), and `ref_codec_state` (`CueRefCodecState`, id `0xEB`, A.39), each `Option<u64>` (the reclaimed appendix lists no defaults). The index is populated whether the `Cues` element sits before the first Cluster or after the last (the late best-effort `scan_cues_from` rescan feeds the same typed collector); unknown children inside `CueTrackPositions` are skipped for forward-compat; the denormalised seek path is unchanged. New `ids::CUE_CODEC_STATE` / `CUE_REFERENCE` / `CUE_REF_TIME` / `CUE_REF_CLUSTER` / `CUE_REF_NUMBER` / `CUE_REF_CODEC_STATE` constants plumb the element ids through. Pinned by 12 new `tests/cue_points.rs` cases: the no-Cues empty-slice surface, the spec-minimum mandatory-pair-only CuePoint (every optional child at absence / default), an all-sub-elements round-trip (every documented child including a full `CueReference`), a minimal `CueReference` (only mandatory `CueRefTime`, reclaimed children `None`), multiple `CueReference` rows preserved in document order, multiple `CueTrackPositions` per CuePoint indexing two tracks at one timestamp, multiple CuePoints in document order, the index-at-end (`scan_cues_from`) layout carrying `CueDuration` / `CueBlockNumber`, unknown-child skip inside `CueTrackPositions`, the explicit-`0` `CueCodecState` default case, the typed-view-stable-across-packet-walk contract, and the `CuePoint::default()` / `CueTrackPositions::default()` shape.
- demux: **`TrackEntry` codec-timing pair** (RFC 9559 §5.1.4.1.25 + §5.1.4.1.26 — `CodecDelay` id `0x56AA`, `SeekPreRoll` id `0x56BB`). New typed accessor `MkvDemuxer::track_codec_timing(stream_index) -> Option<&TrackCodecTiming>` (plus the per-stream `all_track_codec_timing()` slice) folds the two elements — which sit directly on `TrackEntry`, not in a gating master — into one record per track, so every valid track surfaces a record and the accessor returns `None` only for an out-of-range index. Both are nanosecond (Matroska Tick) `uinteger`s: `codec_delay()` is the encoder's built-in delay (Opus pre-skip) the player MUST subtract from each frame timestamp, `seek_pre_roll()` is the audio the decoder MUST decode after a seek before its output is valid (Opus convention 80 ms). Unlike the §5.1.4.1.13/.14 durations, both carry spec default `0` and **no** "not 0" range, so an explicit on-disk `0` is a legal value distinct from "absent": the plain accessors materialise the `0` default while `codec_delay_explicit()` / `seek_pre_roll_explicit()` preserve the on-disk presence so a re-muxer can avoid emitting an element the source omitted. `TrackCodecTiming::is_empty()` reports the both-absent state — a track that emitted an explicit `0` for either element is *not* empty. The mux side already wrote both on the Opus path; this lands the symmetric demux-side read. New coverage in `tests/track_codec_timing.rs` (6 demux-side tests).
- demux + mux: **`TrackEntry` timing trio** (RFC 9559 §5.1.4.1.13..§5.1.4.1.15 — `DefaultDuration` id `0x23E383`, `DefaultDecodedFieldDuration` id `0x234E7A`, `TrackTimestampScale` id `0x23314F`). New typed accessor `MkvDemuxer::track_timing(stream_index) -> Option<&TrackTiming>` (plus the per-stream `all_track_timing()` slice) folds the three elements — which sit directly on `TrackEntry`, not in a gating master — into one record per track. `DefaultDuration` is the container's nominal nanoseconds-per-frame source; `TrackTiming::nominal_frame_rate()` derives fps (`1e9 / ns`). Both nanosecond durations carry a "not 0" range and no spec default, so they stay `Option<u64>` and a spec-illegal explicit `0` is dropped at parse time. `TrackTimestampScale` is a `float` with default `1.0` and range `> 0x0p+0`; `track_timestamp_scale()` materialises the default while `track_timestamp_scale_explicit()` preserves the on-disk presence (a non-finite / non-positive payload is dropped). `TrackTiming::is_empty()` reports the all-absent state. The mux side gains the symmetric `MkvMuxer::set_track_timing(stream_index, MkvTrackTiming)` builder with per-field omission (each `Some(v)` written explicitly, each `None` off-disk), spec-range checks at queue time (`0` durations and `<= 0`/non-finite scale rejected), no track-type restriction (the elements sit on `TrackEntry`), last-write-wins, a pre-`write_header` read-back accessor, and a `MkvTrackTiming::from_frame_rate(fps)` convenience constructor (rounds `1e9/fps` to ns, rejects non-finite/non-positive). A mux→demux pipeline preserves every supplied child bit-exactly. New coverage in `tests/track_timing.rs` (9 demux-side tests) and `tests/mux_track_timing.rs` (11 round-trip + rejection tests).
- mux: **write-side `Video > AspectRatioType`** (RFC 9559 Appendix A.24, reclaimed, id `0x54B3`) via the new `MkvMuxer::set_video_aspect_ratio_type(stream_index, u64)` builder method. The demux side already read this reclaimed appendix element (surfaced through `MkvDemuxer::video_aspect_ratio_type` as a raw `Option<u64>`); the new setter completes the round-trip. The element is a `uinteger` whose appendix documents it only as "Specifies the possible modifications to the aspect ratio" and enumerates no values and no default, so the setter takes the raw `u64` verbatim and the mux side never synthesises an enum — mirroring the demux-side surface. Per-element omission rule: the element is written into the track's `Video` master at `write_header` time only when the caller opts in; an explicit `0` is written and round-trips as `Some(0)`, distinct from absence (`None`), since the appendix defines no default to materialise. Spec rules enforced at queue time: the setter rejects post-`write_header` use, out-of-range `stream_index`, and calls on non-video tracks (only `Video` tracks carry a `Video` master). Read-back accessor `MkvMuxer::video_aspect_ratio_type` surfaces the queued hint pre-`write_header`; repeated calls are last-write-wins. This closes the last `Video` sub-element that the demux side read but the mux side could not write — the `Video` sub-element set is now fully symmetric across demux and mux. New round-trip + rejection coverage in `tests/mux_video_aspect_ratio_type.rs` (11 tests).

- mux: **write-side `Audio` master children** (RFC 9559 §5.1.4.1.29, including §5.1.4.1.29.1..§5.1.4.1.29.4) via the new `MkvMuxer::set_track_audio(stream_index, MkvTrackAudio)` builder method and the new `MkvTrackAudio` payload struct (mux module). Before this round the muxer derived the per-track `Audio` master (id `0xE1`) solely from `StreamInfo` — `sample_rate` → `SamplingFrequency` (id `0xB5`, §5.1.4.1.29.1), `channels` → `Channels` (id `0x9F`, §5.1.4.1.29.3), sample-format bit width → `BitDepth` (id `0x6264`, §5.1.4.1.29.4) — and had **no** way to emit `OutputSamplingFrequency` (id `0x78B5`, §5.1.4.1.29.2), the Spectral Band Replication (SBR) output rate that the demux-side `MkvDemuxer::track_audio` / `TrackAudio::is_sbr()` accessor already read back. The new setter closes that asymmetry: each of the four `Option` fields, when `Some(v)`, overrides the `StreamInfo`-derived child; when `None`, defers to the `StreamInfo` value (and for `output_sampling_frequency`, simply omits the element since `StreamInfo` has no equivalent). Children that resolve to nothing stay off-disk so the demuxer materialises the §5.1.4.1.29.1 default `8000.0` / §5.1.4.1.29.3 default `1` (mono); `BitDepth` has no spec default so its absence surfaces as `None`. The convenience constructor `MkvTrackAudio::sbr(core_sampling_frequency)` produces the canonical HE-AAC pair (`core`, `2 * core`). Spec range checks enforced at queue time: `SamplingFrequency` / `OutputSamplingFrequency` are ranged `> 0x0p+0` (a `Some(v)` with `v <= 0.0` or non-finite is rejected), `Channels` and `BitDepth` are ranged `not 0` (a `Some(0)` is rejected). Track-type restriction mirrors the demux side, which returns `None` from `track_audio` for non-audio tracks: the setter rejects calls on non-`Audio` streams, plus post-`write_header` use and out-of-range `stream_index`; repeated calls are last-write-wins; the read-back `MkvMuxer::track_audio(stream_index)` accessor returns the queued hint pre-`write_header`. Pairs symmetrically with the existing demux-side `MkvDemuxer::track_audio` typed accessor — a mux→demux round-trip preserves every supplied child bit-exactly, including the `OutputSamplingFrequency` SBR signal. Pinned by 12 new `tests/mux_track_audio.rs` cases: the no-hint StreamInfo-derived round-trip (48 kHz / 2ch / 16-bit, no `OutputSamplingFrequency` on disk, `is_sbr()` false), explicit-hint override of all three StreamInfo children, the SBR round-trip (22050 → 44100, `is_sbr()` firing, channels/bit_depth deferred), the `sbr()` constructor, the `0x78B5` on-disk presence scan (present when set / absent both with no hint and with a hint that left it `None`), the bare-StreamInfo fallback to §5.1.4.1.29.1 / .3 spec defaults, last-write-wins, all four rejection contracts (post-`write_header` → `Error::Other`; out-of-range, non-audio, and each range violation → `Error::InvalidData`), and the pre-`write_header` accessor read-back.
- demux+mux: **per-Block `BlockAdditions` typed views** (RFC 9559 §5.1.3.5.2, including §5.1.3.5.2.1..§5.1.3.5.2.3) **+ `MaxBlockAdditionID`** (§5.1.4.1.16), read AND write. Read side: the `BlockGroup` walk now parses the `BlockAdditions` master (id `0x75A1`) it previously skipped — each `BlockMore` (id `0xA6`, §5.1.3.5.2.1) decodes into the new typed `BlockAddition` record pairing `block_add_id()` (`BlockAddID`, id `0xEE`, §5.1.3.5.2.3 — spec default `1` = codec-defined materialised on omission; `is_codec_defined()` convenience) with the verbatim `data()` bytes (`BlockAdditional`, id `0xA5`, §5.1.3.5.2.2 — "interpreted by the codec as it wishes", never parsed by the container). The new `MkvDemuxer::block_additions() -> &[BlockAddition]` accessor surfaces the additions attached to the most recently returned packet (the out-queue now carries an `Arc`-shared additions slot per packet): empty for `SimpleBlock` packets (the element only exists on `BlockGroup`), for `BlockGroup`s without the master, before the first `next_packet`, and after a `seek_to`; every frame de-laced from one laced Block shares the Block's additions (the spec attaches the master to the Block as a whole). Malformed `BlockMore`s are dropped: missing mandatory `BlockAdditional`, `BlockAddID == 0` (range "not 0"), duplicate `BlockAddID` (§5.1.3.5.2.3 uniqueness MUST — first occurrence kept). The TrackEntry walk also gains `MaxBlockAdditionID` (id `0x55EE`, §5.1.4.1.16), surfaced via `MkvDemuxer::max_block_addition_id(stream_index) -> Option<u64>` with the spec default `0` ("there is no BlockAdditions for this track") materialised on absence. This completes the side-channel story the existing typed surfaces pointed at: `AlphaMode::Present` (§5.1.4.1.28.4) names the `BlockAddID = 1` payload and `BlockAdditionMapping` (§5.1.4.1.17) describes ids `>= 2`, but the per-Block bytes themselves were unreachable until now. Write side: new `MkvMuxer::write_packet_with_additions(&packet, &[MkvBlockAddition])` emits the packet as a `BlockGroup` — `Block` (frame bytes, unlaced; a pending same-track lace is flushed first so Block order is preserved, and the cue index records the group exactly like the non-lacing fast path), `BlockAdditions` with one `BlockMore` per addition in slice order (`BlockAdditional` verbatim, `BlockAddID` written only when it differs from the §5.1.3.5.2.3 default `1`), `BlockDuration` (§5.1.3.5.3) when the packet carries a duration (a `SimpleBlock` could not have carried one — so the packet duration now round-trips), and `ReferenceBlock` (§5.1.3.5.5, new minimal-length signed-integer element writer per RFC 8794 §7.1) when the packet is not a keyframe — a plain `Block` has no KEY flag bit, keyframe-ness is the element's absence; the relative value points at the track's most recently written Block (newly tracked per stream), falling back to the spec-sanctioned `0` "necessary reference Block(s) is unknown". Prerequisite + validation (all before any byte is written): `MkvMuxer::set_max_block_addition_id(stream_index, max)` must declare a non-zero maximum before `write_header` (lands as the `MaxBlockAdditionID` TrackEntry element; explicit `0` writes byte-distinctly but decodes like absence); `write_packet_with_additions` rejects an undeclared stream (§5.1.4.1.16 default `0` = "no BlockAdditions"), `BlockAddID == 0`, ids above the declared maximum, and duplicate ids within one call. An empty additions slice degrades to plain `write_packet` (`BlockMore` is mandatory inside the master). `MkvBlockAddition::codec_defined(data)` covers the `BlockAddID = 1` shape (e.g. WebM alpha — pair with `set_video_alpha_mode`). Pinned by 11 new cases: 5 in `tests/block_additions.rs` (hand-built bytes — disk-order surfacing with the default-id materialisation, SimpleBlock/plain-group empty surface without leak-forward, all three malformed-`BlockMore` drops, Xiph-laced frames sharing one Block's additions, `MaxBlockAdditionID` explicit/absent/out-of-range) and 6 in `tests/mux_block_additions.rs` (full mux→demux round-trip incl. duration + on-disk id scans for `0x75A1`/`0x55EE`, keyframe-vs-`ReferenceBlock` round-trip both ways, empty-slice degradation with no master on disk, every rejection contract, lace-flush ordering end-to-end, explicit-zero declaration on disk but still gating writes).
- mux: **write-side TrackEntry audience flags** (RFC 9559 §5.1.4.1.6..§5.1.4.1.11) via the new `MkvMuxer::set_track_audience_flags(stream_index, MkvTrackAudienceFlags)` builder method and the new `MkvTrackAudienceFlags` payload struct (mux module). Emits the six TrackEntry-level uinteger elements — `FlagForced` (id `0x55AA`, §5.1.4.1.6), `FlagHearingImpaired` (id `0x55AB`, §5.1.4.1.7), `FlagVisualImpaired` (id `0x55AC`, §5.1.4.1.8), `FlagTextDescriptions` (id `0x55AD`, §5.1.4.1.9), `FlagOriginal` (id `0x55AE`, §5.1.4.1.10), `FlagCommentary` (id `0x55AF`, §5.1.4.1.11) — directly inside the `TrackEntry` (the spec puts them on `TrackEntry` itself, not in a sub-master) at `write_header` time, after `FlagLacing`, in numerical-id order. Every payload slot is `Option<bool>` with a uniform omission rule (`Some(v)` writes the element explicitly as `0`/`1`; `None` keeps it off-disk) whose on-disk consequences split along the spec's asymmetric defaults: `FlagForced` carries the §5.1.4.1.6 default `0`, so `Some(false)` decodes identically to absence but is byte-distinct (the explicit producer-override path, matching the existing `StereoMode::Mono` explicit-write precedent); the five `minver: 4` flags carry no default, so `Some(false)` round-trips as `Some(false)` — distinct from `None` — preserving the §5.1.4.1.7..§5.1.4.1.11 "set to 1 *if and only if* …" explicit-zero signal end-to-end. Unlike the `set_video_*` family there is no track-type restriction: audio / video / subtitle tracks all accept the call, because the spec carries the elements on every `TrackEntry` (`FlagForced` with `minOccurs: 1`) and the demux-side accessor already surfaces a record for every track; §5.1.4.1.6's "applies only to subtitles" note describes player semantics, not an on-disk placement constraint. The muxer's existing `DocTypeVersion = 4` pin makes the `minver: 4` emissions version-consistent. Convenience constructors `MkvTrackAudienceFlags::forced_subtitle()` / `hearing_impaired_track()` / `visual_impaired_track()` / `commentary_track()` cover the common single-flag shapes; `is_empty()` reports the all-`None` no-op record (legal to queue — writes nothing). Spec rules enforced at queue time: rejects post-`write_header` use and out-of-range `stream_index`; repeated calls are last-write-wins; the read-back `MkvMuxer::track_audience_flags(stream_index)` accessor returns the queued record pre-`write_header`. Pairs symmetrically with the existing demux-side `MkvDemuxer::track_audience_flags` typed accessor — a mux→demux round-trip preserves every explicit flag, including the `Some(false)`-vs-absent distinction on the `minver: 4` five. Pinned by 13 new `tests/mux_track_audience_flags.rs` cases covering the forced-subtitle round-trip on a video+subtitle file (with on-disk id-byte scan for `0x55AA`), the explicit `FlagForced=0` still-writes-the-element contract, the omitted-call surface scanning all six ids absent + the demuxer materialising the §5.1.4.1.6 default `false` / five `None`s, all five `minver: 4` flags `Some(true)` together (ids `0x55AB`..`0x55AF` present, `0x55AA` absent, `is_accessibility()` firing), the `Some(false)`-distinct-from-`None` hearing-impaired case, audience flags accepted on both audio and video tracks, multi-track record independence, the all-`None` no-op record, last-write-wins overwrite, the pre-`write_header` accessor read-back, both rejection contracts (post-`write_header` → `Error::Other`, out-of-range index → `Error::InvalidData`), and the convenience-constructor shapes.
- mux: **write-side `Video > Projection` master** (RFC 9559 §5.1.4.1.28.41, including the §5.1.4.1.28.42..§5.1.4.1.28.46 sub-elements) via the new `MkvMuxer::set_video_projection(stream_index, MkvProjection)` builder method and the new `MkvProjection` payload struct (mux module). Emits a `Projection` master (id `0x7670`) inside the per-track `Video` master at `write_header` time, after the `Colour` master, carrying `ProjectionType` (id `0x7671`), `ProjectionPrivate` (id `0x7672`), and the `ProjectionPose{Yaw,Pitch,Roll}` triple (ids `0x7673` / `0x7674` / `0x7675`). New `ProjectionType::to_raw()` inverse method on the demux-side enum round-trips every Table 18 value (`Rectangular` / `Equirectangular` / `Cubemap` / `Mesh`) plus the `Other(u64)` forward-compat variant (§27.15 leaves the "Matroska Projection Types" registry open). Per-element omission rules implemented at write time: `ProjectionType` is written only for non-`Rectangular` types (the §5.1.4.1.28.42 default `0` stays off-disk so the demuxer materialises it); each `ProjectionPose*` child is written as an 8-byte big-endian `f64` only when non-zero (the §5.1.4.1.28.44..46 default `0.0` stays off-disk); `ProjectionPrivate` is written only when `private` is `Some(_)` and reaches disk verbatim (the muxer never interprets the ISOBMFF box body). Children are written in numerical-id order so the on-disk layout matches the order the demuxer walks them. As a result, queueing `MkvProjection::default()` writes an empty `Projection` master (present-but-childless) which the demuxer parses into `Some(Projection::default())` with every getter at its spec default (rectangular, zero pose, no private) — distinguishable on disk from the call-was-omitted case, which keeps the `Projection` master off-disk so the demuxer surfaces `None` from `video_projection`. Convenience constructors `MkvProjection::equirectangular(private)` (the common 360° VR shape carrying the verbatim `equi` box body) and `MkvProjection::rotated(roll_degrees)` (the §5.1.4.1.28.46 worked example — a flat rectangular track signalling a counter-clockwise roll) cover the two common shapes. Spec rules enforced at queue time: `set_video_projection` rejects calls made after `write_header`, out-of-range `stream_index`, and calls on non-video tracks. Pairs symmetrically with the existing demux-side `MkvDemuxer::video_projection` typed accessor — a mux→demux round-trip preserves the projection record (type, pose, and verbatim `ProjectionPrivate` payload) bit-exactly. This closes the `Projection` write-side gap previously called out in the `## What's NOT implemented` README section. Pinned by 12 new `tests/mux_video_projection.rs` cases covering the equirectangular + private + full yaw/pitch/roll pose round-trip, the cubemap + private round-trip with zero pose, the `ProjectionType::Other(99)` forward-compat passthrough, the §5.1.4.1.28.46 roll-only worked example, the omitted-call `None` surface, the empty-`Projection`-master `Some(default)` surface with an on-disk id-byte scan for `0x7670`, on-disk element-id presence/absence, every rejection contract (post-`write_header`, out-of-range stream index, audio-track), idempotent last-write-wins under repeated calls, and the muxer's own `video_projection(stream_index)` accessor returning the queued value pre-`write_header`.
- demux: **typed `TrackAudio` accessor** (RFC 9559 §5.1.4.1.29.1..§5.1.4.1.29.4). New `MkvDemuxer::track_audio(stream_index) -> Option<&TrackAudio>` (and the all-streams `all_track_audio() -> &[Option<TrackAudio>]`) folds the four `Audio` sub-master children — `SamplingFrequency` (id `0xB5`, §5.1.4.1.29.1), `OutputSamplingFrequency` (id `0x78B5`, §5.1.4.1.29.2), `Channels` (id `0x9F`, §5.1.4.1.29.3), `BitDepth` (id `0x6264`, §5.1.4.1.29.4) — into one typed record per audio track. Spec defaults are materialised asymmetrically: `TrackAudio::sampling_frequency() -> f64` always reflects the §5.1.4.1.29.1 default `0x1.f4p+12` = `8000.0` (an `Audio` master with no explicit child still surfaces 8000.0 Hz, never `0.0`); `TrackAudio::channels() -> u64` always reflects the §5.1.4.1.29.3 default `1` (mono); `TrackAudio::output_sampling_frequency() -> f64` folds Table 19's derived default (= `sampling_frequency()` when the element was absent) but `TrackAudio::output_sampling_frequency_explicit() -> Option<f64>` preserves the on-disk presence so a re-muxer doesn't materialise an element that wasn't in the source; `TrackAudio::bit_depth() -> Option<u64>` stays optional — §5.1.4.1.29.4 defines no default. Convenience predicate `TrackAudio::is_sbr()` returns `true` exactly when the writer emitted an explicit `OutputSamplingFrequency` strictly greater than `SamplingFrequency` (the canonical SBR-doubling signal for HE-AAC and similar tracks); equal-on-disk does NOT fire `is_sbr()`. Records surface only for `TrackEntry`s that carried an `Audio` master at all — video / subtitle / button tracks (where the master is `maxOccurs: 1` but carries no `minOccurs` at the `TrackEntry` level) return `None`, as does a malformed audio track that emitted no `Audio` child; the typed surface never synthesises a record from the §5.1.4.1.29.1 / .3 defaults alone. New `ids::OUTPUT_SAMPLING_FREQUENCY` reachability — `parse_audio` now reads `0x78B5` into the typed staging record alongside the existing `SamplingFrequency` / `Channels` / `BitDepth` flat-field writes the `CodecParameters::audio` builder consumes (no consumer behaviour change). Pinned by 13 new `tests/track_audio.rs` cases covering the all-default `Audio`-master-with-empty-body surface (sampling 8000.0 Hz + derived-default 8000.0 Hz output + 1 channel + `None` bit_depth + `!is_sbr`), explicit `SamplingFrequency=48000.0` round-trip, an f32 (4-byte float) `SamplingFrequency=44100.0` payload, explicit `OutputSamplingFrequency=44100.0` paired with `SamplingFrequency=22050.0` firing `is_sbr()` and `output_sampling_frequency_explicit() == Some(44100.0)`, equal explicit `OutputSamplingFrequency==SamplingFrequency` NOT firing `is_sbr()` while still recording `Some(48000.0)` (distinguishable from absent), explicit `Channels=6` (5.1) round-trip with `SamplingFrequency` keeping its 8000.0 default, explicit `BitDepth=24` round-trip, all four children set together (48 kHz / 48 kHz output / stereo / 16-bit / no SBR), an audio track without an `Audio` sub-master surfacing `None` (pathological-but-tolerated case), a video track surfacing `None`, a mixed audio+video file with `track_audio(0)` populated and `track_audio(1)` empty, an out-of-range `stream_index` returning `None` without panic, and a slice-mirror cross-check confirming `all_track_audio()[idx].as_ref() == track_audio(idx)` for every populated index.
- demux: **typed `TrackAudienceFlags` accessor** (RFC 9559 §5.1.4.1.6..§5.1.4.1.11). New `MkvDemuxer::track_audience_flags(stream_index) -> Option<&TrackAudienceFlags>` (and the all-streams `all_track_audience_flags() -> &[TrackAudienceFlags]`) folds the six per-`TrackEntry` audience flags — `FlagForced` (id `0x55AA`, §5.1.4.1.6), `FlagHearingImpaired` (id `0x55AB`, §5.1.4.1.7), `FlagVisualImpaired` (id `0x55AC`, §5.1.4.1.8), `FlagTextDescriptions` (id `0x55AD`, §5.1.4.1.9), `FlagOriginal` (id `0x55AE`, §5.1.4.1.10), `FlagCommentary` (id `0x55AF`, §5.1.4.1.11) — into one typed record per stream. Spec defaults are materialised asymmetrically: `TrackAudienceFlags::forced() -> bool` always reflects the §5.1.4.1.6 default `0` (a `TrackEntry` with no `FlagForced` child decodes `false`), while the five `minver: 4` flags carry no spec default and surface as `Option<bool>` so callers can distinguish "writer was silent" (`None`) from "writer explicitly cleared the flag" (`Some(false)`) — the §5.1.4.1.7..§5.1.4.1.11 wording ("Set to 1 *if and only if* …") makes that distinction load-bearing. Convenience predicates: `is_default_presentation()` returns `true` when no flag sets a `Some(true)` (a quick filter for vanilla content tracks), `is_accessibility()` returns `true` when any of `hearing_impaired` / `visual_impaired` / `text_descriptions` is explicitly set. Every track surfaces a record — `FlagForced`'s "applies only to subtitles" note doesn't suppress the surface on audio / video tracks because the spec puts the elements on `TrackEntry` itself with `minOccurs: 1` for `FlagForced`; the typed surface trusts the caller to apply each flag where it makes sense for the track's `TrackType` / `CodecID`. New `ids::FLAG_FORCED` / `ids::FLAG_HEARING_IMPAIRED` / `ids::FLAG_VISUAL_IMPAIRED` / `ids::FLAG_TEXT_DESCRIPTIONS` / `ids::FLAG_ORIGINAL` / `ids::FLAG_COMMENTARY` constants plumb the element ids through to the `TrackEntry` walker. Pinned by 10 new `tests/track_audience_flags.rs` cases covering the all-absent default-materialisation surface (`forced == false` + all-`None`), explicit `FlagForced=1` on a subtitle track, explicit `FlagForced=0` observationally indistinguishable from absent (since the spec default *is* `0`), explicit `FlagHearingImpaired=1` + its `is_accessibility()` predicate, explicit `FlagHearingImpaired=0` distinct from absent (`Some(false)` vs `None`), all five `minver: 4` flags set together independently, multi-track files surfacing one record per stream-index, out-of-range stream index returning `None`, `TrackAudienceFlags::default()` matching the empty-`TrackEntry` decode, and the predicate split treating `Some(false)` flags as default-presentation while `Some(true)` triggers the accessibility / non-default surface.
- demux: **typed per-Cluster `Position` / `PrevSize` records** (RFC 9559 §5.1.3.2 / §5.1.3.3). New `MkvDemuxer::cluster_records() -> &[ClusterRecord]` surfaces each Cluster's optional `Position` (id `0xA7`, `uinteger`) and `PrevSize` (id `0xAB`, `uinteger`) children as they're walked. Records are appended in first-encounter order through `next_packet` / `seek_to` and keyed by `body_offset` (the absolute file offset of the byte right after the Cluster's id+size header — the dedup key, so a back-then-forward seek that revisits the same Cluster doesn't push a duplicate row). Both typed fields are `Option<u64>`: `None` when the on-disk child was absent (common for `PrevSize` on the first Cluster of a Segment, and for both fields when a writer omitted them entirely), `Some(v)` when present. The `Some(0)` `Position` case is the §5.1.3.2 spec convention for live streams (offset not determined ahead of time) — distinct from `None`. Consumers can verify a recorded `Position` matches the actual on-disk offset by subtracting `segment_data_start` + the Cluster's header length from `body_offset` (the §16 Segment-Position definition), build a reverse walker on top of `PrevSize` without re-scanning the SeekHead, or detect a live stream by seeing `Some(0)` `Position` values. The slice grows incrementally — callers wanting the full per-Cluster set should drain the file via `next_packet` (or seek to every Cluster of interest) first. New `ids::POSITION` / `ids::PREV_SIZE` constants (`0xA7` / `0xAB`) plumb the element ids through to the Cluster walker. Pinned by 6 new `tests/cluster_records.rs` cases covering the empty-slice surface before any walk, two-Cluster capture preserving on-disk order with mixed present/absent fields, the all-`None` no-children surface, the `Some(0)` live-stream `Position` distinct from `None`, the `body_offset → Position` derivation matching the §16 Segment-Position definition exactly (built bytes-out so offsets are known), and the dedup contract across repeat `next_packet` walks past EOF.
- demux: **typed `Targets::target_level()` hierarchy resolver** (RFC 9559 §5.1.8.1.1.1, Table 33). New `Targets::target_level() -> Option<TargetLevel>` maps the raw `target_type_value: Option<u64>` field through the typed `TargetLevel` enum: `Shot=10` / `Subtrack=20` / `Track=30` / `Part=40` / `Album=50` / `Edition=60` / `Collection=70`, plus `Other(u64)` for values registered under the §27.13 "Matroska Tags Target Types" registry after RFC 9559. The enum derives `Ord` in spec-containment order (lowest to highest), so the §5.1.8.1.1.1 usage note ("Higher values MUST correspond to a logical level that contains the lower logical level TargetTypeValue values") falls straight out of comparison; `Other(_)` sorts after every named level so a forward-compat entry doesn't break the rule for the named ones. Returns `None` when the on-disk `TargetTypeValue` element was absent — distinguishable from `Some(TargetLevel::Album)` (= the spec default `50` materialised explicitly by a writer). Companion methods: `TargetLevel::from_raw(u64)` constructs the enum; `TargetLevel::to_raw() -> u64` is the lossless inverse covering every named variant and the `Other(u64)` forward-compat passthrough; `TargetLevel::canonical_label() -> Option<&'static str>` returns the leftmost / most common Table 33 label for the level (`ALBUM` for value `50`, not the alternate `OPERA` / `CONCERT` / `MOVIE` / `EPISODE` labels — the integer is the canonical hierarchy key and the string is purely a display hint). The file's own `TargetType` informational string remains accessible verbatim on the existing `Targets::target_type` field; the typed level helper doesn't overwrite it. Pinned by 7 new `tests/tags.rs` cases covering the seven named round-trips (`Shot` / `Subtrack` / `Track` / `Part` / `Album` / `Edition` / `Collection` against raw `10` / `20` / `30` / `40` / `50` / `60` / `70` with their canonical Table 33 labels), the `Other(u64)` forward-compat passthrough across `[1, 25, 55, 80, 90, 100, 1_000, u64::MAX]`, the `Ord` chain `Shot < Subtrack < Track < Part < Album < Edition < Collection < Other(1) < Other(2)`, an end-to-end demux of a Tag carrying `TargetTypeValue=30` + `TargetType="TRACK"` + resolved `TagTrackUID=0xA1` mapping to `Some(TargetLevel::Track)`, the `None` surface when the `TargetTypeValue` element was absent, the `Other(25)` forward-compat surface when a Tag carries an unrecognised value, and the explicit-default case (`TargetTypeValue=50` + `TargetType="MOVIE"` → `Some(TargetLevel::Album)` with `canonical_label() == Some("ALBUM")` while `target_type` stays `Some("MOVIE")`).
- mux: **write-side `Colour > MasteringMetadata` sub-master** (RFC 9559 §5.1.4.1.28.30..§5.1.4.1.28.40) via a new `mastering_metadata: Option<MkvMasteringMetadata>` field on `MkvVideoColour` and the new `MkvMasteringMetadata` payload struct (mux module). When `Some(_)`, the muxer emits a `MasteringMetadata` master (id `0x55D0`) inside the parent `Colour` master, after the existing `MaxCLL` / `MaxFALL` pair. Each of the ten chromaticity / luminance children — `PrimaryRChromaticityX/Y` (`0x55D1` / `0x55D2`), `PrimaryGChromaticityX/Y` (`0x55D3` / `0x55D4`), `PrimaryBChromaticityX/Y` (`0x55D5` / `0x55D6`), `WhitePointChromaticityX/Y` (`0x55D7` / `0x55D8`), `LuminanceMax` (`0x55D9`), `LuminanceMin` (`0x55DA`) — is written as an 8-byte big-endian `f64` only when its own `Option<f64>` slot is `Some(v)`, mirroring the per-child omission rules already in use for the scalar Colour children. Children are written in numerical-id order so the on-disk layout matches the order the demuxer walks them. A `Some(MkvMasteringMetadata::default())` (every slot `None`) serialises as an empty 3-byte `MasteringMetadata` master (id `0x55D0` + size VINT `0x80`) that the demuxer parses into `Some(MasteringMetadata::default())` — distinct from the slot-omitted case (`mastering_metadata: None`) which keeps the entire sub-master off-disk so the demuxer surfaces `None` from its `mastering_metadata()` accessor. Convenience constructor `MkvMasteringMetadata::bt2020_d65_hdr10()` populates the ten-child set with BT.2020 primaries (red `(0.708, 0.292)`, green `(0.170, 0.797)`, blue `(0.131, 0.046)`), D65 white point `(0.3127, 0.3290)`, 1000 cd/m² peak luminance and 0.005 cd/m² floor — the canonical HDR10 mastering display. The muxer does NOT validate the spec's `[0.0, 1.0]` chromaticity range or `>= 0` luminance range — out-of-range values reach disk verbatim so a producer mirroring a file with out-of-spec values can preserve them. Pairs symmetrically with the existing demux-side `MasteringMetadata` typed accessor — a mux→demux round-trip preserves every populated child. The write-path closes the §5.1.4.1.28.30 gap previously called out in this CHANGELOG under the §5.1.4.1.28.16 scalar-children entry (and the `## What's NOT implemented` README section): `MkvMuxer::set_video_colour` now covers `Colour` and `Colour > MasteringMetadata` end-to-end. Pinned by 8 new `tests/mux_video_colour.rs` cases covering the BT.2020 + D65 + 1000-nit / 0.005-nit canonical-HDR10 round-trip alongside the BT.2100 PQ scalar children carried in the same `Colour` master, the sparse-LuminanceOnly case with the remaining nine children staying `None` end-to-end, the omitted-slot `None` surface with an on-disk id-byte scan confirming `0x55D0` does not appear, the empty `MasteringMetadata` master surfacing as `Some(MasteringMetadata::default())` with a literal `[0x55, 0xD0, 0x80]` byte-walk pinning the 3-byte empty-master shape, ten independent per-child round-trips (distinct values per slot so any field-id transposition surfaces as the wrong number on the wrong getter), simultaneous round-trip alongside the `MaxCLL` / `MaxFALL` integer pair and the scalar Colour children (BT.2020 / PQ / full range / 10 bpc preserved), the muxer's own `video_colour(stream_index)` accessor returning the queued `MkvMasteringMetadata`, and the out-of-range chromaticity / luminance pass-through contract (`1.5` and `-1.0` reaching disk verbatim).
- demux: **typed `BlockAdditionMapping` decode** (RFC 9559 §5.1.4.1.17). `MkvDemuxer::block_addition_mappings(stream_index)` (and the per-stream `all_block_addition_mappings()` slice) returns each `Tracks > TrackEntry > BlockAdditionMapping` master (id `0x41E4`) as a typed `BlockAdditionMapping` record with `value` (`BlockAddIDValue`, §5.1.4.1.17.1, id `0x41F0`, `Option<u64>` — spec range `>=2`, no default), `name` (`BlockAddIDName`, §5.1.4.1.17.2, id `0x41A4`, `Option<String>`), `addid_type` (`BlockAddIDType`, §5.1.4.1.17.3, id `0x41E7`, `u64` — §5.1.4.1.17.3 default `0` materialised), and `extra_data` (`BlockAddIDExtraData`, §5.1.4.1.17.4, id `0x41ED`, `Option<Vec<u8>>` — opaque per-track binary state the type interpreter consults). New `BlockAdditionMapping::is_codec_defined()` helper reports whether `addid_type == 0` (the §5.1.4.1.17.3 usage-note case in which the matching `BlockAddID` must be `1`). Multiple mappings per `TrackEntry` are preserved in on-disk order (the spec gives the master no `maxOccurs`); tracks with no `BlockAdditionMapping` child surface as an empty slice (the common case — the element only appears on tracks that use `BlockAdditional` to extend their on-disk format, e.g. WebM alpha at `BlockAddID == 1`, HDR dynamic metadata, or ITU-T T.35 frame-level metadata). Unknown child elements inside the master are skipped — the spec leaves the `BlockAddIDType` registry open for future additions. Per-frame `BlockAdditional` payload bytes remain owned by the codec / track-format extension; the container surfaces the *shape* of the side channel only. Pinned by 6 new `tests/block_addition_mapping.rs` cases covering the absent-master empty-slice surface, a single mapping with all four children, the §5.1.4.1.17.3 default-`0` materialisation on an empty mapping master, multiple mappings preserving document order, out-of-range `stream_index` returning an empty slice, and unknown-child skipping.
- mux: **write-side `Video > Colour` scalar children** (RFC 9559 §5.1.4.1.28.16, §5.1.4.1.28.17..§5.1.4.1.28.29) via the new `MkvMuxer::set_video_colour(stream_index, MkvVideoColour)` builder method. Emits a `Colour` master (id `0x55B0`) inside the per-track `Video` master at `write_header` time, after the existing `UncompressedFourCC` block, carrying the eleven scalar children: `MatrixCoefficients` (`0x55B1`), `BitsPerChannel` (`0x55B2`), `ChromaSubsamplingHorz` / `ChromaSubsamplingVert` / `CbSubsamplingHorz` / `CbSubsamplingVert` (`0x55B3` / `0x55B4` / `0x55B5` / `0x55B6`), `ChromaSitingHorz` / `ChromaSitingVert` (`0x55B7` / `0x55B8`), `Range` (`0x55B9`), `TransferCharacteristics` (`0x55BA`), `Primaries` (`0x55BB`), `MaxCLL` (`0x55BC`), `MaxFALL` (`0x55BD`). New `MatrixCoefficients::to_raw()` / `ChromaSitingHorz::to_raw()` / `ChromaSitingVert::to_raw()` / `ColourRange::to_raw()` / `TransferCharacteristics::to_raw()` / `Primaries::to_raw()` inverse methods on the demux-side enums round-trip every Table 12 / Table 13 / Table 14 / Table 15 / Table 16 / Table 17 value plus the `Other(u64)` forward-compat variant (§27.10..§27.13 leave the registries open). Convenience constructors `MkvVideoColour::bt709()` (matrix `1` / transfer `1` / primaries `1` / broadcast range — the canonical SDR HD shape) and `MkvVideoColour::bt2020_pq()` (matrix `9` / transfer `16` / primaries `9` / full range / 10 bpc — the canonical HDR10 shape) cover the two everyday cases. Per-element omission rules implemented at write time: every scalar that equals its §5.1.4.1.28 spec default is left off-disk so the demuxer materialises the spec default; every `Option<u64>` (the four chroma-subsampling integers and the `MaxCLL` / `MaxFALL` pair, none of which have spec defaults) is written when `Some(v)` and skipped when `None`. As a result, queueing `MkvVideoColour::default()` writes an empty 3-byte `Colour` master (id `0x55B0` + size VINT `0x80`), which the demuxer parses into `Some(VideoColour::default())` with every getter returning the materialised spec default — distinguishable on disk from the call-was-omitted case, which keeps the `Colour` master off-disk entirely so the demuxer surfaces `None` from `video_colour`. Spec rules enforced at queue time: `set_video_colour` rejects calls made after `write_header`, out-of-range `stream_index`, and calls on non-video tracks. `MasteringMetadata` (§5.1.4.1.28.30..§5.1.4.1.28.40) is not yet emitted on the write side; this round covers the scalar Colour children only. Pinned by 25 new `tests/mux_video_colour.rs` cases covering the BT.709 SDR + HDR10 PQ round-trips, `MaxCLL` / `MaxFALL` integer pair, the four-element chroma-subsampling quartet, every `Other(u64)` forward-compat path through each of the six enum-typed children, `ChromaSitingHorz` / `ChromaSitingVert` explicit-non-default and shared `Half` round-trips, the Table 17 `EbuTech3213EJedecP22Phosphors` 12-to-22 spec-gap, the omitted-call `None` surface, the empty-Colour-master `Some(default)` surface (including a 3-byte literal walk for the empty master shape), on-disk element-id presence/absence (id-byte scan for `0x55B0`), every rejection contract (post-`write_header`, out-of-range stream index, audio-track), idempotent last-write-wins under repeated calls, the muxer's own `video_colour(stream_index)` accessor, audio-track index returning `None` even on a multi-track file, and independence from every other `Video` sub-element setter (interlacing, stereo, alpha, geometry, UncompressedFourCC).
- mux: **write-side `Video > UncompressedFourCC`** (RFC 9559 §5.1.4.1.28.15) via the new `MkvMuxer::set_video_uncompressed_fourcc(stream_index, [u8; 4])` builder method. Emits `UncompressedFourCC` (id `0x2EB524`, `binary`, schema-fixed `length: 4`) inside the per-track `Video` master at `write_header` time, after the existing `PixelCrop*` / `Display*` block. The setter takes a `[u8; 4]` array directly so the §5.1.4.1.28.15 schema's fixed length is enforced at the type system rather than at queue time, and every byte (including high bytes and `0x00`) is written verbatim — the element is `binary`, not `string`, and the muxer never treats the four-byte payload as text. Spec rules enforced at queue time: `set_video_uncompressed_fourcc` rejects calls made after `write_header`, out-of-range `stream_index`, and calls on non-video tracks. Omitting the call leaves the element off-disk so the demuxer's `MkvDemuxer::video_uncompressed_fourcc` surfaces `None` — §5.1.4.1.28.15 defines no default, and Table 11's `minOccurs=1` pin only fires for `CodecID == "V_UNCOMPRESSED"`, which the muxer does not presently emit. Pairs symmetrically with the existing `MkvDemuxer::video_uncompressed_fourcc` typed accessor — a mux→demux pipeline preserves the four-byte FourCC bit-exactly. Pinned by 13 new `tests/mux_video_uncompressed_fourcc.rs` cases covering printable / non-printable / high-byte FourCC round-trips (`YUY2`, `BGRA`, `NV12`, the `[0xFF, 0x00, 0x12, 0xAB]` binary-passthrough), the omitted-call `None` surface, on-disk element-id presence/absence (id-byte scan for `0x2EB524`), an explicit element-header literal walk confirming size VINT `0x84` + a four-byte payload, every rejection contract (post-`write_header`, out-of-range stream index, audio-track), idempotent last-write-wins under repeated calls, the muxer's own `video_uncompressed_fourcc(stream_index)` accessor returning the queued value pre-`write_header`, and independence from `set_video_geometry` (setting one does not affect the other).
- mux: **write-side `Video` geometry quartet** (RFC 9559 §5.1.4.1.28.8..§5.1.4.1.28.14) via the new `MkvMuxer::set_video_geometry(stream_index, MkvVideoGeometry)` builder method. Emits `PixelCropTop` (id `0x54BB`) / `PixelCropBottom` (id `0x54AA`) / `PixelCropLeft` (id `0x54CC`) / `PixelCropRight` (id `0x54DD`) plus `DisplayWidth` (id `0x54B0`) / `DisplayHeight` (id `0x54BA`) / `DisplayUnit` (id `0x54B2`) inside the per-track `Video` master at `write_header` time, alongside the existing `PixelWidth` / `PixelHeight`. New `DisplayUnit::to_raw()` inverse method on the demux-side enum round-trips every Table 10 value (`Pixels` / `Centimeters` / `Inches` / `DisplayAspectRatio` / `Unknown`) plus the `Other(u64)` forward-compat variant (§27.9 leaves the "Matroska Display Units" registry open). Per-element omission rules implemented at write time: zero crops stay off-disk so the demuxer materialises the §5.1.4.1.28.8..11 default `0`; `DisplayWidth` / `DisplayHeight` are written when `Some` and skipped when `None`; `DisplayUnit` is written explicitly only for non-`Pixels` values, so a producer re-muxing a file that did not carry an explicit `DisplayUnit` stays byte-faithful to the §5.1.4.1.28.14 default-omission case. Convenience constructors `MkvVideoGeometry::cropped(top, bottom, left, right)` (the RFC 9559 §11.1 pillar-box / letterbox shape, no display-size override, `Pixels` unit) and `MkvVideoGeometry::aspect_ratio(num, den)` (`DisplayUnit::DisplayAspectRatio` + the ratio encoded as `DisplayWidth` / `DisplayHeight`) cover the two common shapes. Spec rules enforced at queue time: `set_video_geometry` rejects calls made after `write_header`, out-of-range `stream_index`, calls on non-video tracks, and `Some(0)` on either `display_width` / `display_height` per the §5.1.4.1.28.12 / .13 `range: not 0` pin. Pinned by 15 new `tests/mux_video_geometry.rs` cases covering the pillar-box round-trip with derived-default display dimensions (`PixelWidth - cropLeft - cropRight = 1440` for the 1920x1080 worked example), four-axis non-zero crop round-trips, the aspect-ratio override shape, `DisplayUnit::Other(u64)` and `Centimeters` round-trips, the absent-dimension + non-Pixels-unit `None` surface, the omitted-call spec-default path with on-disk element-id absence scan, the `DisplayUnit::Pixels` default-omission rule, every rejection contract, idempotent last-write-wins under repeated calls, and the typed `video_geometry(stream_index)` accessor.
- mux: **write-side `Video > StereoMode` + `AlphaMode`** (RFC 9559 §5.1.4.1.28.3 + §5.1.4.1.28.4) via the new `MkvMuxer::set_video_stereo_mode(stream_index, StereoMode)` and `MkvMuxer::set_video_alpha_mode(stream_index, AlphaMode)` builder methods. Emits `StereoMode` (id `0x53B8`) and `AlphaMode` (id `0x53C0`) inside the per-track `Video` master at `write_header` time, immediately after the existing `FlagInterlaced` / `FieldOrder` pair. New `StereoMode::to_raw()` / `AlphaMode::to_raw()` inverse methods on the demux-side enums round-trip every Table 5 / Table 6 value plus the `Other(u64)` forward-compat variant (§27.7 / §27.8 leave both registries open). Spec rules enforced at queue time: both setters reject calls made after `write_header`, out-of-range `stream_index`, and calls on non-video tracks. Calling `set_video_stereo_mode(_, StereoMode::Mono)` / `set_video_alpha_mode(_, AlphaMode::None)` explicitly still writes the element on disk so producers can override downstream defaults; omitting the call entirely leaves the element off-disk so the demuxer materialises the §5.1.4.1.28.3 default `0` (Mono) / §5.1.4.1.28.4 default `0` (None). Pinned by 21 new `tests/mux_video_stereo_alpha.rs` cases covering side-by-side / top-bottom / anaglyph / both-eyes-laced / `Other(u64)`-passthrough round-trips, the omitted-call spec-default path, on-disk element-id presence/absence (id-byte scan), every rejection contract, idempotent last-write-wins under repeated calls, and the independence between StereoMode and AlphaMode settings.
- mux: **write-side `Video > FlagInterlaced` + `FieldOrder`** (RFC 9559 §5.1.4.1.28.1 + §5.1.4.1.28.2) via the new `MkvMuxer::set_video_interlacing(stream_index, FlagInterlaced, Option<FieldOrder>)` builder method. Emits `FlagInterlaced` (id `0x9A`) and `FieldOrder` (id `0x9D`) inside the per-track `Video` master at `write_header` time, alongside the existing `PixelWidth` / `PixelHeight`. New `FlagInterlaced::to_raw()` / `FieldOrder::to_raw()` inverse methods on the demux-side enums round-trip every Table 3 / Table 4 value (`Undetermined` / `Interlaced` / `Progressive` + `Progressive` / `Tff` / `Undetermined` / `Bff` / `TffInterleaved` / `BffInterleaved`) plus the `Other(u64)` forward-compat variant. Spec rules enforced at queue time: `set_video_interlacing` rejects calls made after `write_header`, out-of-range `stream_index`, calls on non-video tracks, and `FieldOrder` paired with anything other than `FlagInterlaced::Interlaced` (§5.1.4.1.28.2's "If FlagInterlaced is not set to 1, this element MUST be ignored"). Omitting the call leaves both elements off-disk so the demuxer materialises the §5.1.4.1.28.1 default `0` (Undetermined) / §5.1.4.1.28.2 default `2` (Undetermined). Pinned by 14 new `tests/mux_video_interlacing.rs` cases covering TFF / BFF / progressive / default-FieldOrder / `Other(u64)`-passthrough round-trips, the omitted-call spec-default path, on-disk element-id presence/absence, every rejection contract, and idempotent last-write-wins under repeated calls.
- mux: write a leading `CRC-32` child (RFC 8794 §11.3.1, RFC 9559 §6.2) on every Top-Level master the muxer buffers end-to-end before flushing — `Info`, `Tracks`, `Cues`, plus `Chapters` and `Attachments` when those are queued. 6-byte on-disk shape (`0xBF` id + `0x84` size VINT + 4 LE payload bytes); `SeekHead` is deliberately not CRC'd because its Cues entry is patched in `write_trailer`; `Cluster` is not CRC'd because the muxer streams Clusters with the unknown-size VINT and RFC 8794 §11.3.1 requires a bounded body. Round-trip tests assert every emitted CRC validates through `MkvDemuxer::crc_status()`.
- demux: validate a leading `CRC-32` child on the late best-effort `Cues` rescan (the path used when `Cues` sits after the final `Cluster` — the common single-pass-mux layout). Statuses surface through the existing `crc_status()` accessor with `element_id == ids::CUES`, closing the previously-documented gap.
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
- RFC 9043 v3 slice layout + CRC-32 parity; third-party-muxed → us decodes
- add lossless video codec — bit-exact self-roundtrip, RFC 9043 v3
- add rustfmt + clippy gates; release: macOS universal binary
- add Matroska (MKV) container + Opus crate + proper Ogg header handling
