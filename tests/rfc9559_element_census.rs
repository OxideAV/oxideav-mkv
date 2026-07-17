//! RFC 9559 element-ID registry census (Table 53, Section 27.1).
//!
//! Cross-checks the crate's `src/ids.rs` element-ID surface against the
//! full "Matroska Element IDs" registry the RFC establishes, in both
//! directions, so the "every element in the RFC 9559 element-ID registry
//! is read and written" claim is pinned by CI rather than by prose:
//!
//! * every named registry entry (250 rows; the 4 all-ones `Reserved`
//!   placeholders excluded) has a matching `pub const` in `ids.rs`;
//! * every numeric `pub const` in `ids.rs` is either a registry entry,
//!   one of the 13 RFC 8794 EBML-header / EBML-global IDs (which the
//!   registry text explicitly keeps out: "EBML Element IDs defined for
//!   the EBML Header ... MUST NOT be used as Matroska Element IDs"),
//!   or the single documented out-of-registry legacy element
//!   `ChapterFlagEnabled` (`0x4598`) that RFC 9559 dropped from the
//!   schema but historical files still carry;
//! * const names agree with the registry's Element Names (modulo the
//!   crate's two deliberate disambiguation prefixes);
//! * no `Reserved` all-ones ID is defined as an element;
//! * every named registry ID sits inside the valid VINT ID classes of
//!   Section 27.1 (one-octet 0x80..=0xFE, two-octet 0x407F..=0x7FFE,
//!   three-octet 0x203FFF..=0x3FFFFE, four-octet
//!   0x101FFFFF..=0x1FFFFFFE).
//!
//! The registry table below is a transcription of RFC 9559 Table 53:
//! `(element id, element name, reclaimed)` — `reclaimed` marking the
//! Appendix A entries the registry keeps reserved for historical
//! reasons ("Reclaimed").

/// RFC 9559 Table 53, named entries only (`Reserved` rows excluded).
const REGISTRY: &[(u32, &str, bool)] = &[
    (0x80, "ChapterDisplay", false),
    (0x83, "TrackType", false),
    (0x85, "ChapString", false),
    (0x86, "CodecID", false),
    (0x88, "FlagDefault", false),
    (0x8E, "Slices", true),
    (0x91, "ChapterTimeStart", false),
    (0x92, "ChapterTimeEnd", false),
    (0x96, "CueRefTime", false),
    (0x97, "CueRefCluster", true),
    (0x98, "ChapterFlagHidden", false),
    (0x9A, "FlagInterlaced", false),
    (0x9B, "BlockDuration", false),
    (0x9C, "FlagLacing", false),
    (0x9D, "FieldOrder", false),
    (0x9F, "Channels", false),
    (0xA0, "BlockGroup", false),
    (0xA1, "Block", false),
    (0xA2, "BlockVirtual", true),
    (0xA3, "SimpleBlock", false),
    (0xA4, "CodecState", false),
    (0xA5, "BlockAdditional", false),
    (0xA6, "BlockMore", false),
    (0xA7, "Position", false),
    (0xAA, "CodecDecodeAll", true),
    (0xAB, "PrevSize", false),
    (0xAE, "TrackEntry", false),
    (0xAF, "EncryptedBlock", true),
    (0xB0, "PixelWidth", false),
    (0xB2, "CueDuration", false),
    (0xB3, "CueTime", false),
    (0xB5, "SamplingFrequency", false),
    (0xB6, "ChapterAtom", false),
    (0xB7, "CueTrackPositions", false),
    (0xB9, "FlagEnabled", false),
    (0xBA, "PixelHeight", false),
    (0xBB, "CuePoint", false),
    (0xC0, "TrickTrackUID", true),
    (0xC1, "TrickTrackSegmentUID", true),
    (0xC4, "TrickMasterTrackSegmentUID", true),
    (0xC6, "TrickTrackFlag", true),
    (0xC7, "TrickMasterTrackUID", true),
    (0xC8, "ReferenceFrame", true),
    (0xC9, "ReferenceOffset", true),
    (0xCA, "ReferenceTimestamp", true),
    (0xCB, "BlockAdditionID", true),
    (0xCC, "LaceNumber", true),
    (0xCD, "FrameNumber", true),
    (0xCE, "Delay", true),
    (0xCF, "SliceDuration", true),
    (0xD7, "TrackNumber", false),
    (0xDB, "CueReference", false),
    (0xE0, "Video", false),
    (0xE1, "Audio", false),
    (0xE2, "TrackOperation", false),
    (0xE3, "TrackCombinePlanes", false),
    (0xE4, "TrackPlane", false),
    (0xE5, "TrackPlaneUID", false),
    (0xE6, "TrackPlaneType", false),
    (0xE7, "Timestamp", false),
    (0xE8, "TimeSlice", true),
    (0xE9, "TrackJoinBlocks", false),
    (0xEA, "CueCodecState", false),
    (0xEB, "CueRefCodecState", true),
    (0xED, "TrackJoinUID", false),
    (0xEE, "BlockAddID", false),
    (0xF0, "CueRelativePosition", false),
    (0xF1, "CueClusterPosition", false),
    (0xF7, "CueTrack", false),
    (0xFA, "ReferencePriority", false),
    (0xFB, "ReferenceBlock", false),
    (0xFD, "ReferenceVirtual", true),
    (0x41A4, "BlockAddIDName", false),
    (0x41E4, "BlockAdditionMapping", false),
    (0x41E7, "BlockAddIDType", false),
    (0x41ED, "BlockAddIDExtraData", false),
    (0x41F0, "BlockAddIDValue", false),
    (0x4254, "ContentCompAlgo", false),
    (0x4255, "ContentCompSettings", false),
    (0x437C, "ChapLanguage", false),
    (0x437D, "ChapLanguageBCP47", false),
    (0x437E, "ChapCountry", false),
    (0x4444, "SegmentFamily", false),
    (0x4461, "DateUTC", false),
    (0x447A, "TagLanguage", false),
    (0x447B, "TagLanguageBCP47", false),
    (0x4484, "TagDefault", false),
    (0x4485, "TagBinary", false),
    (0x4487, "TagString", false),
    (0x4489, "Duration", false),
    (0x44B4, "TagDefaultBogus", true),
    (0x450D, "ChapProcessPrivate", false),
    (0x45A3, "TagName", false),
    (0x45B9, "EditionEntry", false),
    (0x45BC, "EditionUID", false),
    (0x45DB, "EditionFlagDefault", false),
    (0x45DD, "EditionFlagOrdered", false),
    (0x465C, "FileData", false),
    (0x4660, "FileMediaType", false),
    (0x4661, "FileUsedStartTime", true),
    (0x4662, "FileUsedEndTime", true),
    (0x466E, "FileName", false),
    (0x4675, "FileReferral", true),
    (0x467E, "FileDescription", false),
    (0x46AE, "FileUID", false),
    (0x47E1, "ContentEncAlgo", false),
    (0x47E2, "ContentEncKeyID", false),
    (0x47E3, "ContentSignature", true),
    (0x47E4, "ContentSigKeyID", true),
    (0x47E5, "ContentSigAlgo", true),
    (0x47E6, "ContentSigHashAlgo", true),
    (0x47E7, "ContentEncAESSettings", false),
    (0x47E8, "AESSettingsCipherMode", false),
    (0x4D80, "MuxingApp", false),
    (0x4DBB, "Seek", false),
    (0x5031, "ContentEncodingOrder", false),
    (0x5032, "ContentEncodingScope", false),
    (0x5033, "ContentEncodingType", false),
    (0x5034, "ContentCompression", false),
    (0x5035, "ContentEncryption", false),
    (0x535F, "CueRefNumber", true),
    (0x536E, "Name", false),
    (0x5378, "CueBlockNumber", false),
    (0x537F, "TrackOffset", true),
    (0x53AB, "SeekID", false),
    (0x53AC, "SeekPosition", false),
    (0x53B8, "StereoMode", false),
    (0x53B9, "OldStereoMode", false),
    (0x53C0, "AlphaMode", false),
    (0x54AA, "PixelCropBottom", false),
    (0x54B0, "DisplayWidth", false),
    (0x54B2, "DisplayUnit", false),
    (0x54B3, "AspectRatioType", true),
    (0x54BA, "DisplayHeight", false),
    (0x54BB, "PixelCropTop", false),
    (0x54CC, "PixelCropLeft", false),
    (0x54DD, "PixelCropRight", false),
    (0x55AA, "FlagForced", false),
    (0x55AB, "FlagHearingImpaired", false),
    (0x55AC, "FlagVisualImpaired", false),
    (0x55AD, "FlagTextDescriptions", false),
    (0x55AE, "FlagOriginal", false),
    (0x55AF, "FlagCommentary", false),
    (0x55B0, "Colour", false),
    (0x55B1, "MatrixCoefficients", false),
    (0x55B2, "BitsPerChannel", false),
    (0x55B3, "ChromaSubsamplingHorz", false),
    (0x55B4, "ChromaSubsamplingVert", false),
    (0x55B5, "CbSubsamplingHorz", false),
    (0x55B6, "CbSubsamplingVert", false),
    (0x55B7, "ChromaSitingHorz", false),
    (0x55B8, "ChromaSitingVert", false),
    (0x55B9, "Range", false),
    (0x55BA, "TransferCharacteristics", false),
    (0x55BB, "Primaries", false),
    (0x55BC, "MaxCLL", false),
    (0x55BD, "MaxFALL", false),
    (0x55D0, "MasteringMetadata", false),
    (0x55D1, "PrimaryRChromaticityX", false),
    (0x55D2, "PrimaryRChromaticityY", false),
    (0x55D3, "PrimaryGChromaticityX", false),
    (0x55D4, "PrimaryGChromaticityY", false),
    (0x55D5, "PrimaryBChromaticityX", false),
    (0x55D6, "PrimaryBChromaticityY", false),
    (0x55D7, "WhitePointChromaticityX", false),
    (0x55D8, "WhitePointChromaticityY", false),
    (0x55D9, "LuminanceMax", false),
    (0x55DA, "LuminanceMin", false),
    (0x55EE, "MaxBlockAdditionID", false),
    (0x5654, "ChapterStringUID", false),
    (0x56AA, "CodecDelay", false),
    (0x56BB, "SeekPreRoll", false),
    (0x5741, "WritingApp", false),
    (0x5854, "SilentTracks", true),
    (0x58D7, "SilentTrackNumber", true),
    (0x61A7, "AttachedFile", false),
    (0x6240, "ContentEncoding", false),
    (0x6264, "BitDepth", false),
    (0x63A2, "CodecPrivate", false),
    (0x63C0, "Targets", false),
    (0x63C3, "ChapterPhysicalEquiv", false),
    (0x63C4, "TagChapterUID", false),
    (0x63C5, "TagTrackUID", false),
    (0x63C6, "TagAttachmentUID", false),
    (0x63C9, "TagEditionUID", false),
    (0x63CA, "TargetType", false),
    (0x6624, "TrackTranslate", false),
    (0x66A5, "TrackTranslateTrackID", false),
    (0x66BF, "TrackTranslateCodec", false),
    (0x66FC, "TrackTranslateEditionUID", false),
    (0x67C8, "SimpleTag", false),
    (0x68CA, "TargetTypeValue", false),
    (0x6911, "ChapProcessCommand", false),
    (0x6922, "ChapProcessTime", false),
    (0x6924, "ChapterTranslate", false),
    (0x6933, "ChapProcessData", false),
    (0x6944, "ChapProcess", false),
    (0x6955, "ChapProcessCodecID", false),
    (0x69A5, "ChapterTranslateID", false),
    (0x69BF, "ChapterTranslateCodec", false),
    (0x69FC, "ChapterTranslateEditionUID", false),
    (0x6D80, "ContentEncodings", false),
    (0x6DE7, "MinCache", true),
    (0x6DF8, "MaxCache", true),
    (0x6E67, "ChapterSegmentUUID", false),
    (0x6EBC, "ChapterSegmentEditionUID", false),
    (0x6FAB, "TrackOverlay", true),
    (0x7373, "Tag", false),
    (0x7384, "SegmentFilename", false),
    (0x73A4, "SegmentUUID", false),
    (0x73C4, "ChapterUID", false),
    (0x73C5, "TrackUID", false),
    (0x7446, "AttachmentLink", false),
    (0x75A1, "BlockAdditions", false),
    (0x75A2, "DiscardPadding", false),
    (0x7670, "Projection", false),
    (0x7671, "ProjectionType", false),
    (0x7672, "ProjectionPrivate", false),
    (0x7673, "ProjectionPoseYaw", false),
    (0x7674, "ProjectionPosePitch", false),
    (0x7675, "ProjectionPoseRoll", false),
    (0x78B5, "OutputSamplingFrequency", false),
    (0x7BA9, "Title", false),
    (0x7D7B, "ChannelPositions", true),
    (0x22B59C, "Language", false),
    (0x22B59D, "LanguageBCP47", false),
    (0x23314F, "TrackTimestampScale", false),
    (0x234E7A, "DefaultDecodedFieldDuration", false),
    (0x2383E3, "FrameRate", true),
    (0x23E383, "DefaultDuration", false),
    (0x258688, "CodecName", false),
    (0x26B240, "CodecDownloadURL", true),
    (0x2AD7B1, "TimestampScale", false),
    (0x2EB524, "UncompressedFourCC", false),
    (0x2FB523, "GammaValue", true),
    (0x3A9697, "CodecSettings", true),
    (0x3B4040, "CodecInfoURL", true),
    (0x3C83AB, "PrevFilename", false),
    (0x3CB923, "PrevUUID", false),
    (0x3E83BB, "NextFilename", false),
    (0x3EB923, "NextUUID", false),
    (0x1043A770, "Chapters", false),
    (0x114D9B74, "SeekHead", false),
    (0x1254C367, "Tags", false),
    (0x1549A966, "Info", false),
    (0x1654AE6B, "Tracks", false),
    (0x18538067, "Segment", false),
    (0x1941A469, "Attachments", false),
    (0x1C53BB6B, "Cues", false),
    (0x1F43B675, "Cluster", false),
];

/// The 4 all-ones `Reserved` registry rows (one per ID width). These are
/// registry placeholders, not elements — `ids.rs` must NOT define them.
const RESERVED: &[u32] = &[0xFF, 0x7FFF, 0x3F_FFFF, 0x1FFF_FFFF];

/// RFC 8794 EBML-header + EBML-global element IDs the crate also names.
/// Section 27.1: "EBML Element IDs defined for the EBML Header -- as
/// defined in Section 17.1 of [RFC8794] -- MUST NOT be used as Matroska
/// Element IDs", so these are legitimately outside Table 53.
const EBML_RFC8794: &[(u32, &str)] = &[
    (0x1A45_DFA3, "EBML"),
    (0x4286, "EBMLVersion"),
    (0x42F7, "EBMLReadVersion"),
    (0x42F2, "EBMLMaxIDLength"),
    (0x42F3, "EBMLMaxSizeLength"),
    (0x4282, "DocType"),
    (0x4287, "DocTypeVersion"),
    (0x4285, "DocTypeReadVersion"),
    (0x4281, "DocTypeExtension"),
    (0x4283, "DocTypeExtensionName"),
    (0x4284, "DocTypeExtensionVersion"),
    (0xEC, "Void"),
    (0xBF, "CRC-32"),
];

/// The one out-of-registry element the crate knowingly keeps: the legacy
/// `ChapterFlagEnabled` (`0x4598`) from the pre-RFC Matroska schema.
/// RFC 9559 dropped it (the ChapterAtom sections jump from
/// ChapterFlagHidden §5.1.7.1.4.5 to ChapterSegmentUUID §5.1.7.1.4.6 and
/// Table 53 does not assign `0x4598`), but historical files carry it, so
/// the crate reads and round-trips it as an ecosystem element.
const OUT_OF_REGISTRY_LEGACY: &[(u32, &str)] = &[(0x4598, "ChapterFlagEnabled")];

/// Crate const names that deliberately differ from the registry Element
/// Name (disambiguation prefixes for name collisions inside `ids.rs`).
const NAME_EXCEPTIONS: &[(u32, &str)] = &[
    // Registry: `BlockAdditionID` (reclaimed, Appendix A.9) — prefixed to
    // avoid colliding with the modern `BlockAddID` (0xEE) family.
    (0xCB, "TIMESLICE_BLOCK_ADDITION_ID"),
    // Registry: `Range` (§5.1.4.1.28.26) — prefixed with its `Colour`
    // parent because a bare `RANGE` says nothing at the call site.
    (0x55B9, "COLOUR_RANGE"),
];

/// A `pub const NAME: u32 = 0x...;` row of `src/ids.rs`.
type NumericConst = (String, u32);
/// A `pub const ALIAS: u32 = TARGET;` row of `src/ids.rs` — the
/// spec-name re-exports like `TIMESTAMP_SCALE = TIMECODE_SCALE`.
type AliasConst = (String, String);

/// Parse the `pub const NAME: u32 = ...;` surface of `src/ids.rs`.
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

/// `TRACK_ENTRY` ~ `TrackEntry` ~ `CRC-32` name normalisation.
fn norm(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_' && *c != '-')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[test]
fn registry_census_counts() {
    // Table 53 has 254 rows: 250 named elements (43 of them Reclaimed)
    // plus the 4 all-ones Reserved placeholders.
    assert_eq!(REGISTRY.len(), 250);
    assert_eq!(REGISTRY.iter().filter(|(_, _, r)| *r).count(), 43);
    assert_eq!(RESERVED.len(), 4);
    // No duplicate IDs or names inside the transcription itself.
    for (i, (id, name, _)) in REGISTRY.iter().enumerate() {
        for (id2, name2, _) in &REGISTRY[i + 1..] {
            assert_ne!(id, id2, "duplicate registry id 0x{id:X}");
            assert_ne!(name, name2, "duplicate registry name {name}");
        }
    }
}

#[test]
fn every_registry_element_has_an_ids_const() {
    let (numeric, _) = parse_ids_rs();
    let values: Vec<u32> = numeric.iter().map(|(_, v)| *v).collect();
    let mut missing = Vec::new();
    for (id, name, _) in REGISTRY {
        if !values.contains(id) {
            missing.push(format!("0x{id:X} {name}"));
        }
    }
    assert!(
        missing.is_empty(),
        "RFC 9559 registry elements with no ids.rs const: {missing:?}"
    );
}

#[test]
fn every_ids_const_is_registry_or_documented_exception() {
    let (numeric, _) = parse_ids_rs();
    let mut rogue = Vec::new();
    for (name, v) in &numeric {
        let in_registry = REGISTRY.iter().any(|(id, _, _)| id == v);
        let in_ebml = EBML_RFC8794.iter().any(|(id, _)| id == v);
        let in_legacy = OUT_OF_REGISTRY_LEGACY.iter().any(|(id, _)| id == v);
        if !(in_registry || in_ebml || in_legacy) {
            rogue.push(format!("{name} = 0x{v:X}"));
        }
    }
    assert!(
        rogue.is_empty(),
        "ids.rs consts outside the RFC 9559 registry, the RFC 8794 EBML \
         header set, and the documented legacy exception: {rogue:?}"
    );
}

#[test]
fn const_names_agree_with_registry_names() {
    let (numeric, aliases) = parse_ids_rs();
    // Resolve aliases onto their target's numeric value so a spec-name
    // alias (e.g. TIMESTAMP_SCALE for TIMECODE_SCALE) satisfies the
    // name check for its ID.
    let mut by_value: Vec<(u32, String)> = numeric.iter().map(|(n, v)| (*v, norm(n))).collect();
    for (alias, target) in &aliases {
        if let Some((_, v)) = numeric.iter().find(|(n, _)| n == target) {
            by_value.push((*v, norm(alias)));
        }
    }
    let mut mismatched = Vec::new();
    for (id, name, _) in REGISTRY {
        if NAME_EXCEPTIONS.iter().any(|(eid, _)| eid == id) {
            let (_, expected) = NAME_EXCEPTIONS.iter().find(|(eid, _)| eid == id).unwrap();
            assert!(
                by_value
                    .iter()
                    .any(|(v, n)| v == id && *n == norm(expected)),
                "0x{id:X}: expected exception const {expected}"
            );
            continue;
        }
        if !by_value.iter().any(|(v, n)| v == id && *n == norm(name)) {
            let actual: Vec<&String> = by_value
                .iter()
                .filter(|(v, _)| v == id)
                .map(|(_, n)| n)
                .collect();
            mismatched.push(format!("0x{id:X} registry `{name}` vs consts {actual:?}"));
        }
    }
    assert!(
        mismatched.is_empty(),
        "const names disagreeing with registry Element Names: {mismatched:?}"
    );
    // The EBML-header consts also carry their RFC 8794 names, allowing
    // the crate's `EBML_` disambiguation prefix (`EBML_HEADER` for the
    // root, `EBML_DOC_TYPE` for `DocType`, ...).
    for (id, name) in EBML_RFC8794 {
        assert!(
            by_value
                .iter()
                .any(|(v, n)| v == id && n.contains(&norm(name))),
            "EBML header id 0x{id:X}: no const named like `{name}`"
        );
    }
}

#[test]
fn no_reserved_id_is_defined() {
    let (numeric, _) = parse_ids_rs();
    for r in RESERVED {
        assert!(
            !numeric.iter().any(|(_, v)| v == r),
            "ids.rs defines the all-ones Reserved id 0x{r:X} as an element"
        );
    }
    for (id, _, _) in REGISTRY {
        assert!(
            !RESERVED.contains(id),
            "registry transcription contains a Reserved id 0x{id:X}"
        );
    }
}

#[test]
fn registry_ids_sit_in_valid_vint_classes() {
    // Section 27.1 valid Element ID ranges per encoded width. The
    // Reserved all-ones values cap each class; named entries must be
    // strictly inside a class and not all-ones.
    for (id, name, _) in REGISTRY {
        let ok = (0x80..=0xFE).contains(id)
            || (0x407F..=0x7FFE).contains(id)
            || (0x20_3FFF..=0x3F_FFFE).contains(id)
            || (0x101F_FFFF..=0x1FFF_FFFE).contains(id);
        assert!(
            ok,
            "registry id 0x{id:X} ({name}) outside every valid ID class"
        );
    }
    // The legacy out-of-registry element still has to be a well-formed
    // two-octet ID — it is simply unassigned in Table 53.
    for (id, name) in OUT_OF_REGISTRY_LEGACY {
        assert!(
            (0x407F..=0x7FFE).contains(id),
            "legacy id 0x{id:X} ({name}) outside the two-octet ID class"
        );
    }
}
