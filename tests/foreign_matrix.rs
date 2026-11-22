//! Live black-box interoperability matrix.
//!
//! **Foreign → ours.** A black-box muxer (`ffmpeg`) writes Matroska and
//! WebM files across every codec it will put in the container and the
//! layouts its options reach (Cues at the end / front / reserved,
//! Cluster size and time limits, live and piped output, CRC-32 on and
//! off, lacing-free and BlockGroup tails, CodecDelay / DiscardPadding,
//! start offsets, B-frame reordering, VFR, 1-frame and long files,
//! alpha in BlockAdditional, colour / aspect / stereo elements, several
//! tracks with flags and languages, chapters, attachments, text
//! subtitles). Each file is read with this crate's demuxer and with a
//! black-box reader (`ffprobe`), and compared per track: packet count,
//! payload size and CRC-32, timestamp, key-frame flag, duration, codec
//! id, dimensions, sample rate, channel count, language and the
//! CodecPrivate-derived extradata.
//!
//! **Ours → foreign.** Every file the reader can read is demuxed and
//! written back by this crate's muxer in each writer mode (file with
//! the index at the end, index at the front, live, short Clusters by
//! time and by size, laced audio, trait-level copy, and the WebM
//! profile for WebM sources). The black-box reader must report the same
//! packets and stream parameters for the copy as for the original,
//! with nothing on its warning channel, and a second reader (`mpv`,
//! when installed) must play the file-mode copies and exit cleanly.
//!
//! Everything is generated into a temporary directory; nothing is
//! committed. The test skips (passes, saying so) when `ffmpeg` /
//! `ffprobe` are not installed, and individual rows skip when the
//! local build of the tool lacks the encoder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use oxideav_core::{Demuxer, Error, Muxer, NullCodecResolver, ReadSeek, StreamInfo};
use oxideav_mkv::demux::open_typed;
use oxideav_mkv::mux::{LacingMode, MkvMuxer};

// ---------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------

/// The reader marks every frame of this codec as a key frame whatever
/// the Block flags say; key flags are checked against nothing.
const READER_KEYS: u32 = 1 << 0;
/// The reader does not report this file's streams (it cannot identify
/// the codec): only this crate's own reading is checked.
const READER_BLIND: u32 = 1 << 1;
/// Timestamps before zero: the reader reports them as unknown or
/// shifted; this crate's are checked against the Block timestamps the
/// row states instead.
const NEGATIVE_START: u32 = 1 << 2;
/// The reader's channel count comes from its decoder, not the track.
const DECODER_CHANNELS: u32 = 1 << 3;
/// Subtitle-only, or a codec the second reader cannot decode: it is not
/// asked to play the copy.
const NO_PLAYER: u32 = 1 << 4;
/// The reader's extradata for this codec is synthesised from other
/// elements and differs between two reads of the same file.
const READER_EXTRADATA: u32 = 1 << 5;
/// The reader hands out something other than the Block payload (it
/// rebuilds a codec frame around it, or drops lines of it); payload
/// bytes are checked against what the row states instead.
const READER_PAYLOAD: u32 = 1 << 6;

struct Row {
    name: &'static str,
    /// Arguments between `ffmpeg -y` and the output path.
    args: Vec<String>,
    /// `mkv`, `webm`, or `pipe` (Matroska written to a pipe).
    ext: &'static str,
    /// Codec id this crate reports per stream, without a codec registry.
    codecs: &'static [&'static str],
    flags: u32,
}

fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_owned).collect()
}

const VIDEO: &str = "-f lavfi -i testsrc2=s=176x144:r=25:d=2";
const AUDIO: &str = "-f lavfi -i sine=f=440:d=2:r=48000";
const STEREO: &str = "-f lavfi -i aevalsrc=sin(440*2*PI*t)|sin(660*2*PI*t):d=2:s=48000";
const NOISE: &str = "-f lavfi -i anoisesrc=d=1:r=48000";

fn rows(dir: &Path) -> Vec<Row> {
    let mut out = Vec::new();
    let mut row = |name: &'static str,
                   args: String,
                   ext: &'static str,
                   codecs: &'static [&'static str],
                   flags: u32| {
        out.push(Row {
            name,
            args: words(&args),
            ext,
            codecs,
            flags,
        });
    };

    // --- every video codec the muxer accepts --------------------------
    let video: &[(&'static str, &str, &'static str, u32)] = &[
        ("v_h264", "-c:v libx264 -bf 2 -g 12", "h264", 0),
        ("v_h264_nob", "-c:v libx264 -bf 0 -g 12", "h264", 0),
        (
            "v_hevc",
            "-c:v libx265 -x265-params log-level=error -g 12",
            "h265",
            0,
        ),
        (
            "v_av1",
            "-c:v libsvtav1 -g 12 -svtav1-params lp=1",
            "av1",
            0,
        ),
        ("v_vp8", "-c:v libvpx -g 12", "vp8", 0),
        ("v_vp9", "-c:v libvpx-vp9 -g 12", "vp9", 0),
        ("v_mpeg4", "-c:v mpeg4 -bf 2", "mpeg4video", 0),
        // The reader's extradata for MPEG-1/2 is the sequence header it
        // finds in the first packet; the track has no CodecPrivate.
        (
            "v_mpeg1",
            "-c:v mpeg1video -bf 2",
            "mpeg1video",
            READER_EXTRADATA,
        ),
        (
            "v_mpeg2",
            "-c:v mpeg2video -bf 2",
            "mpeg2video",
            READER_EXTRADATA,
        ),
        ("v_mjpeg", "-c:v mjpeg", "mjpeg", 0),
        ("v_ffv1", "-c:v ffv1", "ffv1", 0),
        ("v_ffv1_l3", "-c:v ffv1 -level 3 -g 5", "ffv1", 0),
        ("v_prores", "-c:v prores_ks", "prores", 0),
        ("v_huffyuv", "-c:v huffyuv", "huffyuv", 0),
        ("v_ffvhuff", "-c:v ffvhuff", "mkv:vfw:FFVH", 0),
        ("v_utvideo", "-c:v utvideo", "mkv:vfw:ULY0", 0),
        ("v_magicyuv", "-c:v magicyuv", "mkv:vfw:M8Y0", 0),
        ("v_cinepak", "-c:v cinepak", "mkv:V_QUICKTIME", 0),
        ("v_msmpeg4", "-c:v msmpeg4", "msmpeg4v3", 0),
        ("v_msmpeg4v2", "-c:v msmpeg4v2", "mkv:vfw:MP42", 0),
        ("v_wmv1", "-c:v wmv1", "mkv:vfw:WMV1", 0),
        ("v_wmv2", "-c:v wmv2", "mkv:vfw:WMV2", 0),
        ("v_h263p", "-c:v h263p", "mkv:vfw:H263", READER_KEYS),
        ("v_h261", "-c:v h261", "mkv:vfw:H261", READER_KEYS),
        ("v_flv1", "-c:v flv", "mkv:vfw:FLV1", 0),
        (
            "v_dv",
            "-c:v dvvideo -s 720x576 -pix_fmt yuv420p",
            "mkv:vfw:dvsd",
            0,
        ),
        (
            "v_dnxhd",
            "-c:v dnxhd -s 1280x720 -b:v 60M -pix_fmt yuv422p",
            "mkv:V_QUICKTIME",
            0,
        ),
        ("v_jpeg2000", "-c:v jpeg2000", "mkv:vfw:mjp2", 0),
        ("v_png", "-c:v png", "mkv:vfw:MPNG", 0),
        ("v_vc2", "-c:v vc2 -strict -2", "dirac", READER_KEYS),
        ("v_snow", "-c:v snow -strict -2", "snow", 0),
        (
            "v_svq1",
            "-c:v svq1",
            "mkv:V_QUICKTIME",
            READER_BLIND | NO_PLAYER,
        ),
        (
            "v_qtrle",
            "-c:v qtrle",
            "mkv:V_QUICKTIME",
            READER_BLIND | NO_PLAYER,
        ),
        ("v_zlib", "-c:v zlib", "mkv:vfw:ZLIB", 0),
        (
            "v_raw_yuv420p",
            "-c:v rawvideo -pix_fmt yuv420p",
            "rawvideo",
            0,
        ),
        (
            "v_raw_uyvy",
            "-c:v rawvideo -pix_fmt uyvy422",
            "rawvideo",
            0,
        ),
        (
            "v_raw_bgr24_vfw",
            "-c:v rawvideo -pix_fmt bgr24 -allow_raw_vfw 1",
            "rgb24",
            0,
        ),
        ("v_v210", "-c:v v210", "mkv:vfw:v210", 0),
    ];
    for (name, enc, codec, flags) in video {
        let codecs: &'static [&'static str] = Box::leak(Box::new([*codec]));
        row(name, format!("{VIDEO} {enc}"), "mkv", codecs, *flags);
    }

    // --- every audio codec the muxer accepts --------------------------
    let audio: &[(&'static str, &str, &'static str, u32)] = &[
        ("a_aac", "-c:a aac", "aac", 0),
        (
            "a_aac_he",
            "-c:a aac_at -profile:a 4",
            "aac",
            DECODER_CHANNELS,
        ),
        ("a_mp3", "-c:a libmp3lame", "mp3", 0),
        ("a_mp2", "-c:a mp2", "mp2", 0),
        ("a_ac3", "-c:a ac3", "ac3", 0),
        ("a_eac3", "-c:a eac3", "eac3", 0),
        ("a_dts", "-c:a dca -strict -2", "dts", 0),
        ("a_opus", "-c:a libopus", "opus", 0),
        ("a_vorbis", "-c:a vorbis -strict -2 -ac 2", "vorbis", 0),
        ("a_flac", "-c:a flac", "flac", 0),
        ("a_flac24", "-c:a flac -sample_fmt s32", "flac", 0),
        ("a_alac", "-c:a alac", "alac", 0),
        (
            "a_tta",
            "-c:a tta",
            "mkv:A_TTA1",
            READER_EXTRADATA | NO_PLAYER,
        ),
        (
            "a_wavpack",
            "-c:a wavpack",
            "mkv:A_WAVPACK4",
            READER_PAYLOAD,
        ),
        ("a_truehd", "-c:a truehd -strict -2 -ac 2", "truehd", 0),
        ("a_mlp", "-c:a mlp -strict -2 -ac 2", "mlp", 0),
        ("a_pcm_u8", "-c:a pcm_u8", "pcm_u8", 0),
        ("a_pcm_s16le", "-c:a pcm_s16le", "pcm_s16le", 0),
        ("a_pcm_s24le", "-c:a pcm_s24le", "pcm_s24le", 0),
        ("a_pcm_s32le", "-c:a pcm_s32le", "pcm_s32le", 0),
        ("a_pcm_s16be", "-c:a pcm_s16be", "pcm_s16be", 0),
        ("a_pcm_s24be", "-c:a pcm_s24be", "pcm_s24be", 0),
        ("a_pcm_s32be", "-c:a pcm_s32be", "pcm_s32be", 0),
        ("a_pcm_f32le", "-c:a pcm_f32le", "pcm_f32le", 0),
        ("a_pcm_f64le", "-c:a pcm_f64le", "pcm_f64le", 0),
        // The reader counts the 22-byte EXTENSIBLE extension of the
        // WAVEFORMATEX as extradata; this crate parses it.
        (
            "a_pcm_s64le",
            "-c:a pcm_s64le",
            "pcm_s64le",
            READER_EXTRADATA,
        ),
        ("a_pcm_alaw", "-c:a pcm_alaw", "pcm_alaw", 0),
        ("a_pcm_mulaw", "-c:a pcm_mulaw", "pcm_mulaw", 0),
        ("a_adpcm_ms", "-c:a adpcm_ms", "mkv:acm:0x0002", 0),
        ("a_adpcm_ima", "-c:a adpcm_ima_wav", "mkv:acm:0x0011", 0),
        ("a_wmav2", "-c:a wmav2", "mkv:acm:0x0161", 0),
        ("a_g722", "-c:a g722 -ar 16000", "mkv:acm:0x028F", 0),
        (
            "a_real144",
            "-c:a real_144 -ar 8000",
            "mkv:A_REAL/14_4",
            NO_PLAYER,
        ),
    ];
    for (name, enc, codec, flags) in audio {
        let codecs: &'static [&'static str] = Box::leak(Box::new([*codec]));
        row(name, format!("{AUDIO} {enc}"), "mkv", codecs, *flags);
    }
    row(
        "a_aac_51",
        format!("{NOISE} -ac 6 -c:a aac"),
        "mkv",
        &["aac"],
        0,
    );
    row(
        "a_ac3_51",
        format!("{NOISE} -ac 6 -c:a ac3"),
        "mkv",
        &["ac3"],
        0,
    );
    row(
        "a_opus_51",
        format!("{NOISE} -ac 6 -c:a libopus"),
        "mkv",
        &["opus"],
        0,
    );
    row(
        "a_flac_8ch",
        format!("{NOISE} -ac 8 -c:a flac"),
        "mkv",
        &["flac"],
        0,
    );
    row(
        "a_aac_44k",
        "-f lavfi -i sine=d=1:r=44100 -c:a aac".into(),
        "mkv",
        &["aac"],
        0,
    );
    row(
        "a_mp3_22k",
        "-f lavfi -i sine=d=1:r=22050 -c:a libmp3lame".into(),
        "mkv",
        &["mp3"],
        0,
    );
    row(
        "a_flac_96k",
        "-f lavfi -i sine=d=1:r=96000 -c:a flac".into(),
        "mkv",
        &["flac"],
        0,
    );
    row(
        "a_pcm_192k",
        "-f lavfi -i sine=d=1:r=192000 -c:a pcm_s24le".into(),
        "mkv",
        &["pcm_s24le"],
        0,
    );

    // --- WebM ---------------------------------------------------------
    row(
        "w_vp8_vorbis",
        format!("{VIDEO} {STEREO} -c:v libvpx -c:a vorbis -strict -2"),
        "webm",
        &["vp8", "vorbis"],
        0,
    );
    row(
        "w_vp9_opus",
        format!("{VIDEO} {AUDIO} -c:v libvpx-vp9 -c:a libopus"),
        "webm",
        &["vp9", "opus"],
        0,
    );
    row(
        "w_av1_opus",
        format!("{VIDEO} {AUDIO} -c:v libsvtav1 -c:a libopus"),
        "webm",
        &["av1", "opus"],
        0,
    );
    row(
        "w_vp9_live",
        format!("{VIDEO} {AUDIO} -c:v libvpx-vp9 -c:a libopus -live 1"),
        "webm",
        &["vp9", "opus"],
        0,
    );
    row(
        "w_vp9_dash",
        format!("{VIDEO} -c:v libvpx-vp9 -dash 1"),
        "webm",
        &["vp9"],
        0,
    );
    let alpha = "-f lavfi -i testsrc2=s=176x144:r=25:d=1,format=yuva420p";
    row(
        "w_vp9_alpha",
        format!("{alpha} -c:v libvpx-vp9 -auto-alt-ref 0"),
        "webm",
        &["vp9"],
        0,
    );
    row(
        "w_vp8_alpha",
        format!("{alpha} -c:v libvpx -auto-alt-ref 0"),
        "webm",
        &["vp8"],
        0,
    );

    // --- layouts ------------------------------------------------------
    let base = format!("{VIDEO} {AUDIO} -c:v libx264 -bf 2 -g 12 -c:a aac");
    let av: &'static [&'static str] = &["h264", "aac"];
    row("l_default", base.clone(), "mkv", av, 0);
    row(
        "l_cues_front",
        format!("{base} -cues_to_front 1"),
        "mkv",
        av,
        0,
    );
    row(
        "l_reserve_index",
        format!("{base} -reserve_index_space 4096"),
        "mkv",
        av,
        0,
    );
    row("l_live", format!("{base} -live 1"), "mkv", av, 0);
    row("l_nocrc", format!("{base} -write_crc32 0"), "mkv", av, 0);
    row(
        "l_csize_1024",
        format!("{base} -cluster_size_limit 1024"),
        "mkv",
        av,
        0,
    );
    row(
        "l_csize_4096",
        format!("{base} -cluster_size_limit 4096"),
        "mkv",
        av,
        0,
    );
    row(
        "l_csize_65536",
        format!("{base} -cluster_size_limit 65536"),
        "mkv",
        av,
        0,
    );
    row(
        "l_ctime_1",
        format!("{base} -cluster_time_limit 1"),
        "mkv",
        av,
        0,
    );
    row(
        "l_ctime_40",
        format!("{base} -cluster_time_limit 40"),
        "mkv",
        av,
        0,
    );
    row(
        "l_ctime_200",
        format!("{base} -cluster_time_limit 200"),
        "mkv",
        av,
        0,
    );
    row(
        "l_ctime_5000",
        format!("{base} -cluster_time_limit 5000"),
        "mkv",
        av,
        0,
    );
    row("l_pipe", format!("{base} -f matroska"), "pipe", av, 0);
    row(
        "l_pipe_live",
        format!("{base} -live 1 -f matroska"),
        "pipe",
        av,
        0,
    );
    let offset = format!("{VIDEO} {AUDIO} -c:v libx264 -bf 2 -c:a aac -avoid_negative_ts disabled");
    row(
        "l_start_neg",
        format!("{offset} -output_ts_offset -0.5"),
        "mkv",
        av,
        NEGATIVE_START,
    );
    row(
        "l_start_10s",
        format!("{offset} -output_ts_offset 10"),
        "mkv",
        av,
        0,
    );
    row(
        "l_start_1h",
        format!("{offset} -output_ts_offset 3600"),
        "mkv",
        av,
        0,
    );
    row(
        "l_vfr",
        "-f lavfi -i testsrc2=s=176x144:r=25:d=2,select=not(mod(n\\,3))+lt(n\\,5) \
         -fps_mode vfr -c:v libx264"
            .into(),
        "mkv",
        &["h264"],
        0,
    );
    row(
        "l_1frame",
        "-f lavfi -i testsrc2=s=176x144:r=25 -frames:v 1 -c:v libx264".into(),
        "mkv",
        &["h264"],
        0,
    );
    row(
        "l_1frame_a",
        "-f lavfi -i sine=r=48000 -frames:a 1 -c:a libopus".into(),
        "mkv",
        &["opus"],
        0,
    );
    row(
        "l_long",
        "-f lavfi -i testsrc2=s=64x48:r=50:d=120 -f lavfi -i sine=d=120:r=8000 \
         -c:v libx264 -preset ultrafast -c:a pcm_s16le"
            .into(),
        "mkv",
        &["h264", "pcm_s16le"],
        0,
    );
    row(
        "l_ntsc",
        "-f lavfi -i testsrc2=s=176x144:r=30000/1001:d=2 -c:v libx264".into(),
        "mkv",
        &["h264"],
        0,
    );
    row(
        "l_fps60",
        "-f lavfi -i testsrc2=s=176x144:r=60:d=1 -c:v libx264".into(),
        "mkv",
        &["h264"],
        0,
    );
    row(
        "l_multi",
        format!(
            "{VIDEO} {AUDIO} {STEREO} -map 0 -map 1 -map 2 -map 0 -c:v libx264 -c:a aac \
             -metadata:s:a:0 language=fra -metadata:s:a:1 language=jpn \
             -metadata:s:a:1 title=Second -disposition:a:1 default -disposition:a:0 0 \
             -disposition:v:1 forced -metadata title=Matrix -metadata:s:v:0 title=Main"
        ),
        "mkv",
        &["h264", "aac", "aac", "h264"],
        0,
    );
    row(
        "l_flags",
        format!(
            "{VIDEO} {AUDIO} -c:v libx264 -c:a aac \
             -disposition:a:0 hearing_impaired+visual_impaired+original+comment \
             -disposition:v:0 0 -default_mode passthrough"
        ),
        "mkv",
        av,
        0,
    );
    row(
        "l_colour",
        format!(
            "{VIDEO} -c:v libx264 -color_primaries bt2020 -color_trc smpte2084 \
             -colorspace bt2020nc -color_range tv -chroma_sample_location left"
        ),
        "mkv",
        &["h264"],
        0,
    );
    row(
        "l_sar",
        format!("{VIDEO} -c:v libx264 -vf setsar=16/11"),
        "mkv",
        &["h264"],
        0,
    );
    row(
        "l_interlaced",
        format!("{VIDEO} -c:v mpeg2video -flags +ildct+ilme -vf setfield=tff"),
        "mkv",
        &["mpeg2video"],
        READER_EXTRADATA,
    );
    row(
        "l_stereo",
        format!("{VIDEO} -c:v libx264 -metadata:s:v:0 stereo_mode=left_right"),
        "mkv",
        &["h264"],
        0,
    );
    row(
        "l_opus_trim",
        "-f lavfi -i sine=d=0.513:r=48000 -c:a libopus".into(),
        "mkv",
        &["opus"],
        0,
    );
    row(
        "l_vorbis_ctime",
        format!("{STEREO} -c:a vorbis -strict -2 -cluster_time_limit 100"),
        "mkv",
        &["vorbis"],
        0,
    );
    row(
        "l_hevc10",
        format!("{VIDEO} -c:v libx265 -x265-params log-level=error -pix_fmt yuv420p10le"),
        "mkv",
        &["h265"],
        0,
    );
    row(
        "l_odd",
        "-f lavfi -i testsrc2=s=33x17:r=25:d=1 -c:v ffv1".into(),
        "mkv",
        &["ffv1"],
        0,
    );
    row(
        "l_big",
        "-f lavfi -i testsrc2=s=1920x1080:r=25:d=0.4 -c:v mjpeg -q:v 2".into(),
        "mkv",
        &["mjpeg"],
        0,
    );
    let chapters = dir.join("chapters.txt");
    let attachment = dir.join("attachment.txt");
    let srt = dir.join("subtitle.srt");
    row(
        "l_chapters",
        format!(
            "{VIDEO} -f ffmetadata -i {} -map_metadata 1 -map_chapters 1 -c:v libx264",
            chapters.display()
        ),
        "mkv",
        &["h264"],
        0,
    );
    row(
        "l_attach",
        format!(
            "{VIDEO} -attach {} -metadata:s:t mimetype=text/plain -c:v libx264",
            attachment.display()
        ),
        "mkv",
        &["h264"],
        0,
    );

    // --- subtitles ----------------------------------------------------
    let sub = format!("-i {}", srt.display());
    row(
        "s_srt",
        format!("{sub} -c:s srt"),
        "mkv",
        &["subrip"],
        NO_PLAYER,
    );
    row(
        "s_ass",
        format!("{sub} -c:s ass"),
        "mkv",
        &["ass"],
        NO_PLAYER,
    );
    row(
        "s_ssa",
        format!("{sub} -c:s ssa"),
        "mkv",
        &["ass"],
        NO_PLAYER,
    );
    row(
        "s_text",
        format!("{sub} -c:s text"),
        "mkv",
        &["subrip"],
        NO_PLAYER,
    );
    row(
        "s_webvtt",
        format!("{sub} -c:s webvtt"),
        "mkv",
        &["webvtt"],
        NO_PLAYER | READER_PAYLOAD,
    );
    row(
        "s_webvtt_webm",
        format!("{sub} -c:s webvtt"),
        "webm",
        &["webvtt"],
        NO_PLAYER | READER_PAYLOAD,
    );
    row(
        "s_srt_video",
        format!(
            "{VIDEO} {sub} -c:v libx264 -c:s srt -metadata:s:s:0 language=eng \
             -disposition:s:0 forced"
        ),
        "mkv",
        &["h264", "subrip"],
        0,
    );
    out
}

fn write_inputs(dir: &Path) {
    std::fs::write(
        dir.join("chapters.txt"),
        ";FFMETADATA1\ntitle=Chaptered\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=1000\n\
         title=One\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=1000\nEND=2000\ntitle=Two\n",
    )
    .unwrap();
    std::fs::write(dir.join("attachment.txt"), "attachment payload\n").unwrap();
    std::fs::write(
        dir.join("subtitle.srt"),
        "1\n00:00:00,500 --> 00:00:01,200\nHello <i>world</i>\n\n\
         2\n00:00:01,500 --> 00:00:02,750\nSecond line\nwith break\n\n\
         3\n00:00:03,000 --> 00:00:03,010\nShort\n",
    )
    .unwrap();
}

// ---------------------------------------------------------------------
// The two readers
// ---------------------------------------------------------------------

/// ISO 3309 CRC-32, the checksum the black-box reader prints.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (!(crc & 1)).wrapping_add(1));
        }
    }
    !crc
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Pkt {
    pts: Option<i64>,
    duration: Option<i64>,
    key: bool,
    size: usize,
    crc: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Track {
    kind: String,
    codec: String,
    width: Option<u32>,
    height: Option<u32>,
    sample_rate: Option<u32>,
    channels: Option<u32>,
    language: String,
    extradata_len: usize,
    extradata_crc: u32,
    packets: Vec<Pkt>,
}

/// What `ffprobe` says about a file: media tracks in file order, and
/// whatever it printed on its warning channel.
fn probe(path: &Path) -> (Vec<Track>, String) {
    let out = Command::new("ffprobe")
        .args(["-v", "warning", "-show_entries"])
        .arg(
            "packet=stream_index,pts,duration,size,flags,data_hash:\
             stream=index,codec_name,codec_type,width,height,sample_rate,channels,\
             extradata_size,extradata_hash:stream_tags=language",
        )
        .args(["-show_data_hash", "crc32", "-of", "compact"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    let mut tracks: BTreeMap<u32, Track> = BTreeMap::new();
    let mut packets: Vec<(u32, Pkt)> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut fields = line.split('|');
        let section = fields.next().unwrap_or("");
        let kv: BTreeMap<&str, &str> = fields.filter_map(|f| f.split_once('=')).collect();
        let num = |k: &str| kv.get(k).and_then(|v| v.parse::<i64>().ok());
        let hash = |k: &str| {
            kv.get(k)
                .and_then(|v| v.strip_prefix("CRC32:"))
                .and_then(|v| u32::from_str_radix(v, 16).ok())
        };
        match section {
            "packet" => {
                let Some(index) = num("stream_index") else {
                    continue;
                };
                packets.push((
                    index as u32,
                    Pkt {
                        pts: num("pts"),
                        duration: num("duration"),
                        key: kv.get("flags").is_some_and(|f| f.contains('K')),
                        size: num("size").unwrap_or(0) as usize,
                        crc: hash("data_hash").unwrap_or(0),
                    },
                ));
            }
            "stream" => {
                let kind = kv.get("codec_type").copied().unwrap_or("");
                if !matches!(kind, "video" | "audio" | "subtitle") {
                    continue;
                }
                let Some(index) = num("index") else { continue };
                tracks.insert(
                    index as u32,
                    Track {
                        kind: kind.to_owned(),
                        codec: kv.get("codec_name").copied().unwrap_or("").to_owned(),
                        width: num("width").map(|v| v as u32),
                        height: num("height").map(|v| v as u32),
                        sample_rate: num("sample_rate").map(|v| v as u32),
                        channels: num("channels").map(|v| v as u32),
                        language: kv.get("tag:language").copied().unwrap_or("und").to_owned(),
                        extradata_len: num("extradata_size").unwrap_or(0) as usize,
                        extradata_crc: hash("extradata_hash").unwrap_or(0),
                        packets: Vec::new(),
                    },
                );
            }
            _ => {}
        }
    }
    for (index, pkt) in packets {
        if let Some(t) = tracks.get_mut(&index) {
            t.packets.push(pkt);
        }
    }
    // Without the object address the tool prefixes its lines with, so
    // two runs can be compared.
    let warnings = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| match (l.find(" @ 0x"), l.find("] ")) {
            (Some(a), Some(b)) if a < b => format!("{}{}", &l[..a], &l[b..]),
            _ => l.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    (tracks.into_values().collect(), warnings)
}

/// What this crate's demuxer says about a file, plus each stream's
/// extradata.
fn read_ours(bytes: Vec<u8>) -> Result<(Vec<Track>, Vec<Vec<u8>>), String> {
    read_ours_with(bytes, |_| Vec::new()).map(|(tracks, extradata, _)| (tracks, extradata))
}

/// Tracks, each stream's extradata, and the notes of an inspection.
type Reading = (Vec<Track>, Vec<Vec<u8>>, Vec<String>);

/// [`read_ours`], plus whatever `inspect` reports about the opened
/// demuxer.
fn read_ours_with(
    bytes: Vec<u8>,
    inspect: impl FnOnce(&mut oxideav_mkv::demux::MkvDemuxer) -> Vec<String>,
) -> Result<Reading, String> {
    let reader: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes));
    let mut dmx = open_typed(reader, &NullCodecResolver).map_err(|e| format!("open: {e}"))?;
    let notes = inspect(&mut dmx);
    let mut tracks: Vec<Track> = dmx
        .streams()
        .iter()
        .map(|s| Track {
            kind: format!("{:?}", s.params.media_type).to_lowercase(),
            codec: s.params.codec_id.as_str().to_owned(),
            width: s.params.width,
            height: s.params.height,
            sample_rate: s.params.sample_rate,
            channels: s.params.channels.map(u32::from),
            language: s.params.language.clone().unwrap_or_else(|| "und".into()),
            extradata_len: s.params.extradata.len(),
            extradata_crc: crc32(&s.params.extradata),
            packets: Vec::new(),
        })
        .collect();
    let extradata = dmx
        .streams()
        .iter()
        .map(|s| s.params.extradata.clone())
        .collect();
    loop {
        match dmx.next_packet() {
            Ok(p) => tracks[p.stream_index as usize].packets.push(Pkt {
                pts: p.pts,
                duration: p.duration,
                key: p.flags.keyframe,
                size: p.data.len(),
                crc: crc32(&p.data),
            }),
            Err(Error::Eof) => break,
            Err(e) => return Err(format!("next_packet: {e}")),
        }
    }
    Ok((tracks, extradata, notes))
}

/// The chapters and attachments the two container rows carry, as this
/// crate reads them: an empty list when they are what the row wrote.
fn check_records(name: &str, dmx: &mut oxideav_mkv::demux::MkvDemuxer) -> Vec<String> {
    let mut issues = Vec::new();
    match name {
        "l_chapters" => {
            let got: Vec<(u64, Option<u64>, String)> = dmx
                .chapters()
                .first()
                .map(|e| {
                    e.chapters
                        .iter()
                        .map(|c| {
                            (
                                c.time_start_ns,
                                c.time_end_ns,
                                c.displays
                                    .first()
                                    .map(|d| d.string.clone())
                                    .unwrap_or_default(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let want = vec![
                (0, Some(1_000_000_000), "One".to_owned()),
                (1_000_000_000, Some(2_000_000_000), "Two".to_owned()),
            ];
            if got != want {
                issues.push(format!("chapters {got:?}"));
            }
            let title = dmx
                .metadata()
                .iter()
                .find(|(k, _)| k == "title")
                .map(|(_, v)| v.as_str());
            if title != Some("Chaptered") {
                issues.push(format!("title {title:?}"));
            }
        }
        "l_attach" => {
            let list: Vec<(u32, String, String)> = dmx
                .attachments()
                .iter()
                .map(|a| (a.index, a.filename.clone(), a.mime_type.clone()))
                .collect();
            match list.as_slice() {
                [(index, name, mime)] if name == "attachment.txt" && mime == "text/plain" => {
                    if dmx.attachment_data(*index).ok().as_deref()
                        != Some(&b"attachment payload\n"[..])
                    {
                        issues.push("attachment bytes differ".into());
                    }
                }
                _ => issues.push(format!("attachments {list:?}")),
            }
        }
        _ => {}
    }
    issues
}

// ---------------------------------------------------------------------
// Foreign → ours
// ---------------------------------------------------------------------

/// The reader's extradata for a codec, as a function of the
/// CodecPrivate-derived bytes this crate hands out.
fn reader_extradata(codec: &str, ours: &[u8]) -> Vec<u8> {
    match codec {
        // This crate keeps the METADATA_BLOCK_HEADER in front of the
        // STREAMINFO; the reader drops it.
        "flac" if ours.len() >= 4 => ours[4..].to_vec(),
        // docs/audio/alac: the reader wraps the 24-byte
        // ALACSpecificConfig in its 'alac' atom (size, type, version /
        // flags).
        "alac" => {
            let mut atom = ((ours.len() + 12) as u32).to_be_bytes().to_vec();
            atom.extend_from_slice(b"alac\0\0\0\0");
            atom.extend_from_slice(ours);
            atom
        }
        _ => ours.to_vec(),
    }
}

fn check_foreign(row: &Row, path: &Path) -> Vec<String> {
    let mut issues = Vec::new();
    let bytes = std::fs::read(path).unwrap();
    let (ours, extradata, notes) = match read_ours_with(bytes, |d| check_records(row.name, d)) {
        Ok(v) => v,
        Err(e) => return vec![format!("this crate: {e}")],
    };
    issues.extend(notes);
    let got: Vec<&str> = ours.iter().map(|t| t.codec.as_str()).collect();
    if got != row.codecs {
        issues.push(format!("codec ids {got:?}, expected {:?}", row.codecs));
    }
    if ours.iter().any(|t| t.packets.is_empty()) {
        issues.push("a track with no packets".into());
    }
    if row.flags & NEGATIVE_START == 0 {
        issues.extend(check_seeks(path, &ours));
    }
    if row.flags & READER_BLIND != 0 {
        return issues;
    }
    let (theirs, _) = probe(path);
    if theirs.len() != ours.len() {
        issues.push(format!("{} tracks, reader {}", ours.len(), theirs.len()));
        return issues;
    }
    for (i, (o, t)) in ours.iter().zip(&theirs).enumerate() {
        if o.kind != t.kind {
            issues.push(format!("track {i}: kind {} vs {}", o.kind, t.kind));
        }
        for (what, a, b) in [
            ("width", o.width, t.width),
            ("height", o.height, t.height),
            ("sample rate", o.sample_rate, t.sample_rate),
        ] {
            if b.is_some() && a != b {
                issues.push(format!("track {i}: {what} {a:?} vs {b:?}"));
            }
        }
        if row.flags & DECODER_CHANNELS == 0 && t.channels.is_some() && o.channels != t.channels {
            issues.push(format!(
                "track {i}: channels {:?} vs {:?}",
                o.channels, t.channels
            ));
        }
        if o.language != t.language {
            issues.push(format!(
                "track {i}: language {} vs {}",
                o.language, t.language
            ));
        }
        if row.flags & READER_EXTRADATA == 0 {
            let want = reader_extradata(&o.codec, &extradata[i]);
            if (want.len(), crc32(&want)) != (t.extradata_len, t.extradata_crc) {
                issues.push(format!(
                    "track {i}: extradata {} bytes vs reader {}",
                    want.len(),
                    t.extradata_len
                ));
            }
        }
        if o.packets.len() != t.packets.len() {
            issues.push(format!(
                "track {i}: {} packets vs {}",
                o.packets.len(),
                t.packets.len()
            ));
            continue;
        }
        let last = o.packets.len().saturating_sub(1);
        for (n, (a, b)) in o.packets.iter().zip(&t.packets).enumerate() {
            let mut bad = Vec::new();
            if row.flags & READER_PAYLOAD == 0 && (a.size, a.crc) != (b.size, b.crc) {
                bad.push("payload");
            }
            if row.flags & READER_KEYS == 0 && a.key != b.key {
                bad.push("key flag");
            }
            // The reader leaves a timestamp it rejects (negative) open.
            if row.flags & NEGATIVE_START == 0 && b.pts.is_some() && a.pts != b.pts {
                bad.push("pts");
            }
            // Both state a duration: the same to within the tick the
            // reader truncates and this crate rounds (DefaultDuration is
            // in nanoseconds). The last packet of a track may carry the
            // BlockDuration left after a trim, which the reader replaces.
            if let (Some(x), Some(y)) = (a.duration, b.duration)
                && (x - y).abs() > 1
                && n != last
            {
                bad.push("duration");
            }
            if !bad.is_empty() {
                issues.push(format!(
                    "track {i} packet {n}: {} differ ({a:?} vs {b:?})",
                    bad.join(", ")
                ));
                break;
            }
        }
    }
    if row.name.starts_with("s_webvtt") {
        // docs/container/webm, "Storing WebVTT cue in a WebM Block":
        // the cue identifier line (empty here), the cue settings line
        // (empty here), then the payload. The reader hands out the
        // payload alone.
        let first = &ours[0].packets[0];
        let want = b"\n\nHello <i>world</i>";
        if (first.size, first.crc) != (want.len(), crc32(want)) {
            issues.push(format!("first WebVTT Block {first:?}"));
        }
    }
    if row.name == "a_wavpack" {
        // The Block is the WavPack block without the part of its
        // header the reader rebuilds (20 bytes on these mono blocks).
        for (a, b) in ours[0].packets.iter().zip(&theirs[0].packets) {
            if a.size + 20 != b.size {
                issues.push(format!("WavPack Block {a:?} vs reader {b:?}"));
                break;
            }
        }
    }
    if row.flags & NEGATIVE_START != 0 {
        // -0.5 s offset: the first video Block is 500 ticks before
        // zero, and the audio track's Blocks are read minus CodecDelay.
        if ours[0].packets[0].pts != Some(-500) {
            issues.push(format!("first video pts {:?}", ours[0].packets[0].pts));
        }
        if ours[1].packets[0].pts != Some(-521) {
            issues.push(format!("first audio pts {:?}", ours[1].packets[0].pts));
        }
    }
    issues
}

/// Seek every track of a foreign file — whatever index it has: Cues at
/// the end, at the front, or none (live, piped) — to targets before,
/// inside and after its packets, on the resilient path (which also
/// seeks the live shape). The landing must be the track's last key
/// frame at or before the target (the first key frame for a target
/// before it), and the track's next packet must be that key frame.
fn check_seeks(path: &Path, tracks: &[Track]) -> Vec<String> {
    let mut issues = Vec::new();
    let bytes = std::fs::read(path).unwrap();
    for (stream, track) in tracks.iter().enumerate() {
        let keys: Vec<i64> = track
            .packets
            .iter()
            .filter(|p| p.key)
            .filter_map(|p| p.pts)
            .collect();
        let (Some(&first), Some(&last)) = (keys.first(), keys.last()) else {
            continue;
        };
        let end = track
            .packets
            .iter()
            .filter_map(|p| p.pts)
            .max()
            .unwrap_or(last);
        let middle = keys[keys.len() / 2];
        for target in [
            first - 10,
            first,
            middle - 1,
            middle,
            (first + end) / 2,
            end + 100,
        ] {
            let reader: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes.clone()));
            let mut dmx = match oxideav_mkv::demux::open_resilient_typed(reader, &NullCodecResolver)
            {
                Ok(d) => d,
                Err(e) => return vec![format!("resilient open: {e}")],
            };
            // Key frames are in timestamp order on every track here.
            let want = keys
                .iter()
                .rev()
                .find(|k| **k <= target)
                .copied()
                .unwrap_or(first);
            let landed = match dmx.seek_to(stream as u32, target) {
                Ok(t) => t,
                Err(e) => {
                    issues.push(format!("track {stream}: seek to {target}: {e}"));
                    break;
                }
            };
            let next = loop {
                match dmx.next_packet() {
                    Ok(p) if p.stream_index == stream as u32 => break Some(p),
                    Ok(_) => {}
                    Err(_) => break None,
                }
            };
            let next = next.map(|p| (p.pts, p.flags.keyframe));
            if landed != want || next != Some((Some(want), true)) {
                issues.push(format!(
                    "track {stream}: seek to {target} landed {landed} (next packet {next:?}), \
                     expected the key frame at {want}"
                ));
                break;
            }
        }
    }
    issues
}

// ---------------------------------------------------------------------
// Ours → foreign
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Seekable file, index at the end.
    File,
    /// Index in a reservation before the first Cluster.
    FrontCues,
    /// Unknown-size Segment and Clusters, forward-only.
    Live,
    /// A new Cluster every 100 ms.
    ShortClusters,
    /// A new Cluster every 2000 bytes.
    SmallClusters,
    /// Xiph lacing of audio.
    Laced,
    /// Only what the `Demuxer` / `Muxer` traits carry: streams, flat
    /// metadata, packets.
    Traits,
    /// The WebM profile.
    Webm,
}

#[derive(Clone, Default)]
struct Sink(std::sync::Arc<Mutex<std::io::Cursor<Vec<u8>>>>);

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

fn remux(source: Vec<u8>, mode: Mode) -> oxideav_core::Result<Vec<u8>> {
    let reader: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(source));
    let mut dmx = open_typed(reader, &NullCodecResolver)?;
    let streams: Vec<StreamInfo> = dmx.streams().to_vec();
    let sink = Sink::default();
    let mut mux = if mode == Mode::Webm {
        MkvMuxer::new_webm(Box::new(sink.clone()), &streams)?
    } else {
        MkvMuxer::new_matroska(Box::new(sink.clone()), &streams)?
    };
    match mode {
        Mode::Live => {
            mux.with_live_streaming()?;
        }
        Mode::FrontCues => {
            mux.with_front_cues(4096)?;
        }
        Mode::ShortClusters => {
            mux.with_cluster_limits(100, 5 << 20)?;
        }
        Mode::SmallClusters => {
            mux.with_cluster_limits(5000, 2000)?;
        }
        Mode::Laced => {
            mux.with_block_lacing(LacingMode::Xiph)?;
        }
        Mode::File | Mode::Traits | Mode::Webm => {}
    }
    mux.set_metadata(dmx.metadata())?;
    if mode != Mode::Traits {
        for i in 0..streams.len() {
            mux.copy_track_header(i, &dmx, i as u32)?;
        }
        mux.copy_chapters_and_attachments(&mut dmx)?;
        if mode != Mode::Live
            && let Some(us) = dmx.duration_micros().filter(|us| *us > 0)
        {
            mux.set_duration(std::time::Duration::from_micros(us as u64))?;
        }
    }
    mux.write_header()?;
    loop {
        match dmx.next_packet() {
            Ok(p) if mode == Mode::Traits => mux.write_packet(&p)?,
            Ok(p) => mux.write_packet_from(&p, &dmx)?,
            Err(Error::Eof) => break,
            Err(e) => return Err(e),
        }
    }
    mux.write_trailer()?;
    drop(mux);
    let bytes = sink.0.lock().unwrap().get_ref().clone();
    Ok(bytes)
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
        || Command::new(tool)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
}

/// One mode of one row; returns what went wrong.
fn check_copy(
    row: &Row,
    source: &[u8],
    original: &[Track],
    source_warnings: &str,
    mode: Mode,
    out: &Path,
    player: bool,
) -> Vec<String> {
    let mut issues = Vec::new();
    let bytes = match remux(source.to_vec(), mode) {
        Ok(b) => b,
        Err(e) => return vec![format!("remux: {e}")],
    };
    std::fs::write(out, &bytes).unwrap();
    // This crate reads its own output back to the same packets — and,
    // for the typed copy of the two container rows, to the same
    // chapters / title / attachment.
    let records = |d: &mut oxideav_mkv::demux::MkvDemuxer| {
        if matches!(mode, Mode::Traits | Mode::Webm) {
            Vec::new()
        } else {
            check_records(row.name, d)
        }
    };
    match (read_ours(source.to_vec()), read_ours_with(bytes, records)) {
        (Ok((a, _)), Ok((b, _, notes))) => {
            issues.extend(notes);
            for (i, (x, y)) in a.iter().zip(&b).enumerate() {
                if x.codec != y.codec {
                    issues.push(format!(
                        "track {i}: reads back as {} (was {})",
                        y.codec, x.codec
                    ));
                }
                let same = x.packets.len() == y.packets.len()
                    && x.packets.iter().zip(&y.packets).all(|(p, q)| {
                        (p.pts, p.key, p.size, p.crc) == (q.pts, q.key, q.size, q.crc)
                    });
                if !same {
                    issues.push(format!(
                        "track {i}: this crate reads different packets back"
                    ));
                }
            }
        }
        (_, Err(e)) => issues.push(format!("this crate cannot read the copy: {e}")),
        (Err(e), _) => issues.push(format!("source: {e}")),
    }
    let (copy, warnings) = probe(out);
    let warnings: Vec<&str> = warnings
        .lines()
        // A live Segment states no Duration (RFC 9559 §23.2); the
        // reader says it estimated one.
        .filter(|l| !(mode == Mode::Live && l.contains("Estimating duration from bitrate")))
        // What the reader also says about the source is about the
        // content (an attachment it has no decoder for), not the copy.
        .filter(|l| !source_warnings.lines().any(|s| s == *l))
        .collect();
    if !warnings.is_empty() {
        issues.push(format!("reader warns: {}", warnings.join(" / ")));
    }
    if copy.len() != original.len() {
        issues.push(format!(
            "reader sees {} tracks, {} in the source",
            copy.len(),
            original.len()
        ));
        return issues;
    }
    for (i, (a, b)) in original.iter().zip(&copy).enumerate() {
        let mut a = a.clone();
        let mut b = b.clone();
        if row.flags & READER_EXTRADATA != 0 {
            (a.extradata_len, a.extradata_crc) = (0, 0);
            (b.extradata_len, b.extradata_crc) = (0, 0);
        }
        let (mut pa, mut pb) = (
            std::mem::take(&mut a.packets),
            std::mem::take(&mut b.packets),
        );
        if mode == Mode::Traits {
            // `StreamInfo` has no DefaultDuration; the reader's packet
            // durations follow from it.
            for p in pa.iter_mut().chain(pb.iter_mut()) {
                p.duration = None;
            }
        }
        if a != b {
            issues.push(format!("track {i}: reader reports {b:?}, source {a:?}"));
        }
        if pa.len() != pb.len() {
            issues.push(format!(
                "track {i}: reader sees {} packets, source {}",
                pb.len(),
                pa.len()
            ));
            continue;
        }
        if let Some(n) = pa.iter().zip(&pb).position(|(x, y)| x != y) {
            issues.push(format!(
                "track {i} packet {n}: reader reports {:?}, source {:?}",
                pb[n], pa[n]
            ));
        }
    }
    // The chain foreign → ours → foreign: the black-box muxer copies
    // this crate's file, and its reader must still see the source's
    // packets in the result.
    if mode == Mode::File {
        let chained = out.with_extension(format!("chain.{}", row.ext.replace("pipe", "mkv")));
        let copied = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(out)
            .args(["-map", "0", "-c", "copy", "-copyts"])
            .args(["-allow_raw_vfw", "1", "-strict", "-2"])
            .arg(&chained)
            .output()
            .expect("run ffmpeg");
        if !copied.status.success() {
            issues.push(format!(
                "black-box muxer refuses the copy as input: {}",
                String::from_utf8_lossy(&copied.stderr).trim()
            ));
        } else {
            let (again, _) = probe(&chained);
            for (i, (a, b)) in original.iter().zip(&again).enumerate() {
                let same = a.packets.len() == b.packets.len()
                    && a.packets.iter().zip(&b.packets).all(|(x, y)| {
                        (x.pts, x.key, x.size, x.crc) == (y.pts, y.key, y.size, y.crc)
                    });
                if !same {
                    issues.push(format!("track {i}: the chain's packets differ"));
                }
            }
            if again.len() != original.len() {
                issues.push(format!("the chain has {} tracks", again.len()));
            }
        }
        let _ = std::fs::remove_file(&chained);
    }
    if player {
        let played = Command::new("mpv")
            .args([
                "--no-config",
                "--vo=null",
                "--ao=null",
                "--frames=5",
                "--msg-level=all=error",
            ])
            .arg(out)
            .output()
            .expect("run mpv");
        let said = String::from_utf8_lossy(&played.stdout).into_owned()
            + &String::from_utf8_lossy(&played.stderr);
        let complaints: Vec<&str> = said.lines().filter(|l| l.starts_with('[')).collect();
        if !played.status.success() || !complaints.is_empty() {
            issues.push(format!(
                "player: {} {}",
                played.status,
                complaints.join(" / ")
            ));
        }
    }
    issues
}

// ---------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------

#[test]
fn black_box_matrix() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("foreign_matrix: ffmpeg / ffprobe not installed — skipped");
        return;
    }
    let player = have("mpv");
    if !player {
        eprintln!("foreign_matrix: mpv not installed — the second reader is skipped");
    }
    let dir: PathBuf =
        std::env::temp_dir().join(format!("oxideav-mkv-foreign-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_inputs(&dir);
    let rows = rows(&dir);

    let next = AtomicUsize::new(0);
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let skipped: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let (foreign_ok, foreign_all) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let (copies_ok, copies_all) = (AtomicUsize::new(0), AtomicUsize::new(0));

    // The tools are the slow part; four rows at a time.
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                while let Some(row) = rows.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let ext = if row.ext == "pipe" { "mkv" } else { row.ext };
                    let path = dir.join(format!("{}.{ext}", row.name));
                    let mut cmd = Command::new("ffmpeg");
                    cmd.args(["-hide_banner", "-loglevel", "error", "-y"])
                        .args(&row.args);
                    let made = if row.ext == "pipe" {
                        cmd.arg("-").output().is_ok_and(|o| {
                            o.status.success() && std::fs::write(&path, &o.stdout).is_ok()
                        })
                    } else {
                        cmd.arg(&path).output().is_ok_and(|o| o.status.success())
                    };
                    if !made || std::fs::metadata(&path).map_or(0, |m| m.len()) == 0 {
                        skipped.lock().unwrap().push(row.name.to_owned());
                        continue;
                    }

                    foreign_all.fetch_add(1, Ordering::Relaxed);
                    let issues = check_foreign(row, &path);
                    if issues.is_empty() {
                        foreign_ok.fetch_add(1, Ordering::Relaxed);
                    } else {
                        failures.lock().unwrap().push(format!(
                            "{} [foreign → ours]: {}",
                            row.name,
                            issues.join("; ")
                        ));
                    }

                    // Copies are judged against the reader's view of
                    // the source, so the reader has to have one, with
                    // timestamps it reports.
                    if row.flags & (READER_BLIND | NEGATIVE_START) != 0 {
                        continue;
                    }
                    let source = std::fs::read(&path).unwrap();
                    let (original, source_warnings) = probe(&path);
                    let mut modes = vec![
                        Mode::File,
                        Mode::FrontCues,
                        Mode::Live,
                        Mode::ShortClusters,
                        Mode::SmallClusters,
                        Mode::Laced,
                        Mode::Traits,
                    ];
                    if row.ext == "webm" {
                        modes.push(Mode::Webm);
                    }
                    for mode in modes {
                        // What only the typed copy carries: the last
                        // frame's trim, BlockAdditions, a non-PCM
                        // BitDepth, per-track video records.
                        if mode == Mode::Traits && !traits_suffice(row) {
                            continue;
                        }
                        copies_all.fetch_add(1, Ordering::Relaxed);
                        let ext = if mode == Mode::Webm { "webm" } else { "mkv" };
                        let out = dir.join(format!("{}.{mode:?}.{ext}", row.name));
                        let play = player
                            && matches!(mode, Mode::File | Mode::Webm)
                            && row.flags & NO_PLAYER == 0;
                        let issues =
                            check_copy(row, &source, &original, &source_warnings, mode, &out, play);
                        let _ = std::fs::remove_file(&out);
                        if issues.is_empty() {
                            copies_ok.fetch_add(1, Ordering::Relaxed);
                        } else {
                            failures.lock().unwrap().push(format!(
                                "{} [ours → foreign, {mode:?}]: {}",
                                row.name,
                                issues.join("; ")
                            ));
                        }
                    }
                    let _ = std::fs::remove_file(&path);
                }
            });
        }
    });
    let _ = std::fs::remove_dir_all(&dir);

    let failures = failures.into_inner().unwrap();
    let skipped = skipped.into_inner().unwrap();
    eprintln!(
        "foreign_matrix: foreign → ours {}/{} files, ours → foreign {}/{} copies, \
         {} rows not generated by this ffmpeg build {:?}",
        foreign_ok.load(Ordering::Relaxed),
        foreign_all.load(Ordering::Relaxed),
        copies_ok.load(Ordering::Relaxed),
        copies_all.load(Ordering::Relaxed),
        skipped.len(),
        skipped,
    );
    assert!(
        failures.is_empty(),
        "{} failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        foreign_all.load(Ordering::Relaxed) >= 60,
        "the tool generated too few rows for the matrix to mean anything"
    );
}

/// Rows whose copy survives with only what the traits carry. The others
/// need the typed track-header copy: a BlockAddition side channel
/// (alpha), a `BitDepth` / video record the reader reports, or a
/// last-frame trim.
fn traits_suffice(row: &Row) -> bool {
    !matches!(
        row.name,
        "w_vp9_alpha" | "w_vp8_alpha" | "a_tta" | "l_stereo" | "l_sar" | "l_colour"
    )
}
