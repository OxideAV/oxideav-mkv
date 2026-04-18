# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
