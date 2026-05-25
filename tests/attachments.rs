//! Integration test for the demuxer's `Attachments` parsing.
//!
//! Builds a minimal MKV with two `AttachedFile`s and verifies that each
//! attachment surfaces in `Demuxer::metadata()` as
//! `attachment:N:filename` / `:mime_type` / `:size_bytes`. Payload bytes
//! are not exposed (the demuxer skips them via seek).
//!
//! Reference: <https://www.matroska.org/technical/attachments.html>

use std::io::Cursor;

use oxideav_core::{Demuxer, ReadSeek};
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

fn elem_bytes(id: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(bytes.len() as u64, 0));
    out.extend_from_slice(bytes);
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

fn simple_block(track: u8, tc_offset: i16, keyframe: bool, payload: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(if keyframe { 0x80 } else { 0x00 });
    body.push(payload);
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(ids::SIMPLE_BLOCK));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(&body);
    out
}

fn attached_file(uid: u64, filename: &str, mime: &str, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_uint(ids::FILE_UID, uid));
    body.extend_from_slice(&elem_str(ids::FILE_NAME, filename));
    body.extend_from_slice(&elem_str(ids::FILE_MIME_TYPE, mime));
    body.extend_from_slice(&elem_bytes(ids::FILE_DATA, data));
    elem_master(ids::ATTACHED_FILE, &body)
}

fn attached_file_with_description(
    uid: u64,
    filename: &str,
    mime: &str,
    description: &str,
    data: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&elem_str(ids::FILE_DESCRIPTION, description));
    body.extend_from_slice(&elem_uint(ids::FILE_UID, uid));
    body.extend_from_slice(&elem_str(ids::FILE_NAME, filename));
    body.extend_from_slice(&elem_str(ids::FILE_MIME_TYPE, mime));
    body.extend_from_slice(&elem_bytes(ids::FILE_DATA, data));
    elem_master(ids::ATTACHED_FILE, &body)
}

fn build_mkv_with_attachments() -> Vec<u8> {
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

    // Tracks.
    let mut track_body = Vec::new();
    track_body.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_UID, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    track_body.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio_body.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio_body.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    track_body.extend_from_slice(&elem_master(ids::AUDIO, &audio_body));
    let track_entry = elem_master(ids::TRACK_ENTRY, &track_body);
    let tracks = elem_master(ids::TRACKS, &track_entry);

    // Attachments: a 12-byte "font" + a 7-byte "image".
    let mut atts_body = Vec::new();
    atts_body.extend_from_slice(&attached_file(
        0xA1,
        "subs.ttf",
        "application/x-truetype-font",
        b"FONTPAYLOAD!",
    ));
    atts_body.extend_from_slice(&attached_file(0xA2, "cover.png", "image/png", b"PNGDATA"));
    let attachments = elem_master(ids::ATTACHMENTS, &atts_body);

    // Cluster.
    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&attachments);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header);
    out.extend_from_slice(&segment);
    out
}

#[test]
fn attachments_surface_in_metadata() {
    let bytes = build_mkv_with_attachments();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx = oxideav_mkv::demux::open(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let md: Vec<(String, String)> = dmx.metadata().to_vec();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());

    assert_eq!(get("attachment:1:filename").as_deref(), Some("subs.ttf"));
    assert_eq!(
        get("attachment:1:mime_type").as_deref(),
        Some("application/x-truetype-font")
    );
    assert_eq!(get("attachment:1:size_bytes").as_deref(), Some("12"));

    assert_eq!(get("attachment:2:filename").as_deref(), Some("cover.png"));
    assert_eq!(get("attachment:2:mime_type").as_deref(), Some("image/png"));
    assert_eq!(get("attachment:2:size_bytes").as_deref(), Some("7"));
}

#[test]
fn typed_attachments_expose_filename_mime_uid_and_size() {
    let bytes = build_mkv_with_attachments();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    let atts = dmx.attachments();
    assert_eq!(atts.len(), 2, "two AttachedFiles were written");

    assert_eq!(atts[0].index, 1);
    assert_eq!(atts[0].filename, "subs.ttf");
    assert_eq!(atts[0].mime_type, "application/x-truetype-font");
    assert_eq!(atts[0].description, "");
    assert_eq!(atts[0].uid, 0xA1);
    assert_eq!(atts[0].data_size, 12);

    assert_eq!(atts[1].index, 2);
    assert_eq!(atts[1].filename, "cover.png");
    assert_eq!(atts[1].mime_type, "image/png");
    assert_eq!(atts[1].description, "");
    assert_eq!(atts[1].uid, 0xA2);
    assert_eq!(atts[1].data_size, 7);

    // data_offsets are byte positions into the input — not equal, and the
    // second sits after the first plus its payload.
    assert_ne!(atts[0].data_offset, atts[1].data_offset);
}

#[test]
fn attachment_data_returns_full_payload_on_demand() {
    let bytes = build_mkv_with_attachments();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    // Read both payloads — first the cover.png (out of order on purpose to
    // confirm the helper accepts any 1-based index, not a positional
    // iterator), then the font.
    let cover = dmx.attachment_data(2).expect("read cover.png");
    assert_eq!(cover, b"PNGDATA");

    let font = dmx.attachment_data(1).expect("read subs.ttf");
    assert_eq!(font, b"FONTPAYLOAD!");

    // The reader position should be unaffected by attachment fetches — we
    // can still drain a packet from the cluster that follows the
    // Attachments element.
    let pkt = dmx.next_packet().expect("first packet still readable");
    assert_eq!(pkt.data, vec![0xAA]);
}

#[test]
fn attachment_data_rejects_invalid_indices() {
    let bytes = build_mkv_with_attachments();
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    assert!(
        dmx.attachment_data(0).is_err(),
        "index 0 is invalid (attachments are 1-based)"
    );
    assert!(
        dmx.attachment_data(99).is_err(),
        "index past the last attachment must error"
    );
    // Confirm a valid index still works after the rejected calls — the
    // helper restores reader state on error too.
    assert_eq!(dmx.attachment_data(1).unwrap(), b"FONTPAYLOAD!");
}

#[test]
fn file_description_surfaces_in_both_views() {
    // Build a file with one attachment carrying a FileDescription.
    let mut ebml_body = Vec::new();
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    ebml_body.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    let ebml_header = elem_master(ids::EBML_HEADER, &ebml_body);

    let mut info_body = Vec::new();
    info_body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info_body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 1000.0));
    let info = elem_master(ids::INFO, &info_body);

    let mut track_body = Vec::new();
    track_body.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_UID, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    track_body.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio_body.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio_body.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    track_body.extend_from_slice(&elem_master(ids::AUDIO, &audio_body));
    let track_entry = elem_master(ids::TRACK_ENTRY, &track_body);
    let tracks = elem_master(ids::TRACKS, &track_entry);

    let mut atts_body = Vec::new();
    atts_body.extend_from_slice(&attached_file_with_description(
        0x1234,
        "lyrics.txt",
        "text/plain",
        "Song lyrics in plain UTF-8",
        b"line one\nline two",
    ));
    let attachments = elem_master(ids::ATTACHMENTS, &atts_body);

    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&attachments);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&ebml_header);
    bytes.extend_from_slice(&segment);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    // Flat metadata view picks up the description.
    let md: Vec<(String, String)> = dmx.metadata().to_vec();
    let get = |k: &str| md.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    assert_eq!(
        get("attachment:1:description").as_deref(),
        Some("Song lyrics in plain UTF-8")
    );

    // Typed accessor preserves the description, UID, and payload bytes.
    let atts = dmx.attachments();
    assert_eq!(atts.len(), 1);
    assert_eq!(atts[0].description, "Song lyrics in plain UTF-8");
    assert_eq!(atts[0].uid, 0x1234);
    assert_eq!(atts[0].data_size, b"line one\nline two".len() as u64);

    let payload = dmx.attachment_data(1).unwrap();
    assert_eq!(payload, b"line one\nline two");
}

#[test]
fn typed_attachments_empty_when_no_attachments_element() {
    // Build a minimal MKV with no Attachments master at all.
    let mut ebml_body = Vec::new();
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    ebml_body.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    let ebml_header = elem_master(ids::EBML_HEADER, &ebml_body);

    let mut info_body = Vec::new();
    info_body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    info_body.extend_from_slice(&elem_float_be_f64(ids::DURATION, 1000.0));
    let info = elem_master(ids::INFO, &info_body);

    let mut track_body = Vec::new();
    track_body.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_UID, 1));
    track_body.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    track_body.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut audio_body = Vec::new();
    audio_body.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio_body.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    audio_body.extend_from_slice(&elem_uint(ids::BIT_DEPTH, 16));
    track_body.extend_from_slice(&elem_master(ids::AUDIO, &audio_body));
    let track_entry = elem_master(ids::TRACK_ENTRY, &track_body);
    let tracks = elem_master(ids::TRACKS, &track_entry);

    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cluster_body.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&ebml_header);
    bytes.extend_from_slice(&segment);

    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut dmx =
        oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open");

    assert!(dmx.attachments().is_empty());
    assert!(dmx.attachment_data(1).is_err());
}
