//! Write-map round-trip matrix: every codec the Matroska muxer accepts
//! is written under a registered CodecID (RFC 9559 §5.1.4.1.21 — a
//! native mapping, the `V_MS/VFW/FOURCC` / `A_MS/ACM` compatibility
//! tracks, or a CodecID read from a Matroska file and written back
//! verbatim) and reads back through the demuxer with the same codec id,
//! stream parameters, CodecPrivate-derived extradata, packet bytes,
//! timestamps and keyframe flags. Codecs with no registered CodecID are
//! refused at open time; files earlier releases wrote under the
//! unregistered `X_<codec id>` still read back.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, CodecTag, Error, MediaType, NullCodecResolver, Packet, SampleFormat,
    StreamInfo, TimeBase,
};
use oxideav_mkv::codec_id::{TrackCodec, plan_track_codec};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Cursor<Vec<u8>>>>);

impl std::io::Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for Sink {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

/// One row of the matrix.
struct Case {
    /// Codec id handed to the muxer.
    codec: &'static str,
    kind: MediaType,
    /// CodecID string the file must carry.
    matroska: &'static str,
    /// Codec id the demuxer must report (`None` = same as `codec`).
    reads_as: Option<&'static str>,
    extradata: Vec<u8>,
    tag: Option<CodecTag>,
    sample_format: Option<SampleFormat>,
}

fn case(codec: &'static str, kind: MediaType, matroska: &'static str) -> Case {
    Case {
        codec,
        kind,
        matroska,
        reads_as: None,
        extradata: Vec::new(),
        tag: None,
        sample_format: None,
    }
}

impl Case {
    fn extra(mut self, e: &[u8]) -> Self {
        self.extradata = e.to_vec();
        self
    }
    fn tag(mut self, t: CodecTag) -> Self {
        self.tag = Some(t);
        self
    }
    fn reads_as(mut self, id: &'static str) -> Self {
        self.reads_as = Some(id);
        self
    }
    fn fmt(mut self, f: SampleFormat) -> Self {
        self.sample_format = Some(f);
        self
    }

    fn params(&self) -> CodecParameters {
        let id = CodecId::new(self.codec);
        let mut p = match self.kind {
            MediaType::Audio => {
                let mut p = CodecParameters::audio(id);
                p.sample_rate = Some(48_000);
                p.channels = Some(2);
                p
            }
            MediaType::Video => {
                let mut p = CodecParameters::video(id);
                p.width = Some(320);
                p.height = Some(240);
                p
            }
            _ => CodecParameters::subtitle(id),
        };
        p.extradata = self.extradata.clone();
        p.tag = self.tag.clone();
        p.sample_format = self.sample_format;
        p
    }
}

/// Opus identification header: version 1, 2 channels, pre-skip 312,
/// 48 kHz input, gain 0, mapping family 0.
fn opus_head() -> Vec<u8> {
    let mut h = b"OpusHead".to_vec();
    h.push(1);
    h.push(2);
    h.extend_from_slice(&312u16.to_le_bytes());
    h.extend_from_slice(&48_000u32.to_le_bytes());
    h.extend_from_slice(&0i16.to_le_bytes());
    h.push(0);
    h
}

fn matrix() -> Vec<Case> {
    use MediaType::{Audio, Subtitle, Video};
    vec![
        // Native audio.
        case("flac", Audio, "A_FLAC")
            .extra(&[0x80, 0, 0, 34, 0x10, 0x00, 0x10, 0x00])
            .fmt(SampleFormat::S16),
        case("opus", Audio, "A_OPUS").extra(&opus_head()),
        case("vorbis", Audio, "A_VORBIS").extra(&[2, 30, 7, 1, b'v', b'o']),
        case("pcm_s16le", Audio, "A_PCM/INT/LIT").fmt(SampleFormat::S16),
        case("pcm_s16be", Audio, "A_PCM/INT/BIG").fmt(SampleFormat::S16),
        case("pcm_f32le", Audio, "A_PCM/FLOAT/IEEE").fmt(SampleFormat::F32),
        // AAC-LC, 48 kHz, stereo AudioSpecificConfig.
        case("aac", Audio, "A_AAC").extra(&[0x11, 0x90]),
        case("mp3", Audio, "A_MPEG/L3"),
        case("ac3", Audio, "A_AC3"),
        case("eac3", Audio, "A_EAC3"),
        case("dts", Audio, "A_DTS"),
        case("truehd", Audio, "A_TRUEHD"),
        // Native video.
        case("vp8", Video, "V_VP8"),
        case("vp9", Video, "V_VP9"),
        case("av1", Video, "V_AV1"),
        case("mpeg1video", Video, "V_MPEG1"),
        case("mpeg2video", Video, "V_MPEG2"),
        case("h264", Video, "V_MPEG4/ISO/AVC").extra(&[1, 0x64, 0, 0x1F, 0xFF, 0xE0, 0]),
        case("h265", Video, "V_MPEGH/ISO/HEVC").extra(&[1, 2, 3, 4]),
        case("ffv1", Video, "V_FFV1").extra(&[3, 0, 0, 0]),
        case("theora", Video, "V_THEORA").extra(&[2, 42, 9, 0x80, b't', b'h']),
        // Native subtitles.
        case("subrip", Subtitle, "S_TEXT/UTF8"),
        case("ssa", Subtitle, "S_TEXT/SSA").extra(b"[Script Info]\n"),
        case("ass", Subtitle, "S_TEXT/ASS").extra(b"[Script Info]\n"),
        case("webvtt", Subtitle, "S_TEXT/WEBVTT").extra(b"WEBVTT\n"),
        case("usf", Subtitle, "S_TEXT/USF"),
        case("dvd_subtitle", Subtitle, "S_VOBSUB").extra(b"size: 720x480\n"),
        case("hdmv_pgs_subtitle", Subtitle, "S_HDMV/PGS"),
        case("hdmv_text_subtitle", Subtitle, "S_HDMV/TEXTST"),
        case("dvb_subtitle", Subtitle, "S_DVBSUB").extra(&[0, 1, 0, 2]),
        case("kate", Subtitle, "S_KATE").extra(&[1, 2, 3]),
        // VFW compatibility track (docs/container/vfw FourCCs).
        case("mjpeg", Video, "V_MS/VFW/FOURCC").tag(CodecTag::fourcc(b"MJPG")),
        case("cinepak", Video, "V_MS/VFW/FOURCC").tag(CodecTag::fourcc(b"CVID")),
        case("huffyuv", Video, "V_MS/VFW/FOURCC")
            .extra(&[0x02, 0x18, 0x00, 0x00])
            .tag(CodecTag::fourcc(b"HFYU")),
        case("lagarith", Video, "V_MS/VFW/FOURCC").tag(CodecTag::fourcc(b"LAGS")),
        case("rgb24", Video, "V_MS/VFW/FOURCC").tag(CodecTag::fourcc(&[0; 4])),
        // A stream tagged with a FourCC no fleet codec owns: the
        // registry-less demuxer names it by placeholder, keeps the tag,
        // and a remux of that placeholder writes the same FourCC.
        case("vfwonly", Video, "V_MS/VFW/FOURCC")
            .tag(CodecTag::fourcc(b"ZZ9A"))
            .extra(&[9, 8, 7])
            .reads_as("mkv:vfw:ZZ9A"),
        case("mkv:vfw:ZZ9A", Video, "V_MS/VFW/FOURCC")
            .tag(CodecTag::fourcc(b"ZZ9A"))
            .extra(&[9, 8, 7]),
        // ACM compatibility track.
        case("pcm_u8", Audio, "A_MS/ACM")
            .fmt(SampleFormat::U8)
            .tag(CodecTag::wave_format(0x0001)),
        case("pcm_s24le", Audio, "A_MS/ACM")
            .fmt(SampleFormat::S24)
            .tag(CodecTag::wave_format(0x0001)),
        case("pcm_s32le", Audio, "A_MS/ACM")
            .fmt(SampleFormat::S32)
            .tag(CodecTag::wave_format(0x0001)),
        case("pcm_f64le", Audio, "A_MS/ACM")
            .fmt(SampleFormat::F64)
            .tag(CodecTag::wave_format(0x0003)),
        case("pcm_alaw", Audio, "A_MS/ACM").tag(CodecTag::wave_format(0x0006)),
        case("pcm_mulaw", Audio, "A_MS/ACM").tag(CodecTag::wave_format(0x0007)),
        case("acmonly", Audio, "A_MS/ACM")
            .tag(CodecTag::wave_format(0x0031))
            .extra(&[0x40, 0x01])
            .reads_as("mkv:acm:0x0031"),
        case("mkv:acm:0x0031", Audio, "A_MS/ACM")
            .tag(CodecTag::wave_format(0x0031))
            .extra(&[0x40, 0x01]),
        // CodecIDs carried verbatim: one a stream announces through
        // its Matroska tag, and the placeholder the demuxer gives a
        // CodecID no codec claims.
        case("tagged", Video, "V_QUICKTIME")
            .tag(CodecTag::matroska("V_QUICKTIME"))
            .extra(&[0, 0, 0, 8])
            .reads_as("mkv:V_QUICKTIME"),
        case("mkv:V_QUICKTIME", Video, "V_QUICKTIME").extra(&[0, 0, 0, 8]),
        case("mkv:A_MPEG/L2", Audio, "A_MPEG/L2"),
    ]
}

fn packets(c: &Case) -> Vec<Packet> {
    (0..5i64)
        .map(|i| {
            // Payloads that are not ADTS (no 0xFFF sync), opus TOC
            // config 31 (20 ms CELT FB, one frame).
            let mut data: Vec<u8> = (0..(12 + i as usize))
                .map(|j| (0x21 + i as usize * 3 + j) as u8)
                .collect();
            if c.codec == "opus" {
                data[0] = 0xF8;
            }
            Packet::new(0, TimeBase::new(1, 1000), data)
                .with_pts(i * 20)
                .with_dts(i * 20)
                .with_duration(20)
                .with_keyframe(c.kind != MediaType::Video || i % 2 == 0)
        })
        .collect()
}

fn mux(params: CodecParameters, pkts: &[Packet]) -> Vec<u8> {
    let s = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params,
    };
    let sink = Sink::default();
    let mut m = oxideav_mkv::mux::open(Box::new(sink.clone()), std::slice::from_ref(&s)).unwrap();
    m.write_header().unwrap();
    for p in pkts {
        m.write_packet(p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    sink.0.lock().unwrap().get_ref().clone()
}

/// The `CodecID` (0x86) element's string, located by its id + 1-byte
/// size header in the Tracks master.
fn written_codec_id(bytes: &[u8]) -> String {
    let tracks = bytes
        .windows(4)
        .position(|w| w == [0x16, 0x54, 0xAE, 0x6B])
        .expect("Tracks master");
    let mut i = tracks;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x86 && bytes[i + 1] & 0x80 != 0 {
            let n = (bytes[i + 1] & 0x7F) as usize;
            if let Some(s) = bytes.get(i + 2..i + 2 + n)
                && let Ok(s) = std::str::from_utf8(s)
                && s.bytes().all(|b| (0x20..=0x7E).contains(&b))
                && !s.is_empty()
            {
                return s.to_owned();
            }
        }
        i += 1;
    }
    panic!("no CodecID element");
}

#[test]
fn every_written_codec_reads_back_under_its_registered_codec_id() {
    let rows = matrix();
    let mut checked = 0;
    for c in &rows {
        let params = c.params();
        let pkts = packets(c);
        let bytes = mux(params.clone(), &pkts);
        assert_eq!(written_codec_id(&bytes), c.matroska, "{}: CodecID", c.codec);

        let mut d = oxideav_mkv::demux::open(Box::new(Cursor::new(bytes)), &NullCodecResolver)
            .unwrap_or_else(|e| panic!("{}: demux open: {e}", c.codec));
        let got = d.streams()[0].params.clone();
        let want_id = c.reads_as.unwrap_or(c.codec);
        assert_eq!(got.codec_id.as_str(), want_id, "{}: codec id", c.codec);
        assert_eq!(got.media_type, c.kind, "{}: media type", c.codec);
        assert_eq!(got.extradata, params.extradata, "{}: extradata", c.codec);
        match c.kind {
            MediaType::Audio => {
                assert_eq!(got.sample_rate, Some(48_000), "{}: rate", c.codec);
                assert_eq!(got.channels, Some(2), "{}: channels", c.codec);
                if let Some(f) = c.sample_format {
                    assert_eq!(got.sample_format, Some(f), "{}: sample format", c.codec);
                }
            }
            MediaType::Video => {
                assert_eq!(got.width, Some(320), "{}: width", c.codec);
                assert_eq!(got.height, Some(240), "{}: height", c.codec);
            }
            _ => {}
        }
        if c.matroska == "V_MS/VFW/FOURCC" || c.matroska == "A_MS/ACM" {
            assert_eq!(got.tag, c.tag, "{}: compatibility tag", c.codec);
        }

        let mut n = 0;
        while let Ok(p) = d.next_packet() {
            let w = &pkts[n];
            assert_eq!(p.data, w.data, "{}: packet {n} bytes", c.codec);
            let tb = p.time_base;
            let ms = |v: i64| tb.rescale(v, TimeBase::new(1, 1000));
            assert_eq!(p.pts.map(ms), w.pts, "{}: packet {n} pts", c.codec);
            if c.kind == MediaType::Video {
                assert_eq!(p.flags.keyframe, w.flags.keyframe, "{}: key {n}", c.codec);
            }
            n += 1;
        }
        assert_eq!(n, pkts.len(), "{}: packet count", c.codec);

        // A remux of what was read writes the same CodecID again.
        let again = mux(got, &pkts);
        assert_eq!(written_codec_id(&again), c.matroska, "{}: remux", c.codec);
        checked += 1;
    }
    assert_eq!(checked, rows.len());
    assert!(checked >= 49, "matrix shrank to {checked} rows");
}

#[test]
fn plan_names_the_compatibility_header_fields() {
    let mut p = CodecParameters::video(CodecId::new("mjpeg"));
    p.width = Some(320);
    p.height = Some(240);
    let plan = plan_track_codec(&p).unwrap();
    assert_eq!(
        plan,
        TrackCodec::Vfw {
            fourcc: *b"MJPG",
            bit_count: 24
        }
    );
    let bih = oxideav_mkv::codec_id::bitmapinfoheader(*b"MJPG", 24, &p);
    assert_eq!(bih.len(), 40);
    assert_eq!(&bih[0..4], &40u32.to_le_bytes()); // biSize
    assert_eq!(&bih[4..8], &320i32.to_le_bytes());
    assert_eq!(&bih[8..12], &240i32.to_le_bytes());
    assert_eq!(&bih[12..14], &1u16.to_le_bytes()); // biPlanes
    assert_eq!(&bih[14..16], &24u16.to_le_bytes());
    assert_eq!(&bih[16..20], b"MJPG");
    assert_eq!(&bih[20..24], &(320u32 * 3 * 240).to_le_bytes());

    let mut a = CodecParameters::audio(CodecId::new("pcm_s24le"));
    a.sample_rate = Some(48_000);
    a.channels = Some(2);
    let plan = plan_track_codec(&a).unwrap();
    let TrackCodec::Acm {
        format_tag,
        bits_per_sample,
        pcm,
    } = plan
    else {
        panic!("{plan:?}")
    };
    let wfx = oxideav_mkv::codec_id::waveformatex(format_tag, bits_per_sample, pcm, &a).unwrap();
    assert_eq!(
        wfx,
        [
            &1u16.to_le_bytes()[..],
            &2u16.to_le_bytes(),
            &48_000u32.to_le_bytes(),
            &(48_000u32 * 6).to_le_bytes(),
            &6u16.to_le_bytes(),
            &24u16.to_le_bytes(),
            &0u16.to_le_bytes(),
        ]
        .concat()
    );
}

#[test]
fn codecs_without_a_registered_codec_id_are_refused_at_open() {
    let refuse = |p: CodecParameters| {
        let s = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 1000),
            duration: None,
            start_time: Some(0),
            params: p,
        };
        match oxideav_mkv::mux::open(Box::new(Sink::default()), &[s]) {
            Err(Error::Unsupported(msg)) => msg,
            Err(e) => panic!("expected Unsupported, got {e}"),
            Ok(_) => panic!("expected a refusal"),
        }
    };
    // No native id, no FourCC.
    let msg = refuse(CodecParameters::video(CodecId::new("png")));
    assert!(msg.contains("png") && msg.contains("stream 0"), "{msg}");
    // A FourCC tag on an audio stream does not make it carriable.
    let mut p = CodecParameters::audio(CodecId::new("gsm"));
    p.tag = Some(CodecTag::fourcc(b"GSM "));
    refuse(p);
    // A wFormatTag on a video stream neither.
    let mut p = CodecParameters::video(CodecId::new("odd"));
    p.tag = Some(CodecTag::wave_format(0x0031));
    refuse(p);
    // BI_RGB at a depth the stream cannot state.
    let mut p = CodecParameters::video(CodecId::new("mkv:vfw:0x00000000"));
    p.tag = Some(CodecTag::fourcc(&[0; 4]));
    refuse(p);
    // Placeholders for an unreadable header are not CodecIDs.
    refuse(CodecParameters::video(CodecId::new("mkv:vfw:<truncated>")));
    refuse(CodecParameters::audio(CodecId::new("mkv:acm:<truncated>")));
    // The legacy private prefix is never written again.
    refuse(CodecParameters::video(CodecId::new("mkv:X_mjpeg")));
    // Non-printable CodecID strings are not EBML `string`s.
    let mut p = CodecParameters::video(CodecId::new("weird"));
    p.tag = Some(CodecTag::matroska("V_\u{e9}"));
    refuse(p);
}

#[test]
fn legacy_private_x_codec_ids_still_read_back() {
    // Earlier releases wrote `X_<codec id>` for unmapped codecs.
    assert_eq!(
        oxideav_mkv::codec_id::from_matroska("X_mjpeg", &[]).as_str(),
        "mjpeg"
    );
    assert_eq!(
        oxideav_mkv::codec_id::from_matroska("X_png", &[]).as_str(),
        "png"
    );
    // A bare `X_` is not one of ours.
    assert_eq!(
        oxideav_mkv::codec_id::from_matroska("X_", &[]).as_str(),
        "mkv:X_"
    );
}

#[test]
fn webm_whitelist_still_applies_before_compatibility_tracks() {
    let s = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: CodecParameters::video(CodecId::new("mjpeg")),
    };
    assert!(matches!(
        oxideav_mkv::mux::open_webm(Box::new(Sink::default()), &[s]),
        Err(Error::Unsupported(_))
    ));
}
