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
    let ebml_end = input.stream_position()?.saturating_add(hdr.size);
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
    // Typed `Attachments\AttachedFile` list (RFC 9559 §5.1.6), surfaced via
    // `MkvDemuxer::attachments` — populated by `parse_attachments` alongside
    // the flat `attachment:N:*` metadata view. Each entry remembers the
    // on-disk byte range of its `FileData` payload so callers can pull the
    // bytes on demand via `MkvDemuxer::attachment_data` without paying for
    // them at open time.
    let mut attachments: Vec<Attachment> = Vec::new();
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
            Some(body_start.saturating_add(e.size))
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
                    &mut attachments,
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
            if scan_cues_from(
                &mut *input,
                first_cluster,
                segment_data_end,
                &mut cues,
                &mut crc_status,
            )
            .is_err()
            {
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
        // RFC 9559 §5.1.4.1.2.1: surface the per-track Language when
        // the file carried one. We deliberately do NOT synthesise the
        // spec default "eng" here — callers iterating streams keep the
        // "absent" signal so re-muxing doesn't add a Language element
        // that wasn't in the source.
        if let Some(lang) = t.language.clone() {
            params.language = Some(lang);
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

    // Per-stream `BlockAdditionMapping`s (RFC 9559 §5.1.4.1.17), indexed by
    // stream index. Each entry is one mapping master in on-disk order;
    // tracks with no `BlockAdditionMapping` child get an empty `Vec`.
    let block_addition_mappings: Vec<Vec<BlockAdditionMapping>> = tracks
        .iter()
        .map(|t| t.block_addition_mappings.clone())
        .collect();

    // Per-stream `VideoInterlacing` (RFC 9559 §5.1.4.1.28.1 + §5.1.4.1.28.2),
    // indexed by stream index. `None` for non-video tracks and for video
    // tracks whose `TrackEntry` had no `Video` master; for everything else
    // the spec defaults (FlagInterlaced=0, FieldOrder=2) are materialised
    // by `parse_video`.
    let video_interlacings: Vec<Option<VideoInterlacing>> = tracks
        .iter()
        .map(|t| {
            t.interlacing_raw.map(|(flag, fo)| VideoInterlacing {
                flag: FlagInterlaced::from_raw(flag),
                field_order_raw: fo,
            })
        })
        .collect();

    // Per-stream `VideoGeometry` (RFC 9559 §5.1.4.1.28.8..§5.1.4.1.28.14),
    // indexed by stream index. `None` for tracks with no `Video` master.
    // PixelCrop defaults (§5.1.4.1.28.8..11) are materialised as `0` by
    // `parse_video`; the typed surface materialises the §5.1.4.1.28.12 /
    // §5.1.4.1.28.13 derived defaults for DisplayWidth / DisplayHeight
    // (`PixelWidth - crop_left - crop_right` and `PixelHeight - crop_top -
    // crop_bottom`) only when DisplayUnit is `0` (pixels) and the file
    // omitted the explicit element, per the spec note "If the DisplayUnit
    // of the same TrackEntry is 0, then the default value ...; else, there
    // is no default value".
    // Per-stream `VideoColour` (RFC 9559 §5.1.4.1.28.16), indexed by stream
    // index. `None` for non-video tracks and for video tracks whose `Video`
    // master carried no `Colour` child. When the `Colour` master was present
    // but a particular child was absent, `parse_video` materialises the spec
    // default in `RawColour` (`2` for matrix/transfer/primaries, `0` for the
    // chroma siting / range / bits-per-channel) — see the per-field doc on
    // [`VideoColour`].
    let video_colours: Vec<Option<VideoColour>> = tracks
        .iter()
        .map(|t| {
            t.colour_raw.as_ref().map(|c| VideoColour {
                matrix_coefficients: MatrixCoefficients::from_raw(c.matrix_coefficients),
                bits_per_channel: c.bits_per_channel,
                chroma_subsampling_horz: c.chroma_subsampling_horz,
                chroma_subsampling_vert: c.chroma_subsampling_vert,
                cb_subsampling_horz: c.cb_subsampling_horz,
                cb_subsampling_vert: c.cb_subsampling_vert,
                chroma_siting_horz: ChromaSitingHorz::from_raw(c.chroma_siting_horz),
                chroma_siting_vert: ChromaSitingVert::from_raw(c.chroma_siting_vert),
                range: ColourRange::from_raw(c.range),
                transfer_characteristics: TransferCharacteristics::from_raw(
                    c.transfer_characteristics,
                ),
                primaries: Primaries::from_raw(c.primaries),
                max_cll: c.max_cll,
                max_fall: c.max_fall,
                mastering_metadata: c.mastering_metadata,
            })
        })
        .collect();

    // Per-stream `StereoMode` (RFC 9559 §5.1.4.1.28.3), indexed by stream
    // index. `None` when the track has no `Video` master; the spec default
    // `0` (mono) is already materialised in `parse_video` so a `Video`
    // master with no explicit `StereoMode` decodes as `Some(StereoMode::Mono)`.
    let video_stereo_modes: Vec<Option<StereoMode>> = tracks
        .iter()
        .map(|t| t.stereo_mode_raw.map(StereoMode::from_raw))
        .collect();

    // Per-stream `Projection` (RFC 9559 §5.1.4.1.28.41), indexed by stream
    // index. `None` for non-video tracks and for video tracks whose `Video`
    // master carried no `Projection` child. `parse_video` materialises the
    // spec defaults inside `RawProjection` (ProjectionType `0` rectangular,
    // pose components `0.0`) — so an empty `Projection` master decodes to a
    // fully-typed identity projection.
    let video_projections: Vec<Option<Projection>> = tracks
        .iter()
        .map(|t| {
            t.projection_raw.as_ref().map(|p| Projection {
                projection_type: ProjectionType::from_raw(p.projection_type_raw),
                private: p.private.clone(),
                pose_yaw: p.pose_yaw,
                pose_pitch: p.pose_pitch,
                pose_roll: p.pose_roll,
            })
        })
        .collect();

    // Per-stream `AlphaMode` (RFC 9559 §5.1.4.1.28.4), indexed by stream
    // index. `None` for tracks with no `Video` master; the spec default `0`
    // ([`AlphaMode::None`]) is materialised inside `parse_video` so an empty
    // `Video` master decodes as `Some(AlphaMode::None)`.
    let video_alpha_modes: Vec<Option<AlphaMode>> = tracks
        .iter()
        .map(|t| t.alpha_mode_raw.map(AlphaMode::from_raw))
        .collect();

    // Per-stream `AspectRatioType` (RFC 9559 Appendix A.24 "Reclaimed"),
    // indexed by stream index. `None` whenever the file did not carry the
    // element — the reclaimed appendix specifies no default, so absence is
    // never synthesised.
    let video_aspect_ratio_types: Vec<Option<u64>> =
        tracks.iter().map(|t| t.aspect_ratio_type_raw).collect();

    // Per-stream `UncompressedFourCC` (RFC 9559 §5.1.4.1.28.15), indexed by
    // stream index. `None` whenever the file did not carry the element
    // (§5.1.4.1.28.15 Table 11 makes it mandatory only when
    // `CodecID == "V_UNCOMPRESSED"` and there is no spec default).
    let video_uncompressed_fourccs: Vec<Option<UncompressedFourCC>> = tracks
        .iter()
        .map(|t| {
            t.uncompressed_fourcc_raw
                .as_ref()
                .map(|raw| UncompressedFourCC { bytes: raw.clone() })
        })
        .collect();

    let video_geometries: Vec<Option<VideoGeometry>> = tracks
        .iter()
        .map(|t| {
            t.geometry_raw
                .map(|(top, bottom, left, right, dw_raw, dh_raw, unit_raw)| {
                    let unit = DisplayUnit::from_raw(unit_raw);
                    let display_width = if dw_raw != 0 {
                        // Explicit DisplayWidth (range "not 0") overrides the
                        // derivation in every DisplayUnit mode.
                        Some(dw_raw)
                    } else if matches!(unit, DisplayUnit::Pixels) {
                        // Derived pixel-mode default: PixelWidth - crop_left
                        // - crop_right, only when it does not underflow
                        // (a malformed file with crops bigger than the encoded
                        // width produces `None` instead of an underflowed
                        // value).
                        t.width.checked_sub(left).and_then(|v| v.checked_sub(right))
                    } else {
                        None
                    };
                    let display_height = if dh_raw != 0 {
                        Some(dh_raw)
                    } else if matches!(unit, DisplayUnit::Pixels) {
                        t.height
                            .checked_sub(top)
                            .and_then(|v| v.checked_sub(bottom))
                    } else {
                        None
                    };
                    VideoGeometry {
                        pixel_crop_top: top,
                        pixel_crop_bottom: bottom,
                        pixel_crop_left: left,
                        pixel_crop_right: right,
                        display_width,
                        display_height,
                        display_unit: unit,
                    }
                })
        })
        .collect();

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
        attachments,
        crc_status,
        validated_cluster_starts: std::collections::HashSet::new(),
        track_operations,
        content_encodings,
        header_strip_prefixes,
        video_interlacings,
        video_geometries,
        video_colours,
        video_stereo_modes,
        video_projections,
        video_alpha_modes,
        video_aspect_ratio_types,
        video_uncompressed_fourccs,
        block_addition_mappings,
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
/// child as its first element (RFC 9559 §6.2). The demuxer checks the
/// elements it parses up front (Info, Tracks, Tags, Cues, Chapters,
/// Attachments, SeekHead) **and** each Cluster as it first opens it,
/// recording whether the stored CRC matched the IEEE CRC-32 of the rest
/// of the element's data. Elements with no `CRC-32` child are not
/// represented here — absence of a status means "no CRC to check,"
/// which the spec explicitly permits.
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
    /// Raw `(FlagInterlaced, FieldOrder)` captured from the `Video` master
    /// (RFC 9559 §5.1.4.1.28.1 / §5.1.4.1.28.2). `None` when the track is
    /// not a video track or the `Video` master was absent. When the `Video`
    /// master existed but neither child was present, the tuple holds the
    /// spec defaults (`0` = undetermined, `2` = undetermined) so the
    /// downstream typed surface materialises them.
    interlacing_raw: Option<(u64, u64)>,
    /// Raw video-geometry quartet captured from the `Video` master (RFC 9559
    /// §5.1.4.1.28.8..§5.1.4.1.28.14): `(PixelCropTop, PixelCropBottom,
    /// PixelCropLeft, PixelCropRight, DisplayWidth-or-0, DisplayHeight-or-0,
    /// DisplayUnit)`. `None` when the track has no `Video` master.
    /// Per §5.1.4.1.28.12 / .13 the spec ranges DisplayWidth /
    /// DisplayHeight as "not 0" — a `0` slot signals "absent from file"
    /// so the typed surface materialises the default (or returns `None`
    /// when there is no spec default).
    geometry_raw: Option<(u64, u64, u64, u64, u64, u64, u64)>,
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
    /// Raw `Colour` master (RFC 9559 §5.1.4.1.28.16) for the track, captured
    /// during the `Video` walk. `None` when the track is not a video track or
    /// its `Video` master carried no `Colour` child. When the `Colour` master
    /// existed but a particular sub-element was absent the raw record carries
    /// the spec default (e.g. `MatrixCoefficients` defaults to `2`) so the
    /// typed surface can fold defaults uniformly.
    colour_raw: Option<RawColour>,
    /// Raw `StereoMode` (RFC 9559 §5.1.4.1.28.3) captured during the `Video`
    /// walk. `None` when the track has no `Video` master. When the `Video`
    /// master exists but no `StereoMode` child is present, this carries the
    /// spec default `0` (mono) so the typed surface materialises it.
    stereo_mode_raw: Option<u64>,
    /// Raw `Projection` master (RFC 9559 §5.1.4.1.28.41) captured during the
    /// `Video` walk. `None` when the track is not a video track or its
    /// `Video` master carried no `Projection` child. When the `Projection`
    /// master existed but a particular sub-element was absent, the staging
    /// record carries the spec default (`ProjectionType` = `0` rectangular,
    /// pose-components `0.0`) so the typed surface can materialise an
    /// identity projection uniformly.
    projection_raw: Option<RawProjection>,
    /// Raw `AlphaMode` (RFC 9559 §5.1.4.1.28.4) captured during the `Video`
    /// walk. `None` when the track has no `Video` master; otherwise the
    /// spec default `0` is materialised so an empty `Video` master surfaces
    /// `Some(AlphaMode::None)` on the typed side.
    alpha_mode_raw: Option<u64>,
    /// Raw `AspectRatioType` (RFC 9559 Appendix A.24, id `0x54B3` — reclaimed)
    /// captured during the `Video` walk. `None` when the track has no `Video`
    /// master or the element was absent — the reclaimed appendix specifies
    /// no default, so absence does not materialise a synthetic value.
    aspect_ratio_type_raw: Option<u64>,
    /// Raw `UncompressedFourCC` (RFC 9559 §5.1.4.1.28.15) captured during the
    /// `Video` walk. `None` when the track has no `Video` master or the
    /// element was absent. The spec marks the field as a fixed-length 4-byte
    /// binary — a non-4-byte payload is preserved verbatim so callers can
    /// inspect the malformed file; the typed surface's `fourcc()` accessor
    /// returns `None` whenever the byte length isn't exactly 4.
    uncompressed_fourcc_raw: Option<Vec<u8>>,
    /// `Language` (RFC 9559 §5.1.4.1.2.1) — ISO 639-2/T three-letter
    /// tag for the track's primary language. `None` when the element
    /// was absent from the file; the spec default `"eng"` is *not*
    /// materialised here so the typed surface can distinguish
    /// "container said English" from "container said nothing".
    language: Option<String>,
    /// `BlockAdditionMapping` masters (RFC 9559 §5.1.4.1.17) captured during
    /// the `TrackEntry` walk, one entry per master in on-disk order. Empty
    /// when the `TrackEntry` carried no `BlockAdditionMapping` child (the
    /// common case — the element only appears on tracks that use
    /// `BlockAdditional` to extend their on-disk format, e.g. WebM alpha
    /// or HDR dynamic metadata payloads).
    block_addition_mappings: Vec<BlockAdditionMapping>,
}

/// Parser-private staging form of `Colour` — only the bits that have a
/// non-`Option` typed surface (each gets its spec default materialised here)
/// are stored as bare integers; everything that surfaces as `Option<…>` keeps
/// its `Option` so the typed builder can tell "absent" from "explicit
/// default".
#[derive(Default)]
struct RawColour {
    matrix_coefficients: u64,
    bits_per_channel: u64,
    chroma_subsampling_horz: Option<u64>,
    chroma_subsampling_vert: Option<u64>,
    cb_subsampling_horz: Option<u64>,
    cb_subsampling_vert: Option<u64>,
    chroma_siting_horz: u64,
    chroma_siting_vert: u64,
    range: u64,
    transfer_characteristics: u64,
    primaries: u64,
    max_cll: Option<u64>,
    max_fall: Option<u64>,
    mastering_metadata: Option<MasteringMetadata>,
}

/// Parser-private staging form of `Projection` (RFC 9559 §5.1.4.1.28.41).
/// Sub-element defaults are materialised here so the typed surface only has
/// to map the raw `ProjectionType` integer onto its enum. `private` stays
/// `Option` so the typed surface can distinguish "absent" (the only legal
/// state when `projection_type_raw == 0` per §5.1.4.1.28.43) from "explicit
/// empty payload".
#[derive(Default)]
struct RawProjection {
    projection_type_raw: u64,
    private: Option<Vec<u8>>,
    pose_yaw: f64,
    pose_pitch: f64,
    pose_roll: f64,
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
                let tag_end = r.stream_position()?.saturating_add(e.size);
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
                let tg_end = r.stream_position()?.saturating_add(e.size);
                parse_targets(r, tg_end, t)?;
            }
            ids::SIMPLE_TAG => {
                let st_end = r.stream_position()?.saturating_add(e.size);
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

impl Targets {
    /// Resolve [`Targets::target_type_value`] (RFC 9559 §5.1.8.1.1.1) into
    /// the typed [`TargetLevel`] hierarchy (Table 33 — `COLLECTION` ⊇
    /// `EDITION` ⊇ `ALBUM` ⊇ `PART` ⊇ `TRACK` ⊇ `SUBTRACK` ⊇ `SHOT`).
    ///
    /// Returns `None` when the `TargetTypeValue` element was absent from
    /// the file — distinguishable from `Some(TargetLevel::Album)` (which
    /// would be the spec default `50` materialised by the writer). The
    /// `TargetType` informational string (§5.1.8.1.1.2) is *not* consulted
    /// here: the spec lets a single `TargetTypeValue` row carry several
    /// equivalent `TargetType` labels (e.g. `ALBUM` / `OPERA` / `CONCERT`
    /// / `MOVIE` / `EPISODE` all map to value `50`), so the integer is the
    /// canonical hierarchy key and the string is purely a display hint.
    /// Forward-compat values registered after RFC 9559 (§27.13 leaves the
    /// "Matroska Tags Target Types" registry open) surface as
    /// [`TargetLevel::Other`] rather than being clamped or dropped.
    pub fn target_level(&self) -> Option<TargetLevel> {
        self.target_type_value.map(TargetLevel::from_raw)
    }
}

/// Hierarchical level a [`Tag`] applies to (RFC 9559 §5.1.8.1.1.1,
/// Table 33). Variants correspond to the `TargetTypeValue` rows whose
/// "lower hierarchical level" comparison rule (§5.1.8.1.1.1 usage notes:
/// "The TargetTypeValue values are meant to be compared. Higher values
/// MUST correspond to a logical level that contains the lower logical
/// level TargetTypeValue values.") underpins how a player walks an
/// album → track → subtrack hierarchy.
///
/// The variant ordering matches the spec ordering (`Shot` < `Subtrack` <
/// `Track` < `Part` < `Album` < `Edition` < `Collection`), so deriving
/// `Ord` mirrors the §5.1.8.1.1.1 containment semantics. Forward-compat
/// values registered after RFC 9559 (§27.13 leaves the registry open)
/// surface as [`TargetLevel::Other`], which sorts after every named
/// level so an unrecognised value doesn't break the comparison rule for
/// the named ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetLevel {
    /// `10` — `SHOT`. The lowest hierarchy found in music or movies.
    Shot,
    /// `20` — `SUBTRACK` / `MOVEMENT` / `SCENE`. Parts of a track for
    /// audio, such as a movement or scene in a movie.
    Subtrack,
    /// `30` — `TRACK` / `SONG` / `CHAPTER`. The common parts of an album
    /// or movie.
    Track,
    /// `40` — `PART` / `SESSION`. When an album or episode has different
    /// logical parts.
    Part,
    /// `50` — `ALBUM` / `OPERA` / `CONCERT` / `MOVIE` / `EPISODE`. The
    /// spec default for `TargetTypeValue`; the most common grouping
    /// level of music and video (e.g. an episode for TV series).
    Album,
    /// `60` — `EDITION` / `ISSUE` / `VOLUME` / `OPUS` / `SEASON` /
    /// `SEQUEL`. A list of lower levels grouped together.
    Edition,
    /// `70` — `COLLECTION`. The highest hierarchical level that tags
    /// can describe.
    Collection,
    /// A value registered after RFC 9559 under the "Matroska Tags
    /// Target Types" registry (§27.13). Carries the raw integer so the
    /// caller can compare across rounds without losing data.
    Other(u64),
}

impl TargetLevel {
    /// Map a raw `TargetTypeValue` (RFC 9559 §5.1.8.1.1.1) onto the
    /// hierarchy enum, preserving unrecognised values via
    /// [`TargetLevel::Other`].
    pub fn from_raw(v: u64) -> Self {
        match v {
            10 => TargetLevel::Shot,
            20 => TargetLevel::Subtrack,
            30 => TargetLevel::Track,
            40 => TargetLevel::Part,
            50 => TargetLevel::Album,
            60 => TargetLevel::Edition,
            70 => TargetLevel::Collection,
            other => TargetLevel::Other(other),
        }
    }

    /// Inverse of [`TargetLevel::from_raw`] — round-trip an enum value
    /// back to its `TargetTypeValue` integer. Useful for re-mux paths
    /// that want to write the level back out.
    pub fn to_raw(self) -> u64 {
        match self {
            TargetLevel::Shot => 10,
            TargetLevel::Subtrack => 20,
            TargetLevel::Track => 30,
            TargetLevel::Part => 40,
            TargetLevel::Album => 50,
            TargetLevel::Edition => 60,
            TargetLevel::Collection => 70,
            TargetLevel::Other(v) => v,
        }
    }

    /// Canonical (first) `TargetType` informational label for the level
    /// (RFC 9559 §5.1.8.1.1.1, Table 33). When several labels share a
    /// `TargetTypeValue` row (e.g. value `50` covers `ALBUM` / `OPERA`
    /// / `CONCERT` / `MOVIE` / `EPISODE`) the leftmost / most common
    /// label is returned. `None` for [`TargetLevel::Other`] — the spec
    /// gives no canonical label for a forward-compat registry entry.
    pub fn canonical_label(self) -> Option<&'static str> {
        match self {
            TargetLevel::Shot => Some("SHOT"),
            TargetLevel::Subtrack => Some("SUBTRACK"),
            TargetLevel::Track => Some("TRACK"),
            TargetLevel::Part => Some("PART"),
            TargetLevel::Album => Some("ALBUM"),
            TargetLevel::Edition => Some("EDITION"),
            TargetLevel::Collection => Some("COLLECTION"),
            TargetLevel::Other(_) => None,
        }
    }
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

/// One `BlockAdditionMapping` master (RFC 9559 §5.1.4.1.17) on a
/// `TrackEntry`: describes how the per-frame `BlockAdditional` data
/// (`BlockGroup > BlockAdditions > BlockMore > BlockAdditional`, §5.1.3.5.2.4)
/// for a given `BlockAddID` value (§5.1.3.5.2.3) is to be interpreted.
///
/// A track that uses `BlockAdditional` to carry side-channel payloads
/// (e.g. WebM alpha at `BlockAddID == 1`, HDR dynamic metadata, ITU-T T.35
/// frame-level metadata) declares one mapping per non-default
/// `BlockAddID` value it intends to emit. The mapping itself carries no
/// payload bytes — it links a `BlockAddID` to a registered
/// [`addid_type`](Self::addid_type) value plus optional per-track
/// [`extra_data`](Self::extra_data) the type interpreter consults.
///
/// Surfaced per stream via [`MkvDemuxer::block_addition_mappings`].
/// Multiple mappings per `TrackEntry` are permitted by the spec (no
/// `maxOccurs` on §5.1.4.1.17) and surface as a `&[BlockAdditionMapping]`
/// in on-disk order.
///
/// Container-level only — the bytes inside `BlockAdditional` are not
/// surfaced here; this just declares the *shape* of the side channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockAdditionMapping {
    /// `BlockAddIDValue` (RFC 9559 §5.1.4.1.17.1) — the `BlockAddID`
    /// (§5.1.3.5.2.3) value this mapping describes. Range is "`>=2`"
    /// per the spec because `BlockAddID == 1` is reserved for the
    /// codec-defined default and needs no mapping. `None` when the
    /// `BlockAdditionMapping` master had no `BlockAddIDValue` child —
    /// the spec gives the field no default and no minOccurs, so absence
    /// is preserved verbatim.
    pub value: Option<u64>,
    /// `BlockAddIDName` (RFC 9559 §5.1.4.1.17.2) — a human-readable
    /// label for the mapping. `None` when the element was absent (the
    /// spec gives the field no default).
    pub name: Option<String>,
    /// `BlockAddIDType` (RFC 9559 §5.1.4.1.17.3) — the IANA-registered
    /// type identifier the `BlockAdditional` payload follows. Spec
    /// default `0` (codec-defined) is materialised so a `BlockAdditionMapping`
    /// master with no `BlockAddIDType` child decodes as `0`; the spec's
    /// usage note then constrains both `BlockAddIDValue` and the
    /// matching `BlockAddID` to be `1` for that case.
    pub addid_type: u64,
    /// `BlockAddIDExtraData` (RFC 9559 §5.1.4.1.17.4) — opaque per-track
    /// binary state the type interpreter consults to decode
    /// `BlockAdditional` payloads. `None` when the element was absent
    /// (the spec gives the field no default).
    pub extra_data: Option<Vec<u8>>,
}

impl BlockAdditionMapping {
    /// True when this mapping points at the codec-defined default
    /// (`addid_type == 0`). Per RFC 9559 §5.1.4.1.17.3's usage note,
    /// such a mapping constrains its `BlockAddIDValue` (and the
    /// matching `BlockAddID`) to `1`.
    pub fn is_codec_defined(&self) -> bool {
        self.addid_type == 0
    }
}

/// A video track's interlacing settings — `FlagInterlaced` (RFC 9559
/// §5.1.4.1.28.1) paired with `FieldOrder` (§5.1.4.1.28.2).
///
/// `FieldOrder` is only meaningful when [`flag`](Self::flag) reports
/// [`FlagInterlaced::Interlaced`]; the spec is explicit ("If FlagInterlaced
/// is not set to 1, this element MUST be ignored", §5.1.4.1.28.2 usage
/// notes), so this struct returns [`Self::field_order`] as `None` for
/// progressive / undetermined tracks even if the file carried a stray
/// `FieldOrder` child. Surfaced per stream via
/// [`MkvDemuxer::video_interlacing`].
///
/// Spec defaults are materialised: a `Video` master with no
/// `FlagInterlaced` child decodes as [`FlagInterlaced::Undetermined`] (the
/// §5.1.4.1.28.1 default value `0`), and an interlaced track with no
/// explicit `FieldOrder` decodes as
/// `Some(FieldOrder::Undetermined)` (the §5.1.4.1.28.2 default `2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoInterlacing {
    flag: FlagInterlaced,
    field_order_raw: u64,
}

impl VideoInterlacing {
    /// `FlagInterlaced` (RFC 9559 §5.1.4.1.28.1, Table 3) for the track.
    pub fn flag(&self) -> FlagInterlaced {
        self.flag
    }

    /// `FieldOrder` (RFC 9559 §5.1.4.1.28.2, Table 4) — only returned for
    /// tracks marked [`FlagInterlaced::Interlaced`]. Per §5.1.4.1.28.2 the
    /// element MUST be ignored otherwise, so this returns `None` even if a
    /// non-default `FieldOrder` was present on a progressive track.
    pub fn field_order(&self) -> Option<FieldOrder> {
        match self.flag {
            FlagInterlaced::Interlaced => Some(FieldOrder::from_raw(self.field_order_raw)),
            _ => None,
        }
    }
}

/// `FlagInterlaced` (RFC 9559 §5.1.4.1.28.1, Table 3): whether the video
/// track's frames are interlaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FlagInterlaced {
    /// `0` — interlacing status is unknown. Spec default; "SHOULD be
    /// avoided" per Table 3.
    #[default]
    Undetermined,
    /// `1` — interlaced frames.
    Interlaced,
    /// `2` — progressive frames (no interlacing).
    Progressive,
    /// Any other value. The spec only registers `0`/`1`/`2`, so anything
    /// else is malformed — surfaced rather than dropped so callers can log
    /// it.
    Other(u64),
}

impl FlagInterlaced {
    /// Map a raw `FlagInterlaced` integer onto the enum, preserving
    /// unrecognised values via [`FlagInterlaced::Other`].
    pub fn from_raw(v: u64) -> Self {
        match v {
            ids::FLAG_INTERLACED_UNDETERMINED => FlagInterlaced::Undetermined,
            ids::FLAG_INTERLACED_INTERLACED => FlagInterlaced::Interlaced,
            ids::FLAG_INTERLACED_PROGRESSIVE => FlagInterlaced::Progressive,
            other => FlagInterlaced::Other(other),
        }
    }

    /// Inverse of [`FlagInterlaced::from_raw`]: convert the typed enum back
    /// to its on-disk `FlagInterlaced` value (RFC 9559 §5.1.4.1.28.1,
    /// Table 3). [`FlagInterlaced::Other`] round-trips its wrapped value
    /// verbatim. Used by the muxer's `Video > FlagInterlaced` write path.
    pub fn to_raw(self) -> u64 {
        match self {
            FlagInterlaced::Undetermined => ids::FLAG_INTERLACED_UNDETERMINED,
            FlagInterlaced::Interlaced => ids::FLAG_INTERLACED_INTERLACED,
            FlagInterlaced::Progressive => ids::FLAG_INTERLACED_PROGRESSIVE,
            FlagInterlaced::Other(v) => v,
        }
    }
}

/// `FieldOrder` (RFC 9559 §5.1.4.1.28.2, Table 4): the field ordering of an
/// interlaced video track. Only meaningful when paired with
/// [`FlagInterlaced::Interlaced`] — the spec is explicit that the element
/// MUST be ignored otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldOrder {
    /// `0` — progressive. Table 4 marks this value as "SHOULD be avoided;
    /// setting FlagInterlaced to 2 is sufficient", but it's still a legal
    /// in-file value so we surface it.
    Progressive,
    /// `1` — top field displayed first, top field stored first.
    Tff,
    /// `2` — field order unknown. Spec default for `FieldOrder` per
    /// §5.1.4.1.28.2 (default `2`).
    Undetermined,
    /// `6` — bottom field displayed first, bottom field stored first.
    Bff,
    /// `9` — top field displayed first, interleaved with the top line of
    /// the top field stored first.
    TffInterleaved,
    /// `14` — bottom field displayed first, interleaved with the top line
    /// of the top field stored first.
    BffInterleaved,
    /// Any other value. Table 4 only registers `0`/`1`/`2`/`6`/`9`/`14`,
    /// so anything else is malformed — surfaced rather than dropped.
    Other(u64),
}

impl FieldOrder {
    /// Map a raw `FieldOrder` integer onto the enum, preserving
    /// unrecognised values via [`FieldOrder::Other`].
    pub fn from_raw(v: u64) -> Self {
        match v {
            ids::FIELD_ORDER_PROGRESSIVE => FieldOrder::Progressive,
            ids::FIELD_ORDER_TFF => FieldOrder::Tff,
            ids::FIELD_ORDER_UNDETERMINED => FieldOrder::Undetermined,
            ids::FIELD_ORDER_BFF => FieldOrder::Bff,
            ids::FIELD_ORDER_TFF_INTERLEAVED => FieldOrder::TffInterleaved,
            ids::FIELD_ORDER_BFF_INTERLEAVED => FieldOrder::BffInterleaved,
            other => FieldOrder::Other(other),
        }
    }

    /// Inverse of [`FieldOrder::from_raw`]: convert the typed enum back to
    /// its on-disk `FieldOrder` value (RFC 9559 §5.1.4.1.28.2, Table 4).
    /// [`FieldOrder::Other`] round-trips its wrapped value verbatim. Used
    /// by the muxer's `Video > FieldOrder` write path.
    pub fn to_raw(self) -> u64 {
        match self {
            FieldOrder::Progressive => ids::FIELD_ORDER_PROGRESSIVE,
            FieldOrder::Tff => ids::FIELD_ORDER_TFF,
            FieldOrder::Undetermined => ids::FIELD_ORDER_UNDETERMINED,
            FieldOrder::Bff => ids::FIELD_ORDER_BFF,
            FieldOrder::TffInterleaved => ids::FIELD_ORDER_TFF_INTERLEAVED,
            FieldOrder::BffInterleaved => ids::FIELD_ORDER_BFF_INTERLEAVED,
            FieldOrder::Other(v) => v,
        }
    }
}

/// `StereoMode` (RFC 9559 §5.1.4.1.28.3, Table 5): the single-track
/// stereo-3D packing used by the video track's frames. The multi-track
/// alternative is `TrackOperation > TrackCombinePlanes` (§5.1.4.1.30.1,
/// surfaced via [`MkvDemuxer::track_operation`]).
///
/// Surfaced per stream via [`MkvDemuxer::video_stereo_mode`].
///
/// Spec default `0` (mono) is materialised on the typed surface — a `Video`
/// master with no explicit `StereoMode` decodes as [`StereoMode::Mono`].
/// §27.7 leaves the StereoMode registry open for future additions, so any
/// value outside the §5.1.4.1.28.3 Table 5 set passes through the
/// [`StereoMode::Other`] variant rather than being dropped.
///
/// The naming convention "*RightFirst*" / "*LeftFirst*" matches the spec's
/// "right eye is first" / "left eye is first" parenthetical phrasings.
/// Per §18.10 odd values of StereoMode mean the left eye comes first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StereoMode {
    /// `0` — mono (no stereo packing). Spec default.
    #[default]
    Mono,
    /// `1` — side by side, left eye first.
    SideBySideLeftFirst,
    /// `2` — top-bottom, right eye first.
    TopBottomRightFirst,
    /// `3` — top-bottom, left eye first.
    TopBottomLeftFirst,
    /// `4` — checkerboard, right eye first.
    CheckboardRightFirst,
    /// `5` — checkerboard, left eye first.
    CheckboardLeftFirst,
    /// `6` — row interleaved, right eye first.
    RowInterleavedRightFirst,
    /// `7` — row interleaved, left eye first.
    RowInterleavedLeftFirst,
    /// `8` — column interleaved, right eye first.
    ColumnInterleavedRightFirst,
    /// `9` — column interleaved, left eye first.
    ColumnInterleavedLeftFirst,
    /// `10` — anaglyph (cyan / red).
    AnaglyphCyanRed,
    /// `11` — side by side, right eye first.
    SideBySideRightFirst,
    /// `12` — anaglyph (green / magenta).
    AnaglyphGreenMagenta,
    /// `13` — both eyes laced in one Block, left eye first.
    BothEyesLacedLeftFirst,
    /// `14` — both eyes laced in one Block, right eye first.
    BothEyesLacedRightFirst,
    /// Any value not registered in §5.1.4.1.28.3 Table 5. §27.7 leaves the
    /// registry open for future additions; surfaced rather than dropped so
    /// callers can log it.
    Other(u64),
}

impl StereoMode {
    /// Map a raw `StereoMode` integer onto the enum, preserving values
    /// outside §5.1.4.1.28.3 Table 5 via [`StereoMode::Other`].
    pub fn from_raw(v: u64) -> Self {
        match v {
            ids::STEREO_MODE_MONO => StereoMode::Mono,
            ids::STEREO_MODE_SIDE_BY_SIDE_LEFT_FIRST => StereoMode::SideBySideLeftFirst,
            ids::STEREO_MODE_TOP_BOTTOM_RIGHT_FIRST => StereoMode::TopBottomRightFirst,
            ids::STEREO_MODE_TOP_BOTTOM_LEFT_FIRST => StereoMode::TopBottomLeftFirst,
            ids::STEREO_MODE_CHECKBOARD_RIGHT_FIRST => StereoMode::CheckboardRightFirst,
            ids::STEREO_MODE_CHECKBOARD_LEFT_FIRST => StereoMode::CheckboardLeftFirst,
            ids::STEREO_MODE_ROW_INTERLEAVED_RIGHT_FIRST => StereoMode::RowInterleavedRightFirst,
            ids::STEREO_MODE_ROW_INTERLEAVED_LEFT_FIRST => StereoMode::RowInterleavedLeftFirst,
            ids::STEREO_MODE_COLUMN_INTERLEAVED_RIGHT_FIRST => {
                StereoMode::ColumnInterleavedRightFirst
            }
            ids::STEREO_MODE_COLUMN_INTERLEAVED_LEFT_FIRST => {
                StereoMode::ColumnInterleavedLeftFirst
            }
            ids::STEREO_MODE_ANAGLYPH_CYAN_RED => StereoMode::AnaglyphCyanRed,
            ids::STEREO_MODE_SIDE_BY_SIDE_RIGHT_FIRST => StereoMode::SideBySideRightFirst,
            ids::STEREO_MODE_ANAGLYPH_GREEN_MAGENTA => StereoMode::AnaglyphGreenMagenta,
            ids::STEREO_MODE_BOTH_EYES_LACED_LEFT_FIRST => StereoMode::BothEyesLacedLeftFirst,
            ids::STEREO_MODE_BOTH_EYES_LACED_RIGHT_FIRST => StereoMode::BothEyesLacedRightFirst,
            other => StereoMode::Other(other),
        }
    }

    /// `true` when this StereoMode is anything other than [`StereoMode::Mono`].
    /// A convenience for callers that only need a yes/no "is this track 3D?"
    /// answer without matching on the specific packing.
    pub fn is_stereo(&self) -> bool {
        !matches!(self, StereoMode::Mono)
    }

    /// Inverse of [`StereoMode::from_raw`]: convert the typed enum back to its
    /// on-disk `StereoMode` value (RFC 9559 §5.1.4.1.28.3, Table 5).
    /// [`StereoMode::Other`] round-trips its wrapped value verbatim. Used by
    /// the muxer's `Video > StereoMode` write path.
    pub fn to_raw(self) -> u64 {
        match self {
            StereoMode::Mono => ids::STEREO_MODE_MONO,
            StereoMode::SideBySideLeftFirst => ids::STEREO_MODE_SIDE_BY_SIDE_LEFT_FIRST,
            StereoMode::TopBottomRightFirst => ids::STEREO_MODE_TOP_BOTTOM_RIGHT_FIRST,
            StereoMode::TopBottomLeftFirst => ids::STEREO_MODE_TOP_BOTTOM_LEFT_FIRST,
            StereoMode::CheckboardRightFirst => ids::STEREO_MODE_CHECKBOARD_RIGHT_FIRST,
            StereoMode::CheckboardLeftFirst => ids::STEREO_MODE_CHECKBOARD_LEFT_FIRST,
            StereoMode::RowInterleavedRightFirst => ids::STEREO_MODE_ROW_INTERLEAVED_RIGHT_FIRST,
            StereoMode::RowInterleavedLeftFirst => ids::STEREO_MODE_ROW_INTERLEAVED_LEFT_FIRST,
            StereoMode::ColumnInterleavedRightFirst => {
                ids::STEREO_MODE_COLUMN_INTERLEAVED_RIGHT_FIRST
            }
            StereoMode::ColumnInterleavedLeftFirst => {
                ids::STEREO_MODE_COLUMN_INTERLEAVED_LEFT_FIRST
            }
            StereoMode::AnaglyphCyanRed => ids::STEREO_MODE_ANAGLYPH_CYAN_RED,
            StereoMode::SideBySideRightFirst => ids::STEREO_MODE_SIDE_BY_SIDE_RIGHT_FIRST,
            StereoMode::AnaglyphGreenMagenta => ids::STEREO_MODE_ANAGLYPH_GREEN_MAGENTA,
            StereoMode::BothEyesLacedLeftFirst => ids::STEREO_MODE_BOTH_EYES_LACED_LEFT_FIRST,
            StereoMode::BothEyesLacedRightFirst => ids::STEREO_MODE_BOTH_EYES_LACED_RIGHT_FIRST,
            StereoMode::Other(v) => v,
        }
    }
}

/// `ProjectionType` (RFC 9559 §5.1.4.1.28.42, Table 18): the projection used
/// to render the video track's frames. `Rectangular` is the default and
/// covers ordinary flat video; the other three values describe spherical /
/// VR-video projections that pair with the [`Projection::private`] payload
/// (which mirrors the corresponding ISOBMFF box, §5.1.4.1.28.43).
///
/// §27.15 leaves the "Matroska Projection Types" registry open for future
/// additions, so any value outside Table 18 passes through the
/// [`ProjectionType::Other`] variant rather than being dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ProjectionType {
    /// `0` — rectangular (flat) projection. Spec default; per §5.1.4.1.28.43
    /// `ProjectionPrivate` MUST NOT be present when this type is in effect.
    #[default]
    Rectangular,
    /// `1` — equirectangular spherical projection. `ProjectionPrivate` MUST
    /// be present and mirrors the ISOBMFF "equi" box body.
    Equirectangular,
    /// `2` — cubemap projection. `ProjectionPrivate` MUST be present and
    /// mirrors the ISOBMFF "cbmp" box body.
    Cubemap,
    /// `3` — mesh projection. `ProjectionPrivate` MUST be present and
    /// mirrors the ISOBMFF "mshp" box body.
    Mesh,
    /// Any value not registered in §5.1.4.1.28.42 Table 18. §27.15 leaves
    /// the registry open for future additions; surfaced rather than dropped
    /// so callers can log it.
    Other(u64),
}

impl ProjectionType {
    /// Map a raw `ProjectionType` integer onto the enum, preserving values
    /// outside §5.1.4.1.28.42 Table 18 via [`ProjectionType::Other`].
    pub fn from_raw(v: u64) -> Self {
        match v {
            ids::PROJECTION_TYPE_RECTANGULAR => ProjectionType::Rectangular,
            ids::PROJECTION_TYPE_EQUIRECTANGULAR => ProjectionType::Equirectangular,
            ids::PROJECTION_TYPE_CUBEMAP => ProjectionType::Cubemap,
            ids::PROJECTION_TYPE_MESH => ProjectionType::Mesh,
            other => ProjectionType::Other(other),
        }
    }

    /// `true` for any projection that isn't ordinary flat rectangular video.
    /// Convenience for callers that only need a yes/no "is this a spherical
    /// / VR track?" answer without matching on the specific projection.
    pub fn is_spherical(&self) -> bool {
        !matches!(self, ProjectionType::Rectangular)
    }
}

/// A video track's `Projection` master (RFC 9559 §5.1.4.1.28.41 plus the
/// `ProjectionType` / `ProjectionPrivate` / `ProjectionPose{Yaw,Pitch,Roll}`
/// sub-elements §5.1.4.1.28.42..46).
///
/// Surfaced per stream via [`MkvDemuxer::video_projection`]. The pose triple
/// is in degrees; per §5.1.4.1.28.44..46 the yaw/roll are in [-180, 180] and
/// the pitch is in [-90, 90], and all three default to `0.0`. The
/// `private` payload is the verbatim ISOBMFF box body that pairs with the
/// projection type (`equi` / `cbmp` / `mshp`); the demuxer never parses or
/// validates it — that's a renderer concern. `private` is `None` when the
/// `ProjectionPrivate` element is absent (which is the only legal state when
/// `projection_type == Rectangular`, per the §5.1.4.1.28.43 "MUST NOT be
/// present" clause).
///
/// Spec defaults are materialised on the typed surface so an empty
/// `Projection` master (one with only the mandatory `ProjectionType` =
/// `Rectangular` plus pose-zero defaults) decodes as a fully-typed identity
/// projection. The §5.1.4.1.28.46 worked example
/// `<Projection><ProjectionPoseRoll>90</ProjectionPoseRoll></Projection>`
/// — used to signal a 90° counter-clockwise rotation — round-trips through
/// the typed surface with `projection_type == Rectangular`, `pose_roll ==
/// 90.0`, and the other pose components at their zero defaults.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Projection {
    projection_type: ProjectionType,
    private: Option<Vec<u8>>,
    pose_yaw: f64,
    pose_pitch: f64,
    pose_roll: f64,
}

impl Projection {
    /// `ProjectionType` (RFC 9559 §5.1.4.1.28.42) — the projection used for
    /// the track. Spec default `0` ([`ProjectionType::Rectangular`]) is
    /// materialised when the file's `Projection` master carried no explicit
    /// `ProjectionType` child.
    pub fn projection_type(&self) -> ProjectionType {
        self.projection_type
    }

    /// `ProjectionPrivate` (RFC 9559 §5.1.4.1.28.43): the verbatim ISOBMFF
    /// box body (without size/FourCC framing but with the FullBox version
    /// and flag fields) that pairs with [`Projection::projection_type`].
    /// `None` when the element is absent — the only legal state when the
    /// projection type is `Rectangular` (the spec MUSTs it out for that
    /// case). Returned by reference so callers don't copy multi-kilobyte
    /// mesh-box payloads they only want to read.
    pub fn private(&self) -> Option<&[u8]> {
        self.private.as_deref()
    }

    /// `ProjectionPoseYaw` (RFC 9559 §5.1.4.1.28.44): clockwise rotation
    /// around the up vector, in degrees. Spec range `[-180.0, 180.0]`,
    /// default `0.0`. Applied before pitch and roll.
    pub fn pose_yaw(&self) -> f64 {
        self.pose_yaw
    }

    /// `ProjectionPosePitch` (RFC 9559 §5.1.4.1.28.45): counter-clockwise
    /// rotation around the right vector, in degrees. Spec range
    /// `[-90.0, 90.0]`, default `0.0`. Applied after yaw and before roll.
    pub fn pose_pitch(&self) -> f64 {
        self.pose_pitch
    }

    /// `ProjectionPoseRoll` (RFC 9559 §5.1.4.1.28.46): counter-clockwise
    /// rotation around the forward vector, in degrees. Spec range
    /// `[-180.0, 180.0]`, default `0.0`. Applied after both yaw and pitch.
    /// Used by the §5.1.4.1.28.46 worked example (`90` ⇒ "present with a
    /// 90-degree counter-clockwise rotation").
    pub fn pose_roll(&self) -> f64 {
        self.pose_roll
    }

    /// `true` when any pose component is non-zero (i.e. the projection
    /// includes a rotation). Convenience for callers that only need a
    /// yes/no "does this track want to be rotated?" answer.
    pub fn is_rotated(&self) -> bool {
        self.pose_yaw != 0.0 || self.pose_pitch != 0.0 || self.pose_roll != 0.0
    }
}

/// `AlphaMode` (RFC 9559 §5.1.4.1.28.4, Table 6): whether the track carries
/// out-of-band alpha-channel data inside `BlockAdditional` elements with
/// `BlockAddID=1`. Surfaced per stream via [`MkvDemuxer::video_alpha_mode`].
///
/// The spec only enumerates two values (`0` none, `1` present); values
/// outside the registered set are forwarded via [`AlphaMode::Other`] —
/// §27.8 leaves the "Matroska Alpha Modes" registry open for future
/// additions. The §5.1.4.1.28.4 default `0` is materialised on the typed
/// surface so an empty `Video` master decodes as `Some(AlphaMode::None)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AlphaMode {
    /// `0` — no alpha-channel data. Spec default; either the
    /// `BlockAdditional` element with `BlockAddID=1` is absent, or it
    /// SHOULD NOT be treated as alpha data (§5.1.4.1.28.4 Table 6).
    #[default]
    None,
    /// `1` — the `BlockAdditional` element with `BlockAddID=1` carries
    /// alpha-channel data, decoded as the codec mapping for `CodecID`
    /// requires. The WebM alpha-channel extension uses this with
    /// `BlockAddID=1` carrying a parallel VP8/VP9 frame's alpha plane.
    Present,
    /// Any other value — preserved for forward-compatibility with the
    /// "Matroska Alpha Modes" registry (§27.8). The spec also notes that
    /// values other than `0` and `1` "SHOULD NOT be used, as the behavior
    /// of known implementations is different".
    Other(u64),
}

impl AlphaMode {
    /// Map a raw `AlphaMode` integer onto the enum, preserving unrecognised
    /// values via [`AlphaMode::Other`].
    pub fn from_raw(v: u64) -> Self {
        match v {
            ids::ALPHA_MODE_NONE => AlphaMode::None,
            ids::ALPHA_MODE_PRESENT => AlphaMode::Present,
            other => AlphaMode::Other(other),
        }
    }

    /// `true` when the track is signalling alpha-channel data (i.e. the
    /// value is exactly [`AlphaMode::Present`]). Convenience for callers
    /// that want the headline yes/no answer; values outside the registered
    /// set are treated as "no" because the spec leaves their semantics
    /// implementation-defined.
    pub fn has_alpha(&self) -> bool {
        matches!(self, AlphaMode::Present)
    }

    /// Inverse of [`AlphaMode::from_raw`]: convert the typed enum back to its
    /// on-disk `AlphaMode` value (RFC 9559 §5.1.4.1.28.4, Table 6).
    /// [`AlphaMode::Other`] round-trips its wrapped value verbatim. Used by
    /// the muxer's `Video > AlphaMode` write path.
    pub fn to_raw(self) -> u64 {
        match self {
            AlphaMode::None => ids::ALPHA_MODE_NONE,
            AlphaMode::Present => ids::ALPHA_MODE_PRESENT,
            AlphaMode::Other(v) => v,
        }
    }
}

/// `UncompressedFourCC` (RFC 9559 §5.1.4.1.28.15): the 4-byte FourCC that
/// identifies the uncompressed pixel layout used by the track. The spec
/// makes the element mandatory only when `CodecID = "V_UNCOMPRESSED"`
/// (§5.1.4.1.28.15 Table 11) and explicitly notes that there is "neither a
/// definitive list of FourCC values nor an official registry" — so we
/// surface the raw bytes plus a UTF-8 lossy 4-character preview and let the
/// caller compare against whichever FourCC set they care about.
///
/// The spec also pins the on-disk byte length to exactly 4 (the EBML
/// schema's `length:` field). A non-4-byte payload is preserved verbatim
/// so a caller debugging a malformed file can still see what the writer
/// emitted; the [`Self::fourcc`] and [`Self::as_str`] accessors return
/// `None` whenever [`Self::as_bytes`] isn't exactly 4 bytes long.
///
/// Surfaced per stream via [`MkvDemuxer::video_uncompressed_fourcc`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UncompressedFourCC {
    bytes: Vec<u8>,
}

impl UncompressedFourCC {
    /// The raw on-disk bytes verbatim. For a well-formed file the length is
    /// exactly 4; for malformed input the original payload length is
    /// preserved so callers can log the deviation.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The four-byte FourCC as a `[u8; 4]`, or `None` when the on-disk
    /// payload wasn't exactly 4 bytes long. Convenience for callers that
    /// want to match against canonical `*b"YUY2"`-style byte patterns.
    pub fn fourcc(&self) -> Option<[u8; 4]> {
        if self.bytes.len() == 4 {
            Some([self.bytes[0], self.bytes[1], self.bytes[2], self.bytes[3]])
        } else {
            None
        }
    }

    /// The four-byte FourCC as a UTF-8 lossy string preview. Returns `None`
    /// when the on-disk payload wasn't exactly 4 bytes long. The lossy
    /// conversion replaces any byte that isn't valid ASCII / UTF-8 with the
    /// Unicode replacement character — a FourCC of `b"YUY2"` becomes
    /// `"YUY2"`; an exotic 4-byte payload still returns a non-empty string.
    pub fn as_str(&self) -> Option<String> {
        if self.bytes.len() == 4 {
            Some(String::from_utf8_lossy(&self.bytes).into_owned())
        } else {
            None
        }
    }
}

/// A video track's display-geometry quartet — the `PixelCrop*` window plus
/// `DisplayWidth` / `DisplayHeight` / `DisplayUnit` (RFC 9559
/// §5.1.4.1.28.8..§5.1.4.1.28.14).
///
/// `PixelCrop{Top,Bottom,Left,Right}` carve a visible rectangle out of the
/// encoded `PixelWidth` × `PixelHeight` buffer; per §5.1.4.1.28.8..11 they
/// default to `0` and represent "pixel rows / columns the player SHOULD hide
/// from the user". `DisplayWidth` / `DisplayHeight` describe the rendered
/// frame size, in units selected by `DisplayUnit` (Table 10:
/// `0` pixels / `1` cm / `2` in / `3` display-aspect-ratio / `4` unknown).
///
/// Derived defaults for `DisplayWidth` / `DisplayHeight` are materialised on
/// the typed surface as `Option<u64>`: per §5.1.4.1.28.12 / .13 a missing
/// element defaults to `PixelWidth - PixelCropLeft - PixelCropRight` (and
/// the analogous height) *only when DisplayUnit is `0` (pixels)*. For any
/// other DisplayUnit there is no default; the accessor returns `None`.
/// Surfaced per stream via [`MkvDemuxer::video_geometry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoGeometry {
    pixel_crop_top: u64,
    pixel_crop_bottom: u64,
    pixel_crop_left: u64,
    pixel_crop_right: u64,
    display_width: Option<u64>,
    display_height: Option<u64>,
    display_unit: DisplayUnit,
}

impl VideoGeometry {
    /// `PixelCropTop` (RFC 9559 §5.1.4.1.28.9): number of pixel rows to hide
    /// at the top of the encoded image. Default `0`.
    pub fn pixel_crop_top(&self) -> u64 {
        self.pixel_crop_top
    }

    /// `PixelCropBottom` (RFC 9559 §5.1.4.1.28.8): number of pixel rows to
    /// hide at the bottom of the encoded image. Default `0`.
    pub fn pixel_crop_bottom(&self) -> u64 {
        self.pixel_crop_bottom
    }

    /// `PixelCropLeft` (RFC 9559 §5.1.4.1.28.10): number of pixel columns to
    /// hide on the left side of the encoded image. Default `0`.
    pub fn pixel_crop_left(&self) -> u64 {
        self.pixel_crop_left
    }

    /// `PixelCropRight` (RFC 9559 §5.1.4.1.28.11): number of pixel columns to
    /// hide on the right side of the encoded image. Default `0`.
    pub fn pixel_crop_right(&self) -> u64 {
        self.pixel_crop_right
    }

    /// `DisplayWidth` (RFC 9559 §5.1.4.1.28.12): width of the frame to
    /// display, in [`DisplayUnit`] units, applied to the already-cropped
    /// image.
    ///
    /// Returns the explicit `DisplayWidth` element when present (the spec
    /// ranges it as "not 0"). When the element is absent, the spec default
    /// applies only when `DisplayUnit == 0` (pixels): the value derived from
    /// `PixelWidth - PixelCropLeft - PixelCropRight`. For any other
    /// [`DisplayUnit`] the spec note "there is no default value" applies
    /// and this returns `None`. Also returns `None` when the derivation
    /// would underflow (malformed file).
    pub fn display_width(&self) -> Option<u64> {
        self.display_width
    }

    /// `DisplayHeight` (RFC 9559 §5.1.4.1.28.13): height of the frame to
    /// display, in [`DisplayUnit`] units, applied to the already-cropped
    /// image. See [`Self::display_width`] for the default-derivation rules.
    pub fn display_height(&self) -> Option<u64> {
        self.display_height
    }

    /// `DisplayUnit` (RFC 9559 §5.1.4.1.28.14, Table 10): how
    /// [`Self::display_width`] / [`Self::display_height`] are to be
    /// interpreted. Default `0` (pixels).
    pub fn display_unit(&self) -> DisplayUnit {
        self.display_unit
    }
}

/// `DisplayUnit` (RFC 9559 §5.1.4.1.28.14, Table 10): the interpretation of
/// the `DisplayWidth` / `DisplayHeight` pair. The spec also reserves the
/// "Matroska Display Units" registry (§27.9) for additional values; any value
/// outside the registered set surfaces via [`DisplayUnit::Other`] rather than
/// being dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DisplayUnit {
    /// `0` — display dimensions are in pixels. Spec default.
    #[default]
    Pixels,
    /// `1` — display dimensions are in centimeters.
    Centimeters,
    /// `2` — display dimensions are in inches.
    Inches,
    /// `3` — display dimensions encode a display aspect ratio (DAR) rather
    /// than a physical size.
    DisplayAspectRatio,
    /// `4` — display dimensions' unit is unknown.
    Unknown,
    /// Any other value — preserved for forward-compatibility with the
    /// "Matroska Display Units" registry (§27.9).
    Other(u64),
}

impl DisplayUnit {
    /// Map a raw `DisplayUnit` integer onto the enum, preserving
    /// unrecognised values via [`DisplayUnit::Other`].
    pub fn from_raw(v: u64) -> Self {
        match v {
            ids::DISPLAY_UNIT_PIXELS => DisplayUnit::Pixels,
            ids::DISPLAY_UNIT_CENTIMETERS => DisplayUnit::Centimeters,
            ids::DISPLAY_UNIT_INCHES => DisplayUnit::Inches,
            ids::DISPLAY_UNIT_DAR => DisplayUnit::DisplayAspectRatio,
            ids::DISPLAY_UNIT_UNKNOWN => DisplayUnit::Unknown,
            other => DisplayUnit::Other(other),
        }
    }

    /// Inverse of [`DisplayUnit::from_raw`]: return the raw integer this
    /// variant maps to. Used by the muxer to write `DisplayUnit` (RFC 9559
    /// §5.1.4.1.28.14, Table 10) verbatim, including the [`DisplayUnit::Other`]
    /// forward-compat variant for values registered after RFC 9559 in the
    /// "Matroska Display Units" registry (§27.9).
    pub fn to_raw(self) -> u64 {
        match self {
            DisplayUnit::Pixels => ids::DISPLAY_UNIT_PIXELS,
            DisplayUnit::Centimeters => ids::DISPLAY_UNIT_CENTIMETERS,
            DisplayUnit::Inches => ids::DISPLAY_UNIT_INCHES,
            DisplayUnit::DisplayAspectRatio => ids::DISPLAY_UNIT_DAR,
            DisplayUnit::Unknown => ids::DISPLAY_UNIT_UNKNOWN,
            DisplayUnit::Other(v) => v,
        }
    }
}

/// A video track's `Colour` master (RFC 9559 §5.1.4.1.28.16): the
/// chroma / range / transfer / primaries description plus the SMPTE
/// ST 2086 / CTA-861.3 HDR mastering metadata. Surfaced per stream via
/// [`MkvDemuxer::video_colour`].
///
/// Spec defaults are materialised on the typed surface:
/// `MatrixCoefficients` (§5.1.4.1.28.17), `TransferCharacteristics`
/// (§5.1.4.1.28.26) and `Primaries` (§5.1.4.1.28.27) each default to `2`
/// (*unspecified*); `Range` (§5.1.4.1.28.25), `ChromaSitingHorz`
/// (§5.1.4.1.28.23) and `ChromaSitingVert` (§5.1.4.1.28.24) each default to
/// `0` (*unspecified*); `BitsPerChannel` (§5.1.4.1.28.18) defaults to `0`
/// (*unspecified*). Elements without a spec default (`Chroma{Subsampling,
/// Cb}Subsampling{Horz,Vert}`, `MaxCLL`, `MaxFALL`) surface as `None` when
/// absent. The `MasteringMetadata` master (§5.1.4.1.28.30) is `Some` only
/// when the file actually carried it.
///
/// Forward-compat: values outside each table's registered set surface via
/// the enum's `Other(u64)` variant rather than being dropped, since RFC
/// 9559 §27 reserves additional values can be added in future revisions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoColour {
    matrix_coefficients: MatrixCoefficients,
    bits_per_channel: u64,
    chroma_subsampling_horz: Option<u64>,
    chroma_subsampling_vert: Option<u64>,
    cb_subsampling_horz: Option<u64>,
    cb_subsampling_vert: Option<u64>,
    chroma_siting_horz: ChromaSitingHorz,
    chroma_siting_vert: ChromaSitingVert,
    range: ColourRange,
    transfer_characteristics: TransferCharacteristics,
    primaries: Primaries,
    max_cll: Option<u64>,
    max_fall: Option<u64>,
    mastering_metadata: Option<MasteringMetadata>,
}

impl VideoColour {
    /// `MatrixCoefficients` (RFC 9559 §5.1.4.1.28.17, Table 12) — the matrix
    /// used to derive luma/chroma from RGB primaries. Default `2`
    /// (*unspecified*).
    pub fn matrix_coefficients(&self) -> MatrixCoefficients {
        self.matrix_coefficients
    }

    /// `BitsPerChannel` (RFC 9559 §5.1.4.1.28.18). `0` = unspecified
    /// (spec default).
    pub fn bits_per_channel(&self) -> u64 {
        self.bits_per_channel
    }

    /// `ChromaSubsamplingHorz` (RFC 9559 §5.1.4.1.28.19): horizontal
    /// chroma subsampling factor. `None` when the file did not carry the
    /// element (no spec default).
    pub fn chroma_subsampling_horz(&self) -> Option<u64> {
        self.chroma_subsampling_horz
    }

    /// `ChromaSubsamplingVert` (RFC 9559 §5.1.4.1.28.20): vertical chroma
    /// subsampling factor. `None` when the file did not carry the
    /// element (no spec default).
    pub fn chroma_subsampling_vert(&self) -> Option<u64> {
        self.chroma_subsampling_vert
    }

    /// `CbSubsamplingHorz` (RFC 9559 §5.1.4.1.28.21): additional Cb-channel
    /// horizontal subsampling, additive to `ChromaSubsamplingHorz`. `None`
    /// when absent.
    pub fn cb_subsampling_horz(&self) -> Option<u64> {
        self.cb_subsampling_horz
    }

    /// `CbSubsamplingVert` (RFC 9559 §5.1.4.1.28.22): additional Cb-channel
    /// vertical subsampling, additive to `ChromaSubsamplingVert`. `None`
    /// when absent.
    pub fn cb_subsampling_vert(&self) -> Option<u64> {
        self.cb_subsampling_vert
    }

    /// `ChromaSitingHorz` (RFC 9559 §5.1.4.1.28.23, Table 13). Default `0`
    /// (*unspecified*).
    pub fn chroma_siting_horz(&self) -> ChromaSitingHorz {
        self.chroma_siting_horz
    }

    /// `ChromaSitingVert` (RFC 9559 §5.1.4.1.28.24, Table 14). Default `0`
    /// (*unspecified*).
    pub fn chroma_siting_vert(&self) -> ChromaSitingVert {
        self.chroma_siting_vert
    }

    /// `Range` (RFC 9559 §5.1.4.1.28.25, Table 15): clipping of the colour
    /// ranges. Default `0` (*unspecified*).
    pub fn range(&self) -> ColourRange {
        self.range
    }

    /// `TransferCharacteristics` (RFC 9559 §5.1.4.1.28.26, Table 16). Default
    /// `2` (*unspecified*).
    pub fn transfer_characteristics(&self) -> TransferCharacteristics {
        self.transfer_characteristics
    }

    /// `Primaries` (RFC 9559 §5.1.4.1.28.27, Table 17): the colour primaries
    /// the video uses. Default `2` (*unspecified*).
    pub fn primaries(&self) -> Primaries {
        self.primaries
    }

    /// `MaxCLL` (RFC 9559 §5.1.4.1.28.28): Maximum Content Light Level in
    /// cd/m². `None` when absent — no spec default.
    pub fn max_cll(&self) -> Option<u64> {
        self.max_cll
    }

    /// `MaxFALL` (RFC 9559 §5.1.4.1.28.29): Maximum Frame-Average Light Level
    /// in cd/m². `None` when absent — no spec default.
    pub fn max_fall(&self) -> Option<u64> {
        self.max_fall
    }

    /// `MasteringMetadata` (RFC 9559 §5.1.4.1.28.30): the SMPTE 2086 mastering
    /// display description. `None` when the file omitted the master entirely.
    pub fn mastering_metadata(&self) -> Option<&MasteringMetadata> {
        self.mastering_metadata.as_ref()
    }
}

/// `MatrixCoefficients` (RFC 9559 §5.1.4.1.28.17, Table 12) — the matrix
/// the video uses to derive luma / chroma from RGB primaries. Values are
/// adopted from Table 4 of ITU-T H.273; the spec only lists `0..=14`
/// (with `3` reserved) — any other value surfaces via
/// [`MatrixCoefficients::Other`] for forward compatibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixCoefficients {
    Identity,
    BT709,
    /// `2` — spec default.
    Unspecified,
    Reserved,
    UsFcc73682,
    BT470Bg,
    Smpte170M,
    Smpte240M,
    YCoCg,
    BT2020NonConstantLuminance,
    BT2020ConstantLuminance,
    SmpteSt2085,
    ChromaDerivedNonConstantLuminance,
    ChromaDerivedConstantLuminance,
    BT2100,
    Other(u64),
}

impl MatrixCoefficients {
    /// Map a raw `MatrixCoefficients` integer onto the enum, preserving
    /// unrecognised values via [`MatrixCoefficients::Other`].
    pub fn from_raw(v: u64) -> Self {
        match v {
            0 => Self::Identity,
            1 => Self::BT709,
            2 => Self::Unspecified,
            3 => Self::Reserved,
            4 => Self::UsFcc73682,
            5 => Self::BT470Bg,
            6 => Self::Smpte170M,
            7 => Self::Smpte240M,
            8 => Self::YCoCg,
            9 => Self::BT2020NonConstantLuminance,
            10 => Self::BT2020ConstantLuminance,
            11 => Self::SmpteSt2085,
            12 => Self::ChromaDerivedNonConstantLuminance,
            13 => Self::ChromaDerivedConstantLuminance,
            14 => Self::BT2100,
            other => Self::Other(other),
        }
    }

    /// Inverse of [`MatrixCoefficients::from_raw`]: return the raw integer
    /// this variant maps to. Used by the muxer to write `MatrixCoefficients`
    /// (RFC 9559 §5.1.4.1.28.17, Table 12) verbatim, including the
    /// [`MatrixCoefficients::Other`] forward-compat variant for values
    /// registered after RFC 9559 in §27.13.
    pub fn to_raw(self) -> u64 {
        match self {
            Self::Identity => 0,
            Self::BT709 => 1,
            Self::Unspecified => 2,
            Self::Reserved => 3,
            Self::UsFcc73682 => 4,
            Self::BT470Bg => 5,
            Self::Smpte170M => 6,
            Self::Smpte240M => 7,
            Self::YCoCg => 8,
            Self::BT2020NonConstantLuminance => 9,
            Self::BT2020ConstantLuminance => 10,
            Self::SmpteSt2085 => 11,
            Self::ChromaDerivedNonConstantLuminance => 12,
            Self::ChromaDerivedConstantLuminance => 13,
            Self::BT2100 => 14,
            Self::Other(v) => v,
        }
    }
}

/// `ChromaSitingHorz` (RFC 9559 §5.1.4.1.28.23, Table 13).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ChromaSitingHorz {
    /// `0` — spec default.
    #[default]
    Unspecified,
    /// `1` — left collocated.
    LeftCollocated,
    /// `2` — half.
    Half,
    /// Any other value — preserved for the "Matroska Horizontal Chroma
    /// Sitings" registry (§27.10).
    Other(u64),
}

impl ChromaSitingHorz {
    pub fn from_raw(v: u64) -> Self {
        match v {
            ids::CHROMA_SITING_UNSPECIFIED => Self::Unspecified,
            ids::CHROMA_SITING_HORZ_LEFT_COLLOCATED => Self::LeftCollocated,
            ids::CHROMA_SITING_HALF => Self::Half,
            other => Self::Other(other),
        }
    }

    /// Inverse of [`ChromaSitingHorz::from_raw`]: return the raw integer this
    /// variant maps to. Used by the muxer to write `ChromaSitingHorz` (RFC
    /// 9559 §5.1.4.1.28.23, Table 13) verbatim, including the
    /// [`ChromaSitingHorz::Other`] forward-compat variant for values
    /// registered after RFC 9559 in §27.10.
    pub fn to_raw(self) -> u64 {
        match self {
            Self::Unspecified => ids::CHROMA_SITING_UNSPECIFIED,
            Self::LeftCollocated => ids::CHROMA_SITING_HORZ_LEFT_COLLOCATED,
            Self::Half => ids::CHROMA_SITING_HALF,
            Self::Other(v) => v,
        }
    }
}

/// `ChromaSitingVert` (RFC 9559 §5.1.4.1.28.24, Table 14).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ChromaSitingVert {
    /// `0` — spec default.
    #[default]
    Unspecified,
    /// `1` — top collocated.
    TopCollocated,
    /// `2` — half.
    Half,
    /// Any other value — preserved for the "Matroska Vertical Chroma
    /// Sitings" registry (§27.11).
    Other(u64),
}

impl ChromaSitingVert {
    pub fn from_raw(v: u64) -> Self {
        match v {
            ids::CHROMA_SITING_UNSPECIFIED => Self::Unspecified,
            ids::CHROMA_SITING_VERT_TOP_COLLOCATED => Self::TopCollocated,
            ids::CHROMA_SITING_HALF => Self::Half,
            other => Self::Other(other),
        }
    }

    /// Inverse of [`ChromaSitingVert::from_raw`]: return the raw integer this
    /// variant maps to. Used by the muxer to write `ChromaSitingVert` (RFC
    /// 9559 §5.1.4.1.28.24, Table 14) verbatim, including the
    /// [`ChromaSitingVert::Other`] forward-compat variant for values
    /// registered after RFC 9559 in §27.11.
    pub fn to_raw(self) -> u64 {
        match self {
            Self::Unspecified => ids::CHROMA_SITING_UNSPECIFIED,
            Self::TopCollocated => ids::CHROMA_SITING_VERT_TOP_COLLOCATED,
            Self::Half => ids::CHROMA_SITING_HALF,
            Self::Other(v) => v,
        }
    }
}

/// `Range` (RFC 9559 §5.1.4.1.28.25, Table 15): clipping of the colour
/// ranges. Spec default `0` (*unspecified*).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColourRange {
    /// `0` — spec default.
    #[default]
    Unspecified,
    /// `1` — broadcast range (clipped to legal-codeword range).
    Broadcast,
    /// `2` — full range, no clipping.
    Full,
    /// `3` — defined by `MatrixCoefficients` / `TransferCharacteristics`.
    DefinedByMatrixAndTransfer,
    /// Any other value — preserved for the "Matroska Color Ranges" registry
    /// (§27.12).
    Other(u64),
}

impl ColourRange {
    pub fn from_raw(v: u64) -> Self {
        match v {
            ids::COLOUR_RANGE_UNSPECIFIED => Self::Unspecified,
            ids::COLOUR_RANGE_BROADCAST => Self::Broadcast,
            ids::COLOUR_RANGE_FULL => Self::Full,
            ids::COLOUR_RANGE_DEFINED_BY_MATRIX_AND_TRANSFER => Self::DefinedByMatrixAndTransfer,
            other => Self::Other(other),
        }
    }

    /// Inverse of [`ColourRange::from_raw`]: return the raw integer this
    /// variant maps to. Used by the muxer to write `Range` (RFC 9559
    /// §5.1.4.1.28.25, Table 15) verbatim, including the
    /// [`ColourRange::Other`] forward-compat variant for values registered
    /// after RFC 9559 in §27.12.
    pub fn to_raw(self) -> u64 {
        match self {
            Self::Unspecified => ids::COLOUR_RANGE_UNSPECIFIED,
            Self::Broadcast => ids::COLOUR_RANGE_BROADCAST,
            Self::Full => ids::COLOUR_RANGE_FULL,
            Self::DefinedByMatrixAndTransfer => ids::COLOUR_RANGE_DEFINED_BY_MATRIX_AND_TRANSFER,
            Self::Other(v) => v,
        }
    }
}

/// `TransferCharacteristics` (RFC 9559 §5.1.4.1.28.26, Table 16) — the
/// transfer function the video uses; adopted from Table 3 of ITU-T H.273.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferCharacteristics {
    Reserved0,
    BT709,
    /// `2` — spec default.
    Unspecified,
    Reserved3,
    Gamma22BT470M,
    Gamma28BT470Bg,
    Smpte170M,
    Smpte240M,
    Linear,
    Log,
    LogSqrt,
    Iec61966_2_4,
    BT1361ExtendedColourGamut,
    Iec61966_2_1,
    BT2020TenBit,
    BT2020TwelveBit,
    BT2100Pq,
    SmpteSt428_1,
    AribStdB67Hlg,
    Other(u64),
}

impl TransferCharacteristics {
    pub fn from_raw(v: u64) -> Self {
        match v {
            0 => Self::Reserved0,
            1 => Self::BT709,
            2 => Self::Unspecified,
            3 => Self::Reserved3,
            4 => Self::Gamma22BT470M,
            5 => Self::Gamma28BT470Bg,
            6 => Self::Smpte170M,
            7 => Self::Smpte240M,
            8 => Self::Linear,
            9 => Self::Log,
            10 => Self::LogSqrt,
            11 => Self::Iec61966_2_4,
            12 => Self::BT1361ExtendedColourGamut,
            13 => Self::Iec61966_2_1,
            14 => Self::BT2020TenBit,
            15 => Self::BT2020TwelveBit,
            16 => Self::BT2100Pq,
            17 => Self::SmpteSt428_1,
            18 => Self::AribStdB67Hlg,
            other => Self::Other(other),
        }
    }

    /// Inverse of [`TransferCharacteristics::from_raw`]: return the raw
    /// integer this variant maps to. Used by the muxer to write
    /// `TransferCharacteristics` (RFC 9559 §5.1.4.1.28.26, Table 16)
    /// verbatim, including the [`TransferCharacteristics::Other`]
    /// forward-compat variant for values registered after RFC 9559.
    pub fn to_raw(self) -> u64 {
        match self {
            Self::Reserved0 => 0,
            Self::BT709 => 1,
            Self::Unspecified => 2,
            Self::Reserved3 => 3,
            Self::Gamma22BT470M => 4,
            Self::Gamma28BT470Bg => 5,
            Self::Smpte170M => 6,
            Self::Smpte240M => 7,
            Self::Linear => 8,
            Self::Log => 9,
            Self::LogSqrt => 10,
            Self::Iec61966_2_4 => 11,
            Self::BT1361ExtendedColourGamut => 12,
            Self::Iec61966_2_1 => 13,
            Self::BT2020TenBit => 14,
            Self::BT2020TwelveBit => 15,
            Self::BT2100Pq => 16,
            Self::SmpteSt428_1 => 17,
            Self::AribStdB67Hlg => 18,
            Self::Other(v) => v,
        }
    }
}

/// `Primaries` (RFC 9559 §5.1.4.1.28.27, Table 17) — the colour primaries
/// the video uses; adopted from Table 2 of ITU-T H.273.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Primaries {
    Reserved0,
    BT709,
    /// `2` — spec default.
    Unspecified,
    Reserved3,
    BT470M,
    BT470Bg,
    BT601_525Smpte170M,
    Smpte240M,
    Film,
    BT2020,
    SmpteSt428_1,
    SmpteRp432_2,
    SmpteEg432_2,
    EbuTech3213EJedecP22Phosphors,
    Other(u64),
}

impl Primaries {
    pub fn from_raw(v: u64) -> Self {
        match v {
            0 => Self::Reserved0,
            1 => Self::BT709,
            2 => Self::Unspecified,
            3 => Self::Reserved3,
            4 => Self::BT470M,
            5 => Self::BT470Bg,
            6 => Self::BT601_525Smpte170M,
            7 => Self::Smpte240M,
            8 => Self::Film,
            9 => Self::BT2020,
            10 => Self::SmpteSt428_1,
            11 => Self::SmpteRp432_2,
            12 => Self::SmpteEg432_2,
            22 => Self::EbuTech3213EJedecP22Phosphors,
            other => Self::Other(other),
        }
    }

    /// Inverse of [`Primaries::from_raw`]: return the raw integer this
    /// variant maps to. Used by the muxer to write `Primaries` (RFC 9559
    /// §5.1.4.1.28.27, Table 17) verbatim, including the
    /// [`Primaries::Other`] forward-compat variant for values registered
    /// after RFC 9559. Note the table's gap between `12` and `22` —
    /// `EbuTech3213EJedecP22Phosphors` maps back to `22`, not `13`.
    pub fn to_raw(self) -> u64 {
        match self {
            Self::Reserved0 => 0,
            Self::BT709 => 1,
            Self::Unspecified => 2,
            Self::Reserved3 => 3,
            Self::BT470M => 4,
            Self::BT470Bg => 5,
            Self::BT601_525Smpte170M => 6,
            Self::Smpte240M => 7,
            Self::Film => 8,
            Self::BT2020 => 9,
            Self::SmpteSt428_1 => 10,
            Self::SmpteRp432_2 => 11,
            Self::SmpteEg432_2 => 12,
            Self::EbuTech3213EJedecP22Phosphors => 22,
            Self::Other(v) => v,
        }
    }
}

/// `MasteringMetadata` (RFC 9559 §5.1.4.1.28.30..§5.1.4.1.28.40): the
/// SMPTE 2086 mastering-display description that accompanies HDR content.
/// All fields default to `None` when the file omitted them; the typed
/// surface preserves which sub-elements were actually present (the spec
/// does not require all-or-nothing — a file may carry only LuminanceMax,
/// for example).
///
/// `Primary{R,G,B}Chromaticity{X,Y}` and `WhitePointChromaticity{X,Y}` are
/// CIE-1931 chromaticities in the range `[0.0, 1.0]` (RFC 9559 ranges them
/// `0x0p+0..0x1p+0`). `Luminance{Max,Min}` are in cd/m² and ranged `>= 0`.
/// The parser does not validate range — values that fall outside the spec
/// range still surface so callers can detect them.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct MasteringMetadata {
    primary_r_chromaticity_x: Option<f64>,
    primary_r_chromaticity_y: Option<f64>,
    primary_g_chromaticity_x: Option<f64>,
    primary_g_chromaticity_y: Option<f64>,
    primary_b_chromaticity_x: Option<f64>,
    primary_b_chromaticity_y: Option<f64>,
    white_point_chromaticity_x: Option<f64>,
    white_point_chromaticity_y: Option<f64>,
    luminance_max: Option<f64>,
    luminance_min: Option<f64>,
}

impl MasteringMetadata {
    /// Red X chromaticity coordinate (RFC 9559 §5.1.4.1.28.31), defined by
    /// CIE-1931. Range `[0.0, 1.0]`.
    pub fn primary_r_chromaticity_x(&self) -> Option<f64> {
        self.primary_r_chromaticity_x
    }

    /// Red Y chromaticity coordinate (RFC 9559 §5.1.4.1.28.32).
    pub fn primary_r_chromaticity_y(&self) -> Option<f64> {
        self.primary_r_chromaticity_y
    }

    /// Green X chromaticity coordinate (RFC 9559 §5.1.4.1.28.33).
    pub fn primary_g_chromaticity_x(&self) -> Option<f64> {
        self.primary_g_chromaticity_x
    }

    /// Green Y chromaticity coordinate (RFC 9559 §5.1.4.1.28.34).
    pub fn primary_g_chromaticity_y(&self) -> Option<f64> {
        self.primary_g_chromaticity_y
    }

    /// Blue X chromaticity coordinate (RFC 9559 §5.1.4.1.28.35).
    pub fn primary_b_chromaticity_x(&self) -> Option<f64> {
        self.primary_b_chromaticity_x
    }

    /// Blue Y chromaticity coordinate (RFC 9559 §5.1.4.1.28.36).
    pub fn primary_b_chromaticity_y(&self) -> Option<f64> {
        self.primary_b_chromaticity_y
    }

    /// White-point X chromaticity coordinate (RFC 9559 §5.1.4.1.28.37).
    pub fn white_point_chromaticity_x(&self) -> Option<f64> {
        self.white_point_chromaticity_x
    }

    /// White-point Y chromaticity coordinate (RFC 9559 §5.1.4.1.28.38).
    pub fn white_point_chromaticity_y(&self) -> Option<f64> {
        self.white_point_chromaticity_y
    }

    /// Maximum luminance, in cd/m² (RFC 9559 §5.1.4.1.28.39, range `>= 0`).
    pub fn luminance_max(&self) -> Option<f64> {
        self.luminance_max
    }

    /// Minimum luminance, in cd/m² (RFC 9559 §5.1.4.1.28.40, range `>= 0`).
    pub fn luminance_min(&self) -> Option<f64> {
        self.luminance_min
    }
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
///
/// `Default::default()` materialises the spec-defined defaults: `enabled`
/// is `true` (RFC 9559 §5.1.7.1.4 default = 1) and `hidden` is `false`
/// (default = 0); every other field is the zero-value equivalent of
/// "element absent."
#[derive(Clone, Debug, PartialEq)]
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
    /// `ChapterFlagEnabled` (RFC 9559 §5.1.7.1.4.5a / 5.1.7.1.4 enabled
    /// flag). The spec defaults to `1` (enabled); when the element is
    /// absent we materialise that default as `true` so consumers don't
    /// special-case the missing element. A `false` value means the
    /// chapter should NOT be available for playback (Section 20.2.5).
    pub enabled: bool,
    /// `ChapterSegmentUUID` (RFC 9559 §5.1.7.1.4.6) — the 16-byte
    /// SegmentUUID of another Segment to play during this chapter, used
    /// for Medium-Linking Segments (Section 17.2). `None` when absent.
    /// Length is exactly 16 bytes when present.
    pub segment_uuid: Option<Vec<u8>>,
    /// `ChapterSegmentEditionUID` (RFC 9559 §5.1.7.1.4.7) — the
    /// `EditionUID` to play from the Segment named by `segment_uuid`.
    /// `None` when absent (no specific edition selected). Never zero.
    pub segment_edition_uid: Option<u64>,
    /// `ChapterPhysicalEquiv` (RFC 9559 §5.1.7.1.4.8) — the physical
    /// equivalent of this atom (e.g. "DVD" = 60, "SIDE" = 50). See
    /// Section 20.4 for the full table. `None` when absent.
    pub physical_equiv: Option<u64>,
    /// `ChapterDisplay` rows (RFC 9559 §5.1.7.1.4.9), in on-disk order —
    /// one per language. Empty when the atom carries no display string.
    pub displays: Vec<ChapterDisplay>,
    /// `ChapProcess` masters (RFC 9559 §5.1.7.1.4.14), in on-disk order —
    /// the chapter-codec commands (DVD-menu / Matroska-Script) attached to
    /// this atom. Empty when the atom carries no process commands.
    pub chap_processes: Vec<ChapProcess>,
    /// Nested `ChapterAtom`s (RFC 9559 §5.1.7.1.4 is `recursive`).
    pub children: Vec<Chapter>,
}

impl Default for Chapter {
    fn default() -> Self {
        Self {
            index: 0,
            uid: None,
            string_uid: None,
            time_start_ns: 0,
            time_end_ns: None,
            hidden: false,
            // RFC 9559 §5.1.7.1.4: ChapterFlagEnabled has spec default 1.
            enabled: true,
            segment_uuid: None,
            segment_edition_uid: None,
            physical_equiv: None,
            displays: Vec::new(),
            chap_processes: Vec::new(),
            children: Vec::new(),
        }
    }
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

/// One `ChapProcess` master (RFC 9559 §5.1.7.1.4.14) — the set of
/// chapter-codec commands attached to a [`Chapter`]. The
/// [`codec_id`](Self::codec_id) selects how the private/command bytes are
/// interpreted (`0` = Matroska Script, `1` = DVD-menu; see Table 31). The
/// container surfaces the raw payloads only — it never executes a chapter
/// command. Part of [`Chapter::chap_processes`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChapProcess {
    /// `ChapProcessCodecID` (RFC 9559 §5.1.7.1.4.15) — the chapter-codec
    /// type. Spec default is `0` (Matroska Script); materialised so
    /// consumers don't special-case the absent (mandatory) element.
    pub codec_id: u64,
    /// `ChapProcessPrivate` (RFC 9559 §5.1.7.1.4.16) — optional data
    /// attached to the codec id (for `codec_id == 1` this is the "DVD
    /// level" equivalent). `None` when absent. Raw bytes, never decoded.
    pub private: Option<Vec<u8>>,
    /// `ChapProcessCommand` masters (RFC 9559 §5.1.7.1.4.17), in on-disk
    /// order. Each is a timing (when to run) plus a binary command
    /// payload. Empty when the process carries no commands.
    pub commands: Vec<ChapProcessCommand>,
}

/// One `ChapProcessCommand` master (RFC 9559 §5.1.7.1.4.17) — a single
/// chapter-codec command and when it should run. Part of
/// [`ChapProcess::commands`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChapProcessCommand {
    /// `ChapProcessTime` (RFC 9559 §5.1.7.1.4.18) — when the command
    /// SHOULD be handled: `0` during the whole chapter, `1` before
    /// starting playback, `2` after playback (Table 32). Mandatory per
    /// spec; defaults to `0` when the (malformed) element is absent.
    pub time: u64,
    /// `ChapProcessData` (RFC 9559 §5.1.7.1.4.19) — the command
    /// information, interpreted per the owning [`ChapProcess::codec_id`]
    /// (for `codec_id == 1` these are the binary DVD cell pre/post
    /// commands). Raw bytes, never decoded.
    pub data: Vec<u8>,
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
                let ee_end = r.stream_position()?.saturating_add(e.size);
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
                let ca_end = r.stream_position()?.saturating_add(e.size);
                let atom = parse_chapter_atom(
                    r,
                    ca_end,
                    metadata,
                    chapter_index,
                    chapter_uid_to_index,
                    0,
                )?;
                edition.chapters.push(atom);
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(edition)
}

/// Maximum recursion depth for nested `ChapterAtom` elements. RFC 9559
/// permits arbitrary nesting via the spec's recursive `ChapterAtom`
/// definition (a chapter atom may carry child `ChapterAtom` elements);
/// real files never go more than a couple of levels deep, but a crafted
/// input can pile thousands of nested headers in a few KB and blow the
/// (small, libfuzzer-sized) call stack. Cap at a value comfortably
/// beyond any legitimate use.
const MAX_CHAPTER_NESTING: u32 = 64;

fn parse_chapter_atom(
    r: &mut dyn ReadSeek,
    end: u64,
    metadata: &mut Vec<(String, String)>,
    chapter_index: &mut u32,
    chapter_uid_to_index: &mut std::collections::HashMap<u64, u32>,
    depth: u32,
) -> Result<Chapter> {
    if depth >= MAX_CHAPTER_NESTING {
        return Err(Error::invalid(format!(
            "MKV: ChapterAtom nesting exceeds {MAX_CHAPTER_NESTING}"
        )));
    }
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
            ids::CHAPTER_FLAG_ENABLED => atom.enabled = read_uint(r, e.size as usize)? != 0,
            ids::CHAPTER_SEGMENT_UUID => {
                // RFC 9559 §5.1.7.1.4.6: length is exactly 16 bytes.
                // A malformed file may carry a different length; we read
                // exactly what's there and let the consumer treat any
                // value with `len() != 16` as malformed.
                atom.segment_uuid = Some(crate::ebml::read_bytes(r, e.size as usize)?);
            }
            ids::CHAPTER_SEGMENT_EDITION_UID => {
                let v = read_uint(r, e.size as usize)?;
                // Spec range "not 0" — drop zero values silently rather
                // than store a sentinel the consumer would have to filter.
                if v != 0 {
                    atom.segment_edition_uid = Some(v);
                }
            }
            ids::CHAPTER_PHYSICAL_EQUIV => {
                atom.physical_equiv = Some(read_uint(r, e.size as usize)?);
            }
            ids::CHAPTER_DISPLAY => {
                let cd_end = r.stream_position()?.saturating_add(e.size);
                if let Some(disp) = parse_chapter_display(r, cd_end)? {
                    atom.displays.push(disp);
                }
            }
            ids::CHAP_PROCESS => {
                let cp_end = r.stream_position()?.saturating_add(e.size);
                atom.chap_processes.push(parse_chap_process(r, cp_end)?);
            }
            ids::CHAPTER_ATOM => {
                let ca_end = r.stream_position()?.saturating_add(e.size);
                let child = parse_chapter_atom(
                    r,
                    ca_end,
                    metadata,
                    chapter_index,
                    chapter_uid_to_index,
                    depth + 1,
                )?;
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

/// Parse one `ChapProcess` master (RFC 9559 §5.1.7.1.4.14) into a typed
/// [`ChapProcess`]. `ChapProcessCodecID` defaults to `0` (Matroska Script)
/// per §5.1.7.1.4.15; the private data and command payloads are surfaced
/// as raw bytes — the container never executes a chapter command.
fn parse_chap_process(r: &mut dyn ReadSeek, end: u64) -> Result<ChapProcess> {
    let mut proc = ChapProcess::default();
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::CHAP_PROCESS_CODEC_ID => proc.codec_id = read_uint(r, e.size as usize)?,
            ids::CHAP_PROCESS_PRIVATE => {
                proc.private = Some(crate::ebml::read_bytes(r, e.size as usize)?);
            }
            ids::CHAP_PROCESS_COMMAND => {
                let cc_end = r.stream_position()?.saturating_add(e.size);
                proc.commands.push(parse_chap_process_command(r, cc_end)?);
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(proc)
}

/// Parse one `ChapProcessCommand` master (RFC 9559 §5.1.7.1.4.17) into a
/// typed [`ChapProcessCommand`]. `ChapProcessTime` defaults to `0` ("during
/// the whole chapter") when the (mandatory) element is absent; the
/// `ChapProcessData` payload is surfaced as raw bytes.
fn parse_chap_process_command(r: &mut dyn ReadSeek, end: u64) -> Result<ChapProcessCommand> {
    let mut cmd = ChapProcessCommand::default();
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::CHAP_PROCESS_TIME => cmd.time = read_uint(r, e.size as usize)?,
            ids::CHAP_PROCESS_DATA => cmd.data = crate::ebml::read_bytes(r, e.size as usize)?,
            _ => skip(r, e.size)?,
        }
    }
    Ok(cmd)
}

/// Parse an `Attachments` master element. Each `AttachedFile` surfaces in
/// two places: the flat `Demuxer::metadata` view (up to four keys per
/// attachment — `attachment:N:filename`, `attachment:N:mime_type`,
/// `attachment:N:size_bytes`, `attachment:N:description`), and the typed
/// [`MkvDemuxer::attachments`] accessor (full [`Attachment`] record with
/// the on-disk byte range of the `FileData` payload so callers can pull
/// the bytes on demand via [`MkvDemuxer::attachment_data`]).
///
/// File payloads are skipped via seek during the up-front walk so we don't
/// pull megabytes of data into memory just to expose a filename. Sizes are
/// reported from the `FileData` element header so the `size_bytes` value
/// is the on-disk size (no compression decoded).
fn parse_attachments(
    r: &mut dyn ReadSeek,
    end: u64,
    metadata: &mut Vec<(String, String)>,
    attachment_uid_to_index: &mut std::collections::HashMap<u64, u32>,
    attachments: &mut Vec<Attachment>,
) -> Result<()> {
    let mut idx: u32 = 0;
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::ATTACHED_FILE => {
                let af_end = r.stream_position()?.saturating_add(e.size);
                idx += 1;
                parse_attached_file(
                    r,
                    af_end,
                    metadata,
                    idx,
                    attachment_uid_to_index,
                    attachments,
                )?;
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
    attachments: &mut Vec<Attachment>,
) -> Result<()> {
    let mut filename: Option<String> = None;
    let mut mime: Option<String> = None;
    let mut description: Option<String> = None;
    let mut uid: u64 = 0;
    let mut data_offset: u64 = 0;
    let mut data_size: u64 = 0;
    let mut has_data = false;
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::FILE_NAME => filename = Some(read_string(r, e.size as usize)?),
            ids::FILE_MIME_TYPE => mime = Some(read_string(r, e.size as usize)?),
            ids::FILE_DESCRIPTION => description = Some(read_string(r, e.size as usize)?),
            ids::FILE_UID => {
                let v = read_uint(r, e.size as usize)?;
                if v != 0 {
                    uid = v;
                    attachment_uid_to_index.insert(v, index);
                }
            }
            ids::FILE_DATA => {
                // Record the on-disk byte range of the payload before skipping
                // past it. `stream_position()` here is the byte right after
                // the `FileData` element's id+size header — i.e. the first
                // byte of the payload itself. `attachment_data` re-reads from
                // this offset on demand.
                data_offset = r.stream_position()?;
                data_size = e.size;
                has_data = true;
                skip(r, e.size)?;
            }
            _ => skip(r, e.size)?,
        }
    }
    if let Some(ref n) = filename {
        if !n.is_empty() {
            metadata.push((format!("attachment:{index}:filename"), n.clone()));
        }
    }
    if let Some(ref m) = mime {
        if !m.is_empty() {
            metadata.push((format!("attachment:{index}:mime_type"), m.clone()));
        }
    }
    if has_data {
        metadata.push((
            format!("attachment:{index}:size_bytes"),
            data_size.to_string(),
        ));
    }
    if let Some(ref d) = description {
        if !d.is_empty() {
            metadata.push((format!("attachment:{index}:description"), d.clone()));
        }
    }
    attachments.push(Attachment {
        index,
        filename: filename.unwrap_or_default(),
        mime_type: mime.unwrap_or_default(),
        description: description.unwrap_or_default(),
        uid,
        data_offset,
        data_size,
    });
    Ok(())
}

/// One `Attachments\AttachedFile` (RFC 9559 §5.1.6) parsed from the
/// Segment.
///
/// Embedded fonts, cover art, lyrics, scripts, and other auxiliary files
/// can be packed into a Matroska/WebM file as attachments. The demuxer
/// walks the `Attachments` master up front, captures each attachment's
/// metadata (filename / MIME / description / UID) and the on-disk byte
/// range of its `FileData` payload, but does **not** read the payload
/// bytes — pulling a multi-megabyte font into RAM just to see its
/// filename would be wasteful. Use [`MkvDemuxer::attachment_data`] to
/// fetch the payload on demand.
///
/// Returned in segment order. The 1-based [`Attachment::index`] is the
/// same `N` used by the flat `attachment:N:*` metadata keys and by
/// `tag:attachment:N:<name>` Tag scopes, so a caller can correlate the
/// three views.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// 1-based position of this attachment within the Segment's
    /// `Attachments` element. Matches the `N` in the corresponding
    /// `attachment:N:filename` / `attachment:N:mime_type` /
    /// `attachment:N:size_bytes` / `attachment:N:description` metadata
    /// keys and in any `tag:attachment:N:<name>` Tag scope.
    pub index: u32,
    /// `FileName` (RFC 9559 §5.1.6.2). Empty string when the attachment
    /// had no `FileName` child — spec marks it as mandatory but the
    /// demuxer tolerates the omission rather than rejecting the whole
    /// file.
    pub filename: String,
    /// `FileMimeType` (RFC 9559 §5.1.6.3). Empty string when absent.
    pub mime_type: String,
    /// `FileDescription` (RFC 9559 §5.1.6.1) — optional human-readable
    /// description of the attachment's contents. Empty string when
    /// absent.
    pub description: String,
    /// `FileUID` (RFC 9559 §5.1.6.5). `0` when the attachment had no
    /// `FileUID` child or the value was an explicit `0` (which the spec
    /// reserves as "not present"). Non-zero UIDs are what
    /// `Tags.Targets.TagAttachmentUID` references, and the demuxer uses
    /// them to map `tag:attachment:N:<name>` scopes back to this slot.
    pub uid: u64,
    /// Absolute byte offset of the `FileData` payload's first byte in
    /// the input stream. Combined with [`Attachment::data_size`], this is
    /// the byte range [`MkvDemuxer::attachment_data`] reads from.
    pub data_offset: u64,
    /// Length in bytes of the `FileData` payload as it sits on disk (no
    /// compression decoded). `0` when the attachment had no `FileData`
    /// child (which would be unusual — the spec marks the element as
    /// mandatory).
    pub data_size: u64,
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
                let body_end = r.stream_position()?.saturating_add(e.size);
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
                let body_end = r.stream_position()?.saturating_add(e.size);
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
    crc_status: &mut Vec<CrcStatus>,
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
                body_start.saturating_add(e.size)
            };
            if body_end > end {
                r.seek(SeekFrom::Start(pos))?;
                return Ok(());
            }
            // Validate a leading CRC-32 child on the late-Cues path too
            // (RFC 9559 §6.2 — Cues SHOULD carry CRC-32 like every other
            // Top-Level master, and the muxer we ship does). The helper
            // rewinds the reader to `body_start` before returning, so
            // `parse_cues` proceeds unaffected — and if the leading child
            // happens to be a `CRC-32` rather than a `CuePoint`,
            // `parse_cues` skips it the same way the early-Cues path
            // does, since it tolerates unknown ids inside Cues.
            if e.size != VINT_UNKNOWN_SIZE {
                if let Some(s) = validate_top_level_crc(r, ids::CUES, body_start, body_end)? {
                    crc_status.push(s);
                }
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
        let body_end = body_start.saturating_add(e.size);
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
        let body_end = r.stream_position()?.saturating_add(e.size);
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
                let body_end = r.stream_position()?.saturating_add(e.size);
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
            ids::LANGUAGE => t.language = Some(read_string(r, e.size as usize)?),
            ids::AUDIO => {
                let body_end = r.stream_position()?.saturating_add(e.size);
                parse_audio(r, body_end, t)?;
            }
            ids::VIDEO => {
                let body_end = r.stream_position()?.saturating_add(e.size);
                parse_video(r, body_end, t)?;
            }
            ids::TRACK_OPERATION => {
                let body_end = r.stream_position()?.saturating_add(e.size);
                let mut op = RawTrackOperation::default();
                parse_track_operation(r, body_end, &mut op)?;
                t.track_operation = Some(op);
            }
            ids::CONTENT_ENCODINGS => {
                let body_end = r.stream_position()?.saturating_add(e.size);
                t.content_encodings = Some(parse_content_encodings(r, body_end)?);
            }
            ids::BLOCK_ADDITION_MAPPING => {
                let body_end = r.stream_position()?.saturating_add(e.size);
                let mapping = parse_block_addition_mapping(r, body_end)?;
                t.block_addition_mappings.push(mapping);
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
                let body_end = r.stream_position()?.saturating_add(e.size);
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
                let body_end = r.stream_position()?.saturating_add(e.size);
                comp = Some(parse_content_compression(r, body_end)?);
            }
            ids::CONTENT_ENCRYPTION => {
                let body_end = r.stream_position()?.saturating_add(e.size);
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
                let body_end = r.stream_position()?.saturating_add(e.size);
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
                let body_end = r.stream_position()?.saturating_add(e.size);
                parse_combine_planes(r, body_end, op)?;
            }
            ids::TRACK_JOIN_BLOCKS => {
                let body_end = r.stream_position()?.saturating_add(e.size);
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
                let body_end = r.stream_position()?.saturating_add(e.size);
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

/// Parse one `BlockAdditionMapping` master (RFC 9559 §5.1.4.1.17) into the
/// typed [`BlockAdditionMapping`]. `BlockAddIDType` (§5.1.4.1.17.3) has
/// spec default `0` (codec-defined) which is materialised here so an empty
/// mapping master decodes to a fully-typed "codec-defined" record;
/// `BlockAddIDValue` / `BlockAddIDName` / `BlockAddIDExtraData` carry no
/// defaults (`maxOccurs == 1`, no `default:` clause) so they stay
/// `Option<…>` and an absent child surfaces as `None`. Unknown children
/// are skipped — forward-compat with future schema extensions.
fn parse_block_addition_mapping(r: &mut dyn ReadSeek, end: u64) -> Result<BlockAdditionMapping> {
    let mut value: Option<u64> = None;
    let mut name: Option<String> = None;
    // §5.1.4.1.17.3 default is `0` (codec-defined): "If BlockAddIDType is
    // 0, the BlockAddIDValue and corresponding BlockAddID values MUST be 1."
    let mut addid_type: u64 = 0;
    let mut extra_data: Option<Vec<u8>> = None;
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::BLOCK_ADD_ID_VALUE => value = Some(read_uint(r, e.size as usize)?),
            ids::BLOCK_ADD_ID_NAME => name = Some(read_string(r, e.size as usize)?),
            ids::BLOCK_ADD_ID_TYPE => addid_type = read_uint(r, e.size as usize)?,
            ids::BLOCK_ADD_ID_EXTRA_DATA => extra_data = Some(read_bytes(r, e.size as usize)?),
            _ => skip(r, e.size)?,
        }
    }
    Ok(BlockAdditionMapping {
        value,
        name,
        addid_type,
        extra_data,
    })
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
    // The `Video` master was seen. Materialise the spec defaults for
    // FlagInterlaced (§5.1.4.1.28.1, default 0 = undetermined) and
    // FieldOrder (§5.1.4.1.28.2, default 2 = undetermined); explicit
    // children below override them.
    let mut flag_interlaced: u64 = ids::FLAG_INTERLACED_UNDETERMINED;
    let mut field_order: u64 = ids::FIELD_ORDER_UNDETERMINED;
    // Geometry quartet (RFC 9559 §5.1.4.1.28.8..§5.1.4.1.28.14):
    // PixelCrop* defaults are `0` per §5.1.4.1.28.8..11; DisplayUnit
    // default is `0` per §5.1.4.1.28.14 (pixels). DisplayWidth /
    // DisplayHeight have no fixed default — the spec ranges them as
    // "not 0", so a `0` here means "absent from file" and the typed
    // surface will fall back to the §5.1.4.1.28.12 / .13 derived defaults
    // (only valid when DisplayUnit == 0).
    let mut crop_top: u64 = 0;
    let mut crop_bottom: u64 = 0;
    let mut crop_left: u64 = 0;
    let mut crop_right: u64 = 0;
    let mut display_width: u64 = 0;
    let mut display_height: u64 = 0;
    let mut display_unit: u64 = ids::DISPLAY_UNIT_PIXELS;
    // StereoMode (§5.1.4.1.28.3, default 0 = mono). A `Video` master with
    // no explicit `StereoMode` decodes to `Mono` on the typed surface.
    let mut stereo_mode: u64 = ids::STEREO_MODE_MONO;
    // AlphaMode (§5.1.4.1.28.4, default 0 = none).
    let mut alpha_mode: u64 = ids::ALPHA_MODE_NONE;
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::PIXEL_WIDTH => t.width = read_uint(r, e.size as usize)?,
            ids::PIXEL_HEIGHT => t.height = read_uint(r, e.size as usize)?,
            ids::FLAG_INTERLACED => flag_interlaced = read_uint(r, e.size as usize)?,
            ids::FIELD_ORDER => field_order = read_uint(r, e.size as usize)?,
            ids::STEREO_MODE => stereo_mode = read_uint(r, e.size as usize)?,
            ids::ALPHA_MODE => alpha_mode = read_uint(r, e.size as usize)?,
            ids::ASPECT_RATIO_TYPE => {
                // Reclaimed Appendix A.24 element — no spec default; surface only
                // when the file actually carries it.
                t.aspect_ratio_type_raw = Some(read_uint(r, e.size as usize)?)
            }
            ids::UNCOMPRESSED_FOURCC => {
                // 4-byte binary FourCC (§5.1.4.1.28.15). Preserve verbatim;
                // the typed `fourcc()` accessor rejects payloads whose length
                // isn't exactly 4.
                t.uncompressed_fourcc_raw = Some(read_bytes(r, e.size as usize)?)
            }
            ids::PIXEL_CROP_TOP => crop_top = read_uint(r, e.size as usize)?,
            ids::PIXEL_CROP_BOTTOM => crop_bottom = read_uint(r, e.size as usize)?,
            ids::PIXEL_CROP_LEFT => crop_left = read_uint(r, e.size as usize)?,
            ids::PIXEL_CROP_RIGHT => crop_right = read_uint(r, e.size as usize)?,
            ids::DISPLAY_WIDTH => display_width = read_uint(r, e.size as usize)?,
            ids::DISPLAY_HEIGHT => display_height = read_uint(r, e.size as usize)?,
            ids::DISPLAY_UNIT => display_unit = read_uint(r, e.size as usize)?,
            ids::COLOUR => {
                let body_end = r.stream_position()? + e.size;
                // Spec defaults — set up front so an empty `Colour` master
                // still surfaces the §5.1.4.1.28.17 / §5.1.4.1.28.26 /
                // §5.1.4.1.28.27 default `2`s and the §5.1.4.1.28.23..
                // §5.1.4.1.28.25 default `0`s. `parse_colour` overrides only
                // the children the file actually carries.
                let mut c = RawColour {
                    matrix_coefficients: 2,
                    transfer_characteristics: 2,
                    primaries: 2,
                    chroma_siting_horz: ids::CHROMA_SITING_UNSPECIFIED,
                    chroma_siting_vert: ids::CHROMA_SITING_UNSPECIFIED,
                    range: ids::COLOUR_RANGE_UNSPECIFIED,
                    bits_per_channel: 0,
                    ..Default::default()
                };
                parse_colour(r, body_end, &mut c)?;
                t.colour_raw = Some(c);
            }
            ids::PROJECTION => {
                let body_end = r.stream_position()? + e.size;
                // Spec defaults — `ProjectionType` defaults to 0 (rectangular)
                // per §5.1.4.1.28.42; the three pose floats default to
                // 0x0p+0 per §5.1.4.1.28.44..46. `parse_projection` overrides
                // only the children the file actually carries, so an empty
                // `Projection` master surfaces an identity rectangular
                // projection.
                let mut p = RawProjection {
                    projection_type_raw: ids::PROJECTION_TYPE_RECTANGULAR,
                    ..Default::default()
                };
                parse_projection(r, body_end, &mut p)?;
                t.projection_raw = Some(p);
            }
            _ => skip(r, e.size)?,
        }
    }
    t.interlacing_raw = Some((flag_interlaced, field_order));
    t.geometry_raw = Some((
        crop_top,
        crop_bottom,
        crop_left,
        crop_right,
        display_width,
        display_height,
        display_unit,
    ));
    t.stereo_mode_raw = Some(stereo_mode);
    t.alpha_mode_raw = Some(alpha_mode);
    Ok(())
}

/// Parse the `Video > Colour` master (RFC 9559 §5.1.4.1.28.16). `c` is
/// pre-populated by `parse_video` with the spec defaults for every
/// child that has one — this routine overrides only the children the file
/// actually carries, so an empty `Colour` master surfaces exactly the spec
/// defaults.
fn parse_colour(r: &mut dyn ReadSeek, end: u64, c: &mut RawColour) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::MATRIX_COEFFICIENTS => c.matrix_coefficients = read_uint(r, e.size as usize)?,
            ids::BITS_PER_CHANNEL => c.bits_per_channel = read_uint(r, e.size as usize)?,
            ids::CHROMA_SUBSAMPLING_HORZ => {
                c.chroma_subsampling_horz = Some(read_uint(r, e.size as usize)?)
            }
            ids::CHROMA_SUBSAMPLING_VERT => {
                c.chroma_subsampling_vert = Some(read_uint(r, e.size as usize)?)
            }
            ids::CB_SUBSAMPLING_HORZ => {
                c.cb_subsampling_horz = Some(read_uint(r, e.size as usize)?)
            }
            ids::CB_SUBSAMPLING_VERT => {
                c.cb_subsampling_vert = Some(read_uint(r, e.size as usize)?)
            }
            ids::CHROMA_SITING_HORZ => c.chroma_siting_horz = read_uint(r, e.size as usize)?,
            ids::CHROMA_SITING_VERT => c.chroma_siting_vert = read_uint(r, e.size as usize)?,
            ids::COLOUR_RANGE => c.range = read_uint(r, e.size as usize)?,
            ids::TRANSFER_CHARACTERISTICS => {
                c.transfer_characteristics = read_uint(r, e.size as usize)?
            }
            ids::PRIMARIES => c.primaries = read_uint(r, e.size as usize)?,
            ids::MAX_CLL => c.max_cll = Some(read_uint(r, e.size as usize)?),
            ids::MAX_FALL => c.max_fall = Some(read_uint(r, e.size as usize)?),
            ids::MASTERING_METADATA => {
                let body_end = r.stream_position()? + e.size;
                let mut m = MasteringMetadata::default();
                parse_mastering_metadata(r, body_end, &mut m)?;
                c.mastering_metadata = Some(m);
            }
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

/// Parse the `Colour > MasteringMetadata` master (RFC 9559
/// §5.1.4.1.28.30..§5.1.4.1.28.40). Each sub-element is independently
/// optional — the spec does not require any of them to appear together.
fn parse_mastering_metadata(
    r: &mut dyn ReadSeek,
    end: u64,
    m: &mut MasteringMetadata,
) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::PRIMARY_R_CHROMATICITY_X => {
                m.primary_r_chromaticity_x = Some(read_float(r, e.size as usize)?)
            }
            ids::PRIMARY_R_CHROMATICITY_Y => {
                m.primary_r_chromaticity_y = Some(read_float(r, e.size as usize)?)
            }
            ids::PRIMARY_G_CHROMATICITY_X => {
                m.primary_g_chromaticity_x = Some(read_float(r, e.size as usize)?)
            }
            ids::PRIMARY_G_CHROMATICITY_Y => {
                m.primary_g_chromaticity_y = Some(read_float(r, e.size as usize)?)
            }
            ids::PRIMARY_B_CHROMATICITY_X => {
                m.primary_b_chromaticity_x = Some(read_float(r, e.size as usize)?)
            }
            ids::PRIMARY_B_CHROMATICITY_Y => {
                m.primary_b_chromaticity_y = Some(read_float(r, e.size as usize)?)
            }
            ids::WHITE_POINT_CHROMATICITY_X => {
                m.white_point_chromaticity_x = Some(read_float(r, e.size as usize)?)
            }
            ids::WHITE_POINT_CHROMATICITY_Y => {
                m.white_point_chromaticity_y = Some(read_float(r, e.size as usize)?)
            }
            ids::LUMINANCE_MAX => m.luminance_max = Some(read_float(r, e.size as usize)?),
            ids::LUMINANCE_MIN => m.luminance_min = Some(read_float(r, e.size as usize)?),
            _ => skip(r, e.size)?,
        }
    }
    Ok(())
}

/// Parse the `Video > Projection` master (RFC 9559 §5.1.4.1.28.41). `p` is
/// pre-populated by `parse_video` with the spec defaults for every child
/// that has one — this routine overrides only the children the file actually
/// carries. Unknown elements are skipped (forward-compat).
fn parse_projection(r: &mut dyn ReadSeek, end: u64, p: &mut RawProjection) -> Result<()> {
    while r.stream_position()? < end {
        let e = read_element_header(r)?;
        match e.id {
            ids::PROJECTION_TYPE => p.projection_type_raw = read_uint(r, e.size as usize)?,
            ids::PROJECTION_PRIVATE => p.private = Some(read_bytes(r, e.size as usize)?),
            ids::PROJECTION_POSE_YAW => p.pose_yaw = read_float(r, e.size as usize)?,
            ids::PROJECTION_POSE_PITCH => p.pose_pitch = read_float(r, e.size as usize)?,
            ids::PROJECTION_POSE_ROLL => p.pose_roll = read_float(r, e.size as usize)?,
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
    /// Typed `Attachments\AttachedFile` list (RFC 9559 §5.1.6) — see
    /// [`MkvDemuxer::attachments`]. Empty when the file carries no
    /// `Attachments` element. Each entry records the on-disk byte range
    /// of its `FileData` payload so [`MkvDemuxer::attachment_data`] can
    /// pull it on demand without paying for it at open time.
    attachments: Vec<Attachment>,
    /// Per-Top-Level-element `CRC-32` validation results (RFC 8794
    /// §11.3.1) — see [`MkvDemuxer::crc_status`]. Holds both the up-front
    /// statuses captured for `Info` / `Tracks` / `Tags` / `Cues` /
    /// `Chapters` / `Attachments` / `SeekHead` at open time **and** the
    /// statuses captured per `Cluster` as the demuxer first encounters each
    /// one through [`MkvDemuxer::next_packet`] / [`Demuxer::seek_to`]. The
    /// element id distinguishes the two (e.g. [`ids::CLUSTER`] for the
    /// per-Cluster checks).
    crc_status: Vec<CrcStatus>,
    /// Body-start offsets of Cluster elements whose `CRC-32` child has
    /// already been validated and recorded in [`Self::crc_status`]. Used
    /// to dedup the per-Cluster check across the multiple code paths that
    /// open a Cluster (the legacy `advance()` walk and the Cue-driven
    /// [`Self::apply_cue_relative_position`]) and across repeated visits
    /// to the same Cluster (a back-then-forward seek lands on the same
    /// Cluster more than once). Membership keyed by the absolute file
    /// offset of the Cluster's *body* (the byte after its id+size header).
    validated_cluster_starts: std::collections::HashSet<u64>,
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
    /// Per-stream `VideoInterlacing` (RFC 9559 §5.1.4.1.28.1 +
    /// §5.1.4.1.28.2), indexed by stream index. `None` for non-video tracks
    /// and for video tracks whose `TrackEntry` carried no `Video` master —
    /// see [`MkvDemuxer::video_interlacing`].
    video_interlacings: Vec<Option<VideoInterlacing>>,
    /// Per-stream `VideoGeometry` (RFC 9559 §5.1.4.1.28.8..§5.1.4.1.28.14)
    /// — the `PixelCrop{Top,Bottom,Left,Right}` window plus the
    /// `DisplayWidth` / `DisplayHeight` / `DisplayUnit` render-size triple —
    /// indexed by stream index. `None` for non-video tracks and for video
    /// tracks whose `TrackEntry` carried no `Video` master. See
    /// [`MkvDemuxer::video_geometry`].
    video_geometries: Vec<Option<VideoGeometry>>,
    /// Per-stream `VideoColour` (RFC 9559 §5.1.4.1.28.16) — the chroma /
    /// range / transfer / primaries description plus the SMPTE 2086 mastering
    /// metadata — indexed by stream index. `None` for non-video tracks and
    /// for video tracks whose `Video` master carried no `Colour` child. See
    /// [`MkvDemuxer::video_colour`].
    video_colours: Vec<Option<VideoColour>>,
    /// Per-stream `StereoMode` (RFC 9559 §5.1.4.1.28.3) — the single-track
    /// stereo-3D packing — indexed by stream index. `None` for non-video
    /// tracks and for video tracks whose `TrackEntry` carried no `Video`
    /// master; the spec default `0` ([`StereoMode::Mono`]) is materialised
    /// for a `Video` master with no explicit child. See
    /// [`MkvDemuxer::video_stereo_mode`].
    video_stereo_modes: Vec<Option<StereoMode>>,
    /// Per-stream `Projection` (RFC 9559 §5.1.4.1.28.41) — the spherical /
    /// VR-video projection plus the yaw / pitch / roll pose triple —
    /// indexed by stream index. `None` for non-video tracks and for video
    /// tracks whose `Video` master carried no `Projection` child. See
    /// [`MkvDemuxer::video_projection`].
    video_projections: Vec<Option<Projection>>,
    /// Per-stream `AlphaMode` (RFC 9559 §5.1.4.1.28.4) — whether the track
    /// carries WebM-style alpha data in a `BlockAdditional` (`BlockAddID=1`).
    /// `None` for non-video tracks and for video tracks whose `TrackEntry`
    /// carried no `Video` master; the spec default `0` ([`AlphaMode::None`])
    /// is materialised for a `Video` master with no explicit child. See
    /// [`MkvDemuxer::video_alpha_mode`].
    video_alpha_modes: Vec<Option<AlphaMode>>,
    /// Per-stream `AspectRatioType` (RFC 9559 Appendix A.24, reclaimed),
    /// indexed by stream index. `None` when the file did not carry the
    /// element — the reclaimed appendix specifies no default. See
    /// [`MkvDemuxer::video_aspect_ratio_type`].
    video_aspect_ratio_types: Vec<Option<u64>>,
    /// Per-stream `UncompressedFourCC` (RFC 9559 §5.1.4.1.28.15) — the
    /// 4-byte FourCC identifying the uncompressed pixel layout, only
    /// meaningful when `CodecID == "V_UNCOMPRESSED"`. `None` when the
    /// element was absent. See [`MkvDemuxer::video_uncompressed_fourcc`].
    video_uncompressed_fourccs: Vec<Option<UncompressedFourCC>>,
    /// Per-stream `BlockAdditionMapping`s (RFC 9559 §5.1.4.1.17), indexed
    /// by stream index. Empty `Vec` for tracks that carry no
    /// `BlockAdditionMapping` child (the common case — the element only
    /// appears on tracks that use `BlockAdditional` to extend their
    /// on-disk format). See [`MkvDemuxer::block_addition_mappings`].
    block_addition_mappings: Vec<Vec<BlockAdditionMapping>>,
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

    /// `CRC-32` validation results for the Top-Level master and
    /// `Cluster` elements that carried a checksum (RFC 8794 §11.3.1, RFC
    /// 9559 §6.2).
    ///
    /// Matroska files SHOULD put a `CRC-32` child as the first element
    /// of each Top-Level master (Info, Tracks, Tags, Cues, Chapters,
    /// Attachments, SeekHead) *and* each Cluster. When the demuxer
    /// parses an element with such a child, it recomputes the IEEE
    /// CRC-32 over the rest of the element and records a [`CrcStatus`].
    /// Elements without a `CRC-32` child are not represented — the
    /// spec lets a writer omit them.
    ///
    /// Up-front Top-Level masters land at open time. Cluster statuses
    /// are appended as the demuxer first walks each Cluster — either
    /// through [`Demuxer::next_packet`] driving the legacy `advance`
    /// loop, or through a cue-driven [`Demuxer::seek_to`] that opens a
    /// Cluster header. A Cluster is recorded at most once even if a
    /// back-then-forward seek revisits it. The element id on a Cluster
    /// status is [`ids::CLUSTER`]; Cluster bodies declared with the
    /// unknown-size VINT can't be CRC-checked (the spec requires a
    /// bounded body) and produce no status.
    ///
    /// Validation is informational: a mismatching CRC does **not** stop
    /// the demuxer from returning packets (the spec only says a reader
    /// *MAY* ignore the data). Callers that want strict integrity can
    /// inspect this slice and reject a file with any
    /// [`CrcStatus::is_valid`] == `false`.
    ///
    /// Returned in the order the elements were validated — Top-Level
    /// masters in Segment order at open time, then each Cluster in the
    /// order it was first opened by `next_packet` / `seek_to`.
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

    /// Typed `Attachments\AttachedFile` list (RFC 9559 §5.1.6) parsed
    /// from the Segment.
    ///
    /// Surfaces information the flattened [`Demuxer::metadata`] view
    /// drops: every [`Attachment`] keeps its 1-based [`Attachment::index`],
    /// [`Attachment::filename`], [`Attachment::mime_type`],
    /// [`Attachment::description`], [`Attachment::uid`] (for matching
    /// `Tags.Targets.TagAttachmentUID` scopes), and the on-disk byte range
    /// (`data_offset` + `data_size`) of the `FileData` payload — passed
    /// to [`MkvDemuxer::attachment_data`] to fetch the actual bytes on
    /// demand without paying for them at open time.
    ///
    /// Returned in segment order. Empty when the file carries no
    /// `Attachments` element.
    pub fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    /// Read an attachment's `FileData` payload bytes on demand.
    ///
    /// `index` is the 1-based [`Attachment::index`] surfaced by
    /// [`MkvDemuxer::attachments`] — the same `N` used in the
    /// `attachment:N:*` metadata keys.
    ///
    /// Reads exactly [`Attachment::data_size`] bytes from
    /// [`Attachment::data_offset`] in the input stream. The reader's
    /// position is restored afterwards, so calling this between
    /// `next_packet` calls (or while the demuxer is mid-cluster) is
    /// safe.
    ///
    /// Returns `Err(Error::invalid)` if `index` is out of range or
    /// `0` (attachments are 1-indexed).
    pub fn attachment_data(&mut self, index: u32) -> Result<Vec<u8>> {
        if index == 0 {
            return Err(Error::invalid(
                "MKV: attachment index must be 1-based (got 0)",
            ));
        }
        let att = self
            .attachments
            .iter()
            .find(|a| a.index == index)
            .ok_or_else(|| Error::invalid(format!("MKV: no attachment with index {index}")))?;
        let offset = att.data_offset;
        let size = att.data_size;
        // Save and restore the caller's reader position so a payload fetch
        // between `next_packet` calls doesn't shift the cluster walker.
        let saved_pos = self.input.stream_position()?;
        self.input.seek(SeekFrom::Start(offset))?;
        // `Read::take(n).read_to_end()` grows the destination only as bytes
        // actually arrive — defensive against the file being truncated below
        // the recorded `data_size`. Matches the allocation discipline in
        // `ebml::read_bytes`.
        let mut out = Vec::new();
        let n = (&mut *self.input).take(size).read_to_end(&mut out)?;
        // Restore reader position regardless of the read outcome.
        self.input.seek(SeekFrom::Start(saved_pos))?;
        if (n as u64) != size {
            return Err(Error::invalid(format!(
                "MKV: attachment {index} payload truncated (got {n} of {size} bytes)"
            )));
        }
        Ok(out)
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

    /// `BlockAdditionMapping`s (RFC 9559 §5.1.4.1.17) for the stream at
    /// `stream_index`, in on-disk order. Returns an empty slice when the
    /// `TrackEntry` carried no `BlockAdditionMapping` child (the common
    /// case — only tracks that use `BlockAdditional` to extend their
    /// format declare mappings).
    ///
    /// Each [`BlockAdditionMapping`] in the returned slice describes one
    /// `BlockAddID` value the track is allowed to attach to its
    /// `BlockGroup > BlockAdditions > BlockMore` payloads. The mapping
    /// itself carries no payload — the per-frame `BlockAdditional` bytes
    /// stay in the codec/extension's hands; the container only declares
    /// the *shape* of the side channel (which `BlockAddID` values are in
    /// use, what registered [`addid_type`](BlockAdditionMapping::addid_type)
    /// each follows, any per-track
    /// [`extra_data`](BlockAdditionMapping::extra_data) the type
    /// interpreter consults).
    ///
    /// Returns an empty slice for an out-of-range `stream_index`.
    pub fn block_addition_mappings(&self, stream_index: u32) -> &[BlockAdditionMapping] {
        self.block_addition_mappings
            .get(stream_index as usize)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// All per-stream `BlockAdditionMapping` lists (RFC 9559 §5.1.4.1.17),
    /// indexed by stream index. The slice has one `Vec` per stream —
    /// empty when the corresponding `TrackEntry` carried no
    /// `BlockAdditionMapping` child. See
    /// [`MkvDemuxer::block_addition_mappings`] for the semantics.
    pub fn all_block_addition_mappings(&self) -> &[Vec<BlockAdditionMapping>] {
        &self.block_addition_mappings
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

    /// `VideoInterlacing` (RFC 9559 §5.1.4.1.28.1 + §5.1.4.1.28.2) for the
    /// stream at `stream_index`, or `None` when the track is not a video
    /// track / its `TrackEntry` carried no `Video` master.
    ///
    /// The returned [`VideoInterlacing`] folds `FlagInterlaced` and
    /// `FieldOrder` into a single typed pair: a `Video` master with no
    /// `FlagInterlaced` child decodes as [`FlagInterlaced::Undetermined`]
    /// (the spec default `0`), and an interlaced track with no explicit
    /// `FieldOrder` decodes as `Some(FieldOrder::Undetermined)` (the spec
    /// default `2`). `FieldOrder` is suppressed to `None` whenever the track
    /// is not [`FlagInterlaced::Interlaced`], per §5.1.4.1.28.2's "If
    /// FlagInterlaced is not set to 1, this element MUST be ignored".
    ///
    /// Returns `None` for an out-of-range `stream_index`.
    pub fn video_interlacing(&self, stream_index: u32) -> Option<&VideoInterlacing> {
        self.video_interlacings
            .get(stream_index as usize)
            .and_then(|v| v.as_ref())
    }

    /// All per-stream `VideoInterlacing`s (RFC 9559 §5.1.4.1.28.1 +
    /// §5.1.4.1.28.2), indexed by stream index. The slice has one entry per
    /// stream — `None` for non-video tracks and for video tracks whose
    /// `TrackEntry` carried no `Video` master. See
    /// [`MkvDemuxer::video_interlacing`] for the semantics.
    pub fn video_interlacings(&self) -> &[Option<VideoInterlacing>] {
        &self.video_interlacings
    }

    /// `VideoGeometry` (RFC 9559 §5.1.4.1.28.8..§5.1.4.1.28.14) for the
    /// stream at `stream_index`, or `None` when the track is not a video
    /// track / its `TrackEntry` carried no `Video` master.
    ///
    /// The returned [`VideoGeometry`] folds `PixelCrop{Top,Bottom,Left,
    /// Right}` and the `DisplayWidth` / `DisplayHeight` / `DisplayUnit`
    /// triple into a single record. The §5.1.4.1.28.8..11 defaults (`0`)
    /// are always materialised; the derived §5.1.4.1.28.12 / §5.1.4.1.28.13
    /// defaults for `DisplayWidth` / `DisplayHeight`
    /// (`PixelWidth - PixelCropLeft - PixelCropRight` and the analogous
    /// height) are materialised only when `DisplayUnit == 0` (pixels), per
    /// the spec's "If the DisplayUnit of the same TrackEntry is 0, then the
    /// default value for DisplayWidth is ...; else, there is no default
    /// value". For any other `DisplayUnit` an absent element surfaces as
    /// `None`. Underflow in the derived default (a malformed file whose
    /// crops exceed the encoded width or height) also surfaces as `None`.
    ///
    /// Returns `None` for an out-of-range `stream_index`.
    pub fn video_geometry(&self, stream_index: u32) -> Option<&VideoGeometry> {
        self.video_geometries
            .get(stream_index as usize)
            .and_then(|v| v.as_ref())
    }

    /// All per-stream `VideoGeometry`s (RFC 9559
    /// §5.1.4.1.28.8..§5.1.4.1.28.14), indexed by stream index. The slice
    /// has one entry per stream — `None` for non-video tracks and for video
    /// tracks whose `TrackEntry` carried no `Video` master. See
    /// [`MkvDemuxer::video_geometry`] for the semantics.
    pub fn video_geometries(&self) -> &[Option<VideoGeometry>] {
        &self.video_geometries
    }

    /// `VideoColour` (RFC 9559 §5.1.4.1.28.16) for the stream at
    /// `stream_index`, or `None` when the track is not a video track / its
    /// `Video` master carried no `Colour` child.
    ///
    /// The returned [`VideoColour`] folds `MatrixCoefficients`,
    /// `BitsPerChannel`, `Chroma{Subsampling,Cb}Subsampling{Horz,Vert}`,
    /// `ChromaSiting{Horz,Vert}`, `Range`, `TransferCharacteristics`,
    /// `Primaries`, `MaxCLL`, `MaxFALL` and the `MasteringMetadata` master
    /// into a single record. Spec defaults are materialised for the children
    /// that have one (matrix / transfer / primaries default `2` =
    /// unspecified; chroma siting / range / bits-per-channel default `0` =
    /// unspecified); children with no spec default (chroma subsampling,
    /// MaxCLL/MaxFALL) surface as `None` when absent. `MasteringMetadata`
    /// surfaces only when the file actually carried the master.
    ///
    /// Returns `None` for an out-of-range `stream_index`.
    pub fn video_colour(&self, stream_index: u32) -> Option<&VideoColour> {
        self.video_colours
            .get(stream_index as usize)
            .and_then(|v| v.as_ref())
    }

    /// All per-stream `VideoColour`s (RFC 9559 §5.1.4.1.28.16), indexed by
    /// stream index. The slice has one entry per stream — `None` for
    /// non-video tracks and for video tracks whose `Video` master carried no
    /// `Colour` child. See [`MkvDemuxer::video_colour`] for the semantics.
    pub fn video_colours(&self) -> &[Option<VideoColour>] {
        &self.video_colours
    }

    /// `StereoMode` (RFC 9559 §5.1.4.1.28.3) for the stream at
    /// `stream_index`, or `None` when the track is not a video track / its
    /// `TrackEntry` carried no `Video` master.
    ///
    /// The §5.1.4.1.28.3 default `0` ([`StereoMode::Mono`]) is materialised:
    /// a `Video` master with no explicit `StereoMode` decodes as
    /// `Some(StereoMode::Mono)`, distinguishable from `None` (which means
    /// "no `Video` master at all"). Values outside §5.1.4.1.28.3 Table 5
    /// pass through the [`StereoMode::Other`] variant — §27.7 leaves the
    /// registry open for future additions.
    ///
    /// For multi-track stereo (`TrackOperation > TrackCombinePlanes`,
    /// §5.1.4.1.30.1) use [`MkvDemuxer::track_operation`] instead — the two
    /// surfaces are independent and a track MAY carry both (a single-track
    /// StereoMode plus a TrackOperation referring to plane siblings).
    ///
    /// Returns `None` for an out-of-range `stream_index`.
    pub fn video_stereo_mode(&self, stream_index: u32) -> Option<StereoMode> {
        self.video_stereo_modes
            .get(stream_index as usize)
            .and_then(|v| *v)
    }

    /// All per-stream `StereoMode`s (RFC 9559 §5.1.4.1.28.3), indexed by
    /// stream index. The slice has one entry per stream — `None` for
    /// non-video tracks and for video tracks whose `TrackEntry` carried no
    /// `Video` master. See [`MkvDemuxer::video_stereo_mode`] for the
    /// semantics.
    pub fn video_stereo_modes(&self) -> &[Option<StereoMode>] {
        &self.video_stereo_modes
    }

    /// `Projection` (RFC 9559 §5.1.4.1.28.41) for the stream at
    /// `stream_index`, or `None` when the track is not a video track / its
    /// `Video` master carried no `Projection` child.
    ///
    /// The returned [`Projection`] folds `ProjectionType` (§5.1.4.1.28.42 —
    /// `Rectangular` / `Equirectangular` / `Cubemap` / `Mesh` /
    /// `Other(u64)` for values registered after RFC 9559, §27.15),
    /// `ProjectionPrivate` (§5.1.4.1.28.43 — the verbatim ISOBMFF box body),
    /// and the three pose floats (§5.1.4.1.28.44..46) into a single typed
    /// record. Spec defaults are materialised on the typed surface: an empty
    /// `Projection` master decodes as a fully-typed identity projection
    /// (rectangular + zero pose), distinguishable from `None` (which means
    /// "no `Projection` master at all" — the common case for ordinary 2D
    /// video).
    ///
    /// The pose triple is in degrees; the spec ranges are
    /// `[-180.0, 180.0]` for yaw and roll and `[-90.0, 90.0]` for pitch.
    /// The §5.1.4.1.28.46 worked example
    /// `<Projection><ProjectionPoseRoll>90</ProjectionPoseRoll></Projection>`
    /// round-trips with `projection_type == Rectangular`, `pose_roll == 90.0`,
    /// and the other components at `0.0`.
    ///
    /// Returns `None` for an out-of-range `stream_index`.
    pub fn video_projection(&self, stream_index: u32) -> Option<&Projection> {
        self.video_projections
            .get(stream_index as usize)
            .and_then(|v| v.as_ref())
    }

    /// All per-stream `Projection`s (RFC 9559 §5.1.4.1.28.41), indexed by
    /// stream index. The slice has one entry per stream — `None` for
    /// non-video tracks and for video tracks whose `Video` master carried no
    /// `Projection` child. See [`MkvDemuxer::video_projection`] for the
    /// semantics.
    pub fn video_projections(&self) -> &[Option<Projection>] {
        &self.video_projections
    }

    /// `AlphaMode` (RFC 9559 §5.1.4.1.28.4) for the stream at
    /// `stream_index`, or `None` when the track is not a video track / its
    /// `TrackEntry` carried no `Video` master.
    ///
    /// The §5.1.4.1.28.4 default `0` ([`AlphaMode::None`]) is materialised:
    /// a `Video` master with no explicit `AlphaMode` decodes as
    /// `Some(AlphaMode::None)`, distinguishable from `None` (which means
    /// "no `Video` master at all"). Values outside Table 6 surface via
    /// [`AlphaMode::Other`] — §27.8 leaves the registry open for future
    /// additions, and the spec also notes that values other than `0`/`1`
    /// "SHOULD NOT be used" because implementations differ.
    ///
    /// Returns `None` for an out-of-range `stream_index`.
    pub fn video_alpha_mode(&self, stream_index: u32) -> Option<AlphaMode> {
        self.video_alpha_modes
            .get(stream_index as usize)
            .and_then(|v| *v)
    }

    /// All per-stream `AlphaMode`s (RFC 9559 §5.1.4.1.28.4), indexed by
    /// stream index. The slice has one entry per stream — `None` for
    /// non-video tracks and for video tracks whose `TrackEntry` carried no
    /// `Video` master. See [`MkvDemuxer::video_alpha_mode`] for the
    /// semantics.
    pub fn video_alpha_modes(&self) -> &[Option<AlphaMode>] {
        &self.video_alpha_modes
    }

    /// `AspectRatioType` (RFC 9559 Appendix A.24, reclaimed) for the stream
    /// at `stream_index`, or `None` when the file did not carry the
    /// element.
    ///
    /// The element is exposed as the raw `u64` rather than a typed enum:
    /// the reclaimed appendix says only "Specifies the possible
    /// modifications to the aspect ratio" and enumerates no values, so
    /// synthesising labels would be guesswork outside the spec. Returns
    /// `None` whenever the file did not carry the element (there is no
    /// spec default — the appendix does not specify one).
    ///
    /// Returns `None` for an out-of-range `stream_index`.
    pub fn video_aspect_ratio_type(&self, stream_index: u32) -> Option<u64> {
        self.video_aspect_ratio_types
            .get(stream_index as usize)
            .and_then(|v| *v)
    }

    /// All per-stream `AspectRatioType`s (RFC 9559 Appendix A.24), indexed
    /// by stream index. The slice has one entry per stream — `None` when
    /// the file did not carry the element. See
    /// [`MkvDemuxer::video_aspect_ratio_type`] for the semantics.
    pub fn video_aspect_ratio_types(&self) -> &[Option<u64>] {
        &self.video_aspect_ratio_types
    }

    /// `UncompressedFourCC` (RFC 9559 §5.1.4.1.28.15) for the stream at
    /// `stream_index`, or `None` when the file did not carry the element.
    ///
    /// The spec makes the element mandatory only when
    /// `CodecID == "V_UNCOMPRESSED"`; for any other codec the element is
    /// optional and most files omit it, in which case this returns `None`.
    /// The on-disk byte length is pinned to 4 by the schema; a malformed
    /// non-4-byte payload is preserved verbatim on the returned
    /// [`UncompressedFourCC`] so callers can debug the deviation, while
    /// [`UncompressedFourCC::fourcc`] / [`UncompressedFourCC::as_str`]
    /// return `None` for that case.
    ///
    /// Returns `None` for an out-of-range `stream_index`.
    pub fn video_uncompressed_fourcc(&self, stream_index: u32) -> Option<&UncompressedFourCC> {
        self.video_uncompressed_fourccs
            .get(stream_index as usize)
            .and_then(|v| v.as_ref())
    }

    /// All per-stream `UncompressedFourCC`s (RFC 9559 §5.1.4.1.28.15),
    /// indexed by stream index. The slice has one entry per stream —
    /// `None` when the file did not carry the element. See
    /// [`MkvDemuxer::video_uncompressed_fourcc`] for the semantics.
    pub fn video_uncompressed_fourccs(&self) -> &[Option<UncompressedFourCC>] {
        &self.video_uncompressed_fourccs
    }

    /// Apply a `CueRelativePosition` (RFC 9559 §5.1.5.1.2.3) after a
    /// `seek_to` has positioned the reader at a Cluster header.
    ///
    /// Opens the Cluster (reads its id+size), captures the cluster
    /// Timestamp (RFC 9559 §5.1.3.1 — SHOULD be the first child of the
    /// Cluster; we walk children until we find it, or until we reach
    /// `body_start.saturating_add(relative_position)`, whichever comes first), and then
    /// repositions the reader to `body_start.saturating_add(relative_position)` and
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
        let is_unknown_size = e.size == VINT_UNKNOWN_SIZE;
        let body_end = if is_unknown_size {
            self.segment_data_end
        } else {
            body_start.saturating_add(e.size)
        };
        let target = body_start.saturating_add(relative_position);
        if target > body_end {
            // Out-of-range relative position — degrade gracefully:
            // rewind to the cluster header so `advance()` can walk
            // from the start.
            self.input.seek(SeekFrom::Start(cluster_head_pos))?;
            return Ok(());
        }
        // Validate the Cluster's leading CRC-32 child if present (RFC
        // 8794 §11.3.1, RFC 9559 §6.2) before jumping to the cue
        // target. Same routine the legacy advance() path uses; dedup
        // by `body_start` means we only record once even if the
        // demuxer revisits the Cluster after a back-and-forth seek.
        // The helper leaves the reader at `body_start`, so the
        // Timestamp walk below sees the same bytes.
        self.validate_cluster_crc(body_start, body_end, is_unknown_size)?;
        self.input.seek(SeekFrom::Start(body_start))?;

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
            let next = child_body_start.saturating_add(child.size);
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

    /// Validate a `Cluster` element's leading `CRC-32` child (RFC 8794
    /// §11.3.1, RFC 9559 §6.2) and record the result on
    /// [`Self::crc_status`] if not already done for this Cluster.
    ///
    /// `body_start` is the absolute file offset of the Cluster's body
    /// (the byte right after the Cluster's id+size header). `body_end` is
    /// the absolute file offset of the byte right after the last child of
    /// the Cluster. The reader is left at `body_start` on return so the
    /// regular Cluster walk proceeds unchanged.
    ///
    /// Best-effort and *informational*:
    /// * A Cluster declared with unknown size (`body_end ==
    ///   self.segment_data_end`) can't be CRC-checked — RFC 8794 §11.3.1
    ///   requires a bounded body — so the check is skipped.
    /// * A non-bounded body, a truncated read, or any other I/O hiccup
    ///   silently degrades to "no status recorded"; the Cluster still
    ///   demuxes normally per RFC 8794 §12 ("a reader MAY ignore the
    ///   data").
    /// * The dedup set keyed on `body_start` guarantees the same Cluster
    ///   isn't recorded twice when a back-then-forward seek revisits it,
    ///   or when both `advance` and `apply_cue_relative_position` open
    ///   the same Cluster on the same `next_packet` call chain.
    fn validate_cluster_crc(
        &mut self,
        body_start: u64,
        body_end: u64,
        is_unknown_size: bool,
    ) -> Result<()> {
        if body_end <= body_start {
            return Ok(());
        }
        // An unknown-size Cluster body extends until a sibling Segment-
        // child shows up — we can't bound the body up front, so skip
        // (RFC 8794 §11.3.1 needs a known-size parent for the CRC).
        if is_unknown_size {
            return Ok(());
        }
        if self.validated_cluster_starts.contains(&body_start) {
            return Ok(());
        }
        let status =
            match validate_top_level_crc(&mut *self.input, ids::CLUSTER, body_start, body_end) {
                Ok(s) => s,
                Err(_) => {
                    // Make sure the reader is at body_start even on error.
                    self.input.seek(SeekFrom::Start(body_start))?;
                    return Ok(());
                }
            };
        self.validated_cluster_starts.insert(body_start);
        if let Some(s) = status {
            self.crc_status.push(s);
        }
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
                        let is_unknown_size = e.size == VINT_UNKNOWN_SIZE;
                        let body_end = if is_unknown_size {
                            self.segment_data_end
                        } else {
                            body_start.saturating_add(e.size)
                        };
                        // Validate the Cluster's leading CRC-32 child if
                        // present (RFC 8794 §11.3.1, RFC 9559 §6.2). The
                        // helper rewinds the reader to `body_start` so the
                        // child-element walk below sees the same bytes.
                        self.validate_cluster_crc(body_start, body_end, is_unknown_size)?;
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
                        let bg_end = self.input.stream_position()?.saturating_add(e.size);
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
    // Last frame size is whatever's left. Guard the subtraction against
    // a crafted lace where the encoded sizes already over-run the body
    // (debug-build subtract would panic; release would wrap to a huge
    // `last_size` that the per-frame bounds check below would then
    // turn into an error — but only after a length lookup that itself
    // could panic on a Vec growth attempt).
    let used: usize = sizes.iter().sum();
    let last_size = (body.len())
        .checked_sub(i)
        .and_then(|rem| rem.checked_sub(used))
        .ok_or_else(|| Error::invalid("MKV xiph lacing: sizes exceed body"))?;
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
    // `chunks_exact(0)` panics. A zero-length payload with n_frames >= 1
    // is a legitimate "frame size unknown / zero-byte frames" case from
    // a crafted Block — emit n_frames empty sub-frames rather than
    // dividing by zero on the chunker.
    if frame_size == 0 {
        return Ok(vec![Vec::new(); n_frames]);
    }
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
    // `n_frames` is `body[0] + 1` so it is at least 1; guard the
    // subtraction so a one-frame lace (which has no deltas to parse)
    // doesn't underflow the range below.
    let delta_count = n_frames.saturating_sub(2);
    for _ in 0..delta_count {
        let (raw, w) = crate::ebml::read_vint(&mut cur, false)?;
        let bias = ((1i64) << (7 * w as i64 - 1)) - 1;
        let signed = (raw as i64) - bias;
        let prev = *sizes.last().unwrap();
        let next = prev
            .checked_add(signed)
            .ok_or_else(|| Error::invalid("MKV ebml lacing: size addition overflow"))?;
        sizes.push(next);
    }
    // Last frame is whatever remains. The arithmetic happens in i64 so
    // an over-sized or contrived `sum` cannot wrap a usize; the per-
    // frame bounds check below rejects out-of-range values.
    let pos = cur.position() as usize;
    let used: i64 = sizes
        .iter()
        .try_fold(0i64, |acc, s| acc.checked_add(*s))
        .ok_or_else(|| Error::invalid("MKV ebml lacing: sizes overflow"))?;
    let last = (body.len() as i64)
        .checked_sub(pos as i64)
        .and_then(|rem| rem.checked_sub(used))
        .ok_or_else(|| Error::invalid("MKV ebml lacing: sizes exceed body"))?;
    sizes.push(last);
    let mut frames = Vec::with_capacity(n_frames);
    let mut i = pos;
    for s in sizes {
        if s < 0 {
            return Err(Error::invalid("MKV ebml lacing: negative frame size"));
        }
        let s_usize = usize::try_from(s)
            .map_err(|_| Error::invalid("MKV ebml lacing: frame size overflows usize"))?;
        let end = i
            .checked_add(s_usize)
            .ok_or_else(|| Error::invalid("MKV ebml lacing: frame offset overflows"))?;
        if end > body.len() {
            return Err(Error::invalid("MKV ebml lacing: invalid frame size"));
        }
        frames.push(body[i..end].to_vec());
        i = end;
    }
    Ok(frames)
}
