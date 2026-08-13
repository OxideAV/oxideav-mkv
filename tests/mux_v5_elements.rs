//! Write-side tests for the six Matroska **v5** elements (staged
//! `docs/container/matroska/post-rfc9559-elements.md`): the muxer parses
//! all six on the demux side unconditionally, but *writes* them only
//! when explicitly queued — and any emitted v5 element flips the EBML
//! header's `DocTypeVersion` from `4` to `5` (the v5 draft text: files
//! carrying `minver: 5` elements MUST declare `DocTypeVersion >= 5`).
//!
//! Contracts pinned here:
//!
//! 1. Nothing queued → `DocTypeVersion 4` on disk (the conservative
//!    default: "parse all six, write none of them by default").
//! 2. A full v5 surface (EditionDisplay + ChapterSkipType + Emphasis +
//!    TagBlockAddIDValue) round-trips through the production demuxer and
//!    declares `DocTypeVersion 5`; the whole-document schema validator
//!    reports zero violations on the output.
//! 3. `Emphasis` queued as `NoEmphasis` stays off-disk and does **not**
//!    force v5 (emitting `Emphasis=0` would needlessly upgrade an
//!    otherwise-v4 file).
//! 4. Closed enumerations are enforced at queue time: `Reserved` /
//!    `Unknown` emphasis and `Unknown` skip types are rejected.
//! 5. WebM rejects all four v5 write surfaces (none of the six is a WebM
//!    element, and `DocTypeVersion 5` is undefined for the webm
//!    DocType) — with no lenient opt-out.
//! 6. `EditionDisplay` without chapters fails `write_header` instead of
//!    silently dropping; non-ASCII `EditionLanguageIETF` tags are
//!    rejected at queue time.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, Muxer, Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_mkv::demux::{AudioEmphasis, ChapterSkipType};
use oxideav_mkv::mux::{MkvChapter, MkvEditionDisplay, MkvMuxer, MkvTag, MkvTrackAudio};

fn assert_err<T>(r: Result<T, Error>, msg: &str) -> Error {
    match r {
        Ok(_) => panic!("{msg}: expected Err, got Ok"),
        Err(e) => e,
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r442-v5-{}-{}-{n}.mka",
        tag,
        std::process::id()
    ))
}

fn audio_stream(codec: &str) -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new(codec));
    p.sample_rate = Some(48_000);
    p.channels = Some(2);
    p.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn audio_packet(stream: u32, pts: i64) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![0x5A; 64]);
    p.pts = Some(pts);
    p.flags.keyframe = true;
    p
}

/// Mux a single-track Matroska file; `configure` runs before
/// `write_header`. Returns the muxed bytes.
fn mux<F>(configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mx =
            MkvMuxer::new_matroska(ws, &[audio_stream("pcm_s16le")]).expect("muxer construct");
        configure(&mut mx);
        mx.write_header().expect("write_header");
        mx.write_packet(&audio_packet(0, 0)).expect("packet");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn demux(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open_typed")
}

fn webm_muxer() -> MkvMuxer {
    let tmp = tmp_path("webm");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    MkvMuxer::new_webm(ws, &[audio_stream("vorbis")]).expect("webm muxer construct")
}

#[test]
fn no_v5_elements_keeps_doc_type_version_4() {
    // The conservative default: a file with chapters / tags / audio but
    // no v5 surface stays v4 on disk.
    let dmx = demux(mux(|mx| {
        mx.add_chapter(0, Some(1_000_000_000), "Intro").unwrap();
        mx.add_tag(MkvTag::global("TITLE", "t")).unwrap();
    }));
    let h = dmx.ebml_header();
    assert_eq!(h.doc_type_version, 4);
    assert_eq!(h.doc_type_read_version, 2);
}

#[test]
fn full_v5_surface_round_trips_and_declares_version_5() {
    let bytes = mux(|mx| {
        mx.add_chapter_full(MkvChapter {
            time_start_ns: 0,
            time_end_ns: Some(30_000_000_000),
            skip_type: Some(ChapterSkipType::OpeningCredits),
            ..Default::default()
        })
        .unwrap();
        mx.add_chapter_full(MkvChapter {
            time_start_ns: 30_000_000_000,
            skip_type: None,
            ..Default::default()
        })
        .unwrap();
        mx.set_edition_displays(vec![
            MkvEditionDisplay {
                string: "Director's Cut".into(),
                languages: vec!["en".into(), "en-US".into()],
            },
            MkvEditionDisplay {
                string: "".into(),
                languages: vec![],
            },
        ])
        .unwrap();
        mx.set_track_audio(
            0,
            MkvTrackAudio {
                emphasis: Some(AudioEmphasis::CdAudio),
                ..Default::default()
            },
        )
        .unwrap();
        let mut tag = MkvTag::global("MAPPING_NOTE", "v");
        tag.targets.track_uids = vec![1];
        // Zeros are wildcards dropped at write time; the 2 survives.
        tag.targets.block_add_id_values = vec![0, 2];
        mx.add_tag(tag).unwrap();
    });

    // The whole-document schema validator sees zero violations — in
    // particular no VersionMismatch (DocTypeVersion 5 covers minver 5)
    // and no ChapterSkipTypeNesting (flat chapter list).
    let report = oxideav_mkv::schema::validate(&mut Cursor::new(&bytes)).expect("validate");
    assert_eq!(report.doc_type_version, Some(5));
    assert_eq!(
        report.violations, 0,
        "schema violations on muxer output: {:?}",
        report.findings
    );
    assert_eq!(report.informational, 0, "{:?}", report.findings);

    let dmx = demux(bytes);
    assert_eq!(dmx.ebml_header().doc_type_version, 5);
    assert_eq!(dmx.ebml_header().doc_type_read_version, 2);

    let ed = &dmx.chapters()[0];
    assert_eq!(ed.displays.len(), 2);
    assert_eq!(ed.displays[0].string, "Director's Cut");
    assert_eq!(ed.displays[0].languages, vec!["en", "en-US"]);
    assert_eq!(ed.displays[1].string, "");
    assert!(ed.displays[1].languages.is_empty());
    assert_eq!(
        ed.chapters[0].skip_type,
        Some(ChapterSkipType::OpeningCredits)
    );
    assert_eq!(ed.chapters[1].skip_type, None);

    let a = dmx.track_audio(0).expect("audio record");
    assert_eq!(a.emphasis(), AudioEmphasis::CdAudio);
    assert_eq!(a.emphasis_explicit(), Some(AudioEmphasis::CdAudio));

    let tags = dmx.tags();
    assert_eq!(tags.len(), 1);
    // The zero wildcard stayed off-disk; only the concrete selector
    // round-trips.
    assert_eq!(tags[0].targets.block_add_id_values, vec![2]);
    assert!(tags[0].targets.applies_to_block_addition(0, 2));
    assert!(!tags[0].targets.applies_to_block_addition(0, 3));
}

#[test]
fn no_emphasis_hint_stays_v4_and_off_disk() {
    // Some(NoEmphasis) behaves like omission: no Emphasis element, no v5
    // header bump — the staged note says a writer must omit the value-0
    // element rather than force DocTypeVersion 5.
    let dmx = demux(mux(|mx| {
        mx.set_track_audio(
            0,
            MkvTrackAudio {
                emphasis: Some(AudioEmphasis::NoEmphasis),
                ..Default::default()
            },
        )
        .unwrap();
    }));
    assert_eq!(dmx.ebml_header().doc_type_version, 4);
    let a = dmx.track_audio(0).expect("audio record");
    assert_eq!(a.emphasis(), AudioEmphasis::NoEmphasis);
    assert_eq!(a.emphasis_explicit(), None, "element stayed off-disk");
}

#[test]
fn closed_enumerations_rejected_at_queue_time() {
    let mut mx = MkvMuxer::new_matroska(
        Box::new(std::fs::File::create(tmp_path("enum")).unwrap()),
        &[audio_stream("pcm_s16le")],
    )
    .unwrap();
    // Emphasis 2 is reserved — a writer must not emit it.
    assert_err(
        mx.set_track_audio(
            0,
            MkvTrackAudio {
                emphasis: Some(AudioEmphasis::Reserved),
                ..Default::default()
            },
        ),
        "reserved emphasis",
    );
    // Emphasis 6..=9 / 17+ are unassigned.
    assert_err(
        mx.set_track_audio(
            0,
            MkvTrackAudio {
                emphasis: Some(AudioEmphasis::Unknown(7)),
                ..Default::default()
            },
        ),
        "unassigned emphasis",
    );
    // ChapterSkipType 8+ is outside the closed 0..=7 enumeration.
    assert_err(
        mx.add_chapter_full(MkvChapter {
            skip_type: Some(ChapterSkipType::Unknown(8)),
            ..Default::default()
        }),
        "unknown skip type",
    );
    // Intermission (7) — the post-RFC addition — is writable.
    mx.add_chapter_full(MkvChapter {
        skip_type: Some(ChapterSkipType::Intermission),
        ..Default::default()
    })
    .unwrap();
}

#[test]
fn webm_rejects_all_v5_write_surfaces() {
    let mut mx = webm_muxer();
    assert_err(
        mx.set_edition_displays(vec![MkvEditionDisplay::new("Cut", "en")]),
        "webm edition displays",
    );
    assert_err(
        mx.add_chapter_full(MkvChapter {
            skip_type: Some(ChapterSkipType::EndCredits),
            ..Default::default()
        }),
        "webm skip type",
    );
    assert_err(
        mx.set_track_audio(
            0,
            MkvTrackAudio {
                emphasis: Some(AudioEmphasis::Fm50),
                ..Default::default()
            },
        ),
        "webm emphasis",
    );
    let mut tag = MkvTag::global("N", "v");
    tag.targets.block_add_id_values = vec![2];
    assert_err(mx.add_tag(tag), "webm tag selector");

    // No lenient opt-out: DocTypeVersion 5 is undefined for the webm
    // DocType, so with_webm_lenient() does not unlock the v5 surface.
    let mut mx = webm_muxer();
    mx.with_webm_lenient().unwrap();
    assert_err(
        mx.set_edition_displays(vec![MkvEditionDisplay::new("Cut", "en")]),
        "lenient webm edition displays",
    );

    // NoEmphasis is omission-equivalent and legal even on WebM.
    let mut mx = webm_muxer();
    mx.set_track_audio(
        0,
        MkvTrackAudio {
            emphasis: Some(AudioEmphasis::NoEmphasis),
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn edition_displays_without_chapters_fail_write_header() {
    let f = std::fs::File::create(tmp_path("nochap")).unwrap();
    let mut mx = MkvMuxer::new_matroska(Box::new(f), &[audio_stream("pcm_s16le")]).unwrap();
    mx.set_edition_displays(vec![MkvEditionDisplay::new("Cut", "en")])
        .unwrap();
    assert_err(
        mx.write_header(),
        "EditionDisplay without chapters must not be silently dropped",
    );
}

#[test]
fn non_ascii_edition_language_rejected() {
    let f = std::fs::File::create(tmp_path("lang")).unwrap();
    let mut mx = MkvMuxer::new_matroska(Box::new(f), &[audio_stream("pcm_s16le")]).unwrap();
    // EditionLanguageIETF is an EBML `string` (printable ASCII); BCP 47
    // tags are ASCII by construction, so a non-ASCII tag is malformed.
    assert_err(
        mx.set_edition_displays(vec![MkvEditionDisplay::new("Cut", "français")]),
        "non-ASCII language tag",
    );
}
