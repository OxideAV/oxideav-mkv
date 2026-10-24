//! Map between Matroska codec ID strings and oxideav [`CodecId`].
//!
//! Reference: <https://www.matroska.org/technical/codec_specs.html>

use oxideav_core::CodecId;

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
#[derive(Clone, Debug, PartialEq, Eq)]
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
