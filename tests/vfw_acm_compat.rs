//! `V_MS/VFW/FOURCC` and `A_MS/ACM` compatibility tracks resolve
//! through the tunnelled Win32 header: the BITMAPINFOHEADER's
//! `biCompression` FourCC (docs/container/vfw/video-for-windows.md
//! §Definition / §biCompression) and the WAVEFORMATEX(TENSIBLE)
//! `wFormatTag` / SubFormat GUID (docs/container/riff/
//! waveformatextensible/). Codecs claimed in the registry win; an
//! unclaimed track keeps a stable `mkv:vfw:<FOURCC>` /
//! `mkv:acm:0xHHHH` id. CodecPrivate blobs here are crafted by hand.

use std::io::Cursor;

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecResolver, CodecTag, MediaType,
    NullCodecResolver, ReadSeek, SampleFormat,
};
use oxideav_mkv::codec_id::{AcmFormat, from_matroska};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

fn elem_uint(id: u32, value: u64) -> Vec<u8> {
    let n = if value == 0 {
        1
    } else {
        (64 - value.leading_zeros()).div_ceil(8) as usize
    };
    let mut out = write_element_id(id);
    out.extend_from_slice(&write_vint(n as u64, 0));
    for i in (0..n).rev() {
        out.push(((value >> (i * 8)) & 0xFF) as u8);
    }
    out
}

fn elem_bytes(id: u32, b: &[u8]) -> Vec<u8> {
    let mut out = write_element_id(id);
    out.extend_from_slice(&write_vint(b.len() as u64, 0));
    out.extend_from_slice(b);
    out
}

fn elem_f64(id: u32, v: f64) -> Vec<u8> {
    elem_bytes(id, &v.to_be_bytes())
}

/// A 40-byte BITMAPINFOHEADER + `extra` trailing codec bytes.
fn bitmapinfoheader(fourcc: &[u8; 4], bit_count: u16, extra: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(40 + extra.len() as u32).to_le_bytes()); // biSize
    b.extend_from_slice(&320i32.to_le_bytes()); // biWidth
    b.extend_from_slice(&180i32.to_le_bytes()); // biHeight
    b.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    b.extend_from_slice(&bit_count.to_le_bytes()); // biBitCount
    b.extend_from_slice(fourcc); // biCompression
    b.extend_from_slice(&[0u8; 20]); // biSizeImage .. biClrImportant
    b.extend_from_slice(extra);
    b
}

/// An 18-byte WAVEFORMATEX + `extra` (cbSize = extra.len()).
fn waveformatex(tag: u16, ch: u16, sr: u32, bits: u16, extra: &[u8]) -> Vec<u8> {
    let block_align = ch * bits.max(8) / 8;
    let mut b = Vec::new();
    b.extend_from_slice(&tag.to_le_bytes());
    b.extend_from_slice(&ch.to_le_bytes());
    b.extend_from_slice(&sr.to_le_bytes());
    b.extend_from_slice(&(sr * block_align as u32).to_le_bytes());
    b.extend_from_slice(&block_align.to_le_bytes());
    b.extend_from_slice(&bits.to_le_bytes());
    b.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    b.extend_from_slice(extra);
    b
}

/// WAVEFORMATEXTENSIBLE whose SubFormat is
/// `DEFINE_WAVEFORMATEX_GUID(sub_tag)`, followed by `codec` bytes.
fn waveformatextensible(sub_tag: u16, ch: u16, sr: u32, container: u16, valid: u16) -> Vec<u8> {
    let mut ext = Vec::new();
    ext.extend_from_slice(&valid.to_le_bytes()); // wValidBitsPerSample
    ext.extend_from_slice(&0x3u32.to_le_bytes()); // dwChannelMask FL|FR
    ext.extend_from_slice(&(sub_tag as u32).to_le_bytes()); // Data1
    ext.extend_from_slice(&[
        0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
    ]);
    waveformatex(0xFFFE, ch, sr, container, &ext)
}

fn build(track_type: u64, codec_id: &str, private: &[u8]) -> Vec<u8> {
    let mut ebml = Vec::new();
    ebml.extend(elem_uint(ids::EBML_VERSION, 1));
    ebml.extend(elem_uint(ids::EBML_READ_VERSION, 1));
    ebml.extend(elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    ebml.extend(elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    ebml.extend(elem_bytes(ids::EBML_DOC_TYPE, b"matroska"));
    ebml.extend(elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    ebml.extend(elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    let mut out = elem_bytes(ids::EBML_HEADER, &ebml);

    let info = elem_bytes(ids::INFO, &elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    let mut entry = Vec::new();
    entry.extend(elem_uint(ids::TRACK_NUMBER, 1));
    entry.extend(elem_uint(ids::TRACK_UID, 1));
    entry.extend(elem_uint(ids::TRACK_TYPE, track_type));
    entry.extend(elem_bytes(ids::CODEC_ID, codec_id.as_bytes()));
    entry.extend(elem_bytes(ids::CODEC_PRIVATE, private));
    if track_type == ids::TRACK_TYPE_VIDEO {
        let mut v = elem_uint(ids::PIXEL_WIDTH, 320);
        v.extend(elem_uint(ids::PIXEL_HEIGHT, 180));
        entry.extend(elem_bytes(ids::VIDEO, &v));
    } else {
        // An Audio element that leaves Channels / rate at their
        // defaults except the rate, so the WAVEFORMATEX fills gaps.
        entry.extend(elem_bytes(
            ids::AUDIO,
            &elem_f64(ids::SAMPLING_FREQUENCY, 48_000.0),
        ));
    }
    let tracks = elem_bytes(ids::TRACKS, &elem_bytes(ids::TRACK_ENTRY, &entry));
    let mut block = write_vint(1, 0);
    block.extend_from_slice(&[0, 0, 0x80, 0xAB]);
    let mut cluster = elem_uint(ids::TIMECODE, 0);
    cluster.extend(elem_bytes(ids::SIMPLE_BLOCK, &block));
    let mut seg = info;
    seg.extend(tracks);
    seg.extend(elem_bytes(ids::CLUSTER, &cluster));
    out.extend(elem_bytes(ids::SEGMENT, &seg));
    out
}

fn open_with(bytes: Vec<u8>, codecs: &dyn CodecResolver) -> oxideav_core::CodecParameters {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open(rs, codecs).expect("demux open");
    dmx.streams()[0].params.clone()
}

fn registry_claiming(id: &str, tag: CodecTag) -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(
        CodecInfo::new(CodecId::new(id))
            .capabilities(CodecCapabilities::video(format!("{id}_test")))
            .tags([tag]),
    );
    reg
}

#[test]
fn vfw_track_resolves_through_the_registry_fourcc_claim() {
    let cp = bitmapinfoheader(b"MP43", 24, &[]);
    let reg = registry_claiming("msmpeg4v3", CodecTag::fourcc(b"MP43"));
    let p = open_with(build(ids::TRACK_TYPE_VIDEO, "V_MS/VFW/FOURCC", &cp), &reg);
    assert_eq!(p.codec_id.as_str(), "msmpeg4v3");
    assert_eq!(p.media_type, MediaType::Video);
    assert_eq!(p.tag, Some(CodecTag::fourcc(b"MP43")));
    assert!(p.extradata.is_empty(), "the 40-byte header is stripped");
}

#[test]
fn unclaimed_vfw_fourcc_keeps_a_stable_placeholder() {
    // Corpus id 233: ffmpeg's wmv2 in Matroska.
    let cp = bitmapinfoheader(b"WMV2", 24, &[0x11, 0x22, 0x33, 0x44]);
    let p = open_with(
        build(ids::TRACK_TYPE_VIDEO, "V_MS/VFW/FOURCC", &cp),
        &NullCodecResolver,
    );
    assert_eq!(p.codec_id.as_str(), "mkv:vfw:WMV2");
    assert_eq!(p.extradata, vec![0x11, 0x22, 0x33, 0x44]);
    assert_eq!(p.tag, Some(CodecTag::fourcc(b"WMV2")));
    assert_eq!((p.width, p.height), (Some(320), Some(180)));
}

#[test]
fn vfw_static_fallbacks() {
    assert_eq!(
        from_matroska("V_MS/VFW/FOURCC", &bitmapinfoheader(b"FFV1", 24, &[])).as_str(),
        "ffv1"
    );
    assert_eq!(
        from_matroska("V_MS/VFW/FOURCC", &bitmapinfoheader(&[0; 4], 24, &[])).as_str(),
        "rgb24"
    );
    assert_eq!(
        from_matroska("V_MS/VFW/FOURCC", &bitmapinfoheader(&[0; 4], 32, &[])).as_str(),
        "mkv:vfw:0x00000000"
    );
    assert_eq!(
        from_matroska("V_MS/VFW/FOURCC", &[0u8; 12]).as_str(),
        "mkv:vfw:<truncated>"
    );
}

#[test]
fn acm_track_resolves_through_the_registry_format_tag_claim() {
    // WMA v2 (0x0161) with a 10-byte codec tail.
    let extra = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    let cp = waveformatex(0x0161, 2, 44_100, 16, &extra);
    let reg = registry_claiming("wmav2", CodecTag::wave_format(0x0161));
    let p = open_with(build(ids::TRACK_TYPE_AUDIO, "A_MS/ACM", &cp), &reg);
    assert_eq!(p.codec_id.as_str(), "wmav2");
    assert_eq!(p.media_type, MediaType::Audio);
    assert_eq!(p.extradata, extra.to_vec(), "cbSize bytes only");
    assert_eq!(p.tag, Some(CodecTag::wave_format(0x0161)));
    // Track says 48 kHz (the Audio element wins); channels came only
    // from the WAVEFORMATEX (the element was left at its default).
    assert_eq!(p.sample_rate, Some(48_000));
    assert_eq!(p.channels, Some(2));
    assert_eq!(p.bit_rate, Some(44_100 * 4 * 8));
}

#[test]
fn unclaimed_acm_tag_keeps_a_stable_placeholder() {
    let cp = waveformatex(0x0161, 2, 44_100, 16, &[]);
    let p = open_with(
        build(ids::TRACK_TYPE_AUDIO, "A_MS/ACM", &cp),
        &NullCodecResolver,
    );
    assert_eq!(p.codec_id.as_str(), "mkv:acm:0x0161");
}

#[test]
fn acm_pcm_family_maps_depth_aware() {
    let id = |cp: Vec<u8>| from_matroska("A_MS/ACM", &cp).as_str().to_string();
    assert_eq!(id(waveformatex(0x0001, 2, 48_000, 16, &[])), "pcm_s16le");
    assert_eq!(id(waveformatex(0x0001, 2, 48_000, 8, &[])), "pcm_u8");
    assert_eq!(id(waveformatex(0x0001, 2, 48_000, 24, &[])), "pcm_s24le");
    assert_eq!(id(waveformatex(0x0003, 2, 48_000, 32, &[])), "pcm_f32le");
    assert_eq!(id(waveformatex(0x0003, 2, 48_000, 64, &[])), "pcm_f64le");
    assert_eq!(id(waveformatex(0x0006, 1, 8_000, 8, &[])), "pcm_alaw");
    assert_eq!(id(waveformatex(0x0007, 1, 8_000, 8, &[])), "pcm_mulaw");
    // 16-byte PCMWAVEFORMAT (no cbSize) still parses.
    assert_eq!(
        id(waveformatex(0x0001, 2, 48_000, 16, &[])[..16].to_vec()),
        "pcm_s16le"
    );
    assert_eq!(id(vec![1, 0, 2]), "mkv:acm:<truncated>");
}

#[test]
fn acm_extensible_uses_the_subformat_legacy_tag() {
    // 24-in-32 PCM under WAVE_FORMAT_EXTENSIBLE.
    let cp = waveformatextensible(0x0001, 2, 96_000, 32, 24);
    let f = AcmFormat::parse(&cp).unwrap();
    assert_eq!((f.format_tag, f.effective_tag), (0xFFFE, 0x0001));
    assert_eq!(f.bits_per_sample, 24);
    assert!(
        f.extradata.is_empty(),
        "the 22-byte extension is not codec data"
    );
    let p = open_with(
        build(ids::TRACK_TYPE_AUDIO, "A_MS/ACM", &cp),
        &NullCodecResolver,
    );
    assert_eq!(p.codec_id.as_str(), "pcm_s24le");
    assert_eq!(p.sample_format, Some(SampleFormat::S24));
    assert_eq!(p.channels, Some(2), "channels filled from the WAVEFORMATEX");
    assert_eq!(p.tag, Some(CodecTag::wave_format(0xFFFE)));

    // A registered codec claiming the legacy tag wins over the fallback.
    let cp = waveformatextensible(0x0161, 2, 44_100, 16, 16);
    let reg = registry_claiming("wmav2", CodecTag::wave_format(0x0161));
    let p = open_with(build(ids::TRACK_TYPE_AUDIO, "A_MS/ACM", &cp), &reg);
    assert_eq!(p.codec_id.as_str(), "wmav2");

    // A non-DEFINE_WAVEFORMATEX_GUID SubFormat keeps the 0xFFFE tag.
    let mut cp = waveformatextensible(0x0001, 2, 48_000, 16, 16);
    cp[30] ^= 0xFF;
    assert_eq!(from_matroska("A_MS/ACM", &cp).as_str(), "mkv:acm:0xFFFE");
}
