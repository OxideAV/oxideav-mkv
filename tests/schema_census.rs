//! Census + cross-checks for the machine-readable element schema
//! (`oxideav_mkv::schema`), transcribed from the staged
//! `ebml_matroska.xml` (IETF CELLAR EBML Schema for Matroska).
//!
//! Pins the table's shape (row counts, sortedness, version windows,
//! recursion markers), the structural consistency of every row's
//! path/parent derivation, both directions of the `ids.rs` census
//! (modulo the documented post-RFC and absent-by-design exception
//! sets), and the relationship between the schema's `webm="1"`
//! extension markers and the WebM *guidelines* support table.

use oxideav_mkv::ids;
use oxideav_mkv::schema::{
    element_def, ElementDef, ElementType, EBML_SUPPLEMENT, NO_MAX_VER, SCHEMA,
};
use oxideav_mkv::webm::{webm_element_support, WebmSupport};

#[test]
fn schema_counts_and_sortedness() {
    assert_eq!(SCHEMA.len(), 262);
    for w in SCHEMA.windows(2) {
        assert!(
            w[0].id < w[1].id,
            "SCHEMA must be strictly ascending: 0x{:X} then 0x{:X}",
            w[0].id,
            w[1].id
        );
    }
    for w in EBML_SUPPLEMENT.windows(2) {
        assert!(w[0].id < w[1].id, "EBML_SUPPLEMENT must be sorted");
    }
    // No overlap between the two tables.
    for e in EBML_SUPPLEMENT {
        assert!(
            SCHEMA.binary_search_by_key(&e.id, |x| x.id).is_err(),
            "0x{:X} in both tables",
            e.id
        );
    }
    fn count(f: impl Fn(&ElementDef) -> bool) -> usize {
        SCHEMA.iter().filter(|e| f(e)).count()
    }
    assert_eq!(
        count(|e: &ElementDef| e.element_type == ElementType::Master),
        49
    );
    assert_eq!(
        count(|e: &ElementDef| e.webm),
        133,
        "webmproject.org webm=1 markers"
    );
    assert_eq!(
        count(|e: &ElementDef| e.max_ver == 0),
        43,
        "maxver=0 = the Reclaimed set"
    );
    assert_eq!(
        count(|e: &ElementDef| e.min_ver == 5),
        6,
        "post-RFC v5 elements"
    );
    assert_eq!(
        count(|e: &ElementDef| e.recursive),
        2,
        "ChapterAtom + SimpleTag"
    );
    assert_eq!(
        count(|e: &ElementDef| e.unknown_size_allowed),
        2,
        "Segment + Cluster"
    );
    assert_eq!(count(|e: &ElementDef| e.recurring), 3);
    // Every range/length string the table carries is from the schema's
    // small constraint vocabulary — a regeneration that introduces new
    // spellings must extend the validator, so pin the counts.
    let ranges: Vec<&str> = SCHEMA.iter().filter_map(|e| e.range).collect();
    assert_eq!(ranges.len(), 66);
    let rcount = |s: &str| ranges.iter().filter(|r| **r == s).count();
    assert_eq!(rcount("not 0"), 27);
    assert_eq!(rcount("0-1"), 17);
    assert_eq!(rcount("0x0p+0-0x1p+0"), 8);
    assert_eq!(rcount("> 0x0p+0"), 6);
    assert_eq!(rcount(">= 0x0p+0"), 2);
    assert_eq!(rcount(">= -0xB4p+0, <= 0xB4p+0"), 2);
    assert_eq!(rcount(">= -0x5Ap+0, <= 0x5Ap+0"), 1);
    assert_eq!(rcount(">=2"), 1);
    assert_eq!(rcount("4"), 1);
    assert_eq!(rcount("1-8"), 1);
    let lengths: Vec<&str> = SCHEMA.iter().filter_map(|e| e.length).collect();
    assert_eq!(lengths.iter().filter(|l| **l == "16").count(), 7);
    assert_eq!(lengths.iter().filter(|l| **l == "4").count(), 2);
}

#[test]
fn paths_and_parents_are_consistent() {
    for e in SCHEMA {
        let clean = e.path.replace('+', "");
        match e.parent_id {
            None => assert_eq!(e.name, "Segment", "only the Root has no parent"),
            Some(pid) => {
                let parent = element_def(pid)
                    .unwrap_or_else(|| panic!("{}: dangling parent 0x{pid:X}", e.name));
                assert_eq!(
                    parent.element_type,
                    ElementType::Master,
                    "{}: parent {} is not a master",
                    e.name,
                    parent.name
                );
                let parent_clean = parent.path.replace('+', "");
                assert_eq!(
                    clean.rsplit_once('\\').map(|(head, _)| head),
                    Some(parent_clean.as_str()),
                    "{}: path/parent disagree",
                    e.name
                );
            }
        }
        // The recursive marker matches the path's own '+' component.
        let last_component = e.path.rsplit('\\').next().unwrap();
        assert_eq!(e.recursive, last_component.starts_with('+'), "{}", e.name);
    }
}

/// A `pub const NAME: u32 = 0x...;` row of `src/ids.rs`.
type NumericConst = (String, u32);
/// A `pub const ALIAS: u32 = TARGET;` row of `src/ids.rs`.
type AliasConst = (String, String);

/// Parse the `pub const NAME: u32 = ...;` surface of `src/ids.rs` (same
/// approach as the RFC 9559 registry census).
fn parse_ids_rs() -> (Vec<NumericConst>, Vec<AliasConst>) {
    let src = include_str!("../src/ids.rs");
    let mut numeric = Vec::new();
    let mut aliases = Vec::new();
    for line in src.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("pub const ") else {
            continue;
        };
        let Some((name, value)) = rest.split_once(": u32 = ") else {
            continue;
        };
        let Some(value) = value.strip_suffix(';') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if let Some(hex) = value.strip_prefix("0x") {
            let v = u32::from_str_radix(&hex.replace('_', ""), 16)
                .unwrap_or_else(|e| panic!("ids.rs: bad hex in `{line}`: {e}"));
            numeric.push((name.to_string(), v));
        } else {
            aliases.push((name.to_string(), value.to_string()));
        }
    }
    (numeric, aliases)
}

fn norm(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_' && *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[test]
fn every_schema_element_has_an_ids_const_or_is_post_rfc() {
    let (numeric, _) = parse_ids_rs();
    let values: Vec<u32> = numeric.iter().map(|(_, v)| *v).collect();
    let mut missing = Vec::new();
    for e in SCHEMA {
        if e.min_ver >= 5 {
            // Newer than the RFC 9559 registry `ids.rs` transcribes.
            continue;
        }
        if !values.contains(&e.id) {
            missing.push(format!("0x{:X} {}", e.id, e.name));
        }
    }
    assert!(
        missing.is_empty(),
        "schema elements (minver <= 4) with no ids.rs const: {missing:?}"
    );
}

#[test]
fn every_ids_const_is_in_schema_or_documented_absence() {
    let (numeric, _) = parse_ids_rs();
    // The Signature family is absent from the schema by design (see the
    // staged legacy-element-ids.md); ChapterFlagEnabled's schema row is
    // present (the schema kept it, maxver 0-ish window).
    let absent_by_design: &[u32] = &[
        ids::SIGNATURE_SLOT,
        ids::SIGNATURE_ALGO,
        ids::SIGNATURE_HASH,
        ids::SIGNATURE_PUBLIC_KEY,
        ids::SIGNATURE,
        ids::SIGNATURE_ELEMENTS,
        ids::SIGNATURE_ELEMENT_LIST,
        ids::SIGNED_ELEMENT,
    ];
    let mut rogue = Vec::new();
    for (name, v) in &numeric {
        if element_def(*v).is_some() || absent_by_design.contains(v) {
            continue;
        }
        rogue.push(format!("{name} = 0x{v:X}"));
    }
    assert!(
        rogue.is_empty(),
        "ids.rs consts with no schema row and no documented absence: {rogue:?}"
    );
}

#[test]
fn schema_names_agree_with_ids_const_names() {
    let (numeric, aliases) = parse_ids_rs();
    let mut by_value: Vec<(u32, String)> = numeric.iter().map(|(n, v)| (*v, norm(n))).collect();
    for (alias, target) in &aliases {
        if let Some((_, v)) = numeric.iter().find(|(n, _)| n == target) {
            by_value.push((*v, norm(alias)));
        }
    }
    // Crate const names that deliberately differ from the schema name
    // (same two disambiguation prefixes the registry census documents).
    let name_exceptions: &[(u32, &str)] = &[
        (0xCB, "TIMESLICE_BLOCK_ADDITION_ID"), // schema: BlockAdditionID
        (0x55B9, "COLOUR_RANGE"),              // schema: Range
    ];
    let mut mismatched = Vec::new();
    for e in SCHEMA {
        if e.min_ver >= 5 {
            continue;
        }
        if let Some((_, expected)) = name_exceptions.iter().find(|(eid, _)| *eid == e.id) {
            assert!(
                by_value
                    .iter()
                    .any(|(v, n)| *v == e.id && *n == norm(expected)),
                "0x{:X}: expected exception const {expected}",
                e.id
            );
            continue;
        }
        if !by_value
            .iter()
            .any(|(v, n)| *v == e.id && *n == norm(e.name))
        {
            let actual: Vec<&String> = by_value
                .iter()
                .filter(|(v, _)| *v == e.id)
                .map(|(_, n)| n)
                .collect();
            mismatched.push(format!(
                "0x{:X} schema `{}` vs consts {actual:?}",
                e.id, e.name
            ));
        }
    }
    assert!(
        mismatched.is_empty(),
        "const names disagreeing with schema names: {mismatched:?}"
    );
}

/// The schema's `webm="1"` extension markers corroborate the guidelines
/// support table: no schema element carries the marker while the
/// guidelines list it `Unsupported` / `Deprecated`, and exactly three
/// guidelines-`Supported` rows lack the marker (the two EBML-header
/// constraint rows, which carry no extensions at all, and
/// `AspectRatioType`, which the schema deprecated after the guidelines
/// snapshot was written).
#[test]
fn webm_markers_corroborate_guidelines_table() {
    let mut marker_but_unsupported = Vec::new();
    let mut supported_but_no_marker = Vec::new();
    for e in SCHEMA {
        match webm_element_support(e.id) {
            WebmSupport::Unsupported | WebmSupport::Deprecated if e.webm => {
                marker_but_unsupported.push(e.name);
            }
            WebmSupport::Supported if !e.webm => supported_but_no_marker.push(e.id),
            _ => {}
        }
    }
    assert!(
        marker_but_unsupported.is_empty(),
        "schema webm=1 on guidelines-off-profile elements: {marker_but_unsupported:?}"
    );
    supported_but_no_marker.sort_unstable();
    assert_eq!(
        supported_but_no_marker,
        vec![0x42F2, 0x42F3, 0x54B3],
        "EBMLMaxIDLength + EBMLMaxSizeLength + AspectRatioType"
    );
}

#[test]
fn element_def_spot_checks() {
    let ts = element_def(ids::TIMECODE_SCALE).expect("TimestampScale");
    assert_eq!(ts.name, "TimestampScale");
    assert_eq!(ts.element_type, ElementType::Uinteger);
    assert_eq!(ts.parent_id, Some(ids::INFO));
    assert_eq!(ts.range, Some("not 0"));
    assert_eq!(ts.default, Some("1000000"));
    assert!(ts.is_mandatory());

    let cluster = element_def(ids::CLUSTER).expect("Cluster");
    assert!(cluster.unknown_size_allowed);
    assert_eq!(cluster.min_occurs, 0, "zero-Cluster Segments are legal");

    let slices = element_def(ids::SLICES).expect("Slices");
    assert!(slices.is_deprecated());
    assert_eq!(slices.max_ver, 0);

    let atom = element_def(ids::CHAPTER_ATOM).expect("ChapterAtom");
    assert!(atom.recursive);

    let sf = element_def(ids::SAMPLING_FREQUENCY).expect("SamplingFrequency");
    assert_eq!(sf.element_type, ElementType::Float);
    assert_eq!(sf.default, Some("0x1.f4p+12"), "8000 Hz as a hex float");

    // The legacy chapter elements are in the schema...
    assert_eq!(
        element_def(ids::CHAPTER_TRACK_UID).map(|e| e.name),
        Some("ChapterTrackUID")
    );
    assert_eq!(
        element_def(ids::EDITION_FLAG_HIDDEN).map(|e| e.name),
        Some("EditionFlagHidden")
    );
    // ...the Signature family is absent by design.
    assert!(element_def(ids::SIGNATURE_SLOT).is_none());

    // EBML supplement: globals + header elements resolve.
    assert_eq!(element_def(ids::VOID).map(|e| e.name), Some("Void"));
    assert_eq!(element_def(ids::CRC32).map(|e| e.name), Some("CRC-32"));
    assert_eq!(element_def(ids::EBML_HEADER).map(|e| e.name), Some("EBML"));
    // The Matroska schema itself constrains the two EBML length fields.
    let maxid = element_def(ids::EBML_MAX_ID_LENGTH).expect("EBMLMaxIDLength");
    assert_eq!(maxid.range, Some("4"));
    assert_eq!(maxid.parent_id, Some(ids::EBML_HEADER));

    assert!(element_def(0x12345678).is_none());
    // The six post-RFC v5 elements resolve too.
    let mut v5: Vec<&str> = SCHEMA
        .iter()
        .filter(|e| e.min_ver == 5)
        .map(|e| e.name)
        .collect();
    v5.sort_unstable();
    assert_eq!(
        v5,
        vec![
            "ChapterSkipType",
            "EditionDisplay",
            "EditionLanguageIETF",
            "EditionString",
            "Emphasis",
            "TagBlockAddIDValue",
        ]
    );
    let _ = NO_MAX_VER;
}
