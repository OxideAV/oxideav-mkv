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
