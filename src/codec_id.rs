//! Map between Matroska codec ID strings and oxideav [`CodecId`].
//!
//! Reference: <https://www.matroska.org/technical/codec_specs.html>

use oxideav_core::{CodecId, CodecParameters, CodecTag, Error, MediaType, Result};

/// Matroska CodecID strings that WebM permits.
///
/// WebM is a restricted subset of Matroska: the WebM specification
/// (<https://www.webmproject.org/docs/container/>) only allows VP8, VP9 and
/// AV1 for video, and Vorbis and Opus for audio. Anything else must be
/// rejected by a WebM muxer — writing e.g. H.264 into a DocType="webm"
/// file produces an invalid WebM even though the container bytes are valid
/// Matroska.
pub const ALLOWED_WEBM_CODECS: &[&str] = &[
    // Video.
    "V_VP8", "V_VP9", "V_AV1", // Audio.
    "A_VORBIS", "A_OPUS",
];

/// Return true if `matroska_codec_id` (e.g. `"A_OPUS"`) is permitted inside
/// a WebM container.
pub fn is_webm_matroska_codec(matroska_codec_id: &str) -> bool {
    ALLOWED_WEBM_CODECS.contains(&matroska_codec_id)
}

/// Return true if the oxideav-internal [`CodecId`] corresponds to a codec
/// that WebM permits. Unknown / unmapped codec ids return false.
pub fn is_webm_codec(id: &CodecId) -> bool {
    matches!(id.as_str(), "vp8" | "vp9" | "av1" | "vorbis" | "opus")
}

/// Best-effort mapping from a Matroska codec id string (e.g. `"A_FLAC"`) to
/// the oxideav codec id we use internally.
///
/// `codec_private` is consulted for `V_MS/VFW/FOURCC` tracks (the
/// BITMAPINFOHEADER's `biCompression` carries the actual codec) and for
/// `A_MS/ACM` tracks (the WAVEFORMATEX's `wFormatTag`). For other codec
/// ids it is ignored. This is the registry-less fallback; the demuxer
/// first offers those FourCCs / format tags to the codec registry.
pub fn from_matroska(s: &str, codec_private: &[u8]) -> CodecId {
    let id = match s {
        "A_FLAC" => "flac",
        "A_OPUS" => "opus",
        "A_VORBIS" => "vorbis",
        "A_PCM/INT/LIT" => "pcm_s16le",
        "A_PCM/INT/BIG" => "pcm_s16be",
        "A_PCM/FLOAT/IEEE" => "pcm_f32le",
        "A_AAC" => "aac",
        // The deprecated profile-specific AAC ids (Matroska codec
        // mappings) — all AAC; the profile feeds a synthesised
        // AudioSpecificConfig when CodecPrivate is absent.
        s if s.starts_with("A_AAC/MPEG2/") || s.starts_with("A_AAC/MPEG4/") => "aac",
        "A_MPEG/L3" => "mp3",
        "A_AC3" => "ac3",
        "A_EAC3" => "eac3",
        // DTS family — Matroska maps every DTS variant (Core, DTS-HD HRA,
        // DTS-HD MA, DTS:X) onto a single `A_DTS` CodecID. The bitstream
        // syntex itself distinguishes them; the container only carries the
        // wire bytes. See <https://www.matroska.org/technical/codec_specs.html>.
        // Blu-ray ships DTS-HD MA / HRA tracks via this CodecID with no
        // extradata required for passthrough.
        "A_DTS" => "dts",
        // Dolby TrueHD (BD primary lossless audio). Same passthrough model
        // as DTS: container carries the raw TrueHD frames, decoder picks
        // up the substream metadata from the bitstream.
        "A_TRUEHD" => "truehd",
        "V_VP8" => "vp8",
        "V_VP9" => "vp9",
        "V_AV1" => "av1",
        "V_MPEG1" => "mpeg1video",
        "V_MPEG2" => "mpeg2video",
        "V_MPEG4/ISO/AVC" => "h264",
        "V_MPEGH/ISO/HEVC" => "h265",
        "V_FFV1" => "ffv1",
        "V_THEORA" => "theora",
        // Files written by oxideav-mkv up to 0.0.11 carried every codec
        // without a native mapping as `X_<oxideav codec id>` (e.g.
        // `X_mjpeg`). Those ids are not registered Matroska CodecIDs, but
        // the suffix is the fleet codec id verbatim, so they still read
        // back. The muxer no longer writes them.
        s if s.len() > 2 && s.starts_with(LEGACY_PRIVATE_PREFIX) => {
            return CodecId::new(&s[LEGACY_PRIVATE_PREFIX.len()..]);
        }
        VFW_CODEC_ID => return from_bitmapinfoheader(codec_private),
        ACM_CODEC_ID => return from_waveformatex(codec_private),
        // Subtitles. Matroska's "S_TEXT/*" family carries plain UTF-8 with
        // per-format markup; "S_HDMV/PGS" carries Blu-ray bitmap subs;
        // "S_VOBSUB" carries DVD bitmap subs. We map them to short oxideav
        // codec ids so downstream decoders / muxers can recognise the
        // format without parsing the Matroska string themselves.
        "S_TEXT/UTF8" => "subrip",
        "S_TEXT/SSA" => "ssa",
        "S_TEXT/ASS" => "ass",
        "S_TEXT/WEBVTT" => "webvtt",
        "S_TEXT/USF" => "usf",
        "S_VOBSUB" => "dvd_subtitle",
        "S_HDMV/PGS" => "hdmv_pgs_subtitle",
        "S_HDMV/TEXTST" => "hdmv_text_subtitle",
        "S_DVBSUB" => "dvb_subtitle",
        "S_KATE" => "kate",
        other => return CodecId::new(format!("mkv:{other}")),
    };
    CodecId::new(id)
}

/// Prefix of the unregistered `X_<codec id>` CodecIDs earlier releases
/// of this muxer wrote for codecs without a native mapping.
const LEGACY_PRIVATE_PREFIX: &str = "X_";

/// Matroska CodecID of the VFW-compatibility video mode: CodecPrivate
/// carries a Win32 `BITMAPINFOHEADER` whose `biCompression` names the
/// codec.
pub const VFW_CODEC_ID: &str = "V_MS/VFW/FOURCC";

/// Matroska CodecID of the ACM-compatibility audio mode: CodecPrivate
/// carries a Win32 `WAVEFORMATEX` whose `wFormatTag` names the codec.
pub const ACM_CODEC_ID: &str = "A_MS/ACM";

/// Size of the bare `BITMAPINFOHEADER` (docs/container/vfw §Definition).
const BITMAPINFOHEADER_LEN: usize = 40;
/// Size of the bare `WAVEFORMATEX` (wFormatTag … cbSize).
const WAVEFORMATEX_LEN: usize = 18;
/// `WAVE_FORMAT_EXTENSIBLE` escape tag.
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// Bytes the `WAVEFORMATEXTENSIBLE` extension adds after `cbSize`
/// (`Samples` 2 + `dwChannelMask` 4 + `SubFormat` 16).
const EXTENSIBLE_EXT_LEN: usize = 22;
/// The shared tail (`Data2..Data4`) of every `DEFINE_WAVEFORMATEX_GUID`
/// SubFormat — `xxxxxxxx-0000-0010-8000-00aa00389b71`, stored
/// little-endian for `Data2` / `Data3` (docs/container/riff/
/// waveformatextensible/ms-converting-format-tags-and-subformat-guids.md).
const WAVEFORMATEX_GUID_TAIL: [u8; 12] = [
    0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];

/// The `biCompression` FourCC of a `V_MS/VFW/FOURCC` CodecPrivate
/// (`BITMAPINFOHEADER` bytes 16..20, stored as the four characters in
/// file order), or `None` when the blob is too short to hold it.
pub fn vfw_fourcc(codec_private: &[u8]) -> Option<[u8; 4]> {
    codec_private.get(16..20).map(|b| [b[0], b[1], b[2], b[3]])
}

/// The `WAVEFORMATEX` fields of an `A_MS/ACM` CodecPrivate.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct AcmFormat {
    /// `wFormatTag` as written (may be `0xFFFE`, WAVE_FORMAT_EXTENSIBLE).
    pub format_tag: u16,
    /// The tag that names the codec: `format_tag`, or — for an
    /// EXTENSIBLE header whose SubFormat is a `DEFINE_WAVEFORMATEX_GUID`
    /// GUID — the legacy tag in the GUID's first DWORD.
    pub effective_tag: u16,
    /// `nChannels`.
    pub channels: u16,
    /// `nSamplesPerSec`.
    pub samples_per_sec: u32,
    /// `nAvgBytesPerSec`.
    pub avg_bytes_per_sec: u32,
    /// `nBlockAlign`.
    pub block_align: u16,
    /// `wBitsPerSample` (the EXTENSIBLE `wValidBitsPerSample` when
    /// that is non-zero).
    pub bits_per_sample: u16,
    /// Codec-specific bytes after the header (after the 22-byte
    /// extension for EXTENSIBLE), bounded by `cbSize`.
    pub extradata: Vec<u8>,
}

impl AcmFormat {
    /// Parse a `WAVEFORMATEX` / `WAVEFORMATEXTENSIBLE` blob. A
    /// 16-byte `PCMWAVEFORMAT` (no `cbSize`) is accepted with empty
    /// extradata. Returns `None` below 16 bytes.
    pub fn parse(cp: &[u8]) -> Option<Self> {
        if cp.len() < 16 {
            return None;
        }
        let u16_at = |o: usize| u16::from_le_bytes([cp[o], cp[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes([cp[o], cp[o + 1], cp[o + 2], cp[o + 3]]);
        let format_tag = u16_at(0);
        let mut bits_per_sample = u16_at(14);
        let cb_size = if cp.len() >= WAVEFORMATEX_LEN {
            u16_at(16) as usize
        } else {
            0
        };
        let tail_end = (WAVEFORMATEX_LEN + cb_size).min(cp.len());
        let mut extra_start = WAVEFORMATEX_LEN.min(tail_end);
        let mut effective_tag = format_tag;
        if format_tag == WAVE_FORMAT_EXTENSIBLE
            && cb_size >= EXTENSIBLE_EXT_LEN
            && cp.len() >= WAVEFORMATEX_LEN + EXTENSIBLE_EXT_LEN
        {
            let valid = u16_at(18);
            if valid > 0 {
                bits_per_sample = valid;
            }
            let guid = &cp[24..40];
            if guid[4..16] == WAVEFORMATEX_GUID_TAIL && guid[2] == 0 && guid[3] == 0 {
                effective_tag = u16::from_le_bytes([guid[0], guid[1]]);
            }
            extra_start = WAVEFORMATEX_LEN + EXTENSIBLE_EXT_LEN;
        }
        Some(Self {
            format_tag,
            effective_tag,
            channels: u16_at(2),
            samples_per_sec: u32_at(4),
            avg_bytes_per_sec: u32_at(8),
            block_align: u16_at(12),
            bits_per_sample,
            extradata: cp[extra_start..tail_end.max(extra_start)].to_vec(),
        })
    }
}

/// Codec id for a `V_MS/VFW/FOURCC` track when no registered codec
/// claims its FourCC: the few ids the container itself owns, else the
/// stable placeholder `mkv:vfw:<FOURCC>` (`mkv:vfw:0xHHHHHHHH` when the
/// FourCC is not printable ASCII). Codec crates claim their FourCCs
/// through `CodecTag::fourcc`, which the demuxer consults first — the
/// same resolution the AVI demuxer runs on a `strf` BITMAPINFOHEADER.
fn from_bitmapinfoheader(cp: &[u8]) -> CodecId {
    let Some(fourcc) = vfw_fourcc(cp) else {
        return CodecId::new("mkv:vfw:<truncated>");
    };
    // The FourCCs the muxer itself writes through the compatibility
    // track (see [`VFW_FOURCCS`]) resolve without a codec registry, so a
    // file this crate wrote reads back under the same codec id.
    if let Some((id, _)) = VFW_FOURCCS
        .iter()
        .find(|(_, f)| f.eq_ignore_ascii_case(&fourcc))
    {
        return CodecId::new(*id);
    }
    match &fourcc {
        b"FFV1" => return CodecId::new("ffv1"),
        // BI_RGB (0): uncompressed 24-bit RGB is the one layout with a
        // conventional fleet id (mirrors the AVI demuxer).
        [0, 0, 0, 0] if cp.get(14..16) == Some(&24u16.to_le_bytes()[..]) => {
            return CodecId::new("rgb24");
        }
        _ => {}
    }
    if fourcc.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        CodecId::new(format!(
            "mkv:vfw:{}",
            std::str::from_utf8(&fourcc).unwrap_or("????")
        ))
    } else {
        CodecId::new(format!(
            "mkv:vfw:0x{:02X}{:02X}{:02X}{:02X}",
            fourcc[0], fourcc[1], fourcc[2], fourcc[3]
        ))
    }
}

/// Codec id for an `A_MS/ACM` track when no registered codec claims
/// its `wFormatTag`: the PCM / G.711 family the container can name
/// from the header alone (depth-aware, as the AVI demuxer does), else
/// the stable placeholder `mkv:acm:0xHHHH` (the effective tag).
fn from_waveformatex(cp: &[u8]) -> CodecId {
    let Some(f) = AcmFormat::parse(cp) else {
        return CodecId::new("mkv:acm:<truncated>");
    };
    CodecId::new(match (f.effective_tag, f.bits_per_sample) {
        (0x0001, 8) => "pcm_u8",
        (0x0001, 24) => "pcm_s24le",
        (0x0001, 32) => "pcm_s32le",
        (0x0001, _) => "pcm_s16le",
        (0x0003, 64) => "pcm_f64le",
        (0x0003, _) => "pcm_f32le",
        (0x0006, _) => "pcm_alaw",
        (0x0007, _) => "pcm_mulaw",
        (tag, _) => return CodecId::new(format!("mkv:acm:0x{tag:04X}")),
    })
}

/// If `codec_private` is a BITMAPINFOHEADER, return the inner codec-specific
/// extradata (everything after the 40-byte header). Otherwise returns the
/// slice unchanged.
pub fn strip_bitmapinfoheader(codec_id: &str, codec_private: &[u8]) -> Vec<u8> {
    if codec_id == VFW_CODEC_ID && codec_private.len() >= BITMAPINFOHEADER_LEN {
        codec_private[BITMAPINFOHEADER_LEN..].to_vec()
    } else {
        codec_private.to_vec()
    }
}

/// The codec's own configuration bytes from a CodecPrivate: the
/// `BITMAPINFOHEADER` (`V_MS/VFW/FOURCC`) or `WAVEFORMATEX` (`A_MS/ACM`)
/// wrapper is removed; any other CodecPrivate is returned unchanged.
pub fn strip_compat_header(codec_id: &str, codec_private: &[u8]) -> Vec<u8> {
    if codec_id == ACM_CODEC_ID {
        return AcmFormat::parse(codec_private)
            .map(|f| f.extradata)
            .unwrap_or_default();
    }
    strip_bitmapinfoheader(codec_id, codec_private)
}

/// Inverse of `from_matroska` for codecs we support writing. Returns `None`
/// for codecs without a Matroska mapping we know.
pub fn to_matroska(id: &CodecId) -> Option<&'static str> {
    Some(match id.as_str() {
        "flac" => "A_FLAC",
        "opus" => "A_OPUS",
        "vorbis" => "A_VORBIS",
        "pcm_s16le" => "A_PCM/INT/LIT",
        "pcm_s16be" => "A_PCM/INT/BIG",
        "pcm_f32le" => "A_PCM/FLOAT/IEEE",
        "aac" => "A_AAC",
        "mp3" => "A_MPEG/L3",
        "ac3" => "A_AC3",
        "eac3" => "A_EAC3",
        // DTS family — see notes on `A_DTS` in `from_matroska`. The Matroska
        // CodecID does not distinguish DTS-HD MA / HRA / Core: callers carry
        // those distinctions in the bitstream itself.
        "dts" => "A_DTS",
        // Dolby TrueHD — BD's primary lossless audio. Passthrough only;
        // muxer never re-encodes.
        "truehd" => "A_TRUEHD",
        "vp8" => "V_VP8",
        "vp9" => "V_VP9",
        "av1" => "V_AV1",
        "mpeg1video" => "V_MPEG1",
        "mpeg2video" => "V_MPEG2",
        "h264" => "V_MPEG4/ISO/AVC",
        "h265" => "V_MPEGH/ISO/HEVC",
        "ffv1" => "V_FFV1",
        "theora" => "V_THEORA",
        "subrip" => "S_TEXT/UTF8",
        "ssa" => "S_TEXT/SSA",
        "ass" => "S_TEXT/ASS",
        "webvtt" => "S_TEXT/WEBVTT",
        "usf" => "S_TEXT/USF",
        "dvd_subtitle" => "S_VOBSUB",
        "hdmv_pgs_subtitle" => "S_HDMV/PGS",
        "hdmv_text_subtitle" => "S_HDMV/TEXTST",
        "dvb_subtitle" => "S_DVBSUB",
        "kate" => "S_KATE",
        _ => return None,
    })
}

/// Fleet video codecs without a native Matroska CodecID that the muxer
/// carries through the VFW compatibility track (`V_MS/VFW/FOURCC`),
/// with the `biCompression` FourCC each is written under. Every FourCC
/// here is one docs/container/vfw names for that codec; the demuxer maps
/// them back without a registry.
pub const VFW_FOURCCS: &[(&str, [u8; 4])] = &[
    ("mjpeg", *b"MJPG"),
    ("cinepak", *b"CVID"),
    ("huffyuv", *b"HFYU"),
    ("lagarith", *b"LAGS"),
];

/// Fleet PCM / G.711 codecs carried through the ACM compatibility track
/// (`A_MS/ACM`): `(codec id, wFormatTag, wBitsPerSample)`. These are the
/// exact inverses of the depth-aware names the demuxer gives an
/// unclaimed WAVEFORMATEX.
const ACM_FORMATS: &[(&str, u16, u16)] = &[
    ("pcm_u8", 0x0001, 8),
    ("pcm_s24le", 0x0001, 24),
    ("pcm_s32le", 0x0001, 32),
    ("pcm_f64le", 0x0003, 64),
    ("pcm_alaw", 0x0006, 8),
    ("pcm_mulaw", 0x0007, 8),
];

/// How the muxer writes one track's `CodecID` / `CodecPrivate`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub enum TrackCodec {
    /// A Matroska CodecID with its own CodecPrivate rule; the muxer
    /// applies the codec-specific normalisation to the extradata.
    Native(&'static str),
    /// A CodecID string written verbatim with the extradata as
    /// CodecPrivate: an id the demuxer read but no codec claimed
    /// (`mkv:<CodecID>`), or one a stream announced through
    /// [`CodecTag::Matroska`].
    Verbatim(String),
    /// `V_MS/VFW/FOURCC` — CodecPrivate is a `BITMAPINFOHEADER` with
    /// this `biCompression` and `biBitCount`, followed by the extradata.
    Vfw {
        /// `biCompression`, in file byte order.
        fourcc: [u8; 4],
        /// `biBitCount`.
        bit_count: u16,
    },
    /// `A_MS/ACM` — CodecPrivate is a `WAVEFORMATEX` with this
    /// `wFormatTag` / `wBitsPerSample`, `cbSize` = extradata length,
    /// followed by the extradata.
    Acm {
        /// `wFormatTag`.
        format_tag: u16,
        /// `wBitsPerSample` (0 when the stream does not state it).
        bits_per_sample: u16,
        /// `true` for the PCM / G.711 tags, whose `nBlockAlign` and
        /// `nAvgBytesPerSec` follow from the geometry.
        pcm: bool,
    },
}

impl TrackCodec {
    /// The CodecID string this plan writes.
    pub fn codec_id(&self) -> &str {
        match self {
            TrackCodec::Native(s) => s,
            TrackCodec::Verbatim(s) => s,
            TrackCodec::Vfw { .. } => VFW_CODEC_ID,
            TrackCodec::Acm { .. } => ACM_CODEC_ID,
        }
    }
}

/// `true` when `s` can stand as a CodecID the muxer writes verbatim:
/// non-empty printable ASCII (Matroska CodecIDs are `string`, RFC 9559
/// §5.1.4.1.21, i.e. printable ASCII per RFC 8794 §7.4) and not one of
/// the placeholder names this crate generates for an unreadable header.
fn is_writable_codec_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| (0x20..=0x7E).contains(&b))
        && !s.contains('<')
        && !s.starts_with(LEGACY_PRIVATE_PREFIX)
}

/// Parse the FourCC out of a `mkv:vfw:<FOURCC>` / `mkv:vfw:0xHHHHHHHH`
/// placeholder id.
fn parse_vfw_placeholder(rest: &str) -> Option<[u8; 4]> {
    if let Some(hex) = rest.strip_prefix("0x") {
        if hex.len() != 8 {
            return None;
        }
        let v = u32::from_str_radix(hex, 16).ok()?;
        return Some(v.to_be_bytes());
    }
    let b = rest.as_bytes();
    (b.len() == 4).then(|| [b[0], b[1], b[2], b[3]])
}

/// Parse the tag out of a `mkv:acm:0xHHHH` placeholder id.
fn parse_acm_placeholder(rest: &str) -> Option<u16> {
    let hex = rest.strip_prefix("0x")?;
    (hex.len() == 4).then(|| u16::from_str_radix(hex, 16).ok())?
}

/// `wBitsPerSample` a stream states through its sample format.
fn stated_bits(params: &CodecParameters) -> u16 {
    params
        .sample_format
        .map(|f| (f.bytes_per_sample() * 8) as u16)
        .unwrap_or(0)
}

/// Decide the `CodecID` (and CodecPrivate shape) the muxer writes for a
/// stream, or refuse it.
///
/// In order:
/// 1. the native Matroska CodecID ([`to_matroska`]);
/// 2. a [`CodecTag::Matroska`] the stream carries, written verbatim;
/// 3. a demuxer placeholder — `mkv:vfw:<FOURCC>` → the VFW track,
///    `mkv:acm:0xHHHH` → the ACM track, `mkv:<CodecID>` → that CodecID
///    verbatim — so a remux of a track no codec claimed writes back the
///    id it was read under;
/// 4. the fleet codecs this crate carries through the compatibility
///    tracks ([`VFW_FOURCCS`], the PCM / G.711 depths);
/// 5. a video stream tagged with a non-zero FourCC → the VFW track; an
///    audio stream tagged with a `wFormatTag` → the ACM track.
///
/// Anything else is [`Error::Unsupported`]: the muxer never invents a
/// CodecID (earlier releases wrote an unregistered `X_<id>`), so a
/// caller learns at open time that this container cannot carry the
/// stream instead of producing a file no reader resolves.
pub fn plan_track_codec(params: &CodecParameters) -> Result<TrackCodec> {
    let id = params.codec_id.as_str();
    if let Some(native) = to_matroska(&params.codec_id) {
        return Ok(TrackCodec::Native(native));
    }
    let video = params.media_type == MediaType::Video;
    let audio = params.media_type == MediaType::Audio;
    if let Some(CodecTag::Matroska(s)) = &params.tag
        && is_writable_codec_id(s)
    {
        return Ok(TrackCodec::Verbatim(s.clone()));
    }
    if let Some(rest) = id.strip_prefix("mkv:vfw:") {
        if let (true, Some(fourcc)) = (video, parse_vfw_placeholder(rest))
            && fourcc != [0; 4]
        {
            return Ok(TrackCodec::Vfw {
                fourcc,
                bit_count: 24,
            });
        }
    } else if let Some(rest) = id.strip_prefix("mkv:acm:") {
        if let (true, Some(format_tag)) = (audio, parse_acm_placeholder(rest)) {
            return Ok(TrackCodec::Acm {
                format_tag,
                bits_per_sample: stated_bits(params),
                pcm: false,
            });
        }
    } else if let Some(rest) = id.strip_prefix("mkv:")
        && is_writable_codec_id(rest)
    {
        return Ok(TrackCodec::Verbatim(rest.to_owned()));
    }
    if video {
        if id == "rgb24" {
            return Ok(TrackCodec::Vfw {
                fourcc: [0; 4],
                bit_count: 24,
            });
        }
        if let Some((_, fourcc)) = VFW_FOURCCS.iter().find(|(c, _)| *c == id) {
            return Ok(TrackCodec::Vfw {
                fourcc: *fourcc,
                bit_count: 24,
            });
        }
        if let Some(CodecTag::Fourcc(fourcc)) = &params.tag
            && *fourcc != [0; 4]
        {
            return Ok(TrackCodec::Vfw {
                fourcc: *fourcc,
                bit_count: 24,
            });
        }
    }
    if audio {
        if let Some((_, format_tag, bits)) = ACM_FORMATS.iter().find(|(c, _, _)| *c == id) {
            return Ok(TrackCodec::Acm {
                format_tag: *format_tag,
                bits_per_sample: *bits,
                pcm: true,
            });
        }
        if let Some(CodecTag::WaveFormat(format_tag)) = &params.tag {
            return Ok(TrackCodec::Acm {
                format_tag: *format_tag,
                bits_per_sample: stated_bits(params),
                pcm: false,
            });
        }
    }
    Err(Error::unsupported(format!(
        "Matroska muxer: codec '{id}' has no registered Matroska CodecID and no \
         VFW FourCC / ACM wFormatTag to carry it under"
    )))
}

/// Build the `V_MS/VFW/FOURCC` CodecPrivate: a `BITMAPINFOHEADER`
/// (docs/container/vfw, §BITMAPINFOHEADER — 40 bytes, little-endian)
/// followed by the codec's extradata. `biSize` counts the header plus
/// that tail; `biHeight` is positive (required for compressed formats,
/// bottom-up for BI_RGB); `biSizeImage` is the DWORD-aligned
/// `stride * height` of a `biBitCount` picture.
pub fn bitmapinfoheader(fourcc: [u8; 4], bit_count: u16, params: &CodecParameters) -> Vec<u8> {
    let w = params.width.unwrap_or(0);
    let h = params.height.unwrap_or(0);
    let stride = ((u64::from(w) * u64::from(bit_count)).div_ceil(32) * 4) as u32;
    let size_image = stride.saturating_mul(h);
    let mut out = Vec::with_capacity(BITMAPINFOHEADER_LEN + params.extradata.len());
    out.extend_from_slice(&((BITMAPINFOHEADER_LEN + params.extradata.len()) as u32).to_le_bytes());
    out.extend_from_slice(&(w.min(i32::MAX as u32) as i32).to_le_bytes());
    out.extend_from_slice(&(h.min(i32::MAX as u32) as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&bit_count.to_le_bytes());
    out.extend_from_slice(&fourcc); // biCompression, FourCC chars in file order
    out.extend_from_slice(&size_image.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    out.extend_from_slice(&params.extradata);
    out
}

/// Build the `A_MS/ACM` CodecPrivate: a `WAVEFORMATEX` (wFormatTag,
/// nChannels, nSamplesPerSec, nAvgBytesPerSec, nBlockAlign,
/// wBitsPerSample, cbSize — 18 bytes little-endian) followed by
/// `cbSize` = extradata bytes. For PCM / G.711, `nBlockAlign` is one
/// sample frame and `nAvgBytesPerSec` the byte rate; otherwise
/// `nAvgBytesPerSec` is the stated bit rate / 8 and `nBlockAlign` 0
/// (not stated).
pub fn waveformatex(
    format_tag: u16,
    bits_per_sample: u16,
    pcm: bool,
    params: &CodecParameters,
) -> Result<Vec<u8>> {
    let cb_size = u16::try_from(params.extradata.len()).map_err(|_| {
        Error::invalid("Matroska muxer: A_MS/ACM extradata exceeds the 65535-byte cbSize")
    })?;
    let channels = params.channels.unwrap_or(0);
    let rate = params.sample_rate.unwrap_or(0);
    let (block_align, avg) = if pcm {
        let ba = (u32::from(channels) * u32::from(bits_per_sample)).div_ceil(8);
        (ba as u16, rate.saturating_mul(ba))
    } else {
        let avg = params
            .bit_rate
            .map(|b| (b / 8).min(u64::from(u32::MAX)) as u32)
            .unwrap_or(0);
        (0, avg)
    };
    let mut out = Vec::with_capacity(WAVEFORMATEX_LEN + params.extradata.len());
    out.extend_from_slice(&format_tag.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&avg.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());
    out.extend_from_slice(&cb_size.to_le_bytes());
    out.extend_from_slice(&params.extradata);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: round-trip a Matroska CodecID through
    /// [`from_matroska`] → [`to_matroska`] and assert the result is
    /// byte-identical to the original. Codec_private is empty (only
    /// `V_MS/VFW/FOURCC` consults it).
    fn assert_round_trip(matroska_id: &str) {
        let cid = from_matroska(matroska_id, &[]);
        let back = to_matroska(&cid).unwrap_or_else(|| {
            panic!(
                "no inverse mapping for {matroska_id} → oxideav id {}",
                cid.as_str()
            )
        });
        assert_eq!(
            back,
            matroska_id,
            "round-trip mismatch: {matroska_id} → {} → {back}",
            cid.as_str()
        );
    }

    /// Every Blu-ray-relevant Matroska CodecID must round-trip cleanly
    /// through the bi-directional table. Adding a new BD CodecID
    /// requires an entry in [`from_matroska`], a matching entry in
    /// [`to_matroska`], and a line here.
    #[test]
    fn bd_codec_ids_round_trip() {
        // Video.
        assert_round_trip("V_MPEG4/ISO/AVC");
        assert_round_trip("V_MPEGH/ISO/HEVC");
        // Audio.
        assert_round_trip("A_AC3");
        assert_round_trip("A_EAC3");
        assert_round_trip("A_DTS");
        assert_round_trip("A_TRUEHD");
        assert_round_trip("A_PCM/INT/BIG");
        // Subtitle.
        assert_round_trip("S_HDMV/PGS");
        assert_round_trip("S_HDMV/TEXTST");
    }

    /// `A_DTS` and `A_TRUEHD` were added for BD passthrough. Locked
    /// down so the oxideav-internal codec ids match the rest of the
    /// project's naming convention (lowercase, short canonical
    /// names).
    #[test]
    fn dts_and_truehd_map_to_expected_oxideav_ids() {
        assert_eq!(from_matroska("A_DTS", &[]).as_str(), "dts");
        assert_eq!(from_matroska("A_TRUEHD", &[]).as_str(), "truehd");
        assert_eq!(to_matroska(&CodecId::new("dts")), Some("A_DTS"));
        assert_eq!(to_matroska(&CodecId::new("truehd")), Some("A_TRUEHD"));
    }

    /// BD audio is big-endian LPCM (BD-ROM Part 3 §5.4). Make sure
    /// the muxer maps the existing `pcm_s16be` id through to
    /// `A_PCM/INT/BIG` and not the more common little-endian flavour.
    #[test]
    fn bd_lpcm_maps_to_big_endian_pcm() {
        assert_eq!(
            to_matroska(&CodecId::new("pcm_s16be")),
            Some("A_PCM/INT/BIG")
        );
        assert_eq!(from_matroska("A_PCM/INT/BIG", &[]).as_str(), "pcm_s16be");
    }
}
