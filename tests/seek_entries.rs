//! Integration tests for the demuxer's typed `SeekHead` accessor
//! (RFC 9559 §5.1.1), surfaced through `MkvDemuxer::seek_entries`.
//!
//! The `SeekHead` element (a.k.a. MetaSeek, RFC 9559 §4.5 / §6.3) indexes
//! the Segment Position of each Top-Level Element so a reader can jump
//! straight to `Cues` / `Tracks` / `Tags` / `Chapters` / `Attachments` /
//! a second `SeekHead` without scanning the whole file. The in-tree
//! demuxer doesn't *navigate* by it (it walks the Segment children
//! directly and seeks via `Cues`), so the accessor is a pure inspection /
//! re-mux surface. Each `Seek` (§5.1.1.1) carries one `SeekID`
//! (§5.1.1.1.1, a 4-byte binary EBML ID) and one `SeekPosition`
//! (§5.1.1.1.2, a Segment Position — Section 16).
//!
//! These tests exercise:
//! * an absent `SeekHead` → empty slice (legal, §6.3 only RECOMMENDS it);
//! * a single `SeekHead` whose `Seek` rows decode `seek_id()`,
//!   `seek_id_bytes()`, and `seek_position()` correctly, with the
//!   positions matching the real on-disk Segment offsets of the indexed
//!   elements;
//! * the §6.3 two-`SeekHead` layout — entries from both accumulate in
//!   document order;
//! * a `SeekID` referencing an element this build needn't recognise — the
//!   raw bytes survive verbatim and `seek_id()` still decodes the u32;
//! * malformed `Seek` rows (missing `SeekID` bytes / missing
//!   `SeekPosition`) surfaced for inspection rather than dropped;
//! * a mux→demux round-trip — the muxer's emitted `SeekHead` reads back
//!   through the typed accessor with positions that land on the real
//!   on-disk Top-Level elements.

use std::io::Cursor;

use oxideav_core::ReadSeek;
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

fn elem_master(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&write_element_id(id));
    out.extend_from_slice(&write_vint(body.len() as u64, 0));
    out.extend_from_slice(body);
    out
}

fn ebml_header() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    b.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    b.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 4));
    b.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    elem_master(ids::EBML_HEADER, &b)
}

fn info() -> Vec<u8> {
    let mut ib = Vec::new();
    ib.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    elem_master(ids::INFO, &ib)
}

fn one_track() -> Vec<u8> {
    let mut tb = Vec::new();
    tb.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_UID, 0xA1));
    tb.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_VIDEO));
    tb.extend_from_slice(&elem_str(ids::CODEC_ID, "V_VP9"));
    let mut v = Vec::new();
    v.extend_from_slice(&elem_uint(ids::PIXEL_WIDTH, 320));
    v.extend_from_slice(&elem_uint(ids::PIXEL_HEIGHT, 240));
    tb.extend_from_slice(&elem_master(ids::VIDEO, &v));
    elem_master(ids::TRACKS, &elem_master(ids::TRACK_ENTRY, &tb))
}

fn simple_block(track: u8, tc_offset: i16, keyframe: bool, payload: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&write_vint(track as u64, 0));
    body.extend_from_slice(&tc_offset.to_be_bytes());
    body.push(if keyframe { 0x80 } else { 0x00 });
    body.push(payload);
    elem_master(ids::SIMPLE_BLOCK, &body)
}

fn one_cluster() -> Vec<u8> {
    let mut cb = Vec::new();
    cb.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    cb.extend_from_slice(&simple_block(1, 0, true, 0xAA));
    elem_master(ids::CLUSTER, &cb)
}

/// One `Seek` child: SeekID(raw bytes) + SeekPosition(value).
fn seek(seek_id_bytes: &[u8], position: u64) -> Vec<u8> {
    let mut sb = Vec::new();
    sb.extend_from_slice(&elem_bytes(ids::SEEK_ID, seek_id_bytes));
    sb.extend_from_slice(&elem_uint(ids::SEEK_POSITION, position));
    elem_master(ids::SEEK, &sb)
}

/// The 4-byte on-wire EBML id encoding for a known Top-Level element id.
fn id_bytes(id: u32) -> Vec<u8> {
    write_element_id(id).to_vec()
}

fn open(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open")
}

/// Assemble an EBML header + Segment from the supplied Segment body parts,
/// in order. The Segment uses a known size so the demuxer can validate
/// child bounds.
fn assemble(segment_parts: &[Vec<u8>]) -> Vec<u8> {
    let mut seg = Vec::new();
    for p in segment_parts {
        seg.extend_from_slice(p);
    }
    let segment = elem_master(ids::SEGMENT, &seg);
    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header());
    out.extend_from_slice(&segment);
    out
}

/// A file with no `SeekHead` surfaces an empty `seek_entries` slice — the
/// element's use is only RECOMMENDED (RFC 9559 §6.3).
#[test]
fn absent_seek_head_is_empty_slice() {
    let bytes = assemble(&[info(), one_track(), one_cluster()]);
    let dmx = open(bytes);
    assert!(dmx.seek_entries().is_empty(), "no SeekHead → empty slice");
}

/// A single `SeekHead` indexing Info + Tracks. Each entry decodes its
/// `SeekID` to the right element id, preserves the raw bytes, and reports
/// the Segment Position the writer encoded.
#[test]
fn single_seek_head_decodes_entries() {
    // Hand-pick positions; the accessor surfaces them verbatim.
    let sh = elem_master(
        ids::SEEK_HEAD,
        &[
            seek(&id_bytes(ids::INFO), 0x40),
            seek(&id_bytes(ids::TRACKS), 0x80),
        ]
        .concat(),
    );
    let bytes = assemble(&[sh, info(), one_track(), one_cluster()]);
    let dmx = open(bytes);

    let entries = dmx.seek_entries();
    assert_eq!(entries.len(), 2, "two Seek rows");

    assert_eq!(entries[0].seek_id(), Some(ids::INFO));
    assert_eq!(entries[0].seek_id_bytes(), id_bytes(ids::INFO).as_slice());
    assert_eq!(entries[0].seek_position(), 0x40);
    assert!(entries[0].has_position());

    assert_eq!(entries[1].seek_id(), Some(ids::TRACKS));
    assert_eq!(entries[1].seek_position(), 0x80);
    assert!(entries[1].has_position());
}

/// The §6.3 two-`SeekHead` layout — the first SeekHead references the
/// second, and entries from both accumulate in document order onto one
/// slice.
#[test]
fn two_seek_heads_accumulate_in_document_order() {
    let first = elem_master(
        ids::SEEK_HEAD,
        &[
            seek(&id_bytes(ids::INFO), 0x10),
            seek(&id_bytes(ids::SEEK_HEAD), 0x200), // points at the second SeekHead
        ]
        .concat(),
    );
    let second = elem_master(
        ids::SEEK_HEAD,
        &[seek(&id_bytes(ids::CLUSTER), 0x300)].concat(),
    );
    let bytes = assemble(&[first, info(), one_track(), second, one_cluster()]);
    let dmx = open(bytes);

    let entries = dmx.seek_entries();
    assert_eq!(entries.len(), 3, "1 + 2 entries across both SeekHeads");
    assert_eq!(entries[0].seek_id(), Some(ids::INFO));
    assert_eq!(entries[1].seek_id(), Some(ids::SEEK_HEAD));
    assert_eq!(entries[2].seek_id(), Some(ids::CLUSTER));
    assert_eq!(entries[2].seek_position(), 0x300);
}

/// A `SeekID` referencing an element this build needn't recognise: the raw
/// bytes survive verbatim and `seek_id()` still decodes the big-endian
/// u32, so a re-muxer can round-trip it without interpretation.
#[test]
fn unknown_seek_id_survives_verbatim() {
    // A made-up class-D 4-byte id (high bit of first octet set → 4-byte
    // EBML id space). 0x1FFFFFFF is the all-ones reserved id; use a
    // distinct plausible-shaped value instead.
    let raw = [0x1A, 0x45, 0xDF, 0xA3]; // happens to be the EBML header id
    let sh = elem_master(ids::SEEK_HEAD, &seek(&raw, 0x1234));
    let bytes = assemble(&[sh, info(), one_track(), one_cluster()]);
    let dmx = open(bytes);

    let entries = dmx.seek_entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].seek_id_bytes(), &raw[..]);
    assert_eq!(entries[0].seek_id(), Some(0x1A45DFA3));
    assert_eq!(entries[0].seek_position(), 0x1234);
}

/// A malformed `Seek` missing its mandatory `SeekPosition` is surfaced for
/// inspection (with `seek_position() == 0` and `has_position() == false`)
/// rather than dropped; a `Seek` missing `SeekID` surfaces empty raw bytes
/// and `seek_id() == None`.
#[test]
fn malformed_seek_rows_surface_for_inspection() {
    let seek_no_pos = elem_master(ids::SEEK, &elem_bytes(ids::SEEK_ID, &id_bytes(ids::CUES)));
    let seek_no_id = elem_master(ids::SEEK, &elem_uint(ids::SEEK_POSITION, 0x99));
    let sh = elem_master(ids::SEEK_HEAD, &[seek_no_pos, seek_no_id].concat());
    let bytes = assemble(&[sh, info(), one_track(), one_cluster()]);
    let dmx = open(bytes);

    let entries = dmx.seek_entries();
    assert_eq!(entries.len(), 2);

    // First: SeekID present, SeekPosition absent.
    assert_eq!(entries[0].seek_id(), Some(ids::CUES));
    assert!(!entries[0].has_position(), "missing SeekPosition");
    assert_eq!(entries[0].seek_position(), 0);

    // Second: SeekPosition present, SeekID absent.
    assert_eq!(entries[1].seek_id_bytes(), &[] as &[u8]);
    assert_eq!(entries[1].seek_id(), None, "empty SeekID → None");
    assert!(entries[1].has_position());
    assert_eq!(entries[1].seek_position(), 0x99);
}

/// An empty `SeekID` of length > 4 (a malformed payload) can't be a
/// standard EBML id and decodes to `None`, but the raw bytes survive.
#[test]
fn oversize_seek_id_decodes_none_keeps_bytes() {
    let raw = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE]; // 5 bytes — too long for a u32 id
    let sh = elem_master(ids::SEEK_HEAD, &seek(&raw, 7));
    let bytes = assemble(&[sh, info(), one_track(), one_cluster()]);
    let dmx = open(bytes);

    let entries = dmx.seek_entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].seek_id(), None);
    assert_eq!(entries[0].seek_id_bytes(), &raw[..]);
}

// ---------------------------------------------------------------------------
// mux → demux round-trip
// ---------------------------------------------------------------------------

mod roundtrip {
    use oxideav_core::{
        CodecId, CodecParameters, Demuxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
    };
    use oxideav_mkv::ids;

    fn opus_head(channels: u8, sample_rate: u32, pre_skip: u16) -> Vec<u8> {
        let mut out = Vec::with_capacity(19);
        out.extend_from_slice(b"OpusHead");
        out.push(1);
        out.push(channels);
        out.extend_from_slice(&pre_skip.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
        out.push(0);
        out
    }

    /// The muxer emits a fixed `SeekHead` at the top of the Segment with
    /// Seek entries for Info / Tracks / Cues. After a full write the typed
    /// accessor reads those entries back, and each `SeekPosition`
    /// (a Segment Position) plus the Segment data-start lands on the
    /// matching on-disk Top-Level element header.
    #[test]
    fn muxer_seek_head_reads_back_and_positions_are_correct() {
        let mut op = CodecParameters::audio(CodecId::new("opus"));
        op.sample_rate = Some(48_000);
        op.channels = Some(2);
        op.extradata = opus_head(2, 48_000, 312);
        let audio = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 48_000),
            duration: None,
            start_time: Some(0),
            params: op,
        };
        let streams = vec![audio.clone()];

        let tmp = std::env::temp_dir().join("oxideav-mkv-seek-entries-roundtrip.webm");
        {
            let f = std::fs::File::create(&tmp).unwrap();
            let ws: Box<dyn WriteSeek> = Box::new(f);
            let mut mux = oxideav_mkv::mux::open_webm(ws, &streams).unwrap();
            mux.write_header().unwrap();
            let samples_per_20ms: i64 = 48_000 / 50;
            for i in 0..16i64 {
                let mut p = Packet::new(0, audio.time_base, vec![0x80u8, i as u8]);
                p.pts = Some(i * samples_per_20ms);
                p.duration = Some(samples_per_20ms);
                p.flags.keyframe = true;
                mux.write_packet(&p).unwrap();
            }
            mux.write_trailer().unwrap();
        }
        let raw = std::fs::read(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);

        // Re-read with the demuxer and drain so any post-Cluster Cues
        // entry the SeekHead points at is fully materialised.
        let rs: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(raw.clone()));
        let mut dmx = oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).unwrap();
        loop {
            match dmx.next_packet() {
                Ok(_) => {}
                Err(oxideav_core::Error::Eof) => break,
                Err(e) => panic!("demux error: {e:?}"),
            }
        }

        let entries = dmx.seek_entries();
        assert!(
            !entries.is_empty(),
            "muxer must emit a SeekHead with Seek entries"
        );

        // The muxer indexes Info, Tracks and Cues. Confirm each indexed id
        // is present and its SeekPosition lands on the real element.
        let segment_data_start = segment_data_start(&raw);
        let mut saw_info = false;
        let mut saw_tracks = false;
        let mut saw_cues = false;
        for e in entries {
            let id = e.seek_id().expect("muxer writes 4-byte SeekIDs");
            // Skip the Cues entry if the muxer voided it (no packets) — here
            // we wrote packets, so Cues must be real.
            let abs = segment_data_start + e.seek_position();
            let on_disk = element_id_at(&raw, abs);
            assert_eq!(
                on_disk,
                Some(id),
                "SeekPosition for id {id:#x} must point at that element \
                 (abs offset {abs})"
            );
            match id {
                ids::INFO => saw_info = true,
                ids::TRACKS => saw_tracks = true,
                ids::CUES => saw_cues = true,
                _ => {}
            }
        }
        assert!(saw_info, "SeekHead indexes Info");
        assert!(saw_tracks, "SeekHead indexes Tracks");
        assert!(saw_cues, "SeekHead indexes Cues (packets were written)");
    }

    /// Find the Segment data-start (the byte right after the Segment
    /// element's id+size header) in a muxed file.
    fn segment_data_start(raw: &[u8]) -> u64 {
        use oxideav_mkv::ebml::read_element_header;
        use std::io::{Seek, SeekFrom};
        let mut cur = std::io::Cursor::new(raw);
        let h = read_element_header(&mut cur).expect("EBML header");
        assert_eq!(h.id, ids::EBML_HEADER);
        cur.seek(SeekFrom::Current(h.size as i64)).unwrap();
        let seg = read_element_header(&mut cur).expect("Segment header");
        assert_eq!(seg.id, ids::SEGMENT);
        cur.position()
    }

    /// Return the EBML element id whose header begins at absolute offset
    /// `abs`, or `None` if the bytes there don't parse as an element id.
    fn element_id_at(raw: &[u8], abs: u64) -> Option<u32> {
        use oxideav_mkv::ebml::read_element_header;
        let mut cur = std::io::Cursor::new(raw);
        use std::io::{Seek, SeekFrom};
        cur.seek(SeekFrom::Start(abs)).ok()?;
        read_element_header(&mut cur).ok().map(|e| e.id)
    }
}
