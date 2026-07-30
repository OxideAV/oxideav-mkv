//! Machine-readable Matroska element schema, transcribed from the staged
//! IETF CELLAR EBML Schema for Matroska (`ebml_matroska.xml`,
//! `docs/container/matroska/`) — the normative machine-readable form of
//! the element definitions RFC 9559 presents as prose.
//!
//! Each [`ElementDef`] row carries the element's identity (`id`, `name`,
//! schema `path`, derived `parent_id`), its EBML `element_type`, its
//! occurrence constraints (`min_occurs` / `max_occurs`), its value
//! constraints (`range` / `length` / `default`, verbatim schema
//! strings), its schema-version window (`min_ver` / `max_ver` — the
//! `maxver: 0` rows are the deprecated elements RFC 9559 reclaims), the
//! `recursive` / `recurring` / `unknown_size_allowed` structural
//! markers, and the WebM-usability extension marker (`webm`).
//!
//! The table is a superset of the RFC 9559 registry surface: it carries
//! the six post-RFC `minver: 5` elements (`EditionDisplay`,
//! `EditionString`, `EditionLanguageIETF`, `ChapterSkipType`,
//! `Emphasis`, `TagBlockAddIDValue`) and the legacy chapter elements
//! (`EditionFlagHidden`, `ChapterTrack`, `ChapterTrackUID`,
//! `ChapterFlagEnabled`) the registry never assigned. The removed
//! Signature family (see `docs/container/matroska/legacy-element-ids.md`)
//! is absent from the schema by design.
//!
//! [`SCHEMA`] holds the 262 Matroska rows; [`EBML_SUPPLEMENT`] adds the
//! RFC 8794 EBML-header elements and the two EBML *global* elements
//! (`Void`, `CRC-32`) a whole-document walk also meets, so
//! [`element_def`] resolves every element a well-formed Matroska
//! document can legally carry. [`validate`] (see below) walks a whole
//! document against the table.

/// EBML element type (RFC 8794 §7) of a schema element.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ElementType {
    /// Contains child elements only.
    Master,
    /// Big-endian unsigned integer, 0-8 octets.
    Uinteger,
    /// Big-endian signed integer, 0-8 octets.
    Integer,
    /// IEEE-754 float, 0, 4, or 8 octets.
    Float,
    /// Printable ASCII string.
    AsciiString,
    /// UTF-8 string.
    Utf8,
    /// Signed nanoseconds since 2001-01-01T00:00:00 UTC, 0 or 8 octets.
    Date,
    /// Opaque bytes.
    Binary,
}

/// Sentinel for [`ElementDef::max_ver`]: the element is current in the
/// newest schema version (no `maxver` attribute).
pub const NO_MAX_VER: u8 = u8::MAX;

/// One element row of the schema — see the module docs for the field
/// semantics. `range` / `length` / `default` are the schema's verbatim
/// attribute strings (float ranges/defaults use the C hex-float
/// spelling, e.g. `0x1.f4p+12` = 8000.0).
#[derive(Clone, Copy, Debug)]
pub struct ElementDef {
    /// Element ID with the VINT marker bits, as everywhere in [`crate::ids`].
    pub id: u32,
    /// Schema element name (RFC 9559 spelling).
    pub name: &'static str,
    /// Schema path, verbatim (`+` marks a recursive component).
    pub path: &'static str,
    /// The parent master's element ID; `None` only for the Root Element
    /// (`Segment`) and the [`EBML_SUPPLEMENT`] globals (`Void`,
    /// `CRC-32`), which are legal at any level.
    pub parent_id: Option<u32>,
    /// EBML element type.
    pub element_type: ElementType,
    /// Minimum occurrences per parent (`0` when the schema is silent).
    pub min_occurs: u32,
    /// Maximum occurrences per parent; `None` = unbounded.
    pub max_occurs: Option<u32>,
    /// Verbatim schema `range` constraint, when one exists.
    pub range: Option<&'static str>,
    /// Verbatim schema `length` constraint, when one exists.
    pub length: Option<&'static str>,
    /// Verbatim schema `default` value, when one exists.
    pub default: Option<&'static str>,
    /// First schema version the element appears in.
    pub min_ver: u8,
    /// Last schema version the element is legal in ([`NO_MAX_VER`] =
    /// still current; `0` = deprecated before v1 shipped — the
    /// RFC 9559 "Reclaimed" set).
    pub max_ver: u8,
    /// The element may nest inside itself (`ChapterAtom`, `SimpleTag`).
    pub recursive: bool,
    /// `recurring` schema marker (identically recurring element).
    pub recurring: bool,
    /// The element may use the unknown-size VINT (Segment, Cluster).
    pub unknown_size_allowed: bool,
    /// Carries the `webmproject.org` `webm="1"` extension marker — the
    /// schema's own WebM-usability signal. The WebM *guidelines*
    /// support table ([`crate::webm`]) is the authority for the strict
    /// WebM profile; this flag is the schema's corroborating signal.
    pub webm: bool,
}

impl ElementDef {
    /// `true` when the schema requires at least one occurrence per
    /// parent. Note RFC 8794 §11.1.6.2: a mandatory element that
    /// declares a default value may still be absent on disk (the
    /// default is materialised by the reader).
    pub fn is_mandatory(&self) -> bool {
        self.min_occurs >= 1
    }

    /// `true` when the element is deprecated in the current schema
    /// (`maxver` below the version the staged schema describes) — the
    /// RFC 9559 "Reclaimed" rows and the two `maxver` 2/3 stragglers.
    pub fn is_deprecated(&self) -> bool {
        self.max_ver != NO_MAX_VER && self.max_ver < 4
    }

    /// `max_ver` as an `Option` (`None` = current, no ceiling).
    pub fn max_ver_opt(&self) -> Option<u8> {
        if self.max_ver == NO_MAX_VER {
            None
        } else {
            Some(self.max_ver)
        }
    }
}

/// The RFC 8794 EBML-header elements plus the two EBML global elements —
/// everything a whole-document walk meets that the Matroska schema
/// itself does not define. Attribute values per RFC 8794 §11.2 / §11.3.
/// (`EBMLMaxIDLength` / `EBMLMaxSizeLength` are *in* the Matroska schema
/// — it constrains them — so they live in [`SCHEMA`], not here.)
/// Sorted by ID for binary search.
pub const EBML_SUPPLEMENT: &[ElementDef] = &[
    ElementDef {
        id: 0xBF,
        name: "CRC-32",
        path: "\\(-\\)CRC-32",
        parent_id: None,
        element_type: ElementType::Binary,
        min_occurs: 0,
        max_occurs: Some(1),
        range: None,
        length: Some("4"),
        default: None,
        min_ver: 1,
        max_ver: NO_MAX_VER,
        recursive: false,
        recurring: false,
        unknown_size_allowed: false,
        webm: false,
    },
    ElementDef {
        id: 0xEC,
        name: "Void",
        path: "\\(-\\)Void",
        parent_id: None,
        element_type: ElementType::Binary,
        min_occurs: 0,
        max_occurs: None,
        range: None,
        length: None,
        default: None,
        min_ver: 1,
        max_ver: NO_MAX_VER,
        recursive: false,
        recurring: false,
        unknown_size_allowed: false,
        webm: true,
    },
    ElementDef {
        id: 0x4281,
        name: "DocTypeExtension",
        path: "\\EBML\\DocTypeExtension",
        parent_id: Some(0x1A45DFA3),
        element_type: ElementType::Master,
        min_occurs: 0,
        max_occurs: None,
        range: None,
        length: None,
        default: None,
        min_ver: 1,
        max_ver: NO_MAX_VER,
        recursive: false,
        recurring: false,
        unknown_size_allowed: false,
        webm: false,
    },
    ElementDef {
        id: 0x4282,
        name: "DocType",
        path: "\\EBML\\DocType",
        parent_id: Some(0x1A45DFA3),
        element_type: ElementType::AsciiString,
        min_occurs: 1,
        max_occurs: Some(1),
        range: None,
        length: Some(">0"),
        default: None,
        min_ver: 1,
        max_ver: NO_MAX_VER,
        recursive: false,
        recurring: false,
        unknown_size_allowed: false,
        webm: true,
    },
    ElementDef {
        id: 0x4283,
        name: "DocTypeExtensionName",
        path: "\\EBML\\DocTypeExtension\\DocTypeExtensionName",
        parent_id: Some(0x4281),
        element_type: ElementType::AsciiString,
        min_occurs: 1,
        max_occurs: Some(1),
        range: None,
        length: Some(">0"),
        default: None,
        min_ver: 1,
        max_ver: NO_MAX_VER,
        recursive: false,
        recurring: false,
        unknown_size_allowed: false,
        webm: false,
    },
    ElementDef {
        id: 0x4284,
        name: "DocTypeExtensionVersion",
        path: "\\EBML\\DocTypeExtension\\DocTypeExtensionVersion",
        parent_id: Some(0x4281),
        element_type: ElementType::Uinteger,
        min_occurs: 1,
        max_occurs: Some(1),
        range: Some("not 0"),
        length: None,
        default: None,
        min_ver: 1,
        max_ver: NO_MAX_VER,
        recursive: false,
        recurring: false,
        unknown_size_allowed: false,
        webm: false,
    },
    ElementDef {
        id: 0x4285,
        name: "DocTypeReadVersion",
        path: "\\EBML\\DocTypeReadVersion",
        parent_id: Some(0x1A45DFA3),
        element_type: ElementType::Uinteger,
        min_occurs: 1,
        max_occurs: Some(1),
        range: Some("not 0"),
        length: None,
        default: Some("1"),
        min_ver: 1,
        max_ver: NO_MAX_VER,
        recursive: false,
        recurring: false,
        unknown_size_allowed: false,
        webm: true,
    },
    ElementDef {
        id: 0x4286,
        name: "EBMLVersion",
        path: "\\EBML\\EBMLVersion",
        parent_id: Some(0x1A45DFA3),
        element_type: ElementType::Uinteger,
        min_occurs: 1,
        max_occurs: Some(1),
        range: Some("not 0"),
        length: None,
        default: Some("1"),
        min_ver: 1,
        max_ver: NO_MAX_VER,
        recursive: false,
        recurring: false,
        unknown_size_allowed: false,
        webm: true,
    },
    ElementDef {
        id: 0x4287,
        name: "DocTypeVersion",
        path: "\\EBML\\DocTypeVersion",
        parent_id: Some(0x1A45DFA3),
        element_type: ElementType::Uinteger,
        min_occurs: 1,
        max_occurs: Some(1),
        range: Some("not 0"),
        length: None,
        default: Some("1"),
        min_ver: 1,
        max_ver: NO_MAX_VER,
        recursive: false,
        recurring: false,
        unknown_size_allowed: false,
        webm: true,
    },
    ElementDef {
        id: 0x42F7,
        name: "EBMLReadVersion",
        path: "\\EBML\\EBMLReadVersion",
        parent_id: Some(0x1A45DFA3),
        element_type: ElementType::Uinteger,
        min_occurs: 1,
        max_occurs: Some(1),
        range: Some("1"),
        length: None,
        default: Some("1"),
        min_ver: 1,
        max_ver: NO_MAX_VER,
        recursive: false,
        recurring: false,
        unknown_size_allowed: false,
        webm: true,
    },
    ElementDef {
        id: 0x1A45DFA3,
        name: "EBML",
        path: "\\EBML",
        parent_id: None,
        element_type: ElementType::Master,
        min_occurs: 1,
        max_occurs: None,
        range: None,
        length: None,
        default: None,
        min_ver: 1,
        max_ver: NO_MAX_VER,
        recursive: false,
        recurring: false,
        unknown_size_allowed: false,
        webm: true,
    },
];

/// The full Matroska element schema — one row per `<element>` of the
/// staged `ebml_matroska.xml`, sorted by element ID for binary search.
pub const SCHEMA: &[ElementDef] = &[
    ElementDef { id: 0x80, name: "ChapterDisplay", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterDisplay", parent_id: Some(0xB6), element_type: ElementType::Master, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x83, name: "TrackType", path: "\\Segment\\Tracks\\TrackEntry\\TrackType", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x85, name: "ChapString", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterDisplay\\ChapString", parent_id: Some(0x80), element_type: ElementType::Utf8, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x86, name: "CodecID", path: "\\Segment\\Tracks\\TrackEntry\\CodecID", parent_id: Some(0xAE), element_type: ElementType::AsciiString, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x88, name: "FlagDefault", path: "\\Segment\\Tracks\\TrackEntry\\FlagDefault", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("0-1"), length: None, default: Some("1"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x89, name: "ChapterTrackUID", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterTrack\\ChapterTrackUID", parent_id: Some(0x8F), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: None, range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x8E, name: "Slices", path: "\\Segment\\Cluster\\BlockGroup\\Slices", parent_id: Some(0xA0), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x8F, name: "ChapterTrack", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterTrack", parent_id: Some(0xB6), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x91, name: "ChapterTimeStart", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterTimeStart", parent_id: Some(0xB6), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x92, name: "ChapterTimeEnd", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterTimeEnd", parent_id: Some(0xB6), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x96, name: "CueRefTime", path: "\\Segment\\Cues\\CuePoint\\CueTrackPositions\\CueReference\\CueRefTime", parent_id: Some(0xDB), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 2, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x97, name: "CueRefCluster", path: "\\Segment\\Cues\\CuePoint\\CueTrackPositions\\CueReference\\CueRefCluster", parent_id: Some(0xDB), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x98, name: "ChapterFlagHidden", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterFlagHidden", parent_id: Some(0xB6), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("0-1"), length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x9A, name: "FlagInterlaced", path: "\\Segment\\Tracks\\TrackEntry\\Video\\FlagInterlaced", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 2, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x9B, name: "BlockDuration", path: "\\Segment\\Cluster\\BlockGroup\\BlockDuration", parent_id: Some(0xA0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x9C, name: "FlagLacing", path: "\\Segment\\Tracks\\TrackEntry\\FlagLacing", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("0-1"), length: None, default: Some("1"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x9D, name: "FieldOrder", path: "\\Segment\\Tracks\\TrackEntry\\Video\\FieldOrder", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("2"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x9F, name: "Channels", path: "\\Segment\\Tracks\\TrackEntry\\Audio\\Channels", parent_id: Some(0xE1), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: Some("1"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xA0, name: "BlockGroup", path: "\\Segment\\Cluster\\BlockGroup", parent_id: Some(0x1F43B675), element_type: ElementType::Master, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xA1, name: "Block", path: "\\Segment\\Cluster\\BlockGroup\\Block", parent_id: Some(0xA0), element_type: ElementType::Binary, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xA2, name: "BlockVirtual", path: "\\Segment\\Cluster\\BlockGroup\\BlockVirtual", parent_id: Some(0xA0), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xA3, name: "SimpleBlock", path: "\\Segment\\Cluster\\SimpleBlock", parent_id: Some(0x1F43B675), element_type: ElementType::Binary, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 2, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xA4, name: "CodecState", path: "\\Segment\\Cluster\\BlockGroup\\CodecState", parent_id: Some(0xA0), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 2, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xA5, name: "BlockAdditional", path: "\\Segment\\Cluster\\BlockGroup\\BlockAdditions\\BlockMore\\BlockAdditional", parent_id: Some(0xA6), element_type: ElementType::Binary, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xA6, name: "BlockMore", path: "\\Segment\\Cluster\\BlockGroup\\BlockAdditions\\BlockMore", parent_id: Some(0x75A1), element_type: ElementType::Master, min_occurs: 1, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xA7, name: "Position", path: "\\Segment\\Cluster\\Position", parent_id: Some(0x1F43B675), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: 4, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xAA, name: "CodecDecodeAll", path: "\\Segment\\Tracks\\TrackEntry\\CodecDecodeAll", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("0-1"), length: None, default: Some("1"), min_ver: 1, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xAB, name: "PrevSize", path: "\\Segment\\Cluster\\PrevSize", parent_id: Some(0x1F43B675), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xAE, name: "TrackEntry", path: "\\Segment\\Tracks\\TrackEntry", parent_id: Some(0x1654AE6B), element_type: ElementType::Master, min_occurs: 1, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xAF, name: "EncryptedBlock", path: "\\Segment\\Cluster\\EncryptedBlock", parent_id: Some(0x1F43B675), element_type: ElementType::Binary, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xB0, name: "PixelWidth", path: "\\Segment\\Tracks\\TrackEntry\\Video\\PixelWidth", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xB2, name: "CueDuration", path: "\\Segment\\Cues\\CuePoint\\CueTrackPositions\\CueDuration", parent_id: Some(0xB7), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xB3, name: "CueTime", path: "\\Segment\\Cues\\CuePoint\\CueTime", parent_id: Some(0xBB), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xB5, name: "SamplingFrequency", path: "\\Segment\\Tracks\\TrackEntry\\Audio\\SamplingFrequency", parent_id: Some(0xE1), element_type: ElementType::Float, min_occurs: 1, max_occurs: Some(1), range: Some("> 0x0p+0"), length: None, default: Some("0x1.f4p+12"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xB6, name: "ChapterAtom", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom", parent_id: Some(0x45B9), element_type: ElementType::Master, min_occurs: 1, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: true, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xB7, name: "CueTrackPositions", path: "\\Segment\\Cues\\CuePoint\\CueTrackPositions", parent_id: Some(0xBB), element_type: ElementType::Master, min_occurs: 1, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xB9, name: "FlagEnabled", path: "\\Segment\\Tracks\\TrackEntry\\FlagEnabled", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("0-1"), length: None, default: Some("1"), min_ver: 2, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xBA, name: "PixelHeight", path: "\\Segment\\Tracks\\TrackEntry\\Video\\PixelHeight", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xBB, name: "CuePoint", path: "\\Segment\\Cues\\CuePoint", parent_id: Some(0x1C53BB6B), element_type: ElementType::Master, min_occurs: 1, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xC0, name: "TrickTrackUID", path: "\\Segment\\Tracks\\TrackEntry\\TrickTrackUID", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xC1, name: "TrickTrackSegmentUID", path: "\\Segment\\Tracks\\TrackEntry\\TrickTrackSegmentUID", parent_id: Some(0xAE), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: Some("16"), default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xC4, name: "TrickMasterTrackSegmentUID", path: "\\Segment\\Tracks\\TrackEntry\\TrickMasterTrackSegmentUID", parent_id: Some(0xAE), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: Some("16"), default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xC6, name: "TrickTrackFlag", path: "\\Segment\\Tracks\\TrackEntry\\TrickTrackFlag", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xC7, name: "TrickMasterTrackUID", path: "\\Segment\\Tracks\\TrackEntry\\TrickMasterTrackUID", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xC8, name: "ReferenceFrame", path: "\\Segment\\Cluster\\BlockGroup\\ReferenceFrame", parent_id: Some(0xA0), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xC9, name: "ReferenceOffset", path: "\\Segment\\Cluster\\BlockGroup\\ReferenceFrame\\ReferenceOffset", parent_id: Some(0xC8), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xCA, name: "ReferenceTimestamp", path: "\\Segment\\Cluster\\BlockGroup\\ReferenceFrame\\ReferenceTimestamp", parent_id: Some(0xC8), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xCB, name: "BlockAdditionID", path: "\\Segment\\Cluster\\BlockGroup\\Slices\\TimeSlice\\BlockAdditionID", parent_id: Some(0xE8), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xCC, name: "LaceNumber", path: "\\Segment\\Cluster\\BlockGroup\\Slices\\TimeSlice\\LaceNumber", parent_id: Some(0xE8), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xCD, name: "FrameNumber", path: "\\Segment\\Cluster\\BlockGroup\\Slices\\TimeSlice\\FrameNumber", parent_id: Some(0xE8), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xCE, name: "Delay", path: "\\Segment\\Cluster\\BlockGroup\\Slices\\TimeSlice\\Delay", parent_id: Some(0xE8), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xCF, name: "SliceDuration", path: "\\Segment\\Cluster\\BlockGroup\\Slices\\TimeSlice\\SliceDuration", parent_id: Some(0xE8), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xD7, name: "TrackNumber", path: "\\Segment\\Tracks\\TrackEntry\\TrackNumber", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xDB, name: "CueReference", path: "\\Segment\\Cues\\CuePoint\\CueTrackPositions\\CueReference", parent_id: Some(0xB7), element_type: ElementType::Master, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 2, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xE0, name: "Video", path: "\\Segment\\Tracks\\TrackEntry\\Video", parent_id: Some(0xAE), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xE1, name: "Audio", path: "\\Segment\\Tracks\\TrackEntry\\Audio", parent_id: Some(0xAE), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xE2, name: "TrackOperation", path: "\\Segment\\Tracks\\TrackEntry\\TrackOperation", parent_id: Some(0xAE), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 3, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xE3, name: "TrackCombinePlanes", path: "\\Segment\\Tracks\\TrackEntry\\TrackOperation\\TrackCombinePlanes", parent_id: Some(0xE2), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 3, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xE4, name: "TrackPlane", path: "\\Segment\\Tracks\\TrackEntry\\TrackOperation\\TrackCombinePlanes\\TrackPlane", parent_id: Some(0xE3), element_type: ElementType::Master, min_occurs: 1, max_occurs: None, range: None, length: None, default: None, min_ver: 3, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xE5, name: "TrackPlaneUID", path: "\\Segment\\Tracks\\TrackEntry\\TrackOperation\\TrackCombinePlanes\\TrackPlane\\TrackPlaneUID", parent_id: Some(0xE4), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 3, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xE6, name: "TrackPlaneType", path: "\\Segment\\Tracks\\TrackEntry\\TrackOperation\\TrackCombinePlanes\\TrackPlane\\TrackPlaneType", parent_id: Some(0xE4), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 3, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xE7, name: "Timestamp", path: "\\Segment\\Cluster\\Timestamp", parent_id: Some(0x1F43B675), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xE8, name: "TimeSlice", path: "\\Segment\\Cluster\\BlockGroup\\Slices\\TimeSlice", parent_id: Some(0x8E), element_type: ElementType::Master, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xE9, name: "TrackJoinBlocks", path: "\\Segment\\Tracks\\TrackEntry\\TrackOperation\\TrackJoinBlocks", parent_id: Some(0xE2), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 3, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xEA, name: "CueCodecState", path: "\\Segment\\Cues\\CuePoint\\CueTrackPositions\\CueCodecState", parent_id: Some(0xB7), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 2, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xEB, name: "CueRefCodecState", path: "\\Segment\\Cues\\CuePoint\\CueTrackPositions\\CueReference\\CueRefCodecState", parent_id: Some(0xDB), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xED, name: "TrackJoinUID", path: "\\Segment\\Tracks\\TrackEntry\\TrackOperation\\TrackJoinBlocks\\TrackJoinUID", parent_id: Some(0xE9), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: None, range: Some("not 0"), length: None, default: None, min_ver: 3, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xEE, name: "BlockAddID", path: "\\Segment\\Cluster\\BlockGroup\\BlockAdditions\\BlockMore\\BlockAddID", parent_id: Some(0xA6), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: Some("1"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xF0, name: "CueRelativePosition", path: "\\Segment\\Cues\\CuePoint\\CueTrackPositions\\CueRelativePosition", parent_id: Some(0xB7), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xF1, name: "CueClusterPosition", path: "\\Segment\\Cues\\CuePoint\\CueTrackPositions\\CueClusterPosition", parent_id: Some(0xB7), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xF7, name: "CueTrack", path: "\\Segment\\Cues\\CuePoint\\CueTrackPositions\\CueTrack", parent_id: Some(0xB7), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xFA, name: "ReferencePriority", path: "\\Segment\\Cluster\\BlockGroup\\ReferencePriority", parent_id: Some(0xA0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0xFB, name: "ReferenceBlock", path: "\\Segment\\Cluster\\BlockGroup\\ReferenceBlock", parent_id: Some(0xA0), element_type: ElementType::Integer, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0xFD, name: "ReferenceVirtual", path: "\\Segment\\Cluster\\BlockGroup\\ReferenceVirtual", parent_id: Some(0xA0), element_type: ElementType::Integer, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x41A4, name: "BlockAddIDName", path: "\\Segment\\Tracks\\TrackEntry\\BlockAdditionMapping\\BlockAddIDName", parent_id: Some(0x41E4), element_type: ElementType::AsciiString, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x41E4, name: "BlockAdditionMapping", path: "\\Segment\\Tracks\\TrackEntry\\BlockAdditionMapping", parent_id: Some(0xAE), element_type: ElementType::Master, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x41E7, name: "BlockAddIDType", path: "\\Segment\\Tracks\\TrackEntry\\BlockAdditionMapping\\BlockAddIDType", parent_id: Some(0x41E4), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x41ED, name: "BlockAddIDExtraData", path: "\\Segment\\Tracks\\TrackEntry\\BlockAdditionMapping\\BlockAddIDExtraData", parent_id: Some(0x41E4), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x41F0, name: "BlockAddIDValue", path: "\\Segment\\Tracks\\TrackEntry\\BlockAdditionMapping\\BlockAddIDValue", parent_id: Some(0x41E4), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some(">=2"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x4254, name: "ContentCompAlgo", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentCompression\\ContentCompAlgo", parent_id: Some(0x5034), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x4255, name: "ContentCompSettings", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentCompression\\ContentCompSettings", parent_id: Some(0x5034), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x42F2, name: "EBMLMaxIDLength", path: "\\EBML\\EBMLMaxIDLength", parent_id: Some(0x1A45DFA3), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("4"), length: None, default: Some("4"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x42F3, name: "EBMLMaxSizeLength", path: "\\EBML\\EBMLMaxSizeLength", parent_id: Some(0x1A45DFA3), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("1-8"), length: None, default: Some("8"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x437C, name: "ChapLanguage", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterDisplay\\ChapLanguage", parent_id: Some(0x80), element_type: ElementType::AsciiString, min_occurs: 1, max_occurs: None, range: None, length: None, default: Some("eng"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x437D, name: "ChapLanguageBCP47", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterDisplay\\ChapLanguageBCP47", parent_id: Some(0x80), element_type: ElementType::AsciiString, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x437E, name: "ChapCountry", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterDisplay\\ChapCountry", parent_id: Some(0x80), element_type: ElementType::AsciiString, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x4444, name: "SegmentFamily", path: "\\Segment\\Info\\SegmentFamily", parent_id: Some(0x1549A966), element_type: ElementType::Binary, min_occurs: 0, max_occurs: None, range: None, length: Some("16"), default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x4461, name: "DateUTC", path: "\\Segment\\Info\\DateUTC", parent_id: Some(0x1549A966), element_type: ElementType::Date, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x447A, name: "TagLanguage", path: "\\Segment\\Tags\\Tag\\+SimpleTag\\TagLanguage", parent_id: Some(0x67C8), element_type: ElementType::AsciiString, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("und"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x447B, name: "TagLanguageBCP47", path: "\\Segment\\Tags\\Tag\\+SimpleTag\\TagLanguageBCP47", parent_id: Some(0x67C8), element_type: ElementType::AsciiString, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x4484, name: "TagDefault", path: "\\Segment\\Tags\\Tag\\+SimpleTag\\TagDefault", parent_id: Some(0x67C8), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("0-1"), length: None, default: Some("1"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x4485, name: "TagBinary", path: "\\Segment\\Tags\\Tag\\+SimpleTag\\TagBinary", parent_id: Some(0x67C8), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x4487, name: "TagString", path: "\\Segment\\Tags\\Tag\\+SimpleTag\\TagString", parent_id: Some(0x67C8), element_type: ElementType::Utf8, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x4489, name: "Duration", path: "\\Segment\\Info\\Duration", parent_id: Some(0x1549A966), element_type: ElementType::Float, min_occurs: 0, max_occurs: Some(1), range: Some("> 0x0p+0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x44B4, name: "TagDefaultBogus", path: "\\Segment\\Tags\\Tag\\+SimpleTag\\TagDefaultBogus", parent_id: Some(0x67C8), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("0-1"), length: None, default: Some("1"), min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x450D, name: "ChapProcessPrivate", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapProcess\\ChapProcessPrivate", parent_id: Some(0x6944), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x4520, name: "EditionDisplay", path: "\\Segment\\Chapters\\EditionEntry\\EditionDisplay", parent_id: Some(0x45B9), element_type: ElementType::Master, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 5, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x4521, name: "EditionString", path: "\\Segment\\Chapters\\EditionEntry\\EditionDisplay\\EditionString", parent_id: Some(0x4520), element_type: ElementType::Utf8, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 5, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x4588, name: "ChapterSkipType", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterSkipType", parent_id: Some(0xB6), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 5, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x4598, name: "ChapterFlagEnabled", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterFlagEnabled", parent_id: Some(0xB6), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("0-1"), length: None, default: Some("1"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x45A3, name: "TagName", path: "\\Segment\\Tags\\Tag\\+SimpleTag\\TagName", parent_id: Some(0x67C8), element_type: ElementType::Utf8, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x45B9, name: "EditionEntry", path: "\\Segment\\Chapters\\EditionEntry", parent_id: Some(0x1043A770), element_type: ElementType::Master, min_occurs: 1, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x45BC, name: "EditionUID", path: "\\Segment\\Chapters\\EditionEntry\\EditionUID", parent_id: Some(0x45B9), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x45BD, name: "EditionFlagHidden", path: "\\Segment\\Chapters\\EditionEntry\\EditionFlagHidden", parent_id: Some(0x45B9), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("0-1"), length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x45DB, name: "EditionFlagDefault", path: "\\Segment\\Chapters\\EditionEntry\\EditionFlagDefault", parent_id: Some(0x45B9), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("0-1"), length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x45DD, name: "EditionFlagOrdered", path: "\\Segment\\Chapters\\EditionEntry\\EditionFlagOrdered", parent_id: Some(0x45B9), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("0-1"), length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x45E4, name: "EditionLanguageIETF", path: "\\Segment\\Chapters\\EditionEntry\\EditionDisplay\\EditionLanguageIETF", parent_id: Some(0x4520), element_type: ElementType::AsciiString, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 5, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x465C, name: "FileData", path: "\\Segment\\Attachments\\AttachedFile\\FileData", parent_id: Some(0x61A7), element_type: ElementType::Binary, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x4660, name: "FileMediaType", path: "\\Segment\\Attachments\\AttachedFile\\FileMediaType", parent_id: Some(0x61A7), element_type: ElementType::AsciiString, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x4661, name: "FileUsedStartTime", path: "\\Segment\\Attachments\\AttachedFile\\FileUsedStartTime", parent_id: Some(0x61A7), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x4662, name: "FileUsedEndTime", path: "\\Segment\\Attachments\\AttachedFile\\FileUsedEndTime", parent_id: Some(0x61A7), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x466E, name: "FileName", path: "\\Segment\\Attachments\\AttachedFile\\FileName", parent_id: Some(0x61A7), element_type: ElementType::Utf8, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x4675, name: "FileReferral", path: "\\Segment\\Attachments\\AttachedFile\\FileReferral", parent_id: Some(0x61A7), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x467E, name: "FileDescription", path: "\\Segment\\Attachments\\AttachedFile\\FileDescription", parent_id: Some(0x61A7), element_type: ElementType::Utf8, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x46AE, name: "FileUID", path: "\\Segment\\Attachments\\AttachedFile\\FileUID", parent_id: Some(0x61A7), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x47E1, name: "ContentEncAlgo", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentEncryption\\ContentEncAlgo", parent_id: Some(0x5035), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x47E2, name: "ContentEncKeyID", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentEncryption\\ContentEncKeyID", parent_id: Some(0x5035), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x47E3, name: "ContentSignature", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentEncryption\\ContentSignature", parent_id: Some(0x5035), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x47E4, name: "ContentSigKeyID", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentEncryption\\ContentSigKeyID", parent_id: Some(0x5035), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x47E5, name: "ContentSigAlgo", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentEncryption\\ContentSigAlgo", parent_id: Some(0x5035), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 1, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x47E6, name: "ContentSigHashAlgo", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentEncryption\\ContentSigHashAlgo", parent_id: Some(0x5035), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 1, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x47E7, name: "ContentEncAESSettings", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentEncryption\\ContentEncAESSettings", parent_id: Some(0x5035), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x47E8, name: "AESSettingsCipherMode", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentEncryption\\ContentEncAESSettings\\AESSettingsCipherMode", parent_id: Some(0x47E7), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x4D80, name: "MuxingApp", path: "\\Segment\\Info\\MuxingApp", parent_id: Some(0x1549A966), element_type: ElementType::Utf8, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x4DBB, name: "Seek", path: "\\Segment\\SeekHead\\Seek", parent_id: Some(0x114D9B74), element_type: ElementType::Master, min_occurs: 1, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x5031, name: "ContentEncodingOrder", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentEncodingOrder", parent_id: Some(0x6240), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x5032, name: "ContentEncodingScope", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentEncodingScope", parent_id: Some(0x6240), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: Some("1"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x5033, name: "ContentEncodingType", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentEncodingType", parent_id: Some(0x6240), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x5034, name: "ContentCompression", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentCompression", parent_id: Some(0x6240), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x5035, name: "ContentEncryption", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding\\ContentEncryption", parent_id: Some(0x6240), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x52F1, name: "Emphasis", path: "\\Segment\\Tracks\\TrackEntry\\Audio\\Emphasis", parent_id: Some(0xE1), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 5, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x535F, name: "CueRefNumber", path: "\\Segment\\Cues\\CuePoint\\CueTrackPositions\\CueReference\\CueRefNumber", parent_id: Some(0xDB), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("not 0"), length: None, default: Some("1"), min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x536E, name: "Name", path: "\\Segment\\Tracks\\TrackEntry\\Name", parent_id: Some(0xAE), element_type: ElementType::Utf8, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x5378, name: "CueBlockNumber", path: "\\Segment\\Cues\\CuePoint\\CueTrackPositions\\CueBlockNumber", parent_id: Some(0xB7), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x537F, name: "TrackOffset", path: "\\Segment\\Tracks\\TrackEntry\\TrackOffset", parent_id: Some(0xAE), element_type: ElementType::Integer, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x53AB, name: "SeekID", path: "\\Segment\\SeekHead\\Seek\\SeekID", parent_id: Some(0x4DBB), element_type: ElementType::Binary, min_occurs: 1, max_occurs: Some(1), range: None, length: Some("4"), default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x53AC, name: "SeekPosition", path: "\\Segment\\SeekHead\\Seek\\SeekPosition", parent_id: Some(0x4DBB), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x53B8, name: "StereoMode", path: "\\Segment\\Tracks\\TrackEntry\\Video\\StereoMode", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 3, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x53B9, name: "OldStereoMode", path: "\\Segment\\Tracks\\TrackEntry\\Video\\OldStereoMode", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: 2, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x53C0, name: "AlphaMode", path: "\\Segment\\Tracks\\TrackEntry\\Video\\AlphaMode", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 3, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x54AA, name: "PixelCropBottom", path: "\\Segment\\Tracks\\TrackEntry\\Video\\PixelCropBottom", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x54B0, name: "DisplayWidth", path: "\\Segment\\Tracks\\TrackEntry\\Video\\DisplayWidth", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x54B2, name: "DisplayUnit", path: "\\Segment\\Tracks\\TrackEntry\\Video\\DisplayUnit", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x54B3, name: "AspectRatioType", path: "\\Segment\\Tracks\\TrackEntry\\Video\\AspectRatioType", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x54BA, name: "DisplayHeight", path: "\\Segment\\Tracks\\TrackEntry\\Video\\DisplayHeight", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x54BB, name: "PixelCropTop", path: "\\Segment\\Tracks\\TrackEntry\\Video\\PixelCropTop", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x54CC, name: "PixelCropLeft", path: "\\Segment\\Tracks\\TrackEntry\\Video\\PixelCropLeft", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x54DD, name: "PixelCropRight", path: "\\Segment\\Tracks\\TrackEntry\\Video\\PixelCropRight", parent_id: Some(0xE0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55AA, name: "FlagForced", path: "\\Segment\\Tracks\\TrackEntry\\FlagForced", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("0-1"), length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55AB, name: "FlagHearingImpaired", path: "\\Segment\\Tracks\\TrackEntry\\FlagHearingImpaired", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("0-1"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x55AC, name: "FlagVisualImpaired", path: "\\Segment\\Tracks\\TrackEntry\\FlagVisualImpaired", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("0-1"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x55AD, name: "FlagTextDescriptions", path: "\\Segment\\Tracks\\TrackEntry\\FlagTextDescriptions", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("0-1"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x55AE, name: "FlagOriginal", path: "\\Segment\\Tracks\\TrackEntry\\FlagOriginal", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("0-1"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x55AF, name: "FlagCommentary", path: "\\Segment\\Tracks\\TrackEntry\\FlagCommentary", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("0-1"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x55B0, name: "Colour", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour", parent_id: Some(0xE0), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55B1, name: "MatrixCoefficients", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\MatrixCoefficients", parent_id: Some(0x55B0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("2"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55B2, name: "BitsPerChannel", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\BitsPerChannel", parent_id: Some(0x55B0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55B3, name: "ChromaSubsamplingHorz", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\ChromaSubsamplingHorz", parent_id: Some(0x55B0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55B4, name: "ChromaSubsamplingVert", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\ChromaSubsamplingVert", parent_id: Some(0x55B0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55B5, name: "CbSubsamplingHorz", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\CbSubsamplingHorz", parent_id: Some(0x55B0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55B6, name: "CbSubsamplingVert", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\CbSubsamplingVert", parent_id: Some(0x55B0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55B7, name: "ChromaSitingHorz", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\ChromaSitingHorz", parent_id: Some(0x55B0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55B8, name: "ChromaSitingVert", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\ChromaSitingVert", parent_id: Some(0x55B0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55B9, name: "Range", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\Range", parent_id: Some(0x55B0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55BA, name: "TransferCharacteristics", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\TransferCharacteristics", parent_id: Some(0x55B0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("2"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55BB, name: "Primaries", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\Primaries", parent_id: Some(0x55B0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("2"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55BC, name: "MaxCLL", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\MaxCLL", parent_id: Some(0x55B0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55BD, name: "MaxFALL", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\MaxFALL", parent_id: Some(0x55B0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55D0, name: "MasteringMetadata", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\MasteringMetadata", parent_id: Some(0x55B0), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55D1, name: "PrimaryRChromaticityX", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\MasteringMetadata\\PrimaryRChromaticityX", parent_id: Some(0x55D0), element_type: ElementType::Float, min_occurs: 0, max_occurs: Some(1), range: Some("0x0p+0-0x1p+0"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55D2, name: "PrimaryRChromaticityY", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\MasteringMetadata\\PrimaryRChromaticityY", parent_id: Some(0x55D0), element_type: ElementType::Float, min_occurs: 0, max_occurs: Some(1), range: Some("0x0p+0-0x1p+0"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55D3, name: "PrimaryGChromaticityX", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\MasteringMetadata\\PrimaryGChromaticityX", parent_id: Some(0x55D0), element_type: ElementType::Float, min_occurs: 0, max_occurs: Some(1), range: Some("0x0p+0-0x1p+0"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55D4, name: "PrimaryGChromaticityY", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\MasteringMetadata\\PrimaryGChromaticityY", parent_id: Some(0x55D0), element_type: ElementType::Float, min_occurs: 0, max_occurs: Some(1), range: Some("0x0p+0-0x1p+0"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55D5, name: "PrimaryBChromaticityX", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\MasteringMetadata\\PrimaryBChromaticityX", parent_id: Some(0x55D0), element_type: ElementType::Float, min_occurs: 0, max_occurs: Some(1), range: Some("0x0p+0-0x1p+0"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55D6, name: "PrimaryBChromaticityY", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\MasteringMetadata\\PrimaryBChromaticityY", parent_id: Some(0x55D0), element_type: ElementType::Float, min_occurs: 0, max_occurs: Some(1), range: Some("0x0p+0-0x1p+0"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55D7, name: "WhitePointChromaticityX", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\MasteringMetadata\\WhitePointChromaticityX", parent_id: Some(0x55D0), element_type: ElementType::Float, min_occurs: 0, max_occurs: Some(1), range: Some("0x0p+0-0x1p+0"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55D8, name: "WhitePointChromaticityY", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\MasteringMetadata\\WhitePointChromaticityY", parent_id: Some(0x55D0), element_type: ElementType::Float, min_occurs: 0, max_occurs: Some(1), range: Some("0x0p+0-0x1p+0"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55D9, name: "LuminanceMax", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\MasteringMetadata\\LuminanceMax", parent_id: Some(0x55D0), element_type: ElementType::Float, min_occurs: 0, max_occurs: Some(1), range: Some(">= 0x0p+0"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55DA, name: "LuminanceMin", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Colour\\MasteringMetadata\\LuminanceMin", parent_id: Some(0x55D0), element_type: ElementType::Float, min_occurs: 0, max_occurs: Some(1), range: Some(">= 0x0p+0"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x55EE, name: "MaxBlockAdditionID", path: "\\Segment\\Tracks\\TrackEntry\\MaxBlockAdditionID", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x5654, name: "ChapterStringUID", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterStringUID", parent_id: Some(0xB6), element_type: ElementType::Utf8, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 3, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x56AA, name: "CodecDelay", path: "\\Segment\\Tracks\\TrackEntry\\CodecDelay", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x56BB, name: "SeekPreRoll", path: "\\Segment\\Tracks\\TrackEntry\\SeekPreRoll", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x5741, name: "WritingApp", path: "\\Segment\\Info\\WritingApp", parent_id: Some(0x1549A966), element_type: ElementType::Utf8, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x5854, name: "SilentTracks", path: "\\Segment\\Cluster\\SilentTracks", parent_id: Some(0x1F43B675), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x58D7, name: "SilentTrackNumber", path: "\\Segment\\Cluster\\SilentTracks\\SilentTrackNumber", parent_id: Some(0x5854), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x61A7, name: "AttachedFile", path: "\\Segment\\Attachments\\AttachedFile", parent_id: Some(0x1941A469), element_type: ElementType::Master, min_occurs: 1, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x6240, name: "ContentEncoding", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings\\ContentEncoding", parent_id: Some(0x6D80), element_type: ElementType::Master, min_occurs: 1, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x6264, name: "BitDepth", path: "\\Segment\\Tracks\\TrackEntry\\Audio\\BitDepth", parent_id: Some(0xE1), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x63A2, name: "CodecPrivate", path: "\\Segment\\Tracks\\TrackEntry\\CodecPrivate", parent_id: Some(0xAE), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x63C0, name: "Targets", path: "\\Segment\\Tags\\Tag\\Targets", parent_id: Some(0x7373), element_type: ElementType::Master, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x63C3, name: "ChapterPhysicalEquiv", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterPhysicalEquiv", parent_id: Some(0xB6), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x63C4, name: "TagChapterUID", path: "\\Segment\\Tags\\Tag\\Targets\\TagChapterUID", parent_id: Some(0x63C0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: None, range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x63C5, name: "TagTrackUID", path: "\\Segment\\Tags\\Tag\\Targets\\TagTrackUID", parent_id: Some(0x63C0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: None, range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x63C6, name: "TagAttachmentUID", path: "\\Segment\\Tags\\Tag\\Targets\\TagAttachmentUID", parent_id: Some(0x63C0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: None, range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x63C7, name: "TagBlockAddIDValue", path: "\\Segment\\Tags\\Tag\\Targets\\TagBlockAddIDValue", parent_id: Some(0x63C0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: None, range: None, length: None, default: Some("0"), min_ver: 5, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x63C9, name: "TagEditionUID", path: "\\Segment\\Tags\\Tag\\Targets\\TagEditionUID", parent_id: Some(0x63C0), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: None, range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x63CA, name: "TargetType", path: "\\Segment\\Tags\\Tag\\Targets\\TargetType", parent_id: Some(0x63C0), element_type: ElementType::AsciiString, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x6624, name: "TrackTranslate", path: "\\Segment\\Tracks\\TrackEntry\\TrackTranslate", parent_id: Some(0xAE), element_type: ElementType::Master, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x66A5, name: "TrackTranslateTrackID", path: "\\Segment\\Tracks\\TrackEntry\\TrackTranslate\\TrackTranslateTrackID", parent_id: Some(0x6624), element_type: ElementType::Binary, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x66BF, name: "TrackTranslateCodec", path: "\\Segment\\Tracks\\TrackEntry\\TrackTranslate\\TrackTranslateCodec", parent_id: Some(0x6624), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x66FC, name: "TrackTranslateEditionUID", path: "\\Segment\\Tracks\\TrackEntry\\TrackTranslate\\TrackTranslateEditionUID", parent_id: Some(0x6624), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x67C8, name: "SimpleTag", path: "\\Segment\\Tags\\Tag\\+SimpleTag", parent_id: Some(0x7373), element_type: ElementType::Master, min_occurs: 1, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: true, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x68CA, name: "TargetTypeValue", path: "\\Segment\\Tags\\Tag\\Targets\\TargetTypeValue", parent_id: Some(0x63C0), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: Some("50"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x6911, name: "ChapProcessCommand", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapProcess\\ChapProcessCommand", parent_id: Some(0x6944), element_type: ElementType::Master, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x6922, name: "ChapProcessTime", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapProcess\\ChapProcessCommand\\ChapProcessTime", parent_id: Some(0x6911), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x6924, name: "ChapterTranslate", path: "\\Segment\\Info\\ChapterTranslate", parent_id: Some(0x1549A966), element_type: ElementType::Master, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x6933, name: "ChapProcessData", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapProcess\\ChapProcessCommand\\ChapProcessData", parent_id: Some(0x6911), element_type: ElementType::Binary, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x6944, name: "ChapProcess", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapProcess", parent_id: Some(0xB6), element_type: ElementType::Master, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x6955, name: "ChapProcessCodecID", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapProcess\\ChapProcessCodecID", parent_id: Some(0x6944), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x69A5, name: "ChapterTranslateID", path: "\\Segment\\Info\\ChapterTranslate\\ChapterTranslateID", parent_id: Some(0x6924), element_type: ElementType::Binary, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x69BF, name: "ChapterTranslateCodec", path: "\\Segment\\Info\\ChapterTranslate\\ChapterTranslateCodec", parent_id: Some(0x6924), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x69FC, name: "ChapterTranslateEditionUID", path: "\\Segment\\Info\\ChapterTranslate\\ChapterTranslateEditionUID", parent_id: Some(0x6924), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x6D80, name: "ContentEncodings", path: "\\Segment\\Tracks\\TrackEntry\\ContentEncodings", parent_id: Some(0xAE), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x6DE7, name: "MinCache", path: "\\Segment\\Tracks\\TrackEntry\\MinCache", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x6DF8, name: "MaxCache", path: "\\Segment\\Tracks\\TrackEntry\\MaxCache", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x6E67, name: "ChapterSegmentUUID", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterSegmentUUID", parent_id: Some(0xB6), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: Some("16"), default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x6EBC, name: "ChapterSegmentEditionUID", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterSegmentEditionUID", parent_id: Some(0xB6), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x6FAB, name: "TrackOverlay", path: "\\Segment\\Tracks\\TrackEntry\\TrackOverlay", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x7373, name: "Tag", path: "\\Segment\\Tags\\Tag", parent_id: Some(0x1254C367), element_type: ElementType::Master, min_occurs: 1, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x7384, name: "SegmentFilename", path: "\\Segment\\Info\\SegmentFilename", parent_id: Some(0x1549A966), element_type: ElementType::Utf8, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x73A4, name: "SegmentUUID", path: "\\Segment\\Info\\SegmentUUID", parent_id: Some(0x1549A966), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: Some("16"), default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x73C4, name: "ChapterUID", path: "\\Segment\\Chapters\\EditionEntry\\+ChapterAtom\\ChapterUID", parent_id: Some(0xB6), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x73C5, name: "TrackUID", path: "\\Segment\\Tracks\\TrackEntry\\TrackUID", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x7446, name: "AttachmentLink", path: "\\Segment\\Tracks\\TrackEntry\\AttachmentLink", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: 3, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x75A1, name: "BlockAdditions", path: "\\Segment\\Cluster\\BlockGroup\\BlockAdditions", parent_id: Some(0xA0), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x75A2, name: "DiscardPadding", path: "\\Segment\\Cluster\\BlockGroup\\DiscardPadding", parent_id: Some(0xA0), element_type: ElementType::Integer, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x7670, name: "Projection", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Projection", parent_id: Some(0xE0), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x7671, name: "ProjectionType", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Projection\\ProjectionType", parent_id: Some(0x7670), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("0"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x7672, name: "ProjectionPrivate", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Projection\\ProjectionPrivate", parent_id: Some(0x7670), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x7673, name: "ProjectionPoseYaw", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Projection\\ProjectionPoseYaw", parent_id: Some(0x7670), element_type: ElementType::Float, min_occurs: 1, max_occurs: Some(1), range: Some(">= -0xB4p+0, <= 0xB4p+0"), length: None, default: Some("0x0p+0"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x7674, name: "ProjectionPosePitch", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Projection\\ProjectionPosePitch", parent_id: Some(0x7670), element_type: ElementType::Float, min_occurs: 1, max_occurs: Some(1), range: Some(">= -0x5Ap+0, <= 0x5Ap+0"), length: None, default: Some("0x0p+0"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x7675, name: "ProjectionPoseRoll", path: "\\Segment\\Tracks\\TrackEntry\\Video\\Projection\\ProjectionPoseRoll", parent_id: Some(0x7670), element_type: ElementType::Float, min_occurs: 1, max_occurs: Some(1), range: Some(">= -0xB4p+0, <= 0xB4p+0"), length: None, default: Some("0x0p+0"), min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x78B5, name: "OutputSamplingFrequency", path: "\\Segment\\Tracks\\TrackEntry\\Audio\\OutputSamplingFrequency", parent_id: Some(0xE1), element_type: ElementType::Float, min_occurs: 0, max_occurs: Some(1), range: Some("> 0x0p+0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x7BA9, name: "Title", path: "\\Segment\\Info\\Title", parent_id: Some(0x1549A966), element_type: ElementType::Utf8, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x7D7B, name: "ChannelPositions", path: "\\Segment\\Tracks\\TrackEntry\\Audio\\ChannelPositions", parent_id: Some(0xE1), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x22B59C, name: "Language", path: "\\Segment\\Tracks\\TrackEntry\\Language", parent_id: Some(0xAE), element_type: ElementType::AsciiString, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: Some("eng"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x22B59D, name: "LanguageBCP47", path: "\\Segment\\Tracks\\TrackEntry\\LanguageBCP47", parent_id: Some(0xAE), element_type: ElementType::AsciiString, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x23314F, name: "TrackTimestampScale", path: "\\Segment\\Tracks\\TrackEntry\\TrackTimestampScale", parent_id: Some(0xAE), element_type: ElementType::Float, min_occurs: 1, max_occurs: Some(1), range: Some("> 0x0p+0"), length: None, default: Some("0x1p+0"), min_ver: 1, max_ver: 3, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x234E7A, name: "DefaultDecodedFieldDuration", path: "\\Segment\\Tracks\\TrackEntry\\DefaultDecodedFieldDuration", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 4, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x2383E3, name: "FrameRate", path: "\\Segment\\Tracks\\TrackEntry\\Video\\FrameRate", parent_id: Some(0xE0), element_type: ElementType::Float, min_occurs: 0, max_occurs: Some(1), range: Some("> 0x0p+0"), length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x23E383, name: "DefaultDuration", path: "\\Segment\\Tracks\\TrackEntry\\DefaultDuration", parent_id: Some(0xAE), element_type: ElementType::Uinteger, min_occurs: 0, max_occurs: Some(1), range: Some("not 0"), length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x258688, name: "CodecName", path: "\\Segment\\Tracks\\TrackEntry\\CodecName", parent_id: Some(0xAE), element_type: ElementType::Utf8, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x26B240, name: "CodecDownloadURL", path: "\\Segment\\Tracks\\TrackEntry\\CodecDownloadURL", parent_id: Some(0xAE), element_type: ElementType::AsciiString, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x2AD7B1, name: "TimestampScale", path: "\\Segment\\Info\\TimestampScale", parent_id: Some(0x1549A966), element_type: ElementType::Uinteger, min_occurs: 1, max_occurs: Some(1), range: Some("not 0"), length: None, default: Some("1000000"), min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x2EB524, name: "UncompressedFourCC", path: "\\Segment\\Tracks\\TrackEntry\\Video\\UncompressedFourCC", parent_id: Some(0xE0), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: Some("4"), default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x2FB523, name: "GammaValue", path: "\\Segment\\Tracks\\TrackEntry\\Video\\GammaValue", parent_id: Some(0xE0), element_type: ElementType::Float, min_occurs: 0, max_occurs: Some(1), range: Some("> 0x0p+0"), length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x3A9697, name: "CodecSettings", path: "\\Segment\\Tracks\\TrackEntry\\CodecSettings", parent_id: Some(0xAE), element_type: ElementType::Utf8, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x3B4040, name: "CodecInfoURL", path: "\\Segment\\Tracks\\TrackEntry\\CodecInfoURL", parent_id: Some(0xAE), element_type: ElementType::AsciiString, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 0, max_ver: 0, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x3C83AB, name: "PrevFilename", path: "\\Segment\\Info\\PrevFilename", parent_id: Some(0x1549A966), element_type: ElementType::Utf8, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x3CB923, name: "PrevUUID", path: "\\Segment\\Info\\PrevUUID", parent_id: Some(0x1549A966), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: Some("16"), default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x3E83BB, name: "NextFilename", path: "\\Segment\\Info\\NextFilename", parent_id: Some(0x1549A966), element_type: ElementType::Utf8, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x3EB923, name: "NextUUID", path: "\\Segment\\Info\\NextUUID", parent_id: Some(0x1549A966), element_type: ElementType::Binary, min_occurs: 0, max_occurs: Some(1), range: None, length: Some("16"), default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x1043A770, name: "Chapters", path: "\\Segment\\Chapters", parent_id: Some(0x18538067), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: true, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x114D9B74, name: "SeekHead", path: "\\Segment\\SeekHead", parent_id: Some(0x18538067), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(2), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x1254C367, name: "Tags", path: "\\Segment\\Tags", parent_id: Some(0x18538067), element_type: ElementType::Master, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x1549A966, name: "Info", path: "\\Segment\\Info", parent_id: Some(0x18538067), element_type: ElementType::Master, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: true, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x1654AE6B, name: "Tracks", path: "\\Segment\\Tracks", parent_id: Some(0x18538067), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: true, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x18538067, name: "Segment", path: "\\Segment", parent_id: None, element_type: ElementType::Master, min_occurs: 1, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: true, webm: true },
    ElementDef { id: 0x1941A469, name: "Attachments", path: "\\Segment\\Attachments", parent_id: Some(0x18538067), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: false },
    ElementDef { id: 0x1C53BB6B, name: "Cues", path: "\\Segment\\Cues", parent_id: Some(0x18538067), element_type: ElementType::Master, min_occurs: 0, max_occurs: Some(1), range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: false, webm: true },
    ElementDef { id: 0x1F43B675, name: "Cluster", path: "\\Segment\\Cluster", parent_id: Some(0x18538067), element_type: ElementType::Master, min_occurs: 0, max_occurs: None, range: None, length: None, default: None, min_ver: 1, max_ver: NO_MAX_VER, recursive: false, recurring: false, unknown_size_allowed: true, webm: true },
];

/// Look up an element by ID across [`SCHEMA`] and [`EBML_SUPPLEMENT`].
pub fn element_def(id: u32) -> Option<&'static ElementDef> {
    match SCHEMA.binary_search_by_key(&id, |e| e.id) {
        Ok(i) => Some(&SCHEMA[i]),
        Err(_) => EBML_SUPPLEMENT
            .binary_search_by_key(&id, |e| e.id)
            .ok()
            .map(|i| &EBML_SUPPLEMENT[i]),
    }
}

// ---------------------------------------------------------------------------
// Whole-document schema validation.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::Result;

use crate::{ebml, ids};

/// Why [`validate`] flagged an element occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaFindingKind {
    /// The element ID appears in neither [`SCHEMA`] nor
    /// [`EBML_SUPPLEMENT`]. Informational: RFC 8794 §11.1.1 lets a
    /// Reader skip elements it does not know, and the ID may belong to
    /// a `DocTypeExtension` or a newer schema (the removed legacy
    /// Signature family also lands here — the schema dropped it).
    UnknownId,
    /// The element is one of the removed legacy Signature-family
    /// globals (`SignatureSlot` + children, staged
    /// `legacy-element-ids.md`) — absent from the schema by design but
    /// recognised so a legacy file is not reported as carrying unknown
    /// data. The validator descends the family's masters so children
    /// classify individually. Informational.
    KnownLegacy,
    /// The element occurs under a master the schema does not name as
    /// its parent (recursion and the `Void` / `CRC-32` globals are
    /// exempt). Violation.
    WrongParent {
        /// The schema parent (`None` for a root-level element).
        expected: Option<u32>,
        /// The master it actually occurred in (`None` = document root).
        actual: Option<u32>,
    },
    /// The element body violates its type's size shape (uinteger /
    /// integer over 8 octets, float not 0/4/8, date not 0/8) or its
    /// schema `length` attribute. Violation.
    BadLength,
    /// The decoded value falls outside the schema `range`. Violation.
    OutOfRange,
    /// More occurrences in one parent instance than `maxOccurs`
    /// permits (flagged on each excess occurrence). Violation.
    TooManyOccurrences,
    /// A cleanly-walked master instance is missing a child with
    /// `minOccurs >= 1` and no declared default (a defaulted mandatory
    /// element may legally stay off-disk, RFC 8794 §11.1.6.2). Flagged
    /// at the parent's offset with the *missing child's* ID. Violation.
    MissingMandatory,
    /// The element is deprecated (`maxver` below the staged schema
    /// version — the RFC 9559 Reclaimed rows and the `maxver` 2/3
    /// stragglers). Informational.
    Deprecated,
    /// The element's `minver` postdates the document's declared
    /// `DocTypeVersion`. Informational.
    VersionMismatch,
    /// The unknown-size VINT on an element whose schema row does not
    /// carry `unknownsizeallowed` — the walk cannot continue past it.
    /// Violation.
    UnknownSizeNotAllowed,
    /// A `CRC-32` element that is not the first child of its parent
    /// (RFC 8794 §11.3.1: "the CRC-32 Element MUST be the first
    /// ordered EBML Element within its Parent Element"). Violation.
    MisplacedCrc32,
}

impl SchemaFindingKind {
    /// `true` for the kinds that fail [`SchemaReport::is_valid`];
    /// `false` for the informational ones (`UnknownId`, `KnownLegacy`,
    /// `Deprecated`, `VersionMismatch`).
    pub fn is_violation(&self) -> bool {
        !matches!(
            self,
            SchemaFindingKind::UnknownId
                | SchemaFindingKind::KnownLegacy
                | SchemaFindingKind::Deprecated
                | SchemaFindingKind::VersionMismatch
        )
    }
}

/// One flagged occurrence from [`validate`].
#[derive(Clone, Copy, Debug)]
pub struct SchemaFinding {
    /// Absolute file offset of the element's ID byte (for
    /// [`SchemaFindingKind::MissingMandatory`], the *parent's* ID
    /// byte).
    pub offset: u64,
    /// The element ID the finding is about (for `MissingMandatory`,
    /// the missing child's ID).
    pub id: u32,
    /// Why it was flagged.
    pub kind: SchemaFindingKind,
}

/// Maximum number of findings recorded before the report switches to
/// counting only.
const MAX_SCHEMA_FINDINGS: usize = 4096;

/// Maximum master-nesting depth descended (recursive `ChapterAtom` /
/// `SimpleTag` chains are capped like the other in-crate walkers).
const MAX_SCHEMA_DEPTH: usize = 64;

/// The result of a [`validate`] walk.
#[derive(Clone, Debug, Default)]
pub struct SchemaReport {
    /// The EBML header's `DocType` string, when one was found.
    pub doc_type: Option<String>,
    /// The EBML header's `DocTypeVersion` (spec default `1`
    /// materialised once the header has been walked).
    pub doc_type_version: Option<u64>,
    /// Total element headers parsed, masters included.
    pub elements_scanned: u64,
    /// Total violation findings (capped list below; this counter is
    /// exact).
    pub violations: u64,
    /// Total informational findings (`UnknownId` / `Deprecated` /
    /// `VersionMismatch`).
    pub informational: u64,
    /// Every finding in document order, capped at 4096 entries.
    pub findings: Vec<SchemaFinding>,
    /// `true` when more findings occurred than [`findings`]
    /// (SchemaReport::findings) records.
    pub findings_truncated: bool,
    /// First structurally-unwalkable byte (torn header, child
    /// overrunning its parent, unknown-size where not allowed); `None`
    /// when the whole document walked.
    pub scan_stopped_at: Option<u64>,
}

impl SchemaReport {
    /// The headline verdict: zero violation findings and a clean walk.
    /// Informational findings (unknown IDs, deprecated elements,
    /// version mismatches) do not fail validation.
    pub fn is_valid(&self) -> bool {
        self.violations == 0 && self.scan_stopped_at.is_none()
    }
}

/// Parse a schema value literal: a decimal integer or a C hex-float
/// (`0x1.f4p+12`, `-0xB4p+0`, `0x0p+0`). Returns the value as `f64`
/// (exact for every literal the schema carries).
fn parse_schema_number(s: &str) -> Option<f64> {
    let s = s.trim();
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let magnitude = if let Some(hex) = body.strip_prefix("0x") {
        // mantissa[.frac]p±exp
        let (mantissa, exp) = hex.split_once(['p', 'P'])?;
        let exp: i32 = exp.parse().ok()?;
        let (int_part, frac_part) = match mantissa.split_once('.') {
            Some((i, f)) => (i, f),
            None => (mantissa, ""),
        };
        let mut value = if int_part.is_empty() {
            0.0
        } else {
            u64::from_str_radix(int_part, 16).ok()? as f64
        };
        let mut scale = 1.0 / 16.0;
        for c in frac_part.chars() {
            value += c.to_digit(16)? as f64 * scale;
            scale /= 16.0;
        }
        value * (exp as f64).exp2()
    } else {
        body.parse::<f64>().ok()?
    };
    Some(if neg { -magnitude } else { magnitude })
}

/// A parsed schema `range` constraint.
enum RangeCheck {
    NotZero,
    /// Inclusive `lo..=hi`.
    Between(f64, f64),
    /// Exact value.
    Exactly(f64),
    /// `value >(=) bound` and/or `value <(=) bound` terms.
    Comparative(Vec<(std::cmp::Ordering, bool, f64)>),
}

/// Parse the schema's `range` grammar (the full vocabulary the staged
/// schema uses — pinned by the census test; an unknown spelling
/// returns `None` and the value check is skipped).
fn parse_range(range: &str) -> Option<RangeCheck> {
    let range = range.trim();
    if range == "not 0" {
        return Some(RangeCheck::NotZero);
    }
    if range.contains(['<', '>']) {
        // One or two comparative terms, comma-separated.
        let mut terms = Vec::new();
        for term in range.split(',') {
            let term = term.trim();
            let (op, ge, rest) = if let Some(r) = term.strip_prefix(">=") {
                (std::cmp::Ordering::Greater, true, r)
            } else if let Some(r) = term.strip_prefix('>') {
                (std::cmp::Ordering::Greater, false, r)
            } else if let Some(r) = term.strip_prefix("<=") {
                (std::cmp::Ordering::Less, true, r)
            } else {
                // Unknown spelling (no recognised operator) → skip the
                // value check for this range.
                (std::cmp::Ordering::Less, false, term.strip_prefix('<')?)
            };
            terms.push((op, ge, parse_schema_number(rest)?));
        }
        return Some(RangeCheck::Comparative(terms));
    }
    // `a-b` inclusive range or a single exact value. The endpoints in
    // the dash form are non-negative (negative bounds only occur in the
    // comparative form), so the split dash is the one *after* the first
    // character that is not part of a `p+`/`p-` exponent.
    let bytes = range.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b'-' && bytes[i - 1] != b'p' && bytes[i - 1] != b'P' {
            let lo = parse_schema_number(&range[..i])?;
            let hi = parse_schema_number(&range[i + 1..])?;
            return Some(RangeCheck::Between(lo, hi));
        }
    }
    Some(RangeCheck::Exactly(parse_schema_number(range)?))
}

fn range_permits(check: &RangeCheck, value: f64) -> bool {
    match check {
        RangeCheck::NotZero => value != 0.0,
        RangeCheck::Between(lo, hi) => value >= *lo && value <= *hi,
        RangeCheck::Exactly(v) => value == *v,
        RangeCheck::Comparative(terms) => terms.iter().all(|(op, or_eq, bound)| {
            let cmp = value.partial_cmp(bound);
            match cmp {
                Some(c) if c == *op => true,
                Some(std::cmp::Ordering::Equal) => *or_eq,
                _ => false,
            }
        }),
    }
}

/// Validate a whole EBML document against the schema tables: element
/// identity, parent paths, occurrence constraints, type shapes, value
/// ranges, deprecation / version windows, and the RFC 8794 `CRC-32`
/// placement rule. Pure structural walk — O(file) time, O(depth)
/// memory; leaf bodies are read only for the (bounded, <= 8-byte)
/// value checks. Damage stops the walk at the first unwalkable byte
/// and reports it via [`SchemaReport::scan_stopped_at`] rather than
/// erroring.
///
/// The reader is left at an unspecified position.
pub fn validate<R: Read + Seek>(r: &mut R) -> Result<SchemaReport> {
    let mut report = SchemaReport::default();
    r.seek(SeekFrom::Start(0))?;
    let end = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    walk_master(r, 0, end, None, None, 0, &mut report)?;
    if report.doc_type_version.is_none() && report.doc_type.is_some() {
        // Spec default 1 (RFC 8794 §11.2.6) once a header was seen.
        report.doc_type_version = Some(1);
    }
    Ok(report)
}

fn push_finding(report: &mut SchemaReport, offset: u64, id: u32, kind: SchemaFindingKind) {
    if kind.is_violation() {
        report.violations += 1;
    } else {
        report.informational += 1;
    }
    if report.findings.len() < MAX_SCHEMA_FINDINGS {
        report.findings.push(SchemaFinding { offset, id, kind });
    } else {
        report.findings_truncated = true;
    }
}

/// The removed legacy Signature-family element IDs (staged
/// `legacy-element-ids.md`) — absent from the schema by design;
/// classified [`SchemaFindingKind::KnownLegacy`] instead of
/// [`SchemaFindingKind::UnknownId`].
const LEGACY_SIGNATURE_IDS: [u32; 8] = [
    ids::SIGNATURE_SLOT,
    ids::SIGNATURE_ALGO,
    ids::SIGNATURE_HASH,
    ids::SIGNATURE_PUBLIC_KEY,
    ids::SIGNATURE,
    ids::SIGNATURE_ELEMENTS,
    ids::SIGNATURE_ELEMENT_LIST,
    ids::SIGNED_ELEMENT,
];

/// The Signature-family masters the validator descends.
const LEGACY_SIGNATURE_MASTERS: [u32; 3] = [
    ids::SIGNATURE_SLOT,
    ids::SIGNATURE_ELEMENTS,
    ids::SIGNATURE_ELEMENT_LIST,
];

/// IDs that terminate an unknown-size master: any element that is a
/// legal child of an *ancestor* (RFC 8794 §6.2). We approximate with
/// the Top-Level set + Segment, which covers the two
/// `unknownsizeallowed` masters the schema defines.
fn ends_unknown_size(parent: u32, id: u32) -> bool {
    match parent {
        id_ if id_ == ids::SEGMENT => id == ids::SEGMENT,
        id_ if id_ == ids::CLUSTER => {
            id == ids::SEGMENT
                || matches!(element_def(id), Some(d) if d.parent_id == Some(ids::SEGMENT))
        }
        _ => false,
    }
}

/// Walk one master's children over `start..end`.
///
/// `parent` is the enclosing master's def (`None` at document root);
/// `terminate_parent` carries the unknown-size master's ID when the
/// extent is open (children then end where a sibling-of-ancestor ID
/// appears). Returns the offset where walking stopped.
#[allow(clippy::too_many_arguments)]
fn walk_master<R: Read + Seek>(
    r: &mut R,
    start: u64,
    end: u64,
    parent: Option<&'static ElementDef>,
    terminate_parent: Option<u32>,
    depth: usize,
    report: &mut SchemaReport,
) -> Result<u64> {
    let mut pos = start;
    // Per-child-ID occurrence counts within THIS master instance.
    let mut counts: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut children_seen: u64 = 0;
    let mut clean = true;
    let parent_id = parent.map(|p| p.id);
    while pos < end {
        r.seek(SeekFrom::Start(pos))?;
        let header = match ebml::read_element_header(r) {
            Ok(h) => h,
            Err(_) => {
                if report.scan_stopped_at.is_none() {
                    report.scan_stopped_at = Some(pos);
                }
                clean = false;
                pos = end;
                break;
            }
        };
        if let Some(tp) = terminate_parent {
            if ends_unknown_size(tp, header.id) {
                r.seek(SeekFrom::Start(pos))?;
                break;
            }
        }
        report.elements_scanned += 1;
        let body = pos + header.header_len as u64;
        let def = element_def(header.id);

        // --- identity + placement ------------------------------------
        match def {
            None if LEGACY_SIGNATURE_IDS.contains(&header.id) => {
                // Removed legacy Signature family (legacy-element-ids.md):
                // recognised, never reported as unknown data.
                push_finding(report, pos, header.id, SchemaFindingKind::KnownLegacy);
            }
            None => push_finding(report, pos, header.id, SchemaFindingKind::UnknownId),
            Some(d) => {
                let is_global = d.parent_id.is_none() && d.element_type != ElementType::Master
                    || d.id == ids::VOID
                    || d.id == ids::CRC32;
                let legal_here = is_global
                    || d.parent_id == parent_id
                    || (d.recursive && parent_id == Some(d.id))
                    || (parent_id.is_none() && d.parent_id.is_none());
                if !legal_here {
                    push_finding(
                        report,
                        pos,
                        header.id,
                        SchemaFindingKind::WrongParent {
                            expected: d.parent_id,
                            actual: parent_id,
                        },
                    );
                }
                if d.id == ids::CRC32 && children_seen > 0 {
                    push_finding(report, pos, header.id, SchemaFindingKind::MisplacedCrc32);
                }
                if d.is_deprecated() {
                    push_finding(report, pos, header.id, SchemaFindingKind::Deprecated);
                }
                if let Some(ver) = report.doc_type_version {
                    if u64::from(d.min_ver) > ver && d.min_ver != 0 {
                        push_finding(report, pos, header.id, SchemaFindingKind::VersionMismatch);
                    }
                }
                let n = counts.entry(d.id).or_insert(0);
                *n += 1;
                if let Some(max) = d.max_occurs {
                    if *n > max {
                        push_finding(
                            report,
                            pos,
                            header.id,
                            SchemaFindingKind::TooManyOccurrences,
                        );
                    }
                }
            }
        }
        children_seen += 1;

        // --- extent ---------------------------------------------------
        if header.size == ebml::VINT_UNKNOWN_SIZE {
            match def {
                Some(d) if d.unknown_size_allowed => {
                    pos = walk_master(r, body, end, Some(d), Some(d.id), depth + 1, report)?;
                    continue;
                }
                _ => {
                    push_finding(
                        report,
                        pos,
                        header.id,
                        SchemaFindingKind::UnknownSizeNotAllowed,
                    );
                    if report.scan_stopped_at.is_none() {
                        report.scan_stopped_at = Some(pos);
                    }
                    clean = false;
                    pos = end;
                    break;
                }
            }
        }
        let Some(next) = body.checked_add(header.size) else {
            if report.scan_stopped_at.is_none() {
                report.scan_stopped_at = Some(pos);
            }
            clean = false;
            pos = end;
            break;
        };
        if next > end {
            if report.scan_stopped_at.is_none() {
                report.scan_stopped_at = Some(pos);
            }
            clean = false;
            pos = end;
            break;
        }

        // --- value / descent -----------------------------------------
        match def {
            Some(d) if d.element_type == ElementType::Master && depth < MAX_SCHEMA_DEPTH => {
                walk_master(r, body, next, Some(d), None, depth + 1, report)?;
            }
            Some(d) if d.element_type == ElementType::Master => {}
            Some(d) => check_leaf(r, d, pos, body, header.size, report)?,
            None if LEGACY_SIGNATURE_MASTERS.contains(&header.id) && depth < MAX_SCHEMA_DEPTH => {
                // Descend the legacy Signature masters so each child
                // classifies individually (as KnownLegacy). No schema
                // parent context — legacy children carry no schema rows
                // to check placement against.
                walk_master(r, body, next, None, None, depth + 1, report)?;
            }
            None => {}
        }
        pos = next;
    }

    // --- close: mandatory children --------------------------------------
    if clean {
        if let Some(p) = parent {
            for child in SCHEMA.iter().chain(EBML_SUPPLEMENT.iter()) {
                if child.parent_id == Some(p.id)
                    && child.min_occurs >= 1
                    && child.default.is_none()
                    && !counts.contains_key(&child.id)
                {
                    push_finding(report, start, child.id, SchemaFindingKind::MissingMandatory);
                }
            }
        }
    }
    Ok(pos)
}

/// Type-shape, `length`, and `range` checks for one non-master leaf.
/// Reads at most 8 bytes (plus the DocType / DocTypeVersion captures).
fn check_leaf<R: Read + Seek>(
    r: &mut R,
    d: &'static ElementDef,
    offset: u64,
    body: u64,
    size: u64,
    report: &mut SchemaReport,
) -> Result<()> {
    // Fixed `length` attribute ("16", "4", ">0").
    if let Some(len) = d.length {
        let ok = match len.strip_prefix('>') {
            Some(min) => size > min.trim().parse::<u64>().unwrap_or(0),
            None => len.parse::<u64>().map(|l| size == l).unwrap_or(true),
        };
        if !ok {
            push_finding(report, offset, d.id, SchemaFindingKind::BadLength);
            return Ok(());
        }
    }
    let mut value: Option<f64> = None;
    match d.element_type {
        ElementType::Uinteger => {
            if size > 8 {
                push_finding(report, offset, d.id, SchemaFindingKind::BadLength);
                return Ok(());
            }
            r.seek(SeekFrom::Start(body))?;
            let v = ebml::read_uint(r, size as usize)?;
            value = Some(v as f64);
            if d.id == crate::ids::EBML_DOC_TYPE_VERSION {
                report.doc_type_version = Some(v);
            }
        }
        ElementType::Integer => {
            if size > 8 {
                push_finding(report, offset, d.id, SchemaFindingKind::BadLength);
                return Ok(());
            }
            r.seek(SeekFrom::Start(body))?;
            let raw = ebml::read_uint(r, size as usize)?;
            // Sign-extend from the encoded width.
            let v = if size == 0 {
                0
            } else {
                let shift = 64 - 8 * size as u32;
                ((raw << shift) as i64) >> shift
            };
            value = Some(v as f64);
        }
        ElementType::Float => {
            if !matches!(size, 0 | 4 | 8) {
                push_finding(report, offset, d.id, SchemaFindingKind::BadLength);
                return Ok(());
            }
            r.seek(SeekFrom::Start(body))?;
            if size == 4 {
                let mut b = [0u8; 4];
                r.read_exact(&mut b)?;
                value = Some(f32::from_be_bytes(b) as f64);
            } else if size == 8 {
                let mut b = [0u8; 8];
                r.read_exact(&mut b)?;
                value = Some(f64::from_be_bytes(b));
            } else {
                value = Some(0.0);
            }
        }
        ElementType::Date if !matches!(size, 0 | 8) => {
            push_finding(report, offset, d.id, SchemaFindingKind::BadLength);
            return Ok(());
        }
        ElementType::AsciiString
            if d.id == crate::ids::EBML_DOC_TYPE && size <= 64 && report.doc_type.is_none() =>
        {
            r.seek(SeekFrom::Start(body))?;
            if let Ok(s) = ebml::read_string(r, size as usize) {
                report.doc_type = Some(s);
            }
        }
        _ => {}
    }
    if let (Some(v), Some(range)) = (value, d.range) {
        // An absent body (size 0) decodes to the type's zero value; the
        // schema default, not the zero, is what a reader materialises —
        // but on-disk zero-length bodies are still legal encodings of 0
        // and are range-checked as such.
        if let Some(check) = parse_range(range) {
            if !range_permits(&check, v) {
                push_finding(report, offset, d.id, SchemaFindingKind::OutOfRange);
            }
        }
    }
    Ok(())
}
