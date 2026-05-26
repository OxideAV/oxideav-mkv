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
  `Attachments`, `SeekHead`) carries a leading `CRC-32` child, the demuxer
  recomputes the IEEE CRC-32 (reflected poly `0xEDB88320`, init
  `0xFFFFFFFF`, final XOR, little-endian storage) over the rest of the
  element and records the result. `MkvDemuxer::crc_status() -> &[CrcStatus]`
  exposes each `{element_id, stored, computed}` triple with an
  `is_valid()` helper. Validation is informational — a mismatch does **not**
  abort the open (RFC 8794 §12: a reader MAY ignore the data); strict
  callers reject any non-valid status. Elements with no `CRC-32` child
  produce no status (omission is spec-legal).
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
- WebM profile: `mux::open_webm` pins `DocType="webm"` and rejects any
  stream whose codec isn't VP8/VP9/AV1 video or Vorbis/Opus audio with
  `Error::Unsupported`.
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

- CRC-32 validation covers Top-Level master elements parsed up front; the
  late best-effort Cues rescan (when Cues sit after the final Cluster) and
  per-Cluster CRC-32 children are not yet validated. CRC-32 is never
  written on the mux side.
- Attachments are never written on the mux side — the demuxer surfaces
  `AttachedFile` entries via the typed `MkvDemuxer::attachments`
  accessor and on-demand `MkvDemuxer::attachment_data` payload reader
  (see above), but the muxer has no `add_attachment` API yet.
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
- `Video` sub-element coverage is partial: `PixelWidth` / `PixelHeight`
  (§5.1.4.1.28.6 / §5.1.4.1.28.7) feed the `StreamInfo` dimensions;
  `FlagInterlaced` / `FieldOrder` (§5.1.4.1.28.1 / §5.1.4.1.28.2) surface
  through `video_interlacing`; and the `PixelCrop{Top,Bottom,Left,Right}` +
  `DisplayWidth` / `DisplayHeight` / `DisplayUnit` quartet
  (§5.1.4.1.28.8..§5.1.4.1.28.14) surfaces through `video_geometry`
  (see above). `StereoMode`, `AlphaMode`, `AspectRatioType` (reclaimed
  Appendix-A element), `UncompressedFourCC` and the full `Colour` master
  (§5.1.4.1.28.16) — including HDR metadata
  (`MaxCLL` / `MaxFALL` / `MasteringMetadata`) — are still skipped on the
  demux side and never written on the mux side.

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
