# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.6](https://github.com/OxideAV/oxideav-mkv/compare/v0.0.5...v0.0.6) - 2026-04-25

### Other

- pin release-plz to patch-only bumps

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
