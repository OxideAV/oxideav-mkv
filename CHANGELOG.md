# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Other

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
- RFC 9043 v3 slice layout + CRC-32 parity; ffmpeg→us decodes
- add lossless video codec — bit-exact self-roundtrip, RFC 9043 v3
- add rustfmt + clippy gates; release: macOS universal binary
- add Matroska (MKV) container + Opus crate + proper Ogg header handling
