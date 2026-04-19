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
oxideav-container = "0.0"
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
  ISO-8601), Tags `SimpleTag` name/value pairs.
- Duration: `Segment\Info\Duration` translated to microseconds.
- Seek: `seek_to(stream, pts)` uses the Cues index. Handles Cues at
  either end of the Segment, and walks an unknown-size final Cluster to
  find Cues that sit past it.
- An unknown-size Cluster is terminated cleanly when a sibling Segment-
  child element follows it (no more "Cues silently eaten as payload").

### Muxer (`mux::open` and `mux::open_webm`)

- EBML header + Segment (unknown size) for a streaming-friendly layout.
- `Info` (1 ms `TimecodeScale`), `Tracks`, rolling ~5 s `Cluster`s with
  `SimpleBlock` payload.
- `Cues` element emitted in `write_trailer` - index entries for every
  video keyframe and every audio cluster-start, so the resulting file
  is seekable without a second pass.
- Codec-specific fields: `CodecPrivate` normalisation for FLAC (`fLaC`
  magic prepended), Opus `CodecDelay` derived from the `OpusHead`
  pre-skip plus an 80 ms `SeekPreRoll` per the WebM spec.
- WebM profile: `mux::open_webm` pins `DocType="webm"` and rejects any
  stream whose codec isn't VP8/VP9/AV1 video or Vorbis/Opus audio with
  `Error::Unsupported`.

### Codec ID mapping (`codec_id` module)

Matroska `CodecID` string <-> oxideav `CodecId`. Both directions are
implemented for roundtrip:

- Audio: `A_FLAC`, `A_OPUS`, `A_VORBIS`, `A_PCM/INT/LIT`,
  `A_PCM/INT/BIG`, `A_PCM/FLOAT/IEEE`, `A_AAC` (+ `MPEG4/LC` /
  `MPEG2/LC` aliases), `A_MPEG/L3`, `A_AC3`, `A_EAC3`.
- Video: `V_VP8`, `V_VP9`, `V_AV1`, `V_MPEG4/ISO/AVC`,
  `V_MPEGH/ISO/HEVC`, `V_FFV1`, `V_THEORA`, plus `V_MS/VFW/FOURCC` with
  BITMAPINFOHEADER fourcc extraction (e.g. `FFV1`).

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

- No `SeekHead` on write (players scan the file head for Info/Tracks,
  then find Cues past the last Cluster - the existing interop test
  confirms ffmpeg accepts this layout).
- No block lacing on write; every frame becomes a standalone
  SimpleBlock. The read side handles all three lacing modes.
- No `Attachments`, `Chapters`, `ChapterDisplay` beyond skipping them.
- CRC-32 elements are parsed (skipped) but not validated.
- Subtitle tracks pass through as opaque packets with `MediaType::Data`.

## License

MIT - see [LICENSE](LICENSE).
