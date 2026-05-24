//! Matroska demuxer.
//!
//! Strategy: read the EBML header, locate the Segment, parse Info + Tracks
//! up front. Then on each `next_packet` call, walk Cluster children one at a
//! time, extracting frames from `SimpleBlock` and `BlockGroup → Block`
//! elements (lacing-aware).

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecParameters, CodecResolver, CodecTag, Error, MediaType, Packet, ProbeContext, Result,
    SampleFormat, StreamInfo, TimeBase,
};
use oxideav_core::{Demuxer, ReadSeek};

use crate::codec_id::{from_matroska, strip_bitmapinfoheader};
use crate::ebml::{
    crc32_ieee, read_bytes, read_element_header, read_float, read_string, read_uint, read_vint,
    skip, VINT_UNKNOWN_SIZE,
};
use crate::ids;

pub fn open(input: Box<dyn ReadSeek>, codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    open_typed(input, codecs).map(|d| Box::new(d) as Box<dyn Demuxer>)
}

/// Concrete-typed variant of [`open`] returning [`MkvDemuxer`] directly,
/// so callers can reach typed accessors like [`MkvDemuxer::tags`] that
/// the [`Demuxer`] trait does not expose. Same parsing contract as
/// [`open`] — the trait-returning wrapper is implemented in terms of
/// this one.
pub fn open_typed(mut input: Box<dyn ReadSeek>, codecs: &dyn CodecResolver) -> Result<MkvDemuxer> {
    // Validate EBML header.
    let hdr = read_element_header(&mut *input)?;
    if hdr.id != ids::EBML_HEADER {
        return Err(Error::invalid(format!(
            "MKV: expected EBML header at start, got id 0x{:X}",
            hdr.id
        )));
    }
    let mut doc_type = String::from("matroska");
    let ebml_end = input.stream_position()? + hdr.size;
    while input.stream_position()? < ebml_end {
        let e = read_element_header(&mut *input)?;
        match e.id {
            ids::EBML_DOC_TYPE => {
                doc_type = read_string(&mut *input, e.size as usize)?;
            }
            _ => skip(&mut *input, e.size)?,
        }
    }
    if doc_type != "matroska" && doc_type != "webm" {
        return Err(Error::unsupported(format!(
            "MKV: unsupported DocType '{doc_type}'"
        )));
    }

    // Find Segment.
    let seg = read_element_header(&mut *input)?;
    if seg.id != ids::SEGMENT {
        return Err(Error::invalid(format!(
            "MKV: expected Segment after EBML header, got id 0x{:X}",
            seg.id
        )));
    }
    let segment_data_start = input.stream_position()?;
    let segment_data_end = if seg.size == VINT_UNKNOWN_SIZE {
        // Unknown segment size — use file end.
        let cur = input.stream_position()?;
        let end = input.seek(SeekFrom::End(0))?;
        input.seek(SeekFrom::Start(cur))?;
        end
    } else {
        segment_data_start + seg.size
    };

    // Walk segment children, recording where Tracks/Info/Cluster live.
    let mut info = SegmentInfo::default();
    let mut tracks: Vec<TrackEntry> = Vec::new();
    let mut first_cluster_offset: Option<u64> = None;
    let mut metadata: Vec<(String, String)> = Vec::new();
    let mut cues: Vec<CueEntry> = Vec::new();
    // Chapter / attachment / edition UID maps populated during the segment
    // walk, then consulted when we resolve `Tags.Targets` UIDs at the very
    // end. Tags can appear before *or* after Tracks/Chapters/Attachments per
    // RFC 9559, so we defer resolution until the whole segment has been
    // walked. Map values are 1-indexed to match the public `chapter:N:*` /
    // `attachment:N:*` metadata keys.
    let mut chapter_uid_to_index: std::collections::HashMap<u64, u32> =
        std::collections::HashMap::new();
    let mut attachment_uid_to_index: std::collections::HashMap<u64, u32> =
        std::collections::HashMap::new();
    let mut edition_uid_to_index: std::collections::HashMap<u64, u32> =
        std::collections::HashMap::new();
    let mut pending_tags: Vec<RawTag> = Vec::new();
    // Typed `Chapters` tree (RFC 9559 §5.1.7), surfaced via
    // `MkvDemuxer::chapters` — populated by `parse_chapters_typed` alongside
    // the flat metadata view.
    let mut editions: Vec<Edition> = Vec::new();
    // Per-element CRC-32 validation results (RFC 8794 §11.3.1, RFC 9559
    // §6.2). Populated as each Top-Level master with a leading CRC-32
    // child is walked; surfaced via `MkvDemuxer::crc_status`.
    let mut crc_status: Vec<CrcStatus> = Vec::new();

    while input.stream_position()? < segment_data_end {
        let e = read_element_header(&mut *input)?;
        let body_start = input.stream_position()?;
        let body_end_known = if e.size == VINT_UNKNOWN_SIZE {
            None
        } else {
            Some(body_start + e.size)
        };
        // Validate a leading CRC-32 child against the rest of the element
        // when the element size is known (CRC needs a bounded body). The
        // helper rewinds the reader to `body_start` so the parse below is
        // unaffected.
        if let Some(end) = body_end_known {
            if matches!(
                e.id,
                ids::INFO
                    | ids::TRACKS
                    | ids::TAGS
                    | ids::CUES
                    | ids::CHAPTERS
                    | ids::ATTACHMENTS
                    | ids::SEEK_HEAD
            ) {
                if let Some(s) = validate_top_level_crc(&mut *input, e.id, body_start, end)? {
                    crc_status.push(s);
                }
            }
        }
        match e.id {
            ids::INFO => {
                let end = body_end_known.unwrap_or(segment_data_end);
                parse_info(&mut *input, end, &mut info, &mut metadata)?;
            }
            ids::TRACKS => {
                let end = body_end_known.unwrap_or(segment_data_end);
                parse_tracks(&mut *input, end, &mut tracks)?;
            }
            ids::TAGS => {
                let end = body_end_known.unwrap_or(segment_data_end);
                parse_tags(&mut *input, end, &mut pending_tags)?;
            }
            ids::CUES => {
                let end = body_end_known.unwrap_or(segment_data_end);
                parse_cues(&mut *input, end, &mut cues)?;
            }
            ids::CHAPTERS => {
                let end = body_end_known.unwrap_or(segment_data_end);
                parse_chapters_typed(
                    &mut *input,
                    end,
                    &mut metadata,
                    &mut chapter_uid_to_index,
                    &mut edition_uid_to_index,
                    &mut editions,
                )?;
            }
            ids::ATTACHMENTS => {
                let end = body_end_known.unwrap_or(segment_data_end);
                parse_attachments(
                    &mut *input,
                    end,
                    &mut metadata,
                    &mut attachment_uid_to_index,
                )?;
            }
            ids::CLUSTER => {
                if first_cluster_offset.is_none() {
                    first_cluster_offset = Some(body_start - e.header_len as u64);
                }
                input.seek(SeekFrom::Start(body_start - e.header_len as u64))?;
                break;
            }
            _ => {
                if let Some(end) = body_end_known {
                    input.seek(SeekFrom::Start(end))?;
                } else {
                    return Err(Error::unsupported(
                        "MKV: unknown-size element other than Cluster",
                    ));
                }
            }
        }
    }

    // Cues are often written after the final Cluster — if we haven't seen
    // them yet and the segment size is known, scan from the first cluster
    // to segment end looking for a top-level Cues element. We keep this
    // best-effort: any I/O error or parse problem leaves `cues` empty
    // and falls back to Unsupported at seek time.
    if cues.is_empty() {
        if let Some(first_cluster) = first_cluster_offset {
            let resume_pos = input.stream_position()?;
            if scan_cues_from(&mut *input, first_cluster, segment_data_end, &mut cues).is_err() {
                cues.clear();
            }
            // Restore reader position to the first cluster for next_packet().
            input.seek(SeekFrom::Start(resume_pos))?;
        }
    }

    // Sort cues by (track, time) for stable lookup.
    cues.sort_by(|a, b| a.track.cmp(&b.track).then(a.time.cmp(&b.time)));

    if tracks.is_empty() {
        return Err(Error::invalid("MKV: no tracks found"));
    }

    // Use 1ms timebase if not specified (default Matroska timecode_scale = 1_000_000 ns).
    let timecode_scale_ns = if info.timecode_scale == 0 {
        1_000_000
    } else {
        info.timecode_scale
    };
    // For simplicity expose every stream with the segment time base = scale/1e9 seconds per tick.
    // 1 tick = timecode_scale_ns nanoseconds. So time base = timecode_scale_ns / 1_000_000_000.
    let time_base = TimeBase::new(timecode_scale_ns as i64, 1_000_000_000);

    // Build public StreamInfo list, preserving the input track-number → output index mapping.
    let mut streams: Vec<StreamInfo> = Vec::new();
    let mut track_index_by_number: std::collections::HashMap<u64, u32> =
        std::collections::HashMap::new();
    for t in &tracks {
        let idx = streams.len() as u32;
        track_index_by_number.insert(t.number, idx);
        // Ask the CodecResolver registry first (codec crates can claim
        // Matroska CodecID strings). Fall back to the static `from_matroska`
        // table when no crate owns this id — keeps PCM, legacy MS/VFW
        // FourCC tracks, WebM-specific VP tags, etc. working unchanged.
        let tag = CodecTag::matroska(t.codec_id_string.clone());
        let mut ctx = ProbeContext::new(&tag);
        if !t.codec_private.is_empty() {
            ctx = ctx.header(&t.codec_private);
        }
        if t.bit_depth > 0 {
            ctx = ctx.bits(t.bit_depth as u16);
        }
        if t.channels > 0 {
            ctx = ctx.channels(t.channels as u16);
        }
        let sr = t.sample_rate.round() as u32;
        if sr > 0 {
            ctx = ctx.sample_rate(sr);
        }
        if t.width > 0 {
            ctx = ctx.width(t.width as u32);
        }
        if t.height > 0 {
            ctx = ctx.height(t.height as u32);
        }
        let mut codec_id = codecs.resolve_tag(&ctx);
        // V_MS/VFW/FOURCC tunnels a BITMAPINFOHEADER in CodecPrivate. The
        // registry has no "Matroska" tag for this case (every codec claims
        // the inner FourCC directly — that's how AVI resolves the same
        // stream). Extract the FourCC from CodecPrivate bytes 16..20 and
        // retry via the Fourcc tag path.
        if codec_id.is_none()
            && t.codec_id_string == "V_MS/VFW/FOURCC"
            && t.codec_private.len() >= 20
        {
            let mut fcc = [0u8; 4];
            fcc.copy_from_slice(&t.codec_private[16..20]);
            let fcc_tag = CodecTag::fourcc(&fcc);
            let mut fcc_ctx = ProbeContext::new(&fcc_tag).header(&t.codec_private);
            if t.width > 0 {
                fcc_ctx = fcc_ctx.width(t.width as u32);
            }
            if t.height > 0 {
                fcc_ctx = fcc_ctx.height(t.height as u32);
            }
            codec_id = codecs.resolve_tag(&fcc_ctx);
        }
        let codec_id =
            codec_id.unwrap_or_else(|| from_matroska(&t.codec_id_string, &t.codec_private));
        let mut params = match t.track_type {
            ids::TRACK_TYPE_VIDEO => CodecParameters::video(codec_id.clone()),
            ids::TRACK_TYPE_AUDIO => CodecParameters::audio(codec_id.clone()),
            ids::TRACK_TYPE_SUBTITLE => CodecParameters::subtitle(codec_id.clone()),
            _ => {
                // Unknown TrackType (button, control, etc.) — fall back to
                // an opaque Data stream so the demuxer doesn't reject the
                // file outright.
                let mut p = CodecParameters::audio(codec_id.clone());
                p.media_type = MediaType::Data;
                p
            }
        };
        // Codec-specific CodecPrivate normalisation:
        //   * `V_MS/VFW/FOURCC`: the outer 40-byte BITMAPINFOHEADER wraps
        //     real codec extradata — strip it so decoders see their own
        //     config record.
        //   * `A_FLAC`: the CodecPrivate sometimes has a leading `"fLaC"`
        //     magic; our FLAC decoder expects metadata blocks only.
        let stripped = strip_bitmapinfoheader(&t.codec_id_string, &t.codec_private);
        params.extradata = match codec_id.as_str() {
            "flac" if stripped.starts_with(b"fLaC") => stripped[4..].to_vec(),
            _ => stripped,
        };
        if t.track_type == ids::TRACK_TYPE_AUDIO {
            params.sample_rate = Some(t.sample_rate.round() as u32);
            params.channels = Some(t.channels as u16);
            params.sample_format = match (params.codec_id.as_str(), t.bit_depth) {
                ("pcm_s16le", _) => Some(SampleFormat::S16),
                ("pcm_s16be", _) => Some(SampleFormat::S16),
                ("pcm_f32le", _) => Some(SampleFormat::F32),
                ("flac", 8) => Some(SampleFormat::U8),
                ("flac", 16) => Some(SampleFormat::S16),
                ("flac", 24) => Some(SampleFormat::S24),
                ("flac", 32) => Some(SampleFormat::S32),
                _ => None,
            };
        }
        if t.track_type == ids::TRACK_TYPE_VIDEO {
            params.width = Some(t.width as u32);
            params.height = Some(t.height as u32);
        }
        streams.push(StreamInfo {
            index: idx,
            time_base,
            duration: if info.duration > 0.0 {
                Some(info.duration as i64)
            } else {
                None
            },
            start_time: Some(0),
            params,
        });
    }

    // Resolve `Tags.Targets.Tag*UID` references now that the full segment
    // has been walked. Tags appearing before Tracks in segment order are
    // valid per RFC 9559, so this has to happen after the loop above.
    let track_uid_to_index: std::collections::HashMap<u64, u32> = tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.uid != 0)
        .map(|(i, t)| (t.uid, i as u32))
        .collect();
    let mut typed_tags: Vec<Tag> = Vec::new();
    resolve_tags(
        pending_tags,
        &track_uid_to_index,
        &chapter_uid_to_index,
        &attachment_uid_to_index,
        &edition_uid_to_index,
        &mut metadata,
        &mut typed_tags,
    );

    // Resolve each track's `TrackOperation` (RFC 9559 §5.1.4.1.30): map the
    // raw `TrackPlaneUID` / `TrackJoinUID` references onto stream indices via
    // the same `TrackUID -> index` table the tag resolver uses. Indexed by
    // stream index so the typed accessor is a direct lookup.
    let resolve_ref = |uid: u64| TrackRef {
        track_uid: uid,
        stream_index: track_uid_to_index.get(&uid).copied(),
    };
    let track_operations: Vec<Option<TrackOperation>> = tracks
        .iter()
        .map(|t| {
            t.track_operation.as_ref().map(|raw| TrackOperation {
                planes: raw
                    .planes
                    .iter()
                    .map(|&(uid, ty)| TrackPlane {
                        track: resolve_ref(uid),
                        plane_type: TrackPlaneType::from_raw(ty),
                    })
                    .collect(),
                join_tracks: raw.join_uids.iter().map(|&uid| resolve_ref(uid)).collect(),
            })
        })
        .collect();

    // Per-stream `ContentEncodings` (RFC 9559 §5.1.4.1.31), indexed by
    // stream index. No UID resolution needed — encodings are self-contained.
    let content_encodings: Vec<Option<ContentEncodings>> =
        tracks.iter().map(|t| t.content_encodings.clone()).collect();

    // Per-stream Header-Stripping prefix (RFC 9559 §5.1.4.1.31.6 algo 3): the
    // bytes to prepend to each de-laced frame to undo a Block-scoped
    // Header-Stripping chain. Empty when there's nothing to undo or the chain
    // contains a step the container can't reverse — see
    // `compute_header_strip_prefix`.
    let header_strip_prefixes: Vec<Vec<u8>> = content_encodings
        .iter()
        .map(|ce| {
            ce.as_ref()
                .and_then(compute_header_strip_prefix)
                .unwrap_or_default()
        })
        .collect();

    // Position at the first Cluster.
    let cluster_pos = first_cluster_offset.ok_or_else(|| Error::invalid("MKV: no clusters"))?;
    input.seek(SeekFrom::Start(cluster_pos))?;

    // Segment\Info\Duration is in Matroska timecode ticks (timecode_scale ns
    // per tick), stored as a float. Translate to microseconds.
    let duration_micros: i64 = if info.duration > 0.0 {
        (info.duration * (timecode_scale_ns as f64) / 1_000.0) as i64
    } else {
        0
    };

    // Build reverse map: stream index → MKV TrackNumber.
    let mut track_number_by_index: Vec<u64> = vec![0; streams.len()];
    for (num, &idx) in &track_index_by_number {
        track_number_by_index[idx as usize] = *num;
    }

    Ok(MkvDemuxer {
        input,
        streams,
        track_index_by_number,
        track_number_by_index,
        segment_data_start,
        segment_data_end,
        cluster_state: ClusterState::Idle,
        out_queue: std::collections::VecDeque::new(),
        time_base,
        metadata,
        duration_micros,
        cues,
        timecode_scale_ns,
        tags: typed_tags,
        editions,
        crc_status,
        track_operations,
        content_encodings,
        header_strip_prefixes,
    })
}

#[derive(Default)]
struct SegmentInfo {
    timecode_scale: u64,
    duration: f64,
}

/// Result of validating the `CRC-32` element (RFC 8794 §11.3.1) on one
/// Top-Level master element of the Segment.
///
/// In Matroska, every Top-Level master element SHOULD carry a `CRC-32`
/// child as its first element (RFC 9559 §6.2). The demuxer checks each
/// such element it parses up front (Info, Tracks, Tags, Cues, Chapters,
/// Attachments, SeekHead) and records whether the stored CRC matched the
/// IEEE CRC-32 of the rest of the element's data. Elements with no
/// `CRC-32` child are not represented here — absence of a status means
/// "no CRC to check," which the spec explicitly permits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CrcStatus {
    /// EBML element ID of the Top-Level master that carried the `CRC-32`
    /// child (e.g. [`ids::TRACKS`], [`ids::INFO`]).
    pub element_id: u32,
    /// CRC-32 value stored in the file (little-endian decoded).
    pub stored: u32,
    /// CRC-32 the demuxer computed over the element's remaining data.
    pub computed: u32,
}

impl CrcStatus {
    /// True when the stored CRC matched the recomputed one — i.e. the
    /// element's data is intact per its own checksum.
    pub fn is_valid(&self) -> bool {
        self.stored == self.computed
    }
}

/// Check a Top-Level master element for a leading `CRC-32` child and, if
/// present, validate it. `body_start` / `body_end` bracket the element's
/// data (the bytes after its EBML header). On return the reader is left at
/// `body_start` so the caller's normal parse can proceed unchanged.
///
/// Per RFC 8794 §11.3.1 the `CRC-32` element, if used, MUST be the first
/// ordered child of its parent, and its 4-byte value is the IEEE CRC-32 of
/// all the parent's Element Data *except* the `CRC-32` element itself,
/// computed and stored little-endian. So we read the body once, peel off a
/// leading `CRC-32` child if there is one, and CRC the remainder.
///
/// Returns `Ok(None)` when the element has no leading `CRC-32` child (the
/// common, spec-permitted case) and `Ok(Some(status))` when one was found
/// and checked. Any short read leaves the reader rewound and yields
/// `Ok(None)` rather than failing the whole open — a torn checksum should
/// not make an otherwise-readable file un-demuxable.
fn validate_top_level_crc(
    r: &mut dyn ReadSeek,
    element_id: u32,
    body_start: u64,
    body_end: u64,
) -> Result<Option<CrcStatus>> {
    let len = body_end.saturating_sub(body_start);
    // A CRC-32 child is 2 header bytes (id 0xBF + size 0x84) + 4 value
    // bytes. Anything shorter cannot carry one.
    if len < 6 {
        r.seek(SeekFrom::Start(body_start))?;
        return Ok(None);
    }
    r.seek(SeekFrom::Start(body_start))?;
    let body = read_bytes(r, len as usize)?;
    // Always rewind for the caller before returning, regardless of outcome.
    r.seek(SeekFrom::Start(body_start))?;

    let mut cur = std::io::Cursor::new(&body[..]);
    let (id, _) = match read_vint(&mut cur, true) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if id != ids::CRC32 as u64 {
        return Ok(None);
    }
    let (size, _) = match read_vint(&mut cur, false) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if size != 4 {
        // A CRC-32 element is fixed at 4 bytes; a different size is
        // malformed — treat as "no CRC to check" rather than erroring.
        return Ok(None);
    }
    let header_len = cur.position() as usize;
    if header_len + 4 > body.len() {
        return Ok(None);
    }
    let stored = u32::from_le_bytes([
        body[header_len],
        body[header_len + 1],
        body[header_len + 2],
        body[header_len + 3],
    ]);
    let rest = &body[header_len + 4..];
    let computed = crc32_ieee(rest);
    Ok(Some(CrcStatus {
        element_id,
        stored,
        computed,
    }))
}

#[derive(Default)]
struct TrackEntry {
    number: u64,
    /// `TrackUID` (RFC 9559 §5.1.4.1.2); needed for resolving
    /// `Tags.Targets.TagTrackUID` references back to a stream index.
    /// Zero means "not present in the file" which is technically illegal
    /// (TrackUID is mandatory) but we tolerate it.
    uid: u64,
    track_type: u64,
    codec_id_string: String,
    codec_private: Vec<u8>,
    sample_rate: f64,
    channels: u64,
    bit_depth: u64,
    width: u64,
    height: u64,
    /// Raw `TrackOperation` (RFC 9559 §5.1.4.1.30) captured during the
    /// segment walk. Its `TrackPlaneUID` / `TrackJoinUID` references point
    /// at other tracks by `TrackUID`; they're resolved to stream indices
    /// after the full Tracks list is known. `None` when the TrackEntry has
    /// no `TrackOperation` child (the common case for ordinary tracks).
    track_operation: Option<RawTrackOperation>,
    /// `ContentEncodings` (RFC 9559 §5.1.4.1.31) for the track, parsed in
    /// on-disk order. Sorted into decode order (descending
    /// `ContentEncodingOrder`) when surfaced. `None` when the track declares
    /// no encodings (the common, uncompressed/unencrypted case).
    content_encodings: Option<ContentEncodings>,
}

/// Parser-private staging form of `TrackOperation` — the plane / join
/// references are still raw `TrackUID`s here. Resolved into the public
/// [`TrackOperation`] (with `TrackUID` -> stream-index mapping) once the
/// whole Tracks list has been parsed.
#[derive(Default)]
struct RawTrackOperation {
    /// `(TrackPlaneUID, TrackPlaneType)` pairs from `TrackCombinePlanes`
    /// (RFC 9559 §5.1.4.1.30.1). A `TrackPlane` with no `TrackPlaneUID`
    /// is illegal (minOccurs=1) and dropped.
    planes: Vec<(u64, u64)>,
    /// `TrackJoinUID`s from `TrackJoinBlocks` (RFC 9559 §5.1.4.1.30.5).
    join_uids: Vec<u64>,
}

fn parse_info(
    r: &mut dyn ReadSeek,
    end: u64,
    out: &mut SegmentInfo,
    metadata: &mut Vec<(String, String)>,
) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::TIMECODE_SCALE => out.timecode_scale = read_uint(r, e.size as usize)?,
            ids::DURATION => out.duration = read_float(r, e.size as usize)?,
            ids::TITLE => {
                let s = read_string(r, e.size as usize)?;
                if !s.is_empty() {
                    metadata.push(("title".into(), s));
                }
            }
            ids::MUXING_APP => {
                let s = read_string(r, e.size as usize)?;
                if !s.is_empty() {
                    metadata.push(("muxer".into(), s));
                }
            }
            ids::WRITING_APP => {
                let s = read_string(r, e.size as usize)?;
                if !s.is_empty() {
                    metadata.push(("encoder".into(), s));
                }
            }
            ids::DATE_UTC => {
                // 8-byte signed integer: nanoseconds since 2001-01-01 00:00:00 UTC.
                if e.size == 8 {
                    let ns = read_uint(r, 8)? as i64;
                    let secs_since_2001 = ns / 1_000_000_000;
                    let unix_2001: i64 = 978_307_200;
                    let unix = unix_2001 + secs_since_2001;
                    metadata.push(("date".into(), format_iso8601(unix)));
                } else {
                    skip(r, e.size)?;
                }
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

/// A `Tags.Tag` element captured during the segment walk, before its
/// `Targets.Tag*UID` references have been resolved against the corresponding
/// tracks / chapters / attachments. Each tag emits one or more `SimpleTag`
/// entries, all of which share the same `Targets` scope.
///
/// We keep this as a parser-private staging type and translate it into the
/// public [`Tag`] / [`Targets`] / [`SimpleTag`] family during `resolve_tags`
/// — that pass also drops tags whose UID is non-zero but doesn't point at
/// any element in this Segment, per RFC 9559 §5.1.8.1.1.3..§5.1.8.1.1.6.
struct RawTag {
    /// Zero, one, or many TrackUID references. RFC 9559 §5.1.8.1.1.3 marks
    /// `TagTrackUID` with no maxOccurs cap, so multiple per-Targets is
    /// legal — typically used to scope a tag like ARTIST to a chosen set
    /// of audio tracks within a multi-track Segment.
    track_uids: Vec<u64>,
    edition_uids: Vec<u64>,
    chapter_uids: Vec<u64>,
    attachment_uids: Vec<u64>,
    /// Optional `TargetTypeValue` (RFC 9559 §5.1.8.1.1.1, default 50) and
    /// `TargetType` informational string (§5.1.8.1.1.2). Both are kept as
    /// captured — the typed [`Targets`] surface lets consumers decide
    /// whether to filter on them.
    target_type_value: Option<u64>,
    target_type: Option<String>,
    /// Parsed `SimpleTag` children, including language / default / binary
    /// fields that the legacy `(name, value)` summary throws away.
    simple_tags: Vec<RawSimpleTag>,
}

/// Mirror of a `SimpleTag` element (RFC 9559 §5.1.8.1.2) as parsed from
/// the file. Translates 1:1 to the public [`SimpleTag`] via `From`.
#[derive(Default)]
struct RawSimpleTag {
    name: String,
    value: SimpleTagValue,
    /// `TagLanguage` (RFC 9559 §5.1.8.1.2.2) — three-letter Matroska code,
    /// default `"und"`.
    language: String,
    /// `TagLanguageBCP47` (RFC 9559 §5.1.8.1.2.3) — RFC 5646 tag. When
    /// present, `language` MUST be ignored per the spec; we keep both and
    /// let consumers pick.
    language_bcp47: Option<String>,
    /// `TagDefault` (RFC 9559 §5.1.8.1.2.4) — default 1.
    default: bool,
}

fn parse_tags(r: &mut dyn ReadSeek, end: u64, out: &mut Vec<RawTag>) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::TAG => {
                let tag_end = r.stream_position()? + e.size;
                let mut t = RawTag {
                    track_uids: Vec::new(),
                    edition_uids: Vec::new(),
                    chapter_uids: Vec::new(),
                    attachment_uids: Vec::new(),
                    target_type_value: None,
                    target_type: None,
                    simple_tags: Vec::new(),
                };
                parse_tag(r, tag_end, &mut t)?;
                if !t.simple_tags.is_empty() {
                    out.push(t);
                }
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

fn parse_tag(r: &mut dyn ReadSeek, end: u64, t: &mut RawTag) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::TARGETS => {
                let tg_end = r.stream_position()? + e.size;
                parse_targets(r, tg_end, t)?;
            }
            ids::SIMPLE_TAG => {
                let st_end = r.stream_position()? + e.size;
                let mut s = RawSimpleTag {
                    name: String::new(),
                    value: SimpleTagValue::None,
                    language: String::from("und"),
                    language_bcp47: None,
                    default: true,
                };
                parse_simple_tag(r, st_end, &mut s)?;
                // Drop SimpleTags with no name — they're malformed per
                // RFC 9559 §5.1.8.1.2.1 (TagName has minOccurs 1).
                if !s.name.is_empty() {
                    t.simple_tags.push(s);
                }
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

fn parse_targets(r: &mut dyn ReadSeek, end: u64, t: &mut RawTag) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::TAG_TRACK_UID => {
                let v = read_uint(r, e.size as usize)?;
                t.track_uids.push(v);
            }
            ids::TAG_EDITION_UID => {
                let v = read_uint(r, e.size as usize)?;
                t.edition_uids.push(v);
            }
            ids::TAG_CHAPTER_UID => {
                let v = read_uint(r, e.size as usize)?;
                t.chapter_uids.push(v);
            }
            ids::TAG_ATTACHMENT_UID => {
                let v = read_uint(r, e.size as usize)?;
                t.attachment_uids.push(v);
            }
            ids::TARGET_TYPE_VALUE => {
                t.target_type_value = Some(read_uint(r, e.size as usize)?);
            }
            ids::TARGET_TYPE => {
                let s = read_string(r, e.size as usize)?;
                if !s.is_empty() {
                    t.target_type = Some(s);
                }
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

fn parse_simple_tag(r: &mut dyn ReadSeek, end: u64, s: &mut RawSimpleTag) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::TAG_NAME => s.name = read_string(r, e.size as usize)?,
            ids::TAG_STRING => {
                let v = read_string(r, e.size as usize)?;
                // RFC 9559 §5.1.8.1.2.5/§5.1.8.1.2.6 say TagString and
                // TagBinary are mutually exclusive within one SimpleTag;
                // if a producer violates this, the last one wins.
                s.value = SimpleTagValue::String(v);
            }
            ids::TAG_BINARY => {
                let v = read_bytes(r, e.size as usize)?;
                s.value = SimpleTagValue::Binary(v);
            }
            ids::TAG_LANGUAGE => {
                let v = read_string(r, e.size as usize)?;
                if !v.is_empty() {
                    s.language = v;
                }
            }
            ids::TAG_LANGUAGE_BCP47 => {
                let v = read_string(r, e.size as usize)?;
                if !v.is_empty() {
                    s.language_bcp47 = Some(v);
                }
            }
            ids::TAG_DEFAULT => {
                let v = read_uint(r, e.size as usize)?;
                s.default = v != 0;
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

/// Resolve every captured tag's `Targets` UIDs against the per-segment
/// track / chapter / attachment / edition index tables, then emit two
/// parallel surfaces:
///
/// 1. The legacy flattened `metadata` view, where each `SimpleTag` becomes
///    one `(key, value)` entry with the scope encoded in the key:
///    * Global (all UIDs zero):                          `<name>`
///    * Track UID matched stream index N (zero-indexed): `tag:track:<N>:<name>`
///    * Chapter UID matched chapter index N (1-indexed): `tag:chapter:<N>:<name>`
///    * Attachment UID matched index N (1-indexed):     `tag:attachment:<N>:<name>`
///    * Edition UID matched edition index N (1-indexed): `tag:edition:<N>:<name>`
/// 2. The typed `tags_out` vector — one [`Tag`] per *Tag* element in the
///    file, retaining `TargetType` / `TargetTypeValue` / language / default
///    / multi-UID scoping that the flat view discards. Each UID inside a
///    `Tag` is resolved to a [`TargetUid`] pointing at its 0-indexed
///    stream or 1-indexed chapter/attachment/edition slot.
///
/// A `Tag` whose `Targets` master contains *only* unresolved non-zero UIDs
/// (i.e. every reference points outside the Segment) is dropped from
/// **both** surfaces — RFC 9559 §5.1.8.1.1.3 mandates "MUST match the
/// TrackUID value of a track found in this Segment", so unresolved
/// references are non-conformant and we don't try to salvage them. The
/// same logic applies to all four `Tag*UID` flavours. Within a `Targets`
/// master that has a *mix* of resolvable and dangling UIDs, only the
/// resolved ones surface in [`Targets::uids`].
///
/// Names are lower-cased in the flat view to match `parse_info`'s
/// convention but preserved verbatim in [`SimpleTag::name`] for round-trip
/// fidelity.
#[allow(clippy::too_many_arguments)]
fn resolve_tags(
    raw_tags: Vec<RawTag>,
    track_uid_to_index: &std::collections::HashMap<u64, u32>,
    chapter_uid_to_index: &std::collections::HashMap<u64, u32>,
    attachment_uid_to_index: &std::collections::HashMap<u64, u32>,
    edition_uid_to_index: &std::collections::HashMap<u64, u32>,
    metadata: &mut Vec<(String, String)>,
    tags_out: &mut Vec<Tag>,
) {
    for tag in raw_tags {
        // Translate every non-zero UID to a TargetUid, dropping the ones
        // that don't resolve. A Tag with no UIDs at all is a global tag
        // (RFC 9559 §5.1.8.1.1, "If empty or omitted, then the tag value
        // describes everything in the Segment").
        let mut resolved_uids: Vec<TargetUid> = Vec::new();
        let mut had_any_uid = false;
        for &uid in &tag.track_uids {
            had_any_uid = true;
            if uid == 0 {
                continue; // 0 = "all tracks", carried via the no-UID branch.
            }
            if let Some(&idx) = track_uid_to_index.get(&uid) {
                resolved_uids.push(TargetUid::Track {
                    stream_index: idx,
                    track_uid: uid,
                });
            }
        }
        for &uid in &tag.edition_uids {
            had_any_uid = true;
            if uid == 0 {
                continue;
            }
            if let Some(&idx) = edition_uid_to_index.get(&uid) {
                resolved_uids.push(TargetUid::Edition {
                    edition_index: idx,
                    edition_uid: uid,
                });
            }
        }
        for &uid in &tag.chapter_uids {
            had_any_uid = true;
            if uid == 0 {
                continue;
            }
            if let Some(&idx) = chapter_uid_to_index.get(&uid) {
                resolved_uids.push(TargetUid::Chapter {
                    chapter_index: idx,
                    chapter_uid: uid,
                });
            }
        }
        for &uid in &tag.attachment_uids {
            had_any_uid = true;
            if uid == 0 {
                continue;
            }
            if let Some(&idx) = attachment_uid_to_index.get(&uid) {
                resolved_uids.push(TargetUid::Attachment {
                    attachment_index: idx,
                    attachment_uid: uid,
                });
            }
        }
        // Drop the whole Tag if every UID it carried was non-zero but
        // failed to resolve. RFC 9559 §5.1.8.1.1.3..§5.1.8.1.1.6 use
        // "MUST match" phrasing so dangling references are not just
        // unfortunate — they make the Tag non-conformant.
        if had_any_uid && resolved_uids.is_empty() {
            continue;
        }

        // Build the legacy flat-view key prefix. Precedence is
        // track > edition > chapter > attachment, mirroring the order
        // RFC 9559 §5.1.8.1.1 lists the UID children. Real-world files
        // set at most one UID anyway.
        let prefix: String = if let Some(t) = resolved_uids.iter().find_map(|u| match u {
            TargetUid::Track { stream_index, .. } => Some(*stream_index),
            _ => None,
        }) {
            format!("tag:track:{t}:")
        } else if let Some(e) = resolved_uids.iter().find_map(|u| match u {
            TargetUid::Edition { edition_index, .. } => Some(*edition_index),
            _ => None,
        }) {
            format!("tag:edition:{e}:")
        } else if let Some(c) = resolved_uids.iter().find_map(|u| match u {
            TargetUid::Chapter { chapter_index, .. } => Some(*chapter_index),
            _ => None,
        }) {
            format!("tag:chapter:{c}:")
        } else if let Some(a) = resolved_uids.iter().find_map(|u| match u {
            TargetUid::Attachment {
                attachment_index, ..
            } => Some(*attachment_index),
            _ => None,
        }) {
            format!("tag:attachment:{a}:")
        } else {
            // No resolved UIDs → global scope (all UIDs zero, or no UID
            // children at all).
            String::new()
        };

        // Build the typed surface. Each `SimpleTag` keeps its original
        // case / language / default flag / binary payload — none of which
        // the flat view exposes.
        let mut typed_simple: Vec<SimpleTag> = Vec::with_capacity(tag.simple_tags.len());
        for raw in &tag.simple_tags {
            typed_simple.push(SimpleTag {
                name: raw.name.clone(),
                value: raw.value.clone(),
                language: raw.language.clone(),
                language_bcp47: raw.language_bcp47.clone(),
                default: raw.default,
            });
            // Project into the legacy flat view only when the value is a
            // non-empty string. Binary tag values (cover art, etc.) and
            // empty placeholders are skipped to match the pre-typed
            // behaviour where only `(name, str)` pairs surfaced.
            if let SimpleTagValue::String(ref v) = raw.value {
                if !raw.name.is_empty() && !v.is_empty() {
                    let key = format!("{prefix}{}", raw.name.to_ascii_lowercase());
                    metadata.push((key, v.clone()));
                }
            }
        }

        tags_out.push(Tag {
            targets: Targets {
                target_type_value: tag.target_type_value,
                target_type: tag.target_type.clone(),
                uids: resolved_uids,
            },
            simple_tags: typed_simple,
        });
    }
}

/// A typed `Tag` element (RFC 9559 §5.1.8.1) with its `Targets` UIDs
/// resolved against the per-Segment track / edition / chapter / attachment
/// tables. Exposed via [`MkvDemuxer::tags`] so consumers can walk per-track
/// and per-chapter metadata without re-parsing the file.
///
/// Companion to the flattened `metadata()` view: every `(key, value)` pair
/// surfaced in metadata corresponds to a [`SimpleTag`] inside one of these
/// `Tag`s, but [`SimpleTag`] additionally preserves language, default
/// flag, original case, and binary payloads that the flat view discards.
#[derive(Clone, Debug, PartialEq)]
pub struct Tag {
    /// Scope of this tag — see [`Targets`] for the per-field semantics.
    pub targets: Targets,
    /// One or more `(name, value)` descriptors that share this `Targets`
    /// scope (RFC 9559 §5.1.8.1.2). Order matches the on-disk order.
    pub simple_tags: Vec<SimpleTag>,
}

/// `Targets` master (RFC 9559 §5.1.8.1.1) — the scope of a [`Tag`].
///
/// An empty / omitted `Targets` master is a global scope (`uids` empty,
/// `target_type` / `target_type_value` both `None`). When `uids` is empty
/// but a `target_type` / `target_type_value` is set, the tag is still
/// global as far as scoping is concerned — those fields are purely
/// informational display hints per RFC 9559 §5.1.8.1.1.1 / §5.1.8.1.1.2.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Targets {
    /// `TargetTypeValue` (RFC 9559 §5.1.8.1.1.1). Default per spec is 50
    /// ("ALBUM / OPERA / CONCERT / MOVIE / EPISODE") but we surface
    /// `None` when the element was absent so consumers can distinguish.
    pub target_type_value: Option<u64>,
    /// `TargetType` informational string (RFC 9559 §5.1.8.1.1.2) — e.g.
    /// `"ALBUM"`, `"MOVIE"`, `"TRACK"`. The spec says this MUST match
    /// `target_type_value`'s row in Table 34 if both are present; we
    /// don't enforce — we just surface what the file says.
    pub target_type: Option<String>,
    /// Resolved scope references. Empty means global scope.
    ///
    /// All UIDs are resolved against the Segment's tables: a
    /// [`TargetUid::Track`] only appears here when the file actually
    /// contains a matching `TrackUID`. Dangling references are dropped
    /// per RFC 9559 §5.1.8.1.1.3..§5.1.8.1.1.6.
    pub uids: Vec<TargetUid>,
}

/// One resolved entry in [`Targets::uids`]. The `_uid` field preserves
/// the on-disk UID so consumers that want to cross-reference (e.g. emit
/// the same tag back into a re-mux) can do so without re-reading the
/// file; the `_index` field is the 0- or 1-indexed slot the demuxer
/// assigned, matching the indices used in the flat `metadata()` keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetUid {
    /// Resolved `TagTrackUID` (RFC 9559 §5.1.8.1.1.3). `stream_index` is
    /// 0-indexed, matching [`oxideav_core::StreamInfo::index`].
    Track { stream_index: u32, track_uid: u64 },
    /// Resolved `TagEditionUID` (RFC 9559 §5.1.8.1.1.4). 1-indexed to
    /// match the flat `tag:edition:N:*` key convention.
    Edition {
        edition_index: u32,
        edition_uid: u64,
    },
    /// Resolved `TagChapterUID` (RFC 9559 §5.1.8.1.1.5). 1-indexed.
    Chapter {
        chapter_index: u32,
        chapter_uid: u64,
    },
    /// Resolved `TagAttachmentUID` (RFC 9559 §5.1.8.1.1.6). 1-indexed.
    Attachment {
        attachment_index: u32,
        attachment_uid: u64,
    },
}

/// One `SimpleTag` element (RFC 9559 §5.1.8.1.2). Preserves the on-disk
/// `TagName` case (the flat view lower-cases) plus language metadata and
/// binary-payload tags (e.g. cover-art bytes) that the legacy
/// `(key, value)` view can't represent.
#[derive(Clone, Debug, PartialEq)]
pub struct SimpleTag {
    /// `TagName` (RFC 9559 §5.1.8.1.2.1).
    pub name: String,
    /// `TagString` / `TagBinary` payload. Mutually exclusive per the
    /// spec, so we model them as one enum.
    pub value: SimpleTagValue,
    /// `TagLanguage` (RFC 9559 §5.1.8.1.2.2). Defaults to `"und"` per
    /// the spec; we materialise the default rather than leaving it empty
    /// so consumers don't have to special-case the absent element.
    pub language: String,
    /// `TagLanguageBCP47` (RFC 9559 §5.1.8.1.2.3). When present, `language`
    /// MUST be ignored per spec.
    pub language_bcp47: Option<String>,
    /// `TagDefault` (RFC 9559 §5.1.8.1.2.4). Default per spec is true.
    pub default: bool,
}

/// `TagString` vs `TagBinary` payload — mutually exclusive within one
/// `SimpleTag` per RFC 9559 §5.1.8.1.2.5 / §5.1.8.1.2.6.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SimpleTagValue {
    /// `TagString` (RFC 9559 §5.1.8.1.2.5) — UTF-8 text payload.
    String(String),
    /// `TagBinary` (RFC 9559 §5.1.8.1.2.6) — opaque bytes, used for e.g.
    /// embedded cover art that's referenced from a SimpleTag.
    Binary(Vec<u8>),
    /// Neither `TagString` nor `TagBinary` was present. RFC 9559 doesn't
    /// give either of them a mandatory minOccurs, so this case is
    /// reachable for well-formed files.
    #[default]
    None,
}

/// A resolved `TrackOperation` (RFC 9559 §5.1.4.1.30) describing a virtual
/// track that is built by combining other tracks. Surfaced per stream via
/// [`MkvDemuxer::track_operations`].
///
/// Two independent mechanisms can appear (a single virtual track MAY use
/// both):
///
/// * `TrackCombinePlanes` (§5.1.4.1.30.1) — the [`planes`](Self::planes)
///   list names video tracks combined into one stereoscopic 3D track,
///   each tagged with its [`TrackPlaneType`] (left / right eye, background).
/// * `TrackJoinBlocks` (§5.1.4.1.30.5) — the
///   [`join_tracks`](Self::join_tracks) list names tracks whose Blocks are
///   joined into a single timeline.
///
/// Each referenced track is reported as a [`TrackRef`] carrying both the
/// on-disk `TrackUID` and, when that UID matches a track in the same
/// Segment, the resolved 0-indexed stream index. References that don't
/// resolve to a present track keep their `stream_index == None` rather than
/// being dropped — the spec lets a virtual track reference a track that a
/// reader chose not to surface.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackOperation {
    /// Resolved `TrackCombinePlanes` planes, in on-disk order. Empty when
    /// the operation has no `TrackCombinePlanes` child.
    pub planes: Vec<TrackPlane>,
    /// Resolved `TrackJoinBlocks` track references, in on-disk order. Empty
    /// when the operation has no `TrackJoinBlocks` child.
    pub join_tracks: Vec<TrackRef>,
}

impl TrackOperation {
    /// True when this operation carries neither planes nor join references —
    /// i.e. an empty `TrackOperation` master. Such a track is not really a
    /// virtual track, but the element is legal so we still surface it.
    pub fn is_empty(&self) -> bool {
        self.planes.is_empty() && self.join_tracks.is_empty()
    }
}

/// One `TrackPlane` (RFC 9559 §5.1.4.1.30.2) inside a
/// [`TrackOperation::planes`] list: a referenced video track plus the role
/// it plays in the combined 3D track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackPlane {
    /// The plane's source track, resolved from its `TrackPlaneUID`.
    pub track: TrackRef,
    /// `TrackPlaneType` (RFC 9559 §5.1.4.1.30.4).
    pub plane_type: TrackPlaneType,
}

/// `TrackPlaneType` (RFC 9559 §5.1.4.1.30.4, Table 20).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackPlaneType {
    /// `0` — left eye.
    LeftEye,
    /// `1` — right eye.
    RightEye,
    /// `2` — background.
    Background,
    /// `3..` — a value registered under the IANA "Matroska Track Plane
    /// Types" registry (RFC 9559 §27.17) that this build doesn't name.
    Other(u64),
}

impl TrackPlaneType {
    /// Map a raw `TrackPlaneType` integer onto the enum, preserving
    /// unrecognised values via [`TrackPlaneType::Other`].
    pub fn from_raw(v: u64) -> Self {
        match v {
            ids::TRACK_PLANE_TYPE_LEFT_EYE => TrackPlaneType::LeftEye,
            ids::TRACK_PLANE_TYPE_RIGHT_EYE => TrackPlaneType::RightEye,
            ids::TRACK_PLANE_TYPE_BACKGROUND => TrackPlaneType::Background,
            other => TrackPlaneType::Other(other),
        }
    }
}

/// A reference to a track from within a [`TrackOperation`], keyed by
/// `TrackUID`. The `stream_index` is the resolved 0-indexed position in
/// [`Demuxer::streams`](oxideav_core::Demuxer::streams) when the UID
/// matches a track in the same Segment, or `None` for a dangling
/// reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackRef {
    /// The referenced `TrackUID` as stored in the file.
    pub track_uid: u64,
    /// Resolved 0-indexed stream index, or `None` when no track in the
    /// Segment carries this `TrackUID`.
    pub stream_index: Option<u32>,
}

/// A track's `ContentEncodings` (RFC 9559 §5.1.4.1.31): the ordered list of
/// transformations applied to the track's frame data and/or `CodecPrivate`
/// before the bytes were written into Blocks.
///
/// This is purely the *description* of how a track's data was encoded — the
/// container does not decompress or decrypt anything. A reader that wants
/// the raw codec bytes back must undo the encodings itself, in the order
/// the spec defines: highest [`ContentEncoding::order`] first, lowest last
/// (§5.1.4.1.31.2).
///
/// `encodings` is returned sorted by descending `order` so iterating it
/// front-to-back is the spec-mandated *decode* order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContentEncodings {
    /// Each [`ContentEncoding`], sorted by descending
    /// [`ContentEncoding::order`] (decode order — apply the first entry
    /// first).
    pub encodings: Vec<ContentEncoding>,
}

impl ContentEncodings {
    /// True when the track declares no content encodings (an empty or absent
    /// `ContentEncodings` master).
    pub fn is_empty(&self) -> bool {
        self.encodings.is_empty()
    }
}

/// Compute the byte prefix to prepend to every de-laced frame in order to
/// undo a track's Block-scoped Header-Stripping compression (RFC 9559
/// §5.1.4.1.31.6 algo 3, §5.1.4.1.31.7).
///
/// Header Stripping is the only [`ContentEncoding`] transform the container
/// can reverse without a compression/encryption codec: the
/// `ContentCompSettings` bytes were removed from the front of each frame on
/// write, so prepending them restores the original frame.
///
/// The full chain is undone highest-`ContentEncodingOrder` first
/// (§5.1.4.1.31.2). `enc.encodings` is already pre-sorted into that decode
/// order, so iterating it front-to-back and prepending each step's stripped
/// bytes ahead of the bytes accumulated so far yields the correct combined
/// prefix.
///
/// This only fires when *every* Block-scoped (`ContentEncodingScope` bit
/// `0x1`) encoding is Header Stripping. If any Block-scoped step is a
/// different compression (zlib / bzlib / lzo1x) or an encryption, the
/// container cannot reconstruct the raw bytes and returns `None` — the
/// caller must undo the whole chain itself and the demuxer leaves packets
/// encoded. Non-Block-scoped encodings (e.g. `CodecPrivate`-only, scope
/// `0x2`) are ignored here since they never touch frame data.
fn compute_header_strip_prefix(enc: &ContentEncodings) -> Option<Vec<u8>> {
    let mut prefix: Vec<u8> = Vec::new();
    let mut saw_strip = false;
    for e in &enc.encodings {
        if !e.scope.block() {
            // Doesn't touch Block frame data — irrelevant to packet bytes.
            continue;
        }
        match &e.transform {
            ContentEncodingTransform::Compression {
                algo: ContentCompAlgo::HeaderStripping,
                settings,
            } => {
                // Decode order: this (higher-order) step is undone before the
                // ones already accumulated, so its bytes go in front.
                let mut combined = settings.clone();
                combined.extend_from_slice(&prefix);
                prefix = combined;
                saw_strip = true;
            }
            // Any other Block-scoped transform (real compression or
            // encryption) is something the container can't undo — bail so the
            // whole chain is left to the caller rather than corrupting frames.
            _ => return None,
        }
    }
    if saw_strip {
        Some(prefix)
    } else {
        None
    }
}

/// One `ContentEncoding` (RFC 9559 §5.1.4.1.31.1): a single compression or
/// encryption step in a track's [`ContentEncodings`] chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentEncoding {
    /// `ContentEncodingOrder` (§5.1.4.1.31.2, default 0). Encodings are
    /// applied to the data on *write* from lowest order to highest, so a
    /// reader undoes them from highest to lowest.
    pub order: u64,
    /// `ContentEncodingScope` bit field (§5.1.4.1.31.3, default 0x1) naming
    /// which parts of the track this encoding touches.
    pub scope: ContentEncodingScope,
    /// The transformation itself — compression or encryption settings.
    pub transform: ContentEncodingTransform,
}

/// `ContentEncodingScope` (RFC 9559 §5.1.4.1.31.3, Table 21): a bit field
/// describing which elements of the track an encoding was applied to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentEncodingScope(pub u64);

impl ContentEncodingScope {
    /// `0x1` — applies to all frame contents, excluding lacing data.
    pub fn block(self) -> bool {
        self.0 & ids::CONTENT_ENCODING_SCOPE_BLOCK != 0
    }
    /// `0x2` — applies to the track's `CodecPrivate` data.
    pub fn private(self) -> bool {
        self.0 & ids::CONTENT_ENCODING_SCOPE_PRIVATE != 0
    }
    /// `0x4` — applies to the next `ContentEncoding`'s settings. The spec
    /// says this SHOULD NOT be used; surfaced for completeness.
    pub fn next(self) -> bool {
        self.0 & ids::CONTENT_ENCODING_SCOPE_NEXT != 0
    }
}

/// The kind of transformation a [`ContentEncoding`] performs, selected by
/// `ContentEncodingType` (RFC 9559 §5.1.4.1.31.4, Table 22).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentEncodingTransform {
    /// `ContentEncodingType = 0` — compression (§5.1.4.1.31.5). Carries the
    /// algorithm and, for header stripping, the stripped octets.
    Compression {
        /// `ContentCompAlgo` (§5.1.4.1.31.6).
        algo: ContentCompAlgo,
        /// `ContentCompSettings` (§5.1.4.1.31.7). For
        /// [`ContentCompAlgo::HeaderStripping`] these are the bytes removed
        /// from the front of each frame, to be prepended back on decode.
        /// Empty when the element is absent.
        settings: Vec<u8>,
    },
    /// `ContentEncodingType = 1` — encryption (§5.1.4.1.31.8). The container
    /// surfaces the cipher description only; it does not decrypt.
    Encryption {
        /// `ContentEncAlgo` (§5.1.4.1.31.9).
        algo: ContentEncAlgo,
        /// `ContentEncKeyID` (§5.1.4.1.31.10) — the public-key ID for
        /// public-key algorithms. Empty when absent.
        key_id: Vec<u8>,
        /// `AESSettingsCipherMode` (§5.1.4.1.31.12) inside
        /// `ContentEncAESSettings` (§5.1.4.1.31.11). Only meaningful when
        /// `algo` is [`ContentEncAlgo::Aes`]; `None` otherwise.
        aes_cipher_mode: Option<AesCipherMode>,
    },
}

/// `ContentCompAlgo` (RFC 9559 §5.1.4.1.31.6, Table 23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentCompAlgo {
    /// `0` — zlib (RFC 1950).
    Zlib,
    /// `1` — bzip2. SHOULD NOT be used (see spec usage notes).
    Bzlib,
    /// `2` — LZO1X. SHOULD NOT be used (licensing).
    Lzo1x,
    /// `3` — header stripping: octets in `ContentCompSettings` were removed
    /// from the front of each frame.
    HeaderStripping,
    /// A value registered in the IANA "Matroska Compression Algorithms"
    /// registry (§27.2) that this build doesn't name.
    Other(u64),
}

impl ContentCompAlgo {
    /// Map a raw `ContentCompAlgo` integer onto the enum.
    pub fn from_raw(v: u64) -> Self {
        match v {
            ids::CONTENT_COMP_ALGO_ZLIB => ContentCompAlgo::Zlib,
            ids::CONTENT_COMP_ALGO_BZLIB => ContentCompAlgo::Bzlib,
            ids::CONTENT_COMP_ALGO_LZO1X => ContentCompAlgo::Lzo1x,
            ids::CONTENT_COMP_ALGO_HEADER_STRIPPING => ContentCompAlgo::HeaderStripping,
            other => ContentCompAlgo::Other(other),
        }
    }
}

/// `ContentEncAlgo` (RFC 9559 §5.1.4.1.31.9, Table 24).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentEncAlgo {
    /// `0` — not encrypted (signals only signing, per spec).
    None,
    /// `1` — DES. SHOULD be avoided.
    Des,
    /// `2` — 3DES. SHOULD be avoided.
    TripleDes,
    /// `3` — Twofish.
    Twofish,
    /// `4` — Blowfish. SHOULD be avoided.
    Blowfish,
    /// `5` — AES.
    Aes,
    /// A value registered in the IANA "Matroska Encryption Algorithms"
    /// registry (§27.3) that this build doesn't name.
    Other(u64),
}

impl ContentEncAlgo {
    /// Map a raw `ContentEncAlgo` integer onto the enum.
    pub fn from_raw(v: u64) -> Self {
        match v {
            ids::CONTENT_ENC_ALGO_NONE => ContentEncAlgo::None,
            ids::CONTENT_ENC_ALGO_DES => ContentEncAlgo::Des,
            ids::CONTENT_ENC_ALGO_3DES => ContentEncAlgo::TripleDes,
            ids::CONTENT_ENC_ALGO_TWOFISH => ContentEncAlgo::Twofish,
            ids::CONTENT_ENC_ALGO_BLOWFISH => ContentEncAlgo::Blowfish,
            ids::CONTENT_ENC_ALGO_AES => ContentEncAlgo::Aes,
            other => ContentEncAlgo::Other(other),
        }
    }
}

/// `AESSettingsCipherMode` (RFC 9559 §5.1.4.1.31.12, Table 26).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AesCipherMode {
    /// `1` — AES-CTR (counter mode).
    Ctr,
    /// `2` — AES-CBC (cipher block chaining).
    Cbc,
    /// A value registered in the IANA "Matroska AES Cipher Modes" registry
    /// (§27.4) that this build doesn't name.
    Other(u64),
}

impl AesCipherMode {
    /// Map a raw `AESSettingsCipherMode` integer onto the enum.
    pub fn from_raw(v: u64) -> Self {
        match v {
            ids::AES_CIPHER_MODE_CTR => AesCipherMode::Ctr,
            ids::AES_CIPHER_MODE_CBC => AesCipherMode::Cbc,
            other => AesCipherMode::Other(other),
        }
    }
}

/// One `Segment\Chapters\EditionEntry` (RFC 9559 §5.1.7.1) — a complete
/// chapter edition with its tree of [`Chapter`] atoms. Surfaced via
/// [`MkvDemuxer::chapters`].
///
/// The flat [`Demuxer::metadata`] view collapses every edition into one
/// 1-indexed `chapter:N:*` namespace and keeps only the first display
/// string; this typed view preserves the edition grouping, edition flags,
/// nested chapters, and *all* multilingual [`ChapterDisplay`] rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Edition {
    /// `EditionUID` (RFC 9559 §5.1.7.1.1). `None` when the element was
    /// absent (it's optional); never zero (the spec bars 0).
    pub uid: Option<u64>,
    /// `EditionFlagDefault` (RFC 9559 §5.1.7.1.2). `true` when this edition
    /// SHOULD be used as the default. Defaults to `false`.
    pub default: bool,
    /// `EditionFlagOrdered` (RFC 9559 §5.1.7.1.3). `true` for an ordered
    /// edition (chapter playback order is enforced). Defaults to `false`.
    pub ordered: bool,
    /// Top-level `ChapterAtom`s in on-disk order. Nested chapters live in
    /// each [`Chapter::children`].
    pub chapters: Vec<Chapter>,
}

/// One `ChapterAtom` (RFC 9559 §5.1.7.1.4) — recursive: a chapter MAY
/// contain nested child chapters. Part of [`Edition::chapters`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chapter {
    /// 1-based index across the whole `Chapters` element, assigned in
    /// document order (top-level then nested, depth-first). Matches the
    /// `chapter:N:*` keys in the flat [`Demuxer::metadata`] view and the
    /// `chapter_index` carried by a resolved [`TargetUid::Chapter`].
    pub index: u32,
    /// `ChapterUID` (RFC 9559 §5.1.7.1.4.1). Mandatory per spec; `None`
    /// only for a malformed atom that omits it. Never zero.
    pub uid: Option<u64>,
    /// `ChapterStringUID` (RFC 9559 §5.1.7.1.4.2) — a unique string ID,
    /// e.g. a WebVTT cue identifier. `None` when absent.
    pub string_uid: Option<String>,
    /// `ChapterTimeStart` (RFC 9559 §5.1.7.1.4.3) in **nanoseconds**
    /// (Matroska Ticks — independent of the segment `TimecodeScale`).
    pub time_start_ns: u64,
    /// `ChapterTimeEnd` (RFC 9559 §5.1.7.1.4.4) in **nanoseconds**. `None`
    /// when absent (mandatory only for ordered editions / parent chapters).
    pub time_end_ns: Option<u64>,
    /// `ChapterFlagHidden` (RFC 9559 §5.1.7.1.4.5). Defaults to `false`.
    pub hidden: bool,
    /// `ChapterDisplay` rows (RFC 9559 §5.1.7.1.4.9), in on-disk order —
    /// one per language. Empty when the atom carries no display string.
    pub displays: Vec<ChapterDisplay>,
    /// Nested `ChapterAtom`s (RFC 9559 §5.1.7.1.4 is `recursive`).
    pub children: Vec<Chapter>,
}

/// One `ChapterDisplay` master (RFC 9559 §5.1.7.1.4.9) — a chapter title
/// in one language. Part of [`Chapter::displays`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChapterDisplay {
    /// `ChapString` (RFC 9559 §5.1.7.1.4.10) — the title text.
    pub string: String,
    /// `ChapLanguage` (RFC 9559 §5.1.7.1.4.11). Defaults to `"eng"` per
    /// spec; materialised so consumers don't special-case the absent
    /// element. MUST be ignored when [`language_bcp47`](Self::language_bcp47)
    /// is present.
    pub language: String,
    /// `ChapLanguageBCP47` (RFC 9559 §5.1.7.1.4.12). When present, both
    /// `language` and `country` MUST be ignored per spec.
    pub language_bcp47: Option<String>,
    /// `ChapCountry` (RFC 9559 §5.1.7.1.4.13). `None` when absent. MUST be
    /// ignored when `language_bcp47` is present.
    pub country: Option<String>,
}

/// Parse a `Chapters` master element. Populates two views in one pass:
///
/// * The flat [`Demuxer::metadata`] view — each `ChapterAtom` lifts to
///   `chapter:N:start_ms`, `chapter:N:end_ms` (when present), and
///   `chapter:N:title` (first non-empty `ChapterDisplay\ChapString`).
///   Chapters are 1-indexed to match ffprobe's display order.
/// * The typed [`Edition`] / [`Chapter`] tree returned via `editions`,
///   surfaced by [`MkvDemuxer::chapters`].
///
/// `ChapterTimeStart` / `ChapterTimeEnd` carry **nanoseconds**, not
/// timecode-scale ticks — that's spec-defined and independent of the
/// segment's `TimecodeScale`. The flat view surfaces them as integer
/// milliseconds; the typed view keeps the raw nanoseconds.
#[allow(clippy::too_many_arguments)]
fn parse_chapters_typed(
    r: &mut dyn ReadSeek,
    end: u64,
    metadata: &mut Vec<(String, String)>,
    chapter_uid_to_index: &mut std::collections::HashMap<u64, u32>,
    edition_uid_to_index: &mut std::collections::HashMap<u64, u32>,
    editions: &mut Vec<Edition>,
) -> Result<()> {
    // Shared 1-based counter across the whole Chapters element (every
    // EditionEntry, every nesting level), assigned depth-first in document
    // order. Keeps the `chapter:N:*` flat keys and `TagChapterUID`
    // resolution stable while extending indexing to nested atoms.
    let mut chapter_index: u32 = 0;
    let mut edition_index: u32 = 0;
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::EDITION_ENTRY => {
                let ee_end = r.stream_position()? + e.size;
                edition_index += 1;
                let edition = parse_edition_entry(
                    r,
                    ee_end,
                    metadata,
                    &mut chapter_index,
                    edition_index,
                    chapter_uid_to_index,
                    edition_uid_to_index,
                )?;
                editions.push(edition);
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn parse_edition_entry(
    r: &mut dyn ReadSeek,
    end: u64,
    metadata: &mut Vec<(String, String)>,
    chapter_index: &mut u32,
    edition_index: u32,
    chapter_uid_to_index: &mut std::collections::HashMap<u64, u32>,
    edition_uid_to_index: &mut std::collections::HashMap<u64, u32>,
) -> Result<Edition> {
    let mut edition = Edition::default();
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::EDITION_UID => {
                let uid = read_uint(r, e.size as usize)?;
                if uid != 0 {
                    edition_uid_to_index.insert(uid, edition_index);
                    edition.uid = Some(uid);
                }
            }
            ids::EDITION_FLAG_DEFAULT => edition.default = read_uint(r, e.size as usize)? != 0,
            ids::EDITION_FLAG_ORDERED => edition.ordered = read_uint(r, e.size as usize)? != 0,
            ids::CHAPTER_ATOM => {
                let ca_end = r.stream_position()? + e.size;
                let atom =
                    parse_chapter_atom(r, ca_end, metadata, chapter_index, chapter_uid_to_index)?;
                edition.chapters.push(atom);
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(edition)
}

fn parse_chapter_atom(
    r: &mut dyn ReadSeek,
    end: u64,
    metadata: &mut Vec<(String, String)>,
    chapter_index: &mut u32,
    chapter_uid_to_index: &mut std::collections::HashMap<u64, u32>,
) -> Result<Chapter> {
    // Reserve this atom's index *before* descending into children so the
    // numbering is depth-first document order (parent before its kids).
    *chapter_index += 1;
    let index = *chapter_index;
    let mut atom = Chapter {
        index,
        ..Chapter::default()
    };
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::CHAPTER_UID => {
                let uid = read_uint(r, e.size as usize)?;
                if uid != 0 {
                    chapter_uid_to_index.insert(uid, index);
                    atom.uid = Some(uid);
                }
            }
            ids::CHAPTER_STRING_UID => {
                atom.string_uid = Some(read_string(r, e.size as usize)?);
            }
            ids::CHAPTER_TIME_START => atom.time_start_ns = read_uint(r, e.size as usize)?,
            ids::CHAPTER_TIME_END => atom.time_end_ns = Some(read_uint(r, e.size as usize)?),
            ids::CHAPTER_FLAG_HIDDEN => atom.hidden = read_uint(r, e.size as usize)? != 0,
            ids::CHAPTER_DISPLAY => {
                let cd_end = r.stream_position()? + e.size;
                if let Some(disp) = parse_chapter_display(r, cd_end)? {
                    atom.displays.push(disp);
                }
            }
            ids::CHAPTER_ATOM => {
                let ca_end = r.stream_position()? + e.size;
                let child =
                    parse_chapter_atom(r, ca_end, metadata, chapter_index, chapter_uid_to_index)?;
                atom.children.push(child);
            }
            _ => skip(r, e.size)?,
        }
    }
    // Flat metadata view: only top-of-atom fields, keyed by the 1-based
    // index. `title` is the first non-empty display string (back-compat
    // with the pre-typed behaviour).
    metadata.push((
        format!("chapter:{index}:start_ms"),
        (atom.time_start_ns / 1_000_000).to_string(),
    ));
    if let Some(ns) = atom.time_end_ns {
        metadata.push((
            format!("chapter:{index}:end_ms"),
            (ns / 1_000_000).to_string(),
        ));
    }
    if let Some(t) = atom
        .displays
        .iter()
        .map(|d| &d.string)
        .find(|s| !s.is_empty())
    {
        metadata.push((format!("chapter:{index}:title"), t.clone()));
    }
    Ok(atom)
}

/// Parse one `ChapterDisplay` master into a typed [`ChapterDisplay`].
/// Returns `None` only when the master carries no (or an empty) `ChapString`
/// — an unusable row the flat view always dropped. `ChapLanguage` defaults
/// to `"eng"` per RFC 9559 §5.1.7.1.4.11.
fn parse_chapter_display(r: &mut dyn ReadSeek, end: u64) -> Result<Option<ChapterDisplay>> {
    let mut string: Option<String> = None;
    let mut language: Option<String> = None;
    let mut language_bcp47: Option<String> = None;
    let mut country: Option<String> = None;
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::CHAP_STRING => {
                let v = read_string(r, e.size as usize)?;
                if string.is_none() {
                    string = Some(v);
                }
            }
            ids::CHAP_LANGUAGE => language = Some(read_string(r, e.size as usize)?),
            ids::CHAP_LANGUAGE_BCP47 => language_bcp47 = Some(read_string(r, e.size as usize)?),
            ids::CHAP_COUNTRY => country = Some(read_string(r, e.size as usize)?),
            _ => skip(r, e.size)?,
        }
    }
    let string = match string {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(None),
    };
    Ok(Some(ChapterDisplay {
        string,
        language: language.unwrap_or_else(|| "eng".to_string()),
        language_bcp47,
        country,
    }))
}

/// Parse an `Attachments` master element. Each `AttachedFile` surfaces as
/// up to three metadata keys: `attachment:N:filename`,
/// `attachment:N:mime_type`, `attachment:N:size_bytes`. The actual file
/// payload is not returned — callers that want the bytes (e.g. embedded
/// fonts, cover art) should ask for a structured API once we have one;
/// surfacing the index keeps the demuxer's contract small while still
/// telling downstream tooling what's in the file.
///
/// File payloads are skipped via seek so we don't pull megabytes of data
/// into memory just to expose a filename. Sizes are reported from the
/// `FileData` element header so the `size_bytes` value is the on-disk size
/// (no compression decoded).
fn parse_attachments(
    r: &mut dyn ReadSeek,
    end: u64,
    metadata: &mut Vec<(String, String)>,
    attachment_uid_to_index: &mut std::collections::HashMap<u64, u32>,
) -> Result<()> {
    let mut idx: u32 = 0;
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::ATTACHED_FILE => {
                let af_end = r.stream_position()? + e.size;
                idx += 1;
                parse_attached_file(r, af_end, metadata, idx, attachment_uid_to_index)?;
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

fn parse_attached_file(
    r: &mut dyn ReadSeek,
    end: u64,
    metadata: &mut Vec<(String, String)>,
    index: u32,
    attachment_uid_to_index: &mut std::collections::HashMap<u64, u32>,
) -> Result<()> {
    let mut filename: Option<String> = None;
    let mut mime: Option<String> = None;
    let mut size: Option<u64> = None;
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::FILE_NAME => filename = Some(read_string(r, e.size as usize)?),
            ids::FILE_MIME_TYPE => mime = Some(read_string(r, e.size as usize)?),
            ids::FILE_UID => {
                let uid = read_uint(r, e.size as usize)?;
                if uid != 0 {
                    attachment_uid_to_index.insert(uid, index);
                }
            }
            ids::FILE_DATA => {
                size = Some(e.size);
                skip(r, e.size)?;
            }
            _ => skip(r, e.size)?,
        }
    }
    if let Some(n) = filename {
        if !n.is_empty() {
            metadata.push((format!("attachment:{index}:filename"), n));
        }
    }
    if let Some(m) = mime {
        if !m.is_empty() {
            metadata.push((format!("attachment:{index}:mime_type"), m));
        }
    }
    if let Some(sz) = size {
        metadata.push((format!("attachment:{index}:size_bytes"), sz.to_string()));
    }
    Ok(())
}

/// Format a unix timestamp (seconds since 1970-01-01 UTC) as an ISO-8601 date.
/// Roughly ffprobe-compatible; ignores leap seconds.
fn format_iso8601(unix_secs: i64) -> String {
    let (y, m, d, hh, mm, ss) = civil_from_days_seconds(unix_secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hh, mm, ss)
}

fn civil_from_days_seconds(unix_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = unix_secs.div_euclid(86_400);
    let secs_of_day = unix_secs.rem_euclid(86_400) as u32;
    // Howard Hinnant's date algorithms — shift so that era 0 starts 0000-03-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    (year, m, d, hh, mm, ss)
}

/// One Cues → CuePoint entry, denormalised to (track, time, cluster_offset)
/// where `cluster_offset` is a byte offset relative to the Segment payload
/// start (i.e. add it to `segment_data_start` to get an absolute file pos).
#[derive(Clone, Debug)]
struct CueEntry {
    track: u64,
    /// Timestamp in Matroska ticks (timecode_scale ns per tick).
    time: u64,
    cluster_offset: u64,
    /// `CueRelativePosition` (RFC 9559 §5.1.5.1.2.3) — byte offset of the
    /// referenced `SimpleBlock` / `BlockGroup` inside the Cluster, with `0`
    /// being the first possible position for an element inside that Cluster
    /// (i.e. immediately after the Cluster element's id+size header). `None`
    /// when the cue carried no `CueRelativePosition` child (legal — the
    /// element is optional, only `maxOccurs: 1`).
    relative_position: Option<u64>,
}

fn parse_cues(r: &mut dyn ReadSeek, end: u64, out: &mut Vec<CueEntry>) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::CUE_POINT => {
                let body_end = r.stream_position()? + e.size;
                parse_cue_point(r, body_end, out)?;
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

fn parse_cue_point(r: &mut dyn ReadSeek, end: u64, out: &mut Vec<CueEntry>) -> Result<()> {
    let mut time: u64 = 0;
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::CUE_TIME => time = read_uint(r, e.size as usize)?,
            ids::CUE_TRACK_POSITIONS => {
                let body_end = r.stream_position()? + e.size;
                parse_cue_track_positions(r, body_end, time, out)?;
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

fn parse_cue_track_positions(
    r: &mut dyn ReadSeek,
    end: u64,
    time: u64,
    out: &mut Vec<CueEntry>,
) -> Result<()> {
    let mut track: u64 = 0;
    let mut cluster_offset: Option<u64> = None;
    let mut relative_position: Option<u64> = None;
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::CUE_TRACK => track = read_uint(r, e.size as usize)?,
            ids::CUE_CLUSTER_POSITION => cluster_offset = Some(read_uint(r, e.size as usize)?),
            ids::CUE_RELATIVE_POSITION => {
                relative_position = Some(read_uint(r, e.size as usize)?);
            }
            _ => skip(r, e.size)?,
        }
    }
    if let Some(off) = cluster_offset {
        out.push(CueEntry {
            track,
            time,
            cluster_offset: off,
            relative_position,
        });
    }
    Ok(())
}

/// Best-effort scan of the byte range `[start, end)` looking for a top-level
/// Cues element whose header we can find intact. Used when the Cues element
/// appears after the last Cluster in the file (the common ffmpeg layout
/// when muxing in a single pass with index-at-end, and also what our own
/// muxer emits).
///
/// Unknown-size Clusters are walked element-by-element until a sibling
/// top-level element terminates them, so Cues that sit after an
/// unknown-size final Cluster are still found.
fn scan_cues_from(
    r: &mut dyn ReadSeek,
    start: u64,
    end: u64,
    out: &mut Vec<CueEntry>,
) -> Result<()> {
    r.seek(SeekFrom::Start(start))?;
    while r.stream_position()? < end {
        let pos = r.stream_position()?;
        let e = read_element_header(r)?;
        if e.id == ids::CUES {
            let body_start = r.stream_position()?;
            let body_end = if e.size == VINT_UNKNOWN_SIZE {
                end
            } else {
                body_start + e.size
            };
            if body_end > end {
                r.seek(SeekFrom::Start(pos))?;
                return Ok(());
            }
            parse_cues(r, body_end, out)?;
            return Ok(());
        }
        if e.size == VINT_UNKNOWN_SIZE {
            if e.id == ids::CLUSTER {
                // Walk cluster children until we meet a sibling top-level
                // element (another Cluster, Cues, Tags, ...). Push any
                // skip we can't interpret up to the parent loop's guard.
                if !walk_unknown_cluster(r, end)? {
                    return Ok(());
                }
                continue;
            }
            // Unknown-size, non-cluster element we can't interpret — stop.
            r.seek(SeekFrom::Start(pos))?;
            return Ok(());
        }
        let body_start = r.stream_position()?;
        let body_end = body_start + e.size;
        if body_end > end {
            r.seek(SeekFrom::Start(pos))?;
            return Ok(());
        }
        r.seek(SeekFrom::Start(body_end))?;
    }
    Ok(())
}

/// Walk the children of an unknown-size Cluster starting at the current
/// reader position. Returns `true` after positioning the reader on the
/// next top-level element (so the outer scan can continue from there) and
/// `false` if we hit EOF / end of segment before finding one. Any non-child
/// element id that's a valid Segment child terminates the walk.
fn walk_unknown_cluster(r: &mut dyn ReadSeek, end: u64) -> Result<bool> {
    while r.stream_position()? < end {
        let pos = r.stream_position()?;
        let e = match read_element_header(r) {
            Ok(v) => v,
            Err(_) => return Ok(false),
        };
        // Cluster children we know and can size correctly.
        let is_cluster_child = matches!(
            e.id,
            ids::TIMECODE
                | ids::SIMPLE_BLOCK
                | ids::BLOCK_GROUP
                | ids::BLOCK
                | ids::BLOCK_DURATION
                | ids::REFERENCE_BLOCK
                | ids::VOID
                | ids::CRC32
        );
        if !is_cluster_child {
            // Treat as a sibling of Cluster — rewind and let caller handle.
            r.seek(SeekFrom::Start(pos))?;
            return Ok(true);
        }
        if e.size == VINT_UNKNOWN_SIZE {
            // Unexpected inside a cluster; bail.
            return Ok(false);
        }
        let body_end = r.stream_position()? + e.size;
        if body_end > end {
            return Ok(false);
        }
        r.seek(SeekFrom::Start(body_end))?;
    }
    Ok(false)
}

fn parse_tracks(r: &mut dyn ReadSeek, end: u64, out: &mut Vec<TrackEntry>) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::TRACK_ENTRY => {
                let body_end = r.stream_position()? + e.size;
                let mut t = TrackEntry::default();
                parse_track_entry(r, body_end, &mut t)?;
                out.push(t);
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

fn parse_track_entry(r: &mut dyn ReadSeek, end: u64, t: &mut TrackEntry) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::TRACK_NUMBER => t.number = read_uint(r, e.size as usize)?,
            ids::TRACK_UID => t.uid = read_uint(r, e.size as usize)?,
            ids::TRACK_TYPE => t.track_type = read_uint(r, e.size as usize)?,
            ids::CODEC_ID => t.codec_id_string = read_string(r, e.size as usize)?,
            ids::CODEC_PRIVATE => t.codec_private = read_bytes(r, e.size as usize)?,
            ids::AUDIO => {
                let body_end = r.stream_position()? + e.size;
                parse_audio(r, body_end, t)?;
            }
            ids::VIDEO => {
                let body_end = r.stream_position()? + e.size;
                parse_video(r, body_end, t)?;
            }
            ids::TRACK_OPERATION => {
                let body_end = r.stream_position()? + e.size;
                let mut op = RawTrackOperation::default();
                parse_track_operation(r, body_end, &mut op)?;
                t.track_operation = Some(op);
            }
            ids::CONTENT_ENCODINGS => {
                let body_end = r.stream_position()? + e.size;
                t.content_encodings = Some(parse_content_encodings(r, body_end)?);
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

/// Parse a `ContentEncodings` master (RFC 9559 §5.1.4.1.31) into the typed
/// [`ContentEncodings`]. Each `ContentEncoding` child is decoded into a
/// [`ContentEncoding`]; the list is then sorted by **descending**
/// `ContentEncodingOrder` so iterating it is the spec-mandated decode order
/// (§5.1.4.1.31.2: start with the highest order, work down).
///
/// Parse-only: the compression/encryption *settings* are surfaced, but no
/// frame is ever decompressed or decrypted here.
fn parse_content_encodings(r: &mut dyn ReadSeek, end: u64) -> Result<ContentEncodings> {
    let mut encodings: Vec<ContentEncoding> = Vec::new();
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::CONTENT_ENCODING => {
                let body_end = r.stream_position()? + e.size;
                encodings.push(parse_content_encoding(r, body_end)?);
            }
            _ => skip(r, e.size)?,
        }
    }
    // Stable-sort by descending order so equal-order entries keep on-disk
    // order. The spec says order MUST be unique within a ContentEncodings,
    // but a stable sort tolerates a malformed duplicate gracefully.
    encodings.sort_by_key(|e| std::cmp::Reverse(e.order));
    Ok(ContentEncodings { encodings })
}

/// Parse one `ContentEncoding` master (RFC 9559 §5.1.4.1.31.1).
///
/// `ContentEncodingType` (§5.1.4.1.31.4) selects whether the
/// `ContentCompression` or `ContentEncryption` child is the meaningful one:
/// type 0 → compression, type 1 → encryption. The spec requires the
/// matching child be present and the other absent, but we tolerate either
/// by keying off the type and defaulting a missing compression to zlib /
/// missing encryption to "not encrypted" per the element defaults.
fn parse_content_encoding(r: &mut dyn ReadSeek, end: u64) -> Result<ContentEncoding> {
    let mut order: u64 = 0; // §5.1.4.1.31.2 default
    let mut scope: u64 = ids::CONTENT_ENCODING_SCOPE_BLOCK; // §5.1.4.1.31.3 default 0x1
    let mut enc_type: u64 = ids::CONTENT_ENCODING_TYPE_COMPRESSION; // §5.1.4.1.31.4 default 0
    let mut comp: Option<(u64, Vec<u8>)> = None;
    let mut encr: Option<(u64, Vec<u8>, Option<u64>)> = None;
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::CONTENT_ENCODING_ORDER => order = read_uint(r, e.size as usize)?,
            ids::CONTENT_ENCODING_SCOPE => scope = read_uint(r, e.size as usize)?,
            ids::CONTENT_ENCODING_TYPE => enc_type = read_uint(r, e.size as usize)?,
            ids::CONTENT_COMPRESSION => {
                let body_end = r.stream_position()? + e.size;
                comp = Some(parse_content_compression(r, body_end)?);
            }
            ids::CONTENT_ENCRYPTION => {
                let body_end = r.stream_position()? + e.size;
                encr = Some(parse_content_encryption(r, body_end)?);
            }
            _ => skip(r, e.size)?,
        }
    }
    let transform = if enc_type == ids::CONTENT_ENCODING_TYPE_ENCRYPTION {
        let (algo, key_id, mode) = encr.unwrap_or((ids::CONTENT_ENC_ALGO_NONE, Vec::new(), None));
        ContentEncodingTransform::Encryption {
            algo: ContentEncAlgo::from_raw(algo),
            key_id,
            aes_cipher_mode: mode.map(AesCipherMode::from_raw),
        }
    } else {
        // Type 0 (compression) and any unknown type fall through to the
        // compression branch with the §5.1.4.1.31.6 default algo (zlib).
        let (algo, settings) = comp.unwrap_or((ids::CONTENT_COMP_ALGO_ZLIB, Vec::new()));
        ContentEncodingTransform::Compression {
            algo: ContentCompAlgo::from_raw(algo),
            settings,
        }
    };
    Ok(ContentEncoding {
        order,
        scope: ContentEncodingScope(scope),
        transform,
    })
}

/// Parse a `ContentCompression` master (RFC 9559 §5.1.4.1.31.5) into
/// `(ContentCompAlgo, ContentCompSettings)`.
fn parse_content_compression(r: &mut dyn ReadSeek, end: u64) -> Result<(u64, Vec<u8>)> {
    let mut algo: u64 = ids::CONTENT_COMP_ALGO_ZLIB; // §5.1.4.1.31.6 default 0
    let mut settings: Vec<u8> = Vec::new();
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::CONTENT_COMP_ALGO => algo = read_uint(r, e.size as usize)?,
            ids::CONTENT_COMP_SETTINGS => settings = read_bytes(r, e.size as usize)?,
            _ => skip(r, e.size)?,
        }
    }
    Ok((algo, settings))
}

/// Parse a `ContentEncryption` master (RFC 9559 §5.1.4.1.31.8) into
/// `(ContentEncAlgo, ContentEncKeyID, AESSettingsCipherMode)`. The cipher
/// mode is read from the nested `ContentEncAESSettings` master.
fn parse_content_encryption(r: &mut dyn ReadSeek, end: u64) -> Result<(u64, Vec<u8>, Option<u64>)> {
    let mut algo: u64 = ids::CONTENT_ENC_ALGO_NONE; // §5.1.4.1.31.9 default 0
    let mut key_id: Vec<u8> = Vec::new();
    let mut cipher_mode: Option<u64> = None;
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::CONTENT_ENC_ALGO => algo = read_uint(r, e.size as usize)?,
            ids::CONTENT_ENC_KEY_ID => key_id = read_bytes(r, e.size as usize)?,
            ids::CONTENT_ENC_AES_SETTINGS => {
                let body_end = r.stream_position()? + e.size;
                cipher_mode = parse_aes_settings(r, body_end)?;
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok((algo, key_id, cipher_mode))
}

/// Parse a `ContentEncAESSettings` master (RFC 9559 §5.1.4.1.31.11) and
/// return its `AESSettingsCipherMode` (§5.1.4.1.31.12), if present.
fn parse_aes_settings(r: &mut dyn ReadSeek, end: u64) -> Result<Option<u64>> {
    let mut mode: Option<u64> = None;
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::AES_SETTINGS_CIPHER_MODE => mode = Some(read_uint(r, e.size as usize)?),
            _ => skip(r, e.size)?,
        }
    }
    Ok(mode)
}

/// Parse `TrackOperation` (RFC 9559 §5.1.4.1.30): a virtual track built
/// from other tracks via `TrackCombinePlanes` (3D plane combining) and/or
/// `TrackJoinBlocks` (block timeline joining).
fn parse_track_operation(r: &mut dyn ReadSeek, end: u64, op: &mut RawTrackOperation) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::TRACK_COMBINE_PLANES => {
                let body_end = r.stream_position()? + e.size;
                parse_combine_planes(r, body_end, op)?;
            }
            ids::TRACK_JOIN_BLOCKS => {
                let body_end = r.stream_position()? + e.size;
                parse_join_blocks(r, body_end, op)?;
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

/// Parse `TrackCombinePlanes` (RFC 9559 §5.1.4.1.30.1) — a list of
/// `TrackPlane` masters, each carrying a `TrackPlaneUID` + `TrackPlaneType`.
fn parse_combine_planes(r: &mut dyn ReadSeek, end: u64, op: &mut RawTrackOperation) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::TRACK_PLANE => {
                let body_end = r.stream_position()? + e.size;
                let mut uid: Option<u64> = None;
                // Default per Table 20 / §5.1.4.1.30.4: absent type means 0
                // (left eye), but we only keep it when a UID is present.
                let mut plane_type: u64 = ids::TRACK_PLANE_TYPE_LEFT_EYE;
                while r.stream_position()? < body_end {
                    let ce = read_element_header(r)?;
                    match ce.id {
                        ids::TRACK_PLANE_UID => uid = Some(read_uint(r, ce.size as usize)?),
                        ids::TRACK_PLANE_TYPE => plane_type = read_uint(r, ce.size as usize)?,
                        _ => skip(r, ce.size)?,
                    }
                }
                // TrackPlaneUID is mandatory (minOccurs=1) and "not 0"; a
                // plane without one is malformed and dropped.
                if let Some(u) = uid {
                    if u != 0 {
                        op.planes.push((u, plane_type));
                    }
                }
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

/// Parse `TrackJoinBlocks` (RFC 9559 §5.1.4.1.30.5) — a list of
/// `TrackJoinUID`s naming tracks whose Blocks are joined into this one.
fn parse_join_blocks(r: &mut dyn ReadSeek, end: u64, op: &mut RawTrackOperation) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::TRACK_JOIN_UID => {
                let u = read_uint(r, e.size as usize)?;
                // "not 0" per §5.1.4.1.30.6.
                if u != 0 {
                    op.join_uids.push(u);
                }
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

fn parse_audio(r: &mut dyn ReadSeek, end: u64, t: &mut TrackEntry) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::SAMPLING_FREQUENCY => t.sample_rate = read_float(r, e.size as usize)?,
            ids::CHANNELS => t.channels = read_uint(r, e.size as usize)?,
            ids::BIT_DEPTH => t.bit_depth = read_uint(r, e.size as usize)?,
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

fn parse_video(r: &mut dyn ReadSeek, end: u64, t: &mut TrackEntry) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::PIXEL_WIDTH => t.width = read_uint(r, e.size as usize)?,
            ids::PIXEL_HEIGHT => t.height = read_uint(r, e.size as usize)?,
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

// --- Demuxer state machine ------------------------------------------------

enum ClusterState {
    /// Not inside a cluster; the next read must start with a Cluster header.
    Idle,
    /// Inside a Cluster, reading children. `body_end` is where the cluster ends.
    InCluster {
        body_end: u64,
        cluster_timecode: i64,
    },
}

/// Matroska / WebM demuxer.
///
/// Constructed via [`open`] (returning a boxed [`Demuxer`] trait object,
/// the common path used by the container registry) or [`open_typed`]
/// (returning this struct directly so consumers can call typed accessors
/// like [`MkvDemuxer::tags`] that the trait does not expose).
pub struct MkvDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    track_index_by_number: std::collections::HashMap<u64, u32>,
    /// Reverse of `track_index_by_number`: stream index → MKV TrackNumber.
    track_number_by_index: Vec<u64>,
    /// Byte offset of the Segment payload start (immediately after the
    /// Segment element's header). Cue `cluster_offset` values are relative
    /// to this position.
    segment_data_start: u64,
    segment_data_end: u64,
    cluster_state: ClusterState,
    out_queue: std::collections::VecDeque<Packet>,
    time_base: TimeBase,
    metadata: Vec<(String, String)>,
    duration_micros: i64,
    /// Cue index entries, sorted by (track, time). Empty if the file has
    /// no Cues element — `seek_to` returns `Error::Unsupported` in that
    /// case.
    cues: Vec<CueEntry>,
    /// Nanoseconds per Matroska timecode tick (the Segment\Info\TimecodeScale
    /// value, defaulted to 1_000_000 when absent).
    timecode_scale_ns: u64,
    /// Typed `Tags\Tag` collection (RFC 9559 §5.1.8.1) — see
    /// [`MkvDemuxer::tags`].
    tags: Vec<Tag>,
    /// Typed `Chapters\EditionEntry` tree (RFC 9559 §5.1.7) — see
    /// [`MkvDemuxer::chapters`]. Empty when the file carries no
    /// `Chapters` element.
    editions: Vec<Edition>,
    /// Per-Top-Level-element `CRC-32` validation results (RFC 8794
    /// §11.3.1) — see [`MkvDemuxer::crc_status`].
    crc_status: Vec<CrcStatus>,
    /// Per-stream `TrackOperation` (RFC 9559 §5.1.4.1.30), indexed by
    /// stream index. `None` for tracks that aren't virtual tracks — see
    /// [`MkvDemuxer::track_operations`].
    track_operations: Vec<Option<TrackOperation>>,
    /// Per-stream `ContentEncodings` (RFC 9559 §5.1.4.1.31), indexed by
    /// stream index. `None` for tracks with no encodings — see
    /// [`MkvDemuxer::content_encodings`].
    content_encodings: Vec<Option<ContentEncodings>>,
    /// Per-stream Header-Stripping prefix (RFC 9559 §5.1.4.1.31.6 algo 3,
    /// §5.1.4.1.31.7), indexed by stream index. When a track's *entire*
    /// Block-scoped `ContentEncodings` chain is composed only of
    /// Header-Stripping compressions, this holds the bytes to prepend to
    /// every de-laced frame so emitted packets carry the original frame
    /// data. Empty `Vec` when there is nothing to prepend (the common case,
    /// or when the chain contains a step this container can't undo — zlib /
    /// encryption — in which case packets pass through encoded). See
    /// [`compute_header_strip_prefix`].
    header_strip_prefixes: Vec<Vec<u8>>,
}

impl Demuxer for MkvDemuxer {
    fn format_name(&self) -> &str {
        "matroska"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            if let Some(p) = self.out_queue.pop_front() {
                return Ok(p);
            }
            self.advance()?;
        }
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        if self.duration_micros > 0 {
            Some(self.duration_micros)
        } else {
            None
        }
    }

    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        if stream_index as usize >= self.streams.len() {
            return Err(Error::invalid(format!(
                "MKV: stream index {stream_index} out of range"
            )));
        }
        if self.cues.is_empty() {
            return Err(Error::unsupported(
                "MKV: no Cues index in file — cannot seek",
            ));
        }
        let track_number = self.track_number_by_index[stream_index as usize];

        // Convert the stream's pts → Matroska ticks.
        //   pts_seconds  = pts * stream.time_base.num / stream.time_base.den
        //   ticks        = pts_seconds * 1e9 / timecode_scale_ns
        //                = pts * num * 1e9 / (den * timecode_scale_ns)
        // Every stream in this demuxer currently exposes the segment time
        // base (timecode_scale_ns / 1e9), so the conversion collapses to
        // a copy — but we still do the full calculation so behaviour is
        // correct when other time bases are supplied.
        let stream_tb = self.streams[stream_index as usize].time_base.as_rational();
        let target_ticks_i128: i128 = if stream_tb.num == 0 || stream_tb.den == 0 {
            pts as i128
        } else {
            let numer = pts as i128 * stream_tb.num as i128 * 1_000_000_000i128;
            let denom = stream_tb.den as i128 * self.timecode_scale_ns as i128;
            if denom == 0 {
                pts as i128
            } else {
                numer / denom
            }
        };
        let target_ticks: u64 = target_ticks_i128.max(0) as u64;

        // Find last cue entry for this track with time <= target_ticks.
        // Cues are sorted by (track, time); use a manual scan of the
        // contiguous track block to keep the code obvious and panic-free.
        let mut best: Option<&CueEntry> = None;
        for c in self.cues.iter().filter(|c| c.track == track_number) {
            if c.time <= target_ticks {
                best = Some(c);
            } else {
                break;
            }
        }
        // If target is before the first cue, fall back to the first cue
        // for this track (seek returns the actual landed pts).
        if best.is_none() {
            best = self.cues.iter().find(|c| c.track == track_number);
        }
        let cue = best.ok_or_else(|| {
            Error::unsupported(format!(
                "MKV: no Cues entries for track {track_number} (stream {stream_index})"
            ))
        })?;
        // Copy the fields out so the immutable borrow of `self.cues` can
        // be released before we re-borrow `self` mutably to drive the
        // input reader.
        let cue_time = cue.time;
        let cue_cluster_offset = cue.cluster_offset;
        let relative_position = cue.relative_position;

        let abs = self.segment_data_start + cue_cluster_offset;
        self.input.seek(SeekFrom::Start(abs))?;
        // Reset cluster reader state + any previously queued packets.
        self.cluster_state = ClusterState::Idle;
        self.out_queue.clear();

        // RFC 9559 §5.1.5.1.2.3: when the Cues entry carries a
        // `CueRelativePosition`, the referenced SimpleBlock / BlockGroup
        // sits `relative_position` bytes into the Cluster's body (where
        // `0` is the first possible position for an element inside that
        // Cluster — i.e. immediately after the Cluster element's id+size
        // header). Honour it by pre-opening the Cluster, capturing its
        // Timestamp (RFC 9559 §5.1.3.1 SHOULD be the first child element
        // of the Cluster — Cluster timecode is mandatory for decoding
        // any Block timestamp), and then jumping the reader to the exact
        // block, skipping any earlier blocks the cue is not interested
        // in.
        //
        // Without a relative position we leave the reader at the Cluster
        // header and fall back to the regular `advance()` loop (which
        // walks every child from the start), preserving the legacy
        // behaviour byte-for-byte.
        if let Some(rel) = relative_position {
            self.apply_cue_relative_position(rel)?;
        }

        // Convert the landed ticks back into the stream's time base.
        let landed_pts: i64 = if stream_tb.num == 0 || stream_tb.den == 0 {
            cue_time as i64
        } else {
            let numer = cue_time as i128 * stream_tb.den as i128 * self.timecode_scale_ns as i128;
            let denom = stream_tb.num as i128 * 1_000_000_000i128;
            if denom == 0 {
                cue_time as i64
            } else {
                (numer / denom) as i64
            }
        };
        Ok(landed_pts)
    }
}

impl MkvDemuxer {
    /// Typed `Tags\Tag` collection (RFC 9559 §5.1.8.1) parsed from the
    /// Segment, with every `Targets\Tag*UID` already resolved against the
    /// Segment's track / edition / chapter / attachment tables.
    ///
    /// Surfaces information the flattened [`Demuxer::metadata`] view drops:
    /// `TargetType` / `TargetTypeValue` informational hints
    /// ([`Targets::target_type`] / [`Targets::target_type_value`]),
    /// per-`SimpleTag` language ([`SimpleTag::language`] /
    /// [`SimpleTag::language_bcp47`]), the [`SimpleTag::default`] flag,
    /// binary tag payloads ([`SimpleTagValue::Binary`]), and the original
    /// case of [`SimpleTag::name`] (metadata keys are lower-cased).
    ///
    /// Returned in segment order. Tags whose `Targets` master had only
    /// dangling non-zero UIDs are dropped per RFC 9559 §5.1.8.1.1.3..6.
    pub fn tags(&self) -> &[Tag] {
        &self.tags
    }

    /// `CRC-32` validation results for the Top-Level master elements that
    /// carried a checksum (RFC 8794 §11.3.1, RFC 9559 §6.2).
    ///
    /// Matroska files SHOULD put a `CRC-32` child as the first element of
    /// each Top-Level master (Info, Tracks, Tags, Cues, Chapters,
    /// Attachments, SeekHead). When the demuxer parses one with such a
    /// child, it recomputes the IEEE CRC-32 over the rest of the element
    /// and records a [`CrcStatus`]. Elements without a `CRC-32` child are
    /// not represented — the spec lets a writer omit them.
    ///
    /// Validation is informational: a mismatching CRC does **not** stop
    /// the demuxer from returning packets (the spec only says a reader
    /// *MAY* ignore the data). Callers that want strict integrity can
    /// inspect this slice and reject a file with any
    /// [`CrcStatus::is_valid`] == `false`.
    ///
    /// Returned in the order the elements appear in the Segment.
    pub fn crc_status(&self) -> &[CrcStatus] {
        &self.crc_status
    }

    /// Typed `Chapters\EditionEntry` tree (RFC 9559 §5.1.7) parsed from
    /// the Segment.
    ///
    /// Surfaces information the flattened [`Demuxer::metadata`] view drops:
    /// every [`Edition`] keeps its [`Edition::default`] /
    /// [`Edition::ordered`] flags and [`Edition::uid`]; every [`Chapter`]
    /// keeps its [`Chapter::uid`], [`Chapter::string_uid`],
    /// [`Chapter::hidden`] flag, full nanosecond-precision
    /// [`Chapter::time_start_ns`] / [`Chapter::time_end_ns`], **all**
    /// multilingual [`Chapter::displays`] rows (the flat view picks one
    /// title), and any nested [`Chapter::children`].
    ///
    /// Returned in the order editions and atoms appear in the Segment.
    /// Empty when the file carries no `Chapters` element.
    pub fn chapters(&self) -> &[Edition] {
        &self.editions
    }

    /// `TrackOperation` (RFC 9559 §5.1.4.1.30) for the stream at
    /// `stream_index`, or `None` when that track is an ordinary (non-virtual)
    /// track.
    ///
    /// A `TrackOperation` marks a *virtual* track assembled from other
    /// tracks: either a stereoscopic 3D track combining video planes
    /// ([`TrackOperation::planes`]) or a track joining several other tracks'
    /// Blocks into one timeline ([`TrackOperation::join_tracks`]). The
    /// referenced tracks are reported as [`TrackRef`]s carrying both the
    /// on-disk `TrackUID` and, when resolvable, the matching stream index.
    ///
    /// Returns `None` for an out-of-range `stream_index`.
    pub fn track_operation(&self, stream_index: u32) -> Option<&TrackOperation> {
        self.track_operations
            .get(stream_index as usize)
            .and_then(|o| o.as_ref())
    }

    /// All per-stream `TrackOperation`s (RFC 9559 §5.1.4.1.30), indexed by
    /// stream index. The slice has one entry per stream — `None` for
    /// ordinary tracks, `Some` for virtual tracks. See
    /// [`MkvDemuxer::track_operation`] for the semantics.
    pub fn track_operations(&self) -> &[Option<TrackOperation>] {
        &self.track_operations
    }

    /// `ContentEncodings` (RFC 9559 §5.1.4.1.31) for the stream at
    /// `stream_index`, or `None` when the track declares no encodings (the
    /// common case for plain, uncompressed/unencrypted tracks).
    ///
    /// A track's `ContentEncodings` describes the chain of transformations —
    /// compression and/or encryption — that were applied to its frame data
    /// and/or `CodecPrivate` before the bytes were written into Blocks. The
    /// container surfaces these *headers* only: it never decompresses or
    /// decrypts a frame. A caller that wants the raw codec bytes back must
    /// undo each [`ContentEncoding`] itself, iterating
    /// [`ContentEncodings::encodings`] front-to-back (the demuxer pre-sorts
    /// it into decode order — highest `ContentEncodingOrder` first, per
    /// §5.1.4.1.31.2).
    ///
    /// Returns `None` for an out-of-range `stream_index`.
    pub fn content_encodings(&self, stream_index: u32) -> Option<&ContentEncodings> {
        self.content_encodings
            .get(stream_index as usize)
            .and_then(|o| o.as_ref())
    }

    /// All per-stream `ContentEncodings` (RFC 9559 §5.1.4.1.31), indexed by
    /// stream index. The slice has one entry per stream — `None` for tracks
    /// with no encodings. See [`MkvDemuxer::content_encodings`] for the
    /// semantics.
    pub fn all_content_encodings(&self) -> &[Option<ContentEncodings>] {
        &self.content_encodings
    }

    /// Apply a `CueRelativePosition` (RFC 9559 §5.1.5.1.2.3) after a
    /// `seek_to` has positioned the reader at a Cluster header.
    ///
    /// Opens the Cluster (reads its id+size), captures the cluster
    /// Timestamp (RFC 9559 §5.1.3.1 — SHOULD be the first child of the
    /// Cluster; we walk children until we find it, or until we reach
    /// `body_start + relative_position`, whichever comes first), and then
    /// repositions the reader to `body_start + relative_position` and
    /// installs an `InCluster` state with the captured timecode.
    ///
    /// `relative_position` is byte-distance from the first possible
    /// element position inside the Cluster (i.e. immediately after the
    /// Cluster element's id+size header).
    ///
    /// Best-effort: if the Cluster header doesn't look right, or the
    /// relative position runs past the Cluster body, the helper leaves
    /// the reader at the Cluster header and the state at `Idle` so the
    /// regular `advance()` loop can take over — i.e. it degrades to the
    /// legacy "scan the cluster from the start" path.
    fn apply_cue_relative_position(&mut self, relative_position: u64) -> Result<()> {
        let cluster_head_pos = self.input.stream_position()?;
        let e = read_element_header(&mut *self.input)?;
        if e.id != ids::CLUSTER {
            // Cue offset doesn't point at a Cluster header — let the
            // outer state machine sort it out.
            self.input.seek(SeekFrom::Start(cluster_head_pos))?;
            return Ok(());
        }
        let body_start = self.input.stream_position()?;
        let body_end = if e.size == VINT_UNKNOWN_SIZE {
            self.segment_data_end
        } else {
            body_start + e.size
        };
        let target = body_start + relative_position;
        if target > body_end {
            // Out-of-range relative position — degrade gracefully:
            // rewind to the cluster header so `advance()` can walk
            // from the start.
            self.input.seek(SeekFrom::Start(cluster_head_pos))?;
            return Ok(());
        }

        // Walk children from body_start until we either reach the
        // target or pass it, capturing Timestamp on the way. Per
        // RFC 9559 §5.1.3.1 the Timestamp SHOULD be first, so the
        // typical iteration count is 1.
        let mut cluster_timecode: i64 = 0;
        let mut pos = body_start;
        while pos < target {
            self.input.seek(SeekFrom::Start(pos))?;
            let child = match read_element_header(&mut *self.input) {
                Ok(c) => c,
                Err(_) => {
                    self.input.seek(SeekFrom::Start(cluster_head_pos))?;
                    return Ok(());
                }
            };
            // Compute the position right after the child (id+size+body).
            let child_body_start = self.input.stream_position()?;
            if child.id == ids::TIMECODE {
                cluster_timecode = read_uint(&mut *self.input, child.size as usize)? as i64;
            }
            let next = child_body_start + child.size;
            if next > body_end || next <= pos {
                // Malformed child — degrade.
                self.input.seek(SeekFrom::Start(cluster_head_pos))?;
                return Ok(());
            }
            pos = next;
        }
        // `pos` now equals `target` (or `target` was 0 and we never
        // entered the loop). Seek to the target and install the
        // InCluster state.
        self.input.seek(SeekFrom::Start(target))?;
        self.cluster_state = ClusterState::InCluster {
            body_end,
            cluster_timecode,
        };
        Ok(())
    }

    fn advance(&mut self) -> Result<()> {
        match self.cluster_state {
            ClusterState::Idle => {
                let pos = self.input.stream_position()?;
                if pos >= self.segment_data_end {
                    return Err(Error::Eof);
                }
                let e = read_element_header(&mut *self.input)?;
                match e.id {
                    ids::CLUSTER => {
                        let body_start = self.input.stream_position()?;
                        let body_end = if e.size == VINT_UNKNOWN_SIZE {
                            self.segment_data_end
                        } else {
                            body_start + e.size
                        };
                        self.cluster_state = ClusterState::InCluster {
                            body_end,
                            cluster_timecode: 0,
                        };
                        Ok(())
                    }
                    ids::CUES | ids::ATTACHMENTS | ids::CHAPTERS | ids::TAGS => {
                        // Skip — not packet data.
                        skip(&mut *self.input, e.size)?;
                        Ok(())
                    }
                    _ => {
                        // Unknown element at top level — skip it.
                        skip(&mut *self.input, e.size)?;
                        Ok(())
                    }
                }
            }
            ClusterState::InCluster {
                body_end,
                cluster_timecode,
            } => {
                let pos = self.input.stream_position()?;
                if pos >= body_end {
                    self.cluster_state = ClusterState::Idle;
                    return Ok(());
                }
                let e = read_element_header(&mut *self.input)?;
                match e.id {
                    ids::TIMECODE => {
                        let v = read_uint(&mut *self.input, e.size as usize)? as i64;
                        if let ClusterState::InCluster {
                            ref mut cluster_timecode,
                            ..
                        } = self.cluster_state
                        {
                            *cluster_timecode = v;
                        }
                    }
                    ids::SIMPLE_BLOCK => {
                        let bytes = read_bytes(&mut *self.input, e.size as usize)?;
                        self.queue_block_packets(&bytes, cluster_timecode, false)?;
                    }
                    ids::BLOCK_GROUP => {
                        let bg_end = self.input.stream_position()? + e.size;
                        self.parse_block_group(bg_end, cluster_timecode)?;
                    }
                    // An unknown-size Cluster (body_end == segment_data_end)
                    // terminates when a sibling Segment-child element is
                    // encountered. Rewind to the start of that element and
                    // fall back to Idle so the outer loop can dispatch it.
                    ids::CLUSTER
                    | ids::CUES
                    | ids::TAGS
                    | ids::ATTACHMENTS
                    | ids::CHAPTERS
                    | ids::SEEK_HEAD
                    | ids::INFO
                    | ids::TRACKS => {
                        self.input.seek(SeekFrom::Start(pos))?;
                        self.cluster_state = ClusterState::Idle;
                    }
                    _ => skip(&mut *self.input, e.size)?,
                }
                Ok(())
            }
        }
    }

    fn parse_block_group(&mut self, end: u64, cluster_timecode: i64) -> Result<()> {
        let mut block_bytes: Option<Vec<u8>> = None;
        let mut duration: Option<i64> = None;
        let mut is_keyframe = true;
        while self.input.stream_position()? < end {
            let e = read_element_header(&mut *self.input)?;
            match e.id {
                ids::BLOCK => {
                    block_bytes = Some(read_bytes(&mut *self.input, e.size as usize)?);
                }
                ids::BLOCK_DURATION => {
                    duration = Some(read_uint(&mut *self.input, e.size as usize)? as i64);
                }
                ids::REFERENCE_BLOCK => {
                    is_keyframe = false;
                    skip(&mut *self.input, e.size)?;
                }
                _ => skip(&mut *self.input, e.size)?,
            }
        }
        if let Some(b) = block_bytes {
            // For BlockGroup, the lacing flags are in the same place as
            // SimpleBlock (the "keyframe" bit doesn't exist in plain Block —
            // keyframe-ness is inferred from absence of ReferenceBlock).
            self.queue_block_packets_with(&b, cluster_timecode, is_keyframe, duration)?;
        }
        Ok(())
    }

    fn queue_block_packets(
        &mut self,
        bytes: &[u8],
        cluster_timecode: i64,
        _hint: bool,
    ) -> Result<()> {
        // SimpleBlock: keyframe bit is bit 7 of flags byte.
        // BlockGroup/Block has the same layout but no keyframe bit.
        // We pass through whatever's set in the flags byte for SimpleBlock.
        self.queue_block_packets_with(bytes, cluster_timecode, true, None)
    }

    fn queue_block_packets_with(
        &mut self,
        bytes: &[u8],
        cluster_timecode: i64,
        default_keyframe: bool,
        explicit_duration: Option<i64>,
    ) -> Result<()> {
        let mut cur = std::io::Cursor::new(bytes);
        let (track_number, _) = crate::ebml::read_vint(&mut cur, false)?;
        let mut tc_buf = [0u8; 2];
        cur.read_exact(&mut tc_buf)?;
        let timecode_offset = i16::from_be_bytes(tc_buf) as i64;
        let mut flags_buf = [0u8; 1];
        cur.read_exact(&mut flags_buf)?;
        let flags = flags_buf[0];
        let lacing = (flags >> 1) & 0x03;
        let keyframe_flag = flags & 0x80 != 0;

        let stream_idx = match self.track_index_by_number.get(&track_number) {
            Some(i) => *i,
            None => return Ok(()), // Skip frames for unknown tracks.
        };

        // Frame data starts at current cur position.
        let body_start = cur.position() as usize;
        let body = &bytes[body_start..];

        let frames = match lacing {
            0 => vec![body.to_vec()],
            1 => parse_xiph_lacing(body)?,
            2 => parse_fixed_lacing(body)?,
            3 => parse_ebml_lacing(body)?,
            _ => unreachable!(),
        };

        let pts_base = cluster_timecode + timecode_offset;
        let n_frames = frames.len() as i64;
        let per_frame = explicit_duration.map(|d| d / n_frames.max(1));
        // Header-Stripping (RFC 9559 §5.1.4.1.31.6 algo 3) prefix for this
        // stream, prepended to each de-laced frame so the packet carries the
        // original (un-stripped) bytes. Block scope (§5.1.4.1.31.3 bit 0x1) is
        // "all frame contents, excluding lacing data" — i.e. each frame after
        // lacing is split, which is exactly `f` here. Empty when the track has
        // no reversible Header-Stripping chain (the common case).
        let strip_prefix = self
            .header_strip_prefixes
            .get(stream_idx as usize)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for (i, f) in frames.into_iter().enumerate() {
            let pts = pts_base + per_frame.unwrap_or(0) * i as i64;
            let frame_bytes = if strip_prefix.is_empty() {
                f
            } else {
                let mut restored = Vec::with_capacity(strip_prefix.len() + f.len());
                restored.extend_from_slice(strip_prefix);
                restored.extend_from_slice(&f);
                restored
            };
            let mut pkt = Packet::new(stream_idx, self.time_base, frame_bytes);
            pkt.pts = Some(pts);
            pkt.dts = Some(pts);
            pkt.duration = per_frame;
            pkt.flags.keyframe = keyframe_flag || default_keyframe;
            self.out_queue.push_back(pkt);
        }
        Ok(())
    }
}

// --- Lacing helpers --------------------------------------------------------

fn parse_xiph_lacing(body: &[u8]) -> Result<Vec<Vec<u8>>> {
    if body.is_empty() {
        return Ok(vec![]);
    }
    let n_frames = body[0] as usize + 1;
    let mut sizes = Vec::with_capacity(n_frames);
    let mut i = 1;
    for _ in 0..n_frames - 1 {
        let mut s = 0usize;
        loop {
            if i >= body.len() {
                return Err(Error::invalid("MKV xiph lacing: truncated size"));
            }
            let b = body[i];
            i += 1;
            s += b as usize;
            if b < 255 {
                break;
            }
        }
        sizes.push(s);
    }
    // Last frame size is whatever's left.
    let used: usize = sizes.iter().sum();
    let last_size = body.len() - i - used;
    sizes.push(last_size);
    let mut frames = Vec::with_capacity(n_frames);
    for s in sizes {
        if i + s > body.len() {
            return Err(Error::invalid("MKV xiph lacing: frame exceeds body"));
        }
        frames.push(body[i..i + s].to_vec());
        i += s;
    }
    Ok(frames)
}

fn parse_fixed_lacing(body: &[u8]) -> Result<Vec<Vec<u8>>> {
    if body.is_empty() {
        return Ok(vec![]);
    }
    let n_frames = body[0] as usize + 1;
    let payload = &body[1..];
    if payload.len() % n_frames != 0 {
        return Err(Error::invalid("MKV fixed lacing: non-divisible payload"));
    }
    let frame_size = payload.len() / n_frames;
    let mut frames = Vec::with_capacity(n_frames);
    for c in payload.chunks_exact(frame_size) {
        frames.push(c.to_vec());
    }
    Ok(frames)
}

fn parse_ebml_lacing(body: &[u8]) -> Result<Vec<Vec<u8>>> {
    if body.is_empty() {
        return Ok(vec![]);
    }
    let mut cur = std::io::Cursor::new(body);
    let n_frames = {
        let mut buf = [0u8; 1];
        cur.read_exact(&mut buf)?;
        buf[0] as usize + 1
    };
    let mut sizes = Vec::with_capacity(n_frames);
    // First size: full VINT.
    let (first, _) = crate::ebml::read_vint(&mut cur, false)?;
    sizes.push(first as i64);
    // Remaining sizes: signed deltas (raw VINT minus mid-of-range bias).
    for _ in 0..n_frames - 2 {
        let (raw, w) = crate::ebml::read_vint(&mut cur, false)?;
        let bias = ((1i64) << (7 * w as i64 - 1)) - 1;
        let signed = (raw as i64) - bias;
        let prev = *sizes.last().unwrap();
        sizes.push(prev + signed);
    }
    // Last frame is whatever remains.
    let pos = cur.position() as usize;
    let used: i64 = sizes.iter().sum();
    let last = body.len() as i64 - pos as i64 - used;
    sizes.push(last);
    let mut frames = Vec::with_capacity(n_frames);
    let mut i = pos;
    for s in sizes {
        if s < 0 || i + s as usize > body.len() {
            return Err(Error::invalid("MKV ebml lacing: invalid frame size"));
        }
        frames.push(body[i..i + s as usize].to_vec());
        i += s as usize;
    }
    Ok(frames)
}
