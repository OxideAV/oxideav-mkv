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
//! end-of-file Cues by scanning past the last cluster, and mpv / ffmpeg /
//! Chromium accept the same layout. The SeekHead lets players that prefer
//! up-front index lookup (mpv, Chromium) jump directly to Cues without
//! scanning the whole file; the Cues entry's SeekPosition is patched once
//! the Cues element is actually written (or replaced with a Void if no
//! packets were muxed). Timestamps are converted to milliseconds using the
//! standard 1 ms `TIMECODE_SCALE`.

use std::io::Write;

use oxideav_core::{Error, MediaType, Packet, Result, StreamInfo};
use oxideav_core::{Muxer, WriteSeek};

use crate::codec_id;
use crate::ebml::{write_element_id, write_vint, VINT_UNKNOWN_SIZE};
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
    /// Chapter atoms queued via [`MkvMuxer::add_chapter`] /
    /// [`MkvMuxer::add_chapter_full`]. Materialised into a `Chapters`
    /// master right after `Tracks` in [`MkvMuxer::write_header`]; the
    /// `Chapters` SeekHead entry is patched at the same time. Empty list
    /// → no `Chapters` element written and the SeekHead slot is voided.
    chapters: Vec<MkvChapter>,
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
#[derive(Clone, Debug, Default)]
pub struct MkvChapter {
    /// `ChapterTimeStart`, in nanoseconds.
    pub time_start_ns: u64,
    /// `ChapterTimeEnd`, in nanoseconds. `None` → element omitted.
    pub time_end_ns: Option<u64>,
    /// Zero or more `ChapterDisplay` children. Each one carries one
    /// language-tagged title string. A chapter with zero displays is
    /// legal per RFC 9559 §5.1.7 but produces an "untitled" atom that
    /// most players surface as `Chapter N` — the convenience constructor
    /// [`MkvMuxer::add_chapter`] always emits exactly one display.
    pub display: Vec<ChapterDisplay>,
}

/// One `ChapterDisplay` row — a chapter title in one language.
///
/// `language` follows the `ChapLanguage` element convention (RFC 9559
/// §5.1.7.4.1): 3-letter ISO-639-2 alpha-3 code (`"eng"`, `"jpn"`,
/// `"fre"`, …). Use `"und"` for "undetermined", which is also the
/// default `ChapLanguage` value when the element is omitted entirely.
/// `country`, when set, follows RFC 9559 §5.1.7.4.2 (`ChapCountry`,
/// IETF BCP 47 region subtag, e.g. `"us"`, `"jp"`).
#[derive(Clone, Debug)]
pub struct ChapterDisplay {
    /// `ChapString` — UTF-8 title text.
    pub title: String,
    /// `ChapLanguage` — ISO-639-2 alpha-3 code (e.g. `"eng"`). Pass
    /// `"und"` if no specific language applies.
    pub language: String,
    /// Optional `ChapCountry` — BCP 47 region subtag (e.g. `"us"`).
    /// Skipped when `None`.
    pub country: Option<String>,
}

impl ChapterDisplay {
    /// Convenience constructor: `language` is `"und"`, `country` is `None`.
    pub fn untitled_in(language: impl Into<String>) -> Self {
        Self {
            title: String::new(),
            language: language.into(),
            country: None,
        }
    }
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
            cluster_timecode_ms: 0,
            cluster_offset_rel: 0,
            cluster_body_start_abs: 0,
            segment_data_start: 0,
            cues: Vec::new(),
            cue_seen_in_cluster: vec![false; n],
            seek_cues_entry_offset: 0,
            seek_head_written: false,
            header_written: false,
            trailer_written: false,
            doc_type,
            chapters: Vec::new(),
            lacing_mode: LacingMode::None,
            lace_pending: vec![LaceBuffer::default(); n],
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
            }],
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
        self.chapters.push(chapter);
        Ok(())
    }

    /// Read-only view of the queued chapter list. Useful for tests and
    /// for upstream callers (e.g. DVD-to-MKV) that want to confirm the
    /// chapter table they handed to the muxer before sealing the header.
    pub fn chapters(&self) -> &[MkvChapter] {
        &self.chapters
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
        let mut all = Vec::new();
        write_master_element(&mut all, ids::EBML_HEADER, &ebml_body);

        // Segment with unknown size.
        all.extend_from_slice(&write_element_id(ids::SEGMENT));
        all.extend_from_slice(&write_vint(VINT_UNKNOWN_SIZE, 0));
        // Record the file offset of the Segment payload start — Cues
        // cluster positions are stored as byte offsets from this point.
        let segment_data_start_in_buf = all.len() as u64;

        // SeekHead with four Seek entries (Info, Tracks, Chapters, Cues).
        // Each Seek is written at a fixed width (SeekID 4 bytes,
        // SeekPosition 8 bytes) so we know exactly where to patch in the
        // real positions later. Info and Tracks SeekPositions are filled
        // in below before the buffer is flushed; Chapters is filled in
        // immediately after the Tracks emit (or voided if no chapters
        // were queued); Cues stays as a placeholder zero and gets patched
        // in `write_trailer` (or rewritten as a Void element if no Cues
        // was actually emitted).
        let seek_head_offset_in_buf = all.len() as u64 - segment_data_start_in_buf;
        let seek_head_bytes = build_initial_seek_head();
        let seek_head_start_in_buf = all.len();
        all.extend_from_slice(&seek_head_bytes);
        // Compute where each Seek entry starts inside `all` so we can patch
        // in the real offsets without rebuilding the buffer. The fixed
        // layout is documented in `build_initial_seek_head`: each Seek is
        // exactly `SEEK_ENTRY_LEN` bytes; the SeekPosition payload sits at
        // `entry_start + SEEK_POS_PAYLOAD_OFFSET`.
        let info_seek_entry_in_buf = seek_head_start_in_buf + SEEK_HEAD_HEADER_LEN;
        let tracks_seek_entry_in_buf = info_seek_entry_in_buf + SEEK_ENTRY_LEN;
        let chapters_seek_entry_in_buf = tracks_seek_entry_in_buf + SEEK_ENTRY_LEN;
        let cues_seek_entry_in_buf = chapters_seek_entry_in_buf + SEEK_ENTRY_LEN;
        // Sanity: SeekHead occupies a known total size; the next element
        // starts immediately after.
        debug_assert_eq!(seek_head_bytes.len(), SEEK_HEAD_TOTAL_LEN);
        let _ = seek_head_offset_in_buf; // SeekHead always sits at offset 0 — kept for clarity.

        // Info element.
        let info_offset_in_buf = all.len() as u64 - segment_data_start_in_buf;
        let mut info_body = Vec::new();
        write_uint_element(&mut info_body, ids::TIMECODE_SCALE, 1_000_000); // 1 ms
        write_string_element(&mut info_body, ids::MUXING_APP, "oxideav");
        write_string_element(&mut info_body, ids::WRITING_APP, "oxideav");
        write_master_element(&mut all, ids::INFO, &info_body);

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
            let flag_lacing = if self.lacing_mode == LacingMode::None {
                0
            } else {
                1
            };
            write_uint_element(&mut t, ids::FLAG_LACING, flag_lacing);
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
            // Codec-specific timing fields (Opus uses CodecDelay = pre_skip in ns
            // and a recommended SeekPreRoll of 80 ms).
            if s.params.codec_id.as_str() == "opus" {
                let pre_skip_samples = parse_opus_pre_skip(&s.params.extradata);
                let codec_delay_ns = pre_skip_samples as u64 * 1_000_000_000 / 48_000;
                write_uint_element(&mut t, ids::CODEC_DELAY, codec_delay_ns);
                write_uint_element(&mut t, ids::SEEK_PRE_ROLL, 80_000_000);
            }
            if s.params.media_type == MediaType::Audio {
                let mut audio = Vec::new();
                if let Some(sr) = s.params.sample_rate {
                    write_float_element(&mut audio, ids::SAMPLING_FREQUENCY, sr as f64);
                }
                if let Some(ch) = s.params.channels {
                    write_uint_element(&mut audio, ids::CHANNELS, ch as u64);
                }
                if let Some(fmt) = s.params.sample_format {
                    let bd = (fmt.bytes_per_sample() * 8) as u64;
                    write_uint_element(&mut audio, ids::BIT_DEPTH, bd);
                }
                write_master_element(&mut t, ids::AUDIO, &audio);
            }
            if s.params.media_type == MediaType::Video {
                let mut video = Vec::new();
                if let Some(w) = s.params.width {
                    write_uint_element(&mut video, ids::PIXEL_WIDTH, w as u64);
                }
                if let Some(h) = s.params.height {
                    write_uint_element(&mut video, ids::PIXEL_HEIGHT, h as u64);
                }
                write_master_element(&mut t, ids::VIDEO, &video);
            }
            write_master_element(&mut tracks_body, ids::TRACK_ENTRY, &t);
        }
        write_master_element(&mut all, ids::TRACKS, &tracks_body);

        // Chapters (optional). If `add_chapter` calls were made before
        // `write_header`, materialise them now as a single EditionEntry
        // master sandwiched between Tracks and the first Cluster. RFC
        // 9559 §5.1.7 lets Chapters appear anywhere in the Segment, but
        // putting it here keeps the demuxer's pre-Cluster header walk
        // single-pass and matches the order ffmpeg / mkvmerge prefer.
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

        // Patch the Info / Tracks SeekPositions in the SeekHead now that we
        // know where each element landed inside `all`. Cues stays as zero
        // and is patched in `write_trailer`.
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

        self.segment_data_start = base_pos + segment_data_start_in_buf;
        // Absolute file offset of the Cues Seek entry — used in
        // write_trailer to patch in the real Cues offset (or rewrite the
        // 21-byte slot as a Void element when no Cues was emitted).
        self.seek_cues_entry_offset = base_pos + cues_seek_entry_in_buf as u64;
        self.seek_head_written = true;
        self.output.write_all(&all)?;
        self.header_written = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
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
        // SimpleBlock KEY bit applies to the whole Block.
        if self.lacing_mode != LacingMode::None {
            for other_idx in 0..self.lace_pending.len() {
                if other_idx != stream_idx && !self.lace_pending[other_idx].frames.is_empty() {
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

        // Cue index: record the first indexable packet per (cluster, track).
        // For video we only index keyframes (random-access points). For
        // audio/subtitle we index the cluster-start regardless, since every
        // audio frame is independently decodable.
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
        if !self.cue_seen_in_cluster[stream_idx] {
            let indexable = match media_type {
                MediaType::Video => packet.flags.keyframe,
                _ => true,
            };
            if indexable && self.lacing_mode == LacingMode::None {
                // Non-lacing path: the block lands at `pre_block_pos`,
                // emit the cue now with the correct relative position.
                self.cues.push(CueRecord {
                    track: track_number,
                    time_ms: pts_ms.max(0) as u64,
                    cluster_offset: self.cluster_offset_rel,
                    relative_position: pre_block_rel,
                });
                self.cue_seen_in_cluster[stream_idx] = true;
            }
            // Lacing path: cue emission happens in `flush_lace` once we
            // actually know where the (possibly laced) block lands.
        }

        if self.lacing_mode == LacingMode::None {
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
        } else {
            self.append_to_lace(stream_idx, timecode_offset as i16, packet)?;
        }
        Ok(())
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
        let cues_offset_rel = self.write_cues()?;
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
        self.cluster_timecode_ms = timecode_ms.max(0);
        self.cluster_open = true;
        // New cluster → clear the "already cued this track" flags.
        for s in self.cue_seen_in_cluster.iter_mut() {
            *s = false;
        }
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
                write_master_element(&mut cp, ids::CUE_TRACK_POSITIONS, &ctp);
            }
            write_master_element(&mut body, ids::CUE_POINT, &cp);
        }
        let mut out = Vec::with_capacity(body.len() + 8);
        write_master_element(&mut out, ids::CUES, &body);
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
                });
                self.cue_seen_in_cluster[stream_idx] = true;
            }
        }
        let block_bytes = build_simple_block(track_number, tc_offset, keyframe, mode, &frames);
        self.output.write_all(&block_bytes)?;
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
        // CodecPrivate even though many docs imply it's optional. ffmpeg
        // expects it; we always prepend it on the muxer side.
        "flac" => {
            let mut out = Vec::with_capacity(4 + extradata.len());
            out.extend_from_slice(b"fLaC");
            out.extend_from_slice(extradata);
            out
        }
        _ => extradata.to_vec(),
    }
}

// --- Element-writing helpers ----------------------------------------------

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
/// the first Seek child. `4 + 1 = 5` for our 84-byte body (4 × 21).
const SEEK_HEAD_HEADER_LEN: usize = 5;
/// Number of Seek entries the SeekHead reserves: Info, Tracks, Chapters,
/// Cues. Chapters and Cues are voided in `write_header` /
/// `write_trailer` respectively when those elements turn out to be empty.
const SEEK_HEAD_ENTRY_COUNT: usize = 4;
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
/// Tracks, Chapters, and Cues. The caller patches in the real positions
/// via `write_u64_be_at` once each element's offset is known (or rewrites
/// the slot as a Void if the element ends up not being emitted).
fn build_initial_seek_head() -> Vec<u8> {
    let mut body = Vec::with_capacity(SEEK_HEAD_ENTRY_COUNT * SEEK_ENTRY_LEN);
    body.extend_from_slice(&seek_entry(ids::INFO, 0));
    body.extend_from_slice(&seek_entry(ids::TRACKS, 0));
    body.extend_from_slice(&seek_entry(ids::CHAPTERS, 0));
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
    let mut out = Vec::with_capacity(chapters_body.len() + 8);
    write_master_element(&mut out, ids::CHAPTERS, &chapters_body);
    out
}

/// Body of one `ChapterAtom` master (the caller wraps it in
/// `ids::CHAPTER_ATOM`).
fn build_chapter_atom(uid: u64, ch: &MkvChapter) -> Vec<u8> {
    let mut body = Vec::new();
    write_uint_element(&mut body, ids::CHAPTER_UID, uid);
    write_uint_element(&mut body, ids::CHAPTER_TIME_START, ch.time_start_ns);
    if let Some(end) = ch.time_end_ns {
        write_uint_element(&mut body, ids::CHAPTER_TIME_END, end);
    }
    for disp in &ch.display {
        let mut display_body = Vec::new();
        write_string_element(&mut display_body, ids::CHAP_STRING, &disp.title);
        write_string_element(&mut display_body, ids::CHAP_LANGUAGE, &disp.language);
        if let Some(country) = &disp.country {
            write_string_element(&mut display_body, ids::CHAP_COUNTRY, country);
        }
        write_master_element(&mut body, ids::CHAPTER_DISPLAY, &display_body);
    }
    body
}
