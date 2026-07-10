//! Matroska muxer.
//!
//! Layout produced:
//!
//! ```text
//! EBML header
//! Segment (unknown size)
//!   SeekHead (Info, Tracks, Cues — Cues offset patched at trailer time)
//!   Info (timecode scale, muxing/writing app)
//!   Tracks (one TrackEntry per input stream)
//!   Cluster (one per ~5 s of media, or one per file for short input)
//!     Timecode
//!     SimpleBlock × N
//!   Cues (seek index; written in write_trailer)
//! ```
//!
//! Segment and Cluster use the EBML "unknown size" sentinel so the muxer is
//! streaming-friendly during packet writes (no seek-back for Segment size).
//! Cues are emitted at the end of the file — the demuxer supports
//! end-of-file Cues by scanning past the last cluster, and common players
//! accept the same layout. The SeekHead lets players that prefer
//! up-front index lookup jump directly to Cues without
//! scanning the whole file; the Cues entry's SeekPosition is patched once
//! the Cues element is actually written (or replaced with a Void if no
//! packets were muxed). Timestamps are converted to milliseconds using the
//! standard 1 ms `TIMECODE_SCALE`.

use std::io::Write;

use oxideav_core::{Error, MediaType, Packet, Result, StreamInfo};
use oxideav_core::{Muxer, WriteSeek};

use crate::codec_id;
use crate::demux::{
    AlphaMode, ChapterTranslate, ChromaSitingHorz, ChromaSitingVert, ColourRange,
    ContentEncodingTransform, ContentEncodings, DisplayUnit, DocTypeExtension, FieldOrder,
    FlagInterlaced, MatrixCoefficients, OldStereoMode, Primaries, ProjectionType, SegmentLinking,
    StereoMode, TrackPlaneType, TransferCharacteristics,
};
use crate::ebml::{crc32_ieee, write_element_id, write_vint, VINT_UNKNOWN_SIZE};
use crate::ids;

/// Cluster every ~5 seconds (in MKV ms timecode units).
const CLUSTER_DURATION_MS: i64 = 5_000;

/// Open a general Matroska muxer. Writes `DocType="matroska"` and accepts
/// any codec the `codec_id` module maps to a known Matroska ID.
pub fn open(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    MkvMuxer::new(output, streams, DocType::Matroska).map(|m| Box::new(m) as Box<dyn Muxer>)
}

/// Open a WebM muxer. Writes `DocType="webm"` and rejects codecs outside
/// the WebM whitelist ([`crate::codec_id::ALLOWED_WEBM_CODECS`]) with
/// [`Error::Unsupported`].
pub fn open_webm(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    MkvMuxer::new(output, streams, DocType::Webm).map(|m| Box::new(m) as Box<dyn Muxer>)
}

/// Which on-disk flavour the muxer writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DocType {
    Matroska,
    Webm,
}

impl DocType {
    fn as_str(self) -> &'static str {
        match self {
            DocType::Matroska => "matroska",
            DocType::Webm => "webm",
        }
    }
}

/// Opt-in block-lacing mode for the muxer (RFC 9559 §5.1.4.5.5,
/// §10.3). When enabled, the muxer aggregates small consecutive
/// same-track frames into a single laced [`SimpleBlock`] instead of
/// emitting one Block per packet. Default is [`LacingMode::None`] —
/// every packet still becomes a standalone SimpleBlock, matching the
/// pre-lacing-on-write behaviour.
///
/// Aggregation rules (applied uniformly across all three lacing
/// modes):
/// - Same-track only (lacing across tracks is not supported by the
///   on-disk format).
/// - Same cluster only (a Block timestamp is a signed 16-bit offset
///   from the cluster timecode; a new cluster flushes any pending
///   lace).
/// - Same keyframe status — all frames in one lace either are
///   keyframes or are not, since the SimpleBlock KEY bit applies to
///   the whole Block.
/// - Up to 8 frames per Block. The on-disk format allows up to 256
///   (the lacing head is `n_frames - 1`, max 255); 8 is the cap the
///   muxer applies in practice to keep individual Blocks bounded and
///   to match the "small frames" recommendation in RFC 9559 §10.3
///   (lacing is for size economy on small frames, not for assembling
///   very large composite payloads).
/// - For [`LacingMode::FixedSize`], all frames in a lace must have
///   the exact same size — a candidate frame whose size differs from
///   the buffered run flushes the lace as-is and starts a new one.
///
/// When lacing is enabled, the [`MkvMuxer`] also writes
/// `TrackEntry.FlagLacing = 1` (RFC 9559 §5.1.4.1.12) instead of the
/// default-off `0`. Players that key on `FlagLacing` know they need
/// to decode lacing modes on the affected tracks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LacingMode {
    /// No lacing — one frame per Block (the default).
    #[default]
    None,
    /// Xiph lacing (RFC 9559 §10.3.2): per-frame sizes encoded as
    /// 255-additive runs of unsigned octets, the same scheme as Ogg.
    Xiph,
    /// EBML lacing (RFC 9559 §10.3.3): first frame size as an
    /// unsigned VINT, subsequent sizes as signed VINT deltas.
    Ebml,
    /// Fixed-size lacing (RFC 9559 §10.3.4): no per-frame size
    /// header; every frame in the lace must have identical size.
    FixedSize,
}

impl LacingMode {
    /// LACING bits (positions 1..3 of the SimpleBlock flags byte)
    /// per RFC 9559 §10.2: 00 = none, 01 = Xiph, 10 = fixed-size,
    /// 11 = EBML.
    fn flag_bits(self) -> u8 {
        match self {
            LacingMode::None => 0b00,
            LacingMode::Xiph => 0b01,
            LacingMode::FixedSize => 0b10,
            LacingMode::Ebml => 0b11,
        }
    }
}

/// Per-track `Audio` master hint queued via [`MkvMuxer::set_track_audio`]
/// (RFC 9559 §5.1.4.1.29, including §5.1.4.1.29.1..§5.1.4.1.29.4).
///
/// The muxer already derives a minimal `Audio` master from the stream's
/// [`StreamInfo`] (`sample_rate` → `SamplingFrequency`, `channels` →
/// `Channels`, `sample_format` bit width → `BitDepth`). This hint lets a
/// caller override those derived children *and* supply the one child the
/// `StreamInfo`-derived path cannot express: `OutputSamplingFrequency`
/// (§5.1.4.1.29.2), the Spectral Band Replication (SBR) output rate used
/// by HE-AAC and similar tracks.
///
/// Every field is `Option`; a `Some(v)` overrides the corresponding
/// `StreamInfo`-derived child, a `None` leaves the `StreamInfo`-derived
/// value in place (or, for `output_sampling_frequency`, simply omits the
/// element since `StreamInfo` has no equivalent).
///
/// * `sampling_frequency` — `SamplingFrequency` (§5.1.4.1.29.1), Hz.
///   Range `> 0x0p+0`. `Some(v)` overrides the `StreamInfo` `sample_rate`;
///   `None` keeps it. If neither the hint nor `StreamInfo` supplies a
///   value, the element is omitted and the demuxer materialises the spec
///   default `8000.0`.
/// * `output_sampling_frequency` — `OutputSamplingFrequency`
///   (§5.1.4.1.29.2), Hz. Range `> 0x0p+0`. The SBR signal: set it
///   strictly greater than `sampling_frequency` to mark SBR doubling
///   (the demuxer's `is_sbr()` predicate then fires). `None` omits the
///   element so the demuxer applies the Table 19 derived default
///   (= `SamplingFrequency`).
/// * `channels` — `Channels` (§5.1.4.1.29.3). Range `not 0`. `Some(v)`
///   overrides the `StreamInfo` `channels`; `None` keeps it. If neither
///   supplies a value, the element is omitted and the demuxer
///   materialises the spec default `1` (mono).
/// * `bit_depth` — `BitDepth` (§5.1.4.1.29.4). Range `not 0`. No spec
///   default. `Some(v)` overrides the `StreamInfo`-derived bit width;
///   `None` keeps it. If neither supplies a value the element is omitted
///   and the demuxer surfaces `None`.
///
/// Pairs symmetrically with the demux-side
/// [`crate::demux::MkvDemuxer::track_audio`] /
/// [`crate::demux::TrackAudio`] typed accessor — a mux→demux pipeline
/// preserves every supplied child bit-exactly, including the
/// `OutputSamplingFrequency` SBR signal.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MkvTrackAudio {
    /// `SamplingFrequency` (RFC 9559 §5.1.4.1.29.1), Hz. `None` defers to
    /// the stream's `StreamInfo` `sample_rate`.
    pub sampling_frequency: Option<f64>,
    /// `OutputSamplingFrequency` (RFC 9559 §5.1.4.1.29.2), Hz — the SBR
    /// output rate. `None` omits the element (Table 19 derived default
    /// applies on read).
    pub output_sampling_frequency: Option<f64>,
    /// `Channels` (RFC 9559 §5.1.4.1.29.3). `None` defers to the stream's
    /// `StreamInfo` `channels`.
    pub channels: Option<u64>,
    /// `BitDepth` (RFC 9559 §5.1.4.1.29.4), bits per sample. `None` defers
    /// to the `StreamInfo`-derived bit width (or omits when neither is set).
    pub bit_depth: Option<u64>,
}

impl MkvTrackAudio {
    /// Convenience constructor for the canonical HE-AAC SBR shape:
    /// a `core` sampling frequency with an explicit
    /// `OutputSamplingFrequency` of twice that rate (the SBR-doubling
    /// signal — RFC 9559 §5.1.4.1.29.2). `channels` and `bit_depth` are
    /// left to the stream's `StreamInfo`.
    pub fn sbr(core_sampling_frequency: f64) -> Self {
        MkvTrackAudio {
            sampling_frequency: Some(core_sampling_frequency),
            output_sampling_frequency: Some(core_sampling_frequency * 2.0),
            channels: None,
            bit_depth: None,
        }
    }
}

/// A queued `TrackEntry` timing hint (RFC 9559 §5.1.4.1.13..§5.1.4.1.15)
/// installed via [`MkvMuxer::set_track_timing`]. Each field is `Option`;
/// a `Some(v)` writes the element explicitly, a `None` leaves it off-disk
/// so the demuxer surfaces `None` for the two durations and materialises
/// the §5.1.4.1.15 `TrackTimestampScale` default `1.0`.
///
/// Pairs symmetrically with the demux-side
/// [`crate::demux::MkvDemuxer::track_timing`] / [`crate::demux::TrackTiming`]
/// typed accessor — a mux→demux pipeline preserves every supplied child
/// bit-exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MkvTrackTiming {
    /// `DefaultDuration` (RFC 9559 §5.1.4.1.13), nanoseconds per frame.
    /// Range `not 0`; no spec default. `None` omits the element.
    pub default_duration: Option<u64>,
    /// `DefaultDecodedFieldDuration` (RFC 9559 §5.1.4.1.14), nanoseconds
    /// between two successive fields at the decoder output. Range `not 0`;
    /// no spec default. `None` omits the element.
    pub default_decoded_field_duration: Option<u64>,
    /// `TrackTimestampScale` (RFC 9559 §5.1.4.1.15), the per-track
    /// timestamp scale factor. Range `> 0x0p+0`; spec default `1.0`.
    /// `None` omits the element (the demuxer materialises `1.0`).
    pub track_timestamp_scale: Option<f64>,
}

impl MkvTrackTiming {
    /// Convenience constructor that sets only `DefaultDuration` from a
    /// nominal frame rate, in frames per second. The nanosecond
    /// duration is `round(1e9 / fps)`. Returns [`Error::invalid`] when
    /// `fps` is non-finite, non-positive, or rounds to `0` ns (the spec
    /// range for `DefaultDuration` is `not 0`).
    ///
    /// The other two fields are left `None`. Pairs with the demux-side
    /// [`crate::demux::TrackTiming::nominal_frame_rate`].
    pub fn from_frame_rate(fps: f64) -> Result<Self> {
        if !fps.is_finite() || fps <= 0.0 {
            return Err(Error::invalid(format!(
                "MKV muxer: MkvTrackTiming::from_frame_rate fps must be finite and positive (got {fps})"
            )));
        }
        let ns = (1_000_000_000.0_f64 / fps).round();
        if !(ns.is_finite() && ns >= 1.0) {
            return Err(Error::invalid(
                "MKV muxer: MkvTrackTiming::from_frame_rate frame interval rounds to 0 ns",
            ));
        }
        Ok(MkvTrackTiming {
            default_duration: Some(ns as u64),
            default_decoded_field_duration: None,
            track_timestamp_scale: None,
        })
    }
}

/// A queued `TrackEntry` codec-timing hint (RFC 9559 §5.1.4.1.25 /
/// §5.1.4.1.26) installed via [`MkvMuxer::set_track_codec_timing`]. Both
/// fields are `Option<u64>` nanosecond values; a `Some(v)` writes the
/// element explicitly (an explicit `0` is legal and distinct from omission),
/// a `None` leaves it off-disk so the demuxer materialises the spec default
/// `0`.
///
/// The muxer auto-derives `CodecDelay` (from the Opus pre-skip) and a
/// recommended `SeekPreRoll` of 80 ms for an Opus track; an explicit hint
/// installed here overrides either auto-derived value **per field** (a
/// `Some` field wins, a `None` field falls back to the auto-derived value on
/// an Opus track, or to omission on any other track).
///
/// Pairs symmetrically with the demux-side
/// [`crate::demux::MkvDemuxer::track_codec_timing`] /
/// [`crate::demux::TrackCodecTiming`] typed accessor — a mux→demux pipeline
/// preserves every supplied child bit-exactly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MkvTrackCodecTiming {
    /// `CodecDelay` (RFC 9559 §5.1.4.1.25), the codec's built-in delay in
    /// nanoseconds. Range unrestricted; spec default `0`. `None` omits the
    /// element (or, for an Opus track, keeps the auto-derived pre-skip
    /// delay); `Some(v)` writes it explicitly and overrides any auto value.
    pub codec_delay: Option<u64>,
    /// `SeekPreRoll` (RFC 9559 §5.1.4.1.26), the number of nanoseconds that
    /// must be discarded after a seek before the output is correct. Range
    /// unrestricted; spec default `0`. `None` omits the element (or, for an
    /// Opus track, keeps the auto-derived 80 ms recommendation); `Some(v)`
    /// writes it explicitly and overrides any auto value.
    pub seek_pre_roll: Option<u64>,
}

impl MkvTrackCodecTiming {
    /// Convenience constructor from the two nanosecond values, either of
    /// which may be `None` to leave that element off-disk.
    pub fn new(codec_delay: Option<u64>, seek_pre_roll: Option<u64>) -> Self {
        Self {
            codec_delay,
            seek_pre_roll,
        }
    }
}

/// A queued `TrackEntry` identity / selection hint (RFC 9559 §5.1.4.1.18 /
/// .19 / .20 / .23 / .4 / .5 / .12 / .24) installed via
/// [`MkvMuxer::set_track_identity`]. Each field is `Option`; a `Some(v)`
/// writes the element explicitly, a `None` leaves it off-disk so the demuxer
/// surfaces `None` for the strings / `AttachmentLink` and materialises the
/// spec default `1` for the three selection flags.
///
/// Pairs symmetrically with the demux-side
/// [`crate::demux::MkvDemuxer::track_identity`] /
/// [`crate::demux::TrackIdentity`] typed accessor — a mux→demux pipeline
/// preserves every supplied element bit-exactly.
///
/// Two language fields are carried: `language` (Matroska form, §5.1.4.1.19,
/// spec default `"eng"`) and `language_bcp47` (BCP-47, §5.1.4.1.20). Per the
/// spec, when `language_bcp47` is present any `Language` element MUST be
/// ignored — so when both are `Some`, the muxer writes **only**
/// `LanguageBCP47`, mirroring how `TagLanguageBCP47` is handled. This hint's
/// `language` field, when set, also overrides the `StreamInfo`-derived
/// `Language` the muxer would otherwise emit from `CodecParameters::language`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MkvTrackIdentity {
    /// `Name` (RFC 9559 §5.1.4.1.18) — a human-readable track name. `None`
    /// omits the element (it has no spec default).
    pub name: Option<String>,
    /// `CodecName` (RFC 9559 §5.1.4.1.23) — a human-readable codec name.
    /// `None` omits the element (no spec default).
    pub codec_name: Option<String>,
    /// `Language` (RFC 9559 §5.1.4.1.19), Matroska (ISO 639-2) form. Spec
    /// default `"eng"`. `None` defers to the `StreamInfo`-derived language;
    /// `Some(v)` overrides it. Ignored on disk when `language_bcp47` is
    /// `Some` (the spec says `Language` MUST be ignored in that case).
    pub language: Option<String>,
    /// `LanguageBCP47` (RFC 9559 §5.1.4.1.20, `minver: 4`), BCP-47 form.
    /// `None` omits the element. When `Some`, it supersedes `language`.
    pub language_bcp47: Option<String>,
    /// `FlagEnabled` (RFC 9559 §5.1.4.1.4, default `1`). `None` omits the
    /// element (the demuxer materialises `1`); `Some(v)` writes it.
    pub flag_enabled: Option<bool>,
    /// `FlagDefault` (RFC 9559 §5.1.4.1.5, default `1`). `None` omits the
    /// element; `Some(v)` writes it.
    pub flag_default: Option<bool>,
    /// `FlagLacing` (RFC 9559 §5.1.4.1.12, default `1`). `None` defers to the
    /// muxer's auto-derived value (`1` when a lacing mode is opted in, else
    /// `0`); `Some(v)` overrides it explicitly. Note that setting `Some(true)`
    /// only advertises that the track MAY carry laced Blocks — it does not by
    /// itself enable lacing; use [`MkvMuxer::with_block_lacing`] for that.
    pub flag_lacing: Option<bool>,
    /// `AttachmentLink` (RFC 9559 §5.1.4.1.24, `maxver: 3`) — the `FileUID`
    /// of an attachment this codec uses. Range `not 0`; `None` omits the
    /// element; a `Some(0)` is rejected at queue time.
    pub attachment_link: Option<u64>,
}

impl MkvTrackIdentity {
    /// Convenience constructor that sets only the track `Name` (§5.1.4.1.18).
    pub fn named(name: impl Into<String>) -> Self {
        MkvTrackIdentity {
            name: Some(name.into()),
            ..Default::default()
        }
    }

    /// Convenience constructor that sets only the BCP-47 language
    /// (§5.1.4.1.20). Use the struct fields for the Matroska-form `Language`.
    pub fn language_bcp47(lang: impl Into<String>) -> Self {
        MkvTrackIdentity {
            language_bcp47: Some(lang.into()),
            ..Default::default()
        }
    }

    /// Convenience constructor for the "forced subtitle that is not a default
    /// track" shape: `FlagDefault = 0`. Pair with
    /// [`MkvMuxer::set_track_audience_flags`] to set the §5.1.4.1.6
    /// `FlagForced` itself.
    pub fn non_default() -> Self {
        MkvTrackIdentity {
            flag_default: Some(false),
            ..Default::default()
        }
    }
}

pub struct MkvMuxer {
    output: Box<dyn WriteSeek>,
    streams: Vec<StreamInfo>,
    /// Per-stream MKV track numbers (1-indexed).
    track_numbers: Vec<u64>,
    /// Per-stream running pts, in the stream's own time base. Used to
    /// synthesise per-packet timestamps when the input container only
    /// signals page/chunk granules (e.g. Ogg).
    stream_pts: Vec<i64>,
    cluster_open: bool,
    /// `SilentTracks > SilentTrackNumber` (RFC 9559 Appendix A.1 / A.2)
    /// track numbers to emit on the **next** Cluster the muxer opens, set
    /// via [`MkvMuxer::set_next_cluster_silent_tracks`]. Drained (cleared)
    /// when that Cluster's `SilentTracks` master is written, so the list
    /// applies to exactly one Cluster — matching the per-Cluster scope of
    /// the element (A.2: a track silent here MAY become active in a later
    /// Cluster).
    pending_silent_tracks: Vec<u64>,
    /// Timecode (in ms) at the start of the currently open cluster.
    cluster_timecode_ms: i64,
    /// Byte offset of the currently open cluster header, relative to the
    /// Segment payload start. Used to fill in `CueClusterPosition`.
    cluster_offset_rel: u64,
    /// Absolute file offset of the first byte after the currently open
    /// Cluster element's id+size header — i.e. the "first possible
    /// element position" inside the Cluster, the anchor `CueRelativePosition`
    /// (RFC 9559 §5.1.5.1.2.3) is measured against.
    cluster_body_start_abs: u64,
    /// When `true` (see [`MkvMuxer::with_live_streaming`]), the muxer
    /// emits the RFC 9559 §25.3.4 livestreaming layout: no `SeekHead`, no
    /// `Cues`, and (when combined with `cluster_position_hints`) the
    /// §5.1.3.2 `Position = 0` live convention. Everything other than the
    /// Clusters is still placed before the Clusters, as §25.3.4 requires.
    live_streaming: bool,
    /// When `true` (see [`MkvMuxer::with_cluster_position_hints`]), every
    /// Cluster is written with a `Position` child (RFC 9559 §5.1.3.2 — the
    /// Cluster's own Segment Position, "It might help to resynchronize the
    /// offset on damaged streams") and, from the second Cluster on, a
    /// `PrevSize` child (§5.1.3.3 — "Size of the previous Cluster, in
    /// octets. Can be useful for backward playing").
    cluster_position_hints: bool,
    /// Absolute file offset of the previous Cluster's element ID — the
    /// minuend for the `PrevSize` value. `None` before the first Cluster.
    /// The muxer writes Clusters back-to-back, so the distance between two
    /// consecutive Cluster starts is exactly the previous Cluster's full
    /// element size (ID + size header + body).
    prev_cluster_start_abs: Option<u64>,
    /// Absolute file offset of the Segment payload start (first byte after
    /// the Segment element header). `CueClusterPosition` values are stored
    /// relative to this position, per the Matroska spec.
    segment_data_start: u64,
    /// Cue index built up while writing. One entry per (cluster, track) pair
    /// where the track produced a keyframe in that cluster — plus the first
    /// audio packet of each audio track in each cluster (audio frames are
    /// always decodable on their own, so we index every cluster-start).
    cues: Vec<CueRecord>,
    /// Per-cluster, per-track "already recorded a cue for this" flag —
    /// reset whenever a new cluster opens. Keeps us from emitting a Cue
    /// for every keyframe in a cluster when the first is enough.
    cue_seen_in_cluster: Vec<bool>,
    /// Number of `Block` / `SimpleBlock` elements written to the currently
    /// open Cluster so far. Reset to `0` when a new Cluster opens; the next
    /// block written takes number `cluster_block_count + 1` (RFC 9559
    /// §5.1.5.1.2.5 `CueBlockNumber` is the 1-based "Number of the Block in
    /// the specified Cluster"). Captured into a [`CueRecord`] at the moment
    /// its block is recorded, so the demuxer's typed Cues view round-trips
    /// the value verbatim.
    cluster_block_count: u64,
    /// Absolute file offset of the Seek (Cues) entry inside the SeekHead.
    /// In `write_trailer` we either patch the 8-byte SeekPosition payload
    /// at `seek_cues_entry_offset + SEEK_POS_PAYLOAD_OFFSET` with the real
    /// Cues offset, or rewrite the entire 21-byte Seek as a Void element
    /// if no Cues was actually emitted.
    seek_cues_entry_offset: u64,
    /// True after the muxer has emitted a SeekHead at the start of the
    /// Segment payload. Kept so `write_trailer` can decide whether the
    /// Cues SeekPosition needs patching.
    seek_head_written: bool,
    header_written: bool,
    trailer_written: bool,
    doc_type: DocType,
    /// `DocTypeExtension` declarations (RFC 8794 §11.2.9) queued via
    /// [`MkvMuxer::set_doc_type_extensions`]. Emitted into the EBML header at
    /// `write_header` time, after `DocTypeReadVersion`. Empty (the default)
    /// means none are written — the common case. Reuses the demux-side
    /// [`DocTypeExtension`] record for read/write symmetry.
    doc_type_extensions: Vec<DocTypeExtension>,
    /// Chapter atoms queued via [`MkvMuxer::add_chapter`] /
    /// [`MkvMuxer::add_chapter_full`]. Materialised into a `Chapters`
    /// master right after `Tracks` in [`MkvMuxer::write_header`]; the
    /// `Chapters` SeekHead entry is patched at the same time. Empty list
    /// → no `Chapters` element written and the SeekHead slot is voided.
    chapters: Vec<MkvChapter>,
    /// Attached files queued via [`MkvMuxer::add_attachment`]. Materialised
    /// into an `Attachments` master right after `Chapters` (or right after
    /// `Tracks` if no chapters were queued) in [`MkvMuxer::write_header`];
    /// the `Attachments` SeekHead entry is patched at the same time. Empty
    /// list → no `Attachments` element written and the SeekHead slot is
    /// voided. Honours RFC 9559 §5.1.6 element ordering: the
    /// `Attachments` master sits among the other Top-Level masters
    /// before the first `Cluster`, so the demuxer's single-pass header
    /// walk picks it up without late-segment rescanning.
    attachments: Vec<MkvAttachment>,
    /// `Tag` elements queued via [`MkvMuxer::add_tag`]. Materialised into
    /// a single `Tags` master right after `Attachments` (or after
    /// `Chapters` / `Tracks` when those are absent) in
    /// [`MkvMuxer::write_header`]; the `Tags` SeekHead entry is patched at
    /// the same time. Empty list → no `Tags` element written and the
    /// SeekHead slot is voided. Honours RFC 9559 §5.1.8 / §6 element
    /// ordering: the `Tags` master sits among the other Top-Level masters
    /// before the first `Cluster`, so the demuxer's single-pass header
    /// walk picks it up without a late-segment rescan.
    tags: Vec<MkvTag>,
    /// Linked-Segment `Info` metadata queued via
    /// [`MkvMuxer::set_segment_linking`] (RFC 9559 §5.1.2.1..§5.1.2.8 +
    /// Section 17). `None` (the default) means the muxer emits a plain
    /// standalone Segment with no linking elements — the common case.
    /// When `Some`, the `SegmentUUID` / `PrevUUID` / `NextUUID` /
    /// `SegmentFamily` / `*Filename` / `ChapterTranslate` children are
    /// written into the `Info` master right after `WritingApp`, in the
    /// RFC 9559 §5.1.2 element order. The mux-side mirror of the demux-side
    /// [`MkvDemuxer::segment_linking`](crate::demux::MkvDemuxer::segment_linking)
    /// accessor, so a demuxed [`SegmentLinking`] round-trips through the
    /// muxer unchanged.
    segment_linking: Option<SegmentLinking>,
    /// `Title` (RFC 9559 §5.1.2.12, id `0x7BA9`) — the Segment's general
    /// name, queued via [`MkvMuxer::set_title`]. `None` (the default) omits
    /// the element. Written into the `Info` master between `TimestampScale`
    /// and `MuxingApp`, in §5.1.2 element order.
    info_title: Option<String>,
    /// `DateUTC` (RFC 9559 §5.1.2.11, id `0x4461`) — the Segment creation
    /// date, queued via [`MkvMuxer::set_date_utc_ns`] as nanoseconds since
    /// the Matroska epoch (2001-01-01T00:00:00 UTC). `None` (the default)
    /// omits the element. Written into the `Info` master right after
    /// `Duration`, in §5.1.2 element order.
    info_date_utc_ns: Option<i64>,
    /// `Duration` (RFC 9559 §5.1.2.10, id `0x4489`) — the total Segment
    /// length expressed in `TimestampScale` ticks (the muxer's scale is
    /// 1 ms), queued via [`MkvMuxer::set_duration`]. `None` (the default)
    /// omits the element: the muxer streams `Cluster`s with the unknown-size
    /// VINT and the `Info` master is written up front with a leading `CRC-32`,
    /// so the total length is not known at header time and cannot be patched
    /// later without invalidating that CRC. A caller that knows the total
    /// length ahead of time queues it here and the muxer writes it into the
    /// `Info` master before `DateUTC`, in §5.1.2 element order — inside the
    /// CRC-validated body.
    info_duration_ticks: Option<f64>,
    /// Opt-in block lacing mode (RFC 9559 §10.3). Default is
    /// [`LacingMode::None`] which preserves the legacy
    /// one-SimpleBlock-per-packet behaviour. Anything else causes
    /// [`MkvMuxer::write_packet`] to buffer same-track packets into
    /// a per-track [`LaceBuffer`] and emit them as a single laced
    /// SimpleBlock on the next flush point (track switch, cluster
    /// boundary, write_trailer, lace-cap, or a fixed-size mismatch).
    lacing_mode: LacingMode,
    /// Per-stream packet buffer for lace aggregation. Always empty
    /// when `lacing_mode == LacingMode::None`. At most one stream's
    /// buffer is non-empty at a time — emitting a packet for a
    /// different track flushes whichever buffer was previously
    /// pending. The buffer carries each pending packet's bytes plus
    /// the keyframe flag of the first packet (the SimpleBlock KEY
    /// bit applies to the whole Block).
    lace_pending: Vec<LaceBuffer>,
    /// Per-stream `Video > FlagInterlaced` + `FieldOrder` hint queued via
    /// [`MkvMuxer::set_video_interlacing`] (RFC 9559 §5.1.4.1.28.1 +
    /// §5.1.4.1.28.2). Materialised inside each video track's `Video`
    /// master at `write_header` time, alongside `PixelWidth` /
    /// `PixelHeight`. `None` (the default) means the muxer omits both
    /// elements for that stream, so the demuxer materialises the spec
    /// defaults (FlagInterlaced=0, FieldOrder=2). The slice is sized to
    /// `streams.len()`; non-video tracks must stay `None` (validated at
    /// `set_video_interlacing` time).
    video_interlacings: Vec<Option<VideoInterlacingMux>>,
    /// Per-stream `Video > StereoMode` hint queued via
    /// [`MkvMuxer::set_video_stereo_mode`] (RFC 9559 §5.1.4.1.28.3).
    /// Materialised inside each video track's `Video` master at
    /// `write_header` time. `None` (the default) means the muxer omits
    /// the element entirely, so the demuxer materialises the spec
    /// default `0` ([`StereoMode::Mono`]). The slice is sized to
    /// `streams.len()`; non-video tracks must stay `None` (validated
    /// at `set_video_stereo_mode` time).
    video_stereo_modes: Vec<Option<StereoMode>>,
    /// Per-stream `Video > OldStereoMode` hint queued via
    /// [`MkvMuxer::set_video_old_stereo_mode`] (RFC 9559 §5.1.4.1.28.5,
    /// id `0x53B9`). Materialised inside each video track's `Video` master at
    /// `write_header` time. `None` (the default) means the muxer omits the
    /// element entirely — the overwhelmingly common case, since a Writer MUST
    /// NOT emit this legacy element for new files. It exists only so a faithful
    /// re-mux of a Matroska v2 / libmatroska-bug source can round-trip the
    /// element the demuxer surfaced. The slice is sized to `streams.len()`;
    /// non-video tracks must stay `None` (validated at
    /// `set_video_old_stereo_mode` time).
    video_old_stereo_modes: Vec<Option<OldStereoMode>>,
    /// Per-stream `Video > AlphaMode` hint queued via
    /// [`MkvMuxer::set_video_alpha_mode`] (RFC 9559 §5.1.4.1.28.4).
    /// Materialised inside each video track's `Video` master at
    /// `write_header` time. `None` (the default) means the muxer omits
    /// the element entirely, so the demuxer materialises the spec
    /// default `0` ([`AlphaMode::None`]). The slice is sized to
    /// `streams.len()`; non-video tracks must stay `None` (validated
    /// at `set_video_alpha_mode` time).
    video_alpha_modes: Vec<Option<AlphaMode>>,
    /// Per-stream `Video > PixelCrop{Top,Bottom,Left,Right}` +
    /// `DisplayWidth` / `DisplayHeight` / `DisplayUnit` hint queued via
    /// [`MkvMuxer::set_video_geometry`] (RFC 9559
    /// §5.1.4.1.28.8..§5.1.4.1.28.14). Materialised inside each video
    /// track's `Video` master at `write_header` time, alongside
    /// `PixelWidth` / `PixelHeight`. `None` (the default) means the
    /// muxer omits all seven geometry elements for that stream, so the
    /// demuxer materialises the spec defaults (zero crops, derived
    /// display dimensions, `DisplayUnit::Pixels`). The slice is sized
    /// to `streams.len()`; non-video tracks must stay `None` (validated
    /// at `set_video_geometry` time).
    video_geometries: Vec<Option<MkvVideoGeometry>>,
    /// Per-stream `Video > UncompressedFourCC` hint queued via
    /// [`MkvMuxer::set_video_uncompressed_fourcc`] (RFC 9559
    /// §5.1.4.1.28.15). Materialised inside each video track's `Video`
    /// master at `write_header` time as a 4-byte `binary` element
    /// (id `0x2EB524`, fixed `length: 4`). `None` (the default) means
    /// the muxer omits the element for that stream — legal for any
    /// track whose `CodecID` is not `V_UNCOMPRESSED`, since
    /// §5.1.4.1.28.15 Table 11 only pins `minOccurs=1` for that one
    /// codec id. The slice is sized to `streams.len()`; non-video
    /// tracks must stay `None` (validated at
    /// `set_video_uncompressed_fourcc` time).
    video_uncompressed_fourccs: Vec<Option<[u8; 4]>>,
    /// Per-stream `Video > AspectRatioType` hint queued via
    /// [`MkvMuxer::set_video_aspect_ratio_type`] (RFC 9559 Appendix A.24,
    /// reclaimed, id `0x54B3`). Materialised inside each video track's
    /// `Video` master at `write_header` time as a `uinteger` element
    /// carrying the raw value verbatim. `None` (the default) means the
    /// muxer omits the element for that stream — the reclaimed appendix
    /// defines no default, so the demuxer surfaces `None` in that case.
    /// The slice is sized to `streams.len()`; non-video tracks must stay
    /// `None` (validated at `set_video_aspect_ratio_type` time).
    video_aspect_ratio_types: Vec<Option<u64>>,
    /// Per-stream `Video > Colour` master hint queued via
    /// [`MkvMuxer::set_video_colour`] (RFC 9559 §5.1.4.1.28.16). The
    /// `MasteringMetadata` sub-master
    /// (§5.1.4.1.28.30..§5.1.4.1.28.40) is emitted whenever the queued
    /// colour hint's [`MkvVideoColour::mastering_metadata`] slot is
    /// `Some(_)`; each of its ten chromaticity / luminance children is
    /// written only when its own `Option<f64>` slot is `Some(v)`.
    /// Materialised inside each video track's `Video` master at
    /// `write_header` time as a `Colour` master carrying the scalar
    /// children that differ from the §5.1.4.1.28.17..§5.1.4.1.28.27
    /// spec defaults. `None` (the default) means the muxer omits the
    /// `Colour` master entirely so the demuxer surfaces `None` from
    /// `video_colour` for that stream. The slice is sized to
    /// `streams.len()`; non-video tracks must stay `None` (validated
    /// at `set_video_colour` time).
    ///
    video_colours: Vec<Option<MkvVideoColour>>,
    /// Per-stream `Video > Projection` master hint queued via
    /// [`MkvMuxer::set_video_projection`] (RFC 9559 §5.1.4.1.28.41).
    /// Materialised inside each video track's `Video` master at
    /// `write_header` time as a `Projection` master (id `0x7670`), after
    /// the `Colour` master, carrying the `ProjectionType` /
    /// `ProjectionPrivate` / `ProjectionPose{Yaw,Pitch,Roll}` children
    /// that differ from their §5.1.4.1.28.42..46 spec defaults. `None`
    /// (the default) means the muxer omits the `Projection` master
    /// entirely so the demuxer surfaces `None` from `video_projection`
    /// for that stream. The slice is sized to `streams.len()`; non-video
    /// tracks must stay `None` (validated at `set_video_projection` time).
    video_projections: Vec<Option<MkvProjection>>,
    /// Per-stream audience-flag hints queued via
    /// [`MkvMuxer::set_track_audience_flags`] (RFC 9559
    /// §5.1.4.1.6..§5.1.4.1.11). Materialised directly inside each
    /// `TrackEntry` (the six elements sit on `TrackEntry` itself, not in
    /// a sub-master) at `write_header` time, right after `FlagLacing`.
    /// `None` (the default) means the muxer omits all six elements for
    /// that stream, so the demuxer materialises the §5.1.4.1.6 default
    /// `0` for `FlagForced` and surfaces `None` for the five
    /// default-less `minver: 4` flags. Unlike the `Video`-master hints
    /// above, the slice accepts a value on ANY track type — the spec
    /// carries the elements on every `TrackEntry`.
    track_audience_flags: Vec<Option<MkvTrackAudienceFlags>>,
    /// Per-stream `MaxBlockAdditionID` hints queued via
    /// [`MkvMuxer::set_max_block_addition_id`] (RFC 9559 §5.1.4.1.16).
    /// `None` (the default) means the muxer omits the element, so the
    /// demuxer materialises the spec default `0` ("there is no
    /// BlockAdditions for this track") — and
    /// [`MkvMuxer::write_packet_with_additions`] rejects the stream.
    /// `Some(v)` writes the element explicitly, even for `v == 0` (the
    /// explicit producer-override path, byte-distinct from omission but
    /// decoding identically).
    max_block_addition_ids: Vec<Option<u64>>,
    /// Per-stream `BlockAdditionMapping` masters queued via
    /// [`MkvMuxer::set_block_addition_mappings`] (RFC 9559 §5.1.4.1.17).
    /// Each entry is written as a `BlockAdditionMapping` master directly
    /// inside the carrying `TrackEntry` at `write_header` time, in slice
    /// order. The default (an empty `Vec`) omits the element for that
    /// stream, so the demuxer surfaces an empty slice from
    /// `block_addition_mappings`. Sized to `streams.len()`.
    block_addition_mappings: Vec<Vec<crate::demux::BlockAdditionMapping>>,
    /// Per-stream `Audio` master hints queued via
    /// [`MkvMuxer::set_track_audio`] (RFC 9559 §5.1.4.1.29). `None` (the
    /// default) means the muxer derives the `Audio` master's children
    /// from the stream's `StreamInfo` alone (`sample_rate` /
    /// `channels` / `sample_format`). `Some(_)` overrides the derived
    /// children per the hint's `Some` fields and adds the
    /// `OutputSamplingFrequency` SBR child the `StreamInfo`-derived path
    /// can't express. The slice is sized to `streams.len()`; non-audio
    /// tracks must stay `None` (validated at `set_track_audio` time).
    track_audio: Vec<Option<MkvTrackAudio>>,
    /// Per-stream `TrackEntry` timing hints queued via
    /// [`MkvMuxer::set_track_timing`] (RFC 9559 §5.1.4.1.13..§5.1.4.1.15).
    /// `None` (the default) means the muxer omits all three elements, so
    /// the demuxer surfaces `DefaultDuration` / `DefaultDecodedFieldDuration`
    /// as `None` and materialises the §5.1.4.1.15 `TrackTimestampScale`
    /// default `1.0`. `Some(_)` writes each populated child explicitly. The
    /// slice is sized to `streams.len()`.
    track_timing: Vec<Option<MkvTrackTiming>>,
    /// Per-stream `TrackEntry` codec-timing hints queued via
    /// [`MkvMuxer::set_track_codec_timing`] (RFC 9559 §5.1.4.1.25 /
    /// §5.1.4.1.26 — `CodecDelay` / `SeekPreRoll`). `None` (the default)
    /// means the muxer omits both elements unless it auto-derives them for
    /// an Opus track; `Some(_)` overrides the auto-derived value per field.
    /// The slice is sized to `streams.len()`.
    track_codec_timing: Vec<Option<MkvTrackCodecTiming>>,
    /// Per-stream `TrackEntry` identity / selection hints queued via
    /// [`MkvMuxer::set_track_identity`] (RFC 9559 §5.1.4.1.18 / .19 / .20 /
    /// .23 / .4 / .5 / .12 / .24). `None` (the default) means the muxer omits
    /// the optional identity strings / `AttachmentLink`, lets the demuxer
    /// materialise the §default `1` for the three selection flags, and falls
    /// back to the `StreamInfo`-derived `Language` / auto-derived `FlagLacing`.
    /// `Some(_)` writes each populated child explicitly. The slice is sized to
    /// `streams.len()`.
    track_identity: Vec<Option<MkvTrackIdentity>>,
    /// Per-stream timestamp (ms, = track ticks at the muxer's 1 ms
    /// `TimestampScale`) of the most recently written Block. Used to
    /// derive the `ReferenceBlock` (RFC 9559 §5.1.3.5.5) relative value
    /// when a non-keyframe packet is written through the `BlockGroup`
    /// path — "Historically, Matroska Writers didn't write the actual
    /// Block(s) that this Block depends on, but they did write some
    /// Block(s) in the past."
    last_block_pts_ms: Vec<Option<i64>>,
    /// Per-stream `TrackOperation` hint queued via
    /// [`MkvMuxer::set_track_operation`] (RFC 9559 §5.1.4.1.30).
    /// Materialised as a `TrackOperation` master directly inside the
    /// carrying `TrackEntry` (sibling to `Video` / `Audio`) at
    /// `write_header` time. `None` (the default) means the muxer omits the
    /// element for that stream, so the demuxer surfaces `None` from
    /// `track_operation`. Each plane / join reference's 0-indexed stream
    /// index is resolved to the source track's `TrackUID` at write time.
    /// The slice is sized to `streams.len()`.
    track_operations: Vec<Option<MkvTrackOperation>>,
    /// Per-stream `ContentEncodings` hint queued via
    /// [`MkvMuxer::set_track_content_encodings`] (RFC 9559 §5.1.4.1.31).
    /// Materialised as a `ContentEncodings` master directly inside the
    /// carrying `TrackEntry` at `write_header` time. `None` (the default)
    /// means the muxer omits the element for that stream, so the demuxer
    /// surfaces `None` from `content_encodings`. The slice is sized to
    /// `streams.len()`.
    content_encodings: Vec<Option<ContentEncodings>>,
    /// Per-stream `TrackTranslate` hints queued via
    /// [`MkvMuxer::set_track_translates`] (RFC 9559 §5.1.4.1.27).
    /// Materialised as zero or more `TrackTranslate` masters directly inside
    /// the carrying `TrackEntry` at `write_header` time. The default (an
    /// empty `Vec`) means the muxer omits the element for that stream, so the
    /// demuxer surfaces an empty slice from `track_translates`. The slice is
    /// sized to `streams.len()`.
    track_translates: Vec<Vec<MkvTrackTranslate>>,
    /// Per-stream reclaimed Appendix-A `TrackLegacy` hints queued via
    /// [`MkvMuxer::set_track_legacy`] (RFC 9559 Appendix A.19..A.23 +
    /// A.28..A.32). Materialised as the populated legacy elements directly
    /// inside the carrying `TrackEntry` at `write_header` time. `None` (the
    /// default) means the muxer omits all of them for that stream, so the
    /// demuxer surfaces `None` from `track_legacy`. The slice is sized to
    /// `streams.len()`.
    track_legacy: Vec<Option<MkvTrackLegacy>>,
}

/// One `TrackTranslate` mapping (RFC 9559 §5.1.4.1.27) queued for a track via
/// [`MkvMuxer::set_track_translates`]. The mux-side mirror of the demux-side
/// [`TrackTranslate`](crate::demux::TrackTranslate): a mux→demux pipeline
/// round-trips every field verbatim.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MkvTrackTranslate {
    /// `TrackTranslateTrackID` (§5.1.4.1.27.1, binary, `minOccurs: 1`) — the
    /// value the chapter codec uses to name this track. Written verbatim; its
    /// format is defined by the chapter codec (`codec`), not by Matroska. Must
    /// be non-empty (the spec marks the child mandatory).
    pub track_id: Vec<u8>,
    /// `TrackTranslateCodec` (§5.1.4.1.27.2, uinteger, `minOccurs: 1`) — the
    /// chapter codec the mapping applies to (Table 31: `0` = Matroska Script,
    /// `1` = DVD-menu).
    pub codec: u64,
    /// `TrackTranslateEditionUID` (§5.1.4.1.27.3, uinteger, unbounded) — the
    /// chapter editions the mapping applies to. An empty list means "all
    /// editions using `codec`" per the §5.1.4.1.27.3 usage note. Spec ranges
    /// each value "not 0"; a zero is rejected at queue time.
    pub edition_uids: Vec<u64>,
}

impl MkvTrackTranslate {
    /// Convenience constructor for the common shape: a chapter-codec track-id
    /// plus its codec, applying to all editions (empty `edition_uids`).
    pub fn new(track_id: impl Into<Vec<u8>>, codec: u64) -> Self {
        Self {
            track_id: track_id.into(),
            codec,
            edition_uids: Vec::new(),
        }
    }
}

/// The reclaimed Appendix-A `TrackEntry`-level legacy elements (RFC 9559
/// Appendix A.19..A.23 + A.28..A.32) queued for a track via
/// [`MkvMuxer::set_track_legacy`]. The mux-side mirror of the demux-side
/// [`TrackLegacy`](crate::demux::TrackLegacy): a mux→demux pipeline
/// round-trips every populated field verbatim.
///
/// These are historical Matroska `TrackEntry` children the RFC 9559 core
/// body no longer documents but whose Element IDs remain reserved. The
/// muxer writes only the fields the caller populated — an absent field
/// (`None` / empty `Vec`) keeps its element off-disk, so the demuxer
/// observes the same absence. None carries a spec default, so there is no
/// "default suppression" rule: a `Some(0)` `decode_all` / `trick_track_flag`
/// is written as an explicit `0`, distinct from omission.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MkvTrackLegacy {
    /// `CodecSettings` (Appendix A.19, utf-8) — a string describing the
    /// encoding settings used. `None` → element omitted.
    pub codec_settings: Option<String>,
    /// `CodecInfoURL` (Appendix A.20, string) — URLs with information about
    /// the codec, written in slice order. Empty → no element.
    pub codec_info_urls: Vec<String>,
    /// `CodecDownloadURL` (Appendix A.21, string) — URLs to download the
    /// codec, written in slice order. Empty → no element.
    pub codec_download_urls: Vec<String>,
    /// `CodecDecodeAll` (Appendix A.22, uinteger) — `1` if the codec can
    /// decode damaged data. `None` → element omitted; `Some(0)` writes an
    /// explicit `0`.
    pub decode_all: Option<u64>,
    /// `MinCache` (Appendix A.16, uinteger) — the minimum number of frames a
    /// player should cache. `None` → element omitted; `Some(0)` writes an
    /// explicit `0`.
    pub min_cache: Option<u64>,
    /// `MaxCache` (Appendix A.17, uinteger) — the maximum cache size needed.
    /// `None` → element omitted; `Some(0)` writes an explicit `0`.
    pub max_cache: Option<u64>,
    /// `TrackOffset` (Appendix A.18, signed integer) — a Matroska-Tick value
    /// added to each Block's Timestamp to adjust the track's playback offset.
    /// `None` → element omitted; `Some(0)` writes an explicit `0`.
    pub track_offset: Option<i64>,
    /// `GammaValue` (Appendix A.25, float) — the gamma value, written inside
    /// the `Video` master. `None` → omitted. Only meaningful on a video track.
    pub gamma_value: Option<f64>,
    /// `FrameRate` (Appendix A.26, float) — informational fps, written inside
    /// the `Video` master. `None` → omitted. Only meaningful on a video track.
    pub frame_rate: Option<f64>,
    /// `ChannelPositions` (Appendix A.27, binary) — a table of horizontal
    /// angles per channel, written inside the `Audio` master. `None` →
    /// omitted. Only meaningful on an audio track.
    pub channel_positions: Option<Vec<u8>>,
    /// `TrackOverlay` (Appendix A.23, uinteger) — the *ordered* overlay-track
    /// fallback `TrackNumber`s. Written in slice order; the appendix makes the
    /// order load-bearing (first entry preferred). Empty → no element.
    pub track_overlays: Vec<u64>,
    /// `TrickTrackUID` (Appendix A.28, uinteger). `None` → omitted.
    pub trick_track_uid: Option<u64>,
    /// `TrickTrackSegmentUID` (Appendix A.29, binary) — the SegmentUUID of the
    /// Segment containing [`Self::trick_track_uid`]. Written verbatim; `None`
    /// → omitted.
    pub trick_track_segment_uid: Option<Vec<u8>>,
    /// `TrickTrackFlag` (Appendix A.30, uinteger) — `1` if this track is a
    /// Smooth FF/RW track. `None` → omitted; `Some(0)` writes an explicit `0`.
    pub trick_track_flag: Option<u64>,
    /// `TrickMasterTrackUID` (Appendix A.31, uinteger). `None` → omitted.
    pub trick_master_track_uid: Option<u64>,
    /// `TrickMasterTrackSegmentUID` (Appendix A.32, binary). Written verbatim;
    /// `None` → omitted.
    pub trick_master_track_segment_uid: Option<Vec<u8>>,
}

impl MkvTrackLegacy {
    /// `true` when no field is populated — queuing such a record is a no-op
    /// (no element reaches the disk). Mirrors
    /// [`TrackLegacy::is_empty`](crate::demux::TrackLegacy::is_empty).
    pub fn is_empty(&self) -> bool {
        self.codec_settings.is_none()
            && self.codec_info_urls.is_empty()
            && self.codec_download_urls.is_empty()
            && self.decode_all.is_none()
            && self.min_cache.is_none()
            && self.max_cache.is_none()
            && self.track_offset.is_none()
            && self.gamma_value.is_none()
            && self.frame_rate.is_none()
            && self.channel_positions.is_none()
            && self.track_overlays.is_empty()
            && self.trick_track_uid.is_none()
            && self.trick_track_segment_uid.is_none()
            && self.trick_track_flag.is_none()
            && self.trick_master_track_uid.is_none()
            && self.trick_master_track_segment_uid.is_none()
    }
}

/// Per-stream packet aggregation buffer used when lacing is on.
/// Holds the in-flight frames for one track up to the point where
/// the muxer decides to flush the lace — either because the next
/// packet has different track / keyframe / cluster / size
/// properties, or because the per-Block frame cap was reached.
#[derive(Clone, Debug, Default)]
struct LaceBuffer {
    /// Encoded frame payloads queued for this track in arrival
    /// order. First frame's timestamp becomes the Block timestamp.
    frames: Vec<Vec<u8>>,
    /// Cluster-relative timecode (ms) of the first frame, captured
    /// at append time. Lacing flushes carry this verbatim into the
    /// SimpleBlock header.
    first_timecode_offset: i16,
    /// KEY bit value to write on the resulting SimpleBlock —
    /// inherits from the first frame in the lace.
    keyframe: bool,
}

/// Soft cap on frames-per-Block. The on-disk format permits 256
/// (lacing head is `n_frames - 1`, max 255 → 256 frames). 8 keeps
/// individual Blocks bounded and matches the "small frames"
/// recommendation in RFC 9559 §10.3.
const MAX_FRAMES_PER_LACE: usize = 8;

/// One chapter atom as fed to the muxer.
///
/// Round-trips through `Chapters → EditionEntry → ChapterAtom` per RFC
/// 9559 §5.1.7. Timestamps are in nanoseconds (matches
/// `ChapterTimeStart` / `ChapterTimeEnd` units, which are spec-defined as
/// ns and independent of the segment's `TimecodeScale`).
///
/// `end_time_ns == None` is permitted — the muxer simply omits
/// `ChapterTimeEnd`. The demuxer surfaces such an atom without an
/// `end_ms` metadata key, matching ffprobe behaviour on real files.
#[derive(Clone, Debug)]
pub struct MkvChapter {
    /// `ChapterTimeStart`, in nanoseconds.
    pub time_start_ns: u64,
    /// `ChapterTimeEnd`, in nanoseconds. `None` → element omitted.
    pub time_end_ns: Option<u64>,
    /// `ChapterUID` (RFC 9559 §5.1.7.1.4.1, `range: not 0`). `None` →
    /// the muxer auto-derives a stable non-zero UID from the atom's
    /// 1-based index (so `Tags.Targets.TagChapterUID` references can
    /// resolve). `Some(0)` is rejected at queue time.
    pub uid: Option<u64>,
    /// `ChapterStringUID` (RFC 9559 §5.1.7.1.4.2). `None` (or `Some("")`)
    /// → element omitted.
    pub string_uid: Option<String>,
    /// `ChapterFlagHidden` (RFC 9559 §5.1.7.1.4.5, default `0`). Written
    /// only when `true` (the spec default may stay off-disk).
    pub hidden: bool,
    /// `ChapterFlagEnabled` (RFC 9559 §5.1.7.1.4, default `1`). Written
    /// only when `false` — the default `1` stays off-disk and the demuxer
    /// materialises it. Defaults to `true` via [`MkvChapter::default`].
    pub enabled: bool,
    /// `ChapterSegmentUUID` (RFC 9559 §5.1.7.1.4.6) — the 16-byte
    /// SegmentUUID of another Segment to play during this chapter. `None`
    /// → omitted; a non-16-byte value is rejected at queue time.
    pub segment_uuid: Option<Vec<u8>>,
    /// `ChapterSegmentEditionUID` (RFC 9559 §5.1.7.1.4.7, `range: not 0`).
    /// `None` → omitted.
    pub segment_edition_uid: Option<u64>,
    /// `ChapterPhysicalEquiv` (RFC 9559 §5.1.7.1.4.8). `None` → omitted.
    pub physical_equiv: Option<u64>,
    /// Zero or more `ChapterDisplay` children. Each one carries one
    /// language-tagged title string. A chapter with zero displays is
    /// legal per RFC 9559 §5.1.7 but produces an "untitled" atom that
    /// most players surface as `Chapter N` — the convenience constructor
    /// [`MkvMuxer::add_chapter`] always emits exactly one display.
    pub display: Vec<ChapterDisplay>,
    /// Zero or more `ChapProcess` masters (RFC 9559 §5.1.7.1.4.14) — the
    /// chapter-codec commands (DVD-menu / Matroska-Script) attached to
    /// this atom. Empty → no `ChapProcess` child written.
    pub chap_processes: Vec<MkvChapProcess>,
}

impl Default for MkvChapter {
    fn default() -> Self {
        Self {
            time_start_ns: 0,
            time_end_ns: None,
            uid: None,
            string_uid: None,
            hidden: false,
            // RFC 9559 §5.1.7.1.4: ChapterFlagEnabled has spec default 1.
            enabled: true,
            segment_uuid: None,
            segment_edition_uid: None,
            physical_equiv: None,
            display: Vec::new(),
            chap_processes: Vec::new(),
        }
    }
}

/// One `ChapProcess` master (RFC 9559 §5.1.7.1.4.14) queued on an
/// [`MkvChapter`] — the write-side mirror of
/// [`crate::demux::ChapProcess`]. The container never executes a chapter
/// command; it writes the raw payloads verbatim.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MkvChapProcess {
    /// `ChapProcessCodecID` (RFC 9559 §5.1.7.1.4.15) — `0` = Matroska
    /// Script, `1` = DVD-menu. Always written (mandatory; the demuxer
    /// materialises the `0` default, but we emit it explicitly so a
    /// round-trip preserves the exact on-disk shape).
    pub codec_id: u64,
    /// `ChapProcessPrivate` (RFC 9559 §5.1.7.1.4.16) — optional codec
    /// data. `None` → omitted.
    pub private: Option<Vec<u8>>,
    /// `ChapProcessCommand` masters (RFC 9559 §5.1.7.1.4.17), in write
    /// order.
    pub commands: Vec<MkvChapProcessCommand>,
}

/// One `ChapProcessCommand` master (RFC 9559 §5.1.7.1.4.17) — the
/// write-side mirror of [`crate::demux::ChapProcessCommand`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MkvChapProcessCommand {
    /// `ChapProcessTime` (RFC 9559 §5.1.7.1.4.18) — `0` during the whole
    /// chapter, `1` before playback, `2` after. Always written.
    pub time: u64,
    /// `ChapProcessData` (RFC 9559 §5.1.7.1.4.19) — the command payload,
    /// written verbatim.
    pub data: Vec<u8>,
}

/// One `ChapterDisplay` row — a chapter title in one language.
///
/// `language` follows the `ChapLanguage` element convention (RFC 9559
/// §5.1.7.4.1): 3-letter ISO-639-2 alpha-3 code (`"eng"`, `"jpn"`,
/// `"fre"`, …). Use `"und"` for "undetermined", which is also the
/// default `ChapLanguage` value when the element is omitted entirely.
/// `country`, when set, follows RFC 9559 §5.1.7.4.2 (`ChapCountry`,
/// IETF BCP 47 region subtag, e.g. `"us"`, `"jp"`).
///
/// `language_bcp47`, when set, writes the modern `ChapLanguageBCP47`
/// element (RFC 9559 §5.1.7.1.4.11, id `0x437D`) — a full IETF BCP 47
/// language tag (e.g. `"en-US"`, `"pt-BR"`). Per the spec, when
/// `ChapLanguageBCP47` is present any `ChapLanguage` element MUST be
/// ignored, so the muxer writes **only** `ChapLanguageBCP47` in that case
/// (mirroring how `LanguageBCP47` / `TagLanguageBCP47` are handled).
#[derive(Clone, Debug, Default)]
pub struct ChapterDisplay {
    /// `ChapString` — UTF-8 title text.
    pub title: String,
    /// `ChapLanguage` — ISO-639-2 alpha-3 code (e.g. `"eng"`). Pass
    /// `"und"` if no specific language applies. Ignored (not written) when
    /// `language_bcp47` is `Some`.
    pub language: String,
    /// Optional `ChapCountry` — BCP 47 region subtag (e.g. `"us"`).
    /// Skipped when `None`.
    pub country: Option<String>,
    /// Optional `ChapLanguageBCP47` (RFC 9559 §5.1.7.1.4.11) — a full IETF
    /// BCP 47 language tag. When `Some`, the muxer writes this element and
    /// suppresses `ChapLanguage`. Skipped when `None`.
    pub language_bcp47: Option<String>,
}

impl ChapterDisplay {
    /// Convenience constructor: `language` is `"und"`, `country` and
    /// `language_bcp47` are `None`.
    pub fn untitled_in(language: impl Into<String>) -> Self {
        Self {
            title: String::new(),
            language: language.into(),
            country: None,
            language_bcp47: None,
        }
    }
}

/// One `AttachedFile` (RFC 9559 §5.1.6) queued for the muxer to emit.
///
/// Round-trips through `Attachments → AttachedFile → {FileName,
/// FileMediaType, FileData, FileUID, FileDescription}` per RFC 9559
/// §5.1.6.1. Mirrors the demux-side [`crate::demux::Attachment`] surface
/// so a demux-then-mux pipeline can read an `Attachment` out and feed an
/// `MkvAttachment` back in without losing any fields.
///
/// Field handling per the spec:
///
/// * `filename` — `FileName` (§5.1.6.1.2), mandatory per the spec
///   (`minOccurs / maxOccurs: 1 / 1`). Always written, even when empty
///   (the spec does not provide a default).
/// * `mime_type` — `FileMediaType` (§5.1.6.1.3), mandatory. RFC 6838
///   media-type string (e.g. `"image/jpeg"`, `"application/x-truetype-
///   font"`). Always written.
/// * `data` — `FileData` (§5.1.6.1.4), mandatory binary payload. Written
///   verbatim. A zero-length payload is legal per the spec but unusual.
/// * `uid` — `FileUID` (§5.1.6.1.5), mandatory uinteger with `range: not
///   0`. When `None` the muxer auto-derives a stable non-zero UID from
///   the 1-based attachment index (so `Tags.Targets.TagAttachmentUID`
///   references can resolve); when `Some(0)` the muxer rejects the call
///   per the spec's `range: not 0` constraint.
/// * `description` — `FileDescription` (§5.1.6.1.1), optional. Written
///   only when `Some(non_empty)`; an empty `Some("")` is treated as
///   `None` to avoid emitting an empty UTF-8 element.
///
/// The on-disk order matches the demux side's parse order for clean
/// round-tripping under the typed [`crate::demux::Attachment`] accessor.
#[derive(Clone, Debug, Default)]
pub struct MkvAttachment {
    /// `FileName` (RFC 9559 §5.1.6.1.2). Mandatory on disk.
    pub filename: String,
    /// `FileMediaType` (RFC 9559 §5.1.6.1.3). RFC 6838 media-type
    /// string. Mandatory on disk.
    pub mime_type: String,
    /// `FileData` (RFC 9559 §5.1.6.1.4). The verbatim file payload —
    /// font bytes, cover-art image, etc. Mandatory on disk.
    pub data: Vec<u8>,
    /// `FileUID` (RFC 9559 §5.1.6.1.5). `range: not 0`. `None` → muxer
    /// auto-derives a stable non-zero UID from the attachment's 1-based
    /// index.
    pub uid: Option<u64>,
    /// `FileDescription` (RFC 9559 §5.1.6.1.1). Optional human-readable
    /// note. `None` (or `Some("")`) → element omitted on disk.
    pub description: Option<String>,
    /// `FileReferral` (RFC 9559 Appendix A.40, binary) — reclaimed legacy
    /// element. `None` → omitted; `Some(bytes)` written verbatim (an empty
    /// `Some(vec![])` still emits a zero-length element so a demux→mux
    /// round-trip of a present-but-empty referral is faithful).
    pub referral: Option<Vec<u8>>,
    /// `FileUsedStartTime` (RFC 9559 Appendix A.41, uinteger) — reclaimed
    /// legacy element. `None` → omitted; `Some(v)` written (including a
    /// present `0`).
    pub used_start_time: Option<u64>,
    /// `FileUsedEndTime` (RFC 9559 Appendix A.42, uinteger) — reclaimed
    /// legacy element. `None` → omitted; `Some(v)` written (including a
    /// present `0`).
    pub used_end_time: Option<u64>,
}

impl MkvAttachment {
    /// Convenience constructor mirroring [`MkvMuxer::add_chapter`]'s
    /// shape: only the three mandatory on-disk fields. UID is
    /// auto-derived from the 1-based attachment index; description is
    /// omitted.
    pub fn new(
        filename: impl Into<String>,
        mime_type: impl Into<String>,
        data: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            filename: filename.into(),
            mime_type: mime_type.into(),
            data: data.into(),
            uid: None,
            description: None,
            referral: None,
            used_start_time: None,
            used_end_time: None,
        }
    }
}

/// One `Tag` element (RFC 9559 §5.1.8.1) queued for the muxer to emit
/// inside the file's single `Tags` master.
///
/// A `Tag` pairs a [`MkvTagTargets`] scope (which tracks / editions /
/// chapters / attachments — or the whole Segment — the metadata applies
/// to) with one or more [`MkvSimpleTag`] `(name, value)` descriptors
/// (RFC 9559 §5.1.8.1.2). Mirrors the demux-side [`crate::demux::Tag`]
/// surface field-for-field so a demux-then-mux pipeline can read a `Tag`
/// out and feed an `MkvTag` back in without losing scope, language,
/// default-flag, or binary-payload information.
///
/// Element layout written by the muxer (RFC 9559 §5.1.8.1):
///
/// ```text
/// Tag (0x7373)
///   Targets (0x63C0)                 — always emitted (minOccurs 1)
///     TargetTypeValue (0x68CA)       — only when Some and != 50 default
///     TargetType (0x63CA)            — only when Some(non-empty)
///     TagTrackUID (0x63C5) × N       — non-zero UIDs only
///     TagEditionUID (0x63C9) × N
///     TagChapterUID (0x63C4) × N
///     TagAttachmentUID (0x63C6) × N
///   SimpleTag (0x67C8) × M           — see MkvSimpleTag
/// ```
#[derive(Clone, Debug, Default)]
pub struct MkvTag {
    /// Scope of this tag — see [`MkvTagTargets`]. An all-default
    /// `MkvTagTargets` produces a bare `Targets` master (global scope).
    pub targets: MkvTagTargets,
    /// One or more `(name, value)` descriptors that share this `Targets`
    /// scope (RFC 9559 §5.1.8.1.2, `minOccurs: 1`). Written in list
    /// order.
    pub simple_tags: Vec<MkvSimpleTag>,
}

impl MkvTag {
    /// Convenience constructor for a global-scope tag (empty `Targets`)
    /// carrying a single `(name, string-value)` descriptor — the common
    /// case for Segment-level metadata such as `TITLE` / `ARTIST`.
    pub fn global(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            targets: MkvTagTargets::default(),
            simple_tags: vec![MkvSimpleTag::new(name, value)],
        }
    }
}

/// `Targets` master (RFC 9559 §5.1.8.1.1) — the scope of an [`MkvTag`].
///
/// An all-default value (every field empty / `None`) is the global scope
/// "the tag value describes everything in the Segment" (§5.1.8.1.1). The
/// UID lists let one `Tag` scope to several tracks / editions / chapters
/// / attachments at once (the spec gives each UID element no `maxOccurs`).
///
/// UID semantics per RFC 9559 §5.1.8.1.1.3..§5.1.8.1.1.6: a UID of `0`
/// means "all of that kind in the Segment", so the muxer never writes a
/// `0` UID element (it would be redundant with omission and the spec
/// already covers the all-scope case by omission). The caller queues
/// only the concrete non-zero UIDs it wants to scope to; a `0` in any
/// list is dropped at write time.
#[derive(Clone, Debug, Default)]
pub struct MkvTagTargets {
    /// `TargetTypeValue` (RFC 9559 §5.1.8.1.1.1, Table 33). `range: not
    /// 0`. `None` → element omitted (the demuxer surfaces `None`, which
    /// is distinct from the spec default `50`). `Some(50)` is written
    /// explicitly only when the caller wants the default materialised;
    /// see [`MkvTagTargets::target_type_value`] handling in the writer.
    pub target_type_value: Option<u64>,
    /// `TargetType` informational string (RFC 9559 §5.1.8.1.1.2) — e.g.
    /// `"ALBUM"`, `"MOVIE"`, `"TRACK"`. `None` / empty → omitted.
    pub target_type: Option<String>,
    /// `TagTrackUID` list (RFC 9559 §5.1.8.1.1.3). Non-zero `TrackUID`s
    /// this tag scopes to. Zero entries are dropped (a `0` means "all
    /// tracks", which the spec already expresses by omission).
    pub track_uids: Vec<u64>,
    /// `TagEditionUID` list (RFC 9559 §5.1.8.1.1.4).
    pub edition_uids: Vec<u64>,
    /// `TagChapterUID` list (RFC 9559 §5.1.8.1.1.5).
    pub chapter_uids: Vec<u64>,
    /// `TagAttachmentUID` list (RFC 9559 §5.1.8.1.1.6).
    pub attachment_uids: Vec<u64>,
}

impl MkvTagTargets {
    /// `true` when this is the global scope — no UID lists and no
    /// informational `TargetType` / `TargetTypeValue`. Produces a bare
    /// `Targets` master on disk.
    pub fn is_global(&self) -> bool {
        self.target_type_value.is_none()
            && self.target_type.is_none()
            && self.track_uids.is_empty()
            && self.edition_uids.is_empty()
            && self.chapter_uids.is_empty()
            && self.attachment_uids.is_empty()
    }

    /// Convenience constructor scoping a tag to a single `TrackUID`
    /// (RFC 9559 §5.1.8.1.1.3). The muxer assigns each track a
    /// `TrackUID` equal to its 1-based track number, so a stream at
    /// index `i` has `TrackUID == i + 1`.
    pub fn track(track_uid: u64) -> Self {
        Self {
            track_uids: vec![track_uid],
            ..Self::default()
        }
    }
}

/// One `SimpleTag` element (RFC 9559 §5.1.8.1.2) queued for the muxer.
///
/// Mirrors the demux-side [`crate::demux::SimpleTag`] surface, plus the
/// `children` list for the spec's `recursive: True` nesting
/// (§5.1.8.1.2) — a `SimpleTag` MAY contain child `SimpleTag`s to model
/// hierarchical metadata (e.g. a `TITLE` with a `SORT_WITH` sub-tag).
#[derive(Clone, Debug, Default)]
pub struct MkvSimpleTag {
    /// `TagName` (RFC 9559 §5.1.8.1.2.1, `minOccurs: 1`). Written
    /// verbatim — the demuxer preserves case. An empty name is rejected
    /// at queue time.
    pub name: String,
    /// `TagString` (§5.1.8.1.2.5) or `TagBinary` (§5.1.8.1.2.6) payload —
    /// mutually exclusive per the spec, modelled as one enum.
    pub value: MkvSimpleTagValue,
    /// `TagLanguage` (RFC 9559 §5.1.8.1.2.2, default `"und"`). Written
    /// explicitly only when not `"und"` and no BCP-47 language is set,
    /// so a default-language tag stays minimal on disk.
    pub language: String,
    /// `TagLanguageBCP47` (RFC 9559 §5.1.8.1.2.3, `minver: 4`). When
    /// `Some`, the spec says `TagLanguage` MUST be ignored, so the muxer
    /// omits `TagLanguage` entirely and writes only the BCP-47 element.
    pub language_bcp47: Option<String>,
    /// `TagDefault` (RFC 9559 §5.1.8.1.2.4, default `1` / true). Written
    /// explicitly only when cleared (`false`), so a default-true tag
    /// stays minimal on disk and the demuxer materialises the default.
    pub default: bool,
    /// Nested `SimpleTag`s (RFC 9559 §5.1.8.1.2 `recursive: True`).
    /// Empty for the common flat case.
    pub children: Vec<MkvSimpleTag>,
}

impl MkvSimpleTag {
    /// Convenience constructor for a default-language (`"und"`),
    /// default-flag (`true`), string-valued, leaf `SimpleTag`.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: MkvSimpleTagValue::String(value.into()),
            language: String::from("und"),
            language_bcp47: None,
            default: true,
            children: Vec::new(),
        }
    }

    /// Convenience constructor for a binary-valued leaf `SimpleTag`
    /// (RFC 9559 §5.1.8.1.2.6 `TagBinary`) — e.g. an embedded thumbnail
    /// referenced from a tag.
    pub fn binary(name: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            value: MkvSimpleTagValue::Binary(data.into()),
            language: String::from("und"),
            language_bcp47: None,
            default: true,
            children: Vec::new(),
        }
    }
}

/// `TagString` vs `TagBinary` payload — mutually exclusive within one
/// `SimpleTag` per RFC 9559 §5.1.8.1.2.5 / §5.1.8.1.2.6. Mirrors the
/// demux-side [`crate::demux::SimpleTagValue`].
#[derive(Clone, Debug, Default)]
pub enum MkvSimpleTagValue {
    /// `TagString` (RFC 9559 §5.1.8.1.2.5) — UTF-8 text payload.
    String(String),
    /// `TagBinary` (RFC 9559 §5.1.8.1.2.6) — opaque bytes.
    Binary(Vec<u8>),
    /// Neither payload element — the `SimpleTag` carries only a name (and
    /// possibly children). RFC 9559 gives neither `TagString` nor
    /// `TagBinary` a `minOccurs`, so a name-only `SimpleTag` is legal
    /// (typical for a parent node in a `recursive` nesting).
    #[default]
    None,
}

/// Internal record of a per-track `Video > FlagInterlaced` +
/// `FieldOrder` muxer hint. Queued by [`MkvMuxer::set_video_interlacing`]
/// and materialised inside the track's `Video` master at `write_header`
/// time. `field_order` is `None` unless `flag == FlagInterlaced::Interlaced`
/// — RFC 9559 §5.1.4.1.28.2 mandates "If FlagInterlaced is not set to 1,
/// this element MUST be ignored", so a non-interlaced track never carries
/// an explicit `FieldOrder` on disk under our writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VideoInterlacingMux {
    flag: FlagInterlaced,
    field_order: Option<FieldOrder>,
}

/// Per-track `Video > PixelCrop{Top,Bottom,Left,Right}` (RFC 9559
/// §5.1.4.1.28.8..§5.1.4.1.28.11) plus `DisplayWidth` (§5.1.4.1.28.12) /
/// `DisplayHeight` (§5.1.4.1.28.13) / `DisplayUnit` (§5.1.4.1.28.14) muxer
/// hint. Queued by [`MkvMuxer::set_video_geometry`] and materialised inside
/// the track's `Video` master at `write_header` time, alongside `PixelWidth`
/// / `PixelHeight`.
///
/// Field handling per the spec:
///
/// * `crop_top`, `crop_bottom`, `crop_left`, `crop_right` —
///   `PixelCrop{Top,Bottom,Left,Right}`. The spec ranges them at
///   `0` default and the muxer writes the explicit element only for
///   non-zero values; a zero crop is left off-disk so the demuxer
///   materialises the §5.1.4.1.28.8..11 default `0`.
/// * `display_width`, `display_height` — `DisplayWidth` / `DisplayHeight`.
///   `range: not 0` per §5.1.4.1.28.12 / .13. `None` skips the element;
///   `Some(0)` is rejected at queue time. The demuxer materialises the
///   spec-derived default (`PixelWidth - PixelCropLeft - PixelCropRight`,
///   resp.) only when `display_unit == Pixels`.
/// * `display_unit` — `DisplayUnit` (Table 10). The default is
///   [`DisplayUnit::Pixels`] (`0`); the muxer omits the element when set
///   to `Pixels` so the file stays minimal, and writes it explicitly for
///   every other variant (including [`DisplayUnit::Other`] for §27.9
///   forward-compat values).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MkvVideoGeometry {
    /// `PixelCropTop` (RFC 9559 §5.1.4.1.28.9). Spec default `0`.
    pub crop_top: u64,
    /// `PixelCropBottom` (RFC 9559 §5.1.4.1.28.8). Spec default `0`.
    pub crop_bottom: u64,
    /// `PixelCropLeft` (RFC 9559 §5.1.4.1.28.10). Spec default `0`.
    pub crop_left: u64,
    /// `PixelCropRight` (RFC 9559 §5.1.4.1.28.11). Spec default `0`.
    pub crop_right: u64,
    /// `DisplayWidth` (RFC 9559 §5.1.4.1.28.12). `range: not 0`; `None`
    /// skips the element on disk.
    pub display_width: Option<u64>,
    /// `DisplayHeight` (RFC 9559 §5.1.4.1.28.13). `range: not 0`; `None`
    /// skips the element on disk.
    pub display_height: Option<u64>,
    /// `DisplayUnit` (RFC 9559 §5.1.4.1.28.14, Table 10). Spec default
    /// `0` ([`DisplayUnit::Pixels`]); the muxer omits the element when set
    /// to `Pixels` and writes it explicitly otherwise.
    pub display_unit: DisplayUnit,
}

impl MkvVideoGeometry {
    /// Convenience constructor for the pillar-box / letterbox case worked
    /// in RFC 9559 §11.1: equal left+right or top+bottom crops, no
    /// display-size override, default `Pixels` unit. The four cropped
    /// edges hide encoded padding; the demuxer derives `DisplayWidth` /
    /// `DisplayHeight` from `PixelWidth - crops`.
    pub fn cropped(top: u64, bottom: u64, left: u64, right: u64) -> Self {
        Self {
            crop_top: top,
            crop_bottom: bottom,
            crop_left: left,
            crop_right: right,
            display_width: None,
            display_height: None,
            display_unit: DisplayUnit::Pixels,
        }
    }

    /// Convenience constructor for an aspect-ratio override: no crops,
    /// `DisplayUnit::DisplayAspectRatio` and the `(num, den)` ratio
    /// encoded into `DisplayWidth` / `DisplayHeight` per RFC 9559
    /// §5.1.4.1.28.14 (Table 10 value `3`).
    pub fn aspect_ratio(num: u64, den: u64) -> Self {
        Self {
            crop_top: 0,
            crop_bottom: 0,
            crop_left: 0,
            crop_right: 0,
            display_width: Some(num),
            display_height: Some(den),
            display_unit: DisplayUnit::DisplayAspectRatio,
        }
    }
}

/// Per-track `Video > Colour` (RFC 9559 §5.1.4.1.28.16) muxer hint, queued
/// by [`MkvMuxer::set_video_colour`] and materialised inside the track's
/// `Video` master at `write_header` time, alongside the existing geometry /
/// interlacing / `UncompressedFourCC` block.
///
/// This struct covers the eleven scalar children of the `Colour` master
/// (§5.1.4.1.28.17..§5.1.4.1.28.29). The `MasteringMetadata` sub-master
/// (§5.1.4.1.28.30..§5.1.4.1.28.40) is intentionally absent from this
/// round — HDR mastering metadata is a separate addition.
///
/// Field handling per the spec:
///
/// * `matrix_coefficients` — `MatrixCoefficients` (Table 12). Spec default
///   `2` ([`MatrixCoefficients::Unspecified`]); the muxer omits the
///   element when it equals the default so the file stays minimal.
/// * `bits_per_channel` — `BitsPerChannel`. Spec default `0`
///   (*unspecified*); omitted when zero.
/// * `chroma_subsampling_horz` / `chroma_subsampling_vert` —
///   `ChromaSubsamplingHorz` / `ChromaSubsamplingVert`. No spec default;
///   `None` skips the element, `Some(v)` writes it.
/// * `cb_subsampling_horz` / `cb_subsampling_vert` — `CbSubsamplingHorz`
///   / `CbSubsamplingVert`. No spec default; `None` skips, `Some(v)`
///   writes.
/// * `chroma_siting_horz` / `chroma_siting_vert` — `ChromaSitingHorz`
///   (Table 13) / `ChromaSitingVert` (Table 14). Spec default `0`
///   ([`ChromaSitingHorz::Unspecified`] / [`ChromaSitingVert::Unspecified`]);
///   omitted when equal to the default.
/// * `range` — `Range` (Table 15). Spec default `0`
///   ([`ColourRange::Unspecified`]); omitted when equal.
/// * `transfer_characteristics` — `TransferCharacteristics` (Table 16).
///   Spec default `2` ([`TransferCharacteristics::Unspecified`]); omitted
///   when equal.
/// * `primaries` — `Primaries` (Table 17). Spec default `2`
///   ([`Primaries::Unspecified`]); omitted when equal.
/// * `max_cll` / `max_fall` — `MaxCLL` / `MaxFALL`. No spec default;
///   `None` skips, `Some(v)` writes.
///
/// Every enum-typed field accepts the `Other(u64)` forward-compat variant
/// so a value the demuxer parsed from a §27 open registry can be re-muxed
/// verbatim.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MkvVideoColour {
    /// `MatrixCoefficients` (RFC 9559 §5.1.4.1.28.17, Table 12). Default
    /// [`MatrixCoefficients::Unspecified`].
    pub matrix_coefficients: MatrixCoefficients,
    /// `BitsPerChannel` (RFC 9559 §5.1.4.1.28.18). Default `0`
    /// (*unspecified*).
    pub bits_per_channel: u64,
    /// `ChromaSubsamplingHorz` (RFC 9559 §5.1.4.1.28.19). `None` skips
    /// the element on disk.
    pub chroma_subsampling_horz: Option<u64>,
    /// `ChromaSubsamplingVert` (RFC 9559 §5.1.4.1.28.20). `None` skips.
    pub chroma_subsampling_vert: Option<u64>,
    /// `CbSubsamplingHorz` (RFC 9559 §5.1.4.1.28.21). `None` skips.
    pub cb_subsampling_horz: Option<u64>,
    /// `CbSubsamplingVert` (RFC 9559 §5.1.4.1.28.22). `None` skips.
    pub cb_subsampling_vert: Option<u64>,
    /// `ChromaSitingHorz` (RFC 9559 §5.1.4.1.28.23, Table 13). Default
    /// [`ChromaSitingHorz::Unspecified`].
    pub chroma_siting_horz: ChromaSitingHorz,
    /// `ChromaSitingVert` (RFC 9559 §5.1.4.1.28.24, Table 14). Default
    /// [`ChromaSitingVert::Unspecified`].
    pub chroma_siting_vert: ChromaSitingVert,
    /// `Range` (RFC 9559 §5.1.4.1.28.25, Table 15). Default
    /// [`ColourRange::Unspecified`].
    pub range: ColourRange,
    /// `TransferCharacteristics` (RFC 9559 §5.1.4.1.28.26, Table 16).
    /// Default [`TransferCharacteristics::Unspecified`].
    pub transfer_characteristics: TransferCharacteristics,
    /// `Primaries` (RFC 9559 §5.1.4.1.28.27, Table 17). Default
    /// [`Primaries::Unspecified`].
    pub primaries: Primaries,
    /// `MaxCLL` (RFC 9559 §5.1.4.1.28.28). No spec default; `None`
    /// skips.
    pub max_cll: Option<u64>,
    /// `MaxFALL` (RFC 9559 §5.1.4.1.28.29). No spec default; `None`
    /// skips.
    pub max_fall: Option<u64>,
    /// `MasteringMetadata` sub-master (RFC 9559 §5.1.4.1.28.30) — the
    /// SMPTE ST 2086 / CTA-861.3 mastering-display description. `None`
    /// skips the entire master on disk so the demuxer surfaces `None`
    /// from [`crate::demux::VideoColour::mastering_metadata`]. `Some(m)`
    /// emits the master with each child written only when its slot in
    /// `m` is `Some(v)` — see [`MkvMasteringMetadata`] for the per-
    /// child semantics.
    pub mastering_metadata: Option<MkvMasteringMetadata>,
}

/// `Colour > MasteringMetadata` payload (RFC 9559
/// §5.1.4.1.28.30..§5.1.4.1.28.40) on the write side: the SMPTE ST 2086
/// / CTA-861.3 mastering-display description that accompanies HDR
/// content.
///
/// Each child is independently optional — the spec does not require any
/// of them to appear together. The muxer emits the `MasteringMetadata`
/// master only when its slot in [`MkvVideoColour::mastering_metadata`]
/// is `Some`; inside that master, each child element is written only
/// when its corresponding `Option<f64>` is `Some(v)`. Every chromaticity
/// or luminance is written as an 8-byte big-endian `f64`, which the
/// demux side's [`crate::demux::MasteringMetadata`] reads back through
/// the shared `ebml::read_float` helper (also accepts 4-byte
/// `f32`-shaped values on read).
///
/// `Primary{R,G,B}Chromaticity{X,Y}` and `WhitePointChromaticity{X,Y}`
/// are CIE-1931 chromaticities in the range `[0.0, 1.0]` per RFC 9559
/// §5.1.4.1.28.31..§5.1.4.1.28.38. `Luminance{Max,Min}` are in cd/m²
/// (§5.1.4.1.28.39 / §5.1.4.1.28.40, range `>= 0`). The muxer does
/// **not** validate range — values outside the spec range still reach
/// disk verbatim so a caller mirroring a file with out-of-spec values
/// can preserve them.
///
/// Pairs symmetrically with [`crate::demux::MasteringMetadata`]: a
/// mux→demux round-trip preserves every child verbatim, and a
/// `MkvMasteringMetadata` with every slot `None` round-trips through
/// an empty `MasteringMetadata` master on disk that the demuxer parses
/// into `Some(MasteringMetadata::default())` — distinct from "no
/// `MasteringMetadata` element present" which leaves the demux-side
/// accessor `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MkvMasteringMetadata {
    /// `PrimaryRChromaticityX` (RFC 9559 §5.1.4.1.28.31, id `0x55D1`).
    /// CIE-1931 X coordinate of the mastering display's red primary,
    /// range `[0.0, 1.0]`.
    pub primary_r_chromaticity_x: Option<f64>,
    /// `PrimaryRChromaticityY` (RFC 9559 §5.1.4.1.28.32, id `0x55D2`).
    pub primary_r_chromaticity_y: Option<f64>,
    /// `PrimaryGChromaticityX` (RFC 9559 §5.1.4.1.28.33, id `0x55D3`).
    pub primary_g_chromaticity_x: Option<f64>,
    /// `PrimaryGChromaticityY` (RFC 9559 §5.1.4.1.28.34, id `0x55D4`).
    pub primary_g_chromaticity_y: Option<f64>,
    /// `PrimaryBChromaticityX` (RFC 9559 §5.1.4.1.28.35, id `0x55D5`).
    pub primary_b_chromaticity_x: Option<f64>,
    /// `PrimaryBChromaticityY` (RFC 9559 §5.1.4.1.28.36, id `0x55D6`).
    pub primary_b_chromaticity_y: Option<f64>,
    /// `WhitePointChromaticityX` (RFC 9559 §5.1.4.1.28.37, id `0x55D7`).
    pub white_point_chromaticity_x: Option<f64>,
    /// `WhitePointChromaticityY` (RFC 9559 §5.1.4.1.28.38, id `0x55D8`).
    pub white_point_chromaticity_y: Option<f64>,
    /// `LuminanceMax` (RFC 9559 §5.1.4.1.28.39, id `0x55D9`), in cd/m²;
    /// spec range `>= 0`. Maximum luminance of the mastering display.
    pub luminance_max: Option<f64>,
    /// `LuminanceMin` (RFC 9559 §5.1.4.1.28.40, id `0x55DA`), in cd/m²;
    /// spec range `>= 0`. Minimum luminance of the mastering display.
    pub luminance_min: Option<f64>,
}

impl MkvMasteringMetadata {
    /// Convenience constructor for the BT.2020 primaries + D65 white
    /// point shape used by the canonical HDR10 mastering display
    /// (a 1000 cd/m² peak, 0.005 cd/m² floor). Chromaticities follow
    /// ITU-R BT.2020 Table 2 (red `(0.708, 0.292)`, green
    /// `(0.170, 0.797)`, blue `(0.131, 0.046)`) and D65 white point
    /// `(0.3127, 0.3290)`.
    pub fn bt2020_d65_hdr10() -> Self {
        Self {
            primary_r_chromaticity_x: Some(0.708),
            primary_r_chromaticity_y: Some(0.292),
            primary_g_chromaticity_x: Some(0.170),
            primary_g_chromaticity_y: Some(0.797),
            primary_b_chromaticity_x: Some(0.131),
            primary_b_chromaticity_y: Some(0.046),
            white_point_chromaticity_x: Some(0.3127),
            white_point_chromaticity_y: Some(0.3290),
            luminance_max: Some(1000.0),
            luminance_min: Some(0.005),
        }
    }
}

impl Default for MkvVideoColour {
    /// Default Colour hint: every scalar at the §5.1.4.1.28 spec default
    /// (`MatrixCoefficients` / `TransferCharacteristics` / `Primaries` =
    /// `2` *unspecified*; `BitsPerChannel` / `ChromaSitingHorz` /
    /// `ChromaSitingVert` / `Range` = `0` *unspecified*) and every
    /// `Option<…>` set to `None`. Queueing this default still causes the
    /// `Colour` master to be emitted — just as an empty master with no
    /// children — which mirrors how the demuxer parses an empty `Colour`
    /// master (the typed `VideoColour` surfaces `Some` with every default
    /// materialised). To omit the `Colour` master entirely, simply do not
    /// call [`MkvMuxer::set_video_colour`].
    fn default() -> Self {
        Self {
            matrix_coefficients: MatrixCoefficients::Unspecified,
            bits_per_channel: 0,
            chroma_subsampling_horz: None,
            chroma_subsampling_vert: None,
            cb_subsampling_horz: None,
            cb_subsampling_vert: None,
            chroma_siting_horz: ChromaSitingHorz::Unspecified,
            chroma_siting_vert: ChromaSitingVert::Unspecified,
            range: ColourRange::Unspecified,
            transfer_characteristics: TransferCharacteristics::Unspecified,
            primaries: Primaries::Unspecified,
            max_cll: None,
            max_fall: None,
            mastering_metadata: None,
        }
    }
}

impl MkvVideoColour {
    /// Convenience constructor for the BT.709 SDR shape: matrix `1`,
    /// transfer `1`, primaries `1`, broadcast range. This is the canonical
    /// shape for legacy 8-bit HD video (RFC 9559 §5.1.4.1.28.17 / .26 / .27
    /// Tables 12 / 16 / 17 entry `1`).
    pub fn bt709() -> Self {
        Self {
            matrix_coefficients: MatrixCoefficients::BT709,
            transfer_characteristics: TransferCharacteristics::BT709,
            primaries: Primaries::BT709,
            range: ColourRange::Broadcast,
            ..Self::default()
        }
    }

    /// Convenience constructor for the BT.2020 HDR PQ shape: matrix `9`
    /// (BT.2020 non-constant luminance), transfer `16` (BT.2100 PQ),
    /// primaries `9` (BT.2020), full range, 10 bits per channel. This is
    /// the canonical shape used for HDR10 video — the `MaxCLL` /
    /// `MaxFALL` and `mastering_metadata` slots are *not* populated
    /// here; a caller wanting them can override `max_cll` / `max_fall`
    /// on the returned value, and attach a
    /// [`MkvMasteringMetadata`] via `mastering_metadata: Some(…)` (e.g.
    /// [`MkvMasteringMetadata::bt2020_d65_hdr10`]).
    pub fn bt2020_pq() -> Self {
        Self {
            matrix_coefficients: MatrixCoefficients::BT2020NonConstantLuminance,
            transfer_characteristics: TransferCharacteristics::BT2100Pq,
            primaries: Primaries::BT2020,
            range: ColourRange::Full,
            bits_per_channel: 10,
            ..Self::default()
        }
    }
}

/// Per-track `Video > Projection` write hint (RFC 9559 §5.1.4.1.28.41,
/// including the §5.1.4.1.28.42..§5.1.4.1.28.46 sub-elements). Queued by
/// [`MkvMuxer::set_video_projection`] and materialised inside the track's
/// `Video` master at `write_header` time, after the `Colour` master.
///
/// The pose triple is in degrees: per §5.1.4.1.28.44..46 yaw / roll are in
/// `[-180.0, 180.0]` and pitch is in `[-90.0, 90.0]`, all defaulting to
/// `0.0`. `private` is the verbatim ISOBMFF box body (`equi` / `cbmp` /
/// `mshp`) that pairs with a spherical [`ProjectionType`]; it is written
/// only when `Some(_)` and never interpreted by the muxer. Per
/// §5.1.4.1.28.43 `ProjectionPrivate` MUST NOT be present for a
/// `Rectangular` projection — that's a producer concern; the muxer writes
/// what it's handed.
///
/// Per-element omission rules at write time: `ProjectionType` is written
/// only for non-`Rectangular` types (the §5.1.4.1.28.42 default `0` is
/// omitted so the demuxer materialises it); each pose component is written
/// only when non-zero (the §5.1.4.1.28.44..46 default `0.0` is omitted);
/// `ProjectionPrivate` is written only when `Some(_)`. As a result a
/// [`MkvProjection::default`] (rectangular, zero pose, no private) queued
/// via the setter serialises as an *empty* `Projection` master — present
/// but childless — which the demuxer parses into `Some(Projection)` with
/// every getter returning the spec default, distinct from the
/// call-was-omitted case (`None`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MkvProjection {
    /// `ProjectionType` (RFC 9559 §5.1.4.1.28.42, id `0x7671`). Default
    /// [`ProjectionType::Rectangular`]; written on disk only for
    /// non-rectangular projections.
    pub projection_type: ProjectionType,
    /// `ProjectionPrivate` (RFC 9559 §5.1.4.1.28.43, id `0x7672`): the
    /// verbatim ISOBMFF box body for the projection type. Written only
    /// when `Some(_)`; never interpreted.
    pub private: Option<Vec<u8>>,
    /// `ProjectionPoseYaw` (RFC 9559 §5.1.4.1.28.44, id `0x7673`),
    /// degrees, range `[-180.0, 180.0]`, default `0.0`.
    pub pose_yaw: f64,
    /// `ProjectionPosePitch` (RFC 9559 §5.1.4.1.28.45, id `0x7674`),
    /// degrees, range `[-90.0, 90.0]`, default `0.0`.
    pub pose_pitch: f64,
    /// `ProjectionPoseRoll` (RFC 9559 §5.1.4.1.28.46, id `0x7675`),
    /// degrees, range `[-180.0, 180.0]`, default `0.0`.
    pub pose_roll: f64,
}

impl MkvProjection {
    /// Convenience constructor for the equirectangular spherical
    /// projection (RFC 9559 §5.1.4.1.28.42 value `1`) — the common 360°
    /// monoscopic / stereoscopic VR shape — carrying the verbatim
    /// ISOBMFF `equi` box body in `ProjectionPrivate` and a zero pose.
    pub fn equirectangular(private: Vec<u8>) -> Self {
        Self {
            projection_type: ProjectionType::Equirectangular,
            private: Some(private),
            ..Self::default()
        }
    }

    /// Convenience constructor for a flat rectangular track that only
    /// needs a roll rotation — the §5.1.4.1.28.46 worked example
    /// (`ProjectionPoseRoll = 90` ⇒ a 90° counter-clockwise rotation).
    /// `roll_degrees` lands in `ProjectionPoseRoll`; yaw / pitch stay at
    /// their `0.0` defaults and `ProjectionType` stays `Rectangular`.
    pub fn rotated(roll_degrees: f64) -> Self {
        Self {
            pose_roll: roll_degrees,
            ..Self::default()
        }
    }
}

/// Per-track "audience" flags payload (RFC 9559 §5.1.4.1.6..§5.1.4.1.11)
/// queued by [`MkvMuxer::set_track_audience_flags`] and materialised
/// directly inside the `TrackEntry` (NOT inside a `Video` / `Audio`
/// sub-master — the six elements sit on `TrackEntry` itself) at
/// `write_header` time.
///
/// Every field is `Option<bool>` with the same omission rule: `None`
/// keeps the element off-disk, `Some(v)` writes it explicitly as `0` /
/// `1`. The on-disk consequences differ per the spec's asymmetric
/// defaults:
///
/// * [`forced`](Self::forced) (`FlagForced`, id `0x55AA`, §5.1.4.1.6)
///   carries the spec default `0`, so `None` and `Some(false)` are
///   *observationally* identical to a reader (both decode `false`) but
///   byte-distinct on disk — `Some(false)` writes the element, the
///   explicit way for a producer to override a downstream tool that
///   might infer something else.
/// * The five `minver: 4` flags — [`hearing_impaired`](Self::hearing_impaired)
///   (`FlagHearingImpaired`, id `0x55AB`, §5.1.4.1.7),
///   [`visual_impaired`](Self::visual_impaired) (`FlagVisualImpaired`,
///   id `0x55AC`, §5.1.4.1.8), [`text_descriptions`](Self::text_descriptions)
///   (`FlagTextDescriptions`, id `0x55AD`, §5.1.4.1.9),
///   [`original`](Self::original) (`FlagOriginal`, id `0x55AE`,
///   §5.1.4.1.10), [`commentary`](Self::commentary) (`FlagCommentary`,
///   id `0x55AF`, §5.1.4.1.11) — carry **no** spec default, so `None`
///   vs `Some(false)` is semantically load-bearing: the §5.1.4.1.7..11
///   wording ("Set to 1 *if and only if* …") makes a writer's explicit
///   `0` a stronger signal than silence. The demux-side
///   [`crate::demux::TrackAudienceFlags`] accessor preserves exactly
///   that distinction (`None` / `Some(false)` / `Some(true)`).
///
/// The muxer already pins `DocTypeVersion` to `4`, so emitting the
/// `minver: 4` elements never violates the declared document version.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MkvTrackAudienceFlags {
    /// `FlagForced` (§5.1.4.1.6). Applies only to subtitles per the spec
    /// definition; the muxer still accepts it on any track type because
    /// the spec carries the element on every `TrackEntry` with
    /// `minOccurs: 1` — the demux side surfaces it everywhere too.
    pub forced: Option<bool>,
    /// `FlagHearingImpaired` (§5.1.4.1.7) — track is suitable for users
    /// with hearing impairments (e.g. SDH subtitles).
    pub hearing_impaired: Option<bool>,
    /// `FlagVisualImpaired` (§5.1.4.1.8) — track is suitable for users
    /// with visual impairments (e.g. an audio-description track).
    pub visual_impaired: Option<bool>,
    /// `FlagTextDescriptions` (§5.1.4.1.9) — track contains textual
    /// descriptions of video content.
    pub text_descriptions: Option<bool>,
    /// `FlagOriginal` (§5.1.4.1.10) — track is in the content's original
    /// language (vs a dub).
    pub original: Option<bool>,
    /// `FlagCommentary` (§5.1.4.1.11) — track contains commentary.
    pub commentary: Option<bool>,
}

impl MkvTrackAudienceFlags {
    /// Convenience constructor for the forced-subtitle shape
    /// (§5.1.4.1.6): `FlagForced = 1`, everything else off-disk. The
    /// canonical use is a subtitle track carrying only translations of
    /// foreign-language audio or on-screen text.
    pub fn forced_subtitle() -> Self {
        Self {
            forced: Some(true),
            ..Self::default()
        }
    }

    /// Convenience constructor for an SDH-style subtitle track
    /// (§5.1.4.1.7): `FlagHearingImpaired = 1`, everything else
    /// off-disk.
    pub fn hearing_impaired_track() -> Self {
        Self {
            hearing_impaired: Some(true),
            ..Self::default()
        }
    }

    /// Convenience constructor for an audio-description track
    /// (§5.1.4.1.8): `FlagVisualImpaired = 1`, everything else off-disk.
    pub fn visual_impaired_track() -> Self {
        Self {
            visual_impaired: Some(true),
            ..Self::default()
        }
    }

    /// Convenience constructor for a commentary track (§5.1.4.1.11):
    /// `FlagCommentary = 1`, everything else off-disk.
    pub fn commentary_track() -> Self {
        Self {
            commentary: Some(true),
            ..Self::default()
        }
    }

    /// `true` when every slot is `None` — queueing such a record is a
    /// functional no-op (no element reaches disk), kept legal so a
    /// caller can pass through a fully-silent source record without
    /// special-casing.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One per-Block side-channel payload to be written as a
/// `BlockGroup > BlockAdditions > BlockMore` master (RFC 9559
/// §5.1.3.5.2.1) by [`MkvMuxer::write_packet_with_additions`].
///
/// `id` is the `BlockAddID` (§5.1.3.5.2.3, range "not 0"): `1` means the
/// `data` bytes are codec-defined (e.g. a WebM alpha plane — pair with
/// [`MkvMuxer::set_video_alpha_mode`]); any other value should be
/// described by a `BlockAdditionMapping` on the track. The on-disk
/// `BlockAddID` element is omitted when `id == 1` (the spec default) and
/// written explicitly otherwise. `data` is the verbatim
/// `BlockAdditional` payload (§5.1.3.5.2.2) — never interpreted by the
/// muxer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MkvBlockAddition {
    /// `BlockAddID` (RFC 9559 §5.1.3.5.2.3). Must be non-zero and at
    /// most the track's declared `MaxBlockAdditionID` — validated at
    /// [`MkvMuxer::write_packet_with_additions`] time.
    pub id: u64,
    /// `BlockAdditional` (RFC 9559 §5.1.3.5.2.2) payload bytes, written
    /// verbatim.
    pub data: Vec<u8>,
}

impl MkvBlockAddition {
    /// Construct an addition with an explicit `BlockAddID`.
    pub fn new(id: u64, data: Vec<u8>) -> Self {
        Self { id, data }
    }

    /// Convenience constructor for the codec-defined channel
    /// (`BlockAddID == 1`, the §5.1.3.5.2.3 default — e.g. WebM alpha
    /// data).
    pub fn codec_defined(data: Vec<u8>) -> Self {
        Self { id: 1, data }
    }
}

/// The full set of `BlockGroup` (RFC 9559 §5.1.3.5) children a single
/// packet can be wrapped with via [`MkvMuxer::write_packet_with_block_group`]
/// — the write-side counterpart of
/// [`MkvDemuxer::block_group_meta`](crate::demux::MkvDemuxer::block_group_meta)
/// plus [`MkvDemuxer::block_additions`](crate::demux::MkvDemuxer::block_additions).
///
/// Every field is optional; an all-default `BlockGroupOptions` still forces
/// a `BlockGroup` wrapper (rather than a bare `SimpleBlock`) so the caller
/// can pin the on-disk shape, but in practice at least one field is set.
/// On-disk child order follows the §5.1.3.5 subsection order: `Block`,
/// `BlockAdditions`, `BlockDuration`, `ReferencePriority`, `ReferenceBlock`
/// (one per entry), `CodecState`, `DiscardPadding`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockGroupOptions {
    /// `BlockAdditions` (§5.1.3.5.2) — validated exactly as
    /// [`MkvMuxer::write_packet_with_additions`] validates them.
    pub additions: Vec<MkvBlockAddition>,
    /// `ReferenceBlock` values (§5.1.3.5.5), one element written per entry,
    /// in slice order. Each is a signed track-tick offset relative to this
    /// Block's timestamp. A non-empty list marks the Block as a
    /// non-keyframe. When empty, the muxer falls back to deriving a single
    /// `ReferenceBlock` from the most recent same-track Block for a
    /// non-keyframe packet (matching [`MkvMuxer::write_packet_with_additions`]).
    pub reference_blocks: Vec<i64>,
    /// `ReferencePriority` (§5.1.3.5.4, uinteger, default `0`). Written only
    /// when non-zero — `0` is the spec default and a mandatory element with
    /// a default may stay off-disk at that default.
    pub reference_priority: u64,
    /// `CodecState` (§5.1.3.5.6, binary) verbatim bytes. `None` → no child.
    pub codec_state: Option<Vec<u8>>,
    /// `DiscardPadding` (§5.1.3.5.7, signed nanoseconds). `None` → no child.
    pub discard_padding: Option<i64>,
    /// `BlockVirtual` (RFC 9559 Appendix A.3, binary) verbatim bytes — a
    /// reclaimed data-less Block. `None` → no child. Written for a faithful
    /// re-mux; the container does not interpret it.
    pub block_virtual: Option<Vec<u8>>,
    /// `ReferenceVirtual` (RFC 9559 Appendix A.4, signed integer) — the
    /// Segment Position of a virtual Block's data. `None` → no child.
    pub reference_virtual: Option<i64>,
    /// `Slices > TimeSlice` masters (RFC 9559 Appendix A.5..A.11), written in
    /// slice order, one `TimeSlice` per entry inside a single `Slices` master.
    /// Empty → no `Slices` child. Each [`TimeSlice`](crate::demux::TimeSlice)
    /// emits only the fields the caller set (`Some(_)`); an empty `TimeSlice`
    /// still writes an empty `TimeSlice` master so the on-disk count is
    /// preserved.
    pub slices: Vec<crate::demux::TimeSlice>,
    /// `ReferenceFrame` master (RFC 9559 Appendix A.12..A.14) — the Smooth
    /// FF/RW trick-track back-reference. `None` → no child; a present
    /// [`ReferenceFrame`](crate::demux::ReferenceFrame) emits only its set
    /// children.
    pub reference_frame: Option<crate::demux::ReferenceFrame>,
}

/// A `TrackOperation` (RFC 9559 §5.1.4.1.30) hint queued via
/// [`MkvMuxer::set_track_operation`]: the recipe that turns the carrying
/// `TrackEntry` into a *virtual* track assembled from other tracks in the
/// same Segment.
///
/// References to the source tracks are given as 0-indexed **stream
/// indices** (the same unit [`Muxer::write_packet`] uses), which the muxer
/// resolves to the corresponding on-disk `TrackUID` at `write_header` time.
/// This is the symmetric inverse of the demux side, which resolves each
/// on-disk UID back to a stream index in
/// [`TrackRef`](crate::demux::TrackRef).
///
/// Two independent operation kinds are supported, exactly as the spec
/// allows them to coexist:
///
/// * [`combine_planes`](Self::combine_planes) — `TrackCombinePlanes`
///   (§5.1.4.1.30.1), the list of video plane tracks (each tagged with a
///   [`TrackPlaneType`](crate::demux::TrackPlaneType)) merged into one
///   stereoscopic 3D track.
/// * [`join_tracks`](Self::join_tracks) — `TrackJoinBlocks`
///   (§5.1.4.1.30.5), the list of tracks whose Blocks are joined into a
///   single timeline.
///
/// Pairs symmetrically with the existing
/// [`MkvDemuxer::track_operation`](crate::demux::MkvDemuxer::track_operation)
/// typed accessor — a mux→demux pipeline preserves every plane (with its
/// type) and every join reference.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MkvTrackOperation {
    /// `TrackCombinePlanes` planes, in write order. Empty → no
    /// `TrackCombinePlanes` child is emitted.
    pub combine_planes: Vec<MkvTrackPlane>,
    /// `TrackJoinBlocks` source stream indices, in write order. Empty →
    /// no `TrackJoinBlocks` child is emitted.
    pub join_tracks: Vec<usize>,
}

impl MkvTrackOperation {
    /// Construct an empty operation — add planes / joins via the fields or
    /// the helpers below.
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience constructor for the canonical stereoscopic-3D shape: a
    /// left-eye plane track and a right-eye plane track combined into this
    /// virtual track (RFC 9559 §5.1.4.1.30.1, §18.8).
    pub fn stereo_3d(left_eye: usize, right_eye: usize) -> Self {
        Self {
            combine_planes: vec![
                MkvTrackPlane {
                    stream_index: left_eye,
                    plane_type: TrackPlaneType::LeftEye,
                },
                MkvTrackPlane {
                    stream_index: right_eye,
                    plane_type: TrackPlaneType::RightEye,
                },
            ],
            join_tracks: Vec::new(),
        }
    }

    /// Convenience constructor for a `TrackJoinBlocks` virtual track that
    /// concatenates the Blocks of the given source streams onto one
    /// timeline (RFC 9559 §5.1.4.1.30.5).
    pub fn join(streams: Vec<usize>) -> Self {
        Self {
            combine_planes: Vec::new(),
            join_tracks: streams,
        }
    }

    /// True when neither a `TrackCombinePlanes` nor a `TrackJoinBlocks`
    /// child would be written. Such a queue produces an empty
    /// `TrackOperation` master — legal but inert; the setter rejects it so
    /// callers don't silently emit a no-op.
    pub fn is_empty(&self) -> bool {
        self.combine_planes.is_empty() && self.join_tracks.is_empty()
    }
}

/// One `TrackPlane` (RFC 9559 §5.1.4.1.30.2) inside an
/// [`MkvTrackOperation::combine_planes`] list: a source video track plus
/// the role it plays in the combined 3D track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MkvTrackPlane {
    /// 0-indexed stream index of the source plane track, resolved to its
    /// `TrackPlaneUID` (= the track's `TrackUID`) at `write_header` time.
    pub stream_index: usize,
    /// `TrackPlaneType` (RFC 9559 §5.1.4.1.30.4).
    pub plane_type: TrackPlaneType,
}

/// One Cues → CuePoint entry the muxer will emit in `write_trailer`.
#[derive(Clone, Copy, Debug)]
struct CueRecord {
    /// MKV TrackNumber (1-indexed).
    track: u64,
    /// Timestamp in milliseconds (matches our `TIMECODE_SCALE = 1_000_000` ns).
    time_ms: u64,
    /// Offset of the Cluster header relative to the Segment payload start.
    cluster_offset: u64,
    /// `CueRelativePosition` (RFC 9559 §5.1.5.1.2.3) — byte offset of the
    /// indexed `SimpleBlock` / `BlockGroup` element from the first possible
    /// element position inside the Cluster (i.e. immediately after the
    /// Cluster element's id+size header).
    relative_position: u64,
    /// `CueBlockNumber` (RFC 9559 §5.1.5.1.2.5) — the 1-based number of the
    /// indexed Block within its Cluster (counting every `SimpleBlock` /
    /// `BlockGroup` regardless of track, in write order). `range: not 0`,
    /// so the smallest legal value is `1` (the first block in the cluster).
    block_number: u64,
    /// `CueDuration` (RFC 9559 §5.1.5.1.2.4) — the duration of the indexed
    /// Block in Segment Ticks (ms, matching `TIMECODE_SCALE`). `None` when
    /// the packet carried no usable duration; absence means "no cue-level
    /// duration is available" (§5.1.5.1.2.4) and the element is omitted.
    duration_ms: Option<u64>,
}

impl MkvMuxer {
    /// Construct a muxer in the given DocType flavour. Validates codec
    /// compatibility up front for WebM.
    fn new(output: Box<dyn WriteSeek>, streams: &[StreamInfo], doc_type: DocType) -> Result<Self> {
        if streams.is_empty() {
            return Err(Error::invalid("MKV muxer: need at least one stream"));
        }
        if doc_type == DocType::Webm {
            for (i, s) in streams.iter().enumerate() {
                if !codec_id::is_webm_codec(&s.params.codec_id) {
                    return Err(Error::unsupported(format!(
                        "WebM muxer: stream {i} uses codec '{}' which is not in the WebM whitelist (allowed: vp8, vp9, av1, vorbis, opus)",
                        s.params.codec_id.as_str()
                    )));
                }
            }
        }
        let stream_track_numbers: Vec<u64> = (0..streams.len() as u64).map(|i| i + 1).collect();
        let n = streams.len();
        Ok(MkvMuxer {
            output,
            streams: streams.to_vec(),
            track_numbers: stream_track_numbers,
            stream_pts: vec![0i64; n],
            cluster_open: false,
            pending_silent_tracks: Vec::new(),
            cluster_timecode_ms: 0,
            cluster_offset_rel: 0,
            cluster_body_start_abs: 0,
            cluster_position_hints: false,
            prev_cluster_start_abs: None,
            live_streaming: false,
            segment_data_start: 0,
            cues: Vec::new(),
            cue_seen_in_cluster: vec![false; n],
            cluster_block_count: 0,
            seek_cues_entry_offset: 0,
            seek_head_written: false,
            header_written: false,
            trailer_written: false,
            doc_type,
            doc_type_extensions: Vec::new(),
            chapters: Vec::new(),
            attachments: Vec::new(),
            tags: Vec::new(),
            segment_linking: None,
            info_title: None,
            info_date_utc_ns: None,
            info_duration_ticks: None,
            lacing_mode: LacingMode::None,
            lace_pending: vec![LaceBuffer::default(); n],
            video_interlacings: vec![None; n],
            video_stereo_modes: vec![None; n],
            video_old_stereo_modes: vec![None; n],
            video_alpha_modes: vec![None; n],
            video_geometries: vec![None; n],
            video_uncompressed_fourccs: vec![None; n],
            video_aspect_ratio_types: vec![None; n],
            video_colours: vec![None; n],
            video_projections: vec![None; n],
            track_audience_flags: vec![None; n],
            max_block_addition_ids: vec![None; n],
            block_addition_mappings: vec![Vec::new(); n],
            track_audio: vec![None; n],
            track_timing: vec![None; n],
            track_codec_timing: vec![None; n],
            track_identity: vec![None; n],
            last_block_pts_ms: vec![None; n],
            track_operations: vec![None; n],
            content_encodings: vec![None; n],
            track_translates: vec![Vec::new(); n],
            track_legacy: vec![None; n],
        })
    }

    /// Opt the muxer in to block lacing (RFC 9559 §10.3). Must be
    /// called before [`Muxer::write_header`]; returns
    /// [`Error::other`] otherwise — `FlagLacing` is part of the
    /// `Tracks` element and we don't rewrite Tracks once it's been
    /// emitted.
    ///
    /// `LacingMode::None` is the default and matches the legacy
    /// behaviour (one SimpleBlock per packet, `FlagLacing = 0`).
    /// Any other mode causes the muxer to aggregate same-track
    /// frames (subject to the rules listed on [`LacingMode`]) into
    /// laced SimpleBlocks, and writes `FlagLacing = 1` on every
    /// `TrackEntry`.
    ///
    /// Returns a mutable reference back so calls can chain
    /// builder-style if the caller has a `&mut MkvMuxer`.
    pub fn with_block_lacing(&mut self, mode: LacingMode) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: with_block_lacing called after write_header",
            ));
        }
        self.lacing_mode = mode;
        Ok(self)
    }

    /// Read-only accessor for the currently configured lacing mode.
    /// Returns [`LacingMode::None`] when the muxer is in default
    /// (no-lacing) state. Useful for tests + diagnostics.
    pub fn block_lacing_mode(&self) -> LacingMode {
        self.lacing_mode
    }

    /// Opt in to per-Cluster damage-recovery / backward-play hints: every
    /// Cluster the muxer opens gains a `Position` child (RFC 9559
    /// §5.1.3.2 — the Cluster's own Segment Position; the spec notes "It
    /// might help to resynchronize the offset on damaged streams") right
    /// after its `Timestamp`, and every Cluster from the second on gains
    /// a `PrevSize` child (§5.1.3.3 — the previous Cluster's full element
    /// size in octets, "useful for backward playing": subtracting it from
    /// a Cluster's own start offset lands on the previous Cluster's ID).
    ///
    /// Off by default — the two elements are optional and byte-identical
    /// output with prior releases is preserved unless the caller opts in.
    /// Must be called before [`Muxer::write_header`]. The demux side reads
    /// both onto [`crate::demux::ClusterRecord`].
    pub fn with_cluster_position_hints(&mut self) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: with_cluster_position_hints called after write_header",
            ));
        }
        self.cluster_position_hints = true;
        Ok(self)
    }

    /// Read-only accessor: `true` when
    /// [`MkvMuxer::with_cluster_position_hints`] was called.
    pub fn cluster_position_hints(&self) -> bool {
        self.cluster_position_hints
    }

    /// Opt in to the RFC 9559 §25.3.4 livestreaming layout: "In
    /// livestreaming, only a few elements make sense. For example,
    /// SeekHead and Cues are useless." The muxer omits the up-front
    /// `SeekHead` and writes no `Cues` in `write_trailer`; everything
    /// other than the Clusters is still placed before the Clusters, as
    /// §25.3.4 requires (Info, Tracks, Attachments, Tags — the layout the
    /// muxer already emits). Per §23.2, a stream with neither a SeekHead
    /// nor a Cues list at its start SHOULD be considered non-seekable by
    /// players — exactly the signal a live producer wants to send; the
    /// Segment (and each Cluster) already uses the unknown-size VINT
    /// §23.2 mandates for a stream with no known end, so the output can
    /// be cut at any Cluster boundary. When combined with
    /// [`MkvMuxer::with_cluster_position_hints`], each Cluster's
    /// `Position` is written as `0` — the §5.1.3.2 live-stream convention
    /// (`PrevSize` stays real; the previous Cluster's size is known even
    /// live).
    ///
    /// Off by default. Must be called before [`Muxer::write_header`].
    /// The in-tree demuxer pairs naturally: a resilient open
    /// ([`crate::demux::open_resilient`]) of a live capture demuxes the
    /// Cluster stream, survives an arbitrary cut point, and can still
    /// seek via the Cues-less Cluster-Timestamp scan fallback.
    pub fn with_live_streaming(&mut self) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: with_live_streaming called after write_header",
            ));
        }
        self.live_streaming = true;
        Ok(self)
    }

    /// Read-only accessor: `true` when [`MkvMuxer::with_live_streaming`]
    /// was called.
    pub fn live_streaming(&self) -> bool {
        self.live_streaming
    }

    /// Set the per-track `Video > FlagInterlaced` (RFC 9559 §5.1.4.1.28.1)
    /// and optional `FieldOrder` (§5.1.4.1.28.2) for one stream. Must be
    /// called before [`Muxer::write_header`]; returns [`Error::other`]
    /// otherwise — both elements live in the `Video` master inside
    /// `Tracks`, which is written exactly once at header time.
    ///
    /// Spec rules enforced at queue time:
    ///
    /// * `stream_index` must point at an existing stream. Out-of-range
    ///   indices return [`Error::invalid`].
    /// * The target stream's [`MediaType`] must be
    ///   [`MediaType::Video`]. Non-video tracks have no `Video` master,
    ///   so the `FlagInterlaced` / `FieldOrder` elements have no on-disk
    ///   home for them and the call is rejected.
    /// * If `flag` is anything other than [`FlagInterlaced::Interlaced`],
    ///   `field_order` MUST be `None`. §5.1.4.1.28.2 mandates "If
    ///   FlagInterlaced is not set to 1, this element MUST be ignored",
    ///   so the muxer refuses to write a `FieldOrder` element that would
    ///   be a no-op on every conforming reader. An interlaced track MAY
    ///   pass `None`; the demuxer materialises the §5.1.4.1.28.2 default
    ///   `2` (undetermined) in that case.
    ///
    /// `FlagInterlaced::Other(v)` and `FieldOrder::Other(v)` round-trip
    /// their wrapped values verbatim, so a caller copying values from
    /// another file's `MkvDemuxer::video_interlacing(...)` (including
    /// forward-compatibility values registered after RFC 9559) gets a
    /// byte-faithful copy.
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued value. Calling it on a stream that already has
    /// `FlagInterlaced::Undetermined` + `field_order: None` is
    /// functionally a no-op (matches the on-disk omission default).
    ///
    /// Returns a mutable reference back so calls can chain
    /// builder-style if the caller has a `&mut MkvMuxer`.
    pub fn set_video_interlacing(
        &mut self,
        stream_index: usize,
        flag: FlagInterlaced,
        field_order: Option<FieldOrder>,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_video_interlacing called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_interlacing stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        if self.streams[stream_index].params.media_type != MediaType::Video {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_interlacing on stream {stream_index} ({}) — only Video tracks carry a Video master",
                self.streams[stream_index].params.codec_id.as_str()
            )));
        }
        // §5.1.4.1.28.2: "If FlagInterlaced is not set to 1, this element
        // MUST be ignored". Writing FieldOrder on a non-Interlaced track
        // would either be ignored by readers (best case) or trip
        // §5.1.4.1.28.2 conformance checkers; reject the call so the
        // caller sees the spec violation up front.
        if field_order.is_some() && flag != FlagInterlaced::Interlaced {
            return Err(Error::invalid(
                "MKV muxer: FieldOrder requires FlagInterlaced::Interlaced per RFC 9559 §5.1.4.1.28.2",
            ));
        }
        self.video_interlacings[stream_index] = Some(VideoInterlacingMux { flag, field_order });
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream interlacing hint
    /// installed via [`MkvMuxer::set_video_interlacing`]. Returns
    /// `None` for any stream that didn't have the API called (the
    /// muxer will omit both `FlagInterlaced` and `FieldOrder` from
    /// the on-disk `Video` master, and the demuxer materialises the
    /// §5.1.4.1.28.1 / §5.1.4.1.28.2 spec defaults). Mostly useful
    /// for tests; production callers typically just configure and
    /// then call `write_header`.
    pub fn video_interlacing(
        &self,
        stream_index: usize,
    ) -> Option<(FlagInterlaced, Option<FieldOrder>)> {
        self.video_interlacings
            .get(stream_index)?
            .map(|m| (m.flag, m.field_order))
    }

    /// Set the per-track `Video > StereoMode` (RFC 9559 §5.1.4.1.28.3) for
    /// one stream. Must be called before [`Muxer::write_header`]; returns
    /// [`Error::other`] otherwise — the element lives in the `Video` master
    /// inside `Tracks`, which is written exactly once at header time.
    ///
    /// Spec rules enforced at queue time:
    ///
    /// * `stream_index` must point at an existing stream. Out-of-range
    ///   indices return [`Error::invalid`].
    /// * The target stream's [`MediaType`] must be [`MediaType::Video`].
    ///   Non-video tracks have no `Video` master, so the element has no
    ///   on-disk home and the call is rejected.
    ///
    /// [`StereoMode::Other(v)`] round-trips its wrapped value verbatim, so
    /// a caller copying a value from another file's
    /// `MkvDemuxer::video_stereo_mode(...)` (including forward-compat values
    /// registered in §27.7 after RFC 9559) gets a byte-faithful copy.
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued value. Calling it with [`StereoMode::Mono`] (the
    /// spec default per §5.1.4.1.28.3) still writes the element on disk —
    /// that is the explicit way to override a downstream tool that might
    /// otherwise infer something else. Omitting the call entirely is
    /// functionally a no-op (matches the on-disk omission default).
    ///
    /// Returns a mutable reference back so calls can chain builder-style.
    pub fn set_video_stereo_mode(
        &mut self,
        stream_index: usize,
        mode: StereoMode,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_video_stereo_mode called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_stereo_mode stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        if self.streams[stream_index].params.media_type != MediaType::Video {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_stereo_mode on stream {stream_index} ({}) — only Video tracks carry a Video master",
                self.streams[stream_index].params.codec_id.as_str()
            )));
        }
        self.video_stereo_modes[stream_index] = Some(mode);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream `StereoMode` hint
    /// installed via [`MkvMuxer::set_video_stereo_mode`]. Returns `None`
    /// for any stream that didn't have the API called (the muxer will
    /// omit `StereoMode` from the on-disk `Video` master, and the demuxer
    /// materialises the §5.1.4.1.28.3 spec default `0` mono). Mostly
    /// useful for tests; production callers typically just configure and
    /// then call `write_header`.
    pub fn video_stereo_mode(&self, stream_index: usize) -> Option<StereoMode> {
        *self.video_stereo_modes.get(stream_index)?
    }

    /// Set the per-track legacy `Video > OldStereoMode` (RFC 9559
    /// §5.1.4.1.28.5, id `0x53B9`) for one stream. Must be called before
    /// [`Muxer::write_header`]; returns [`Error::other`] otherwise — the
    /// element lives in the `Video` master inside `Tracks`, written exactly
    /// once at header time.
    ///
    /// **This is a legacy / re-mux-only surface.** `OldStereoMode` is the
    /// "bogus" stereo-3D value libmatroska prior to 0.9.0 wrote at the wrong
    /// Element ID (`0x53B9` instead of `0x53B8`, §18.10). The spec marks the
    /// element `maxver: 2` and tells Writers MUST NOT use it for new files —
    /// modern stereo-3D belongs in `StereoMode` (see
    /// [`MkvMuxer::set_video_stereo_mode`]) or `TrackOperation`. This setter
    /// exists only so a faithful re-mux of a Matroska v2 / libmatroska-bug
    /// source can round-trip the element the demuxer surfaced through
    /// [`MkvDemuxer::video_old_stereo_mode`]. Omitting the call (the default)
    /// keeps the element off-disk — the correct behaviour for every modern
    /// file. The two value spaces are incompatible (Table 7 vs Table 5), so
    /// this never mixes with `set_video_stereo_mode`; a caller copying a
    /// legacy file should propagate whichever element the source carried.
    ///
    /// Spec rules enforced at queue time:
    ///
    /// * `stream_index` must point at an existing stream. Out-of-range indices
    ///   return [`Error::invalid`].
    /// * The target stream's [`MediaType`] must be [`MediaType::Video`].
    ///   Non-video tracks have no `Video` master and the call is rejected.
    ///
    /// [`OldStereoMode::Other(v)`] round-trips its wrapped value verbatim.
    /// Calling this twice on the same `stream_index` overwrites the previously
    /// queued value. Returns a mutable reference back so calls can chain
    /// builder-style.
    pub fn set_video_old_stereo_mode(
        &mut self,
        stream_index: usize,
        mode: OldStereoMode,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_video_old_stereo_mode called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_old_stereo_mode stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        if self.streams[stream_index].params.media_type != MediaType::Video {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_old_stereo_mode on stream {stream_index} ({}) — only Video tracks carry a Video master",
                self.streams[stream_index].params.codec_id.as_str()
            )));
        }
        self.video_old_stereo_modes[stream_index] = Some(mode);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream `OldStereoMode` hint
    /// installed via [`MkvMuxer::set_video_old_stereo_mode`]. Returns `None`
    /// for any stream that didn't have the API called (the muxer omits the
    /// legacy element — the correct behaviour for modern files). Mostly useful
    /// for tests.
    pub fn video_old_stereo_mode(&self, stream_index: usize) -> Option<OldStereoMode> {
        *self.video_old_stereo_modes.get(stream_index)?
    }

    /// Set the per-track `Video > AlphaMode` (RFC 9559 §5.1.4.1.28.4) for
    /// one stream. Must be called before [`Muxer::write_header`]; returns
    /// [`Error::other`] otherwise — the element lives in the `Video` master
    /// inside `Tracks`, which is written exactly once at header time.
    ///
    /// Spec rules enforced at queue time:
    ///
    /// * `stream_index` must point at an existing stream. Out-of-range
    ///   indices return [`Error::invalid`].
    /// * The target stream's [`MediaType`] must be [`MediaType::Video`].
    ///   Non-video tracks have no `Video` master and the call is rejected.
    ///
    /// Note that §5.1.4.1.28.4 itself warns that values outside `0` / `1`
    /// "SHOULD NOT be used, as the behavior of known implementations is
    /// different". [`AlphaMode::Other(v)`] is still accepted (and round-trips
    /// verbatim) for forward-compatibility with the §27.8 registry — the
    /// caller is responsible for knowing whether the consuming decoder
    /// understands the value.
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued value.
    ///
    /// Returns a mutable reference back so calls can chain builder-style.
    pub fn set_video_alpha_mode(
        &mut self,
        stream_index: usize,
        mode: AlphaMode,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_video_alpha_mode called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_alpha_mode stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        if self.streams[stream_index].params.media_type != MediaType::Video {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_alpha_mode on stream {stream_index} ({}) — only Video tracks carry a Video master",
                self.streams[stream_index].params.codec_id.as_str()
            )));
        }
        self.video_alpha_modes[stream_index] = Some(mode);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream `AlphaMode` hint
    /// installed via [`MkvMuxer::set_video_alpha_mode`]. Returns `None`
    /// for any stream that didn't have the API called (the muxer will
    /// omit `AlphaMode` from the on-disk `Video` master, and the demuxer
    /// materialises the §5.1.4.1.28.4 spec default `0` none). Mostly
    /// useful for tests.
    pub fn video_alpha_mode(&self, stream_index: usize) -> Option<AlphaMode> {
        *self.video_alpha_modes.get(stream_index)?
    }

    /// Set the per-track `TrackOperation` (RFC 9559 §5.1.4.1.30) for one
    /// stream — the recipe that makes the carrying `TrackEntry` a *virtual*
    /// track assembled from other tracks (stereoscopic-3D plane combining
    /// via `TrackCombinePlanes`, §5.1.4.1.30.1, and/or Block joining via
    /// `TrackJoinBlocks`, §5.1.4.1.30.5). Must be called before
    /// [`Muxer::write_header`]; returns [`Error::other`] otherwise — the
    /// element lives in `Tracks`, which is written exactly once at header
    /// time.
    ///
    /// References inside the [`MkvTrackOperation`] are 0-indexed stream
    /// indices (the same unit [`Muxer::write_packet`] uses). The muxer
    /// resolves each one to the source track's on-disk `TrackUID` at
    /// `write_header` time — the symmetric inverse of the demux side, which
    /// resolves each UID back to a stream index.
    ///
    /// Spec rules enforced at queue time:
    ///
    /// * `stream_index` must point at an existing stream. Out-of-range
    ///   indices return [`Error::invalid`].
    /// * The operation must not be empty (`TrackCombinePlanes` /
    ///   `TrackJoinBlocks` are masters whose only purpose is to carry
    ///   references — an empty `TrackOperation` is inert). An
    ///   [`MkvTrackOperation::is_empty`] queue is rejected.
    /// * Every `TrackPlaneUID` / `TrackJoinUID` references a track by its
    ///   `TrackUID`, which the spec pins to "not 0"
    ///   (§5.1.4.1.30.3 / §5.1.4.1.30.6). Because the muxer derives each
    ///   `TrackUID` from the 1-based stream index, every referenced
    ///   `stream_index` must itself point at an existing stream; an
    ///   out-of-range plane / join reference is rejected. A track MAY
    ///   reference itself — the spec does not forbid it.
    ///
    /// Note that §5.1.4.1.30 marks `TrackOperation` `minver: 3`; the muxer
    /// pins `DocTypeVersion` to `4`, so emitting it never violates the
    /// declared document version.
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued operation.
    ///
    /// Returns a mutable reference back so calls can chain builder-style.
    pub fn set_track_operation(
        &mut self,
        stream_index: usize,
        op: MkvTrackOperation,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_track_operation called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_track_operation stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        if op.is_empty() {
            return Err(Error::invalid(
                "MKV muxer: set_track_operation given an empty TrackOperation \
                 (no TrackCombinePlanes planes and no TrackJoinBlocks references)",
            ));
        }
        let n = self.streams.len();
        for p in &op.combine_planes {
            if p.stream_index >= n {
                return Err(Error::invalid(format!(
                    "MKV muxer: set_track_operation TrackPlane references stream_index {} out of range ({n} streams)",
                    p.stream_index
                )));
            }
        }
        for &j in &op.join_tracks {
            if j >= n {
                return Err(Error::invalid(format!(
                    "MKV muxer: set_track_operation TrackJoinBlocks references stream_index {j} out of range ({n} streams)"
                )));
            }
        }
        self.track_operations[stream_index] = Some(op);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream `TrackOperation` hint
    /// installed via [`MkvMuxer::set_track_operation`]. Returns `None` for
    /// any stream that didn't have the API called (the muxer omits the
    /// `TrackOperation` master, and the demuxer surfaces `None` from
    /// `track_operation`). Mostly useful for tests.
    pub fn track_operation(&self, stream_index: usize) -> Option<&MkvTrackOperation> {
        self.track_operations.get(stream_index)?.as_ref()
    }

    /// Queue the per-track `TrackTranslate` masters (RFC 9559 §5.1.4.1.27) for
    /// a stream — the chapter-codec track-mapping that lets a file be remuxed
    /// without rewriting opaque chapter-codec data. Each [`MkvTrackTranslate`]
    /// lands as a `TrackTranslate` master directly inside the carrying
    /// `TrackEntry` (after the existing `TrackOperation` / `ContentEncodings`
    /// masters) at `write_header` time, in slice order.
    ///
    /// Unlike the `set_video_*` family there is no track-type restriction —
    /// the spec carries `TrackTranslate` on every `TrackEntry`. Calling with
    /// an empty slice clears any previously-queued mappings for the stream so
    /// the muxer emits nothing (the demuxer then surfaces an empty slice from
    /// `track_translates`); repeated calls are last-write-wins.
    ///
    /// Spec rules enforced at queue time (before any byte is written): rejects
    /// post-`write_header` use, an out-of-range `stream_index`, a mapping with
    /// an empty `track_id` (`TrackTranslateTrackID` is `minOccurs: 1`), and a
    /// zero `TrackTranslateEditionUID` (the spec ranges each "not 0").
    ///
    /// Pairs symmetrically with
    /// [`MkvDemuxer::track_translates`](crate::demux::MkvDemuxer::track_translates)
    /// — a mux→demux pipeline round-trips every mapping field-for-field.
    ///
    /// Returns a mutable reference back so calls can chain builder-style.
    pub fn set_track_translates(
        &mut self,
        stream_index: usize,
        translates: Vec<MkvTrackTranslate>,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_track_translates called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_track_translates stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        for tt in &translates {
            if tt.track_id.is_empty() {
                return Err(Error::invalid(
                    "MKV muxer: set_track_translates given a TrackTranslate with an empty \
                     TrackTranslateTrackID (the child is mandatory, minOccurs 1)",
                ));
            }
            if tt.edition_uids.contains(&0) {
                return Err(Error::invalid(
                    "MKV muxer: set_track_translates given a zero TrackTranslateEditionUID \
                     (the spec ranges each value 'not 0')",
                ));
            }
        }
        self.track_translates[stream_index] = translates;
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream `TrackTranslate` hints
    /// installed via [`MkvMuxer::set_track_translates`]. Returns an empty
    /// slice for any stream that didn't have the API called. Mostly useful
    /// for tests.
    pub fn track_translates(&self, stream_index: usize) -> &[MkvTrackTranslate] {
        self.track_translates
            .get(stream_index)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Queue the reclaimed Appendix-A `TrackEntry`-level legacy elements
    /// (RFC 9559 Appendix A.19..A.23 + A.28..A.32) for a stream — the
    /// historical `CodecSettings` / `CodecInfoURL` / `CodecDownloadURL` /
    /// `CodecDecodeAll` codec-description metadata, the ordered `TrackOverlay`
    /// fallback list, and the DivXTrickTrack pairing quintet. Each populated
    /// field lands as its element directly inside the carrying `TrackEntry`
    /// (after the `TrackTranslate` masters) at `write_header` time.
    ///
    /// Only populated fields reach the disk: an absent field (`None` / empty
    /// `Vec`) keeps its element off-disk, so the demuxer observes the same
    /// absence. The appendix specifies no defaults, so a `Some(0)`
    /// `decode_all` / `trick_track_flag` is written as an explicit `0`,
    /// distinct from omission. Calling with an all-absent record (or `None`)
    /// clears any previously-queued legacy elements; repeated calls are
    /// last-write-wins.
    ///
    /// Spec rules enforced at queue time (before any byte is written): rejects
    /// post-`write_header` use, an out-of-range `stream_index`, and a SegmentUID
    /// binary (`TrickTrackSegmentUID` / `TrickMasterTrackSegmentUID`) whose
    /// length is not the canonical 16 bytes (a `SegmentUUID` is a 128-bit
    /// value per RFC 9559 §5.1.2.1).
    ///
    /// Pairs symmetrically with
    /// [`MkvDemuxer::track_legacy`](crate::demux::MkvDemuxer::track_legacy) —
    /// a mux→demux pipeline round-trips every populated field verbatim.
    ///
    /// Returns a mutable reference back so calls can chain builder-style.
    pub fn set_track_legacy(
        &mut self,
        stream_index: usize,
        legacy: MkvTrackLegacy,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_track_legacy called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_track_legacy stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        for (label, uid) in [
            ("TrickTrackSegmentUID", &legacy.trick_track_segment_uid),
            (
                "TrickMasterTrackSegmentUID",
                &legacy.trick_master_track_segment_uid,
            ),
        ] {
            if let Some(bytes) = uid {
                if bytes.len() != 16 {
                    return Err(Error::invalid(format!(
                        "MKV muxer: set_track_legacy given a {label} of {} bytes \
                         (a SegmentUUID is a 128-bit / 16-byte value)",
                        bytes.len()
                    )));
                }
            }
        }
        self.track_legacy[stream_index] = if legacy.is_empty() {
            None
        } else {
            Some(legacy)
        };
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream `TrackLegacy` hint
    /// installed via [`MkvMuxer::set_track_legacy`]. Returns `None` for any
    /// stream that didn't have the API called (or was cleared). Mostly useful
    /// for tests.
    pub fn track_legacy(&self, stream_index: usize) -> Option<&MkvTrackLegacy> {
        self.track_legacy.get(stream_index)?.as_ref()
    }

    /// Set the per-track `ContentEncodings` (RFC 9559 §5.1.4.1.31) chain —
    /// the ordered list of compression and/or encryption transformations
    /// that were applied to the track's frame data and/or `CodecPrivate`
    /// before the bytes were written into Blocks. Must be called before
    /// [`Muxer::write_header`]; returns [`Error::other`] otherwise (the
    /// `ContentEncodings` master lives in the `TrackEntry` inside `Tracks`,
    /// which is written exactly once at header time).
    ///
    /// This is the symmetric inverse of the demux-side
    /// [`MkvDemuxer::content_encodings`](crate::demux::MkvDemuxer::content_encodings)
    /// accessor: the muxer takes the same
    /// [`ContentEncodings`](crate::demux::ContentEncodings) record the
    /// demuxer produces and serialises it back element-for-element, so a
    /// mux→demux pipeline round-trips the whole chain.
    ///
    /// The container is purely a *carrier* here — it does **not** compress
    /// or encrypt the frame data. The caller is responsible for having
    /// already transformed the bytes it hands to [`Muxer::write_packet`] to
    /// match the declared chain (e.g. stripping the `ContentCompSettings`
    /// prefix for Header Stripping, or encrypting for an `Encryption`
    /// step). Declaring an encoding the bytes don't actually carry produces
    /// a file no reader can decode.
    ///
    /// Spec rules enforced at queue time (all checked before any byte is
    /// written, so a rejected call leaves the muxer state untouched):
    ///
    /// * `stream_index` must point at an existing stream
    ///   ([`Error::invalid`] otherwise).
    /// * At least one [`ContentEncoding`](crate::demux::ContentEncoding)
    ///   must be present — `ContentEncoding` is `minOccurs: 1` inside the
    ///   `ContentEncodings` master (§5.1.4.1.31.1), so an empty chain would
    ///   serialise to a malformed master. An empty chain is rejected; pass
    ///   no hint at all to leave the element off-disk.
    /// * Each `ContentEncodingOrder` (§5.1.4.1.31.2) "MUST be unique for
    ///   each ContentEncoding"; a duplicate order is rejected.
    /// * Each `ContentEncodingScope` (§5.1.4.1.31.3) is `range: not 0`; a
    ///   scope of `0` is rejected.
    /// * For an [`Encryption`](crate::demux::ContentEncodingTransform::Encryption)
    ///   step, `AESSettingsCipherMode` (§5.1.4.1.31.12) is `range: not 0`,
    ///   and `ContentEncAESSettings` "MUST NOT be set if ContentEncAlgo is
    ///   not AES" (Table 25 / Table 27). So a `Some(cipher_mode)` is
    ///   rejected on any non-AES algorithm, and an `aes_cipher_mode` of
    ///   `Some(AesCipherMode::Other(0))` is rejected.
    ///
    /// Write-time element ordering matches the demux-side parser's
    /// expectations and the spec paths: the muxer sorts the chain by
    /// **ascending** `ContentEncodingOrder` on disk (lowest first) — the
    /// demuxer re-sorts it into descending decode order on read, so either
    /// on-disk order parses identically; ascending is the conventional
    /// write order. Each `ContentEncoding` carries `ContentEncodingOrder`,
    /// `ContentEncodingScope`, `ContentEncodingType` (derived from the
    /// transform kind), then the matching `ContentCompression` /
    /// `ContentEncryption` sub-master.
    ///
    /// Per-element omission inside each `ContentEncoding` mirrors the
    /// demux-side defaults so a round-trip is byte-exact for the common
    /// shapes:
    ///
    /// * `ContentEncodingType` is written explicitly (the demux parser
    ///   keys the transform off it).
    /// * `ContentCompSettings` (§5.1.4.1.31.7) is written only when
    ///   non-empty.
    /// * `ContentEncKeyID` (§5.1.4.1.31.10) is written only when non-empty.
    /// * `ContentEncAESSettings` (§5.1.4.1.31.11) — and the
    ///   `AESSettingsCipherMode` inside it — is written only when
    ///   `aes_cipher_mode` is `Some` (which the validation above pins to
    ///   AES-only).
    ///
    /// The muxer pins `DocTypeVersion` to `4`, so emitting the `minver: 4`
    /// `ContentEncAESSettings` / `AESSettingsCipherMode` elements never
    /// violates the declared document version.
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued chain. Returns a mutable reference back so calls
    /// can chain builder-style.
    pub fn set_track_content_encodings(
        &mut self,
        stream_index: usize,
        encodings: ContentEncodings,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_track_content_encodings called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_track_content_encodings stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        if encodings.encodings.is_empty() {
            return Err(Error::invalid(
                "MKV muxer: set_track_content_encodings given an empty ContentEncodings \
                 (ContentEncoding is minOccurs: 1 inside the master, RFC 9559 §5.1.4.1.31.1)",
            ));
        }
        let mut seen_orders: Vec<u64> = Vec::with_capacity(encodings.encodings.len());
        for e in &encodings.encodings {
            if seen_orders.contains(&e.order) {
                return Err(Error::invalid(format!(
                    "MKV muxer: set_track_content_encodings duplicate ContentEncodingOrder {} \
                     (RFC 9559 §5.1.4.1.31.2 requires it be unique within the chain)",
                    e.order
                )));
            }
            seen_orders.push(e.order);
            if e.scope.0 == 0 {
                return Err(Error::invalid(
                    "MKV muxer: set_track_content_encodings ContentEncodingScope 0 out of range \
                     (RFC 9559 §5.1.4.1.31.3 pins it at 'not 0')"
                        .to_string(),
                ));
            }
            if let ContentEncodingTransform::Encryption {
                algo,
                aes_cipher_mode: Some(mode),
                ..
            } = &e.transform
            {
                if *algo != crate::demux::ContentEncAlgo::Aes {
                    return Err(Error::invalid(
                        "MKV muxer: set_track_content_encodings ContentEncAESSettings set on a \
                         non-AES ContentEncAlgo (RFC 9559 Table 25 forbids it)"
                            .to_string(),
                    ));
                }
                if mode.to_raw() == 0 {
                    return Err(Error::invalid(
                        "MKV muxer: set_track_content_encodings AESSettingsCipherMode 0 out of \
                         range (RFC 9559 §5.1.4.1.31.12 pins it at 'not 0')"
                            .to_string(),
                    ));
                }
            }
        }
        self.content_encodings[stream_index] = Some(encodings);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream `ContentEncodings` hint
    /// installed via [`MkvMuxer::set_track_content_encodings`]. Returns
    /// `None` for any stream that didn't have the API called. Mostly useful
    /// for tests.
    pub fn content_encodings(&self, stream_index: usize) -> Option<&ContentEncodings> {
        self.content_encodings.get(stream_index)?.as_ref()
    }

    /// Set the per-track `Video > PixelCrop{Top,Bottom,Left,Right}` +
    /// `DisplayWidth` / `DisplayHeight` / `DisplayUnit` geometry quartet
    /// (RFC 9559 §5.1.4.1.28.8..§5.1.4.1.28.14) for one stream. Must be
    /// called before [`Muxer::write_header`]; returns [`Error::other`]
    /// otherwise — every targeted element lives in the `Video` master
    /// inside `Tracks`, which is written exactly once at header time.
    ///
    /// Spec rules enforced at queue time:
    ///
    /// * `stream_index` must point at an existing stream. Out-of-range
    ///   indices return [`Error::invalid`].
    /// * The target stream's [`MediaType`] must be [`MediaType::Video`].
    ///   Non-video tracks have no `Video` master, so the elements have no
    ///   on-disk home and the call is rejected.
    /// * `display_width == Some(0)` and `display_height == Some(0)` are
    ///   rejected. RFC 9559 §5.1.4.1.28.12 / .13 explicitly pins both
    ///   elements at `range: not 0`. Use `None` to leave the element off
    ///   disk instead.
    ///
    /// Element omission rules at write time:
    ///
    /// * A zero crop on any of the four axes is left off-disk so the
    ///   demuxer materialises the §5.1.4.1.28.8..11 default `0`. A
    ///   non-zero crop is always written explicitly.
    /// * `display_width` / `display_height` are written when `Some`,
    ///   skipped when `None`. When skipped + `display_unit == Pixels`,
    ///   the demuxer derives the value from `PixelWidth - crop_left -
    ///   crop_right` (resp. height). Skipped + non-Pixels: the demuxer
    ///   returns `None` (the spec mandates "there is no default value").
    /// * `display_unit` is written when not [`DisplayUnit::Pixels`].
    ///   Setting it to `Pixels` (the spec default) is treated as "omit
    ///   the element" so a downstream re-mux of a file that did not
    ///   carry an explicit `DisplayUnit` stays byte-faithful to the
    ///   common case. [`DisplayUnit::Other(v)`] round-trips its wrapped
    ///   value verbatim for §27.9 forward-compat.
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued value. Calling it with a zero-everything
    /// [`MkvVideoGeometry::default()`] is functionally a no-op (matches
    /// the on-disk omission default).
    ///
    /// Returns a mutable reference back so calls can chain builder-style.
    pub fn set_video_geometry(
        &mut self,
        stream_index: usize,
        geometry: MkvVideoGeometry,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_video_geometry called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_geometry stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        if self.streams[stream_index].params.media_type != MediaType::Video {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_geometry on stream {stream_index} ({}) — only Video tracks carry a Video master",
                self.streams[stream_index].params.codec_id.as_str()
            )));
        }
        // §5.1.4.1.28.12 / .13: DisplayWidth / DisplayHeight are pinned at
        // `range: not 0`. Reject Some(0) so a caller who meant "omit"
        // hears about the spec violation up front rather than producing a
        // file conforming readers will refuse to load.
        if matches!(geometry.display_width, Some(0)) {
            return Err(Error::invalid(
                "MKV muxer: set_video_geometry display_width == Some(0) violates RFC 9559 §5.1.4.1.28.12 (range: not 0). Use None to omit the element.",
            ));
        }
        if matches!(geometry.display_height, Some(0)) {
            return Err(Error::invalid(
                "MKV muxer: set_video_geometry display_height == Some(0) violates RFC 9559 §5.1.4.1.28.13 (range: not 0). Use None to omit the element.",
            ));
        }
        self.video_geometries[stream_index] = Some(geometry);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream geometry hint
    /// installed via [`MkvMuxer::set_video_geometry`]. Returns `None`
    /// for any stream that didn't have the API called (the muxer will
    /// omit all seven geometry elements from the on-disk `Video` master,
    /// and the demuxer materialises the §5.1.4.1.28.8..14 spec defaults).
    /// Mostly useful for tests.
    pub fn video_geometry(&self, stream_index: usize) -> Option<MkvVideoGeometry> {
        *self.video_geometries.get(stream_index)?
    }

    /// Set the per-track `Video > UncompressedFourCC` (RFC 9559
    /// §5.1.4.1.28.15) for one stream. Must be called before
    /// [`Muxer::write_header`]; returns [`Error::other`] otherwise —
    /// the element lives in the `Video` master inside `Tracks`, which is
    /// written exactly once at header time.
    ///
    /// `fourcc` is a four-byte FourCC identifying the uncompressed pixel
    /// layout used by the track's frames (e.g. `*b"YUY2"`, `*b"BGRA"`).
    /// The spec defines neither a definitive list of values nor an
    /// official registry — the caller is responsible for picking a
    /// FourCC the consuming decoder understands. The on-disk element
    /// length is pinned to exactly 4 bytes per §5.1.4.1.28.15's
    /// `length: 4` schema field; the muxer enforces this by taking a
    /// `[u8; 4]` array directly.
    ///
    /// Spec rules enforced at queue time:
    ///
    /// * `stream_index` must point at an existing stream. Out-of-range
    ///   indices return [`Error::invalid`].
    /// * The target stream's [`MediaType`] must be [`MediaType::Video`].
    ///   Non-video tracks have no `Video` master and the call is rejected.
    ///
    /// Omitting the call leaves the element off-disk so the demuxer
    /// surfaces `None` for that stream's
    /// `MkvDemuxer::video_uncompressed_fourcc`. Per §5.1.4.1.28.15
    /// Table 11 the element is only spec-mandatory when `CodecID ==
    /// "V_UNCOMPRESSED"`, and the muxer does not currently emit that
    /// codec id, so the omission case stays spec-conformant for every
    /// codec the muxer presently supports.
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued value (last-write-wins).
    ///
    /// Returns a mutable reference back so calls can chain builder-style.
    pub fn set_video_uncompressed_fourcc(
        &mut self,
        stream_index: usize,
        fourcc: [u8; 4],
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_video_uncompressed_fourcc called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_uncompressed_fourcc stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        if self.streams[stream_index].params.media_type != MediaType::Video {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_uncompressed_fourcc on stream {stream_index} ({}) — only Video tracks carry a Video master",
                self.streams[stream_index].params.codec_id.as_str()
            )));
        }
        self.video_uncompressed_fourccs[stream_index] = Some(fourcc);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream `UncompressedFourCC`
    /// hint installed via [`MkvMuxer::set_video_uncompressed_fourcc`].
    /// Returns `None` for any stream that didn't have the API called
    /// (the muxer omits the element from the on-disk `Video` master,
    /// and the demuxer surfaces `None` as well). Mostly useful for
    /// tests.
    pub fn video_uncompressed_fourcc(&self, stream_index: usize) -> Option<[u8; 4]> {
        *self.video_uncompressed_fourccs.get(stream_index)?
    }

    /// Set the per-track `Video > AspectRatioType` (RFC 9559 Appendix A.24,
    /// reclaimed, id `0x54B3`) for one stream. Must be called before
    /// [`Muxer::write_header`]; returns [`Error::other`] otherwise — the
    /// element lives in the `Video` master inside `Tracks`, which is
    /// written exactly once at header time.
    ///
    /// `value` is the raw `uinteger` written verbatim. The reclaimed
    /// appendix documents the element only as "Specifies the possible
    /// modifications to the aspect ratio" and enumerates no values and no
    /// default — the demux side deliberately surfaces it as a raw
    /// `Option<u64>` rather than a synthesised enum, and this setter
    /// mirrors that: the caller owns the meaning of the value.
    ///
    /// Spec rules enforced at queue time:
    ///
    /// * `stream_index` must point at an existing stream. Out-of-range
    ///   indices return [`Error::invalid`].
    /// * The target stream's [`MediaType`] must be [`MediaType::Video`].
    ///   Non-video tracks have no `Video` master and the call is rejected.
    ///
    /// Omitting the call leaves the element off-disk so the demuxer
    /// surfaces `None` for that stream's
    /// `MkvDemuxer::video_aspect_ratio_type` — the appendix defines no
    /// default, so absence is not materialised on either side.
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued value (last-write-wins).
    ///
    /// Returns a mutable reference back so calls can chain builder-style.
    pub fn set_video_aspect_ratio_type(
        &mut self,
        stream_index: usize,
        value: u64,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_video_aspect_ratio_type called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_aspect_ratio_type stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        if self.streams[stream_index].params.media_type != MediaType::Video {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_aspect_ratio_type on stream {stream_index} ({}) — only Video tracks carry a Video master",
                self.streams[stream_index].params.codec_id.as_str()
            )));
        }
        self.video_aspect_ratio_types[stream_index] = Some(value);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream `AspectRatioType`
    /// hint installed via [`MkvMuxer::set_video_aspect_ratio_type`].
    /// Returns `None` for any stream that didn't have the API called
    /// (the muxer omits the element from the on-disk `Video` master,
    /// and the demuxer surfaces `None` as well). Mostly useful for
    /// tests.
    pub fn video_aspect_ratio_type(&self, stream_index: usize) -> Option<u64> {
        *self.video_aspect_ratio_types.get(stream_index)?
    }

    /// Set the per-track `Video > Colour` master (RFC 9559 §5.1.4.1.28.16)
    /// for one stream. Must be called before [`Muxer::write_header`];
    /// returns [`Error::other`] otherwise.
    ///
    /// The supplied [`MkvVideoColour`] is materialised inside the
    /// `Video` master at `write_header` time. The muxer applies per-element
    /// omission rules to keep the file minimal — every scalar child that
    /// equals its §5.1.4.1.28 spec default is left off-disk so the demuxer
    /// surfaces the spec-defined default value; every `Option<…>` field
    /// is only emitted when `Some(v)`. As a result, calling this with
    /// [`MkvVideoColour::default`] writes an *empty* `Colour` master on
    /// disk — present-but-childless — which the demuxer parses into a
    /// `Some(VideoColour)` whose every getter returns the spec default,
    /// distinguishable from the call-was-omitted case (`None`).
    ///
    /// Errors:
    ///
    /// * `Error::other` — called after `write_header`.
    /// * `Error::invalid` — `stream_index` is out of range, or the stream
    ///   at that index is not [`MediaType::Video`] (only video tracks
    ///   carry a `Video` master per RFC 9559 §5.1.4.1.28).
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued value (last-write-wins).
    ///
    /// Omitting the call entirely keeps the `Colour` master off-disk so
    /// the demuxer surfaces `None` from
    /// [`crate::demux::MkvDemuxer::video_colour`] — distinct from "empty
    /// `Colour` master present" (`Some(VideoColour::default())`).
    ///
    /// `MasteringMetadata` (§5.1.4.1.28.30..§5.1.4.1.28.40) is emitted
    /// whenever the queued [`MkvVideoColour::mastering_metadata`] slot
    /// is `Some(_)`. Inside that master, each chromaticity / luminance
    /// child is written only when its own `Option<f64>` slot is
    /// `Some(v)`, mirroring the scalar-child omission rules above —
    /// a [`MkvMasteringMetadata::default`] (all slots `None`) inside
    /// `Some(_)` round-trips through an empty `MasteringMetadata`
    /// master on disk.
    ///
    /// Returns a mutable reference back so calls can chain builder-style.
    pub fn set_video_colour(
        &mut self,
        stream_index: usize,
        colour: MkvVideoColour,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_video_colour called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_colour stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        if self.streams[stream_index].params.media_type != MediaType::Video {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_colour on stream {stream_index} ({}) — only Video tracks carry a Video master",
                self.streams[stream_index].params.codec_id.as_str()
            )));
        }
        self.video_colours[stream_index] = Some(colour);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream `Colour` hint
    /// installed via [`MkvMuxer::set_video_colour`]. Returns `None` for
    /// any stream that didn't have the API called (the muxer omits the
    /// `Colour` master from the on-disk `Video` master, and the demuxer
    /// surfaces `None` as well). Mostly useful for tests.
    pub fn video_colour(&self, stream_index: usize) -> Option<MkvVideoColour> {
        *self.video_colours.get(stream_index)?
    }

    /// Set the per-track `Video > Projection` master (RFC 9559
    /// §5.1.4.1.28.41, including the §5.1.4.1.28.42..§5.1.4.1.28.46
    /// sub-elements) for one stream. Must be called before
    /// [`Muxer::write_header`]; returns [`Error::other`] otherwise.
    ///
    /// The supplied [`MkvProjection`] is materialised inside the `Video`
    /// master at `write_header` time as a `Projection` master placed after
    /// the `Colour` master. The muxer applies per-element omission rules to
    /// keep the file minimal: `ProjectionType` is written only when it is
    /// not [`ProjectionType::Rectangular`] (the §5.1.4.1.28.42 default `0`),
    /// each `ProjectionPose{Yaw,Pitch,Roll}` child is written only when
    /// non-zero (the §5.1.4.1.28.44..46 default `0.0`), and
    /// `ProjectionPrivate` is written only when `private` is `Some(_)`. As a
    /// result, calling this with [`MkvProjection::default`] writes an
    /// *empty* `Projection` master on disk — present-but-childless — which
    /// the demuxer parses into a `Some(Projection)` whose every getter
    /// returns the spec default (rectangular, zero pose, no private),
    /// distinguishable from the call-was-omitted case (`None`).
    ///
    /// Errors:
    ///
    /// * `Error::other` — called after `write_header`.
    /// * `Error::invalid` — `stream_index` is out of range, or the stream
    ///   at that index is not [`MediaType::Video`] (only video tracks
    ///   carry a `Video` master per RFC 9559 §5.1.4.1.28).
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued value (last-write-wins).
    ///
    /// Omitting the call entirely keeps the `Projection` master off-disk so
    /// the demuxer surfaces `None` from
    /// [`crate::demux::MkvDemuxer::video_projection`] — distinct from "empty
    /// `Projection` master present" (`Some(Projection::default())`).
    ///
    /// Pairs symmetrically with the existing
    /// [`crate::demux::MkvDemuxer::video_projection`] typed accessor — a
    /// mux→demux pipeline preserves the projection record (type, pose, and
    /// verbatim `ProjectionPrivate` payload) bit-exactly.
    ///
    /// Returns a mutable reference back so calls can chain builder-style.
    pub fn set_video_projection(
        &mut self,
        stream_index: usize,
        projection: MkvProjection,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_video_projection called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_projection stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        if self.streams[stream_index].params.media_type != MediaType::Video {
            return Err(Error::invalid(format!(
                "MKV muxer: set_video_projection on stream {stream_index} ({}) — only Video tracks carry a Video master",
                self.streams[stream_index].params.codec_id.as_str()
            )));
        }
        self.video_projections[stream_index] = Some(projection);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream `Projection` hint
    /// installed via [`MkvMuxer::set_video_projection`]. Returns `None` for
    /// any stream that didn't have the API called (the muxer omits the
    /// `Projection` master from the on-disk `Video` master, and the demuxer
    /// surfaces `None` as well). Mostly useful for tests.
    pub fn video_projection(&self, stream_index: usize) -> Option<&MkvProjection> {
        self.video_projections.get(stream_index)?.as_ref()
    }

    /// Set the per-track audience flags (RFC 9559 §5.1.4.1.6..
    /// §5.1.4.1.11 — `FlagForced` / `FlagHearingImpaired` /
    /// `FlagVisualImpaired` / `FlagTextDescriptions` / `FlagOriginal` /
    /// `FlagCommentary`) for one stream. Must be called before
    /// [`Muxer::write_header`]; returns [`Error::other`] otherwise — the
    /// six elements live directly in the `TrackEntry`, which is written
    /// exactly once at header time.
    ///
    /// Unlike the `set_video_*` family there is **no track-type
    /// restriction**: the spec carries all six elements on every
    /// `TrackEntry` (`FlagForced` with `minOccurs: 1`), so audio, video,
    /// and subtitle tracks all accept the call. §5.1.4.1.6's "applies
    /// only to subtitles" note describes player semantics, not an
    /// on-disk placement constraint — mirroring the demux side, which
    /// surfaces a [`crate::demux::TrackAudienceFlags`] record for every
    /// track.
    ///
    /// Per-element omission rule: each `Some(v)` slot writes the element
    /// explicitly as `0` / `1`; each `None` slot keeps it off-disk. For
    /// `FlagForced` (the only one with a spec default), omission and
    /// `Some(false)` decode identically (`false`) but differ on disk.
    /// For the five default-less `minver: 4` flags, omission decodes as
    /// `None` while `Some(false)` decodes as `Some(false)` — the
    /// explicit-zero "the track is definitely NOT x" signal the
    /// §5.1.4.1.7..§5.1.4.1.11 "if and only if" wording defines.
    ///
    /// Errors:
    ///
    /// * `Error::other` — called after `write_header`.
    /// * `Error::invalid` — `stream_index` is out of range.
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued record (last-write-wins). Queueing
    /// [`MkvTrackAudienceFlags::default`] (every slot `None`) is legal
    /// and functionally a no-op — no element reaches disk.
    ///
    /// Pairs symmetrically with the existing
    /// [`crate::demux::MkvDemuxer::track_audience_flags`] typed accessor
    /// — a mux→demux pipeline preserves every explicit flag, including
    /// the `Some(false)`-vs-absent distinction on the `minver: 4` five.
    ///
    /// Returns a mutable reference back so calls can chain builder-style.
    pub fn set_track_audience_flags(
        &mut self,
        stream_index: usize,
        flags: MkvTrackAudienceFlags,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_track_audience_flags called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_track_audience_flags stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        self.track_audience_flags[stream_index] = Some(flags);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream audience-flag hint
    /// installed via [`MkvMuxer::set_track_audience_flags`]. Returns
    /// `None` for any stream that didn't have the API called (the muxer
    /// omits all six elements, so the demuxer materialises the
    /// §5.1.4.1.6 default `false` for `forced()` and `None` for the
    /// five `minver: 4` flags). Mostly useful for tests.
    pub fn track_audience_flags(&self, stream_index: usize) -> Option<MkvTrackAudienceFlags> {
        *self.track_audience_flags.get(stream_index)?
    }

    /// Declare the track's `MaxBlockAdditionID` (RFC 9559 §5.1.4.1.16) —
    /// the maximum `BlockAddID` (§5.1.3.5.2.3) value any of the track's
    /// Blocks may carry. Must be called before [`Muxer::write_header`];
    /// returns [`Error::other`] otherwise — the element lives in the
    /// `TrackEntry`, which is written exactly once at header time.
    ///
    /// Declaring a non-zero value is the prerequisite for
    /// [`MkvMuxer::write_packet_with_additions`] on the stream: the spec
    /// default `0` means "there is no BlockAdditions for this track", so
    /// the muxer refuses to attach additions to an undeclared track
    /// rather than emit a file whose Blocks contradict its `TrackEntry`.
    ///
    /// Omission rule: skipping the call keeps the element off-disk (the
    /// demuxer materialises the spec default `0`); calling it writes the
    /// element explicitly — including `set_max_block_addition_id(i, 0)`,
    /// which decodes identically to absence but is byte-distinct (the
    /// explicit producer-override path). There is no track-type
    /// restriction — the spec carries the element on every `TrackEntry`
    /// with `minOccurs: 1`.
    ///
    /// Errors:
    ///
    /// * `Error::other` — called after `write_header`.
    /// * `Error::invalid` — `stream_index` is out of range.
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued value (last-write-wins). Returns a mutable
    /// reference back so calls can chain builder-style.
    pub fn set_max_block_addition_id(
        &mut self,
        stream_index: usize,
        max: u64,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_max_block_addition_id called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_max_block_addition_id stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        self.max_block_addition_ids[stream_index] = Some(max);
        Ok(self)
    }

    /// Queue a `SilentTracks > SilentTrackNumber` list (RFC 9559 Appendix
    /// A.1 / A.2) to be written on the **next** Cluster the muxer opens —
    /// the write-side counterpart of
    /// [`ClusterRecord::silent_track_numbers`](crate::demux::ClusterRecord::silent_track_numbers).
    ///
    /// `track_numbers` are on-wire `TrackNumber`s (the same values the
    /// muxer assigns each stream, queryable via
    /// [`MkvMuxer::track_number`]), listing the tracks "not used in that
    /// part of the stream". The list applies to exactly one Cluster: it is
    /// drained when that Cluster's header is written, mirroring the
    /// per-Cluster scope of the element (A.2 — a track silent in one
    /// Cluster MAY be active again in a later one).
    ///
    /// Because the muxer decides Cluster boundaries from packet timing,
    /// "next Cluster" means the next one opened by a `write_packet*` call
    /// (or the first, if none is open yet). Call this immediately before
    /// the first packet of the Cluster you want the element on. Passing an
    /// empty slice clears any queued list. The element is deprecated
    /// (`maxver: 0`) but emitted by historical Writers; the muxer offers it
    /// for faithful re-mux.
    pub fn set_next_cluster_silent_tracks(&mut self, track_numbers: &[u64]) {
        self.pending_silent_tracks = track_numbers.to_vec();
    }

    /// The on-wire `TrackNumber` (RFC 9559 §5.1.4.1.1) the muxer assigned
    /// the stream at `stream_index`, or `None` for an out-of-range index.
    /// Useful for building a [`MkvMuxer::set_next_cluster_silent_tracks`]
    /// list from stream indices.
    pub fn track_number(&self, stream_index: usize) -> Option<u64> {
        self.track_numbers.get(stream_index).copied()
    }

    /// Read-only accessor for the queued per-stream `MaxBlockAdditionID`
    /// hint installed via [`MkvMuxer::set_max_block_addition_id`].
    /// Returns `None` for any stream that didn't have the API called
    /// (the element stays off-disk and the demuxer materialises the
    /// §5.1.4.1.16 default `0`). Mostly useful for tests.
    pub fn max_block_addition_id(&self, stream_index: usize) -> Option<u64> {
        *self.max_block_addition_ids.get(stream_index)?
    }

    /// Set the per-track `BlockAdditionMapping` masters (RFC 9559
    /// §5.1.4.1.17) for one stream — the declarations describing how each
    /// non-default `BlockAddID` (§5.1.3.5.2.3) side channel a track emits is
    /// to be interpreted (WebM alpha, HDR dynamic metadata, ITU-T T.35, …).
    /// A track can carry any number of mappings (the spec sets no
    /// `maxOccurs`); they are written in slice order directly inside the
    /// carrying `TrackEntry` at `write_header` time.
    ///
    /// The setter takes the **same** demux-side
    /// [`crate::demux::BlockAdditionMapping`] records
    /// [`crate::demux::MkvDemuxer::block_addition_mappings`] surfaces, so a
    /// mux→demux pipeline round-trips every mapping element-for-element.
    /// Per-field omission mirrors the demux projection: `value` / `name` /
    /// `extra_data` write their `BlockAddIDValue` / `BlockAddIDName` /
    /// `BlockAddIDExtraData` child only when `Some`; `addid_type` writes its
    /// `BlockAddIDType` child only when non-zero (the §5.1.4.1.17.3 default
    /// `0` = codec-defined is materialised by the demuxer from an absent
    /// element, so a `0` stays off-disk and still round-trips as `0`).
    ///
    /// Must be called before [`Muxer::write_header`]. Passing an empty slice
    /// clears any previously-queued mappings; repeated calls are
    /// last-write-wins.
    ///
    /// Errors:
    ///
    /// * `Error::other` — called after `write_header`.
    /// * `Error::invalid` — `stream_index` out of range.
    ///
    /// Note that a `BlockAdditionMapping` only *declares* a side channel;
    /// the per-frame `BlockAdditional` bytes are written separately via
    /// [`MkvMuxer::write_packet_with_additions`], which the track's
    /// `MaxBlockAdditionID` (see [`MkvMuxer::set_max_block_addition_id`])
    /// gates. Returns a mutable reference back so calls can chain.
    pub fn set_block_addition_mappings(
        &mut self,
        stream_index: usize,
        mappings: Vec<crate::demux::BlockAdditionMapping>,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_block_addition_mappings called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_block_addition_mappings stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        self.block_addition_mappings[stream_index] = mappings;
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream `BlockAdditionMapping`
    /// list installed via [`MkvMuxer::set_block_addition_mappings`]. Returns
    /// an empty slice for any stream that didn't have the API called (or was
    /// cleared). Mostly useful for tests.
    pub fn block_addition_mappings(
        &self,
        stream_index: usize,
    ) -> &[crate::demux::BlockAdditionMapping] {
        self.block_addition_mappings
            .get(stream_index)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Set the per-track `Audio` master children (RFC 9559 §5.1.4.1.29)
    /// for one stream. Must be called before [`Muxer::write_header`];
    /// returns [`Error::other`] otherwise.
    ///
    /// The muxer already derives a minimal `Audio` master from the
    /// stream's [`StreamInfo`] (`sample_rate` → `SamplingFrequency`,
    /// `channels` → `Channels`, sample-format bit width → `BitDepth`).
    /// This hint lets a caller override those derived children and — most
    /// importantly — supply the `OutputSamplingFrequency` child
    /// (§5.1.4.1.29.2) the `StreamInfo`-derived path cannot express. That
    /// element is the Spectral Band Replication (SBR) output rate: a
    /// HE-AAC track typically encodes a half-rate core and signals the
    /// doubled output rate here, which the demuxer's
    /// [`crate::demux::TrackAudio::is_sbr`] predicate reads back.
    ///
    /// Per-field rule: a `Some(v)` overrides the `StreamInfo`-derived
    /// child; a `None` defers to the `StreamInfo` value (and for
    /// `output_sampling_frequency`, simply omits the element, since
    /// `StreamInfo` has no equivalent). When neither the hint nor
    /// `StreamInfo` supplies `SamplingFrequency` / `Channels`, those
    /// elements stay off-disk and the demuxer materialises the
    /// §5.1.4.1.29.1 default `8000.0` / §5.1.4.1.29.3 default `1`.
    ///
    /// Spec range checks enforced at queue time (§5.1.4.1.29):
    ///
    /// * `sampling_frequency` / `output_sampling_frequency` are ranged
    ///   `> 0x0p+0` — a `Some(v)` with `v <= 0.0` (or non-finite) is
    ///   rejected.
    /// * `channels` is ranged `not 0` — a `Some(0)` is rejected.
    /// * `bit_depth` is ranged `not 0` — a `Some(0)` is rejected.
    ///
    /// Errors:
    ///
    /// * `Error::other` — called after `write_header`.
    /// * `Error::invalid` — `stream_index` is out of range, the stream at
    ///   that index is not [`MediaType::Audio`] (only audio tracks carry
    ///   an `Audio` master per RFC 9559 §5.1.4.1.29 — mirroring the demux
    ///   side, which returns `None` from `track_audio` for non-audio
    ///   tracks), or any field violates its spec range.
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued value (last-write-wins). Returns a mutable
    /// reference back so calls can chain builder-style.
    pub fn set_track_audio(
        &mut self,
        stream_index: usize,
        audio: MkvTrackAudio,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_track_audio called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_track_audio stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        if self.streams[stream_index].params.media_type != MediaType::Audio {
            return Err(Error::invalid(format!(
                "MKV muxer: set_track_audio on stream {stream_index} ({}) — only Audio tracks carry an Audio master",
                self.streams[stream_index].params.codec_id.as_str()
            )));
        }
        // RFC 9559 §5.1.4.1.29.1 / .2: SamplingFrequency and
        // OutputSamplingFrequency are ranged "> 0x0p+0".
        for (name, freq) in [
            ("sampling_frequency", audio.sampling_frequency),
            ("output_sampling_frequency", audio.output_sampling_frequency),
        ] {
            if let Some(v) = freq {
                if !(v.is_finite() && v > 0.0) {
                    return Err(Error::invalid(format!(
                        "MKV muxer: set_track_audio {name} {v} out of range (must be finite and > 0)"
                    )));
                }
            }
        }
        // RFC 9559 §5.1.4.1.29.3 / .4: Channels and BitDepth are ranged
        // "not 0".
        if audio.channels == Some(0) {
            return Err(Error::invalid(
                "MKV muxer: set_track_audio channels 0 out of range (must be not 0)".to_string(),
            ));
        }
        if audio.bit_depth == Some(0) {
            return Err(Error::invalid(
                "MKV muxer: set_track_audio bit_depth 0 out of range (must be not 0)".to_string(),
            ));
        }
        self.track_audio[stream_index] = Some(audio);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream `Audio` master hint
    /// installed via [`MkvMuxer::set_track_audio`]. Returns `None` for any
    /// stream that didn't have the API called (the muxer derives the
    /// `Audio` master from `StreamInfo` alone). Mostly useful for tests.
    pub fn track_audio(&self, stream_index: usize) -> Option<MkvTrackAudio> {
        *self.track_audio.get(stream_index)?
    }

    /// Set the per-track timing elements (RFC 9559 §5.1.4.1.13..§5.1.4.1.15)
    /// for one stream — `DefaultDuration`, `DefaultDecodedFieldDuration`,
    /// and `TrackTimestampScale`. Must be called before
    /// [`Muxer::write_header`]; the elements live in the `TrackEntry`, which
    /// is written exactly once at header time.
    ///
    /// Per-field omission rule: each `Some(v)` writes the element explicitly,
    /// each `None` stays off-disk. There is **no track-type restriction** —
    /// the spec carries all three on every `TrackEntry` (though
    /// `DefaultDuration` and `DefaultDecodedFieldDuration` are mostly used on
    /// video tracks).
    ///
    /// Spec range checks enforced at queue time: `DefaultDuration` and
    /// `DefaultDecodedFieldDuration` are ranged `not 0` (a `Some(0)` is
    /// rejected); `TrackTimestampScale` is ranged `> 0x0p+0` (a non-finite
    /// or non-positive `Some(v)` is rejected).
    ///
    /// Errors:
    ///
    /// * `Error::other` — called after `write_header`.
    /// * `Error::invalid` — `stream_index` out of range, or a field
    ///   violates its spec range.
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued value (last-write-wins). Returns a mutable
    /// reference back so calls can chain builder-style. Pairs symmetrically
    /// with the demux-side [`crate::demux::MkvDemuxer::track_timing`].
    pub fn set_track_timing(
        &mut self,
        stream_index: usize,
        timing: MkvTrackTiming,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_track_timing called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_track_timing stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        // RFC 9559 §5.1.4.1.13 / .14: DefaultDuration and
        // DefaultDecodedFieldDuration are ranged "not 0".
        if timing.default_duration == Some(0) {
            return Err(Error::invalid(
                "MKV muxer: set_track_timing default_duration 0 out of range (must be not 0)"
                    .to_string(),
            ));
        }
        if timing.default_decoded_field_duration == Some(0) {
            return Err(Error::invalid(
                "MKV muxer: set_track_timing default_decoded_field_duration 0 out of range (must be not 0)"
                    .to_string(),
            ));
        }
        // RFC 9559 §5.1.4.1.15: TrackTimestampScale is ranged "> 0x0p+0".
        if let Some(v) = timing.track_timestamp_scale {
            if !(v.is_finite() && v > 0.0) {
                return Err(Error::invalid(format!(
                    "MKV muxer: set_track_timing track_timestamp_scale {v} out of range (must be finite and > 0)"
                )));
            }
        }
        self.track_timing[stream_index] = Some(timing);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream timing hint installed
    /// via [`MkvMuxer::set_track_timing`]. Returns `None` for any stream
    /// that didn't have the API called (all three elements stay off-disk).
    /// Mostly useful for tests.
    pub fn track_timing(&self, stream_index: usize) -> Option<MkvTrackTiming> {
        *self.track_timing.get(stream_index)?
    }

    /// Set the per-track codec-timing elements (RFC 9559 §5.1.4.1.25 /
    /// §5.1.4.1.26) for one stream — `CodecDelay` and `SeekPreRoll`. Must be
    /// called before [`Muxer::write_header`]; the elements live in the
    /// `TrackEntry`, which is written exactly once at header time.
    ///
    /// Per-field omission rule: each `Some(v)` writes the element explicitly
    /// (an explicit `0` is legal and round-trips distinctly from omission),
    /// each `None` stays off-disk. There is **no track-type restriction** —
    /// the spec carries both on every `TrackEntry`, though they are chiefly
    /// meaningful for audio codecs with a built-in encoder delay / pre-roll
    /// (Opus, AAC).
    ///
    /// Interaction with the Opus auto-derivation: for an Opus track the muxer
    /// auto-derives `CodecDelay` from the pre-skip and a recommended
    /// `SeekPreRoll` of 80 ms. An explicit hint installed here overrides
    /// either auto value **per field** — a `Some` field replaces the
    /// auto-derived value, a `None` field keeps it. On any non-Opus track a
    /// `None` field simply omits the element.
    ///
    /// Both values are ranged `>= 0` by their `uint` type; no additional
    /// spec range check applies, so this method only validates the muxer
    /// state and the stream index.
    ///
    /// Errors:
    ///
    /// * `Error::other` — called after `write_header`.
    /// * `Error::invalid` — `stream_index` out of range.
    ///
    /// Calling this twice on the same `stream_index` overwrites the
    /// previously queued value (last-write-wins). Returns a mutable
    /// reference back so calls can chain builder-style. Pairs symmetrically
    /// with the demux-side
    /// [`crate::demux::MkvDemuxer::track_codec_timing`].
    pub fn set_track_codec_timing(
        &mut self,
        stream_index: usize,
        timing: MkvTrackCodecTiming,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_track_codec_timing called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_track_codec_timing stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        self.track_codec_timing[stream_index] = Some(timing);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream codec-timing hint
    /// installed via [`MkvMuxer::set_track_codec_timing`]. Returns `None` for
    /// any stream that didn't have the API called. Mostly useful for tests.
    pub fn track_codec_timing(&self, stream_index: usize) -> Option<MkvTrackCodecTiming> {
        *self.track_codec_timing.get(stream_index)?
    }

    /// Set the per-track identity / selection elements (RFC 9559 §5.1.4.1.18 /
    /// .19 / .20 / .23 / .4 / .5 / .12 / .24) for one stream — `Name`,
    /// `CodecName`, the `Language` / `LanguageBCP47` pair, the three selection
    /// flags (`FlagEnabled` / `FlagDefault` / `FlagLacing`), and
    /// `AttachmentLink`. Must be called before [`Muxer::write_header`]; the
    /// elements live in the `TrackEntry`, which is written exactly once at
    /// header time.
    ///
    /// Per-field omission rule: each `Some(v)` writes the element explicitly,
    /// each `None` stays off-disk. There is **no track-type restriction** —
    /// the spec carries all eight on every `TrackEntry`. The `language` field,
    /// when `Some`, overrides the `StreamInfo`-derived `Language`; the
    /// `flag_lacing` field, when `Some`, overrides the muxer's auto-derived
    /// `FlagLacing`. Per §5.1.4.1.20, when both `language` and
    /// `language_bcp47` are `Some` the muxer writes only `LanguageBCP47`
    /// (`Language` MUST be ignored when BCP-47 is present).
    ///
    /// Spec checks enforced at queue time: `AttachmentLink` is ranged `not 0`
    /// (a `Some(0)` is rejected, §5.1.4.1.24); an empty `Name` / `CodecName` /
    /// `Language` / `LanguageBCP47` string is rejected (a zero-length element
    /// is meaningless and would round-trip as an empty value).
    ///
    /// Errors:
    ///
    /// * `Error::other` — called after `write_header`.
    /// * `Error::invalid` — `stream_index` out of range, a `Some("")` string,
    ///   or `attachment_link == Some(0)`.
    ///
    /// Calling this twice on the same `stream_index` overwrites the previously
    /// queued value (last-write-wins). Returns a mutable reference back so
    /// calls can chain builder-style. Pairs symmetrically with the demux-side
    /// [`crate::demux::MkvDemuxer::track_identity`].
    pub fn set_track_identity(
        &mut self,
        stream_index: usize,
        identity: MkvTrackIdentity,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_track_identity called after write_header",
            ));
        }
        if stream_index >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: set_track_identity stream_index {stream_index} out of range ({} streams)",
                self.streams.len()
            )));
        }
        for (field, val) in [
            ("name", &identity.name),
            ("codec_name", &identity.codec_name),
            ("language", &identity.language),
            ("language_bcp47", &identity.language_bcp47),
        ] {
            if val.as_deref() == Some("") {
                return Err(Error::invalid(format!(
                    "MKV muxer: set_track_identity {field} must not be an empty string"
                )));
            }
        }
        // RFC 9559 §5.1.4.1.24: AttachmentLink is ranged "not 0".
        if identity.attachment_link == Some(0) {
            return Err(Error::invalid(
                "MKV muxer: set_track_identity attachment_link 0 out of range (must be not 0)"
                    .to_string(),
            ));
        }
        self.track_identity[stream_index] = Some(identity);
        Ok(self)
    }

    /// Read-only accessor for the queued per-stream identity hint installed
    /// via [`MkvMuxer::set_track_identity`]. Returns `None` for any stream
    /// that didn't have the API called. Mostly useful for tests.
    pub fn track_identity(&self, stream_index: usize) -> Option<&MkvTrackIdentity> {
        self.track_identity.get(stream_index)?.as_ref()
    }

    /// Queue a chapter atom with one English-language `ChapterDisplay`
    /// carrying `title`. Must be called before [`MkvMuxer::write_header`];
    /// returns [`Error::other`] if the header has already been emitted.
    ///
    /// `end_time_ns == None` omits the `ChapterTimeEnd` element entirely.
    /// This matches how DVD-derived chapters are typically expressed:
    /// each program-chain cell has a start PTM but no explicit end
    /// (it's implicit from the next chapter's start, or end-of-program).
    /// The same shape works for Blu-ray MPLS `PlayListMark` entries —
    /// each mark carries a `mark_time_stamp` (90 kHz PTS) and the muxer
    /// just needs the nanosecond-converted start. Suggested converter:
    ///
    /// ```text
    /// // BD PTS is 90 kHz; ns is exact (no FP), no overflow up to ~5×10^15 ticks.
    /// fn bd_pts90k_to_ns(pts_90k: u64) -> u64 {
    ///     pts_90k * 100_000 / 9
    /// }
    /// ```
    ///
    /// Surface model: a `Chapters → EditionEntry → ChapterAtom →
    /// ChapterDisplay` master per RFC 9559 §5.1.7. The `ChapterTimeStart`
    /// / `ChapterTimeEnd` payload units are nanoseconds and are
    /// **independent of the segment's `TimecodeScale`** (the spec pins
    /// them to ns regardless), so what you pass here is what lands on
    /// disk. Use [`MkvMuxer::add_chapter_full`] for multilingual
    /// displays or explicit `ChapCountry` tagging.
    pub fn add_chapter(
        &mut self,
        start_time_ns: u64,
        end_time_ns: Option<u64>,
        title: impl Into<String>,
    ) -> Result<()> {
        self.add_chapter_full(MkvChapter {
            time_start_ns: start_time_ns,
            time_end_ns: end_time_ns,
            display: vec![ChapterDisplay {
                title: title.into(),
                language: "eng".into(),
                country: None,
                language_bcp47: None,
            }],
            ..Default::default()
        })
    }

    /// Queue a fully-specified [`MkvChapter`] (zero or more displays,
    /// each with its own language / country). Same call-ordering
    /// constraint as [`MkvMuxer::add_chapter`]: must happen before
    /// `write_header`.
    pub fn add_chapter_full(&mut self, chapter: MkvChapter) -> Result<()> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: add_chapter_full called after write_header",
            ));
        }
        if let Some(end) = chapter.time_end_ns {
            if end < chapter.time_start_ns {
                return Err(Error::invalid(format!(
                    "MKV muxer: chapter end_time_ns ({end}) < start_time_ns ({})",
                    chapter.time_start_ns
                )));
            }
        }
        // RFC 9559 §5.1.7.1.4.1: ChapterUID has range "not 0".
        if chapter.uid == Some(0) {
            return Err(Error::invalid(
                "MKV muxer: ChapterUID 0 is out of range — RFC 9559 §5.1.7.1.4.1 ranges it as \
                 \"not 0\"; pass None to auto-derive a non-zero UID",
            ));
        }
        // §5.1.7.1.4.7: ChapterSegmentEditionUID has range "not 0".
        if chapter.segment_edition_uid == Some(0) {
            return Err(Error::invalid(
                "MKV muxer: ChapterSegmentEditionUID 0 is out of range — RFC 9559 §5.1.7.1.4.7 \
                 ranges it as \"not 0\"",
            ));
        }
        // §5.1.7.1.4.6: ChapterSegmentUUID is a 16-byte SegmentUUID.
        if let Some(uuid) = &chapter.segment_uuid {
            if uuid.len() != 16 {
                return Err(Error::invalid(format!(
                    "MKV muxer: ChapterSegmentUUID must be 16 bytes (RFC 9559 §5.1.7.1.4.6), got {}",
                    uuid.len()
                )));
            }
        }
        self.chapters.push(chapter);
        Ok(())
    }

    /// Read-only view of the queued chapter list. Useful for tests and
    /// for upstream callers (e.g. DVD-to-MKV) that want to confirm the
    /// chapter table they handed to the muxer before sealing the header.
    pub fn chapters(&self) -> &[MkvChapter] {
        &self.chapters
    }

    /// Queue an attached file (RFC 9559 §5.1.6 `AttachedFile`). Must be
    /// called before [`Muxer::write_header`]; returns [`Error::other`]
    /// otherwise — the `Attachments` master is emitted up front so the
    /// demuxer's single-pass header walk catches it.
    ///
    /// Spec validation applied at queue time, not at write time, so the
    /// caller sees the error attached to the offending call:
    ///
    /// * `attachment.filename` must be non-empty (`FileName` is
    ///   `minOccurs / maxOccurs: 1 / 1` per §5.1.6.1.2 and has no
    ///   default — an empty value would write a zero-length string the
    ///   demuxer would surface as the empty string, breaking
    ///   `tag:attachment:N:<name>` scope lookups).
    /// * `attachment.mime_type` must be non-empty for the same reason
    ///   (§5.1.6.1.3, mandatory, no default; RFC 6838 media-type string).
    /// * `attachment.uid == Some(0)` is rejected per `range: not 0`
    ///   (§5.1.6.1.5). `None` triggers the muxer's auto-derivation from
    ///   the 1-based attachment index, which is always non-zero.
    ///
    /// The attachment is appended to the queue in arrival order; the
    /// 1-based index the demuxer surfaces (the `N` in
    /// `attachment:N:filename`) follows arrival order too.
    pub fn add_attachment(&mut self, attachment: MkvAttachment) -> Result<()> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: add_attachment called after write_header",
            ));
        }
        if attachment.filename.is_empty() {
            return Err(Error::invalid(
                "MKV muxer: attachment FileName is mandatory (RFC 9559 §5.1.6.1.2, minOccurs=1)",
            ));
        }
        if attachment.mime_type.is_empty() {
            return Err(Error::invalid(
                "MKV muxer: attachment FileMediaType is mandatory (RFC 9559 §5.1.6.1.3, minOccurs=1)",
            ));
        }
        if attachment.uid == Some(0) {
            return Err(Error::invalid(
                "MKV muxer: attachment FileUID range: not 0 (RFC 9559 §5.1.6.1.5)",
            ));
        }
        self.attachments.push(attachment);
        Ok(())
    }

    /// Read-only view of the queued attachment list. Mirrors
    /// [`MkvMuxer::chapters`] for tests / upstream callers that want to
    /// confirm the list before sealing the header.
    pub fn attachments(&self) -> &[MkvAttachment] {
        &self.attachments
    }

    /// Queue a metadata `Tag` (RFC 9559 §5.1.8.1). Must be called before
    /// [`Muxer::write_header`]; returns [`Error::other`] otherwise — the
    /// `Tags` master is emitted up front so the demuxer's single-pass
    /// header walk catches it.
    ///
    /// The file carries a single `Tags` master (RFC 9559 §5.1.8,
    /// `maxOccurs: 1`); every `add_tag` call appends one `Tag` child to
    /// it in arrival order. Spec validation applied at queue time so the
    /// caller sees the error attached to the offending call:
    ///
    /// * Each `Tag` MUST carry at least one `SimpleTag` (§5.1.8.1.2,
    ///   `minOccurs: 1`) — a tag with an empty `simple_tags` list is
    ///   rejected.
    /// * Every `SimpleTag` (at every nesting depth) MUST have a non-empty
    ///   `TagName` (§5.1.8.1.2.1, `minOccurs: 1`) — an empty name is
    ///   rejected.
    /// * `target_type_value == Some(0)` is rejected per `range: not 0`
    ///   (§5.1.8.1.1.1).
    ///
    /// Zero `TagTrackUID` / `TagEditionUID` / `TagChapterUID` /
    /// `TagAttachmentUID` entries are *not* rejected: a `0` UID means
    /// "all of that kind in the Segment" per §5.1.8.1.1.3..§5.1.8.1.1.6,
    /// which the muxer expresses by omitting the element, so a `0` in any
    /// list is silently dropped at write time rather than failing the
    /// call.
    pub fn add_tag(&mut self, tag: MkvTag) -> Result<()> {
        if self.header_written {
            return Err(Error::other("MKV muxer: add_tag called after write_header"));
        }
        if tag.simple_tags.is_empty() {
            return Err(Error::invalid(
                "MKV muxer: Tag requires at least one SimpleTag (RFC 9559 §5.1.8.1.2, minOccurs=1)",
            ));
        }
        if tag.targets.target_type_value == Some(0) {
            return Err(Error::invalid(
                "MKV muxer: TargetTypeValue range: not 0 (RFC 9559 §5.1.8.1.1.1)",
            ));
        }
        validate_simple_tags(&tag.simple_tags)?;
        self.tags.push(tag);
        Ok(())
    }

    /// Read-only view of the queued tag list. Mirrors
    /// [`MkvMuxer::attachments`] for tests / upstream callers that want
    /// to confirm the list before sealing the header.
    pub fn tags(&self) -> &[MkvTag] {
        &self.tags
    }

    /// Emit a `Tags` element *between Clusters* on a livestreaming muxer
    /// (RFC 9559 §23.2: "In the context of live radio or web TV, it is
    /// possible to \"tag\" the content while it is playing. The Tags
    /// element can be placed between Clusters each time it is necessary.
    /// In that case, the new Tags element MUST reset the previously
    /// encountered Tags elements and use the new values instead.").
    ///
    /// Requires [`MkvMuxer::with_live_streaming`] — the between-Clusters
    /// `Tags` placement is the §23.2 live-tagging mechanism, and the
    /// non-live layout keeps its single up-front `Tags` master
    /// (§5.1.8 `maxOccurs: 1` per Segment walk) — and a written header.
    /// Any in-flight lace is flushed and the open Cluster is ended (the
    /// unknown-size Cluster is terminated by the `Tags` element itself on
    /// read; the next packet opens a fresh Cluster). Each `Tag` gets the
    /// same queue-time validation as [`MkvMuxer::add_tag`]. The element
    /// carries a leading `CRC-32` like every Top-Level master this muxer
    /// writes. The in-tree demuxer applies the reset: its `tags()` /
    /// tag-derived metadata are replaced when the walk crosses this
    /// element.
    pub fn write_live_tags(&mut self, tags: &[MkvTag]) -> Result<()> {
        if !self.header_written {
            return Err(Error::other(
                "MKV muxer: write_live_tags called before write_header",
            ));
        }
        if !self.live_streaming {
            return Err(Error::other(
                "MKV muxer: write_live_tags requires with_live_streaming (RFC 9559 §23.2)",
            ));
        }
        for tag in tags {
            if tag.simple_tags.is_empty() {
                return Err(Error::invalid(
                    "MKV muxer: Tag requires at least one SimpleTag (RFC 9559 §5.1.8.1.2, minOccurs=1)",
                ));
            }
            if tag.targets.target_type_value == Some(0) {
                return Err(Error::invalid(
                    "MKV muxer: TargetTypeValue range: not 0 (RFC 9559 §5.1.8.1.1.1)",
                ));
            }
            validate_simple_tags(&tag.simple_tags)?;
        }
        // Flush in-flight laces — their frames belong to the Cluster the
        // Tags element is about to terminate.
        if self.lacing_mode != LacingMode::None {
            for i in 0..self.lace_pending.len() {
                if !self.lace_pending[i].frames.is_empty() {
                    self.flush_lace(i)?;
                }
            }
        }
        // The top-level Tags element ends the open unknown-size Cluster
        // on read; mirror that in the writer state so the next packet
        // starts a fresh Cluster instead of appending Blocks after a
        // sibling element.
        self.cluster_open = false;
        let bytes = build_tags_element(tags);
        self.output.write_all(&bytes)?;
        Ok(())
    }

    /// Queue the Linked-Segment `Info` metadata (RFC 9559 §5.1.2.1..§5.1.2.8 +
    /// Section 17) the muxer writes into the `Info` master, right after
    /// `WritingApp`. The mux-side mirror of the demux-side
    /// [`MkvDemuxer::segment_linking`](crate::demux::MkvDemuxer::segment_linking)
    /// accessor: feed it a [`SegmentLinking`] obtained from another file's
    /// demuxer (or one built by hand) and the same `SegmentUUID` / `PrevUUID`
    /// / `NextUUID` / `SegmentFamily` / `*Filename` / `ChapterTranslate`
    /// children round-trip onto disk unchanged.
    ///
    /// Must be called before [`MkvMuxer::write_header`].
    ///
    /// # Errors / validation (RFC 9559 §5.1.2)
    ///
    /// * The three 128-bit UID elements — `SegmentUUID` (§5.1.2.1),
    ///   `PrevUUID` (§5.1.2.3), `NextUUID` (§5.1.2.5) — and every
    ///   `SegmentFamily` (§5.1.2.7) carry a fixed `length: 16`. A
    ///   wrong-length value is rejected (the demuxer keeps off-length bytes
    ///   verbatim for inspection, but the muxer refuses to write a
    ///   spec-violating element).
    /// * `PrevUUID` / `NextUUID` MUST NOT equal `SegmentUUID`
    ///   (§5.1.2.3 / §5.1.2.5). A self-referential link is rejected.
    /// * If any `ChapterTranslate` (§5.1.2.8) is present, a `SegmentFamily`
    ///   is REQUIRED (§5.1.2.7 usage note); omitting it is rejected.
    /// * Each `ChapterTranslate` carries a mandatory non-empty
    ///   `ChapterTranslateID` (§5.1.2.8.1, `minOccurs: 1`); an empty id is
    ///   rejected.
    ///
    /// An all-default (empty) [`SegmentLinking`] is accepted and writes
    /// nothing — equivalent to never calling the setter.
    pub fn set_segment_linking(&mut self, linking: SegmentLinking) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_segment_linking called after write_header",
            ));
        }
        // RFC 9559 §5.1.2.1 / .3 / .5 / .7: the 128-bit UID elements are
        // `length: 16`. Reject any off-length UID rather than emit a
        // spec-violating element.
        for (name, uuid) in [
            ("SegmentUUID", &linking.segment_uuid),
            ("PrevUUID", &linking.prev_uuid),
            ("NextUUID", &linking.next_uuid),
        ] {
            if let Some(bytes) = uuid {
                if bytes.len() != 16 {
                    return Err(Error::invalid(format!(
                        "MKV muxer: {name} must be exactly 16 bytes (RFC 9559 §5.1.2, length: 16), got {}",
                        bytes.len()
                    )));
                }
            }
        }
        for fam in &linking.families {
            if fam.len() != 16 {
                return Err(Error::invalid(format!(
                    "MKV muxer: SegmentFamily must be exactly 16 bytes (RFC 9559 §5.1.2.7, length: 16), got {}",
                    fam.len()
                )));
            }
        }
        // RFC 9559 §5.1.2.3 / §5.1.2.5: "PrevUUID MUST NOT be equal to the
        // SegmentUUID" / "NextUUID MUST NOT be equal to the SegmentUUID".
        if let Some(seg) = &linking.segment_uuid {
            if linking.prev_uuid.as_deref() == Some(seg.as_slice()) {
                return Err(Error::invalid(
                    "MKV muxer: PrevUUID MUST NOT equal SegmentUUID (RFC 9559 §5.1.2.3)",
                ));
            }
            if linking.next_uuid.as_deref() == Some(seg.as_slice()) {
                return Err(Error::invalid(
                    "MKV muxer: NextUUID MUST NOT equal SegmentUUID (RFC 9559 §5.1.2.5)",
                ));
            }
        }
        // RFC 9559 §5.1.2.7 usage note: "If the Segment Info contains a
        // ChapterTranslate element, [SegmentFamily] is REQUIRED."
        if !linking.chapter_translates.is_empty() && linking.families.is_empty() {
            return Err(Error::invalid(
                "MKV muxer: SegmentFamily is REQUIRED when a ChapterTranslate is present (RFC 9559 §5.1.2.7)",
            ));
        }
        // RFC 9559 §5.1.2.8.1: ChapterTranslateID is `minOccurs: 1` — the
        // mapping is meaningless without the binary value identifying this
        // Segment in the chapter-codec data.
        for ct in &linking.chapter_translates {
            if ct.id.is_empty() {
                return Err(Error::invalid(
                    "MKV muxer: ChapterTranslate requires a non-empty ChapterTranslateID (RFC 9559 §5.1.2.8.1, minOccurs: 1)",
                ));
            }
        }
        self.segment_linking = Some(linking);
        Ok(self)
    }

    /// Read-only view of the queued Linked-Segment metadata, if any. Mirrors
    /// [`MkvMuxer::tags`] for tests / callers that want to confirm what was
    /// queued before sealing the header.
    pub fn segment_linking(&self) -> Option<&SegmentLinking> {
        self.segment_linking.as_ref()
    }

    /// Queue the Segment `Title` (RFC 9559 §5.1.2.12, id `0x7BA9`) — the
    /// general name of the Segment — for the `Info` master. Written between
    /// `TimestampScale` and `MuxingApp` in §5.1.2 element order. Surfaces on
    /// the demuxer's flat metadata view under the `"title"` key.
    ///
    /// Must be called before [`MkvMuxer::write_header`]. Last-write-wins;
    /// passing an empty string still emits an (empty) `Title` element, matching
    /// the spec's `maxOccurs: 1` with no length floor.
    pub fn set_title(&mut self, title: impl Into<String>) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_title called after write_header",
            ));
        }
        self.info_title = Some(title.into());
        Ok(self)
    }

    /// Queue the Segment `Duration` (RFC 9559 §5.1.2.10, id `0x4489`) — the
    /// total length of the Segment — for the `Info` master. Written between
    /// `TimestampScale` and `DateUTC` in §5.1.2 element order, inside the
    /// CRC-validated `Info` body.
    ///
    /// The muxer streams `Cluster`s with the unknown-size VINT and seals the
    /// `Info` master (with a leading `CRC-32`) at header time, so it never
    /// auto-derives `Duration` from the written packets — doing so would need
    /// a trailer patch that invalidates the `Info` CRC. A caller that knows
    /// the total length ahead of time supplies it here; the value round-trips
    /// through the demuxer's [`duration_micros`](oxideav_core::Demuxer::duration_micros).
    ///
    /// The muxer's `TimestampScale` is fixed at 1 ms, so the on-disk
    /// `Duration` float is the length in milliseconds; the argument is a
    /// [`std::time::Duration`] and the conversion is exact for millisecond
    /// multiples.
    ///
    /// Spec range enforced at queue time: §5.1.2.10 is ranged `> 0x0p+0`
    /// (strictly positive), so a zero or otherwise non-positive length is
    /// rejected. Must be called before [`MkvMuxer::write_header`];
    /// last-write-wins.
    ///
    /// Errors:
    ///
    /// * `Error::other` — called after `write_header`.
    /// * `Error::invalid` — the resulting length is not finite and `> 0`.
    pub fn set_duration(&mut self, duration: std::time::Duration) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_duration called after write_header",
            ));
        }
        let ticks = duration.as_secs_f64() * 1000.0;
        if !(ticks.is_finite() && ticks > 0.0) {
            return Err(Error::invalid(format!(
                "MKV muxer: set_duration {ticks} ms out of range (RFC 9559 §5.1.2.10 range > 0x0p+0)"
            )));
        }
        self.info_duration_ticks = Some(ticks);
        Ok(self)
    }

    /// Read-only accessor for the queued Segment `Duration`, in
    /// `TimestampScale` ticks (= milliseconds at the muxer's 1 ms scale).
    /// Returns `None` when [`MkvMuxer::set_duration`] was not called. Mostly
    /// useful for tests.
    pub fn duration_ticks(&self) -> Option<f64> {
        self.info_duration_ticks
    }

    /// Queue the EBML-header `DocTypeExtension` declarations (RFC 8794 §11.2.9)
    /// to write into this file's header. Each [`DocTypeExtension`] adds an
    /// extra (name, version) tuple to the main `DocType` + `DocTypeVersion`,
    /// declaring an experimental / out-of-band element set a reader MAY know.
    /// Emitted in slice order into the EBML header at `write_header` time,
    /// after `DocTypeReadVersion`.
    ///
    /// The setter takes the **same** demux-side [`DocTypeExtension`] record
    /// [`MkvDemuxer::ebml_header`] surfaces, so a header→header copy of a file
    /// declaring extensions round-trips them verbatim. Calling with an empty
    /// slice clears any previously-queued extensions; repeated calls are
    /// last-write-wins.
    ///
    /// Spec rules enforced at queue time (before any byte is written):
    ///
    /// * Rejects post-[`MkvMuxer::write_header`] use — the EBML header is
    ///   written exactly once.
    /// * An empty `DocTypeExtensionName` (§11.2.10, length `>0`) is rejected.
    /// * A duplicate `DocTypeExtensionName` is rejected (§11.2.10 "MUST be
    ///   unique within the EBML Header").
    /// * A zero `DocTypeExtensionVersion` (§11.2.11, range "not 0") is
    ///   rejected.
    ///
    /// Omitting the call entirely keeps the EBML header free of any
    /// `DocTypeExtension` — the common case, mirrored by the demuxer surfacing
    /// an empty `doc_type_extensions` list.
    pub fn set_doc_type_extensions(
        &mut self,
        extensions: Vec<DocTypeExtension>,
    ) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_doc_type_extensions called after write_header",
            ));
        }
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for ext in &extensions {
            if ext.name.is_empty() {
                return Err(Error::invalid(
                    "MKV muxer: DocTypeExtensionName MUST be non-empty (RFC 8794 §11.2.10, length >0)",
                ));
            }
            if ext.version == 0 {
                return Err(Error::invalid(
                    "MKV muxer: DocTypeExtensionVersion MUST NOT be 0 (RFC 8794 §11.2.11, range 'not 0')",
                ));
            }
            if !seen.insert(ext.name.as_str()) {
                return Err(Error::invalid(format!(
                    "MKV muxer: duplicate DocTypeExtensionName '{}' (RFC 8794 §11.2.10 MUST be unique)",
                    ext.name
                )));
            }
        }
        self.doc_type_extensions = extensions;
        Ok(self)
    }

    /// Read-only accessor for the queued `DocTypeExtension` declarations
    /// installed via [`MkvMuxer::set_doc_type_extensions`]. Empty when none
    /// were queued. Mostly useful for tests.
    pub fn doc_type_extensions(&self) -> &[DocTypeExtension] {
        &self.doc_type_extensions
    }

    /// Queue the Segment `DateUTC` (RFC 9559 §5.1.2.11, id `0x4461`) — the
    /// date and time the Segment was created — as **nanoseconds since the
    /// Matroska epoch** (2001-01-01T00:00:00 UTC), the on-disk representation
    /// of the `date` element type. Written into the `Info` master right after
    /// `Duration` in §5.1.2 element order. The demuxer decodes the same value
    /// back onto its flat metadata view under the `"date"` key (as an ISO-8601
    /// string).
    ///
    /// Use [`MkvMuxer::set_date_utc_unix_secs`] when you have a Unix timestamp
    /// instead. Must be called before [`MkvMuxer::write_header`].
    pub fn set_date_utc_ns(&mut self, ns_since_2001: i64) -> Result<&mut Self> {
        if self.header_written {
            return Err(Error::other(
                "MKV muxer: set_date_utc_ns called after write_header",
            ));
        }
        self.info_date_utc_ns = Some(ns_since_2001);
        Ok(self)
    }

    /// Convenience wrapper over [`MkvMuxer::set_date_utc_ns`] that accepts a
    /// Unix timestamp (seconds since 1970-01-01T00:00:00 UTC) and rebases it
    /// onto the Matroska epoch (2001-01-01) the `DateUTC` element uses.
    ///
    /// The Matroska epoch is `978_307_200` seconds after the Unix epoch, so a
    /// pre-2001 timestamp yields a negative `DateUTC` (the element is a signed
    /// `date`). Must be called before [`MkvMuxer::write_header`].
    pub fn set_date_utc_unix_secs(&mut self, unix_secs: i64) -> Result<&mut Self> {
        // RFC 9559 §5.1.2.11 `date` type: nanoseconds since 2001-01-01 UTC.
        const UNIX_2001: i64 = 978_307_200;
        let ns = (unix_secs - UNIX_2001).saturating_mul(1_000_000_000);
        self.set_date_utc_ns(ns)
    }

    /// Construct a plain Matroska muxer. Thin wrapper around the boxed
    /// [`open`] factory for callers that want a concrete type back (e.g. to
    /// introspect its state in tests).
    pub fn new_matroska(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Self> {
        Self::new(output, streams, DocType::Matroska)
    }

    /// Construct a WebM muxer. Validates codec whitelist up front; returns
    /// [`Error::Unsupported`] on the first stream whose codec WebM does not
    /// permit.
    pub fn new_webm(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Self> {
        Self::new(output, streams, DocType::Webm)
    }
}

impl Muxer for MkvMuxer {
    fn format_name(&self) -> &str {
        self.doc_type.as_str()
    }

    fn write_header(&mut self) -> Result<()> {
        if self.header_written {
            return Err(Error::other("MKV muxer: write_header called twice"));
        }
        // Anchor so segment_data_start is an absolute file offset even when
        // the output stream already has bytes before us.
        let base_pos = self.output.stream_position().unwrap_or(0);
        // EBML header element.
        let mut ebml_body = Vec::new();
        write_uint_element(&mut ebml_body, ids::EBML_VERSION, 1);
        write_uint_element(&mut ebml_body, ids::EBML_READ_VERSION, 1);
        write_uint_element(&mut ebml_body, ids::EBML_MAX_ID_LENGTH, 4);
        write_uint_element(&mut ebml_body, ids::EBML_MAX_SIZE_LENGTH, 8);
        write_string_element(&mut ebml_body, ids::EBML_DOC_TYPE, self.doc_type.as_str());
        // WebM pins DocTypeVersion to 4 / DocTypeReadVersion to 2 as of the
        // current spec. Matroska also sits at 4/2 for the features we emit.
        write_uint_element(&mut ebml_body, ids::EBML_DOC_TYPE_VERSION, 4);
        write_uint_element(&mut ebml_body, ids::EBML_DOC_TYPE_READ_VERSION, 2);
        // DocTypeExtension masters (RFC 8794 §11.2.9), if any were queued via
        // `set_doc_type_extensions`. Each carries a mandatory
        // `DocTypeExtensionName` (§11.2.10) + `DocTypeExtensionVersion`
        // (§11.2.11). Queue-time validation already rejected empty names /
        // zero versions / duplicate names, so we can emit verbatim here.
        for ext in &self.doc_type_extensions {
            let mut ext_body = Vec::new();
            write_string_element(&mut ext_body, ids::DOC_TYPE_EXTENSION_NAME, &ext.name);
            write_uint_element(&mut ext_body, ids::DOC_TYPE_EXTENSION_VERSION, ext.version);
            write_master_element(&mut ebml_body, ids::DOC_TYPE_EXTENSION, &ext_body);
        }
        let mut all = Vec::new();
        write_master_element(&mut all, ids::EBML_HEADER, &ebml_body);

        // Segment with unknown size.
        all.extend_from_slice(&write_element_id(ids::SEGMENT));
        all.extend_from_slice(&write_vint(VINT_UNKNOWN_SIZE, 0));
        // Record the file offset of the Segment payload start — Cues
        // cluster positions are stored as byte offsets from this point.
        let segment_data_start_in_buf = all.len() as u64;

        // SeekHead with five Seek entries (Info, Tracks, Chapters,
        // Attachments, Cues). Each Seek is written at a fixed width (SeekID
        // 4 bytes, SeekPosition 8 bytes) so we know exactly where to patch
        // in the real positions later. Info and Tracks SeekPositions are
        // filled in below before the buffer is flushed; Chapters and
        // Attachments are filled in immediately after the Tracks emit (or
        // voided if no chapters / attachments were queued); Cues stays as a
        // placeholder zero and gets patched in `write_trailer` (or rewritten
        // as a Void element if no Cues was actually emitted).
        // Livestreaming layout (RFC 9559 §25.3.4): "SeekHead and Cues are
        // useless" in a live stream, and §23.2 says a stream with neither
        // at its start SHOULD be considered non-seekable — exactly the
        // signal a live muxer wants to send. Skip the whole fixed-size
        // SeekHead when live streaming was requested; otherwise the
        // fixed layout is documented in `build_initial_seek_head`: each
        // Seek is exactly `SEEK_ENTRY_LEN` bytes and the SeekPosition
        // payload sits at `entry_start + SEEK_POS_PAYLOAD_OFFSET`, so the
        // patch section below can write the real offsets in place.
        let seek_head_start_in_buf = if self.live_streaming {
            None
        } else {
            let seek_head_bytes = build_initial_seek_head();
            let start = all.len();
            all.extend_from_slice(&seek_head_bytes);
            // Sanity: SeekHead occupies a known total size; the next
            // element starts immediately after.
            debug_assert_eq!(seek_head_bytes.len(), SEEK_HEAD_TOTAL_LEN);
            Some(start)
        };

        // Info element.
        let info_offset_in_buf = all.len() as u64 - segment_data_start_in_buf;
        let mut info_body = Vec::new();
        // Linked-Segment elements (RFC 9559 §5.1.2.1..§5.1.2.8) precede
        // TimestampScale in the §5.1.2 element order. Written only when the
        // caller queued them via `set_segment_linking`; absent on the common
        // standalone Segment.
        if let Some(linking) = &self.segment_linking {
            write_segment_linking(&mut info_body, linking);
        }
        write_uint_element(&mut info_body, ids::TIMECODE_SCALE, 1_000_000); // 1 ms
                                                                            // RFC 9559 §5.1.2 element order: Duration (.10), DateUTC (.11),
                                                                            // Title (.12), MuxingApp (.13), WritingApp (.14).
                                                                            // Duration is emitted only when the caller queued a known total
                                                                            // length via `set_duration`; a value in TimestampScale ticks
                                                                            // (= ms at the muxer's 1 ms scale), stored as a `float`.
        if let Some(ticks) = self.info_duration_ticks {
            write_float_element(&mut info_body, ids::DURATION, ticks);
        }
        if let Some(ns) = self.info_date_utc_ns {
            // §5.1.2.11 `date`: signed nanoseconds since the 2001-01-01 epoch.
            write_date_element(&mut info_body, ids::DATE_UTC, ns);
        }
        if let Some(title) = &self.info_title {
            write_string_element(&mut info_body, ids::TITLE, title);
        }
        write_string_element(&mut info_body, ids::MUXING_APP, "oxideav");
        write_string_element(&mut info_body, ids::WRITING_APP, "oxideav");
        write_master_element_with_crc(&mut all, ids::INFO, &info_body);

        // Tracks element.
        let tracks_offset_in_buf = all.len() as u64 - segment_data_start_in_buf;
        let mut tracks_body = Vec::new();
        for (i, s) in self.streams.iter().enumerate() {
            let track_number = self.track_numbers[i];
            let mut t = Vec::new();
            write_uint_element(&mut t, ids::TRACK_NUMBER, track_number);
            write_uint_element(&mut t, ids::TRACK_UID, track_number);
            let track_type = match s.params.media_type {
                MediaType::Audio => ids::TRACK_TYPE_AUDIO,
                MediaType::Video => ids::TRACK_TYPE_VIDEO,
                MediaType::Subtitle => ids::TRACK_TYPE_SUBTITLE,
                _ => 17, // treat as subtitle/data fallback
            };
            write_uint_element(&mut t, ids::TRACK_TYPE, track_type);
            // RFC 9559 §5.1.4.1.12: FlagLacing = 1 advertises that
            // this track MAY carry laced Blocks. We write 1
            // unconditionally once any lacing mode is opted in,
            // since the per-track choice of whether a given Block
            // ends up laced is made at write time based on packet
            // sizes / keyframe boundaries. With LacingMode::None the
            // muxer never laces, so FlagLacing stays at 0.
            // A `set_track_identity` hint with an explicit `flag_lacing`
            // overrides the auto-derived value (RFC 9559 §5.1.4.1.12).
            let identity = self.track_identity[i].as_ref();
            let flag_lacing = match identity.and_then(|id| id.flag_lacing) {
                Some(v) => v as u64,
                None if self.lacing_mode == LacingMode::None => 0,
                None => 1,
            };
            write_uint_element(&mut t, ids::FLAG_LACING, flag_lacing);
            // Identity selection flags (RFC 9559 §5.1.4.1.4 / .5) queued via
            // `set_track_identity`. Per-element omission rule: each `Some(v)`
            // is written explicitly as 0/1; each `None` stays off-disk (the
            // demuxer materialises the spec default `1`). `FlagLacing` is
            // handled above because the muxer auto-derives it from the lacing
            // mode when the hint leaves it `None`.
            if let Some(id) = identity {
                if let Some(v) = id.flag_enabled {
                    write_uint_element(&mut t, ids::FLAG_ENABLED, v as u64);
                }
                if let Some(v) = id.flag_default {
                    write_uint_element(&mut t, ids::FLAG_DEFAULT, v as u64);
                }
            }
            // Audience flags (RFC 9559 §5.1.4.1.6..§5.1.4.1.11) — six
            // TrackEntry-level uinteger elements queued via
            // `set_track_audience_flags`. Per-element omission rule:
            // every `Some(v)` slot is written explicitly as 0/1, every
            // `None` slot stays off-disk (the demuxer materialises the
            // §5.1.4.1.6 default `0` for FlagForced and surfaces `None`
            // for the five default-less minver-4 flags). Children land
            // in numerical-id order (0x55AA..0x55AF), matching the
            // order the demuxer's TrackEntry walker reports them.
            if let Some(af) = self.track_audience_flags[i] {
                if let Some(v) = af.forced {
                    write_uint_element(&mut t, ids::FLAG_FORCED, v as u64);
                }
                if let Some(v) = af.hearing_impaired {
                    write_uint_element(&mut t, ids::FLAG_HEARING_IMPAIRED, v as u64);
                }
                if let Some(v) = af.visual_impaired {
                    write_uint_element(&mut t, ids::FLAG_VISUAL_IMPAIRED, v as u64);
                }
                if let Some(v) = af.text_descriptions {
                    write_uint_element(&mut t, ids::FLAG_TEXT_DESCRIPTIONS, v as u64);
                }
                if let Some(v) = af.original {
                    write_uint_element(&mut t, ids::FLAG_ORIGINAL, v as u64);
                }
                if let Some(v) = af.commentary {
                    write_uint_element(&mut t, ids::FLAG_COMMENTARY, v as u64);
                }
            }
            // MaxBlockAdditionID (RFC 9559 §5.1.4.1.16) queued via
            // `set_max_block_addition_id`. Omission rule: `None` stays
            // off-disk (the demuxer materialises the spec default `0` =
            // "no BlockAdditions for this track"); `Some(v)` writes the
            // element explicitly, including the byte-distinct explicit
            // `0`. A non-zero declaration is what unlocks
            // `write_packet_with_additions` for the stream.
            if let Some(m) = self.max_block_addition_ids[i] {
                write_uint_element(&mut t, ids::MAX_BLOCK_ADDITION_ID, m);
            }
            // BlockAdditionMapping (RFC 9559 §5.1.4.1.17) masters queued via
            // `set_block_addition_mappings`, in slice order. Each declares
            // the shape of one non-default `BlockAddID` side channel; the
            // per-frame `BlockAdditional` bytes ride the Block path.
            for mapping in &self.block_addition_mappings[i] {
                write_block_addition_mapping(&mut t, mapping);
            }
            // Track timing (RFC 9559 §5.1.4.1.13..§5.1.4.1.15) queued via
            // `set_track_timing`. Per-field omission rule: each `Some(v)` is
            // written explicitly, each `None` stays off-disk (the demuxer
            // surfaces `None` for the two durations and materialises the
            // §5.1.4.1.15 `TrackTimestampScale` default `1.0`). All three sit
            // directly on `TrackEntry` (no gating master).
            if let Some(tm) = self.track_timing[i] {
                if let Some(v) = tm.default_duration {
                    write_uint_element(&mut t, ids::DEFAULT_DURATION, v);
                }
                if let Some(v) = tm.default_decoded_field_duration {
                    write_uint_element(&mut t, ids::DEFAULT_DECODED_FIELD_DURATION, v);
                }
                if let Some(v) = tm.track_timestamp_scale {
                    write_float_element(&mut t, ids::TRACK_TIMESTAMP_SCALE, v);
                }
            }
            // Identity strings (RFC 9559 §5.1.4.1.18 / .23) queued via
            // `set_track_identity`. `Name` and `CodecName` carry no spec
            // default — written only when the hint supplies them.
            if let Some(id) = identity {
                if let Some(name) = id.name.as_deref() {
                    write_string_element(&mut t, ids::NAME, name);
                }
                if let Some(cn) = id.codec_name.as_deref() {
                    write_string_element(&mut t, ids::CODEC_NAME, cn);
                }
            }
            // Language (RFC 9559 §5.1.4.1.19) / LanguageBCP47 (§5.1.4.1.20).
            // Precedence: a `set_track_identity` hint's `language_bcp47` wins
            // and, per the spec, suppresses any `Language` element ("any
            // Language elements ... MUST be ignored" when BCP-47 is present);
            // otherwise the hint's `language` overrides the
            // `StreamInfo`-derived `Language`, which is itself the fallback.
            // The Matroska-form spec default is `"eng"`, so the element is
            // omitted entirely when no source supplies a value.
            let hint_bcp47 = identity.and_then(|id| id.language_bcp47.as_deref());
            let hint_lang = identity.and_then(|id| id.language.as_deref());
            if let Some(bcp47) = hint_bcp47 {
                write_string_element(&mut t, ids::LANGUAGE_BCP47, bcp47);
            } else if let Some(lang) = hint_lang.or(s.params.language.as_deref()) {
                write_string_element(&mut t, ids::LANGUAGE, lang);
            }
            if let Some(name) = codec_id::to_matroska(&s.params.codec_id) {
                write_string_element(&mut t, ids::CODEC_ID, name);
            } else {
                // Fall back to a Matroska-style unknown id; players will reject
                // this but the file is otherwise valid.
                let raw = format!("X_{}", s.params.codec_id);
                write_string_element(&mut t, ids::CODEC_ID, &raw);
            }
            // CodecPrivate with codec-specific normalisation.
            let cp = encode_codec_private(&s.params.codec_id, &s.params.extradata);
            if !cp.is_empty() {
                write_bytes_element(&mut t, ids::CODEC_PRIVATE, &cp);
            }
            // AttachmentLink (RFC 9559 §5.1.4.1.24, `maxver: 3`) queued via
            // `set_track_identity` — the `FileUID` of an attachment this
            // codec uses. Range "not 0" enforced at queue time; written only
            // when the hint supplies it.
            if let Some(link) = identity.and_then(|id| id.attachment_link) {
                write_uint_element(&mut t, ids::ATTACHMENT_LINK, link);
            }
            // Codec-specific timing fields (RFC 9559 §5.1.4.1.25 /
            // §5.1.4.1.26). An Opus track auto-derives CodecDelay = pre_skip
            // in ns and a recommended SeekPreRoll of 80 ms; an explicit
            // `set_track_codec_timing` hint overrides either value per field.
            let (mut codec_delay, mut seek_pre_roll) = if s.params.codec_id.as_str() == "opus" {
                let pre_skip_samples = parse_opus_pre_skip(&s.params.extradata);
                let codec_delay_ns = pre_skip_samples as u64 * 1_000_000_000 / 48_000;
                (Some(codec_delay_ns), Some(80_000_000u64))
            } else {
                (None, None)
            };
            if let Some(hint) = self.track_codec_timing[i] {
                if let Some(v) = hint.codec_delay {
                    codec_delay = Some(v);
                }
                if let Some(v) = hint.seek_pre_roll {
                    seek_pre_roll = Some(v);
                }
            }
            if let Some(v) = codec_delay {
                write_uint_element(&mut t, ids::CODEC_DELAY, v);
            }
            if let Some(v) = seek_pre_roll {
                write_uint_element(&mut t, ids::SEEK_PRE_ROLL, v);
            }
            if s.params.media_type == MediaType::Audio {
                let mut audio = Vec::new();
                let hint = self.track_audio[i];
                // RFC 9559 §5.1.4.1.29: each child resolves from the
                // explicit `set_track_audio` hint first (a `Some` field
                // overrides) and falls back to the StreamInfo-derived
                // value. Children that end up unresolved are omitted so
                // the demuxer materialises the §5.1.4.1.29.1 / .3 spec
                // defaults (8000.0 Hz / 1 channel) or surfaces `None`
                // (BitDepth — §5.1.4.1.29.4 has no default).
                let sampling_frequency = hint
                    .and_then(|h| h.sampling_frequency)
                    .or_else(|| s.params.sample_rate.map(|sr| sr as f64));
                if let Some(sf) = sampling_frequency {
                    write_float_element(&mut audio, ids::SAMPLING_FREQUENCY, sf);
                }
                // OutputSamplingFrequency (§5.1.4.1.29.2): the SBR output
                // rate. StreamInfo has no equivalent, so this child only
                // appears when the hint supplied it. Omission lets the
                // demuxer apply the Table 19 derived default
                // (= SamplingFrequency).
                if let Some(osf) = hint.and_then(|h| h.output_sampling_frequency) {
                    write_float_element(&mut audio, ids::OUTPUT_SAMPLING_FREQUENCY, osf);
                }
                let channels = hint
                    .and_then(|h| h.channels)
                    .or_else(|| s.params.channels.map(|ch| ch as u64));
                if let Some(ch) = channels {
                    write_uint_element(&mut audio, ids::CHANNELS, ch);
                }
                let bit_depth = hint.and_then(|h| h.bit_depth).or_else(|| {
                    s.params
                        .sample_format
                        .map(|fmt| (fmt.bytes_per_sample() * 8) as u64)
                });
                if let Some(bd) = bit_depth {
                    write_uint_element(&mut audio, ids::BIT_DEPTH, bd);
                }
                // ChannelPositions (RFC 9559 Appendix A.27) — reclaimed Audio
                // child queued through `set_track_legacy`, written verbatim
                // for a faithful re-mux.
                if let Some(leg) = &self.track_legacy[i] {
                    if let Some(cp) = &leg.channel_positions {
                        write_bytes_element(&mut audio, ids::CHANNEL_POSITIONS, cp);
                    }
                }
                write_master_element(&mut t, ids::AUDIO, &audio);
            }
            if s.params.media_type == MediaType::Video {
                let mut video = Vec::new();
                // Per RFC 9559 §5.1.4.1.28 the on-disk child order is not
                // semantically meaningful, but writing fields in the same
                // order a demuxer typically encounters them (FlagInterlaced
                // / FieldOrder before PixelWidth/PixelHeight per the IANA
                // numerical-id ordering, then geometry, then the masters)
                // keeps the byte layout close to the conventional muxer output and
                // keeps diff-friendly fixtures small.
                if let Some(vi) = self.video_interlacings[i] {
                    // FlagInterlaced (§5.1.4.1.28.1) — only emitted when
                    // the caller explicitly opted in. Omitting it lets the
                    // demuxer materialise the default `0` (undetermined).
                    write_uint_element(&mut video, ids::FLAG_INTERLACED, vi.flag.to_raw());
                    // FieldOrder (§5.1.4.1.28.2) — written only when the
                    // caller paired it with FlagInterlaced::Interlaced.
                    // set_video_interlacing rejects FieldOrder paired with
                    // any other flag, so this branch is only taken on
                    // genuinely-interlaced tracks.
                    if let Some(fo) = vi.field_order {
                        write_uint_element(&mut video, ids::FIELD_ORDER, fo.to_raw());
                    }
                }
                // StereoMode (§5.1.4.1.28.3) — written only when the caller
                // explicitly opted in via `set_video_stereo_mode`. Omitting
                // it lets the demuxer materialise the spec default `0` mono.
                if let Some(sm) = self.video_stereo_modes[i] {
                    write_uint_element(&mut video, ids::STEREO_MODE, sm.to_raw());
                }
                // OldStereoMode (§5.1.4.1.28.5, id 0x53B9) — the legacy
                // libmatroska-bug element. Written ONLY when the caller
                // explicitly opted in via `set_video_old_stereo_mode`, which
                // exists solely so a faithful re-mux of a Matroska v2 /
                // libmatroska-bug file can round-trip the element the demuxer
                // surfaced. The spec marks it maxver 2 and says a Writer MUST
                // NOT use it for new files — never emitted unless the caller
                // copies it deliberately from a legacy source.
                if let Some(osm) = self.video_old_stereo_modes[i] {
                    write_uint_element(&mut video, ids::OLD_STEREO_MODE, osm.to_raw());
                }
                // AlphaMode (§5.1.4.1.28.4) — written only when the caller
                // explicitly opted in via `set_video_alpha_mode`. Omitting
                // it lets the demuxer materialise the spec default `0` none.
                if let Some(am) = self.video_alpha_modes[i] {
                    write_uint_element(&mut video, ids::ALPHA_MODE, am.to_raw());
                }
                if let Some(w) = s.params.width {
                    write_uint_element(&mut video, ids::PIXEL_WIDTH, w as u64);
                }
                if let Some(h) = s.params.height {
                    write_uint_element(&mut video, ids::PIXEL_HEIGHT, h as u64);
                }
                // PixelCrop{Top,Bottom,Left,Right} (§5.1.4.1.28.8..11) +
                // DisplayWidth (§.12) / DisplayHeight (§.13) / DisplayUnit
                // (§.14). Per-element omission rules per the docstring on
                // `set_video_geometry`: zero crops stay off-disk (spec
                // default `0`), `display_*` written when `Some`,
                // `DisplayUnit::Pixels` (the spec default) stays off-disk.
                if let Some(g) = self.video_geometries[i] {
                    if g.crop_top != 0 {
                        write_uint_element(&mut video, ids::PIXEL_CROP_TOP, g.crop_top);
                    }
                    if g.crop_bottom != 0 {
                        write_uint_element(&mut video, ids::PIXEL_CROP_BOTTOM, g.crop_bottom);
                    }
                    if g.crop_left != 0 {
                        write_uint_element(&mut video, ids::PIXEL_CROP_LEFT, g.crop_left);
                    }
                    if g.crop_right != 0 {
                        write_uint_element(&mut video, ids::PIXEL_CROP_RIGHT, g.crop_right);
                    }
                    if let Some(dw) = g.display_width {
                        write_uint_element(&mut video, ids::DISPLAY_WIDTH, dw);
                    }
                    if let Some(dh) = g.display_height {
                        write_uint_element(&mut video, ids::DISPLAY_HEIGHT, dh);
                    }
                    if g.display_unit != DisplayUnit::Pixels {
                        write_uint_element(&mut video, ids::DISPLAY_UNIT, g.display_unit.to_raw());
                    }
                }
                // AspectRatioType (RFC 9559 Appendix A.24, reclaimed,
                // id 0x54B3) — written only when the caller explicitly
                // opted in via `set_video_aspect_ratio_type`. Omitting it
                // keeps the element off-disk so the demuxer surfaces
                // `None` for the stream's `video_aspect_ratio_type` (the
                // reclaimed appendix defines no default — absence is not
                // materialised). Written as a plain `uinteger` verbatim.
                if let Some(art) = self.video_aspect_ratio_types[i] {
                    write_uint_element(&mut video, ids::ASPECT_RATIO_TYPE, art);
                }
                // UncompressedFourCC (§5.1.4.1.28.15) — written only when
                // the caller explicitly opted in via
                // `set_video_uncompressed_fourcc`. Omitting it keeps the
                // element off-disk so the demuxer surfaces `None` for the
                // stream's `video_uncompressed_fourcc` (the spec defines
                // no default — Table 11 only pins `minOccurs=1` for
                // `CodecID == "V_UNCOMPRESSED"`). The element is `binary`
                // with a fixed `length: 4`, so the 4-byte payload is
                // written verbatim.
                if let Some(fourcc) = self.video_uncompressed_fourccs[i] {
                    write_bytes_element(&mut video, ids::UNCOMPRESSED_FOURCC, &fourcc);
                }
                // Colour master (§5.1.4.1.28.16). Emitted only when the
                // caller explicitly opted in via `set_video_colour`. Each
                // scalar child is written only when its value differs
                // from the §5.1.4.1.28.17..§5.1.4.1.28.27 spec default so
                // an empty colour hint serialises as an empty Colour
                // master — the demuxer parses that as
                // `Some(VideoColour::default())` with every getter
                // returning the materialised spec default, which is the
                // round-trip semantics the demux side already
                // documents. Children are written in numerical-id order
                // (the order the demuxer also encounters them while
                // walking 0x55B1..0x55BD) so the layout is diff-friendly.
                // The `MasteringMetadata` sub-master
                // (§5.1.4.1.28.30..§5.1.4.1.28.40, id 0x55D0) is emitted
                // last when the queued hint carries
                // `mastering_metadata: Some(MkvMasteringMetadata)`; each
                // of its ten chromaticity / luminance children is
                // written only when its own Option is `Some(v)`,
                // mirroring the scalar-child omission rules above. An
                // explicit `Some(MkvMasteringMetadata::default())`
                // serialises as an empty MasteringMetadata master that
                // the demuxer parses into
                // `Some(MasteringMetadata::default())` — distinct from
                // the slot-omitted case which keeps the master off-disk
                // entirely so the demuxer surfaces `None`.
                if let Some(c) = self.video_colours[i] {
                    let mut colour = Vec::new();
                    if c.matrix_coefficients != MatrixCoefficients::Unspecified {
                        write_uint_element(
                            &mut colour,
                            ids::MATRIX_COEFFICIENTS,
                            c.matrix_coefficients.to_raw(),
                        );
                    }
                    if c.bits_per_channel != 0 {
                        write_uint_element(&mut colour, ids::BITS_PER_CHANNEL, c.bits_per_channel);
                    }
                    if let Some(v) = c.chroma_subsampling_horz {
                        write_uint_element(&mut colour, ids::CHROMA_SUBSAMPLING_HORZ, v);
                    }
                    if let Some(v) = c.chroma_subsampling_vert {
                        write_uint_element(&mut colour, ids::CHROMA_SUBSAMPLING_VERT, v);
                    }
                    if let Some(v) = c.cb_subsampling_horz {
                        write_uint_element(&mut colour, ids::CB_SUBSAMPLING_HORZ, v);
                    }
                    if let Some(v) = c.cb_subsampling_vert {
                        write_uint_element(&mut colour, ids::CB_SUBSAMPLING_VERT, v);
                    }
                    if c.chroma_siting_horz != ChromaSitingHorz::Unspecified {
                        write_uint_element(
                            &mut colour,
                            ids::CHROMA_SITING_HORZ,
                            c.chroma_siting_horz.to_raw(),
                        );
                    }
                    if c.chroma_siting_vert != ChromaSitingVert::Unspecified {
                        write_uint_element(
                            &mut colour,
                            ids::CHROMA_SITING_VERT,
                            c.chroma_siting_vert.to_raw(),
                        );
                    }
                    if c.range != ColourRange::Unspecified {
                        write_uint_element(&mut colour, ids::COLOUR_RANGE, c.range.to_raw());
                    }
                    if c.transfer_characteristics != TransferCharacteristics::Unspecified {
                        write_uint_element(
                            &mut colour,
                            ids::TRANSFER_CHARACTERISTICS,
                            c.transfer_characteristics.to_raw(),
                        );
                    }
                    if c.primaries != Primaries::Unspecified {
                        write_uint_element(&mut colour, ids::PRIMARIES, c.primaries.to_raw());
                    }
                    if let Some(v) = c.max_cll {
                        write_uint_element(&mut colour, ids::MAX_CLL, v);
                    }
                    if let Some(v) = c.max_fall {
                        write_uint_element(&mut colour, ids::MAX_FALL, v);
                    }
                    if let Some(mm) = c.mastering_metadata {
                        // MasteringMetadata sub-master (RFC 9559
                        // §5.1.4.1.28.30, id 0x55D0). Children emitted
                        // in numerical-id order (0x55D1..0x55DA) so the
                        // on-disk layout matches the order the demuxer
                        // walks them.
                        let mut mast = Vec::new();
                        if let Some(v) = mm.primary_r_chromaticity_x {
                            write_float_element(&mut mast, ids::PRIMARY_R_CHROMATICITY_X, v);
                        }
                        if let Some(v) = mm.primary_r_chromaticity_y {
                            write_float_element(&mut mast, ids::PRIMARY_R_CHROMATICITY_Y, v);
                        }
                        if let Some(v) = mm.primary_g_chromaticity_x {
                            write_float_element(&mut mast, ids::PRIMARY_G_CHROMATICITY_X, v);
                        }
                        if let Some(v) = mm.primary_g_chromaticity_y {
                            write_float_element(&mut mast, ids::PRIMARY_G_CHROMATICITY_Y, v);
                        }
                        if let Some(v) = mm.primary_b_chromaticity_x {
                            write_float_element(&mut mast, ids::PRIMARY_B_CHROMATICITY_X, v);
                        }
                        if let Some(v) = mm.primary_b_chromaticity_y {
                            write_float_element(&mut mast, ids::PRIMARY_B_CHROMATICITY_Y, v);
                        }
                        if let Some(v) = mm.white_point_chromaticity_x {
                            write_float_element(&mut mast, ids::WHITE_POINT_CHROMATICITY_X, v);
                        }
                        if let Some(v) = mm.white_point_chromaticity_y {
                            write_float_element(&mut mast, ids::WHITE_POINT_CHROMATICITY_Y, v);
                        }
                        if let Some(v) = mm.luminance_max {
                            write_float_element(&mut mast, ids::LUMINANCE_MAX, v);
                        }
                        if let Some(v) = mm.luminance_min {
                            write_float_element(&mut mast, ids::LUMINANCE_MIN, v);
                        }
                        write_master_element(&mut colour, ids::MASTERING_METADATA, &mast);
                    }
                    write_master_element(&mut video, ids::COLOUR, &colour);
                }
                // Projection master (§5.1.4.1.28.41). Emitted only when the
                // caller explicitly opted in via `set_video_projection`.
                // Children are written in numerical-id order (0x7671..0x7675,
                // the order the demuxer also encounters them) so the on-disk
                // layout is diff-friendly. Per-element omission rules per the
                // `set_video_projection` docstring: `ProjectionType` is
                // written only for non-rectangular types (the §5.1.4.1.28.42
                // default `0` stays off-disk), each pose component only when
                // non-zero (the §5.1.4.1.28.44..46 default `0.0` stays
                // off-disk), and `ProjectionPrivate` only when `Some(_)`. An
                // explicit `MkvProjection::default()` therefore serialises as
                // an empty `Projection` master that the demuxer parses into
                // `Some(Projection::default())` — distinct from the
                // call-omitted case which keeps the master off-disk so the
                // demuxer surfaces `None`.
                if let Some(p) = &self.video_projections[i] {
                    let mut proj = Vec::new();
                    if p.projection_type != ProjectionType::Rectangular {
                        write_uint_element(
                            &mut proj,
                            ids::PROJECTION_TYPE,
                            p.projection_type.to_raw(),
                        );
                    }
                    if let Some(private) = &p.private {
                        write_bytes_element(&mut proj, ids::PROJECTION_PRIVATE, private);
                    }
                    if p.pose_yaw != 0.0 {
                        write_float_element(&mut proj, ids::PROJECTION_POSE_YAW, p.pose_yaw);
                    }
                    if p.pose_pitch != 0.0 {
                        write_float_element(&mut proj, ids::PROJECTION_POSE_PITCH, p.pose_pitch);
                    }
                    if p.pose_roll != 0.0 {
                        write_float_element(&mut proj, ids::PROJECTION_POSE_ROLL, p.pose_roll);
                    }
                    write_master_element(&mut video, ids::PROJECTION, &proj);
                }
                // GammaValue / FrameRate (RFC 9559 Appendix A.25 / A.26) —
                // reclaimed Video children queued through `set_track_legacy`,
                // written verbatim for a faithful re-mux. FrameRate is
                // informational; the container never uses it for timing.
                if let Some(leg) = &self.track_legacy[i] {
                    if let Some(g) = leg.gamma_value {
                        write_float_element(&mut video, ids::GAMMA_VALUE, g);
                    }
                    if let Some(fr) = leg.frame_rate {
                        write_float_element(&mut video, ids::FRAME_RATE, fr);
                    }
                }
                write_master_element(&mut t, ids::VIDEO, &video);
            }
            // TrackOperation (RFC 9559 §5.1.4.1.30) — a TrackEntry-level
            // master (sibling to Video / Audio) describing the virtual
            // track recipe queued via `set_track_operation`. Emitted only
            // when the caller opted in; an omitted hint keeps the master
            // off-disk so the demuxer surfaces `None` from
            // `track_operation`. The setter has already validated that the
            // operation is non-empty and that every plane / join
            // stream_index is in range, so the resolution below cannot
            // produce a dangling or zero TrackUID.
            if let Some(op) = &self.track_operations[i] {
                let mut op_body = Vec::new();
                // TrackCombinePlanes (§5.1.4.1.30.1): one TrackPlane master
                // per plane, each carrying the mandatory TrackPlaneUID
                // (§5.1.4.1.30.3 — the source track's TrackUID, "not 0")
                // and TrackPlaneType (§5.1.4.1.30.4).
                if !op.combine_planes.is_empty() {
                    let mut combine_body = Vec::new();
                    for p in &op.combine_planes {
                        let mut plane = Vec::new();
                        write_uint_element(
                            &mut plane,
                            ids::TRACK_PLANE_UID,
                            self.track_numbers[p.stream_index],
                        );
                        write_uint_element(
                            &mut plane,
                            ids::TRACK_PLANE_TYPE,
                            p.plane_type.to_raw(),
                        );
                        write_master_element(&mut combine_body, ids::TRACK_PLANE, &plane);
                    }
                    write_master_element(&mut op_body, ids::TRACK_COMBINE_PLANES, &combine_body);
                }
                // TrackJoinBlocks (§5.1.4.1.30.5): one TrackJoinUID
                // (§5.1.4.1.30.6 — source track's TrackUID, "not 0") per
                // joined track.
                if !op.join_tracks.is_empty() {
                    let mut join_body = Vec::new();
                    for &j in &op.join_tracks {
                        write_uint_element(
                            &mut join_body,
                            ids::TRACK_JOIN_UID,
                            self.track_numbers[j],
                        );
                    }
                    write_master_element(&mut op_body, ids::TRACK_JOIN_BLOCKS, &join_body);
                }
                write_master_element(&mut t, ids::TRACK_OPERATION, &op_body);
            }
            // ContentEncodings (RFC 9559 §5.1.4.1.31) — a TrackEntry-level
            // master describing the compression / encryption chain queued
            // via `set_track_content_encodings`. Emitted only when the
            // caller opted in; an omitted hint keeps the master off-disk so
            // the demuxer surfaces `None` from `content_encodings`. The
            // setter has already validated order uniqueness, non-zero
            // scope, and the AES-only / non-zero cipher-mode rules.
            if let Some(enc) = &self.content_encodings[i] {
                let mut encs_body = Vec::new();
                // Write each ContentEncoding sorted by ascending
                // ContentEncodingOrder (lowest first). The demuxer re-sorts
                // into descending decode order on read, so on-disk order is
                // not load-bearing; ascending is the conventional write
                // order (§5.1.4.1.31.2 "highest first" is a *decode* rule).
                let mut chain: Vec<&crate::demux::ContentEncoding> = enc.encodings.iter().collect();
                chain.sort_by_key(|e| e.order);
                for e in chain {
                    let mut ce = Vec::new();
                    write_uint_element(&mut ce, ids::CONTENT_ENCODING_ORDER, e.order);
                    write_uint_element(&mut ce, ids::CONTENT_ENCODING_SCOPE, e.scope.0);
                    match &e.transform {
                        ContentEncodingTransform::Compression { algo, settings } => {
                            write_uint_element(
                                &mut ce,
                                ids::CONTENT_ENCODING_TYPE,
                                ids::CONTENT_ENCODING_TYPE_COMPRESSION,
                            );
                            let mut comp = Vec::new();
                            write_uint_element(&mut comp, ids::CONTENT_COMP_ALGO, algo.to_raw());
                            if !settings.is_empty() {
                                write_bytes_element(
                                    &mut comp,
                                    ids::CONTENT_COMP_SETTINGS,
                                    settings,
                                );
                            }
                            write_master_element(&mut ce, ids::CONTENT_COMPRESSION, &comp);
                        }
                        ContentEncodingTransform::Encryption {
                            algo,
                            key_id,
                            aes_cipher_mode,
                            signing,
                        } => {
                            write_uint_element(
                                &mut ce,
                                ids::CONTENT_ENCODING_TYPE,
                                ids::CONTENT_ENCODING_TYPE_ENCRYPTION,
                            );
                            let mut crypt = Vec::new();
                            write_uint_element(&mut crypt, ids::CONTENT_ENC_ALGO, algo.to_raw());
                            if !key_id.is_empty() {
                                write_bytes_element(&mut crypt, ids::CONTENT_ENC_KEY_ID, key_id);
                            }
                            if let Some(mode) = aes_cipher_mode {
                                let mut aes = Vec::new();
                                write_uint_element(
                                    &mut aes,
                                    ids::AES_SETTINGS_CIPHER_MODE,
                                    mode.to_raw(),
                                );
                                write_master_element(
                                    &mut crypt,
                                    ids::CONTENT_ENC_AES_SETTINGS,
                                    &aes,
                                );
                            }
                            // Reclaimed content-signing quartet (RFC 9559
                            // Appendix A.33..A.36) — each child written only
                            // when its `Option` slot is `Some`, so an empty
                            // `ContentSigning` adds no bytes and round-trips
                            // to `None` on every field.
                            if let Some(sig) = &signing.signature {
                                write_bytes_element(&mut crypt, ids::CONTENT_SIGNATURE, sig);
                            }
                            if let Some(sig_key) = &signing.key_id {
                                write_bytes_element(&mut crypt, ids::CONTENT_SIG_KEY_ID, sig_key);
                            }
                            if let Some(sig_algo) = signing.algo {
                                write_uint_element(&mut crypt, ids::CONTENT_SIG_ALGO, sig_algo);
                            }
                            if let Some(sig_hash) = signing.hash_algo {
                                write_uint_element(
                                    &mut crypt,
                                    ids::CONTENT_SIG_HASH_ALGO,
                                    sig_hash,
                                );
                            }
                            write_master_element(&mut ce, ids::CONTENT_ENCRYPTION, &crypt);
                        }
                    }
                    write_master_element(&mut encs_body, ids::CONTENT_ENCODING, &ce);
                }
                write_master_element(&mut t, ids::CONTENT_ENCODINGS, &encs_body);
            }
            // TrackTranslate (RFC 9559 §5.1.4.1.27) — zero or more
            // chapter-codec track-mapping masters queued via
            // `set_track_translates`. Each carries the mandatory
            // `TrackTranslateTrackID` (binary, verbatim) and
            // `TrackTranslateCodec` (uinteger), plus zero or more optional
            // `TrackTranslateEditionUID`s. The setter has already validated
            // that no `track_id` is empty and no edition UID is zero. Emitted
            // only when the caller opted in; an empty list keeps every master
            // off-disk so the demuxer surfaces an empty `track_translates`
            // slice.
            for tt in &self.track_translates[i] {
                let mut tt_body = Vec::new();
                write_bytes_element(&mut tt_body, ids::TRACK_TRANSLATE_TRACK_ID, &tt.track_id);
                write_uint_element(&mut tt_body, ids::TRACK_TRANSLATE_CODEC, tt.codec);
                for &uid in &tt.edition_uids {
                    write_uint_element(&mut tt_body, ids::TRACK_TRANSLATE_EDITION_UID, uid);
                }
                write_master_element(&mut t, ids::TRACK_TRANSLATE, &tt_body);
            }
            // Reclaimed Appendix-A legacy elements (RFC 9559 Appendix
            // A.19..A.23 + A.28..A.32) queued via `set_track_legacy`. Only the
            // populated fields are written; the setter has already validated
            // that the two SegmentUID binaries are 16 bytes. The order follows
            // the registry id ordering for determinism; no element carries a
            // spec default, so an explicit `Some(0)` is emitted as a real `0`.
            if let Some(leg) = &self.track_legacy[i] {
                if let Some(s) = &leg.codec_settings {
                    write_string_element(&mut t, ids::CODEC_SETTINGS, s);
                }
                for url in &leg.codec_info_urls {
                    write_string_element(&mut t, ids::CODEC_INFO_URL, url);
                }
                for url in &leg.codec_download_urls {
                    write_string_element(&mut t, ids::CODEC_DOWNLOAD_URL, url);
                }
                if let Some(v) = leg.decode_all {
                    write_uint_element(&mut t, ids::CODEC_DECODE_ALL, v);
                }
                if let Some(v) = leg.min_cache {
                    write_uint_element(&mut t, ids::MIN_CACHE, v);
                }
                if let Some(v) = leg.max_cache {
                    write_uint_element(&mut t, ids::MAX_CACHE, v);
                }
                if let Some(v) = leg.track_offset {
                    write_int_element(&mut t, ids::TRACK_OFFSET, v);
                }
                for &n in &leg.track_overlays {
                    write_uint_element(&mut t, ids::TRACK_OVERLAY, n);
                }
                if let Some(v) = leg.trick_track_uid {
                    write_uint_element(&mut t, ids::TRICK_TRACK_UID, v);
                }
                if let Some(b) = &leg.trick_track_segment_uid {
                    write_bytes_element(&mut t, ids::TRICK_TRACK_SEGMENT_UID, b);
                }
                if let Some(v) = leg.trick_track_flag {
                    write_uint_element(&mut t, ids::TRICK_TRACK_FLAG, v);
                }
                if let Some(v) = leg.trick_master_track_uid {
                    write_uint_element(&mut t, ids::TRICK_MASTER_TRACK_UID, v);
                }
                if let Some(b) = &leg.trick_master_track_segment_uid {
                    write_bytes_element(&mut t, ids::TRICK_MASTER_TRACK_SEGMENT_UID, b);
                }
            }
            write_master_element(&mut tracks_body, ids::TRACK_ENTRY, &t);
        }
        write_master_element_with_crc(&mut all, ids::TRACKS, &tracks_body);

        // Chapters (optional). If `add_chapter` calls were made before
        // `write_header`, materialise them now as a single EditionEntry
        // master sandwiched between Tracks and the first Cluster. RFC
        // 9559 §5.1.7 lets Chapters appear anywhere in the Segment, but
        // putting it here keeps the demuxer's pre-Cluster header walk
        // single-pass and matches the conventional single-pass ordering.
        // If no chapters were queued, the SeekHead Chapters slot stays
        // at its placeholder zero and gets voided below.
        let chapters_offset_opt: Option<u64> = if self.chapters.is_empty() {
            None
        } else {
            let chapters_offset_in_buf = all.len() as u64 - segment_data_start_in_buf;
            let chapters_bytes = build_chapters_element(&self.chapters);
            all.extend_from_slice(&chapters_bytes);
            Some(chapters_offset_in_buf)
        };

        // Attachments (optional). Same shape as Chapters: emit the
        // `Attachments` master sandwiched between Chapters and the first
        // Cluster when `add_attachment` was called before `write_header`.
        // RFC 9559 §5.1.6 lets the master appear anywhere in the Segment;
        // sitting it here keeps the demuxer's pre-Cluster header walk
        // single-pass. If no attachments were queued, the SeekHead
        // Attachments slot stays at its placeholder zero and gets voided
        // below.
        let attachments_offset_opt: Option<u64> = if self.attachments.is_empty() {
            None
        } else {
            let attachments_offset_in_buf = all.len() as u64 - segment_data_start_in_buf;
            let attachments_bytes = build_attachments_element(&self.attachments);
            all.extend_from_slice(&attachments_bytes);
            Some(attachments_offset_in_buf)
        };

        // Tags (optional). Same shape as Chapters / Attachments: emit the
        // single `Tags` master sandwiched after Attachments and before the
        // first Cluster when `add_tag` was called before `write_header`.
        // RFC 9559 §5.1.8 lets the master appear anywhere in the Segment;
        // sitting it here keeps the demuxer's pre-Cluster header walk
        // single-pass. If no tags were queued, the SeekHead Tags slot
        // stays at its placeholder zero and gets voided below.
        let tags_offset_opt: Option<u64> = if self.tags.is_empty() {
            None
        } else {
            let tags_offset_in_buf = all.len() as u64 - segment_data_start_in_buf;
            let tags_bytes = build_tags_element(&self.tags);
            all.extend_from_slice(&tags_bytes);
            Some(tags_offset_in_buf)
        };

        // Patch the Info / Tracks SeekPositions in the SeekHead now that we
        // know where each element landed inside `all`. Cues stays as zero
        // and is patched in `write_trailer`. In the live-streaming layout
        // there is no SeekHead at all (§25.3.4) — nothing to patch.
        if let Some(seek_head_start) = seek_head_start_in_buf {
            let info_seek_entry_in_buf = seek_head_start + SEEK_HEAD_HEADER_LEN;
            let tracks_seek_entry_in_buf = info_seek_entry_in_buf + SEEK_ENTRY_LEN;
            let chapters_seek_entry_in_buf = tracks_seek_entry_in_buf + SEEK_ENTRY_LEN;
            let attachments_seek_entry_in_buf = chapters_seek_entry_in_buf + SEEK_ENTRY_LEN;
            let tags_seek_entry_in_buf = attachments_seek_entry_in_buf + SEEK_ENTRY_LEN;
            let cues_seek_entry_in_buf = tags_seek_entry_in_buf + SEEK_ENTRY_LEN;
            write_u64_be_at(
                &mut all,
                info_seek_entry_in_buf + SEEK_POS_PAYLOAD_OFFSET,
                info_offset_in_buf,
            );
            write_u64_be_at(
                &mut all,
                tracks_seek_entry_in_buf + SEEK_POS_PAYLOAD_OFFSET,
                tracks_offset_in_buf,
            );
            match chapters_offset_opt {
                Some(off) => write_u64_be_at(
                    &mut all,
                    chapters_seek_entry_in_buf + SEEK_POS_PAYLOAD_OFFSET,
                    off,
                ),
                None => {
                    // No Chapters element emitted — rewrite the 21-byte slot
                    // as a Void so SeekHead walkers don't chase a placeholder
                    // zero that resolves to the SeekHead itself.
                    let void = void_seek_entry();
                    all[chapters_seek_entry_in_buf..chapters_seek_entry_in_buf + SEEK_ENTRY_LEN]
                        .copy_from_slice(&void);
                }
            }
            match attachments_offset_opt {
                Some(off) => write_u64_be_at(
                    &mut all,
                    attachments_seek_entry_in_buf + SEEK_POS_PAYLOAD_OFFSET,
                    off,
                ),
                None => {
                    // No Attachments element emitted — same void treatment as
                    // the Chapters slot above.
                    let void = void_seek_entry();
                    all[attachments_seek_entry_in_buf
                        ..attachments_seek_entry_in_buf + SEEK_ENTRY_LEN]
                        .copy_from_slice(&void);
                }
            }
            match tags_offset_opt {
                Some(off) => write_u64_be_at(
                    &mut all,
                    tags_seek_entry_in_buf + SEEK_POS_PAYLOAD_OFFSET,
                    off,
                ),
                None => {
                    // No Tags element emitted — same void treatment as the
                    // Chapters / Attachments slots above.
                    let void = void_seek_entry();
                    all[tags_seek_entry_in_buf..tags_seek_entry_in_buf + SEEK_ENTRY_LEN]
                        .copy_from_slice(&void);
                }
            }
            // Absolute file offset of the Cues Seek entry — used in
            // write_trailer to patch in the real Cues offset (or rewrite
            // the 21-byte slot as a Void element when no Cues was emitted).
            self.seek_cues_entry_offset = base_pos + cues_seek_entry_in_buf as u64;
            self.seek_head_written = true;
        }

        self.segment_data_start = base_pos + segment_data_start_in_buf;
        self.output.write_all(&all)?;
        self.header_written = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.write_packet_inner(packet, None)
    }

    fn write_trailer(&mut self) -> Result<()> {
        if self.trailer_written {
            return Ok(());
        }
        // Flush any in-flight lace buffers before the last Cluster is
        // sealed by the Cues element — otherwise the buffered frames
        // would be silently dropped.
        if self.lacing_mode != LacingMode::None {
            for i in 0..self.lace_pending.len() {
                if !self.lace_pending[i].frames.is_empty() {
                    self.flush_lace(i)?;
                }
            }
        }
        // Emit a Cues element after the last Cluster. The prior clusters are
        // left with unknown size (their EBML parser stops when it meets the
        // top-level Cues element id, which is outside the cluster subtree).
        // The live-streaming layout (§25.3.4) writes no Cues at all — a
        // live stream has no known end to index.
        let cues_offset_rel = if self.live_streaming {
            None
        } else {
            self.write_cues()?
        };
        // Patch the Cues entry in the SeekHead. If we did emit Cues, write
        // its offset (relative to the Segment payload start). If not, replace
        // the 21-byte Seek slot with a Void so the SeekHead stays self-
        // consistent — players that pre-walk the SeekHead would otherwise
        // chase a placeholder zero offset that points at the SeekHead itself.
        if self.seek_head_written {
            self.patch_cues_seek_entry(cues_offset_rel)?;
        }
        self.output.flush()?;
        self.trailer_written = true;
        Ok(())
    }
}

impl MkvMuxer {
    /// Shared body of [`Muxer::write_packet`] and
    /// [`MkvMuxer::write_packet_with_additions`]. When `additions` is
    /// `Some`, the packet is emitted as a `BlockGroup` (RFC 9559
    /// §5.1.3.5) carrying `Block` + `BlockAdditions` (+ `BlockDuration`
    /// when the packet has a duration, + `ReferenceBlock` when it is not
    /// a keyframe) instead of the usual `SimpleBlock`; the lacing buffer
    /// is bypassed for that packet (any pending same-track lace is
    /// flushed first so Block order is preserved). Validation of the
    /// additions themselves happens in `write_packet_with_additions`
    /// before this is called.
    fn write_packet_inner(
        &mut self,
        packet: &Packet,
        group: Option<&BlockGroupOptions>,
    ) -> Result<()> {
        if !self.header_written {
            return Err(Error::other("MKV muxer: write_header not called"));
        }
        let stream_idx = packet.stream_index as usize;
        if stream_idx >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: unknown stream index {}",
                stream_idx
            )));
        }
        let track_number = self.track_numbers[stream_idx];
        let stream_time_base = self.streams[stream_idx].time_base;
        let media_type = self.streams[stream_idx].params.media_type;
        let codec = self.streams[stream_idx].params.codec_id.as_str().to_owned();

        // Effective per-packet pts. If the source set one, use it; otherwise
        // derive from accumulated stream_pts and codec-specific durations.
        let derived_duration: Option<i64> = match codec.as_str() {
            "opus" => opus_packet_duration_samples(&packet.data).map(|s| s as i64),
            _ => packet.duration,
        };
        let effective_pts = match packet.pts {
            Some(v) => v,
            None => self.stream_pts[stream_idx],
        };
        // Advance the running counter for the next packet without an explicit pts.
        if let Some(d) = derived_duration {
            self.stream_pts[stream_idx] = effective_pts + d;
        } else if packet.pts.is_some() {
            self.stream_pts[stream_idx] = effective_pts;
        }

        let pts_ms = pts_to_ms(effective_pts, stream_time_base);

        // Flush any pending lace on a different track than this packet,
        // and flush the current track's lace before any cluster
        // restart — both because a lace is bounded to a single
        // cluster (the Block timestamp is a cluster-relative
        // signed 16-bit offset; spanning clusters would orphan the
        // tail frames' implicit timestamps) and because the
        // SimpleBlock KEY bit applies to the whole Block. A packet
        // carrying BlockAdditions bypasses the lacing buffer entirely
        // (it becomes its own BlockGroup), so its own track's pending
        // lace must flush too — otherwise the buffered earlier frames
        // would land *after* this one in the file.
        if self.lacing_mode != LacingMode::None {
            for other_idx in 0..self.lace_pending.len() {
                if (other_idx != stream_idx || group.is_some())
                    && !self.lace_pending[other_idx].frames.is_empty()
                {
                    self.flush_lace(other_idx)?;
                }
            }
        }

        // Decide whether to start a new cluster.
        let needs_new_cluster = !self.cluster_open
            || pts_ms - self.cluster_timecode_ms > CLUSTER_DURATION_MS
            || pts_ms - self.cluster_timecode_ms > i16::MAX as i64
            || pts_ms - self.cluster_timecode_ms < 0;
        if needs_new_cluster {
            // Flush the in-flight lace on the SAME track before
            // moving to a new cluster — its frames belong to the
            // old cluster's timecode space.
            if self.lacing_mode != LacingMode::None
                && !self.lace_pending[stream_idx].frames.is_empty()
            {
                self.flush_lace(stream_idx)?;
            }
            self.start_cluster(pts_ms)?;
        }

        let timecode_offset = pts_ms - self.cluster_timecode_ms;
        if timecode_offset < i16::MIN as i64 || timecode_offset > i16::MAX as i64 {
            return Err(Error::other(
                "MKV muxer: packet timecode delta exceeds i16 range",
            ));
        }

        // Cue index. For video we index the first keyframe (random-access
        // point) per (cluster, track). For audio we index the first frame
        // per cluster — every audio frame is independently decodable, so
        // the cluster-start is a valid seek target and §22.1 recommends at
        // most one audio cue per ~500 ms / cluster. For SUBTITLE tracks
        // §22.1 is stronger: "each subtitle frame SHOULD be referenced by a
        // CuePoint element with a CueDuration element" — so we index EVERY
        // subtitle frame (bypassing the per-cluster dedup) and carry its
        // CueDuration (§5.1.5.1.2.4).
        //
        // `CueRelativePosition` (RFC 9559 §5.1.5.1.2.3) is measured from the
        // first possible element position inside the Cluster (i.e. the byte
        // immediately after the Cluster id+size header — see
        // `start_cluster`). For the non-lacing fast path we know the
        // SimpleBlock will be written at the current file position, so we
        // capture the offset before the write. For the lacing path the
        // block is buffered and flushed later, so the relative position is
        // computed inside `flush_lace`.
        let pre_block_pos = self.output.stream_position().unwrap_or(0);
        let pre_block_rel = pre_block_pos.saturating_sub(self.cluster_body_start_abs);
        let is_subtitle = media_type == MediaType::Subtitle;
        // Subtitle tracks index every frame; others index only once per
        // cluster (gated by `cue_seen_in_cluster`).
        let dedup_allows = is_subtitle || !self.cue_seen_in_cluster[stream_idx];
        if dedup_allows {
            let indexable = match media_type {
                MediaType::Video => packet.flags.keyframe,
                _ => true,
            };
            if indexable && (self.lacing_mode == LacingMode::None || group.is_some()) {
                // Non-lacing path (and the BlockGroup-with-additions
                // path, which always writes immediately): the block
                // lands at `pre_block_pos`, emit the cue now with the
                // correct relative position. The block about to be
                // written takes the next 1-based number in the cluster
                // (RFC 9559 §5.1.5.1.2.5 `CueBlockNumber`), and its
                // `CueDuration` (§5.1.5.1.2.4) comes from the packet's
                // derived duration when one is available.
                let cue_duration_ms = derived_duration
                    .map(|d| pts_to_ms(d, stream_time_base))
                    .filter(|d| *d >= 0)
                    .map(|d| d as u64);
                self.cues.push(CueRecord {
                    track: track_number,
                    time_ms: pts_ms.max(0) as u64,
                    cluster_offset: self.cluster_offset_rel,
                    relative_position: pre_block_rel,
                    block_number: self.cluster_block_count + 1,
                    duration_ms: cue_duration_ms,
                });
                self.cue_seen_in_cluster[stream_idx] = true;
            }
            // Lacing path: cue emission happens in `flush_lace` once we
            // actually know where the (possibly laced) block lands.
        }

        if let Some(opts) = group {
            // BlockGroup path (RFC 9559 §5.1.3.5): Block + BlockAdditions
            // (+ BlockDuration when the packet carries a duration,
            // + ReferencePriority / ReferenceBlock / CodecState /
            // DiscardPadding per the caller's options). A plain Block
            // has no KEY flag bit — keyframe-ness is signalled by the
            // *absence* of ReferenceBlock (§5.1.3.5.5: "If the BlockGroup
            // doesn't have a ReferenceBlock element, then the Block it
            // contains can be decoded without using any other Block
            // data").
            //
            // ReferenceBlock list resolution (§5.1.3.5.5): an explicit
            // list from the caller is written verbatim; otherwise a
            // non-keyframe packet derives a single ReferenceBlock from the
            // most recently written Block on the same track (`0` when none
            // exists — "cannot be decoded on its own, but the necessary
            // reference Block(s) is unknown"), and a keyframe writes none.
            let reference_blocks: Vec<i64> = if !opts.reference_blocks.is_empty() {
                opts.reference_blocks.clone()
            } else if packet.flags.keyframe {
                Vec::new()
            } else {
                vec![self.last_block_pts_ms[stream_idx]
                    .map(|prev| prev - pts_ms)
                    .unwrap_or(0)]
            };
            // BlockDuration (§5.1.3.5.3) in track ticks (ms). Only
            // emitted when the packet carries a non-negative duration —
            // the element is unsigned.
            let duration_ms = derived_duration
                .map(|d| pts_to_ms(d, stream_time_base))
                .filter(|d| *d >= 0)
                .map(|d| d as u64);
            let bytes = build_block_group(
                track_number,
                timecode_offset as i16,
                &packet.data,
                &opts.additions,
                duration_ms,
                &reference_blocks,
                opts.reference_priority,
                opts.codec_state.as_deref(),
                opts.discard_padding,
                opts.block_virtual.as_deref(),
                opts.reference_virtual,
                &opts.slices,
                opts.reference_frame.as_ref(),
            );
            self.output.write_all(&bytes)?;
            // One Block written to the open Cluster — advances the 1-based
            // `CueBlockNumber` counter (RFC 9559 §5.1.5.1.2.5).
            self.cluster_block_count += 1;
        } else if self.lacing_mode == LacingMode::None {
            // Fast path: emit a standalone SimpleBlock with lacing
            // bits = 00 (RFC 9559 §10.3.1). Matches the
            // pre-with_block_lacing behaviour byte-for-byte.
            let block_bytes = build_simple_block(
                track_number,
                timecode_offset as i16,
                packet.flags.keyframe,
                LacingMode::None,
                std::slice::from_ref(&packet.data),
            );
            self.output.write_all(&block_bytes)?;
            // One SimpleBlock written to the open Cluster — advances the
            // 1-based `CueBlockNumber` counter (RFC 9559 §5.1.5.1.2.5).
            self.cluster_block_count += 1;
        } else {
            self.append_to_lace(stream_idx, timecode_offset as i16, packet)?;
        }
        // Remember this Block's timestamp so a later non-keyframe
        // BlockGroup on the same track can derive its ReferenceBlock
        // (§5.1.3.5.5) relative value.
        self.last_block_pts_ms[stream_idx] = Some(pts_ms);
        Ok(())
    }

    /// Write one packet as a `BlockGroup` (RFC 9559 §5.1.3.5) carrying
    /// the given `BlockAdditions` (§5.1.3.5.2) side-channel payloads in
    /// addition to the frame data — the write-side counterpart of
    /// [`crate::demux::MkvDemuxer::block_additions`].
    ///
    /// On-disk shape: `BlockGroup > Block` (the frame bytes, unlaced —
    /// a packet with additions always bypasses any
    /// [`MkvMuxer::with_block_lacing`] aggregation and flushes the
    /// track's pending lace first so Block order is preserved), one
    /// `BlockMore` per addition in slice order (each writing
    /// `BlockAdditional` verbatim and `BlockAddID` only when it differs
    /// from the §5.1.3.5.2.3 default `1`), `BlockDuration` (§5.1.3.5.3)
    /// when the packet carries a duration, and `ReferenceBlock`
    /// (§5.1.3.5.5) when the packet is not a keyframe (a plain `Block`
    /// has no KEY flag bit; keyframe-ness is the *absence* of
    /// `ReferenceBlock`).
    ///
    /// Prerequisite: the stream must have declared a non-zero
    /// `MaxBlockAdditionID` via [`MkvMuxer::set_max_block_addition_id`]
    /// before `write_header` — §5.1.4.1.16's default `0` means "there
    /// is no BlockAdditions for this track", and the muxer refuses to
    /// emit Blocks that contradict their own `TrackEntry`.
    ///
    /// Validation (all before any byte is written):
    ///
    /// * `Error::other` — `write_header` not called yet.
    /// * `Error::invalid` — out-of-range `packet.stream_index`; an
    ///   addition with `id == 0` (§5.1.3.5.2.3 ranges `BlockAddID` as
    ///   "not 0"); an addition whose `id` exceeds the declared
    ///   `MaxBlockAdditionID` (§5.1.4.1.16); two additions sharing an
    ///   `id` (§5.1.3.5.2.3: "Each BlockAddID value MUST be unique
    ///   between all BlockMore elements found in a BlockAdditions
    ///   element"); the stream never declared a `MaxBlockAdditionID`.
    ///
    /// An empty `additions` slice degrades to plain
    /// [`Muxer::write_packet`] behaviour (a `SimpleBlock`, lacing
    /// eligible) — no empty `BlockAdditions` master is written, since
    /// `BlockMore` is mandatory inside one (§5.1.3.5.2.1
    /// `minOccurs: 1`).
    pub fn write_packet_with_additions(
        &mut self,
        packet: &Packet,
        additions: &[MkvBlockAddition],
    ) -> Result<()> {
        if additions.is_empty() {
            return self.write_packet_inner(packet, None);
        }
        self.validate_additions(packet, additions)?;
        let opts = BlockGroupOptions {
            additions: additions.to_vec(),
            ..Default::default()
        };
        self.write_packet_inner(packet, Some(&opts))
    }

    /// Write one packet wrapped in a `BlockGroup` (RFC 9559 §5.1.3.5)
    /// carrying the full set of group children specified by `opts` —
    /// `BlockAdditions` (§5.1.3.5.2), `ReferenceBlock`(s) (§5.1.3.5.5),
    /// `ReferencePriority` (§5.1.3.5.4), `CodecState` (§5.1.3.5.6) and
    /// `DiscardPadding` (§5.1.3.5.7). This is the write-side counterpart of
    /// [`MkvDemuxer::block_group_meta`](crate::demux::MkvDemuxer::block_group_meta);
    /// a mux→demux round-trip preserves every child value.
    ///
    /// The packet always becomes its own `BlockGroup` (never a
    /// `SimpleBlock`, never laced) and flushes the track's pending lace
    /// first so Block order is preserved — exactly as
    /// [`MkvMuxer::write_packet_with_additions`] does.
    ///
    /// On-disk child order follows §5.1.3.5: `Block`, `BlockAdditions`,
    /// `BlockDuration`, `ReferencePriority`, `ReferenceBlock`(s),
    /// `CodecState`, `DiscardPadding`. `BlockDuration` is derived from the
    /// packet's duration as usual.
    ///
    /// Validation mirrors [`MkvMuxer::write_packet_with_additions`] for the
    /// `additions` field (non-zero `BlockAddID`s, within the declared
    /// `MaxBlockAdditionID`, unique); the other children carry no
    /// cross-field constraints the muxer can check. When `opts.additions`
    /// is empty the addition checks are skipped (no `MaxBlockAdditionID`
    /// declaration is required for a group that only sets, e.g.,
    /// `DiscardPadding`).
    pub fn write_packet_with_block_group(
        &mut self,
        packet: &Packet,
        opts: &BlockGroupOptions,
    ) -> Result<()> {
        if !self.header_written {
            return Err(Error::other("MKV muxer: write_header not called"));
        }
        if !opts.additions.is_empty() {
            self.validate_additions(packet, &opts.additions)?;
        } else {
            // Still range-check the stream index even when there are no
            // additions to validate, so a bad index errors before any byte.
            let stream_idx = packet.stream_index as usize;
            if stream_idx >= self.streams.len() {
                return Err(Error::invalid(format!(
                    "MKV muxer: unknown stream index {stream_idx}"
                )));
            }
        }
        self.write_packet_inner(packet, Some(opts))
    }

    /// Shared `BlockAdditions` validation (RFC 9559 §5.1.3.5.2.3 +
    /// §5.1.4.1.16) used by both `write_packet_with_additions` and
    /// `write_packet_with_block_group`. Runs before any byte is written.
    fn validate_additions(&self, packet: &Packet, additions: &[MkvBlockAddition]) -> Result<()> {
        if !self.header_written {
            return Err(Error::other("MKV muxer: write_header not called"));
        }
        let stream_idx = packet.stream_index as usize;
        if stream_idx >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV muxer: unknown stream index {stream_idx}"
            )));
        }
        let declared = self.max_block_addition_ids[stream_idx].unwrap_or(0);
        if declared == 0 {
            return Err(Error::invalid(format!(
                "MKV muxer: stream {stream_idx} has MaxBlockAdditionID 0 — RFC 9559 §5.1.4.1.16 \
                 means no BlockAdditions for this track; declare a non-zero maximum via \
                 set_max_block_addition_id before write_header"
            )));
        }
        for (i, a) in additions.iter().enumerate() {
            if a.id == 0 {
                return Err(Error::invalid(
                    "MKV muxer: BlockAddID 0 is out of range — RFC 9559 §5.1.3.5.2.3 ranges the \
                     element as \"not 0\"",
                ));
            }
            if a.id > declared {
                return Err(Error::invalid(format!(
                    "MKV muxer: BlockAddID {} exceeds the track's declared MaxBlockAdditionID {} \
                     (RFC 9559 §5.1.4.1.16)",
                    a.id, declared
                )));
            }
            if additions[..i].iter().any(|b| b.id == a.id) {
                return Err(Error::invalid(format!(
                    "MKV muxer: duplicate BlockAddID {} — RFC 9559 §5.1.3.5.2.3 requires each \
                     value to be unique between the BlockMore elements of one BlockAdditions",
                    a.id
                )));
            }
        }
        Ok(())
    }

    fn start_cluster(&mut self, timecode_ms: i64) -> Result<()> {
        // Capture the absolute file offset of the Cluster element header —
        // Cues will store (offset - segment_data_start) as
        // CueClusterPosition.
        let cluster_abs = self.output.stream_position().unwrap_or(0);
        self.cluster_offset_rel = cluster_abs.saturating_sub(self.segment_data_start);
        // Write Cluster element id + unknown-size sentinel.
        self.output.write_all(&write_element_id(ids::CLUSTER))?;
        self.output.write_all(&write_vint(VINT_UNKNOWN_SIZE, 0))?;
        // Right after the id+size header is the "first possible element
        // position" inside the Cluster — the anchor that
        // `CueRelativePosition` (RFC 9559 §5.1.5.1.2.3) is measured
        // against.
        self.cluster_body_start_abs = self.output.stream_position().unwrap_or(0);
        // Write Timecode child element.
        let mut tc = Vec::new();
        write_uint_element(&mut tc, ids::TIMECODE, timecode_ms.max(0) as u64);
        self.output.write_all(&tc)?;
        // Damage-recovery / backward-play hints (opt-in via
        // `with_cluster_position_hints`): `Position` (RFC 9559 §5.1.3.2,
        // the Cluster's Segment Position — same value space as
        // `CueClusterPosition`) and `PrevSize` (§5.1.3.3, the previous
        // Cluster's full element size in octets — Clusters are written
        // back-to-back, so it is the distance between consecutive Cluster
        // starts). Emitted right after `Timestamp` per the §5.1.3.1 usage
        // note that keeps `Timestamp` first.
        if self.cluster_position_hints {
            let mut hints = Vec::new();
            // §5.1.3.2: Position is "0 in live streams" — the Cluster
            // offset is not determined ahead of time on a live path.
            let pos_value = if self.live_streaming {
                0
            } else {
                self.cluster_offset_rel
            };
            write_uint_element(&mut hints, ids::POSITION, pos_value);
            if let Some(prev) = self.prev_cluster_start_abs {
                write_uint_element(&mut hints, ids::PREV_SIZE, cluster_abs.saturating_sub(prev));
            }
            self.output.write_all(&hints)?;
        }
        self.prev_cluster_start_abs = Some(cluster_abs);
        // SilentTracks (RFC 9559 Appendix A.1) — emitted once per Cluster
        // when the caller queued track numbers for this Cluster via
        // `set_next_cluster_silent_tracks`. The list is drained so it
        // applies to exactly this one Cluster (A.2: a track silent here MAY
        // be active again later).
        if !self.pending_silent_tracks.is_empty() {
            let mut st_body = Vec::new();
            for n in self.pending_silent_tracks.drain(..) {
                write_uint_element(&mut st_body, ids::SILENT_TRACK_NUMBER, n);
            }
            let mut st = Vec::new();
            write_master_element(&mut st, ids::SILENT_TRACKS, &st_body);
            self.output.write_all(&st)?;
        }
        self.cluster_timecode_ms = timecode_ms.max(0);
        self.cluster_open = true;
        // New cluster → clear the "already cued this track" flags and the
        // per-cluster Block counter that backs `CueBlockNumber`.
        for s in self.cue_seen_in_cluster.iter_mut() {
            *s = false;
        }
        self.cluster_block_count = 0;
        Ok(())
    }

    /// Build a Cues element from the `cues` vector and write it out. Returns
    /// the absolute file offset of the Cues element header relative to the
    /// Segment payload start, or `None` if the muxer had no cues to emit.
    /// Called from `write_trailer`.
    fn write_cues(&mut self) -> Result<Option<u64>> {
        if self.cues.is_empty() {
            return Ok(None);
        }
        // Group cues by time, combining the per-track entries of a
        // single cluster into one CuePoint. Per the EBML spec
        // (matroska CuePoint definition) multiple CueTrackPositions
        // may appear under one CuePoint at a given CueTime; this
        // grouping produces the more compact form that common
        // matroska demuxers (validated by black-box round-trip
        // against mkvalidator + black-box file equivalence with
        // streams emitted by widely-deployed muxers) consume
        // without quirks.
        let mut by_time: std::collections::BTreeMap<u64, Vec<CueRecord>> =
            std::collections::BTreeMap::new();
        for c in &self.cues {
            by_time.entry(c.time_ms).or_default().push(*c);
        }
        let mut body = Vec::new();
        for (time, entries) in by_time {
            let mut cp = Vec::new();
            write_uint_element(&mut cp, ids::CUE_TIME, time);
            for e in entries {
                let mut ctp = Vec::new();
                write_uint_element(&mut ctp, ids::CUE_TRACK, e.track);
                write_uint_element(&mut ctp, ids::CUE_CLUSTER_POSITION, e.cluster_offset);
                // `CueRelativePosition` (RFC 9559 §5.1.5.1.2.3) is a
                // SHOULD-write per §22.1: "If the referenced frame is not
                // stored within the first SimpleBlock or first BlockGroup
                // within its Cluster element, then the
                // CueRelativePosition element SHOULD be written to
                // reference where in the Cluster the reference frame is
                // stored." We write it unconditionally — it costs at
                // most a handful of bytes per cue entry and lets readers
                // skip past intervening blocks instead of scanning the
                // cluster from the start.
                write_uint_element(&mut ctp, ids::CUE_RELATIVE_POSITION, e.relative_position);
                // `CueDuration` (RFC 9559 §5.1.5.1.2.4) when the indexed
                // packet carried a usable duration — §22.1 specifically
                // recommends it for subtitle cues ("each subtitle frame
                // SHOULD be referenced by a CuePoint element with a
                // CueDuration element"). Omitted on absence, per the
                // §5.1.5.1.2.4 "no duration information available" reading
                // (rather than materialising a zero, which would falsely
                // claim a zero-length block). Sits before `CueBlockNumber`
                // to follow the spec's element definition order.
                if let Some(dur) = e.duration_ms {
                    write_uint_element(&mut ctp, ids::CUE_DURATION, dur);
                }
                // `CueBlockNumber` (RFC 9559 §5.1.5.1.2.5) — the 1-based
                // number of the indexed Block within its Cluster. Always
                // written: it lets a reader land on the exact Block without
                // relying solely on `CueRelativePosition`, and the value is
                // never `0` (the counter starts the cluster's first block
                // at `1`, honouring the element's `range: not 0`).
                write_uint_element(&mut ctp, ids::CUE_BLOCK_NUMBER, e.block_number);
                write_master_element(&mut cp, ids::CUE_TRACK_POSITIONS, &ctp);
            }
            write_master_element(&mut body, ids::CUE_POINT, &cp);
        }
        let mut out = Vec::with_capacity(body.len() + 8 + CRC32_CHILD_LEN);
        write_master_element_with_crc(&mut out, ids::CUES, &body);
        let cues_abs = self.output.stream_position().unwrap_or(0);
        self.output.write_all(&out)?;
        Ok(Some(cues_abs.saturating_sub(self.segment_data_start)))
    }

    /// Append one frame to the lace buffer for `stream_idx`.
    ///
    /// The append flushes the existing buffer first when the new
    /// packet cannot extend it — different keyframe flag, the
    /// per-Block frame cap reached, or (for `LacingMode::FixedSize`)
    /// a size mismatch with the buffered run. After the flush, the
    /// new packet seeds the buffer with its own timecode + keyframe
    /// flag.
    ///
    /// The buffer's first frame anchors the resulting Block's
    /// timecode (per RFC 9559 §10.3.5: a Block carries one
    /// timestamp value, which "applies to the first frame in the
    /// lace"). Subsequent frames' presentation timestamps are
    /// inferred by the demuxer from `DefaultDuration` or the
    /// laced-frames spec, and the muxer doesn't write per-frame
    /// timestamps anywhere on disk.
    fn append_to_lace(
        &mut self,
        stream_idx: usize,
        timecode_offset: i16,
        packet: &Packet,
    ) -> Result<()> {
        // Decide if the buffer can absorb this frame. Conditions
        // for "incompatible with current lace" (flush + restart):
        //   * keyframe flag differs (KEY bit applies to the whole
        //     Block)
        //   * we've hit the frame cap
        //   * (FixedSize only) frame size differs from the
        //     buffered run
        let must_flush = {
            let buf = &self.lace_pending[stream_idx];
            if buf.frames.is_empty() {
                false
            } else {
                buf.keyframe != packet.flags.keyframe
                    || buf.frames.len() >= MAX_FRAMES_PER_LACE
                    || (self.lacing_mode == LacingMode::FixedSize
                        && buf.frames[0].len() != packet.data.len())
            }
        };
        if must_flush {
            self.flush_lace(stream_idx)?;
        }
        let buf = &mut self.lace_pending[stream_idx];
        if buf.frames.is_empty() {
            buf.first_timecode_offset = timecode_offset;
            buf.keyframe = packet.flags.keyframe;
        }
        buf.frames.push(packet.data.clone());
        Ok(())
    }

    /// Drain the lace buffer for `stream_idx` to disk as a single
    /// SimpleBlock — laced if more than one frame is queued, or as
    /// a `LacingMode::None` Block if exactly one frame is queued
    /// (the spec forbids lacing a single frame; see RFC 9559
    /// §10.3 "Lacing MUST NOT be used to store a single frame").
    fn flush_lace(&mut self, stream_idx: usize) -> Result<()> {
        let frames = std::mem::take(&mut self.lace_pending[stream_idx].frames);
        if frames.is_empty() {
            return Ok(());
        }
        let track_number = self.track_numbers[stream_idx];
        let tc_offset = self.lace_pending[stream_idx].first_timecode_offset;
        let keyframe = self.lace_pending[stream_idx].keyframe;
        let media_type = self.streams[stream_idx].params.media_type;
        // Per §10.3, a single-frame lace MUST use no-lacing mode.
        let mode = if frames.len() == 1 {
            LacingMode::None
        } else {
            self.lacing_mode
        };
        // Record a Cue entry for the first indexable laced block per
        // (cluster, track). The block lands at the current file
        // position; `CueRelativePosition` (RFC 9559 §5.1.5.1.2.3) is
        // measured from `cluster_body_start_abs` — see `start_cluster`.
        if !self.cue_seen_in_cluster[stream_idx] {
            let indexable = match media_type {
                MediaType::Video => keyframe,
                _ => true,
            };
            if indexable {
                let pre_block_pos = self.output.stream_position().unwrap_or(0);
                let pre_block_rel = pre_block_pos.saturating_sub(self.cluster_body_start_abs);
                let pts_ms = (self.cluster_timecode_ms + tc_offset as i64).max(0) as u64;
                self.cues.push(CueRecord {
                    track: track_number,
                    time_ms: pts_ms,
                    cluster_offset: self.cluster_offset_rel,
                    relative_position: pre_block_rel,
                    // The laced block about to be written takes the next
                    // 1-based number in the cluster (RFC 9559
                    // §5.1.5.1.2.5). A laced block aggregates several
                    // frames, so no single `CueDuration` (§5.1.5.1.2.4)
                    // applies — absence here means "no cue-level duration."
                    block_number: self.cluster_block_count + 1,
                    duration_ms: None,
                });
                self.cue_seen_in_cluster[stream_idx] = true;
            }
        }
        let block_bytes = build_simple_block(track_number, tc_offset, keyframe, mode, &frames);
        self.output.write_all(&block_bytes)?;
        // One SimpleBlock (possibly laced) written to the open Cluster —
        // advances the 1-based `CueBlockNumber` counter (§5.1.5.1.2.5).
        self.cluster_block_count += 1;
        Ok(())
    }

    /// Seek back to the SeekHead and either write the real Cues offset into
    /// the Cues SeekPosition slot, or replace the entire 21-byte Seek entry
    /// with a Void filler if `cues_offset_rel` is `None`. Restores the
    /// stream position to end-of-file before returning so subsequent writes
    /// (in case anyone calls `write_trailer` followed by more output) see a
    /// consistent cursor.
    fn patch_cues_seek_entry(&mut self, cues_offset_rel: Option<u64>) -> Result<()> {
        use std::io::SeekFrom;
        let resume_pos = self.output.stream_position().unwrap_or(0);
        match cues_offset_rel {
            Some(off) => {
                // Patch the 8-byte SeekPosition payload only; the rest of
                // the Seek entry was written correctly up front.
                let payload_pos = self.seek_cues_entry_offset + SEEK_POS_PAYLOAD_OFFSET as u64;
                self.output.seek(SeekFrom::Start(payload_pos))?;
                self.output.write_all(&off.to_be_bytes())?;
            }
            None => {
                // Rewrite the whole 21-byte slot as a Void element.
                self.output
                    .seek(SeekFrom::Start(self.seek_cues_entry_offset))?;
                self.output.write_all(&void_seek_entry())?;
            }
        }
        // Return the cursor to where the trailer left it — keeps the file's
        // logical end-of-write at the post-Cues position.
        self.output.seek(SeekFrom::Start(resume_pos))?;
        Ok(())
    }
}

/// Build a SimpleBlock element (RFC 9559 §10.2). `frames` is the
/// ordered list of frame payloads — exactly one for `LacingMode::None`,
/// two or more for a laced Block. The KEY bit (`keyframe`) applies
/// to the whole Block; lacing bits come from `mode.flag_bits()`.
///
/// Block layout (matches Figure 13 / §10.2 of RFC 9559):
///   - TrackNumber as a VINT (1..8 bytes)
///   - Timestamp as signed 16-bit big-endian
///   - 8-bit flags: KEY | rsvd(3) | INV | LACING(2) | DIS
///   - lacing-payload header (FrameSizes; absent for `None` /
///     `FixedSize` rules differ — see emit_*_lacing functions)
///   - frame payloads concatenated
fn build_simple_block(
    track: u64,
    tc_offset: i16,
    keyframe: bool,
    mode: LacingMode,
    frames: &[Vec<u8>],
) -> Vec<u8> {
    // Conservative initial capacity: header + sum of frame sizes.
    let payload_total: usize = frames.iter().map(|f| f.len()).sum();
    let mut body = Vec::with_capacity(4 + payload_total + 8 * frames.len());
    body.extend_from_slice(&write_vint(track, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    let mut flags: u8 = 0;
    if keyframe {
        flags |= 0x80;
    }
    // LACING bits sit in positions 1..3 of the flags byte (RFC 9559
    // §10.2). `mode.flag_bits()` returns the 2-bit value; shift
    // left by 1 to place it correctly.
    flags |= mode.flag_bits() << 1;
    body.push(flags);

    match mode {
        LacingMode::None => {
            debug_assert_eq!(
                frames.len(),
                1,
                "no-lacing Block must carry exactly 1 frame"
            );
            body.extend_from_slice(&frames[0]);
        }
        LacingMode::Xiph => {
            emit_xiph_lacing(&mut body, frames);
        }
        LacingMode::Ebml => {
            emit_ebml_lacing(&mut body, frames);
        }
        LacingMode::FixedSize => {
            emit_fixed_lacing(&mut body, frames);
        }
    }

    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&write_element_id(ids::SIMPLE_BLOCK));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(&body);
    out
}

/// Build a `BlockGroup` element (RFC 9559 §5.1.3.5) carrying one unlaced
/// frame plus its `BlockAdditions` (§5.1.3.5.2) side-channel payloads.
///
/// Children, in order:
///
/// * `Block` (§5.1.3.5.1, §10.2): TrackNumber VINT + signed 16-bit
///   timestamp + flags byte `0x00` — a plain Block has no KEY flag bit
///   (keyframe-ness is signalled by the absence of `ReferenceBlock`)
///   and these frames are never laced — followed by the frame bytes.
/// * `BlockAdditions` (§5.1.3.5.2): one `BlockMore` per addition in
///   slice order. Inside each `BlockMore` the children follow the
///   §5.1.3.5.2.x subsection order — `BlockAdditional` (§5.1.3.5.2.2)
///   verbatim, then `BlockAddID` (§5.1.3.5.2.3) only when it differs
///   from the spec default `1` (a mandatory element with a default may
///   stay off-disk at that default).
/// * `BlockDuration` (§5.1.3.5.3, uinteger, track ticks) when `Some`.
/// * `ReferencePriority` (§5.1.3.5.4, uinteger) when non-zero — the spec
///   default `0` stays off-disk.
/// * `ReferenceBlock` (§5.1.3.5.5, signed integer, track ticks relative
///   to this Block's timestamp): one element per `reference_blocks` entry
///   (a non-keyframe references at least one Block; a keyframe none).
/// * `CodecState` (§5.1.3.5.6, binary) when `Some`.
/// * `DiscardPadding` (§5.1.3.5.7, signed integer, Matroska Ticks) when
///   `Some`.
///
/// An empty `additions` slice writes no `BlockAdditions` master at all —
/// `BlockMore` is mandatory inside one (§5.1.3.5.2.1 `minOccurs: 1`), so an
/// empty master would be malformed.
#[allow(clippy::too_many_arguments)]
fn build_block_group(
    track: u64,
    tc_offset: i16,
    frame: &[u8],
    additions: &[MkvBlockAddition],
    duration_ticks: Option<u64>,
    reference_blocks: &[i64],
    reference_priority: u64,
    codec_state: Option<&[u8]>,
    discard_padding: Option<i64>,
    block_virtual: Option<&[u8]>,
    reference_virtual: Option<i64>,
    slices: &[crate::demux::TimeSlice],
    reference_frame: Option<&crate::demux::ReferenceFrame>,
) -> Vec<u8> {
    // Block child: same §10.2 header layout as SimpleBlock minus the
    // keyframe / discardable flag bits, lacing bits 00.
    let mut block_body = Vec::with_capacity(4 + frame.len());
    block_body.extend_from_slice(&write_vint(track, 0));
    block_body.extend_from_slice(&tc_offset.to_be_bytes());
    block_body.push(0x00);
    block_body.extend_from_slice(frame);

    let mut group_body = Vec::new();
    write_bytes_element(&mut group_body, ids::BLOCK, &block_body);

    if !additions.is_empty() {
        let mut additions_body = Vec::new();
        for a in additions {
            let mut more = Vec::with_capacity(8 + a.data.len());
            write_bytes_element(&mut more, ids::BLOCK_ADDITIONAL, &a.data);
            if a.id != 1 {
                write_uint_element(&mut more, ids::BLOCK_ADD_ID, a.id);
            }
            write_master_element(&mut additions_body, ids::BLOCK_MORE, &more);
        }
        write_master_element(&mut group_body, ids::BLOCK_ADDITIONS, &additions_body);
    }

    if let Some(d) = duration_ticks {
        write_uint_element(&mut group_body, ids::BLOCK_DURATION, d);
    }
    // ReferencePriority's spec default is 0; a mandatory element at its
    // default may stay off-disk.
    if reference_priority != 0 {
        write_uint_element(&mut group_body, ids::REFERENCE_PRIORITY, reference_priority);
    }
    for r in reference_blocks {
        write_int_element(&mut group_body, ids::REFERENCE_BLOCK, *r);
    }
    if let Some(s) = codec_state {
        write_bytes_element(&mut group_body, ids::CODEC_STATE, s);
    }
    if let Some(p) = discard_padding {
        write_int_element(&mut group_body, ids::DISCARD_PADDING, p);
    }

    // Reclaimed DivX trick-track / old-lacing children (RFC 9559 Appendix
    // A.3..A.14), written for a faithful re-mux. Only populated fields reach
    // the disk; the appendix names no defaults, so a present `0` is a real
    // value distinct from absence.
    if let Some(bv) = block_virtual {
        write_bytes_element(&mut group_body, ids::BLOCK_VIRTUAL, bv);
    }
    if let Some(rv) = reference_virtual {
        write_int_element(&mut group_body, ids::REFERENCE_VIRTUAL, rv);
    }
    if !slices.is_empty() {
        let mut slices_body = Vec::new();
        for ts in slices {
            let mut ts_body = Vec::new();
            if let Some(v) = ts.lace_number() {
                write_uint_element(&mut ts_body, ids::LACE_NUMBER, v);
            }
            if let Some(v) = ts.frame_number() {
                write_uint_element(&mut ts_body, ids::FRAME_NUMBER, v);
            }
            if let Some(v) = ts.block_addition_id() {
                write_uint_element(&mut ts_body, ids::TIME_SLICE_BLOCK_ADDITION_ID, v);
            }
            if let Some(v) = ts.delay() {
                write_uint_element(&mut ts_body, ids::DELAY, v);
            }
            if let Some(v) = ts.slice_duration() {
                write_uint_element(&mut ts_body, ids::SLICE_DURATION, v);
            }
            write_master_element(&mut slices_body, ids::TIME_SLICE, &ts_body);
        }
        write_master_element(&mut group_body, ids::SLICES, &slices_body);
    }
    if let Some(rf) = reference_frame {
        let mut rf_body = Vec::new();
        if let Some(v) = rf.reference_offset() {
            write_uint_element(&mut rf_body, ids::REFERENCE_OFFSET, v);
        }
        if let Some(v) = rf.reference_timestamp() {
            write_uint_element(&mut rf_body, ids::REFERENCE_TIMESTAMP, v);
        }
        write_master_element(&mut group_body, ids::REFERENCE_FRAME, &rf_body);
    }

    let mut out = Vec::with_capacity(8 + group_body.len());
    write_master_element(&mut out, ids::BLOCK_GROUP, &group_body);
    out
}

/// Append a Xiph-lacing payload to `body` (RFC 9559 §10.3.2):
/// `n_frames-1` octet, then for every frame except the last the
/// size as a sum of 255-additive unsigned octets (e.g. 500 →
/// `0xFF 0xF5`; 765 → `0xFF 0xFF 0xFF 0x00`), then the frame
/// payloads concatenated. The last frame's size is implicit
/// (Block size minus everything else).
fn emit_xiph_lacing(body: &mut Vec<u8>, frames: &[Vec<u8>]) {
    debug_assert!(frames.len() >= 2 && frames.len() <= 256);
    body.push((frames.len() - 1) as u8);
    // Per-frame size for every frame except the last.
    for f in &frames[..frames.len() - 1] {
        let mut remaining = f.len();
        while remaining >= 255 {
            body.push(0xFF);
            remaining -= 255;
        }
        body.push(remaining as u8);
    }
    for f in frames {
        body.extend_from_slice(f);
    }
}

/// Append a fixed-size-lacing payload to `body` (RFC 9559 §10.3.4):
/// `n_frames-1` octet, then the frame payloads concatenated. No
/// per-frame size header — every frame must have identical size,
/// which the caller ([`MkvMuxer::append_to_lace`]) enforces by
/// flushing on a size mismatch.
fn emit_fixed_lacing(body: &mut Vec<u8>, frames: &[Vec<u8>]) {
    debug_assert!(frames.len() >= 2 && frames.len() <= 256);
    debug_assert!(
        frames.iter().all(|f| f.len() == frames[0].len()),
        "fixed-size lacing requires equal-size frames"
    );
    body.push((frames.len() - 1) as u8);
    for f in frames {
        body.extend_from_slice(f);
    }
}

/// Append an EBML-lacing payload to `body` (RFC 9559 §10.3.3):
/// `n_frames-1` octet, first frame size as an unsigned VINT,
/// remaining sizes as signed VINT deltas (signed → unsigned with
/// `+ 2^(7n-1) - 1` bias, per Table 37), then frame payloads
/// concatenated. The last frame's size is implicit.
fn emit_ebml_lacing(body: &mut Vec<u8>, frames: &[Vec<u8>]) {
    debug_assert!(frames.len() >= 2 && frames.len() <= 256);
    body.push((frames.len() - 1) as u8);
    // First frame size: unsigned VINT.
    body.extend_from_slice(&write_vint(frames[0].len() as u64, 0));
    // Remaining sizes (except the last): signed deltas.
    let mut prev = frames[0].len() as i64;
    for f in &frames[1..frames.len() - 1] {
        let cur = f.len() as i64;
        let delta = cur - prev;
        body.extend_from_slice(&write_signed_vint(delta));
        prev = cur;
    }
    for f in frames {
        body.extend_from_slice(f);
    }
}

/// Encode a signed integer as a VINT with the §10.3.3 sign-to-
/// unsigned mapping. The smallest valid width is chosen so the
/// signed value fits exactly in its range (Table 37):
///
/// | width | range                                  |
/// |-------|----------------------------------------|
/// | 1     | -2^6 + 1 ..= 2^6                       |
/// | 2     | -2^13 + 1 ..= 2^13                     |
/// | 3     | -2^20 + 1 ..= 2^20                     |
/// | 4     | -2^27 + 1 ..= 2^27                     |
///
/// Unsigned encoding: `unsigned = signed + 2^(7n-1) - 1` for
/// width n. The result is then written as a fixed-width VINT —
/// the decoder reads the width from the leading-zeros prefix and
/// derives the bias from `n`, so emitting at a larger-than-
/// necessary width would land at the wrong bias and decode to a
/// different signed value. The bias-encoded value for the maximum
/// positive end of the range collides with that width's
/// "unknown-size sentinel" (`all-payload-ones`) for element-size
/// VINTs, so we use the lacing-specific helper [`write_vint_fixed`]
/// that emits the literal bit pattern without the sentinel
/// rejection that [`write_vint`] applies.
fn write_signed_vint(value: i64) -> Vec<u8> {
    for width in 1u8..=8 {
        let bias = (1i64 << (7 * width as i64 - 1)) - 1;
        let max_pos = 1i64 << (7 * width as i64 - 1);
        let min_neg = -(max_pos - 1);
        if value >= min_neg && value <= max_pos {
            let unsigned = (value + bias) as u64;
            return write_vint_fixed(unsigned, width);
        }
    }
    panic!("EBML signed VINT: value {value} out of range");
}

/// Emit `value` as a VINT at exactly `width` bytes, without the
/// "value equals the all-ones unknown-size sentinel" rejection
/// that [`write_vint`] applies. The sentinel rule applies to
/// element-size VINTs (RFC 8794 §6.1); lacing-payload sizes
/// (RFC 9559 §10.3.3) carry the literal bit pattern, so for those
/// we deliberately allow the all-payload-ones encoding.
///
/// Caller must guarantee `value < 2^(7*width)` — otherwise the
/// value would not fit and the function panics. Width must be in
/// `1..=8`.
fn write_vint_fixed(value: u64, width: u8) -> Vec<u8> {
    assert!((1..=8).contains(&width), "VINT width must be 1..=8");
    let payload_bits = 7u32 * width as u32;
    if payload_bits < 64 && value >= (1u64 << payload_bits) {
        panic!("write_vint_fixed: value {value} exceeds {width}-byte VINT range");
    }
    let mut out = vec![0u8; width as usize];
    // Marker bit at top of byte 0.
    out[0] = 1u8 << (8 - width);
    let mut v = value;
    for i in (0..width as usize).rev() {
        out[i] |= (v & 0xFF) as u8;
        v >>= 8;
    }
    out
}

fn pts_to_ms(value: i64, tb: oxideav_core::TimeBase) -> i64 {
    let r = tb.as_rational();
    if r.den == 0 {
        return value;
    }
    // value * num / den (in seconds) * 1000 (to ms).
    // Use i128 to avoid overflow.
    let v = value as i128 * r.num as i128 * 1000;
    (v / r.den as i128) as i64
}

/// Decode the Opus TOC byte (and code-3 frame count byte if needed) to get
/// the packet's total decoded sample count at 48 kHz. Returns `None` if the
/// packet doesn't look like a valid Opus packet.
///
/// Reference: RFC 6716 §3.1, Table 2.
fn opus_packet_duration_samples(packet: &[u8]) -> Option<u32> {
    if packet.is_empty() {
        return None;
    }
    let toc = packet[0];
    let config = toc >> 3;
    let frame_size_48k: u32 = match config {
        0 | 4 | 8 => 480,
        1 | 5 | 9 => 960,
        2 | 6 | 10 => 1920,
        3 | 7 | 11 => 2880,
        12 | 14 => 480,
        13 | 15 => 960,
        16 | 20 | 24 | 28 => 120,
        17 | 21 | 25 | 29 => 240,
        18 | 22 | 26 | 30 => 480,
        19 | 23 | 27 | 31 => 960,
        _ => return None,
    };
    let n_frames: u32 = match toc & 0x03 {
        0 => 1,
        1 | 2 => 2,
        3 => {
            if packet.len() < 2 {
                return None;
            }
            (packet[1] & 0x3F) as u32
        }
        _ => unreachable!(),
    };
    Some(frame_size_48k * n_frames)
}

/// Read the 16-bit pre-skip field from an OpusHead packet (RFC 7845 §5.1
/// bytes 10..12 little-endian). Returns 0 if the buffer doesn't look like
/// a valid OpusHead.
fn parse_opus_pre_skip(extradata: &[u8]) -> u16 {
    if extradata.len() < 12 || &extradata[0..8] != b"OpusHead" {
        return 0;
    }
    u16::from_le_bytes([extradata[10], extradata[11]])
}

fn encode_codec_private(codec_id: &oxideav_core::CodecId, extradata: &[u8]) -> Vec<u8> {
    match codec_id.as_str() {
        // Matroska's A_FLAC mapping carries the leading "fLaC" magic in
        // CodecPrivate even though many docs imply it's optional. Common
        // decoders expect it; we always prepend it on the muxer side.
        "flac" => {
            let mut out = Vec::with_capacity(4 + extradata.len());
            out.extend_from_slice(b"fLaC");
            out.extend_from_slice(extradata);
            out
        }
        _ => extradata.to_vec(),
    }
}

/// Write one `BlockAdditionMapping` master (RFC 9559 §5.1.4.1.17) into
/// `out`, mirroring the demux-side [`crate::demux::BlockAdditionMapping`]
/// projection. `BlockAddIDValue` / `BlockAddIDName` / `BlockAddIDExtraData`
/// are written only when their `Option` is `Some`; `BlockAddIDType` is
/// written only when non-zero, since the §5.1.4.1.17.3 default `0`
/// (codec-defined) is materialised by the demuxer from an absent element —
/// so a `0` stays off-disk and still round-trips as `0`.
fn write_block_addition_mapping(out: &mut Vec<u8>, mapping: &crate::demux::BlockAdditionMapping) {
    let mut body = Vec::new();
    if let Some(v) = mapping.value {
        write_uint_element(&mut body, ids::BLOCK_ADD_ID_VALUE, v);
    }
    if let Some(name) = &mapping.name {
        write_string_element(&mut body, ids::BLOCK_ADD_ID_NAME, name);
    }
    if mapping.addid_type != 0 {
        write_uint_element(&mut body, ids::BLOCK_ADD_ID_TYPE, mapping.addid_type);
    }
    if let Some(extra) = &mapping.extra_data {
        write_bytes_element(&mut body, ids::BLOCK_ADD_ID_EXTRA_DATA, extra);
    }
    write_master_element(out, ids::BLOCK_ADDITION_MAPPING, &body);
}

// --- Element-writing helpers ----------------------------------------------

/// Write a `date` element (RFC 9559 §5.1.2.11 / RFC 8794 §7.6): a signed
/// 8-byte big-endian integer counting nanoseconds since the Matroska epoch
/// (2001-01-01T00:00:00 UTC). The width is **fixed at 8 bytes** (the `date`
/// type's only legal on-disk length), unlike the minimal-width
/// [`write_int_element`] — the demuxer's `DateUTC` handler decodes only the
/// 8-byte form.
fn write_date_element(buf: &mut Vec<u8>, id: u32, ns_since_2001: i64) {
    buf.extend_from_slice(&write_element_id(id));
    buf.extend_from_slice(&write_vint(8, 0));
    buf.extend_from_slice(&ns_since_2001.to_be_bytes());
}

/// Write the Linked-Segment `Info` children (RFC 9559 §5.1.2.1..§5.1.2.8) into
/// `buf`, in the §5.1.2 element order: `SegmentUUID`, `SegmentFilename`,
/// `PrevUUID`, `PrevFilename`, `NextUUID`, `NextFilename`, `SegmentFamily`(s),
/// `ChapterTranslate`(s). All present/absent decisions follow the queued
/// [`SegmentLinking`]; an all-default record writes nothing. Validation
/// (16-byte UID lengths, PrevUUID/NextUUID ≠ SegmentUUID, SegmentFamily
/// required with ChapterTranslate, non-empty ChapterTranslateID) was enforced
/// at `set_segment_linking` time, so this writer assumes a well-formed record.
fn write_segment_linking(buf: &mut Vec<u8>, linking: &SegmentLinking) {
    if let Some(uuid) = &linking.segment_uuid {
        write_bytes_element(buf, ids::SEGMENT_UID, uuid);
    }
    if let Some(name) = &linking.segment_filename {
        write_string_element(buf, ids::SEGMENT_FILENAME, name);
    }
    if let Some(uuid) = &linking.prev_uuid {
        write_bytes_element(buf, ids::PREV_UID, uuid);
    }
    if let Some(name) = &linking.prev_filename {
        write_string_element(buf, ids::PREV_FILENAME, name);
    }
    if let Some(uuid) = &linking.next_uuid {
        write_bytes_element(buf, ids::NEXT_UID, uuid);
    }
    if let Some(name) = &linking.next_filename {
        write_string_element(buf, ids::NEXT_FILENAME, name);
    }
    for fam in &linking.families {
        write_bytes_element(buf, ids::SEGMENT_FAMILY, fam);
    }
    for ct in &linking.chapter_translates {
        write_chapter_translate(buf, ct);
    }
}

/// Write one `Segment\Info\ChapterTranslate` master (RFC 9559 §5.1.2.8) into
/// `buf`. Child order follows §5.1.2.8: the mandatory `ChapterTranslateID`
/// (§5.1.2.8.1) and `ChapterTranslateCodec` (§5.1.2.8.2), then any
/// `ChapterTranslateEditionUID`s (§5.1.2.8.3, unbounded — empty means "all
/// editions using the given codec").
fn write_chapter_translate(buf: &mut Vec<u8>, ct: &ChapterTranslate) {
    let mut body = Vec::new();
    write_bytes_element(&mut body, ids::CHAPTER_TRANSLATE_ID, &ct.id);
    write_uint_element(&mut body, ids::CHAPTER_TRANSLATE_CODEC, ct.codec);
    for &edition_uid in &ct.edition_uids {
        write_uint_element(&mut body, ids::CHAPTER_TRANSLATE_EDITION_UID, edition_uid);
    }
    write_master_element(buf, ids::CHAPTER_TRANSLATE, &body);
}

fn write_uint_element(buf: &mut Vec<u8>, id: u32, value: u64) {
    let n = if value == 0 {
        1
    } else {
        (64 - value.leading_zeros()).div_ceil(8) as usize
    };
    buf.extend_from_slice(&write_element_id(id));
    buf.extend_from_slice(&write_vint(n as u64, 0));
    for i in (0..n).rev() {
        buf.push(((value >> (i * 8)) & 0xFF) as u8);
    }
}

/// Write a signed-integer element (RFC 8794 §7.1: two's complement
/// notation with the leftmost bit being the sign bit, 0-8 octets).
/// This writer always picks the minimal octet count that represents
/// the value — `n` octets cover `-(2^(8n-1)) ..= 2^(8n-1) - 1`.
fn write_int_element(buf: &mut Vec<u8>, id: u32, value: i64) {
    let mut n = 1usize;
    while n < 8 {
        let min = -(1i64 << (8 * n - 1));
        let max = (1i64 << (8 * n - 1)) - 1;
        if value >= min && value <= max {
            break;
        }
        n += 1;
    }
    buf.extend_from_slice(&write_element_id(id));
    buf.extend_from_slice(&write_vint(n as u64, 0));
    for i in (0..n).rev() {
        buf.push(((value >> (i * 8)) & 0xFF) as u8);
    }
}

fn write_string_element(buf: &mut Vec<u8>, id: u32, value: &str) {
    buf.extend_from_slice(&write_element_id(id));
    buf.extend_from_slice(&write_vint(value.len() as u64, 0));
    buf.extend_from_slice(value.as_bytes());
}

fn write_bytes_element(buf: &mut Vec<u8>, id: u32, value: &[u8]) {
    buf.extend_from_slice(&write_element_id(id));
    buf.extend_from_slice(&write_vint(value.len() as u64, 0));
    buf.extend_from_slice(value);
}

fn write_float_element(buf: &mut Vec<u8>, id: u32, value: f64) {
    buf.extend_from_slice(&write_element_id(id));
    buf.extend_from_slice(&write_vint(8, 0));
    buf.extend_from_slice(&value.to_be_bytes());
}

fn write_master_element(buf: &mut Vec<u8>, id: u32, body: &[u8]) {
    buf.extend_from_slice(&write_element_id(id));
    buf.extend_from_slice(&write_vint(body.len() as u64, 0));
    buf.extend_from_slice(body);
}

/// Size of a serialised `CRC-32` child element on disk: 1-byte id
/// `0xBF` + 1-byte size VINT `0x84` (payload-length 4) + 4-byte
/// little-endian value = 6 bytes total. Constant because RFC 8794
/// §11.3.1 fixes the `CRC-32` element to exactly 4 payload bytes and
/// its id encodes in one byte.
const CRC32_CHILD_LEN: usize = 6;

/// Serialise a `CRC-32` element (RFC 8794 §11.3.1) whose 4-byte
/// payload is the IEEE CRC-32 of `data`, stored little-endian. The
/// returned buffer is always `CRC32_CHILD_LEN` bytes long: id `0xBF`
/// + size VINT `0x84` + four payload bytes.
fn build_crc32_child(data: &[u8]) -> [u8; CRC32_CHILD_LEN] {
    let crc = crc32_ieee(data);
    let bytes = crc.to_le_bytes();
    [
        ids::CRC32 as u8, // 0xBF
        0x84,             // VINT for payload size 4 (marker | 4)
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
    ]
}

/// Write a master element with a leading `CRC-32` child computed
/// over `body` (RFC 9559 §6.2: "all Top-Level Elements of an EBML
/// Document SHOULD include a CRC-32 element as their first Child
/// Element"). The on-disk shape is
/// `id | size(crc_child + body) | CRC-32 child | body` so the demuxer
/// validates the CRC over `body` exactly, matching the demuxer's
/// existing `validate_top_level_crc` peel-off-leading-CRC rule.
fn write_master_element_with_crc(buf: &mut Vec<u8>, id: u32, body: &[u8]) {
    let crc = build_crc32_child(body);
    let inner_len = CRC32_CHILD_LEN + body.len();
    buf.extend_from_slice(&write_element_id(id));
    buf.extend_from_slice(&write_vint(inner_len as u64, 0));
    buf.extend_from_slice(&crc);
    buf.extend_from_slice(body);
}

// --- SeekHead helpers -----------------------------------------------------
//
// We emit a fixed-size SeekHead at the very start of the Segment payload so
// the muxer never has to "grow" the SeekHead after the fact. Each Seek
// entry is built with the maximum widths we'd ever need (4-byte SeekID, 8-byte
// SeekPosition), giving a constant per-entry size. The trailer rewrites the
// Cues entry's SeekPosition (or replaces the whole entry with a Void) once
// the real Cues offset is known — Info and Tracks offsets are known up
// front, so they're patched directly into the buffer before we flush.

/// Number of bytes consumed by the SeekHead header (id + size VINT) before
/// the first Seek child. `4 + 1 = 5` for the 126-byte body (6 × 21).
const SEEK_HEAD_HEADER_LEN: usize = 5;
/// Number of Seek entries the SeekHead reserves: Info, Tracks, Chapters,
/// Attachments, Tags, Cues. Chapters / Attachments / Tags are voided in
/// `write_header` and Cues in `write_trailer` when those elements
/// turn out to be empty.
const SEEK_HEAD_ENTRY_COUNT: usize = 6;
/// Total size of the SeekHead element on disk: header + N × 21-byte
/// Seek entries.
const SEEK_HEAD_TOTAL_LEN: usize = SEEK_HEAD_HEADER_LEN + SEEK_HEAD_ENTRY_COUNT * SEEK_ENTRY_LEN;
/// Size of one Seek entry on disk. The body is 7-byte SeekID +
/// 11-byte SeekPosition = 18 bytes; the entry header (id + size) adds 3
/// bytes for a fixed total of 21.
const SEEK_ENTRY_LEN: usize = 21;
/// Byte offset of the SeekPosition payload (the 8-byte big-endian uint)
/// within a 21-byte Seek entry. Layout:
///   bytes 0..3   — Seek master header (id 0x4DBB + size VINT 0x92)
///   bytes 3..10  — SeekID element (id 0x53AB + size VINT 0x84 + 4-byte id)
///   bytes 10..13 — SeekPosition header (id 0x53AC + size VINT 0x88)
///   bytes 13..21 — SeekPosition payload (big-endian u64)
const SEEK_POS_PAYLOAD_OFFSET: usize = 13;

/// Build the initial SeekHead with placeholder positions for Info,
/// Tracks, Chapters, Attachments, Tags, and Cues. The caller patches in
/// the real positions via `write_u64_be_at` once each element's offset is
/// known (or rewrites the slot as a Void if the element ends up not
/// being emitted).
fn build_initial_seek_head() -> Vec<u8> {
    let mut body = Vec::with_capacity(SEEK_HEAD_ENTRY_COUNT * SEEK_ENTRY_LEN);
    body.extend_from_slice(&seek_entry(ids::INFO, 0));
    body.extend_from_slice(&seek_entry(ids::TRACKS, 0));
    body.extend_from_slice(&seek_entry(ids::CHAPTERS, 0));
    body.extend_from_slice(&seek_entry(ids::ATTACHMENTS, 0));
    body.extend_from_slice(&seek_entry(ids::TAGS, 0));
    body.extend_from_slice(&seek_entry(ids::CUES, 0));
    debug_assert_eq!(body.len(), SEEK_HEAD_ENTRY_COUNT * SEEK_ENTRY_LEN);
    let mut out = Vec::with_capacity(SEEK_HEAD_TOTAL_LEN);
    write_master_element(&mut out, ids::SEEK_HEAD, &body);
    debug_assert_eq!(out.len(), SEEK_HEAD_TOTAL_LEN);
    out
}

/// Build a single 21-byte Seek entry with `target_id` (always a 4-byte
/// EBML class id for our top-level elements) and `position` (8-byte
/// big-endian, may be a placeholder zero).
fn seek_entry(target_id: u32, position: u64) -> Vec<u8> {
    let mut body = Vec::with_capacity(SEEK_ENTRY_LEN - 3);
    // SeekID: 4-byte big-endian id payload, regardless of how few bytes the
    // VINT encoding of the id itself would technically need. The Matroska
    // spec stores SeekID as the literal element id (with marker), so the
    // value 0x1654AE6B is written as 4 bytes 16 54 AE 6B.
    body.extend_from_slice(&write_element_id(ids::SEEK_ID));
    body.extend_from_slice(&write_vint(4, 0));
    body.extend_from_slice(&target_id.to_be_bytes());
    // SeekPosition: pinned to 8 bytes so we always have room to patch in
    // any offset later without resizing the SeekHead.
    body.extend_from_slice(&write_element_id(ids::SEEK_POSITION));
    body.extend_from_slice(&write_vint(8, 0));
    body.extend_from_slice(&position.to_be_bytes());
    debug_assert_eq!(body.len(), SEEK_ENTRY_LEN - 3);
    let mut entry = Vec::with_capacity(SEEK_ENTRY_LEN);
    write_master_element(&mut entry, ids::SEEK, &body);
    debug_assert_eq!(entry.len(), SEEK_ENTRY_LEN);
    entry
}

/// Build a Void element exactly the size of a Seek entry. Used in the
/// trailer to neutralise the Cues SeekHead entry when no Cues was emitted.
/// Layout: 0xEC (1 byte id) + 0x93 (size VINT for 19) + 19 bytes padding.
fn void_seek_entry() -> Vec<u8> {
    let mut out = Vec::with_capacity(SEEK_ENTRY_LEN);
    out.push(ids::VOID as u8); // 0xEC
    out.push(0x93); // size VINT, payload = 19
    out.resize(SEEK_ENTRY_LEN, 0u8);
    debug_assert_eq!(out.len(), SEEK_ENTRY_LEN);
    out
}

/// Write a 64-bit big-endian value at `pos` in `buf`. Caller must ensure
/// `pos + 8 <= buf.len()`.
fn write_u64_be_at(buf: &mut [u8], pos: usize, value: u64) {
    buf[pos..pos + 8].copy_from_slice(&value.to_be_bytes());
}

// --- Chapters --------------------------------------------------------------
//
// One `Chapters` master per file. Inside it we always emit exactly one
// `EditionEntry` — Matroska allows multiple editions (alternate cuts /
// language-versions / etc.) but the muxer's public surface
// (`add_chapter`) is single-edition-shaped, which matches every
// upstream use case so far (DVD ⟶ MKV: one VTS = one program chain =
// one chapter list).
//
// Element layout (RFC 9559 §5.1.7):
//
//   Chapters (0x1043A770)
//     EditionEntry (0x45B9)
//       EditionUID (0x45BC)        — 1-based, derived from edition index
//       EditionFlagDefault — omitted (default 0)
//       EditionFlagHidden  — omitted (default 0)
//       ChapterAtom (0xB6) × N
//         ChapterUID (0x73C4)      — 1-based atom index
//         ChapterTimeStart (0x91)  — ns, uint
//         ChapterTimeEnd   (0x92)  — ns, uint, optional
//         ChapterDisplay (0x80)
//           ChapString   (0x85)    — UTF-8 title
//           ChapLanguage (0x437C)  — ISO-639-2 3-letter
//           ChapCountry  (0x437E)  — optional BCP-47 region subtag

/// One stable edition UID used by every file we mux. The value is
/// arbitrary (UIDs are scope-local within a segment) — what matters is
/// that it's non-zero so that downstream `Tags.Targets.TagEditionUID`
/// references can resolve.
const EDITION_UID_DEFAULT: u64 = 1;

/// Build the bytes of a complete `Chapters` master element from the
/// queued chapter list. Caller appends the returned slice into the
/// muxer's outgoing buffer.
fn build_chapters_element(chapters: &[MkvChapter]) -> Vec<u8> {
    let mut edition_body = Vec::new();
    write_uint_element(&mut edition_body, ids::EDITION_UID, EDITION_UID_DEFAULT);
    for (i, ch) in chapters.iter().enumerate() {
        let atom = build_chapter_atom(i as u64 + 1, ch);
        write_master_element(&mut edition_body, ids::CHAPTER_ATOM, &atom);
    }
    let mut chapters_body = Vec::new();
    write_master_element(&mut chapters_body, ids::EDITION_ENTRY, &edition_body);
    let mut out = Vec::with_capacity(chapters_body.len() + 8 + CRC32_CHILD_LEN);
    write_master_element_with_crc(&mut out, ids::CHAPTERS, &chapters_body);
    out
}

/// Body of one `ChapterAtom` master (the caller wraps it in
/// `ids::CHAPTER_ATOM`). `default_uid` is the auto-derived 1-based UID used
/// when the chapter didn't pin one. Children are emitted in RFC 9559
/// §5.1.7.1.4 subsection order; the demuxer parses by element id so the
/// order is for validator-friendliness, not correctness.
fn build_chapter_atom(default_uid: u64, ch: &MkvChapter) -> Vec<u8> {
    let mut body = Vec::new();
    write_uint_element(&mut body, ids::CHAPTER_UID, ch.uid.unwrap_or(default_uid));
    if let Some(suid) = ch.string_uid.as_deref() {
        if !suid.is_empty() {
            write_string_element(&mut body, ids::CHAPTER_STRING_UID, suid);
        }
    }
    write_uint_element(&mut body, ids::CHAPTER_TIME_START, ch.time_start_ns);
    if let Some(end) = ch.time_end_ns {
        write_uint_element(&mut body, ids::CHAPTER_TIME_END, end);
    }
    // ChapterFlagHidden / ChapterFlagEnabled: write only the non-default
    // value (§5.1.7.1.4.5 default 0, enabled default 1), so a plain chapter
    // stays byte-compatible with the prior writer.
    if ch.hidden {
        write_uint_element(&mut body, ids::CHAPTER_FLAG_HIDDEN, 1);
    }
    if !ch.enabled {
        write_uint_element(&mut body, ids::CHAPTER_FLAG_ENABLED, 0);
    }
    if let Some(uuid) = &ch.segment_uuid {
        write_bytes_element(&mut body, ids::CHAPTER_SEGMENT_UUID, uuid);
    }
    if let Some(seuid) = ch.segment_edition_uid {
        write_uint_element(&mut body, ids::CHAPTER_SEGMENT_EDITION_UID, seuid);
    }
    if let Some(pe) = ch.physical_equiv {
        write_uint_element(&mut body, ids::CHAPTER_PHYSICAL_EQUIV, pe);
    }
    for disp in &ch.display {
        let mut display_body = Vec::new();
        write_string_element(&mut display_body, ids::CHAP_STRING, &disp.title);
        // RFC 9559 §5.1.7.1.4.11: when ChapLanguageBCP47 is present the
        // legacy ChapLanguage MUST be ignored, so write only the BCP-47 tag
        // in that case; otherwise write the ISO-639-2 ChapLanguage.
        if let Some(bcp47) = &disp.language_bcp47 {
            write_string_element(&mut display_body, ids::CHAP_LANGUAGE_BCP47, bcp47);
        } else {
            write_string_element(&mut display_body, ids::CHAP_LANGUAGE, &disp.language);
        }
        if let Some(country) = &disp.country {
            write_string_element(&mut display_body, ids::CHAP_COUNTRY, country);
        }
        write_master_element(&mut body, ids::CHAPTER_DISPLAY, &display_body);
    }
    for proc in &ch.chap_processes {
        let mut proc_body = Vec::new();
        // ChapProcessCodecID (§5.1.7.1.4.15) — mandatory, written
        // explicitly even at the `0` default so the round-trip is exact.
        write_uint_element(&mut proc_body, ids::CHAP_PROCESS_CODEC_ID, proc.codec_id);
        if let Some(priv_data) = &proc.private {
            write_bytes_element(&mut proc_body, ids::CHAP_PROCESS_PRIVATE, priv_data);
        }
        for cmd in &proc.commands {
            let mut cmd_body = Vec::new();
            write_uint_element(&mut cmd_body, ids::CHAP_PROCESS_TIME, cmd.time);
            write_bytes_element(&mut cmd_body, ids::CHAP_PROCESS_DATA, &cmd.data);
            write_master_element(&mut proc_body, ids::CHAP_PROCESS_COMMAND, &cmd_body);
        }
        write_master_element(&mut body, ids::CHAP_PROCESS, &proc_body);
    }
    body
}

// --- Attachments ----------------------------------------------------------
//
// One `Attachments` master per file, containing one `AttachedFile` per
// queued entry, in arrival order. Per RFC 9559 §5.1.6.1 the on-disk
// child set is:
//
//   Attachments (0x1941A469)
//     AttachedFile (0x61A7) × N
//       FileDescription (0x467E)   — optional UTF-8, §5.1.6.1.1
//       FileName        (0x466E)   — UTF-8, mandatory, §5.1.6.1.2
//       FileMediaType   (0x4660)   — string (RFC 6838), mandatory, §5.1.6.1.3
//       FileData        (0x465C)   — binary, mandatory, §5.1.6.1.4
//       FileUID         (0x46AE)   — uinteger, mandatory, range: not 0, §5.1.6.1.5
//
// We emit children in the demux side's parse order so the typed
// `Attachment` round-trips field-for-field.

/// Build the bytes of a complete `Attachments` master element from the
/// queued attachment list. Caller appends the returned slice into the
/// muxer's outgoing buffer.
fn build_attachments_element(attachments: &[MkvAttachment]) -> Vec<u8> {
    let mut attachments_body = Vec::new();
    for (i, att) in attachments.iter().enumerate() {
        let index = i as u64 + 1;
        let file_body = build_attached_file(index, att);
        write_master_element(&mut attachments_body, ids::ATTACHED_FILE, &file_body);
    }
    let mut out = Vec::with_capacity(attachments_body.len() + 8 + CRC32_CHILD_LEN);
    write_master_element_with_crc(&mut out, ids::ATTACHMENTS, &attachments_body);
    out
}

/// Body of one `AttachedFile` master (the caller wraps it in
/// `ids::ATTACHED_FILE`). Field order matches the demux side's parse
/// order so a typed [`crate::demux::Attachment`] round-trips through an
/// `MkvAttachment` without losing or reordering fields.
fn build_attached_file(index: u64, att: &MkvAttachment) -> Vec<u8> {
    let mut body = Vec::new();
    if let Some(desc) = att.description.as_deref() {
        if !desc.is_empty() {
            write_string_element(&mut body, ids::FILE_DESCRIPTION, desc);
        }
    }
    write_string_element(&mut body, ids::FILE_NAME, &att.filename);
    write_string_element(&mut body, ids::FILE_MIME_TYPE, &att.mime_type);
    write_bytes_element(&mut body, ids::FILE_DATA, &att.data);
    // Mandatory `range: not 0` UID — auto-derive from the 1-based
    // attachment index when the caller passed `None`. The index is
    // always >= 1, so the resulting UID is always non-zero. The queue
    // method `add_attachment` rejects an explicit `Some(0)` up front.
    let uid = att.uid.unwrap_or(index);
    write_uint_element(&mut body, ids::FILE_UID, uid);
    // Reclaimed DivX-font children (RFC 9559 Appendix A.40..A.42), written
    // only when the caller supplied them so a non-DivX attachment stays
    // clean. `Some(empty)` referral still emits a zero-length element.
    if let Some(referral) = att.referral.as_deref() {
        write_bytes_element(&mut body, ids::FILE_REFERRAL, referral);
    }
    if let Some(t) = att.used_start_time {
        write_uint_element(&mut body, ids::FILE_USED_START_TIME, t);
    }
    if let Some(t) = att.used_end_time {
        write_uint_element(&mut body, ids::FILE_USED_END_TIME, t);
    }
    body
}

// --- Tags -----------------------------------------------------------------
//
// One `Tags` master per file (RFC 9559 §5.1.8, `maxOccurs: 1`), containing
// one `Tag` per queued [`MkvTag`], in arrival order. Per RFC 9559 §5.1.8.1
// the on-disk child set is:
//
//   Tags (0x1254C367)
//     Tag (0x7373) × N
//       Targets (0x63C0)              — mandatory master, §5.1.8.1.1
//         TargetTypeValue (0x68CA)    — uinteger, default 50, range: not 0
//         TargetType      (0x63CA)    — string, informational
//         TagTrackUID     (0x63C5)×M  — uinteger, default 0 (= all)
//         TagEditionUID   (0x63C9)×M
//         TagChapterUID   (0x63C4)×M
//         TagAttachmentUID(0x63C6)×M
//       SimpleTag (0x67C8) × P        — recursive, §5.1.8.1.2
//         TagName         (0x45A3)    — UTF-8, mandatory
//         TagLanguage     (0x447A)    — string, default "und"
//         TagLanguageBCP47(0x447B)    — string, minver 4
//         TagDefault      (0x4484)    — uinteger, default 1
//         TagString       (0x4487)    — UTF-8 \  mutually
//         TagBinary       (0x4485)    — binary /  exclusive
//         SimpleTag       (0x67C8)×Q  — nested children
//
// We emit children in the demux side's parse order so the typed
// [`crate::demux::Tag`] surface round-trips field-for-field.

/// Walk every `SimpleTag` (at every nesting depth) and reject an empty
/// `TagName` — `TagName` is `minOccurs: 1` per RFC 9559 §5.1.8.1.2.1 and
/// the demuxer drops a name-less `SimpleTag` on read, which would break a
/// round-trip silently. Called from [`MkvMuxer::add_tag`] so the caller
/// sees the error at queue time.
fn validate_simple_tags(simple_tags: &[MkvSimpleTag]) -> Result<()> {
    for st in simple_tags {
        if st.name.is_empty() {
            return Err(Error::invalid(
                "MKV muxer: SimpleTag TagName is mandatory (RFC 9559 §5.1.8.1.2.1, minOccurs=1)",
            ));
        }
        validate_simple_tags(&st.children)?;
    }
    Ok(())
}

/// Build the bytes of the file's single `Tags` master element from the
/// queued tag list. Caller appends the returned slice into the muxer's
/// outgoing buffer. The master carries a leading `CRC-32` child like the
/// other Top-Level masters (RFC 9559 §6.2).
fn build_tags_element(tags: &[MkvTag]) -> Vec<u8> {
    let mut tags_body = Vec::new();
    for tag in tags {
        let tag_body = build_tag(tag);
        write_master_element(&mut tags_body, ids::TAG, &tag_body);
    }
    let mut out = Vec::with_capacity(tags_body.len() + 8 + CRC32_CHILD_LEN);
    write_master_element_with_crc(&mut out, ids::TAGS, &tags_body);
    out
}

/// Body of one `Tag` master (the caller wraps it in `ids::TAG`). The
/// `Targets` master is always emitted (§5.1.8.1.1 `minOccurs: 1`), even
/// for a global-scope tag where it is empty.
fn build_tag(tag: &MkvTag) -> Vec<u8> {
    let mut body = Vec::new();
    // Targets master — always present (minOccurs 1). For a global-scope
    // tag the body is empty, which the demuxer reads as global scope.
    let targets_body = build_tag_targets(&tag.targets);
    write_master_element(&mut body, ids::TARGETS, &targets_body);
    for st in &tag.simple_tags {
        let st_body = build_simple_tag(st);
        write_master_element(&mut body, ids::SIMPLE_TAG, &st_body);
    }
    body
}

/// Body of one `Targets` master (the caller wraps it in `ids::TARGETS`).
///
/// Per-element omission rules:
/// * `TargetTypeValue` (§5.1.8.1.1.1) — written only when `Some`. The
///   spec default is `50`; the demuxer surfaces `None` for an absent
///   element (distinct from a materialised `50`), so the muxer writes the
///   element exactly when the caller asked for an explicit value, leaving
///   the default unmaterialised when `None`. `Some(0)` is rejected at
///   queue time (`range: not 0`).
/// * `TargetType` (§5.1.8.1.1.2) — written only when `Some(non-empty)`.
/// * The four UID lists — only non-zero entries are written; a `0` UID
///   means "all of that kind" (§5.1.8.1.1.3..§5.1.8.1.1.6) which is
///   already expressed by omission, so a `0` is dropped.
fn build_tag_targets(t: &MkvTagTargets) -> Vec<u8> {
    let mut body = Vec::new();
    if let Some(ttv) = t.target_type_value {
        write_uint_element(&mut body, ids::TARGET_TYPE_VALUE, ttv);
    }
    if let Some(tt) = t.target_type.as_deref() {
        if !tt.is_empty() {
            write_string_element(&mut body, ids::TARGET_TYPE, tt);
        }
    }
    for &uid in &t.track_uids {
        if uid != 0 {
            write_uint_element(&mut body, ids::TAG_TRACK_UID, uid);
        }
    }
    for &uid in &t.edition_uids {
        if uid != 0 {
            write_uint_element(&mut body, ids::TAG_EDITION_UID, uid);
        }
    }
    for &uid in &t.chapter_uids {
        if uid != 0 {
            write_uint_element(&mut body, ids::TAG_CHAPTER_UID, uid);
        }
    }
    for &uid in &t.attachment_uids {
        if uid != 0 {
            write_uint_element(&mut body, ids::TAG_ATTACHMENT_UID, uid);
        }
    }
    body
}

/// Body of one `SimpleTag` master (the caller wraps it in
/// `ids::SIMPLE_TAG`). Recursive per RFC 9559 §5.1.8.1.2.
///
/// Per-element handling:
/// * `TagName` (§5.1.8.1.2.1) — always written (mandatory). Validated
///   non-empty at queue time.
/// * `TagLanguageBCP47` (§5.1.8.1.2.3) vs `TagLanguage` (§5.1.8.1.2.2) —
///   when a BCP-47 language is present the spec says `TagLanguage` MUST be
///   ignored, so the muxer writes only the BCP-47 element. Otherwise the
///   `TagLanguage` element is written only when it differs from the spec
///   default `"und"`, keeping a default-language tag minimal.
/// * `TagDefault` (§5.1.8.1.2.4) — written only when cleared (`false`),
///   since the spec default is `1` (true).
/// * `TagString` (§5.1.8.1.2.5) / `TagBinary` (§5.1.8.1.2.6) — mutually
///   exclusive; the `None` value writes neither (a name-only / parent
///   node).
/// * Nested `SimpleTag`s last, recursively.
fn build_simple_tag(st: &MkvSimpleTag) -> Vec<u8> {
    let mut body = Vec::new();
    write_string_element(&mut body, ids::TAG_NAME, &st.name);
    match st.language_bcp47.as_deref() {
        Some(bcp) if !bcp.is_empty() => {
            write_string_element(&mut body, ids::TAG_LANGUAGE_BCP47, bcp);
        }
        _ => {
            if !st.language.is_empty() && st.language != "und" {
                write_string_element(&mut body, ids::TAG_LANGUAGE, &st.language);
            }
        }
    }
    if !st.default {
        write_uint_element(&mut body, ids::TAG_DEFAULT, 0);
    }
    match &st.value {
        MkvSimpleTagValue::String(s) => write_string_element(&mut body, ids::TAG_STRING, s),
        MkvSimpleTagValue::Binary(b) => write_bytes_element(&mut body, ids::TAG_BINARY, b),
        MkvSimpleTagValue::None => {}
    }
    for child in &st.children {
        let child_body = build_simple_tag(child);
        write_master_element(&mut body, ids::SIMPLE_TAG, &child_body);
    }
    body
}
