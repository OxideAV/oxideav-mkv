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
- Segment walk: `Info`, `Tracks`, `Tags`, `Cues`, `Cluster`. Known- and
  unknown-size Segment/Cluster both supported.
- Clusters: `SimpleBlock` and `BlockGroup -> Block`, all three lacing
  modes (Xiph, fixed, EBML-signed-delta).
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
  / `Blowfish` / `Aes` / `Other(u64)`, the `ContentEncKeyID`, and the
  nested `ContentEncAESSettings` → `AESSettingsCipherMode` as
  `Ctr` / `Cbc` / `Other(u64)`). The list is pre-sorted into *decode* order
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
- Fixed-size `SeekHead` at the start of the Segment with Seek entries
  for `Info`, `Tracks`, and `Cues` - so players that pre-walk the
  SeekHead (mpv, Chromium) jump straight to Cues without scanning. The
  Cues `SeekPosition` is patched in `write_trailer`; if no packets were
  written, the Cues entry is rewritten as a Void filler.
- `Info` (1 ms `TimecodeScale`), `Tracks`, rolling ~5 s `Cluster`s with
  `SimpleBlock` payload.
- `Cues` element emitted in `write_trailer` - index entries for every
  video keyframe and every audio cluster-start, so the resulting file
  is seekable without a second pass. Each entry carries
  `CueRelativePosition` (RFC 9559 §5.1.5.1.2.3, recommended by §22.1)
  so seek-aware readers jump straight to the indexed `SimpleBlock`
  inside the Cluster instead of scanning from the cluster header.
- Codec-specific fields: `CodecPrivate` normalisation for FLAC (`fLaC`
  magic prepended), Opus `CodecDelay` derived from the `OpusHead`
  pre-skip plus an 80 ms `SeekPreRoll` per the WebM spec.
- `Chapters` (RFC 9559 §5.1.7): `MkvMuxer::add_chapter(start_ns,
  end_ns, title)` queues a single English-language `ChapterAtom`;
  `add_chapter_full(MkvChapter)` takes a fully-specified record with
  multilingual `ChapterDisplay` rows (`ChapString` + `ChapLanguage`
  + optional `ChapCountry`). Chapters must be added before
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
- ContentSignature (RFC 9559 §A.33 reclaimed `0x47E3`) is parsed by neither
  side. The element is reserved for a future per-segment signature scheme.
- `TrackOperation` is decoded and surfaced (left/right-eye plane combining,
  block joining) but the demuxer does not yet *apply* it — virtual tracks
  are reported alongside their source tracks rather than being synthesised
  into a single combined output stream. `TrackOperation` is never written
  on the mux side.
- `ContentEncodings` is decoded and surfaced (compression / encryption
  headers). The demuxer *undoes* a Block-scoped Header-Stripping chain
  (algo 3) on read — packets carry the original frame bytes — but the
  generic compression algorithms (zlib / bzlib / lzo1x) and encryption are
  not reversed: for those a caller that wants raw codec bytes must apply the
  reported encoding chain itself. zlib/bzlib/lzo1x decompression and
  decryption are out of container scope; `ContentEncodings` is never written
  on the mux side.
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
  `video_stereo_mode`; the `Projection` master (§5.1.4.1.28.41) —
  including `ProjectionType`, the verbatim ISOBMFF-mirrored
  `ProjectionPrivate` payload, and the yaw / pitch / roll pose triple —
  surfaces through `video_projection`; `AlphaMode` (§5.1.4.1.28.4)
  surfaces through `video_alpha_mode`; the reclaimed Appendix-A
  `AspectRatioType` element surfaces through
  `video_aspect_ratio_type`; and `UncompressedFourCC`
  (§5.1.4.1.28.15) surfaces through `video_uncompressed_fourcc`. On the
  mux side, `PixelWidth` / `PixelHeight`, the `FlagInterlaced` /
  `FieldOrder` pair (`MkvMuxer::set_video_interlacing`,
  §5.1.4.1.28.1 + §5.1.4.1.28.2), and the `StereoMode` / `AlphaMode`
  pair (`MkvMuxer::set_video_stereo_mode` /
  `MkvMuxer::set_video_alpha_mode`, §5.1.4.1.28.3 + §5.1.4.1.28.4) are
  written; the remaining `Video` sub-elements (geometry quartet,
  Colour / MasteringMetadata, Projection, AspectRatioType,
  UncompressedFourCC) are not yet written.

## Robustness

`tests/injection_robustness.rs` pins sixteen attacker-shaped byte
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
`seek_to`; and an inline fuzz-corpus replay of five malformed seed
shapes. All checks land as standard `cargo test` targets so a regression
on any one surfaces in CI without waiting for a fuzz cycle.

## Fuzzing

A cargo-fuzz harness for the demuxer lives in `fuzz/`. It drives
`demux::open`, drains up to 256 packets via `next_packet`, and exercises
the `seek_to` cluster pre-open path — over arbitrary bytes — against the
contract that no call panics, aborts, integer-overflows (in a debug
build), or attempts an attacker-controlled allocation that exceeds what
the input can back. The seed corpus in `fuzz/corpus/demux/` covers a
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
