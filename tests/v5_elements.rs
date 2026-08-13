//! Demux-side tests for the six Matroska **v5** elements (staged
//! `docs/container/matroska/post-rfc9559-elements.md` — the elements the
//! CELLAR schema carries with `minver: 5` that no RFC and no IANA
//! registry row define):
//!
//! * `EditionDisplay` (`0x4520`) / `EditionString` (`0x4521`) /
//!   `EditionLanguageIETF` (`0x45E4`) — edition-level display names;
//! * `ChapterSkipType` (`0x4588`) — per-atom skip classification;
//! * `Emphasis` (`0x52F1`) — per-Audio-master emphasis filter;
//! * `TagBlockAddIDValue` (`0x63C7`) — Targets selector for
//!   `BlockAdditionMapping`s, joint with `TagTrackUID`.
//!
//! Also pins RFC 9559 errata ID 8615 (transcribed in the staged doc §8):
//! `AttachmentLink`'s printed `maxOccurs: 1` is bogus — the typed surface
//! models a list.
//!
//! Fixtures are hand-assembled EBML using the crate's own primitives —
//! no third-party Matroska code is consulted.

use std::io::Cursor;

use oxideav_core::ReadSeek;
use oxideav_mkv::demux::{AudioEmphasis, ChapterSkipType};
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

/// One `EditionDisplay` master body: `string` (None = omit the mandatory
/// `EditionString` → malformed) + languages.
fn edition_display(string: Option<&str>, languages: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    if let Some(s) = string {
        body.extend_from_slice(&elem_str(ids::EDITION_STRING, s));
    }
    for lang in languages {
        body.extend_from_slice(&elem_str(ids::EDITION_LANGUAGE_IETF, lang));
    }
    elem_master(ids::EDITION_DISPLAY, &body)
}

/// A chapter atom with a UID, a start time, an optional skip type, and
/// optional nested children (pre-encoded atom bytes).
fn chapter_atom(uid: u64, start_ns: u64, skip: Option<u64>, children: &[Vec<u8>]) -> Vec<u8> {
    let mut atom = Vec::new();
    atom.extend_from_slice(&elem_uint(ids::CHAPTER_UID, uid));
    atom.extend_from_slice(&elem_uint(ids::CHAPTER_TIME_START, start_ns));
    if let Some(v) = skip {
        atom.extend_from_slice(&elem_uint(ids::CHAPTER_SKIP_TYPE, v));
    }
    for child in children {
        atom.extend_from_slice(child);
    }
    elem_master(ids::CHAPTER_ATOM, &atom)
}

/// Build a self-contained v5 Matroska file:
///
/// * Track 1: audio (TrackUID 0x11) with an explicit `Emphasis` and two
///   `AttachmentLink`s (plus a spec-illegal `0` that must be dropped),
///   and a `BlockAdditionMapping` with `BlockAddIDValue` 2.
/// * Track 2: audio (TrackUID 0x22) with no `Emphasis` child and a
///   `BlockAdditionMapping` with `BlockAddIDValue` 3.
/// * Chapters: one edition with two `EditionDisplay`s (one well-formed
///   multi-language, one malformed with no `EditionString`, one with an
///   empty `EditionString`), and atoms carrying skip types incl. a
///   nested child and an out-of-enumeration value.
/// * Tags: the four joint-matrix shapes for `TagBlockAddIDValue`.
fn build_v5_mkv(emphasis: Option<u64>) -> Vec<u8> {
    let mut ebml_body = Vec::new();
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_READ_VERSION, 1));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_ID_LENGTH, 4));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_MAX_SIZE_LENGTH, 8));
    ebml_body.extend_from_slice(&elem_str(ids::EBML_DOC_TYPE, "matroska"));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_VERSION, 5));
    ebml_body.extend_from_slice(&elem_uint(ids::EBML_DOC_TYPE_READ_VERSION, 2));
    let ebml_header = elem_master(ids::EBML_HEADER, &ebml_body);

    let mut info_body = Vec::new();
    info_body.extend_from_slice(&elem_uint(ids::TIMECODE_SCALE, 1_000_000));
    let info = elem_master(ids::INFO, &info_body);

    // Track 1 — audio with Emphasis + AttachmentLinks + mapping value 2.
    let mut t1 = Vec::new();
    t1.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 1));
    t1.extend_from_slice(&elem_uint(ids::TRACK_UID, 0x11));
    t1.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    t1.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    // Errata 8615: several AttachmentLink elements; the 0 is spec-illegal
    // (range "not 0") and must be dropped.
    t1.extend_from_slice(&elem_uint(ids::ATTACHMENT_LINK, 7));
    t1.extend_from_slice(&elem_uint(ids::ATTACHMENT_LINK, 0));
    t1.extend_from_slice(&elem_uint(ids::ATTACHMENT_LINK, 9));
    let mut bam = Vec::new();
    bam.extend_from_slice(&elem_uint(ids::BLOCK_ADD_ID_VALUE, 2));
    t1.extend_from_slice(&elem_master(ids::BLOCK_ADDITION_MAPPING, &bam));
    let mut audio1 = Vec::new();
    audio1.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio1.extend_from_slice(&elem_uint(ids::CHANNELS, 2));
    if let Some(e) = emphasis {
        audio1.extend_from_slice(&elem_uint(ids::EMPHASIS, e));
    }
    t1.extend_from_slice(&elem_master(ids::AUDIO, &audio1));
    let track1 = elem_master(ids::TRACK_ENTRY, &t1);

    // Track 2 — audio, no Emphasis, mapping value 3.
    let mut t2 = Vec::new();
    t2.extend_from_slice(&elem_uint(ids::TRACK_NUMBER, 2));
    t2.extend_from_slice(&elem_uint(ids::TRACK_UID, 0x22));
    t2.extend_from_slice(&elem_uint(ids::TRACK_TYPE, ids::TRACK_TYPE_AUDIO));
    t2.extend_from_slice(&elem_str(ids::CODEC_ID, "A_PCM/INT/LIT"));
    let mut bam2 = Vec::new();
    bam2.extend_from_slice(&elem_uint(ids::BLOCK_ADD_ID_VALUE, 3));
    t2.extend_from_slice(&elem_master(ids::BLOCK_ADDITION_MAPPING, &bam2));
    let mut audio2 = Vec::new();
    audio2.extend_from_slice(&elem_float_be_f64(ids::SAMPLING_FREQUENCY, 48_000.0));
    audio2.extend_from_slice(&elem_uint(ids::CHANNELS, 1));
    t2.extend_from_slice(&elem_master(ids::AUDIO, &audio2));
    let track2 = elem_master(ids::TRACK_ENTRY, &t2);

    let mut tracks_body = Vec::new();
    tracks_body.extend_from_slice(&track1);
    tracks_body.extend_from_slice(&track2);
    let tracks = elem_master(ids::TRACKS, &tracks_body);

    // Chapters.
    let mut edition_body = Vec::new();
    edition_body.extend_from_slice(&elem_uint(ids::EDITION_UID, 0xED));
    // Well-formed: name + two BCP 47 tags.
    edition_body.extend_from_slice(&edition_display(Some("Director's Cut"), &["en", "en-US"]));
    // Malformed: no EditionString (minOccurs 1, no default) → dropped.
    edition_body.extend_from_slice(&edition_display(None, &["fr"]));
    // Present-but-empty string: legal per the schema → kept.
    edition_body.extend_from_slice(&edition_display(Some(""), &[]));
    // Atom 1: opening credits with a nested atom carrying a *different*
    // skip value (legal per the nesting rule).
    let nested = chapter_atom(0xC9, 0, Some(0), &[]);
    edition_body.extend_from_slice(&chapter_atom(0xC1, 0, Some(1), &[nested]));
    // Atom 2: no skip type (absence carries no assertion).
    edition_body.extend_from_slice(&chapter_atom(0xC2, 1_000_000_000, None, &[]));
    // Atom 3: out-of-enumeration value 9 → Unknown(9).
    edition_body.extend_from_slice(&chapter_atom(0xC3, 2_000_000_000, Some(9), &[]));
    let edition = elem_master(ids::EDITION_ENTRY, &edition_body);
    let chapters = elem_master(ids::CHAPTERS, &edition);

    // Tags — the four joint-matrix shapes.
    let simple_tag = |name: &str| {
        let mut st = Vec::new();
        st.extend_from_slice(&elem_str(ids::TAG_NAME, name));
        st.extend_from_slice(&elem_str(ids::TAG_STRING, "v"));
        elem_master(ids::SIMPLE_TAG, &st)
    };
    let tag = |targets_body: Vec<u8>, name: &str| {
        let mut t = Vec::new();
        t.extend_from_slice(&elem_master(ids::TARGETS, &targets_body));
        t.extend_from_slice(&simple_tag(name));
        elem_master(ids::TAG, &t)
    };
    let mut tags_body = Vec::new();
    // Cell 1: no selector, no track scope (explicit zeros — semantically
    // identical to omission).
    let mut tg = Vec::new();
    tg.extend_from_slice(&elem_uint(ids::TAG_BLOCK_ADD_ID_VALUE, 0));
    tags_body.extend_from_slice(&tag(tg, "ALL_MAPPINGS"));
    // Cell 2: no selector, track scope = track 1.
    let mut tg = Vec::new();
    tg.extend_from_slice(&elem_uint(ids::TAG_TRACK_UID, 0x11));
    tags_body.extend_from_slice(&tag(tg, "TRACK1_MAPPINGS"));
    // Cell 3: selector 2, no track scope.
    let mut tg = Vec::new();
    tg.extend_from_slice(&elem_uint(ids::TAG_BLOCK_ADD_ID_VALUE, 2));
    tags_body.extend_from_slice(&tag(tg, "VALUE2_ANYWHERE"));
    // Cell 4: selector 3 + track 2 (the value matches a mapping there —
    // the usage-note MUST holds for this writer).
    let mut tg = Vec::new();
    tg.extend_from_slice(&elem_uint(ids::TAG_TRACK_UID, 0x22));
    tg.extend_from_slice(&elem_uint(ids::TAG_BLOCK_ADD_ID_VALUE, 3));
    tags_body.extend_from_slice(&tag(tg, "TRACK2_VALUE3"));
    let tags = elem_master(ids::TAGS, &tags_body);

    let mut cluster_body = Vec::new();
    cluster_body.extend_from_slice(&elem_uint(ids::TIMECODE, 0));
    let mut sb = Vec::new();
    sb.extend_from_slice(&write_vint(1, 0));
    sb.extend_from_slice(&0i16.to_be_bytes());
    sb.push(0x80);
    sb.push(0xAA);
    let mut block = Vec::new();
    block.extend_from_slice(&write_element_id(ids::SIMPLE_BLOCK));
    block.extend_from_slice(&write_vint(sb.len() as u64, 0));
    block.extend_from_slice(&sb);
    cluster_body.extend_from_slice(&block);
    let cluster = elem_master(ids::CLUSTER, &cluster_body);

    let mut seg_body = Vec::new();
    seg_body.extend_from_slice(&info);
    seg_body.extend_from_slice(&tracks);
    seg_body.extend_from_slice(&chapters);
    seg_body.extend_from_slice(&tags);
    seg_body.extend_from_slice(&cluster);
    let segment = elem_master(ids::SEGMENT, &seg_body);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_header);
    out.extend_from_slice(&segment);
    out
}

fn demux(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open_typed")
}

#[test]
fn edition_displays_surface_with_languages() {
    let dmx = demux(build_v5_mkv(None));
    let ed = &dmx.chapters()[0];
    // Malformed (string-less) master dropped; the well-formed and the
    // empty-string masters survive, in on-disk order.
    assert_eq!(ed.displays.len(), 2, "malformed EditionDisplay dropped");
    assert_eq!(ed.displays[0].string, "Director's Cut");
    assert_eq!(ed.displays[0].languages, vec!["en", "en-US"]);
    // Present-but-empty EditionString is legal and kept; no `und`
    // fallback is synthesised for the language-less entry.
    assert_eq!(ed.displays[1].string, "");
    assert!(ed.displays[1].languages.is_empty());
}

#[test]
fn chapter_skip_types_surface_without_default() {
    let dmx = demux(build_v5_mkv(None));
    let atoms = &dmx.chapters()[0].chapters;
    assert_eq!(atoms.len(), 3);
    // Atom 1: OpeningCredits, nested child NoSkipping (a *different*
    // value from the parent — legal; and Some(NoSkipping) is distinct
    // from absence).
    assert_eq!(atoms[0].skip_type, Some(ChapterSkipType::OpeningCredits));
    assert_eq!(
        atoms[0].children[0].skip_type,
        Some(ChapterSkipType::NoSkipping)
    );
    // Atom 2: absent — no default materialised.
    assert_eq!(atoms[1].skip_type, None);
    // Atom 3: out-of-enumeration 9 → Unknown, not an error.
    assert_eq!(atoms[2].skip_type, Some(ChapterSkipType::Unknown(9)));
}

#[test]
fn chapter_skip_type_enum_round_trips() {
    for v in 0..=7u64 {
        let e = ChapterSkipType::from_raw(v);
        assert!(!matches!(e, ChapterSkipType::Unknown(_)), "value {v} known");
        assert_eq!(e.to_raw(), v);
    }
    assert_eq!(ChapterSkipType::from_raw(8).to_raw(), 8);
    assert!(matches!(
        ChapterSkipType::from_raw(8),
        ChapterSkipType::Unknown(8)
    ));
    // is_skippable: every known value except NoSkipping; Unknown never.
    assert!(!ChapterSkipType::NoSkipping.is_skippable());
    assert!(ChapterSkipType::OpeningCredits.is_skippable());
    assert!(ChapterSkipType::Intermission.is_skippable());
    assert!(!ChapterSkipType::Unknown(8).is_skippable());
}

#[test]
fn emphasis_explicit_value_surfaces() {
    let dmx = demux(build_v5_mkv(Some(1)));
    let a = dmx.track_audio(0).expect("track 1 audio");
    assert_eq!(a.emphasis(), AudioEmphasis::CdAudio);
    assert_eq!(a.emphasis_explicit(), Some(AudioEmphasis::CdAudio));
    assert!(a.emphasis().needs_deemphasis());
    // Track 2 carried no Emphasis child: the mandatory-but-defaulted `0`
    // is materialised on `emphasis()` while the on-disk absence stays
    // observable.
    let b = dmx.track_audio(1).expect("track 2 audio");
    assert_eq!(b.emphasis(), AudioEmphasis::NoEmphasis);
    assert_eq!(b.emphasis_explicit(), None);
}

#[test]
fn emphasis_explicit_zero_is_distinct_from_absence() {
    let dmx = demux(build_v5_mkv(Some(0)));
    let a = dmx.track_audio(0).expect("track 1 audio");
    assert_eq!(a.emphasis(), AudioEmphasis::NoEmphasis);
    assert_eq!(a.emphasis_explicit(), Some(AudioEmphasis::NoEmphasis));
}

#[test]
fn emphasis_enum_is_non_contiguous_and_closed() {
    // Assigned values round-trip.
    for v in [0u64, 1, 2, 3, 4, 5, 10, 11, 12, 13, 14, 15, 16] {
        let e = AudioEmphasis::from_raw(v);
        assert!(!matches!(e, AudioEmphasis::Unknown(_)), "value {v} known");
        assert_eq!(e.to_raw(), v);
    }
    // The deliberate gap 6..=9 and 17+ are unassigned.
    for v in [6u64, 7, 8, 9, 17, 255] {
        assert!(matches!(AudioEmphasis::from_raw(v), AudioEmphasis::Unknown(u) if u == v));
    }
    // needs_deemphasis: known filters yes; NoEmphasis / Reserved /
    // Unknown no (they name no filter to invert).
    assert!(AudioEmphasis::PhonoRiaa.needs_deemphasis());
    assert!(AudioEmphasis::CcitJ17.needs_deemphasis());
    assert!(!AudioEmphasis::NoEmphasis.needs_deemphasis());
    assert!(!AudioEmphasis::Reserved.needs_deemphasis());
    assert!(!AudioEmphasis::Unknown(9).needs_deemphasis());
}

#[test]
fn tag_block_add_id_values_surface_verbatim() {
    let dmx = demux(build_v5_mkv(None));
    let tags = dmx.tags();
    assert_eq!(tags.len(), 4);
    // Explicit zero is kept verbatim (empty list ≡ single 0 semantically,
    // but the on-disk shape survives for re-mux).
    assert_eq!(tags[0].targets.block_add_id_values, vec![0]);
    assert!(!tags[0].targets.block_addition_scoped());
    assert!(tags[1].targets.block_add_id_values.is_empty());
    assert_eq!(tags[2].targets.block_add_id_values, vec![2]);
    assert!(tags[2].targets.block_addition_scoped());
    assert_eq!(tags[3].targets.block_add_id_values, vec![3]);
}

#[test]
fn joint_matrix_resolution() {
    let dmx = demux(build_v5_mkv(None));
    let tags = dmx.tags();
    // Streams: track 1 (UID 0x11) = stream 0 with mapping value 2;
    // track 2 (UID 0x22) = stream 1 with mapping value 3.
    //
    // Cell 1 (zero selector, no track scope): applies to every mapping
    // in the Segment.
    assert!(tags[0].targets.applies_to_block_addition(0, 2));
    assert!(tags[0].targets.applies_to_block_addition(1, 3));
    // Cell 2 (no selector, track scope = stream 0): all mappings on that
    // track only.
    assert!(tags[1].targets.applies_to_block_addition(0, 2));
    assert!(!tags[1].targets.applies_to_block_addition(1, 3));
    // Cell 3 (selector 2, no track scope): value-2 mappings anywhere;
    // an unmatched value selects nothing (not an error).
    assert!(tags[2].targets.applies_to_block_addition(0, 2));
    assert!(!tags[2].targets.applies_to_block_addition(1, 3));
    assert!(!tags[2].targets.applies_to_block_addition(0, 3));
    // Cell 4 (selector 3 + track scope stream 1): exactly that mapping.
    assert!(tags[3].targets.applies_to_block_addition(1, 3));
    assert!(!tags[3].targets.applies_to_block_addition(0, 3));
    assert!(!tags[3].targets.applies_to_block_addition(1, 2));
    // A selector of 1 can never match a real mapping (BlockAddIDValue is
    // ranged >= 2) — and never matches here either.
    assert!(!tags[3].targets.applies_to_block_addition(1, 1));

    // Demuxer-level convenience: mapping (stream 1, value 3) is matched
    // by the wildcard cell-1 tag and the cell-4 tag.
    let hits = dmx.tags_for_block_addition_mapping(1, 3);
    let names: Vec<&str> = hits
        .iter()
        .map(|t| t.simple_tags[0].name.as_str())
        .collect();
    assert_eq!(names, vec!["ALL_MAPPINGS", "TRACK2_VALUE3"]);
}

#[test]
fn attachment_links_are_a_list_per_erratum_8615() {
    let dmx = demux(build_v5_mkv(None));
    let id = dmx.track_identity(0).expect("track identity");
    // Both non-zero links, in on-disk order; the spec-illegal 0 dropped.
    assert_eq!(id.attachment_links(), &[7, 9]);
    // The singular accessor keeps its historical shape (= first).
    assert_eq!(id.attachment_link(), Some(7));
    let id2 = dmx.track_identity(1).expect("track 2 identity");
    assert!(id2.attachment_links().is_empty());
    assert_eq!(id2.attachment_link(), None);
}

#[test]
fn skip_type_at_applies_the_implicit_range_rule() {
    use oxideav_mkv::demux::{Chapter, Edition};
    let atom = |start: u64, end: Option<u64>, skip: Option<ChapterSkipType>| Chapter {
        time_start_ns: start,
        time_end_ns: end,
        skip_type: skip,
        ..Chapter::default()
    };
    let s = 1_000_000_000u64; // one second in Matroska Ticks
    let ed = Edition {
        chapters: vec![
            // Governing, end-less: runs until the *next governing* atom.
            atom(0, None, Some(ChapterSkipType::OpeningCredits)),
            // Non-governing: carries no assertion and does NOT terminate
            // the predecessor's open-ended range.
            atom(10 * s, None, None),
            // Governing, bounded: [20 s, 25 s).
            atom(20 * s, Some(25 * s), Some(ChapterSkipType::Recap)),
            // Trailing non-governing atom.
            atom(30 * s, None, None),
        ],
        ..Edition::default()
    };
    // Inside the open-ended first range — including past the
    // non-governing atom at 10 s.
    assert_eq!(
        ed.skip_type_at(5 * s),
        Some(ChapterSkipType::OpeningCredits)
    );
    assert_eq!(
        ed.skip_type_at(15 * s),
        Some(ChapterSkipType::OpeningCredits)
    );
    // The next governing atom (20 s) terminates it and governs its own
    // bounded range.
    assert_eq!(ed.skip_type_at(22 * s), Some(ChapterSkipType::Recap));
    // Past the bounded end: nothing governs (the first range was
    // terminated at 20 s, the second ended at 25 s).
    assert_eq!(ed.skip_type_at(26 * s), None);
    assert_eq!(ed.skip_type_at(24 * s), Some(ChapterSkipType::Recap));

    // A file whose only governing atom is end-less classifies to EOF.
    let ed2 = Edition {
        chapters: vec![atom(3 * s, None, Some(ChapterSkipType::EndCredits))],
        ..Edition::default()
    };
    assert_eq!(ed2.skip_type_at(2 * s), None, "before the range");
    assert_eq!(
        ed2.skip_type_at(1_000_000 * s),
        Some(ChapterSkipType::EndCredits),
        "open-ended range runs to EOF"
    );

    // Overlap tie-break: a bounded governing atom overlapped by a later
    // governing atom — the latest-starting one wins (documented Reader
    // choice; the staged text names no precedence).
    let ed3 = Edition {
        chapters: vec![
            atom(0, Some(30 * s), Some(ChapterSkipType::Advertisement)),
            atom(10 * s, Some(12 * s), Some(ChapterSkipType::Preview)),
        ],
        ..Edition::default()
    };
    assert_eq!(ed3.skip_type_at(11 * s), Some(ChapterSkipType::Preview));
    assert_eq!(
        ed3.skip_type_at(13 * s),
        Some(ChapterSkipType::Advertisement),
        "outer bounded range resumes after the overlap"
    );

    // Pre-v5 file: no atom governs anywhere.
    let ed4 = Edition {
        chapters: vec![atom(0, Some(10 * s), None)],
        ..Edition::default()
    };
    assert_eq!(ed4.skip_type_at(5 * s), None);
}

#[test]
fn v5_fuzz_seed_is_a_valid_v5_document() {
    // The fuzz corpus seed carrying all six v5 elements must stay a
    // *valid* document (zero schema violations, DocTypeVersion 5, every
    // v5 surface populated) so mutation fuzzing explores the v5 parse
    // arms from a well-formed starting point rather than dying at open.
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fuzz/corpus/demux/seed_v5_elements.mkv"
    );
    let bytes = std::fs::read(path).expect("v5 fuzz seed present");
    let report =
        oxideav_mkv::schema::validate(&mut Cursor::new(&bytes)).expect("validator walks the seed");
    assert_eq!(report.doc_type_version, Some(5));
    assert_eq!(report.violations, 0, "{:?}", report.findings);

    let dmx = demux(bytes);
    assert_eq!(dmx.ebml_header().doc_type_version, 5);
    let ed = &dmx.chapters()[0];
    assert_eq!(ed.displays.len(), 1);
    assert_eq!(ed.displays[0].languages.len(), 2);
    assert_eq!(
        ed.chapters[0].skip_type,
        Some(ChapterSkipType::OpeningCredits)
    );
    assert_eq!(
        ed.chapters[0].children[0].skip_type,
        Some(ChapterSkipType::NoSkipping)
    );
    assert_eq!(
        ed.chapters[1].skip_type,
        Some(ChapterSkipType::Intermission)
    );
    let a = dmx.track_audio(0).expect("audio record");
    assert_eq!(a.emphasis_explicit(), Some(AudioEmphasis::CdAudio));
    // Single AttachmentLink in the seed: the staged XML still caps
    // maxOccurs at 1 (errata 8615 is Reported, not merged), and the
    // seed stays violation-free against the transcription-faithful
    // validator. The multi-occurrence list surface is covered by the
    // hand-built fixture above.
    assert_eq!(
        dmx.track_identity(0).expect("identity").attachment_links(),
        &[7]
    );
    let tags = dmx.tags();
    assert_eq!(tags[0].targets.block_add_id_values, vec![2]);
    assert!(tags[0].targets.applies_to_block_addition(0, 2));
}
