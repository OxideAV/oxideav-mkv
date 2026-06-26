# oxideav-mkv

Pure-Rust **Matroska (MKV)** and **WebM** container — demuxer + muxer
built on the EBML primitives from RFC 8794. Zero C dependencies.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-codec = "0.1"
oxideav-container = "0.1"
oxideav-mkv = "0.0"
```

## Quick use

Register both containers (`"matroska"` and `"webm"`) and let the probe
pick which DocType the file carries:

```rust
use oxideav_container::ContainerRegistry;

let mut containers = ContainerRegistry::new();
oxideav_mkv::register(&mut containers);

let input: Box<dyn oxideav_container::ReadSeek> = Box::new(
    std::fs::File::open("movie.mkv")?,
);
let mut dmx = containers.open_demuxer("matroska", input)?;
for s in dmx.streams() {
    println!("track {}: {}", s.index, s.params.codec_id.as_str());
}
loop {
    match dmx.next_packet() {
        Ok(p) => { /* feed p into a decoder from oxideav-codec */ }
        Err(oxideav_core::Error::Eof) => break,
        Err(e) => return Err(e.into()),
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

The demuxer returns raw `Packet` bytes — pair it with a decoder crate
(e.g. [`oxideav-opus`](https://crates.io/crates/oxideav-opus),
[`oxideav-flac`](https://crates.io/crates/oxideav-flac),
[`oxideav-vp9`](https://crates.io/crates/oxideav-vp9)) or go through
the unified `oxideav` aggregator to wire decoding automatically.

## What's implemented

### Demuxer (`demux::open`)

- EBML header parse, DocType validation (`matroska` / `webm`).
- **Typed EBML header accessor** (RFC 8794 §11.2): `MkvDemuxer::ebml_header()
  -> &EbmlHeader` surfaces the full parsed header — `ebml_version` /
  `ebml_read_version` / `ebml_max_id_length` / `ebml_max_size_length`
  (§11.2.2..§11.2.5, spec defaults `1` / `1` / `4` / `8` materialised when
  absent), `doc_type`,
  `doc_type_version` / `doc_type_read_version` (spec default `1` materialised
  when the element was absent), and every well-formed `DocTypeExtension`
  (§11.2.9..§11.2.11) declaration in document order. Each `DocTypeExtension`
  pairs a per-header-unique `name` (`DocTypeExtensionName`, §11.2.10, length
  `>0`) with a non-zero `version` (`DocTypeExtensionVersion`, §11.2.11) — the
  experimental / out-of-band element-set declarations a reader checks before
  relying on extension elements. A malformed extension missing either
  mandatory child (or carrying an empty name / zero version) is dropped at
  parse time. The common file declares none and surfaces an empty list.
- Segment walk: `Info`, `Tracks`, `Tags`, `Cues`, `Cluster`. Known- and
  unknown-size Segment/Cluster both supported.
- Clusters: `SimpleBlock` and `BlockGroup -> Block`, all three lacing
  modes (Xiph, fixed, EBML-signed-delta).
- **Typed `BlockGroup` meta** (`demux::open_typed`):
  `MkvDemuxer::block_group_meta() -> Option<&BlockGroupMeta>` surfaces the
  four `BlockGroup` children the `Packet` type has no slot for —
  `ReferenceBlock` (RFC 9559 §5.1.3.5.5, every value in on-disk order),
  `ReferencePriority` (§5.1.3.5.4, default `0` materialised), `CodecState`
  (§5.1.3.5.6), and `DiscardPadding` (§5.1.3.5.7) — alongside the existing
  `block_additions()` side channel (same read-after-`next_packet` call
  discipline, cleared on seek). The same record also surfaces the reclaimed
  DivX trick-track / old-lacing `BlockGroup` children (RFC 9559 Appendix
  A.3..A.14) for a faithful re-mux: `block_virtual()` (`BlockVirtual`, A.3),
  `reference_virtual()` (`ReferenceVirtual`, A.4), `slices()` (every
  `Slices > TimeSlice` master, A.5..A.11 — each `TimeSlice` folds `LaceNumber` /
  `FrameNumber` / `BlockAdditionID` / `Delay` / `SliceDuration`), and
  `reference_frame()` (`ReferenceFrame`, A.12..A.14 — `ReferenceOffset` +
  `ReferenceTimestamp`). Every reclaimed field is a pure on-disk projection
  (`None`/empty = absent, present `0` = `Some(0)`); none is interpreted by the
  container. The muxer writes them through `BlockGroupOptions`
  (`block_virtual` / `reference_virtual` / `slices` / `reference_frame`), so a
  mux→demux pipeline round-trips every populated child.
- **`SilentTracks`** (RFC 9559 Appendix A.1 / A.2): per-Cluster
  `SilentTrackNumber` lists surface on `ClusterRecord::silent_track_numbers`
  in on-disk order (deprecated element, but read for faithful re-mux).
- Metadata lift: title, muxer, encoder, date (Matroska `DateUTC` ->
  ISO-8601), Tags `SimpleTag` name/value pairs with **target-scope
  resolution** (`Tags.Targets.TagTrackUID` -> `tag:track:N:<name>`,
  `TagChapterUID` -> `tag:chapter:N:<name>`, `TagAttachmentUID` ->
  `tag:attachment:N:<name>`, `TagEditionUID` -> `tag:edition:N:<name>`;
  all-zero UIDs -> bare `<name>` global key; unresolved non-zero UIDs
  are dropped per RFC 9559 §5.1.8.1.1.x "MUST match"), `Chapters`
  (`chapter:N:start_ms` / `:end_ms` / `:title`, ns→ms), and
  `Attachments` (`attachment:N:filename` / `:mime_type` / `:size_bytes`;
  payload is skipped, only the index surfaces).
- **Typed `Tag` accessor**: `demux::open_typed` returns the concrete
  `MkvDemuxer`, whose `.tags() -> &[Tag]` exposes RFC 9559 §5.1.8.1
  fields the flat metadata view drops — `TargetType` /
  `TargetTypeValue` informational hints, multi-UID `Targets` masters
  (one `Tag` can scope to several tracks/chapters at once), per-
  `SimpleTag` `TagLanguage` / `TagLanguageBCP47` / `TagDefault`,
  and binary `TagBinary` payloads (e.g. embedded cover-art bytes).
  Tags with only dangling non-zero UIDs are filtered out per
  §5.1.8.1.1.3..§5.1.8.1.1.6; mixed Targets keep their resolvable UIDs.
  Nested `SimpleTag`s (§5.1.8.1.2 `recursive: True`) surface through
  `SimpleTag::children` — a hierarchical tag (e.g. a `TITLE` carrying a
  `SORT_WITH` sub-tag, or a name-only parent grouping several `ARTIST`
  leaves) is preserved as a tree rather than being flattened or dropped,
  parsed up to a 16-level depth cap with name-less children dropped per
  the §5.1.8.1.2.1 `minOccurs: 1` rule. The flat `metadata()` view still
  surfaces only top-level descriptors.
- **`Targets::target_level()` typed hierarchy** (RFC 9559 §5.1.8.1.1.1,
  Table 33): `Targets::target_level() -> Option<TargetLevel>` resolves
  the raw `target_type_value` integer into the typed `TargetLevel`
  enum (`Shot=10` / `Subtrack=20` / `Track=30` / `Part=40` /
  `Album=50` / `Edition=60` / `Collection=70`, plus `Other(u64)` for
  values registered under the §27.13 "Matroska Tags Target Types"
  registry after RFC 9559). The enum derives `Ord` in spec-containment
  order so a player can walk the album → track → subtrack hierarchy
  without re-comparing raw integers — the §5.1.8.1.1.1 usage note
  ("Higher values MUST correspond to a logical level that contains
  the lower logical level TargetTypeValue values") falls straight out
  of `Ord`. `Other(_)` sorts after every named level so a future entry
  doesn't break the comparison rule for the named ones. Returns `None`
  when the `TargetTypeValue` element was absent on disk —
  distinguishable from `Some(TargetLevel::Album)` (the spec default
  `50` materialised by a writer). Inverse `TargetLevel::to_raw()`
  round-trips every named variant + the `Other(u64)` forward-compat
  passthrough. Companion `TargetLevel::canonical_label()` returns the
  leftmost / most common Table 33 label for the level (e.g. `ALBUM`
  for value `50`, not the alternate `OPERA` / `CONCERT` / `MOVIE` /
  `EPISODE` labels); the file's own `TargetType` informational string
  stays on the existing `Targets::target_type` field — the typed level
  helper doesn't overwrite it.
- **Typed `TrackAudienceFlags` accessor** (RFC 9559 §5.1.4.1.6..§5.1.4.1.11):
  `MkvDemuxer::track_audience_flags(stream_index) -> Option<&TrackAudienceFlags>`
  (and the per-stream `all_track_audience_flags()` slice) folds the six
  per-`TrackEntry` audience hints — `FlagForced` (id `0x55AA`),
  `FlagHearingImpaired` (id `0x55AB`), `FlagVisualImpaired` (id `0x55AC`),
  `FlagTextDescriptions` (id `0x55AD`), `FlagOriginal` (id `0x55AE`),
  `FlagCommentary` (id `0x55AF`) — into one typed record per stream. Spec
  defaults are materialised asymmetrically: `forced()` returns a bare `bool`
  with the §5.1.4.1.6 default `0` always reflected (a `TrackEntry` with no
  `FlagForced` child decodes `false`); the five `minver: 4` flags carry no
  spec default and surface as `Option<bool>` so callers can distinguish
  "writer was silent" (`None`) from "writer explicitly cleared the flag"
  (`Some(false)`) — the §5.1.4.1.7..§5.1.4.1.11 wording ("Set to 1 *if and
  only if* …") makes that distinction load-bearing. Convenience predicates
  `is_default_presentation()` (no flag is `Some(true)`) and
  `is_accessibility()` (any of `hearing_impaired` / `visual_impaired` /
  `text_descriptions` is `Some(true)`) cover the common filter cases. Every
  track surfaces a record — `FlagForced`'s spec wording "applies only to
  subtitles" does not suppress the surface on audio / video tracks because
  the spec puts the elements on `TrackEntry` itself with `minOccurs: 1` for
  `FlagForced`; the typed surface trusts the caller to apply each flag
  where it makes sense for the track's `TrackType` / `CodecID`.
- **Typed `TrackAudio` accessor** (RFC 9559 §5.1.4.1.29.1..§5.1.4.1.29.4):
  `MkvDemuxer::track_audio(stream_index) -> Option<&TrackAudio>` (and the
  per-stream `all_track_audio()` slice) folds the four `Audio` sub-master
  children — `SamplingFrequency` (id `0xB5`, §5.1.4.1.29.1),
  `OutputSamplingFrequency` (id `0x78B5`, §5.1.4.1.29.2), `Channels`
  (id `0x9F`, §5.1.4.1.29.3), `BitDepth` (id `0x6264`, §5.1.4.1.29.4) —
  into one typed record. Spec defaults are materialised asymmetrically:
  `sampling_frequency()` returns a bare `f64` with the §5.1.4.1.29.1
  default `0x1.f4p+12` = `8000.0` always reflected (an `Audio` master with
  no explicit child still surfaces 8000.0 Hz, never `0.0`); `channels()`
  returns a bare `u64` with the §5.1.4.1.29.3 default `1` (mono) always
  reflected; `output_sampling_frequency()` folds Table 19's derived default
  (= `sampling_frequency()` when the element was absent) but
  `output_sampling_frequency_explicit()` preserves the on-disk presence as
  `Option<f64>` so a re-muxer doesn't materialise an element that wasn't
  in the source. `bit_depth()` stays `Option<u64>` — §5.1.4.1.29.4 defines
  no default, so absence is observable. Convenience predicate `is_sbr()`
  returns `true` exactly when the writer emitted an explicit
  `OutputSamplingFrequency` strictly greater than `SamplingFrequency` (the
  canonical SBR-doubling signal for HE-AAC and similar tracks). Records
  surface only for `TrackEntry`s that carried an `Audio` master at all:
  video / subtitle / button tracks (where the master is `maxOccurs: 1` but
  carries no `minOccurs` at the `TrackEntry` level) return `None`, as does
  a malformed audio track that emitted no `Audio` child — the typed
  surface never synthesises a record from the spec defaults alone.
- **Typed `TrackTiming` accessor** (RFC 9559 §5.1.4.1.13..§5.1.4.1.15):
  `MkvDemuxer::track_timing(stream_index) -> Option<&TrackTiming>` (and the
  per-stream `all_track_timing()` slice) folds the three `TrackEntry`-level
  timing elements — `DefaultDuration` (id `0x23E383`, §5.1.4.1.13),
  `DefaultDecodedFieldDuration` (id `0x234E7A`, §5.1.4.1.14), and
  `TrackTimestampScale` (id `0x23314F`, §5.1.4.1.15) — into one record per
  track. The elements sit directly on `TrackEntry` (no gating master), so
  every valid track surfaces a record; `track_timing` returns `None` only
  for an out-of-range stream index. `default_duration()` is the container's
  nominal nanoseconds-per-frame source — `TrackTiming::nominal_frame_rate()`
  derives fps (`1e9 / ns`), so e.g. a `41708333` ns track yields `~23.976`.
  Both nanosecond durations carry a "not 0" range and no spec default, so
  they stay `Option<u64>` and a spec-illegal explicit `0` is dropped at parse
  time. `track_timestamp_scale()` materialises the §5.1.4.1.15 default `1.0`
  while `track_timestamp_scale_explicit()` preserves the on-disk presence
  (a non-finite / non-positive payload is dropped, since the spec range is
  `> 0x0p+0`). `TrackTiming::is_empty()` reports the all-absent state — a
  track that carried none of the three elements.
- **Typed `TrackIdentity` accessor** (RFC 9559 §5.1.4.1.18 / .19 / .20 / .23 /
  .4 / .5 / .12 / .24): `MkvDemuxer::track_identity(stream_index) ->
  Option<&TrackIdentity>` (and the per-stream `all_track_identity()` slice)
  folds the eight `TrackEntry`-level identity / selection elements — `Name`
  (id `0x536E`, §5.1.4.1.18), `Language` (id `0x22B59C`, §5.1.4.1.19),
  `LanguageBCP47` (id `0x22B59D`, §5.1.4.1.20), `CodecName` (id `0x258688`,
  §5.1.4.1.23), `FlagEnabled` (id `0xB9`, §5.1.4.1.4), `FlagDefault` (id
  `0x88`, §5.1.4.1.5), `FlagLacing` (id `0x9C`, §5.1.4.1.12), and
  `AttachmentLink` (id `0x7446`, §5.1.4.1.24) — into one record per track.
  The elements sit directly on `TrackEntry` (no gating master), so every
  valid track surfaces a record; `track_identity` returns `None` only for an
  out-of-range stream index. The four strings carry no spec default and stay
  `Option` (`name()` / `codec_name()` / `language_matroska()` /
  `language_bcp47()`); `language()` returns the effective language honouring
  the §5.1.4.1.20 precedence (`LanguageBCP47` supersedes `Language` — "any
  Language elements ... MUST be ignored"), and `uses_bcp47()` reports it. The
  three selection flags carry the spec default `1`: `enabled()` / `default()`
  / `lacing_allowed()` materialise it while `*_explicit()` preserve the
  on-disk presence so a re-muxer can distinguish "writer was silent" from
  "writer explicitly cleared the flag". `attachment_link()` surfaces the
  `FileUID` of an attachment the track's codec uses (e.g. a font for an
  ASS/SSA subtitle track), matching an `Attachment::uid` from `attachments()`;
  a spec-illegal `0` (range "not 0") is dropped at parse time.
  `TrackIdentity::is_default()` reports the all-absent state. The effective
  language also lifts onto the flat `StreamInfo` view (BCP-47 preferred).
- **Typed `TrackCodecTiming` accessor** (RFC 9559 §5.1.4.1.25 + §5.1.4.1.26):
  `MkvDemuxer::track_codec_timing(stream_index) -> Option<&TrackCodecTiming>`
  (and the per-stream `all_track_codec_timing()` slice) folds the two
  `TrackEntry`-level codec-timing elements — `CodecDelay` (id `0x56AA`,
  §5.1.4.1.25) and `SeekPreRoll` (id `0x56BB`, §5.1.4.1.26), both nanosecond
  (Matroska Tick) `uinteger`s — into one record per track. The elements sit
  directly on `TrackEntry` (no gating master), so every valid track surfaces a
  record; `track_codec_timing` returns `None` only for an out-of-range stream
  index. `codec_delay()` is the encoder's built-in delay (Opus pre-skip) the
  player MUST subtract from each frame timestamp; `seek_pre_roll()` is the
  audio the decoder MUST decode after a seek before its output is valid (Opus
  convention 80 ms). Unlike the `TrackTiming` durations, both elements carry
  spec default `0` and **no** "not 0" range, so an explicit on-disk `0` is a
  legal value distinct from "absent": the plain accessors materialise the `0`
  default while `codec_delay_explicit()` / `seek_pre_roll_explicit()` preserve
  the on-disk presence (a re-muxer can avoid emitting an element the source
  omitted). `TrackCodecTiming::is_empty()` reports the both-absent state — a
  track that emitted an explicit `0` for either element is *not* empty. The
  mux side already writes both on the Opus path (`CodecDelay` = `OpusHead`
  pre-skip in ns, `SeekPreRoll` = 80 ms).
- **Typed `TrackTranslate` accessor** (RFC 9559 §5.1.4.1.27):
  `MkvDemuxer::track_translates(stream_index) -> &[TrackTranslate]` (and the
  per-stream `all_track_translates()` slice) returns each
  `Tracks > TrackEntry > TrackTranslate` master the file carries, in on-disk
  order. `TrackTranslate` maps a track to the value a Chapter Codec (DVD-menu,
  Matroska Script) uses to name it, so a file can be remuxed (acquiring new
  `TrackNumber` / `TrackUID` values) without rewriting the opaque chapter-codec
  command data — only the mapping changes. It is the `TrackEntry`-level twin of
  the `Info > ChapterTranslate` (§5.1.2.8) master already surfaced through
  `segment_linking()`, and is unbounded (a single `TrackEntry` may carry
  several). Each record exposes `track_id` (`TrackTranslateTrackID`,
  §5.1.4.1.27.1 — surfaced verbatim, *not* a Matroska `TrackUID`; its format is
  defined by the chapter codec), `codec` (`TrackTranslateCodec`, §5.1.4.1.27.2 —
  same value space as `ChapProcessCodecID`, Table 31: `0` = Matroska Script,
  `1` = DVD-menu), and the unbounded `edition_uids` list
  (`TrackTranslateEditionUID`, §5.1.4.1.27.3 — an empty list means "all
  editions using the codec"). A master missing a mandatory child is surfaced
  verbatim (empty bytes / `0` codec) so callers can inspect a malformed file,
  mirroring the tolerant `ChapterTranslate` parse. Returns an empty slice for a
  track with no `TrackTranslate` child (the common case) or an out-of-range
  `stream_index`.
- **Typed `TrackLegacy` accessor** (RFC 9559 Appendix A.16..A.27 +
  A.28..A.32): `MkvDemuxer::track_legacy(stream_index) ->
  Option<&TrackLegacy>` (and the per-stream `all_track_legacy()` slice)
  folds the reclaimed `TrackEntry`-level legacy elements — the ones the
  RFC 9559 core body no longer documents but whose Element IDs stay
  reserved in the registry, and which historical Writers still emit — into
  one typed record per track. Three families share the record: the
  codec-description metadata (`CodecSettings` A.19 utf-8, `CodecInfoURL`
  A.20 + `CodecDownloadURL` A.21 string lists, `CodecDecodeAll` A.22
  uinteger — `can_decode_damaged()` predicate), the cache / offset hints
  (`MinCache` A.16, `MaxCache` A.17, signed `TrackOffset` A.18 — a
  Matroska-Tick playback-offset the container surfaces but does not apply),
  the reclaimed Video/Audio-master children (`GammaValue` A.25 + `FrameRate`
  A.26 floats nested in `Video`; `ChannelPositions` A.27 binary nested in
  `Audio` — all surfaced verbatim, none applied), the **ordered**
  `TrackOverlay` fallback list (A.23 — "the order of multiple TrackOverlay
  matters", surfaced verbatim so the preference chain is preserved), and
  the DivXTrickTrack Smooth-FF/RW pairing quintet (`TrickTrackUID` A.28,
  `TrickTrackSegmentUID` A.29, `TrickTrackFlag` A.30 — `is_trick_track()`,
  `TrickMasterTrackUID` A.31, `TrickMasterTrackSegmentUID` A.32). None of
  the appendix entries carries a spec default or range, so every field is a
  pure on-disk projection: absence is always observable (`None` / empty
  `Vec`), a present `0` round-trips as `Some(0)` distinct from omission, and
  an off-length SegmentUID binary is preserved verbatim for inspection. The
  accessor returns `None` (never a hollow record) when the `TrackEntry`
  carried none of these — the common case for a modern file — or for an
  out-of-range `stream_index`; `TrackLegacy::is_empty()` reports the
  all-absent state. The container surfaces these for a faithful re-mux and
  never interprets them. The mux side writes the populated fields via
  `MkvMuxer::set_track_legacy` (see the Muxer section) — a mux→demux
  pipeline round-trips every populated field verbatim.
- **Typed per-Cluster `Position` / `PrevSize` records** (RFC 9559
  §5.1.3.2 / §5.1.3.3): `MkvDemuxer::cluster_records() ->
  &[ClusterRecord]` surfaces each Cluster's optional `Position`
  (id `0xA7`, `uinteger`) and `PrevSize` (id `0xAB`, `uinteger`)
  children as they're walked. Records are appended in first-encounter
  order through `next_packet` / `seek_to`, with `body_offset` (the
  absolute file offset of the byte right after the Cluster's id+size
  header) as the dedup key — a back-then-forward seek that revisits
  the same Cluster doesn't push a duplicate row. Both typed fields
  are `Option<u64>`: `None` when the on-disk child was absent (common
  for `PrevSize` on the first Cluster of a Segment, and for both
  fields when a writer omitted them entirely), `Some(v)` when present.
  The `Some(0)` `Position` case is the §5.1.3.2 spec convention for
  live streams (Cluster offset not determined ahead of time) and is
  distinct from `None`. Consumers can verify a recorded `Position`
  matches the actual on-disk offset by subtracting `segment_data_start`
  + the Cluster's header length from `body_offset` (the §16
  Segment-Position definition), build a reverse walker on top of
  `PrevSize` without re-scanning the SeekHead, or detect a live stream
  by seeing `Some(0)` `Position` values. The slice grows incrementally
  as the demuxer walks the Segment — callers wanting the full
  per-Cluster set should drain the file via `next_packet` first.
- **Typed `SeekHead` accessor** (RFC 9559 §5.1.1, including
  §5.1.1.1..§5.1.1.1.2): `MkvDemuxer::seek_entries() -> &[SeekEntry]`
  surfaces the MetaSeek index — the `SeekHead > Seek` rows that point each
  Top-Level Element to its Segment Position — in document order. The
  demuxer doesn't *navigate* by the SeekHead (it walks Segment children
  directly and seeks via `Cues`), so this is a pure inspection / re-mux
  surface: the one Top-Level master that was CRC-validated but never read
  back. Each `SeekEntry` pairs a `SeekID` (§5.1.1.1.1, the 4-byte binary
  EBML ID of the referenced element) with a `SeekPosition` (§5.1.1.1.2, a
  Segment Position per Section 16 — relative to the first Segment-data
  byte, *not* an absolute file offset). `seek_id() -> Option<u32>` decodes
  the big-endian EBML id for the common case (compare against `ids::CUES`
  / `ids::TRACKS` / …); `seek_id_bytes()` keeps the raw bytes verbatim so
  a `SeekID` referencing an element this build doesn't recognise still
  round-trips through a re-mux. A malformed `Seek` missing its mandatory
  `SeekPosition` (`minOccurs: 1`) is surfaced for inspection with
  `seek_position() == 0` and `has_position() == false` rather than dropped.
  Files using the §6.3 two-`SeekHead` layout (`maxOccurs: 2`, the first
  referencing the second) accumulate both SeekHeads' entries onto the one
  slice in document order. The in-tree muxer's emitted `SeekHead` (Info /
  Tracks / Cues) reads back through this accessor with every
  `SeekPosition + segment_data_start` landing on the matching on-disk
  element header. Returns an empty slice when the file carries no
  `SeekHead` (legal — §6.3 only RECOMMENDS it).
- **Typed `Attachments` accessor** (RFC 9559 §5.1.6):
  `MkvDemuxer::attachments() -> &[Attachment]` returns one
  [`Attachment`] per `AttachedFile` parsed from the Segment, in document
  order. Each entry carries the 1-based `index` (matching the
  `attachment:N:*` flat metadata keys and any `tag:attachment:N:<name>`
  Tag scope), `filename` (`FileName`, §5.1.6.2), `mime_type`
  (`FileMimeType`, §5.1.6.3), `description` (`FileDescription`,
  §5.1.6.1), `uid` (`FileUID`, §5.1.6.5), and the on-disk byte range
  (`data_offset` + `data_size`) of the `FileData` payload. The payload
  bytes are **not** read up front — a multi-megabyte embedded font
  stays on disk until `MkvDemuxer::attachment_data(index)` is called,
  at which point exactly `data_size` bytes are read from `data_offset`
  and returned; the demuxer's reader position is preserved across the
  fetch so calling it between `next_packet` calls is safe. The flat
  `metadata()` view also gains an `attachment:N:description` key when
  the source element was present.
- **Typed `Chapters` accessor** (RFC 9559 §5.1.7):
  `MkvDemuxer::chapters() -> &[Edition]` exposes the structured chapter
  tree the flat `chapter:N:*` metadata view collapses — every
  `EditionEntry` keeps its `EditionUID`, `EditionFlagDefault` and
  `EditionFlagOrdered` flags; every `ChapterAtom` keeps its
  `ChapterUID`, `ChapterStringUID` (e.g. WebVTT cue id), full-precision
  `ChapterTimeStart` / `ChapterTimeEnd` nanoseconds, `ChapterFlagHidden`,
  `ChapterFlagEnabled` (spec default `1` materialised as `true`),
  Medium-Linking fields `ChapterSegmentUUID` (raw 16 B) +
  `ChapterSegmentEditionUID` (zero suppressed per spec "range: not 0"),
  `ChapterPhysicalEquiv` (DVD/SIDE physical mapping per §20.4),
  **all** multilingual `ChapterDisplay` rows (each with `ChapString`,
  `ChapLanguage` + `ChapLanguageBCP47`, `ChapCountry`), the `ChapProcess`
  sub-tree (RFC 9559 §5.1.7.1.4.14–19 — `ChapProcessCodecID`,
  `ChapProcessPrivate`, and zero or more `ChapProcessCommand` rows each
  with `ChapProcessTime` + raw `ChapProcessData`; payloads surfaced
  verbatim, never executed), and any nested
  child atoms (the spec marks `ChapterAtom` as recursive). Atoms are
  1-indexed depth-first in document order — the same index the flat
  `chapter:N:*` keys and `TagChapterUID`-resolved tags use, now extended
  to nested chapters. Returns an empty slice when the file has no
  `Chapters` element.
- Duration: `Segment\Info\Duration` translated to microseconds.
- **Linked-Segment `Info` metadata** (RFC 9559 §5.1.2.1..§5.1.2.8 +
  Section 17) via `MkvDemuxer::segment_linking()` → [`SegmentLinking`]:
  the Segment's own `SegmentUUID`, the previous / next Segment UIDs of a
  Hard-Linked chain (`PrevUUID` / `NextUUID`) with their display
  `*Filename`s, the unbounded `SegmentFamily` UID list, and the
  `ChapterTranslate` sub-tree ([`ChapterTranslate`]: `ChapterTranslateID`
  + `ChapterTranslateCodec` + optional `ChapterTranslateEditionUID` list).
  UID binaries are surfaced verbatim — off-length values round-trip for
  inspection rather than being truncated. `is_empty()` reports the common
  standalone Segment; `is_hard_linked()` reports a chain member. Pure
  container surface: no neighbouring-file resolution.
- Seek: `seek_to(stream, pts)` uses the Cues index. Handles Cues at
  either end of the Segment, and walks an unknown-size final Cluster to
  find Cues that sit past it.
- **`CueRelativePosition` honoured on seek** (RFC 9559 §5.1.5.1.2.3): when
  a Cues entry carries the `CueRelativePosition` element, `seek_to` opens
  the target Cluster, captures its `Timestamp` (RFC 9559 §5.1.3.1 — SHOULD
  be the first child), and then repositions the reader directly at the
  byte offset of the referenced `SimpleBlock` / `BlockGroup` (`0` being
  the first possible element position inside that Cluster). The next
  packet emitted is the cue's exact block, not the first block in the
  Cluster — finer seek granularity than the legacy "scan from cluster
  start" path, which is preserved as a fallback when the cue has no
  `CueRelativePosition` or the encoded position is out of range.
- **`CueBlockNumber` seek fallback** (RFC 9559 §5.1.5.1.2.5): when a Cues
  entry carries `CueBlockNumber` ("Number of the Block in the specified
  Cluster") but no `CueRelativePosition` — common for files indexed by
  older tools — `seek_to` walks the Cluster body counting `SimpleBlock` /
  `BlockGroup` elements and lands the reader on the exact 1-based n-th
  Block instead of scanning from the Cluster start. Out-of-range or
  malformed block numbers degrade gracefully to the cluster-start walk.
- **Typed `Cues` accessor** (RFC 9559 §5.1.5.1, including
  §5.1.5.1.1..§5.1.5.1.2.8 and the reclaimed Appendix A.37..A.39
  `CueReference` children): `MkvDemuxer::cue_points() -> &[CuePoint]`
  surfaces the full on-disk seek-index tree in document order. The
  `seek_to` path consumes a denormalised, sorted projection internally
  (track, time, cluster offset, relative position); `cue_points` instead
  preserves everything that projection collapses, so callers can read
  per-cue `CueDuration` (§5.1.5.1.2.4) and `CueBlockNumber`
  (§5.1.5.1.2.5), the `CueCodecState` (§5.1.5.1.2.6, spec default `0`
  materialised — `0` meaning "taken from the initial `TrackEntry`"), and
  walk the nested `CueReference` rows (§5.1.5.1.2.7 — each carrying
  `CueRefTime` plus the reclaimed `CueRefCluster` / `CueRefNumber` /
  `CueRefCodecState`), or re-mux the `Cues` element sub-element-for-sub-
  element. Each `CuePoint` pairs an absolute `CueTime` (in Segment Ticks
  — the file's `TimestampScale`, not microseconds) with one or more
  `CueTrackPositions` (the spec gives the latter `minOccurs: 1` with no
  `maxOccurs`, so a single timestamp can index blocks on several tracks).
  Populated whether `Cues` sits before the first Cluster or after the
  last (the late best-effort rescan feeds the same typed collector);
  optional children surface as `Option<u64>` (absent vs present), `0`-but-
  present and `0`-by-default `CueCodecState` are observationally identical
  per the spec default. Unknown children inside `CueTrackPositions` are
  skipped (forward-compat). Returns an empty slice when the file has no
  `Cues` element.
- An unknown-size Cluster is terminated cleanly when a sibling Segment-
  child element follows it (no more "Cues silently eaten as payload").
- **CRC-32 validation** (RFC 8794 §11.3.1, RFC 9559 §6.2): when a Top-Level
  master element (`Info`, `Tracks`, `Tags`, `Cues`, `Chapters`,
  `Attachments`, `SeekHead`) **or a `Cluster`** carries a leading `CRC-32`
  child, the demuxer recomputes the IEEE CRC-32 (reflected poly
  `0xEDB88320`, init `0xFFFFFFFF`, final XOR, little-endian storage) over
  the rest of the element and records the result.
  `MkvDemuxer::crc_status() -> &[CrcStatus]` exposes each
  `{element_id, stored, computed}` triple with an `is_valid()` helper.
  Up-front masters are checked at open time in segment order; Cluster
  checks land lazily on the first `next_packet` / `seek_to` that opens
  each Cluster (the element id on a Cluster status is `ids::CLUSTER`),
  with a body-offset dedup so a back-then-forward seek revisiting the
  same Cluster never produces two statuses for it. The late best-effort
  Cues rescan (the path the demuxer uses when `Cues` sits after the
  final `Cluster` — the common single-pass-mux layout, and the one our
  own muxer emits) also validates a leading `CRC-32` on the rediscovered
  `Cues` element and pushes its status, so a Cues CRC mismatch surfaces
  regardless of whether the `Cues` was placed before or after Clusters.
  A Cluster declared with the unknown-size VINT can't be CRC-checked
  (the spec requires a bounded body) and produces no status. Validation
  is informational — a mismatch does **not** abort the open (RFC 8794
  §12: a reader MAY ignore the data); strict callers reject any non-
  valid status. Elements with no `CRC-32` child produce no status
  (omission is spec-legal).
- **`TrackOperation` typed decode** (RFC 9559 §5.1.4.1.30): a *virtual*
  track assembled from other tracks. `MkvDemuxer::track_operation(stream_index)`
  (and the per-stream `track_operations()` slice) returns a typed
  `TrackOperation` for any `TrackEntry` carrying the element, `None` for an
  ordinary track. `TrackCombinePlanes` (§5.1.4.1.30.1) surfaces as a
  `Vec<TrackPlane>` — each pairs a referenced track with its
  `TrackPlaneType` (`LeftEye` / `RightEye` / `Background`, with `Other(u64)`
  preserving FCFS-registry values per §27.17) — and `TrackJoinBlocks`
  (§5.1.4.1.30.5) surfaces as a `Vec<TrackRef>`. Every `TrackPlaneUID` /
  `TrackJoinUID` is resolved back to a `TrackRef` carrying both the on-disk
  `TrackUID` and the matching 0-indexed stream index (`None` for a dangling
  reference, kept rather than dropped). A `TrackPlane` missing its mandatory
  `TrackPlaneUID` and a zero `TrackJoinUID` ("not 0" per spec) are dropped.
- **`BlockAdditionMapping` typed decode** (RFC 9559 §5.1.4.1.17):
  `MkvDemuxer::block_addition_mappings(stream_index)` (and the per-stream
  `all_block_addition_mappings()` slice) returns each
  `Tracks > TrackEntry > BlockAdditionMapping` master the file carries,
  in on-disk order, as a typed `BlockAdditionMapping` record exposing
  `value` (`BlockAddIDValue`, §5.1.4.1.17.1, `Option<u64>` — spec range
  `>=2`, no default), `name` (`BlockAddIDName`, §5.1.4.1.17.2,
  `Option<String>`), `addid_type` (`BlockAddIDType`, §5.1.4.1.17.3,
  `u64` — spec default `0` (codec-defined) materialised), and
  `extra_data` (`BlockAddIDExtraData`, §5.1.4.1.17.4, `Option<Vec<u8>>`
  — opaque per-track binary state the type interpreter consults). The
  helper `is_codec_defined()` reports whether `addid_type == 0` (the
  §5.1.4.1.17.3 usage-note case in which the matching `BlockAddID` must
  be `1`). Unknown child elements inside the master are skipped — the
  spec allows additions to the registry. Tracks with no
  `BlockAdditionMapping` child surface as an empty slice (the common
  case — the element only appears on tracks that use `BlockAdditional`
  to extend their on-disk format). The typed view declares the *shape*
  of the side channel; the per-frame `BlockAdditional` payload bytes
  themselves surface through the per-packet `block_additions()`
  accessor below, and payload semantics stay with the codec /
  track-format extension that owns each `BlockAddIDType` value.
- **Per-Block `BlockAdditions` typed decode** (RFC 9559 §5.1.3.5.2,
  including §5.1.3.5.2.1..§5.1.3.5.2.3) **+ `MaxBlockAdditionID`**
  (§5.1.4.1.16): `MkvDemuxer::block_additions() -> &[BlockAddition]`
  surfaces the side-channel payloads attached to the most recently
  returned packet — one typed `BlockAddition` per `BlockMore` in
  on-disk order, each pairing `block_add_id()` (`BlockAddID`,
  §5.1.3.5.2.3, spec default `1` = codec-defined materialised on
  omission) with the verbatim `data()` bytes (`BlockAdditional`,
  §5.1.3.5.2.2, never interpreted by the container — id `1` is e.g.
  the WebM alpha plane when the track's `AlphaMode` is `Present`; ids
  `>= 2` are described by the track's `BlockAdditionMapping`). The
  slice is empty for `SimpleBlock` packets (the element only exists on
  `BlockGroup`), for `BlockGroup`s without the master (the common
  case), before the first `next_packet`, and after a seek; every frame
  de-laced from one laced Block shares the Block's additions (the spec
  attaches the master to the Block as a whole). Malformed `BlockMore`s
  are dropped: a missing mandatory `BlockAdditional`, a `BlockAddID`
  of `0` (range "not 0"), and a duplicate `BlockAddID` (uniqueness
  MUST — first occurrence kept). The per-track declaration surfaces
  through `MkvDemuxer::max_block_addition_id(stream_index)` with the
  §5.1.4.1.16 spec default `0` ("there is no BlockAdditions for this
  track") materialised on absence.
- **`ContentEncodings` typed decode** (RFC 9559 §5.1.4.1.31):
  `MkvDemuxer::content_encodings(stream_index)` (and the per-stream
  `all_content_encodings()` slice) returns the track's transformation chain
  — compression and/or encryption applied to frame data / `CodecPrivate`
  before the bytes hit Blocks — as typed `ContentEncodings`, `None` for an
  ordinary track. Each `ContentEncoding` carries its `ContentEncodingOrder`,
  `ContentEncodingScope` bit field (`block()` / `private()` / `next()`
  accessors), and a `ContentEncodingTransform` enum: `Compression`
  (`ContentCompAlgo` → `Zlib` / `Bzlib` / `Lzo1x` / `HeaderStripping` /
  `Other(u64)`, plus the `ContentCompSettings` stripped bytes) or
  `Encryption` (`ContentEncAlgo` → `None` / `Des` / `TripleDes` / `Twofish`
  / `Blowfish` / `Aes` / `Other(u64)`, the `ContentEncKeyID`, the
  nested `ContentEncAESSettings` → `AESSettingsCipherMode` as
  `Ctr` / `Cbc` / `Other(u64)`, and the reclaimed content-signing quartet
  (RFC 9559 Appendix A.33..A.36) as a typed `ContentSigning` record —
  `ContentSignature` (id `0x47E3`, binary) + `ContentSigKeyID` (`0x47E4`,
  binary) + `ContentSigAlgo` (`0x47E5`, uinteger) + `ContentSigHashAlgo`
  (`0x47E6`, uinteger). Each signing field is an `Option` whose `None` means
  "absent on disk" — the appendix names no values and no defaults, so a
  present `0` round-trips as `Some(0)` distinct from absence, mirroring the
  reclaimed Appendix-A `AspectRatioType` raw-value rule;
  `ContentSigning::is_empty()` reports the all-absent state). The list is
  pre-sorted into *decode* order
  (highest `ContentEncodingOrder` first, per §5.1.4.1.31.2). Element
  defaults are honoured (order 0, scope 0x1 Block, type 0 compression,
  comp-algo 0 zlib). The headers are surfaced; zlib/bzlib/lzo1x and
  encryption are never decompressed or decrypted (out of container scope).
- **Header-Stripping applied on read** (RFC 9559 §5.1.4.1.31.6 algo 3,
  §5.1.4.1.31.7): Header Stripping is the one `ContentEncoding` transform
  the container can reverse without a codec — the `ContentCompSettings`
  bytes were removed from the front of each frame on write, so the demuxer
  prepends them back to every de-laced frame, and `next_packet` returns the
  original (un-stripped) frame data. Block scope (§5.1.4.1.31.3 bit 0x1) is
  honoured per-frame (the prefix lands on each laced sub-frame, not the
  Block once); a chain of several Header-Stripping steps is combined in
  decode order. If the Block-scoped chain contains any step the container
  can't undo (zlib/bzlib/lzo1x compression or encryption), packets pass
  through encoded — the demuxer never *partially* strips. Private-scope
  (`CodecPrivate`-only) Header Stripping leaves frame data untouched.
- **`Video` geometry quartet typed decode** (RFC 9559
  §5.1.4.1.28.8..§5.1.4.1.28.14):
  `MkvDemuxer::video_geometry(stream_index)` (and the per-stream
  `video_geometries()` slice) folds the `PixelCrop{Top,Bottom,Left,Right}`
  hide-window plus the `DisplayWidth` / `DisplayHeight` / `DisplayUnit`
  render-size triple into a single typed `VideoGeometry`. `DisplayUnit`
  surfaces as the `DisplayUnit` enum (`Pixels` / `Centimeters` / `Inches` /
  `DisplayAspectRatio` / `Unknown` / `Other(u64)` for forward-compat with
  the §27.9 "Matroska Display Units" registry). `display_width()` /
  `display_height()` return `Option<u64>`: the explicit element when the
  file carries it, otherwise the §5.1.4.1.28.12 / §5.1.4.1.28.13 derived
  default (`PixelWidth - PixelCropLeft - PixelCropRight` / `PixelHeight -
  PixelCropTop - PixelCropBottom`) — but only when `DisplayUnit == 0`
  (pixels), since the spec explicitly states "If the DisplayUnit of the
  same TrackEntry is 0, then the default value for DisplayWidth is ...;
  else, there is no default value". For any other `DisplayUnit` an absent
  element resolves to `None`. The PixelCrop defaults (`0`, §5.1.4.1.28.8..11)
  and DisplayUnit default (`0`, §5.1.4.1.28.14) are always materialised.
  Non-video tracks (and video tracks with no `Video` master) return `None`;
  a derivation that would underflow (malformed file with crops larger than
  the encoded width or height on the same axis) returns `None` on that
  axis rather than wrapping.
- **`Video > Colour` typed decode** (RFC 9559 §5.1.4.1.28.16, including
  §5.1.4.1.28.17..§5.1.4.1.28.40 sub-elements and the SMPTE 2086 /
  CTA-861.3 HDR `MasteringMetadata`):
  `MkvDemuxer::video_colour(stream_index)` (and the per-stream
  `video_colours()` slice) folds the `Colour` master's children into a
  single typed `VideoColour`. Each of `MatrixCoefficients`,
  `TransferCharacteristics`, `Primaries`, `ColourRange`,
  `ChromaSitingHorz` and `ChromaSitingVert` surfaces as a typed enum;
  forward-compat values outside the registered tables pass through
  via an `Other(u64)` variant (§27 leaves registries open for future
  additions). `BitsPerChannel`, `ChromaSubsampling{Horz,Vert}`,
  `CbSubsampling{Horz,Vert}`, `MaxCLL` / `MaxFALL` surface as the raw
  unsigned integer (Optional when the spec doesn't define a default).
  The nested `MasteringMetadata` (§5.1.4.1.28.30..§5.1.4.1.28.40)
  surfaces as `Option<&MasteringMetadata>` with the six
  `Primary{R,G,B}Chromaticity{X,Y}` floats, the two
  `WhitePointChromaticity{X,Y}` floats and the
  `Luminance{Max,Min}` cd/m² pair — each independently optional, since
  the spec does not require all-or-nothing. Spec defaults are
  materialised on the typed surface so an empty `Colour` master decodes
  as fully-typed *unspecified* (§5.1.4.1.28.17 / .26 / .27 default `2`;
  §5.1.4.1.28.23..25 default `0`). Non-video tracks (and video tracks
  with no `Colour` child) return `None`.
- **`Video > StereoMode` typed decode** (RFC 9559 §5.1.4.1.28.3):
  `MkvDemuxer::video_stereo_mode(stream_index) -> Option<StereoMode>`
  (and the per-stream `video_stereo_modes()` slice) returns the
  single-track stereo-3D packing — `Mono` / `SideBySide{Left,Right}First`
  / `TopBottom{Left,Right}First` / `Checkboard{Left,Right}First` /
  `RowInterleaved{Left,Right}First` /
  `ColumnInterleaved{Left,Right}First` / `Anaglyph{CyanRed,GreenMagenta}`
  / `BothEyesLaced{Left,Right}First` (the full §5.1.4.1.28.3 Table 5
  set) plus `Other(u64)` for values registered after RFC 9559 (§27.7
  leaves the registry open). The §5.1.4.1.28.3 default `0` (`Mono`) is
  materialised: a `Video` master with no explicit `StereoMode` decodes
  as `Some(StereoMode::Mono)`, distinguishable from `None` (which means
  "no `Video` master at all"). Multi-track stereo (`TrackOperation >
  TrackCombinePlanes`, §5.1.4.1.30.1) is independent and surfaces
  through `track_operation`; a single track MAY carry both. A convenience
  `StereoMode::is_stereo()` returns `true` for any non-`Mono` packing.
- **`Video > OldStereoMode` typed decode** (RFC 9559 §5.1.4.1.28.5, id
  `0x53B9`, `maxver 2`): `MkvDemuxer::video_old_stereo_mode(stream_index) ->
  Option<OldStereoMode>` (and the per-stream `video_old_stereo_modes()` slice)
  surfaces the "bogus" stereo-3D mode value that [libmatroska] prior to 0.9.0
  wrote at the *wrong* Element ID (`0x53B9` instead of `0x53B8`, §18.10). The
  spec marks the element `maxver 2` and says a Writer MUST NOT use it, but a
  Reader MAY support legacy files by reading it — which this accessor does. Its
  value space (Table 7) is **incompatible** with the modern `StereoMode`
  (Table 5): only `Mono` (`0`) / `RightEye` (`1`) / `LeftEye` (`2`) /
  `BothEyes` (`3`) appear here, plus `Other(u64)` for any out-of-Table-7 value,
  so the surface is deliberately kept **separate** from `video_stereo_mode`.
  Unlike `StereoMode`, **no** spec default is materialised — a modern file with
  no `OldStereoMode` element returns `None`, never a synthesised `Mono`; a
  track MAY carry both (a transitional file aware of the libmatroska bug), and
  the two surfaces report independently. `OldStereoMode::is_stereo()` /
  `to_raw()` mirror the `StereoMode` helpers. This closes the last RFC 9559
  element-registry entry the crate had not yet handled.
- **`Video > Projection` typed decode** (RFC 9559 §5.1.4.1.28.41,
  including §5.1.4.1.28.42..§5.1.4.1.28.46):
  `MkvDemuxer::video_projection(stream_index)` (and the per-stream
  `video_projections()` slice) folds the `Projection` master's children
  into a single typed `Projection`. `ProjectionType` surfaces as a typed
  enum (`Rectangular` / `Equirectangular` / `Cubemap` / `Mesh` /
  `Other(u64)` for values registered after RFC 9559 — §27.15 leaves the
  registry open). `ProjectionPrivate` (the verbatim ISOBMFF box body —
  `equi` / `cbmp` / `mshp` — that pairs with the projection type)
  surfaces verbatim as `Option<&[u8]>` and is never parsed or validated
  by the container; that's a renderer concern. The yaw / pitch / roll
  pose triple (degrees, ranges `±180 / ±90 / ±180` per §5.1.4.1.28.44..46)
  surfaces as three `f64`s with the spec default `0.0` materialised. An
  empty `Projection` master decodes as a fully-typed identity projection
  (rectangular + zero pose), distinguishable from `None` (which means
  "no `Projection` master at all" — the common case for ordinary 2D
  video). The §5.1.4.1.28.46 worked example
  `<Projection><ProjectionPoseRoll>90</ProjectionPoseRoll></Projection>`
  (signalling a 90° counter-clockwise rotation) round-trips with
  `projection_type == Rectangular`, `pose_roll == 90.0`, and the other
  pose components at their defaults. Convenience helpers
  `ProjectionType::is_spherical()` and `Projection::is_rotated()` provide
  the headline yes/no answers. Non-video tracks (and video tracks with no
  `Projection` child) return `None`.
- **`Video > AlphaMode` typed decode** (RFC 9559 §5.1.4.1.28.4):
  `MkvDemuxer::video_alpha_mode(stream_index) -> Option<AlphaMode>`
  (and the per-stream `video_alpha_modes()` slice) folds the per-track
  WebM-alpha hint into a typed enum (`None` / `Present` / `Other(u64)`
  for values registered after RFC 9559 — §27.8 leaves the registry
  open). The §5.1.4.1.28.4 default `0` (`None`) is materialised: a
  `Video` master with no explicit `AlphaMode` decodes as
  `Some(AlphaMode::None)`, distinguishable from `None` (which means "no
  `Video` master at all"). `AlphaMode::Present` (value `1`) signals
  that the track's `BlockAdditional` element with `BlockAddID=1` carries
  alpha-channel data per the codec mapping for `CodecID` (the WebM
  VP8/VP9 alpha extension is the canonical user). A convenience
  `AlphaMode::has_alpha()` returns `true` exactly for the `Present`
  variant — values outside Table 6 are conservatively treated as "no
  alpha" because the spec leaves their semantics implementation-defined.
- **`Video > AspectRatioType` typed decode** (RFC 9559 Appendix A.24,
  reclaimed): `MkvDemuxer::video_aspect_ratio_type(stream_index) ->
  Option<u64>` (and the per-stream `video_aspect_ratio_types()` slice)
  surfaces the raw `u64` value rather than synthesising an enum — the
  reclaimed appendix says only "Specifies the possible modifications to
  the aspect ratio" and enumerates no values. Returns `None` whenever
  the file did not carry the element (the appendix specifies no
  default, so absence is *not* materialised).
- **`Video > UncompressedFourCC` typed decode** (RFC 9559
  §5.1.4.1.28.15): `MkvDemuxer::video_uncompressed_fourcc(stream_index)
  -> Option<&UncompressedFourCC>` (and the per-stream
  `video_uncompressed_fourccs()` slice) surfaces the 4-byte FourCC that
  identifies the uncompressed pixel layout. Spec-mandatory only when
  `CodecID == "V_UNCOMPRESSED"` (Table 11); the typed surface carries
  the verbatim on-disk bytes via `as_bytes()`, plus convenience
  `fourcc() -> Option<[u8; 4]>` and `as_str() -> Option<String>` (UTF-8
  lossy) accessors that return `None` whenever the on-disk payload
  isn't exactly 4 bytes. A malformed non-4-byte payload is preserved
  verbatim rather than being dropped, so callers debugging a malformed
  file can still see what the writer emitted. Absence on any track is
  legal — the spec specifies no default — and returns `None`.
- **`Video > FlagInterlaced` + `FieldOrder` typed decode** (RFC 9559
  §5.1.4.1.28.1 + §5.1.4.1.28.2):
  `MkvDemuxer::video_interlacing(stream_index)` (and the per-stream
  `video_interlacings()` slice) folds both elements into a typed
  `VideoInterlacing` — `flag()` returns a `FlagInterlaced` enum
  (`Undetermined` / `Interlaced` / `Progressive` / `Other(u64)`) and
  `field_order()` returns `Some(FieldOrder)`
  (`Progressive` / `Tff` / `Undetermined` / `Bff` / `TffInterleaved` /
  `BffInterleaved` / `Other(u64)`) only when the track is actually
  interlaced. §5.1.4.1.28.2's "If FlagInterlaced is not set to 1, this
  element MUST be ignored" is honoured by the typed surface: a stray
  `FieldOrder` on a progressive / undetermined track silently resolves to
  `None`. Spec defaults materialised — bare `Video` master with no
  `FlagInterlaced` child decodes as `Undetermined` (default `0`); an
  interlaced track with no explicit `FieldOrder` decodes as
  `Some(FieldOrder::Undetermined)` (default `2`). Non-video tracks (and
  video tracks with no `Video` master) return `None`.

### Muxer (`mux::open` and `mux::open_webm`)

- EBML header + Segment (unknown size) for a streaming-friendly layout.
- **`DocTypeExtension` on write** (RFC 8794 §11.2.9..§11.2.11):
  `MkvMuxer::set_doc_type_extensions(Vec<DocTypeExtension>)` queues the
  EBML-header extension declarations, emitted after `DocTypeReadVersion` in
  the header at `write_header` time. Takes the **same** demux-side
  `DocTypeExtension` record `MkvDemuxer::ebml_header` surfaces, so a
  header→header copy round-trips every extension verbatim. Queue-time
  validation rejects post-`write_header` use, an empty `DocTypeExtensionName`
  (§11.2.10 length `>0`), a zero `DocTypeExtensionVersion` (§11.2.11 "not 0"),
  and a duplicate name (§11.2.10 "MUST be unique within the EBML Header").
  Omitting the call (the default) keeps the header free of extensions — the
  common case. Pairs symmetrically with `MkvDemuxer::ebml_header`.
- Fixed-size `SeekHead` at the start of the Segment with Seek entries
  for `Info`, `Tracks`, and `Cues` - so players that pre-walk the
  SeekHead (mpv, Chromium) jump straight to Cues without scanning. The
  Cues `SeekPosition` is patched in `write_trailer`; if no packets were
  written, the Cues entry is rewritten as a Void filler.
- `Info` (1 ms `TimecodeScale`), `Tracks`, rolling ~5 s `Cluster`s with
  `SimpleBlock` payload.
- `Cues` element emitted in `write_trailer` - index entries for every
  video keyframe and every audio cluster-start, so the resulting file
  is seekable without a second pass. Each entry carries the full
  `CueTrackPositions` sub-tree, symmetric with the demux read surface:
  `CueRelativePosition` (RFC 9559 §5.1.5.1.2.3, recommended by §22.1)
  so seek-aware readers jump straight to the indexed `SimpleBlock`
  inside the Cluster; `CueBlockNumber` (§5.1.5.1.2.5) — the 1-based
  ordinal of the indexed Block within its Cluster (every block counted
  across all tracks in write order, `range: not 0` honoured); and
  `CueDuration` (§5.1.5.1.2.4) when the indexed packet carried a usable
  duration. **Subtitle tracks** follow §22.1's stronger recommendation
  ("each subtitle frame SHOULD be referenced by a CuePoint element with
  a CueDuration element"): a `MediaType::Subtitle` track is indexed once
  per *frame* (each carrying its `CueDuration`), while audio/video keep
  the once-per-cluster cadence.
- Codec-specific fields: `CodecPrivate` normalisation for FLAC (`fLaC`
  magic prepended), Opus `CodecDelay` derived from the `OpusHead`
  pre-skip plus an 80 ms `SeekPreRoll` per the WebM spec.
- `Chapters` (RFC 9559 §5.1.7): `MkvMuxer::add_chapter(start_ns,
  end_ns, title)` queues a single English-language `ChapterAtom`;
  `add_chapter_full(MkvChapter)` takes a fully-specified record with
  multilingual `ChapterDisplay` rows (`ChapString` + `ChapLanguage`
  + optional `ChapCountry`) plus the complete `ChapterAtom` field set —
  `ChapterUID` (§5.1.7.1.4.1, auto-derived non-zero when `None`),
  `ChapterStringUID` (.2), `ChapterFlagHidden` (.5), `ChapterFlagEnabled`
  (default `1`, written only when cleared), `ChapterSegmentUUID` (.6,
  16-byte), `ChapterSegmentEditionUID` (.7), `ChapterPhysicalEquiv` (.8),
  and the `ChapProcess > ChapProcessCommand` chapter-codec command tree
  (§5.1.7.1.4.14–19) via `MkvChapProcess` / `MkvChapProcessCommand`
  (write-side mirror of the demux `ChapProcess` surface). `add_chapter_full`
  rejects `ChapterUID 0` / `ChapterSegmentEditionUID 0` (range "not 0") and
  a non-16-byte `ChapterSegmentUUID`. Chapters must be added before
  `write_header`; the muxer emits a single `EditionEntry` between
  Tracks and the first Cluster and patches the SeekHead `Chapters`
  slot to point at it (slot is voided if no chapters were queued).
- `Attachments` (RFC 9559 §5.1.6): `MkvMuxer::add_attachment(MkvAttachment
  { filename, mime_type, data, uid, description })` queues one
  `AttachedFile`. Attachments must be added before `write_header`; the
  muxer emits the `Attachments` master right after `Chapters` (or
  directly after `Tracks` when no chapters are queued) and patches the
  SeekHead `Attachments` slot to point at it (slot is voided if no
  attachments were queued). Field handling matches the demux side
  field-for-field so an end-to-end demux→mux pipeline preserves
  attachments: `FileName` (§5.1.6.1.2) + `FileMediaType` (§5.1.6.1.3)
  are mandatory and rejected up front when empty; `FileUID` (§5.1.6.1.5,
  `range: not 0`) auto-derives from the 1-based attachment index when
  the caller passes `None`, and an explicit `Some(0)` is rejected;
  `FileDescription` (§5.1.6.1.1) is omitted on disk when `None` or
  empty. `MkvAttachment::new(filename, mime_type, data)` is a
  convenience constructor mirroring the demux-side typed surface.
- `Tags` (RFC 9559 §5.1.8): `MkvMuxer::add_tag(MkvTag)` queues one `Tag`
  per call, emitted as the file's single `Tags` master (§5.1.8,
  `maxOccurs: 1`) sandwiched after `Attachments` and before the first
  `Cluster` so the demuxer's single-pass header walk catches it. Tags
  must be added before `write_header`; the SeekHead grew a sixth slot
  (`Tags`, between `Attachments` and `Cues`) that is patched to the
  emitted master's offset or voided when no tags were queued, and the
  master carries a leading `CRC-32` child like the other Top-Level
  masters (§6.2). `MkvTag` pairs an `MkvTagTargets` scope with one or
  more `MkvSimpleTag` `(name, value)` descriptors — the write-side mirror
  of the demux `tags()` surface, field-for-field, so a demux→mux pipeline
  preserves tags. `MkvTagTargets` carries `TargetTypeValue`
  (§5.1.8.1.1.1, omitted when `None`, `Some(0)` rejected per `range: not
  0`), `TargetType` (§5.1.8.1.1.2, informational), and the four
  `TagTrackUID` / `TagEditionUID` / `TagChapterUID` / `TagAttachmentUID`
  lists (§5.1.8.1.1.3..§5.1.8.1.1.6 — multi-UID scoping, a `0` UID
  dropped since "all of that kind" is expressed by omission). The muxer
  assigns each track `TrackUID == track_number` (1-based), so a tag
  scoped via `MkvTagTargets::track(N)` resolves back to stream index
  `N - 1` on read. `MkvSimpleTag` carries `TagName` (§5.1.8.1.2.1,
  mandatory), `TagLanguage` (§5.1.8.1.2.2, default `"und"` left
  off-disk), `TagLanguageBCP47` (§5.1.8.1.2.3, written instead of
  `TagLanguage` when present per the spec's "MUST be ignored" rule),
  `TagDefault` (§5.1.8.1.2.4, default `1` written only when cleared), a
  `TagString` (§5.1.8.1.2.5) / `TagBinary` (§5.1.8.1.2.6) payload enum
  (`MkvSimpleTagValue`), and a `children` list for the spec's
  `recursive: True` nesting — symmetric with the demux-side
  `SimpleTag::children`. Queue-time validation rejects an empty
  `simple_tags` list (§5.1.8.1.2 `minOccurs: 1`), an empty `TagName` at
  any nesting depth (§5.1.8.1.2.1), `TargetTypeValue 0`, and any call
  after `write_header`. Convenience constructors: `MkvTag::global(name,
  value)`, `MkvTagTargets::track(uid)`, `MkvSimpleTag::new(name, value)`
  / `MkvSimpleTag::binary(name, data)`.
- WebM profile: `mux::open_webm` pins `DocType="webm"` and rejects any
  stream whose codec isn't VP8/VP9/AV1 video or Vorbis/Opus audio with
  `Error::Unsupported`.
- **CRC-32 on Top-Level masters** (RFC 8794 §11.3.1, RFC 9559 §6.2):
  the muxer prepends a 6-byte `CRC-32` child (id `0xBF`, fixed size 4,
  little-endian IEEE CRC-32 of the rest of the element's data) to every
  Top-Level master it buffers end-to-end before flushing — `Info`,
  `Tracks`, `Cues`, plus `Chapters` and `Attachments` when those are
  queued. RFC 9559 §6.2 says "all Top-Level Elements of an EBML Document
  SHOULD include a CRC-32 element as their first Child Element," and the
  in-tree demuxer's `validate_top_level_crc` peel-off-leading-CRC rule
  verifies every emitted master round-trips to a matching stored /
  computed pair. `SeekHead` is deliberately not CRC'd — its Cues entry
  is patched in `write_trailer`, which would invalidate any CRC computed
  up front. `Cluster` is not CRC'd because the muxer streams Clusters
  with the unknown-size VINT and RFC 8794 §11.3.1 requires a bounded
  body for CRC.
- **`Video > FlagInterlaced` + `FieldOrder` on write** (RFC 9559
  §5.1.4.1.28.1 + §5.1.4.1.28.2): `MkvMuxer::set_video_interlacing(
  stream_index, FlagInterlaced, Option<FieldOrder>)` queues a per-track
  interlacing hint that lands inside the track's `Video` master at
  `write_header` time, alongside the existing `PixelWidth` /
  `PixelHeight`. The demux-side `FlagInterlaced` / `FieldOrder` enums
  gained `to_raw()` inverses so every Table 3 / Table 4 value
  round-trips, including the `Other(u64)` forward-compat variant on
  both. Spec rules enforced at queue time: the call rejects
  post-`write_header` use, out-of-range `stream_index`, non-video
  tracks, and `FieldOrder` paired with anything other than
  `FlagInterlaced::Interlaced` (the §5.1.4.1.28.2 "If FlagInterlaced is
  not set to 1, this element MUST be ignored" rule applied
  symmetrically on write). Omitting the call leaves both elements
  off-disk so the demuxer materialises the §5.1.4.1.28.1 default `0` /
  §5.1.4.1.28.2 default `2` (Undetermined). Pairs symmetrically with
  the existing `MkvDemuxer::video_interlacing` typed accessor — a
  mux→demux pipeline preserves the interlacing pair bit-exactly.
- **`Video` geometry quartet on write** (RFC 9559
  §5.1.4.1.28.8..§5.1.4.1.28.14):
  `MkvMuxer::set_video_geometry(stream_index, MkvVideoGeometry)` queues a
  per-track hint that lands inside the track's `Video` master at
  `write_header` time, alongside `PixelWidth` / `PixelHeight`. The hint
  carries `PixelCrop{Top,Bottom,Left,Right}` (§5.1.4.1.28.8..11),
  `DisplayWidth` / `DisplayHeight` (§5.1.4.1.28.12 / .13), and
  `DisplayUnit` (§5.1.4.1.28.14). The demux-side `DisplayUnit` enum
  gained a `to_raw()` inverse so every Table 10 value round-trips,
  including the `Other(u64)` forward-compat variant (§27.9 leaves the
  "Matroska Display Units" registry open). Per-element omission rules:
  zero crops stay off-disk (spec default `0`); `DisplayWidth` /
  `DisplayHeight` are written when `Some` and skipped when `None`;
  `DisplayUnit` is written explicitly only for non-`Pixels` values
  (omitting it lets the demuxer materialise the §5.1.4.1.28.14 spec
  default). Spec rules enforced at queue time: rejects post-`write_header`
  use, out-of-range `stream_index`, calls on non-video tracks, and
  `Some(0)` on either `display_width` / `display_height` per the
  §5.1.4.1.28.12 / .13 `range: not 0` pin. Convenience constructors
  `MkvVideoGeometry::cropped(top, bottom, left, right)` (RFC 9559 §11.1
  pillar-box / letterbox shape, no display-size override, `Pixels` unit)
  and `MkvVideoGeometry::aspect_ratio(num, den)`
  (`DisplayUnit::DisplayAspectRatio` + the ratio encoded as
  `DisplayWidth` / `DisplayHeight`) cover the two common shapes. Pairs
  symmetrically with the existing `MkvDemuxer::video_geometry` typed
  accessor — a mux→demux pipeline preserves the quartet bit-exactly,
  including the §5.1.4.1.28.12 / .13 derived-default behaviour when
  display dimensions were omitted on write and `DisplayUnit == Pixels`.
- **`Video > StereoMode` + `AlphaMode` on write** (RFC 9559
  §5.1.4.1.28.3 + §5.1.4.1.28.4):
  `MkvMuxer::set_video_stereo_mode(stream_index, StereoMode)` and
  `MkvMuxer::set_video_alpha_mode(stream_index, AlphaMode)` queue
  per-track hints that land inside the track's `Video` master at
  `write_header` time. The demux-side `StereoMode` and `AlphaMode`
  enums gained `to_raw()` inverses so every Table 5 / Table 6 value
  round-trips, including the `Other(u64)` forward-compat variant on
  both (§27.7 / §27.8 leave the "Matroska Stereo Modes" / "Matroska
  Alpha Modes" registries open). Spec rules enforced at queue time:
  both setters reject post-`write_header` use, out-of-range
  `stream_index`, and calls on non-video tracks. The two settings are
  independent — setting one does not affect the other. Omitting the
  call leaves the element off-disk so the demuxer materialises the
  §5.1.4.1.28.3 default `0` (`Mono`) / §5.1.4.1.28.4 default `0`
  (`None`). Calling `set_video_stereo_mode(_, StereoMode::Mono)` /
  `set_video_alpha_mode(_, AlphaMode::None)` explicitly still writes
  the element on disk — that is the way for a producer to override a
  downstream tool that might infer something else. Pairs symmetrically
  with the existing `MkvDemuxer::video_stereo_mode` /
  `MkvDemuxer::video_alpha_mode` typed accessors.
- **`Video > OldStereoMode` on write** (RFC 9559 §5.1.4.1.28.5, id `0x53B9`):
  `MkvMuxer::set_video_old_stereo_mode(stream_index, OldStereoMode)` queues the
  legacy libmatroska-bug element, written inside the track's `Video` master at
  `write_header` time. This is a **legacy / re-mux-only** surface: the spec
  marks the element `maxver 2` and says a Writer MUST NOT use it for new files
  (modern stereo-3D belongs in `set_video_stereo_mode` or
  `set_track_operation`). It exists solely so a faithful re-mux of a Matroska
  v2 / libmatroska-bug source can round-trip the element the demuxer surfaced
  through `video_old_stereo_mode`. `OldStereoMode::to_raw()` round-trips every
  Table 7 value plus the `Other(u64)` forward-compat variant. Omitting the call
  (the default) keeps the element off-disk — the correct behaviour for every
  modern file. Spec rules enforced at queue time: rejects post-`write_header`
  use, out-of-range `stream_index`, and calls on non-video tracks. Pairs
  symmetrically with `MkvDemuxer::video_old_stereo_mode` — a mux→demux pipeline
  preserves the legacy value bit-exactly. With this, **every element in the
  RFC 9559 element-ID registry is now both read and written**.
- **`Video > UncompressedFourCC` on write** (RFC 9559 §5.1.4.1.28.15):
  `MkvMuxer::set_video_uncompressed_fourcc(stream_index, [u8; 4])`
  queues a per-track FourCC hint that lands inside the track's
  `Video` master at `write_header` time (id `0x2EB524`, `binary`
  type, schema-fixed `length: 4`). The setter takes a `[u8; 4]` array
  directly, so the schema's fixed length is enforced at the type
  system; every byte (including high bytes and `0x00`) is written
  verbatim — the element is `binary`, not `string`, and the muxer
  never interprets the payload as text. Spec rules enforced at queue
  time: the setter rejects post-`write_header` use, out-of-range
  `stream_index`, and calls on non-video tracks. Omitting the call
  leaves the element off-disk so the demuxer's
  `MkvDemuxer::video_uncompressed_fourcc` surfaces `None` —
  §5.1.4.1.28.15 defines no default, and Table 11's `minOccurs=1`
  only fires for `CodecID == "V_UNCOMPRESSED"`, which the muxer does
  not presently emit. Pairs symmetrically with the existing
  `MkvDemuxer::video_uncompressed_fourcc` typed accessor — a
  mux→demux pipeline preserves the four-byte FourCC bit-exactly.
- **`Video > AspectRatioType` on write** (RFC 9559 Appendix A.24,
  reclaimed, id `0x54B3`): `MkvMuxer::set_video_aspect_ratio_type(
  stream_index, u64)` queues a per-track hint that lands inside the
  track's `Video` master at `write_header` time as a plain `uinteger`
  element. The reclaimed appendix documents the element only as
  "Specifies the possible modifications to the aspect ratio" and
  enumerates no values and no default, so the setter takes the raw
  `u64` verbatim — mirroring the demux side, which deliberately
  surfaces it as a raw `Option<u64>` rather than a synthesised enum.
  Per-element omission rule: the element is written only when the
  caller opts in; an explicit `0` is written and round-trips as
  `Some(0)` (distinct from absence, since the appendix defines no
  default). Spec rules enforced at queue time: the setter rejects
  post-`write_header` use, out-of-range `stream_index`, and calls on
  non-video tracks. Omitting the call leaves the element off-disk so
  the demuxer's `MkvDemuxer::video_aspect_ratio_type` surfaces `None`.
  Pairs symmetrically with the existing
  `MkvDemuxer::video_aspect_ratio_type` typed accessor — a mux→demux
  pipeline preserves the raw value bit-exactly. This closes the last
  remaining `Video` sub-element that the demux side read but the mux
  side could not write.
- **`Video > Colour` scalar children on write** (RFC 9559
  §5.1.4.1.28.16, §5.1.4.1.28.17..§5.1.4.1.28.29):
  `MkvMuxer::set_video_colour(stream_index, MkvVideoColour)` queues a
  per-track colour-description hint that lands inside the track's
  `Video` master at `write_header` time as a `Colour` master (id
  `0x55B0`) carrying the eleven scalar children: `MatrixCoefficients`
  / `BitsPerChannel` / `ChromaSubsampling{Horz,Vert}` /
  `CbSubsampling{Horz,Vert}` / `ChromaSiting{Horz,Vert}` / `Range` /
  `TransferCharacteristics` / `Primaries` / `MaxCLL` / `MaxFALL`.
  Convenience constructors `MkvVideoColour::bt709()` (matrix `1` /
  transfer `1` / primaries `1` / broadcast range — the canonical SDR
  HD shape) and `MkvVideoColour::bt2020_pq()` (matrix `9` / transfer
  `16` / primaries `9` / full range / 10 bpc — the canonical HDR10
  shape) cover the two everyday cases; every field can be overridden
  on the returned value for one-off departures. Per-element omission
  rules apply at write time: every scalar that equals its
  §5.1.4.1.28 spec default is left off-disk so the demuxer
  materialises the spec default; every `Option<u64>` (the four
  chroma-subsampling integers + `MaxCLL` / `MaxFALL`) is written
  when `Some(v)` and skipped when `None`. As a result, queueing
  `MkvVideoColour::default()` writes an empty 3-byte `Colour` master
  (id `0x55B0` + size VINT `0x80`), which the demuxer parses into
  `Some(VideoColour::default())` with every getter returning the
  materialised spec default — distinguishable on disk from the
  call-was-omitted case, which keeps the `Colour` master off-disk
  entirely so the demuxer surfaces `None` from `video_colour`. Spec
  rules enforced at queue time: the setter rejects post-`write_header`
  use, out-of-range `stream_index`, and calls on non-video tracks.
  The `Colour > MasteringMetadata` sub-master
  (§5.1.4.1.28.30..§5.1.4.1.28.40, id `0x55D0`) is emitted whenever
  the queued hint carries `mastering_metadata: Some(MkvMasteringMetadata)`;
  inside that master each chromaticity / luminance child
  (`PrimaryRChromaticityX/Y` / `PrimaryGChromaticityX/Y` /
  `PrimaryBChromaticityX/Y` / `WhitePointChromaticityX/Y` /
  `LuminanceMax` / `LuminanceMin`, ids `0x55D1`..`0x55DA`) is written
  as an 8-byte big-endian `f64` only when its own `Option<f64>` slot is
  `Some(v)` — mirroring the per-child omission rules above. A
  `Some(MkvMasteringMetadata::default())` (every slot `None`)
  serialises as an empty 3-byte `MasteringMetadata` master that the
  demuxer parses into `Some(MasteringMetadata::default())`; setting
  `mastering_metadata: None` keeps the entire sub-master off-disk so
  the demuxer surfaces `None` from `mastering_metadata()`. The
  convenience `MkvMasteringMetadata::bt2020_d65_hdr10()` populates the
  ten-child set with BT.2020 primaries + D65 white point + 1000 cd/m²
  peak / 0.005 cd/m² floor — the canonical HDR10 mastering display.
  Pairs symmetrically with the existing `MkvDemuxer::video_colour`
  typed accessor — a mux→demux pipeline preserves every scalar child
  verbatim, including the `Other(u64)` forward-compat variants on each
  of the six enum-typed children, plus every populated
  `MasteringMetadata` chromaticity / luminance child.
- **`Video > Projection` master on write** (RFC 9559 §5.1.4.1.28.41,
  including §5.1.4.1.28.42..§5.1.4.1.28.46):
  `MkvMuxer::set_video_projection(stream_index, MkvProjection)` queues a
  per-track hint that lands inside the track's `Video` master at
  `write_header` time, after the `Colour` master, as a `Projection`
  master (id `0x7670`). The demux-side `ProjectionType` enum gained a
  `to_raw()` inverse so every Table 18 value round-trips, including the
  `Other(u64)` forward-compat variant (§27.15 leaves the registry open).
  Per-element omission rules: `ProjectionType` is written only for
  non-`Rectangular` types (the §5.1.4.1.28.42 default `0` stays off-disk);
  each `ProjectionPose{Yaw,Pitch,Roll}` child is written as an 8-byte
  big-endian `f64` only when non-zero (the §5.1.4.1.28.44..46 default
  `0.0` stays off-disk); `ProjectionPrivate` (the verbatim ISOBMFF box
  body — `equi` / `cbmp` / `mshp`) is written only when `Some(_)` and is
  never interpreted by the muxer. Queueing `MkvProjection::default()`
  writes an empty `Projection` master that the demuxer parses into
  `Some(Projection::default())`; omitting the call keeps the master
  off-disk so the demuxer surfaces `None`. Convenience constructors
  `MkvProjection::equirectangular(private)` (the 360°-VR shape) and
  `MkvProjection::rotated(roll_degrees)` (the §5.1.4.1.28.46 worked
  example) cover the two common shapes. Spec rules enforced at queue
  time: rejects post-`write_header` use, out-of-range `stream_index`, and
  calls on non-video tracks. Pairs symmetrically with the existing
  `MkvDemuxer::video_projection` typed accessor — a mux→demux pipeline
  preserves the projection record (type, pose, and verbatim
  `ProjectionPrivate` payload) bit-exactly.
- **TrackEntry audience flags on write** (RFC 9559
  §5.1.4.1.6..§5.1.4.1.11):
  `MkvMuxer::set_track_audience_flags(stream_index, MkvTrackAudienceFlags)`
  queues a per-track hint whose six `Option<bool>` slots — `forced`
  (`FlagForced`, id `0x55AA`), `hearing_impaired` (`FlagHearingImpaired`,
  id `0x55AB`), `visual_impaired` (`FlagVisualImpaired`, id `0x55AC`),
  `text_descriptions` (`FlagTextDescriptions`, id `0x55AD`), `original`
  (`FlagOriginal`, id `0x55AE`), `commentary` (`FlagCommentary`, id
  `0x55AF`) — land directly inside the `TrackEntry` (the elements sit on
  `TrackEntry` itself, not in a sub-master) at `write_header` time, after
  `FlagLacing`, in numerical-id order. Per-element omission rule: each
  `Some(v)` slot writes the element explicitly as `0` / `1`; each `None`
  slot stays off-disk. For `FlagForced` (the only one with a spec
  default), omission and `Some(false)` decode identically (`false`) but
  differ on disk — the explicit write is the way to override a
  downstream tool. For the five default-less `minver: 4` flags the
  distinction is semantic: omission decodes as `None` while `Some(false)`
  round-trips as `Some(false)`, preserving the §5.1.4.1.7..§5.1.4.1.11
  "set to 1 *if and only if* …" explicit-zero signal. Unlike the
  `set_video_*` family there is **no track-type restriction** — the spec
  carries all six elements on every `TrackEntry`, so audio / video /
  subtitle tracks all accept the call (mirroring the demux side, which
  surfaces a record for every track). The muxer already pins
  `DocTypeVersion` to `4`, so emitting the `minver: 4` elements never
  violates the declared document version. Convenience constructors
  `MkvTrackAudienceFlags::forced_subtitle()` /
  `hearing_impaired_track()` / `visual_impaired_track()` /
  `commentary_track()` cover the common single-flag shapes. Rejects
  post-`write_header` use and out-of-range `stream_index`. Pairs
  symmetrically with the existing `MkvDemuxer::track_audience_flags`
  typed accessor — a mux→demux pipeline preserves every explicit flag,
  including the `Some(false)`-vs-absent distinction.
- **Per-Block `BlockAdditions` on write** (RFC 9559 §5.1.3.5.2 +
  §5.1.4.1.16): `MkvMuxer::write_packet_with_additions(&packet,
  &[MkvBlockAddition])` emits the packet as a `BlockGroup` (§5.1.3.5)
  instead of a `SimpleBlock` — `Block` (frame bytes, unlaced; any
  pending same-track lace is flushed first so Block order is
  preserved), `BlockAdditions` with one `BlockMore` per addition in
  slice order (each writing `BlockAdditional` verbatim and `BlockAddID`
  only when it differs from the §5.1.3.5.2.3 default `1`),
  `BlockDuration` (§5.1.3.5.3) when the packet carries a duration (a
  `SimpleBlock` could not have carried it), and `ReferenceBlock`
  (§5.1.3.5.5) when the packet is not a keyframe (a plain `Block` has
  no KEY flag bit; keyframe-ness is the element's absence — the
  relative value points at the track's most recently written Block,
  falling back to the spec-sanctioned `0` "reference unknown" when
  there is none). Prerequisite: declare the track's maximum id via
  `MkvMuxer::set_max_block_addition_id(stream_index, max)` before
  `write_header` — it lands as the `MaxBlockAdditionID` TrackEntry
  element, and `write_packet_with_additions` rejects an undeclared
  stream (§5.1.4.1.16's default `0` means "no BlockAdditions for this
  track"), a `BlockAddID` of `0` (range "not 0"), an id above the
  declared maximum, and duplicate ids within one call (§5.1.3.5.2.3
  uniqueness MUST) — all before any byte is written. An empty
  additions slice degrades to plain `write_packet` behaviour
  (`BlockMore` is mandatory inside the master, so an empty
  `BlockAdditions` would be malformed). The convenience constructor
  `MkvBlockAddition::codec_defined(data)` covers the `BlockAddID = 1`
  shape (e.g. WebM alpha — pair with `set_video_alpha_mode`). Pairs
  symmetrically with the new `MkvDemuxer::block_additions` /
  `max_block_addition_id` typed accessors — a mux→demux pipeline
  preserves every addition byte-for-byte, plus the packet's keyframe
  flag and duration.
- **Full `BlockGroup` child set on write** (RFC 9559 §5.1.3.5.4–§5.1.3.5.7):
  `MkvMuxer::write_packet_with_block_group(&packet, &BlockGroupOptions)`
  wraps a packet in a `BlockGroup` carrying any combination of
  `BlockAdditions`, an explicit multi-`ReferenceBlock` list (§5.1.3.5.5),
  `ReferencePriority` (§5.1.3.5.4, written only when non-zero),
  `CodecState` (§5.1.3.5.6) and `DiscardPadding` (§5.1.3.5.7), emitted in
  §5.1.3.5 order, plus the reclaimed DivX trick-track / old-lacing children
  (RFC 9559 Appendix A.3..A.14): `block_virtual` (`BlockVirtual`),
  `reference_virtual` (`ReferenceVirtual`), `slices` (a `Vec<TimeSlice>`
  emitted as one `Slices` master, each `TimeSlice` built via
  `TimeSlice::from_fields`), and `reference_frame` (`ReferenceFrame::from_fields`).
  The write-side mirror of `MkvDemuxer::block_group_meta`
  — a mux→demux round-trip preserves every child value, including an empty
  `TimeSlice` (the on-disk element count is preserved). A group needing
  no additions skips the `MaxBlockAdditionID` requirement.
- **`SilentTracks` on write** (RFC 9559 Appendix A.1 / A.2):
  `MkvMuxer::set_next_cluster_silent_tracks(&[track_numbers])` queues a
  `SilentTrackNumber` list for the next Cluster the muxer opens (drained
  after one Cluster to match the element's per-Cluster scope);
  `MkvMuxer::track_number(stream_index)` maps a stream index to its
  on-wire `TrackNumber`.
- Opt-in **block lacing** on write (RFC 9559 §5.1.4.5.5, §10.3):
  `MkvMuxer::with_block_lacing(LacingMode::{Xiph,Ebml,FixedSize})`
  before `write_header` aggregates same-track, same-keyframe-status
  consecutive frames (up to 8 per Block, never crossing a cluster
  boundary) into a single laced `SimpleBlock`. Default stays
  `LacingMode::None` (one frame per Block, `FlagLacing = 0`) for
  byte-identical back-compat. When lacing is on, the muxer writes
  `TrackEntry.FlagLacing = 1`, sets the LACING bits in the
  SimpleBlock flags byte to the requested mode, and encodes the
  per-frame size header (Xiph 255-additive octets,
  EBML signed-VINT deltas, or no header for fixed-size). For
  fixed-size mode, a frame whose size differs from the buffered run
  flushes the lace and starts a new one. Demuxer side already
  handles all three modes — the new write path completes the
  round-trip in-tree.
- **`Audio` master children on write** (RFC 9559 §5.1.4.1.29,
  §5.1.4.1.29.1..§5.1.4.1.29.4):
  `MkvMuxer::set_track_audio(stream_index, MkvTrackAudio)` queues a
  per-track hint that lands inside the track's `Audio` master (id
  `0xE1`) at `write_header` time. The muxer already derives a minimal
  `Audio` master from the stream's `StreamInfo` (`sample_rate` →
  `SamplingFrequency`, `channels` → `Channels`, sample-format bit width
  → `BitDepth`); this hint lets a caller override those derived children
  **and** supply the one child the `StreamInfo`-derived path cannot
  express: `OutputSamplingFrequency` (id `0x78B5`, §5.1.4.1.29.2), the
  Spectral Band Replication (SBR) output rate the demux-side
  `track_audio` / `TrackAudio::is_sbr()` accessor already reads back.
  Per-field rule: a `Some(v)` overrides the `StreamInfo`-derived child;
  a `None` defers to the `StreamInfo` value (and for
  `output_sampling_frequency`, simply omits the element). Children that
  resolve to nothing stay off-disk so the demuxer materialises the
  §5.1.4.1.29.1 default `8000.0` / §5.1.4.1.29.3 default `1` (mono);
  `BitDepth` has no spec default, so its absence surfaces as `None`. The
  convenience constructor `MkvTrackAudio::sbr(core)` produces the
  canonical HE-AAC pair (`core`, `2*core`). Spec range checks enforced
  at queue time: `SamplingFrequency` / `OutputSamplingFrequency` ranged
  `> 0x0p+0` (a `Some(v)` `<= 0.0` / non-finite is rejected),
  `Channels` / `BitDepth` ranged `not 0` (a `Some(0)` is rejected).
  Track-type restriction mirrors the demux side (which returns `None`
  for non-audio tracks): the setter rejects non-`Audio` streams plus
  post-`write_header` use and out-of-range `stream_index`; repeated
  calls are last-write-wins; the read-back
  `MkvMuxer::track_audio(stream_index)` accessor returns the queued hint
  pre-`write_header`. Pairs symmetrically with the existing
  `MkvDemuxer::track_audio` typed accessor — a mux→demux pipeline
  preserves every supplied child bit-exactly, including the
  `OutputSamplingFrequency` SBR signal.
- **`TrackEntry` timing trio on write** (RFC 9559
  §5.1.4.1.13..§5.1.4.1.15): `MkvMuxer::set_track_timing(stream_index,
  MkvTrackTiming)` queues a per-track hint whose three `Option` slots —
  `default_duration` (`DefaultDuration`, id `0x23E383`),
  `default_decoded_field_duration` (`DefaultDecodedFieldDuration`, id
  `0x234E7A`), and `track_timestamp_scale` (`TrackTimestampScale`, id
  `0x23314F`) — land directly inside the `TrackEntry` (no gating master) at
  `write_header` time, after `MaxBlockAdditionID`. Per-field omission rule:
  each `Some(v)` writes the element explicitly, each `None` stays off-disk
  (the demuxer surfaces `None` for the two durations and materialises the
  §5.1.4.1.15 `TrackTimestampScale` default `1.0`). There is no track-type
  restriction — the spec carries all three on every `TrackEntry`. Spec
  range checks enforced at queue time: the two durations are ranged `not 0`
  (a `Some(0)` is rejected) and `TrackTimestampScale` is ranged `> 0x0p+0`
  (a non-finite / non-positive `Some(v)` is rejected); the setter also
  rejects post-`write_header` use and out-of-range `stream_index`. The
  convenience constructor `MkvTrackTiming::from_frame_rate(fps)` rounds
  `1e9 / fps` to the nanosecond `DefaultDuration` interval (rejecting
  non-finite / non-positive fps). Repeated calls are last-write-wins; the
  read-back `MkvMuxer::track_timing(stream_index)` accessor returns the
  queued hint pre-`write_header`. Pairs symmetrically with the new
  `MkvDemuxer::track_timing` typed accessor — a mux→demux pipeline
  preserves every supplied child bit-exactly, including the
  `DefaultDuration`-derived nominal frame rate.
- **`TrackEntry` identity / selection on write** (RFC 9559 §5.1.4.1.18 /
  .19 / .20 / .23 / .4 / .5 / .12 / .24):
  `MkvMuxer::set_track_identity(stream_index, MkvTrackIdentity)` queues a
  per-track hint whose eight `Option` slots — `name` (`Name`, id `0x536E`),
  `codec_name` (`CodecName`, id `0x258688`), `language` (`Language`, id
  `0x22B59C`), `language_bcp47` (`LanguageBCP47`, id `0x22B59D`),
  `flag_enabled` (`FlagEnabled`, id `0xB9`), `flag_default` (`FlagDefault`,
  id `0x88`), `flag_lacing` (`FlagLacing`, id `0x9C`), and `attachment_link`
  (`AttachmentLink`, id `0x7446`) — land directly inside the `TrackEntry` (no
  gating master) at `write_header` time. Per-field omission rule: each
  `Some(v)` writes the element explicitly, each `None` stays off-disk (the
  demuxer materialises the §default `1` for the three selection flags and
  `None` for the strings / link). The `language` slot, when `Some`, overrides
  the `StreamInfo`-derived `Language`; the `flag_lacing` slot, when `Some`,
  overrides the muxer's auto-derived `FlagLacing`. Per §5.1.4.1.20, when both
  `language` and `language_bcp47` are `Some` the muxer writes only
  `LanguageBCP47` (the spec says `Language` MUST be ignored when BCP-47 is
  present), mirroring the `TagLanguageBCP47` handling. There is **no
  track-type restriction** — the spec carries all eight on every
  `TrackEntry`. Spec checks enforced at queue time: `attachment_link` is
  ranged `not 0` (a `Some(0)` is rejected, §5.1.4.1.24); an empty `Name` /
  `CodecName` / `Language` / `LanguageBCP47` string is rejected; plus
  post-`write_header` use and out-of-range `stream_index`. Convenience
  constructors `MkvTrackIdentity::named(name)` /
  `MkvTrackIdentity::language_bcp47(lang)` / `MkvTrackIdentity::non_default()`
  cover the common shapes; a read-back `MkvMuxer::track_identity(stream_index)`
  returns the queued hint pre-`write_header`. Pairs symmetrically with the new
  `MkvDemuxer::track_identity` typed accessor — a mux→demux pipeline preserves
  every supplied element, including the BCP-47 precedence and the
  explicit-`0`-vs-absent flag distinction.
- **`TrackOperation` on write** (RFC 9559 §5.1.4.1.30):
  `MkvMuxer::set_track_operation(stream_index, MkvTrackOperation)` queues a
  per-track *virtual-track* recipe that lands as a `TrackOperation` master
  (id `0xE2`) directly inside the carrying `TrackEntry` (sibling to
  `Video` / `Audio`) at `write_header` time. `MkvTrackOperation` carries a
  `combine_planes: Vec<MkvTrackPlane>` (`TrackCombinePlanes`,
  §5.1.4.1.30.1 — each `MkvTrackPlane` pairs a 0-indexed source
  `stream_index` with a `TrackPlaneType`, §5.1.4.1.30.4) and a
  `join_tracks: Vec<usize>` (`TrackJoinBlocks`, §5.1.4.1.30.5). Each
  plane / join reference's stream index is resolved to the source track's
  on-disk `TrackUID` at write time — the symmetric inverse of the demux
  side, which resolves each `TrackPlaneUID` (§5.1.4.1.30.3) /
  `TrackJoinUID` (§5.1.4.1.30.6) back to a stream index. The demux-side
  `TrackPlaneType` enum gained a `to_raw()` inverse so every Table 20
  value (`LeftEye` / `RightEye` / `Background`) round-trips, including the
  `Other(u64)` forward-compat variant (§27.17 leaves the "Matroska Track
  Plane Types" registry open). Both operation kinds may coexist on one
  track. Convenience constructors `MkvTrackOperation::stereo_3d(left,
  right)` (the canonical left/right-eye 3D recipe) and
  `MkvTrackOperation::join(streams)` cover the two common shapes. Spec
  rules enforced at queue time: rejects post-`write_header` use,
  out-of-range `stream_index`, an empty operation (`TrackCombinePlanes` /
  `TrackJoinBlocks` exist only to carry references), and any plane / join
  reference pointing at a non-existent stream (the `TrackPlaneUID` /
  `TrackJoinUID` "not 0" pins fall out of the stream-index→`TrackUID`
  mapping). Unlike the `set_video_*` family there is no track-type
  restriction — the spec carries `TrackOperation` on every `TrackEntry`,
  so a `TrackJoinBlocks` audio virtual track is accepted. Omitting the
  call keeps the master off-disk so the demuxer surfaces `None` from
  `track_operation`. Pairs symmetrically with the existing
  `MkvDemuxer::track_operation` typed accessor — a mux→demux pipeline
  preserves every plane (with its type) and every join reference.
- **`TrackTranslate` on write** (RFC 9559 §5.1.4.1.27):
  `MkvMuxer::set_track_translates(stream_index, Vec<MkvTrackTranslate>)` queues
  zero or more chapter-codec track-mapping masters that land directly inside the
  carrying `TrackEntry` (after the `TrackOperation` / `ContentEncodings` masters)
  at `write_header` time, in slice order. Each `MkvTrackTranslate` carries the
  mandatory `track_id` (`TrackTranslateTrackID`, binary, written verbatim — its
  format is defined by the chapter codec, not by Matroska) and `codec`
  (`TrackTranslateCodec`, Table 31), plus zero or more `edition_uids`
  (`TrackTranslateEditionUID`, an empty list meaning "all editions using the
  codec"). Unlike the `set_video_*` family there is **no track-type
  restriction** — the spec carries `TrackTranslate` on every `TrackEntry`. Spec
  rules enforced at queue time: rejects post-`write_header` use, out-of-range
  `stream_index`, an empty `track_id` (`minOccurs: 1`), and a zero
  `TrackTranslateEditionUID` ("not 0"). Calling with an empty slice clears any
  previously-queued mappings; repeated calls are last-write-wins. The
  convenience constructor `MkvTrackTranslate::new(track_id, codec)` covers the
  all-editions shape. Omitting the call keeps every master off-disk so the
  demuxer surfaces an empty `track_translates` slice. Pairs symmetrically with
  the new `MkvDemuxer::track_translates` typed accessor — a mux→demux pipeline
  round-trips every mapping field-for-field.
- **Reclaimed Appendix-A `TrackLegacy` on write** (RFC 9559 Appendix
  A.16..A.27 + A.28..A.32): `MkvMuxer::set_track_legacy(stream_index,
  MkvTrackLegacy)` queues the historical `TrackEntry`-level legacy elements —
  `CodecSettings` / `CodecInfoURL` / `CodecDownloadURL` / `CodecDecodeAll`
  codec-description metadata, the `MinCache` / `MaxCache` / signed `TrackOffset`
  cache-and-offset hints (A.16..A.18), the `GammaValue` / `FrameRate` floats
  (A.25 / A.26, written inside the `Video` master) and `ChannelPositions`
  binary (A.27, written inside the `Audio` master), the ordered `TrackOverlay`
  fallback list, and the DivXTrickTrack pairing quintet — which land inside the carrying `TrackEntry`
  (after the `TrackTranslate` masters) at `write_header` time. Only populated
  fields reach the disk: an absent field (`None` / empty `Vec`) keeps its
  element off-disk, so the demuxer observes the same absence. The appendix
  specifies no defaults, so a `Some(0)` `decode_all` / `trick_track_flag` is
  written as an explicit `0` distinct from omission. Spec rules enforced at
  queue time: rejects post-`write_header` use, an out-of-range `stream_index`,
  and a `TrickTrackSegmentUID` / `TrickMasterTrackSegmentUID` whose length is
  not the canonical 16 bytes (a `SegmentUUID` is a 128-bit value). Calling with
  an all-absent record clears the queue; repeated calls are last-write-wins.
  Pairs symmetrically with the `MkvDemuxer::track_legacy` typed accessor — a
  mux→demux pipeline round-trips every populated field verbatim.
- **Linked-Segment `Info` on write** (RFC 9559 §5.1.2.1..§5.1.2.8 + Section 17):
  `MkvMuxer::set_segment_linking(SegmentLinking)` queues the Linked-Segment
  metadata that lands inside the `Info` master at `write_header` time, in the
  §5.1.2 element order (before `TimestampScale`): `SegmentUUID` (§5.1.2.1),
  `SegmentFilename` (§5.1.2.2), `PrevUUID` (§5.1.2.3), `PrevFilename`
  (§5.1.2.4), `NextUUID` (§5.1.2.5), `NextFilename` (§5.1.2.6), every
  `SegmentFamily` (§5.1.2.7), and every `ChapterTranslate` (§5.1.2.8 —
  `ChapterTranslateID` + `ChapterTranslateCodec` + zero or more
  `ChapterTranslateEditionUID`). The setter takes the **same** demux-side
  `SegmentLinking` record `MkvDemuxer::segment_linking()` produces, so a
  mux→demux pipeline round-trips every UID / filename / family / translate
  field byte-for-byte — the Segment-level twin of the `set_track_translates`
  surface. Spec rules enforced at queue time (before any byte is written):
  rejects post-`write_header` use; an off-length UID (§5.1.2.1 / .3 / .5 / .7
  are all `length: 16`); a `PrevUUID` / `NextUUID` equal to `SegmentUUID`
  (§5.1.2.3 / .5 "MUST NOT be equal"); a `ChapterTranslate` without the
  REQUIRED `SegmentFamily` (§5.1.2.7 usage note); and an empty
  `ChapterTranslateID` (§5.1.2.8.1, `minOccurs: 1`). The read-only
  `segment_linking()` accessor exposes the queued record before sealing. An
  all-default record (or omitting the call) writes nothing — the common
  standalone Segment, which the demuxer surfaces as an empty `SegmentLinking`.
- **`Info` metadata on write** (RFC 9559 §5.1.2.11 / §5.1.2.12):
  `MkvMuxer::set_title(impl Into<String>)` writes the Segment `Title`
  (§5.1.2.12, the general name), and `MkvMuxer::set_date_utc_ns(i64)` writes
  the Segment `DateUTC` (§5.1.2.11) as signed nanoseconds since the Matroska
  epoch (2001-01-01T00:00:00 UTC — the `date` element type), at a fixed 8-byte
  on-disk width. `MkvMuxer::set_date_utc_unix_secs(i64)` is a convenience that
  rebases a Unix timestamp onto that epoch (a pre-2001 instant produces a
  negative, still-valid `DateUTC`). Both elements land in the `Info` master in
  §5.1.2 element order (after `TimestampScale`, before `MuxingApp`) and
  round-trip onto the demuxer's flat metadata view under the `"title"` /
  `"date"` keys (the latter formatted back to ISO-8601). All three setters
  reject post-`write_header` use; omitting them writes neither element.
- **`ContentEncodings` on write** (RFC 9559 §5.1.4.1.31):
  `MkvMuxer::set_track_content_encodings(stream_index, ContentEncodings)`
  queues a per-track transformation chain that lands as a
  `ContentEncodings` master (id `0x6D80`) directly inside the carrying
  `TrackEntry` at `write_header` time. The setter takes the **same**
  demux-side `ContentEncodings` record `MkvDemuxer::content_encodings`
  produces, so a mux→demux pipeline round-trips the whole chain
  element-for-element. Each `ContentEncoding` writes `ContentEncodingOrder`
  (§5.1.4.1.31.2), `ContentEncodingScope` (§5.1.4.1.31.3), and the
  `ContentEncodingType`-keyed sub-master: `ContentCompression`
  (§5.1.4.1.31.5 — `ContentCompAlgo` + `ContentCompSettings`, the latter
  written only when non-empty) or `ContentEncryption` (§5.1.4.1.31.8 —
  `ContentEncAlgo`, `ContentEncKeyID` when non-empty, the nested
  `ContentEncAESSettings > AESSettingsCipherMode` written only on AES, and
  the reclaimed content-signing quartet (RFC 9559 Appendix A.33..A.36 —
  `ContentSignature` / `ContentSigKeyID` / `ContentSigAlgo` /
  `ContentSigHashAlgo`) carried by the demux-side `ContentSigning` record,
  each of whose four children is written only when its `Option` slot is
  `Some` so an empty `ContentSigning` adds no bytes and round-trips to
  `None` on every field).
  The demux-side `ContentCompAlgo` / `ContentEncAlgo` / `AesCipherMode`
  enums gained `to_raw()` inverses so every Table 23 / 24 / 26 value
  round-trips, including each `Other(u64)` forward-compat variant (§27.2 /
  §27.3 / §27.4 leave the registries open). The container is a pure
  carrier — it does **not** compress or encrypt the frame bytes; the
  caller hands `write_packet` payloads already matching the declared
  chain. Spec rules enforced at queue time (before any byte is written):
  rejects post-`write_header` use, out-of-range `stream_index`, an empty
  chain (`ContentEncoding` is `minOccurs: 1`), a duplicate
  `ContentEncodingOrder` (§5.1.4.1.31.2 "MUST be unique"), a zero
  `ContentEncodingScope` (§5.1.4.1.31.3 "not 0"), an
  `AESSettingsCipherMode` paired with a non-AES `ContentEncAlgo` (Table 25
  forbids it), and a zero `AESSettingsCipherMode` (§5.1.4.1.31.12 "not 0").
  Chain steps are written ascending by order on disk; the demuxer re-sorts
  into descending decode order on read, so either layout parses
  identically. Omitting the call keeps the master off-disk so the demuxer
  surfaces `None` from `content_encodings`. Pairs symmetrically with the
  existing `MkvDemuxer::content_encodings` typed accessor.

### Codec ID mapping (`codec_id` module)

Matroska `CodecID` string <-> oxideav `CodecId`. Both directions are
implemented for roundtrip:

- Audio: `A_FLAC`, `A_OPUS`, `A_VORBIS`, `A_PCM/INT/LIT`,
  `A_PCM/INT/BIG`, `A_PCM/FLOAT/IEEE`, `A_AAC` (+ `MPEG4/LC` /
  `MPEG2/LC` aliases), `A_MPEG/L3`, `A_AC3`, `A_EAC3`.
- Video: `V_VP8`, `V_VP9`, `V_AV1`, `V_MPEG4/ISO/AVC`,
  `V_MPEGH/ISO/HEVC`, `V_FFV1`, `V_THEORA`, plus `V_MS/VFW/FOURCC` with
  BITMAPINFOHEADER fourcc extraction (e.g. `FFV1`).
- Subtitle: `S_TEXT/UTF8` (subrip), `S_TEXT/SSA`, `S_TEXT/ASS`,
  `S_TEXT/WEBVTT`, `S_TEXT/USF`, `S_VOBSUB` (DVD), `S_HDMV/PGS` /
  `S_HDMV/TEXTST` (Blu-ray), `S_DVBSUB`, `S_KATE`. Subtitle tracks
  surface with `MediaType::Subtitle`; their payload bytes pass through
  unchanged.

Unknown MKV codec IDs fall back to a pass-through `mkv:<raw-id>` form
so the demuxer never hides an unrecognised track.

### Probes + registration

- Registers both `"matroska"` and `"webm"` with the container registry.
- Extensions: `.mkv`, `.mka`, `.mks` -> `matroska`; `.webm` -> `webm`.
- Probe scoring: DocType=webm scores 100 on `probe_webm` and 0 on
  `probe_matroska` (so `.mkv` never masquerades as `webm`). DocType=
  matroska scores 100 on `probe_matroska` and 0 on `probe_webm`. Files
  with an ambiguous DocType fall through to the matroska entry.

## What's NOT implemented

- CRC-32 validation covers Top-Level master elements parsed up front and
  every `Cluster` the demuxer opens through `next_packet` / `seek_to`; the
  late best-effort Cues rescan (when Cues sit after the final Cluster) is
  now checksummed too — a leading `CRC-32` child on the late-Cues `Cues`
  element validates and surfaces through `crc_status()` exactly the same
  way the up-front masters do. A `Cluster` declared with the unknown-size
  VINT still produces no status (RFC 8794 §11.3.1 needs a bounded body).
  The muxer writes a leading `CRC-32` child on every Top-Level master it
  buffers end-to-end before flushing — `Info`, `Tracks`, `Cues`, plus
  `Chapters` and `Attachments` when those are queued. `SeekHead` and
  `Cluster` are deliberately not CRC'd on the mux side: the `SeekHead`
  Cues entry is patched in `write_trailer` (which would invalidate any
  CRC computed up front), and `Cluster` is streamed with the unknown-size
  VINT (RFC 8794 §11.3.1's bounded-body requirement).
- The reclaimed content-signing quartet (RFC 9559 Appendix A.33..A.36 —
  `ContentSignature` `0x47E3`, `ContentSigKeyID` `0x47E4`, `ContentSigAlgo`
  `0x47E5`, `ContentSigHashAlgo` `0x47E6`) is now decoded and written on both
  sides (see the `ContentEncodings` entries below). The container surfaces and
  round-trips the four values verbatim; it never computes or verifies a
  signature (out of container scope).
- `TrackOperation` is decoded and surfaced (left/right-eye plane combining,
  block joining) and now written on the mux side
  (`MkvMuxer::set_track_operation`, see the Muxer section) but the demuxer
  does not yet *apply* it — virtual tracks are reported alongside their
  source tracks rather than being synthesised into a single combined output
  stream.
- `ContentEncodings` is decoded and surfaced (compression / encryption
  headers). The demuxer *undoes* a Block-scoped Header-Stripping chain
  (algo 3) on read — packets carry the original frame bytes — but the
  generic compression algorithms (zlib / bzlib / lzo1x) and encryption are
  not reversed: for those a caller that wants raw codec bytes must apply the
  reported encoding chain itself. zlib/bzlib/lzo1x decompression and
  decryption are out of container scope. The mux side now *writes* the
  `ContentEncodings` master (`MkvMuxer::set_track_content_encodings`, see
  the Muxer section) — it carries the declared chain but does not itself
  compress or encrypt the frame bytes; the caller supplies already-encoded
  payloads.
- `Video` sub-element coverage is now complete on the demux side:
  `PixelWidth` / `PixelHeight` (§5.1.4.1.28.6 / §5.1.4.1.28.7) feed the
  `StreamInfo` dimensions; `FlagInterlaced` / `FieldOrder`
  (§5.1.4.1.28.1 / §5.1.4.1.28.2) surface through `video_interlacing`;
  the `PixelCrop{Top,Bottom,Left,Right}` + `DisplayWidth` /
  `DisplayHeight` / `DisplayUnit` quartet
  (§5.1.4.1.28.8..§5.1.4.1.28.14) surfaces through `video_geometry`;
  the full `Colour` master (§5.1.4.1.28.16) — including HDR metadata
  (`MaxCLL` / `MaxFALL` / `MasteringMetadata`) — surfaces through
  `video_colour`; `StereoMode` (§5.1.4.1.28.3) surfaces through
  `video_stereo_mode` and the legacy `OldStereoMode` (§5.1.4.1.28.5, id
  `0x53B9`) through `video_old_stereo_mode`; the `Projection` master
  (§5.1.4.1.28.41) —
  including `ProjectionType`, the verbatim ISOBMFF-mirrored
  `ProjectionPrivate` payload, and the yaw / pitch / roll pose triple —
  surfaces through `video_projection`; `AlphaMode` (§5.1.4.1.28.4)
  surfaces through `video_alpha_mode`; the reclaimed Appendix-A
  `AspectRatioType` element surfaces through
  `video_aspect_ratio_type`; and `UncompressedFourCC`
  (§5.1.4.1.28.15) surfaces through `video_uncompressed_fourcc`. On the
  mux side, `PixelWidth` / `PixelHeight`, the `FlagInterlaced` /
  `FieldOrder` pair (`MkvMuxer::set_video_interlacing`,
  §5.1.4.1.28.1 + §5.1.4.1.28.2), the `StereoMode` / `AlphaMode`
  pair (`MkvMuxer::set_video_stereo_mode` /
  `MkvMuxer::set_video_alpha_mode`, §5.1.4.1.28.3 + §5.1.4.1.28.4),
  the `PixelCrop{Top,Bottom,Left,Right}` + `DisplayWidth` /
  `DisplayHeight` / `DisplayUnit` quartet
  (`MkvMuxer::set_video_geometry`, §5.1.4.1.28.8..§5.1.4.1.28.14),
  `UncompressedFourCC`
  (`MkvMuxer::set_video_uncompressed_fourcc`, §5.1.4.1.28.15), the
  eleven scalar children of the `Colour` master
  (`MkvMuxer::set_video_colour`, §5.1.4.1.28.16,
  §5.1.4.1.28.17..§5.1.4.1.28.29 — `MatrixCoefficients`,
  `BitsPerChannel`, `ChromaSubsampling{Horz,Vert}`,
  `CbSubsampling{Horz,Vert}`, `ChromaSiting{Horz,Vert}`, `Range`,
  `TransferCharacteristics`, `Primaries`, `MaxCLL`, `MaxFALL`; the
  convenience constructors `MkvVideoColour::bt709()` and
  `MkvVideoColour::bt2020_pq()` cover the SDR HD and HDR10 PQ
  shapes), and the ten chromaticity / luminance children of the
  `Colour > MasteringMetadata` sub-master
  (`MkvVideoColour::mastering_metadata = Some(MkvMasteringMetadata)`,
  §5.1.4.1.28.30..§5.1.4.1.28.40 — `Primary{R,G,B}Chromaticity{X,Y}`,
  `WhitePointChromaticity{X,Y}`, `Luminance{Max,Min}`; the convenience
  constructor `MkvMasteringMetadata::bt2020_d65_hdr10()` covers the
  canonical HDR10 shape), and the `Projection` master
  (`MkvMuxer::set_video_projection`, §5.1.4.1.28.41 — `ProjectionType`,
  the verbatim `ProjectionPrivate` payload, and the yaw / pitch / roll
  pose triple; the convenience constructors
  `MkvProjection::equirectangular()` and `MkvProjection::rotated()` cover
  the 360°-VR and roll-only shapes), and the reclaimed Appendix-A
  `AspectRatioType` element (`MkvMuxer::set_video_aspect_ratio_type`,
  Appendix A.24, id `0x54B3`), and the legacy `OldStereoMode`
  (`MkvMuxer::set_video_old_stereo_mode`, §5.1.4.1.28.5, id `0x53B9`) are
  written. The `Video` sub-element set is now fully symmetric — every
  element the demux side reads, the mux side can write.

## Robustness

`tests/injection_robustness.rs` pins eighteen attacker-shaped byte
patterns against the open / `next_packet` / `seek_to` / `attachment_data`
surface: a `skip` helper that previously cast `u64 as i64` and could
seek the reader *backwards* on a forged `Size` field; demux-open
rejection of an empty input, an EBML-magic with a truncated header, an
oversize EBML-header `Size`, oversize `DocType` / `CodecID` / `TagString`
strings, and a `Segment` declared size that runs past EoF; cluster-time
handling of an oversize `SimpleBlock`, a Xiph-laced `SimpleBlock` whose
declared sub-frame sizes overrun the body, and a fixed-laced
`SimpleBlock` with `n_frames = 5` over an empty payload; on-demand
`attachment_data` short-read on a forged 4 GiB `FileData` size and a
forged 2 GiB `FileName`; an out-of-range `CueRelativePosition` in
`seek_to`; a 4000-level-deep nested `SimpleTag` chain that must parse
without a stack overflow (the §5.1.8.1.2 `recursive` depth cap), a
name-less nested `SimpleTag` that must be dropped (§5.1.8.1.2.1
`minOccurs: 1`) while its named parent survives; and an inline
fuzz-corpus replay of five malformed seed shapes. All checks land as
standard `cargo test` targets so a regression on any one surfaces in CI
without waiting for a fuzz cycle.

`tests/ebml_walker_property.rs` adds deterministic property-style coverage
for the EBML element walker (RFC 8794) that backs the whole demuxer — a
seeded splitmix64 PRNG drives ~100k generated cases per run (no
`proptest`/`quickcheck` dependency): VINT `write_vint`↔`read_vint`
round-trips with `min_width` honoured across the full 56-bit range, the
unknown-size sentinel at every width, `read_element_header` round-trips, a
sequential `read_element_header` + `skip` walk that recovers every element
id and lands exactly on the buffer end, and a no-panic / no-backward-seek
guarantee over arbitrary byte streams and every prefix of a well-formed
tree.

## Fuzzing

A cargo-fuzz harness for the demuxer lives in `fuzz/`. It drives
`demux::open`, drains up to 256 packets via `next_packet`, and exercises
the `seek_to` cluster pre-open path — over arbitrary bytes — against the
contract that no call panics, aborts, integer-overflows (in a debug
build), or attempts an attacker-controlled allocation that exceeds what
the input can back. A second pass through `open_typed` additionally
fuzzes the typed-accessor surface — the per-Block `block_additions` /
`block_group_meta` side channels, the `ClusterRecord` `SilentTrackNumber`
lists, and the Chapters / SeekHead trees. The seed corpus in
`fuzz/corpus/demux/` covers a
minimal valid Matroska file, a minimal valid WebM file, an EBML-header-
only stream, and two regression inputs (one for an EBML size-overflow,
one for a zero-frame-size fixed-lacing `SimpleBlock`).

Run locally with a nightly toolchain:

```sh
cd fuzz
cargo +nightly fuzz run demux            # libFuzzer drives indefinitely
cargo +nightly fuzz run demux -- -max_total_time=60   # bounded
```

CI runs a 30-minute fuzz cycle daily via
`.github/workflows/fuzz.yml` (the OxideAV org-level reusable
`crate-fuzz.yml`).

## License

MIT - see [LICENSE](LICENSE).
