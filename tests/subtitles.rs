//! Integration test for the demuxer's subtitle-track handling.
//!
//! Builds a minimal MKV containing one PCM audio track and one
//! `S_TEXT/UTF8` subtitle track, opens it through the demuxer, and
//! checks that
//!
//! 1. The subtitle stream surfaces with `MediaType::Subtitle`, not
//!    `MediaType::Data` (the pre-fix behaviour).
//! 2. The codec id maps to `subrip` via `from_matroska` rather than the
//!    pass-through `mkv:S_TEXT/UTF8` form.
//! 3. The subtitle packet is delivered through `next_packet()` carrying
//!    the original UTF-8 cue text.
//!
//! Reference: <https://www.matroska.org/technical/codec_specs.html#subtitles>

use std::io::Cursor;

use oxideav_core::{Error, MediaType, ReadSeek};
use oxideav_mkv::ebml::{write_element_id, write_vint};
use oxideav_mkv::ids;

fn elem_uint(id: u32, value: u64) -> Vec<u8> {
    let n = if value == 0 {
        1
    } else {
        (64 - value.leading_zeros()).div_ceil(8) as usize
    };
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(n as u64, 0));
    for i in (0..n).rev() {
        out.push(((value >> (i * 8)) & 0xFF) as u8);
    }
    out
}

fn elem_str(id: u32, s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(s.len() as u64, 0));
    out.extend_from_slice(s.as_bytes());
    out
}

fn elem_float_be_f64(id: u32, value: f64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(8, 0));
    out.extend_from_slice(&value.to_be_bytes());
    out
}

fn elem_master(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

fn simple_block(track: u8, tc_offset: i16, keyframe: bool, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(if keyframe { 0x80 } else { 0x00 });
    body.extend_from_slice(payload);
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(ids::SIMPLE_BLOCK));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(&body);
    out
}

fn build_mkv_with_subtitle() -> Vec<u8> {
    // EBML header.
    let mut ebml_body = Vec::new();
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    ebml_body.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    let ebml_header = elem_master(ids::EBML_HEADER, &ebml_body);

    // Info.
    let mut info_body = Vec::new();
    info_body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info_body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 1000.0));
    let info = elem_master(ids::INFO, &info_body);

    // Tracks: 1 = PCM audio, 2 = S_TEXT/UTF8 subtitle.
    let mut audio_entry = Vec::new();
    audio_entry.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    audio_entry.extend_from_slice(&elem_uint(ids::TRACK_UID, 1));
    audio_entry.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    audio_entry.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio_body.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio_body.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    audio_entry.extend_from_slice(&elem_master(ids::AUDIO, &audio_body));
    let audio_track = elem_master(ids::TRACK_ENTRY, &audio_entry);

    let mut sub_entry = Vec::new();
    sub_entry.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 2));
    sub_entry.extend_from_slice(&elem_uint(ids::TRACK_UID, 2));
    sub_entry.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_SUBTITLE));
    sub_entry.extend_from_slice(&elem_str(ids::CODEC_ID, "S_TEXT/UTF8"));
    let sub_track = elem_master(ids::TRACK_ENTRY, &sub_entry);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&audio_track);
    tracks_body.extend_from_slice(&sub_track);
    let tracks = elem_master(ids::TRACKS, &tracks_body);

    // Cluster: one audio block at t=0, one subtitle block at t=500.
    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&simple_block(1, 0, true, &[0xAA]));
    cluster_body.extend_from_slice(&simple_block(2, 500, true, b"Hello, subs!"));
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header);
    out.extend_from_slice(&segment);
    out
}

#[test]
fn subtitle_stream_has_subtitle_media_type() {
    let bytes = build_mkv_with_subtitle();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    // Two streams: audio @ idx 0, subtitle @ idx 1.
    assert_eq!(dmx.streams().len(), 2);
    assert_eq!(dmx.streams()[0].params.media_type, MediaType::Audio);

    let sub = &dmx.streams()[1];
    assert_eq!(
        sub.params.media_type,
        MediaType::Subtitle,
        "subtitle track must surface as MediaType::Subtitle (was MediaType::Data before fix)"
    );
    assert_eq!(
        sub.params.codec_id.as_str(),
        "subrip",
        "S_TEXT/UTF8 should map to 'subrip', not the pass-through 'mkv:S_TEXT/UTF8'"
    );

    // Pull packets and check the subtitle bytes round-trip.
    let mut got_subtitle = false;
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                if p.stream_index == 1 {
                    assert_eq!(p.data, b"Hello, subs!".to_vec());
                    assert_eq!(p.pts, Some(500));
                    got_subtitle = true;
                }
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }
    assert!(got_subtitle, "subtitle packet should be delivered");
}
