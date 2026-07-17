//! WebM-profile conformance: the element-subset gating the WebM container
//! guidelines (the staged WebM container document) define on top of the
//! shared Matroska/EBML syntax.
//!
//! WebM is byte-compatible with Matroska but supports only a subset of the
//! element registry: the guidelines tabulate, element by element, whether a
//! WebM reader supports it (`Supported`), rejects it (`Unsupported`), or
//! tolerates it as a legacy leftover (`Deprecated`). This module transcribes
//! that table (keyed by Element ID) and layers a whole-file conformance
//! scanner on top: [`scan`] walks every element of an EBML document with the
//! RFC 8794 walker and classifies each occurrence, so a producer can verify
//! "will a strict WebM reader accept this file?" before shipping it, and a
//! consumer can explain *why* a `.webm` upload was rejected.
//!
//! Elements newer than the guidelines table (e.g. the `Projection` master,
//! `LanguageBCP47`, the `BlockAdditionMapping` family) are classified
//! [`WebmSupport::Unlisted`]: the scan surfaces them informationally but
//! [`WebmConformanceReport::is_conformant`] does not fail on them — the
//! WebM ecosystem adopted several post-table elements, and the guidelines
//! table is the only staged authority on the subset, so anything it does
//! not list is left to the caller's judgement.
//!
//! A few guideline rows name elements whose Element IDs appear in neither
//! RFC 9559 Table 53 nor the staged WebM document itself (the old EBML
//! signature family, `EditionFlagHidden`, `ChapterTrack`,
//! `ChapterTrackNumber` — all `Unsupported` rows); with no ID to key on,
//! occurrences of those classify as `Unlisted`.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::Result;

use crate::{ebml, ids};

/// WebM-guidelines support status for one element (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WebmSupport {
    /// Listed as `Supported`: part of the WebM profile.
    Supported,
    /// Listed as `Deprecated`: legacy leftover a WebM reader tolerates but
    /// a WebM writer must not emit.
    Deprecated,
    /// Listed as `Unsupported`: outside the WebM profile.
    Unsupported,
    /// Not in the guidelines table at all (typically an element registered
    /// after the table was written, e.g. `Projection`).
    Unlisted,
}

/// The WebM guidelines support table, transcribed by Element ID and sorted
/// by ID for binary search. 239 rows: 137 `Supported`, 98 `Unsupported`,
/// 4 `Deprecated`.
const WEBM_SUPPORT_TABLE: &[(u32, WebmSupport)] = &[
    (0x80, WebmSupport::Supported),         // ChapterDisplay
    (0x83, WebmSupport::Supported),         // TrackType
    (0x85, WebmSupport::Supported),         // ChapString
    (0x86, WebmSupport::Supported),         // CodecID
    (0x88, WebmSupport::Supported),         // FlagDefault
    (0x8E, WebmSupport::Unsupported),       // Slices
    (0x91, WebmSupport::Supported),         // ChapterTimeStart
    (0x92, WebmSupport::Supported),         // ChapterTimeEnd
    (0x96, WebmSupport::Unsupported),       // CueRefTime
    (0x97, WebmSupport::Unsupported),       // CueRefCluster
    (0x98, WebmSupport::Unsupported),       // ChapterFlagHidden
    (0x9A, WebmSupport::Supported),         // FlagInterlaced
    (0x9B, WebmSupport::Supported),         // BlockDuration
    (0x9C, WebmSupport::Supported),         // FlagLacing
    (0x9F, WebmSupport::Supported),         // Channels
    (0xA0, WebmSupport::Supported),         // BlockGroup
    (0xA1, WebmSupport::Supported),         // Block
    (0xA2, WebmSupport::Deprecated),        // BlockVirtual
    (0xA3, WebmSupport::Supported),         // SimpleBlock
    (0xA4, WebmSupport::Unsupported),       // CodecState
    (0xA5, WebmSupport::Supported),         // BlockAdditional
    (0xA6, WebmSupport::Supported),         // BlockMore
    (0xA7, WebmSupport::Unsupported),       // Position
    (0xAA, WebmSupport::Unsupported),       // CodecDecodeAll
    (0xAB, WebmSupport::Supported),         // PrevSize
    (0xAE, WebmSupport::Supported),         // TrackEntry
    (0xAF, WebmSupport::Unsupported),       // EncryptedBlock
    (0xB0, WebmSupport::Supported),         // PixelWidth
    (0xB2, WebmSupport::Supported),         // CueDuration
    (0xB3, WebmSupport::Supported),         // CueTime
    (0xB5, WebmSupport::Supported),         // SamplingFrequency
    (0xB6, WebmSupport::Supported),         // ChapterAtom
    (0xB7, WebmSupport::Supported),         // CueTrackPositions
    (0xB9, WebmSupport::Supported),         // FlagEnabled
    (0xBA, WebmSupport::Supported),         // PixelHeight
    (0xBB, WebmSupport::Supported),         // CuePoint
    (0xBF, WebmSupport::Unsupported),       // CRC-32
    (0xC0, WebmSupport::Unsupported),       // TrickTrackUID
    (0xC1, WebmSupport::Unsupported),       // TrickTrackSegmentUID
    (0xC4, WebmSupport::Unsupported),       // TrickMasterTrackSegmentUID
    (0xC6, WebmSupport::Unsupported),       // TrickTrackFlag
    (0xC7, WebmSupport::Unsupported),       // TrickMasterTrackUID
    (0xC8, WebmSupport::Unsupported),       // ReferenceFrame
    (0xC9, WebmSupport::Unsupported),       // ReferenceOffset
    (0xCA, WebmSupport::Unsupported),       // ReferenceTimeCode
    (0xCB, WebmSupport::Unsupported),       // BlockAdditionID
    (0xCC, WebmSupport::Deprecated),        // LaceNumber
    (0xCD, WebmSupport::Unsupported),       // FrameNumber
    (0xCE, WebmSupport::Unsupported),       // Delay
    (0xCF, WebmSupport::Unsupported),       // SliceDuration
    (0xD7, WebmSupport::Supported),         // TrackNumber
    (0xDB, WebmSupport::Unsupported),       // CueReference
    (0xE0, WebmSupport::Supported),         // Video
    (0xE1, WebmSupport::Supported),         // Audio
    (0xE2, WebmSupport::Unsupported),       // TrackOperation
    (0xE3, WebmSupport::Unsupported),       // TrackCombinePlanes
    (0xE4, WebmSupport::Unsupported),       // TrackPlane
    (0xE5, WebmSupport::Unsupported),       // TrackPlaneUID
    (0xE6, WebmSupport::Unsupported),       // TrackPlaneType
    (0xE7, WebmSupport::Supported),         // Timecode
    (0xE8, WebmSupport::Deprecated),        // TimeSlice
    (0xE9, WebmSupport::Unsupported),       // TrackJoinBlocks
    (0xEA, WebmSupport::Unsupported),       // CueCodecState
    (0xEB, WebmSupport::Unsupported),       // CueRefCodecState
    (0xEC, WebmSupport::Supported),         // Void
    (0xED, WebmSupport::Unsupported),       // TrackJoinUID
    (0xEE, WebmSupport::Supported),         // BlockAddID
    (0xF0, WebmSupport::Supported),         // CueRelativePosition
    (0xF1, WebmSupport::Supported),         // CueClusterPosition
    (0xF7, WebmSupport::Supported),         // CueTrack
    (0xFA, WebmSupport::Unsupported),       // ReferencePriority
    (0xFB, WebmSupport::Supported),         // ReferenceBlock
    (0xFD, WebmSupport::Unsupported),       // ReferenceVirtual
    (0x4254, WebmSupport::Unsupported),     // ContentCompAlgo
    (0x4255, WebmSupport::Unsupported),     // ContentCompSettings
    (0x4282, WebmSupport::Supported),       // DocType
    (0x4285, WebmSupport::Supported),       // DocTypeReadVersion
    (0x4286, WebmSupport::Supported),       // EBMLVersion
    (0x4287, WebmSupport::Supported),       // DocTypeVersion
    (0x42F2, WebmSupport::Supported),       // EBMLMaxIDLength
    (0x42F3, WebmSupport::Supported),       // EBMLMaxSizeLength
    (0x42F7, WebmSupport::Supported),       // EBMLReadVersion
    (0x437C, WebmSupport::Supported),       // ChapLanguage
    (0x437E, WebmSupport::Supported),       // ChapCountry
    (0x4444, WebmSupport::Unsupported),     // SegmentFamily
    (0x4461, WebmSupport::Supported),       // DateUTC
    (0x447A, WebmSupport::Supported),       // TagLanguage
    (0x4484, WebmSupport::Supported),       // TagDefault
    (0x4485, WebmSupport::Supported),       // TagBinary
    (0x4487, WebmSupport::Supported),       // TagString
    (0x4489, WebmSupport::Supported),       // Duration
    (0x450D, WebmSupport::Unsupported),     // ChapProcessPrivate
    (0x4598, WebmSupport::Unsupported),     // ChapterFlagEnabled
    (0x45A3, WebmSupport::Supported),       // TagName
    (0x45B9, WebmSupport::Supported),       // EditionEntry
    (0x45BC, WebmSupport::Unsupported),     // EditionUID
    (0x45DB, WebmSupport::Unsupported),     // EditionFlagDefault
    (0x45DD, WebmSupport::Unsupported),     // EditionFlagOrdered
    (0x465C, WebmSupport::Unsupported),     // FileData
    (0x4660, WebmSupport::Unsupported),     // FileMimeType
    (0x4661, WebmSupport::Unsupported),     // FileUsedStartTime
    (0x4662, WebmSupport::Unsupported),     // FileUsedEndTime
    (0x466E, WebmSupport::Unsupported),     // FileName
    (0x4675, WebmSupport::Unsupported),     // FileReferral
    (0x467E, WebmSupport::Unsupported),     // FileDescription
    (0x46AE, WebmSupport::Unsupported),     // FileUID
    (0x47E1, WebmSupport::Supported),       // ContentEncAlgo
    (0x47E2, WebmSupport::Supported),       // ContentEncKeyID
    (0x47E3, WebmSupport::Unsupported),     // ContentSignature
    (0x47E4, WebmSupport::Unsupported),     // ContentSigKeyID
    (0x47E5, WebmSupport::Unsupported),     // ContentSigAlgo
    (0x47E6, WebmSupport::Unsupported),     // ContentSigHashAlgo
    (0x47E7, WebmSupport::Supported),       // ContentEncAESSettings
    (0x47E8, WebmSupport::Supported),       // AESSettingsCipherMode
    (0x4D80, WebmSupport::Supported),       // MuxingApp
    (0x4DBB, WebmSupport::Supported),       // Seek
    (0x5031, WebmSupport::Supported),       // ContentEncodingOrder
    (0x5032, WebmSupport::Supported),       // ContentEncodingScope
    (0x5033, WebmSupport::Supported),       // ContentEncodingType
    (0x5034, WebmSupport::Unsupported),     // ContentCompression
    (0x5035, WebmSupport::Supported),       // ContentEncryption
    (0x535F, WebmSupport::Unsupported),     // CueRefNumber
    (0x536E, WebmSupport::Supported),       // Name
    (0x5378, WebmSupport::Supported),       // CueBlockNumber
    (0x537F, WebmSupport::Unsupported),     // TrackOffset
    (0x53AB, WebmSupport::Supported),       // SeekID
    (0x53AC, WebmSupport::Supported),       // SeekPosition
    (0x53B8, WebmSupport::Supported),       // StereoMode
    (0x53C0, WebmSupport::Supported),       // AlphaMode
    (0x54AA, WebmSupport::Supported),       // PixelCropBottom
    (0x54B0, WebmSupport::Supported),       // DisplayWidth
    (0x54B2, WebmSupport::Supported),       // DisplayUnit
    (0x54B3, WebmSupport::Supported),       // AspectRatioType
    (0x54BA, WebmSupport::Supported),       // DisplayHeight
    (0x54BB, WebmSupport::Supported),       // PixelCropTop
    (0x54CC, WebmSupport::Supported),       // PixelCropLeft
    (0x54DD, WebmSupport::Supported),       // PixelCropRight
    (0x55AA, WebmSupport::Supported),       // FlagForced
    (0x55B0, WebmSupport::Supported),       // Colour
    (0x55B1, WebmSupport::Supported),       // MatrixCoefficients
    (0x55B2, WebmSupport::Supported),       // BitsPerChannel
    (0x55B3, WebmSupport::Supported),       // ChromaSubsamplingHorz
    (0x55B4, WebmSupport::Supported),       // ChromaSubsamplingVert
    (0x55B5, WebmSupport::Supported),       // CbSubsamplingHorz
    (0x55B6, WebmSupport::Supported),       // CbSubsamplingVert
    (0x55B7, WebmSupport::Supported),       // ChromaSitingHorz
    (0x55B8, WebmSupport::Supported),       // ChromaSitingVert
    (0x55B9, WebmSupport::Supported),       // Range
    (0x55BA, WebmSupport::Supported),       // TransferCharacteristics
    (0x55BB, WebmSupport::Supported),       // Primaries
    (0x55BC, WebmSupport::Supported),       // MaxCLL
    (0x55BD, WebmSupport::Supported),       // MaxFALL
    (0x55D0, WebmSupport::Supported),       // MasteringMetadata
    (0x55D1, WebmSupport::Supported),       // PrimaryRChromaticityX
    (0x55D2, WebmSupport::Supported),       // PrimaryRChromaticityY
    (0x55D3, WebmSupport::Supported),       // PrimaryGChromaticityX
    (0x55D4, WebmSupport::Supported),       // PrimaryGChromaticityY
    (0x55D5, WebmSupport::Supported),       // PrimaryBChromaticityX
    (0x55D6, WebmSupport::Supported),       // PrimaryBChromaticityY
    (0x55D7, WebmSupport::Supported),       // WhitePointChromaticityX
    (0x55D8, WebmSupport::Supported),       // WhitePointChromaticityY
    (0x55D9, WebmSupport::Supported),       // LuminanceMax
    (0x55DA, WebmSupport::Supported),       // LuminanceMin
    (0x55EE, WebmSupport::Unsupported),     // MaxBlockAdditionID
    (0x5654, WebmSupport::Supported),       // ChapterStringUID
    (0x56AA, WebmSupport::Supported),       // CodecDelay
    (0x56BB, WebmSupport::Supported),       // SeekPreRoll
    (0x5741, WebmSupport::Supported),       // WritingApp
    (0x5854, WebmSupport::Unsupported),     // SilentTracks
    (0x58D7, WebmSupport::Unsupported),     // SilentTrackNumber
    (0x61A7, WebmSupport::Unsupported),     // AttachedFile
    (0x6240, WebmSupport::Supported),       // ContentEncoding
    (0x6264, WebmSupport::Supported),       // BitDepth
    (0x63A2, WebmSupport::Supported),       // CodecPrivate
    (0x63C0, WebmSupport::Supported),       // Targets
    (0x63C3, WebmSupport::Unsupported),     // ChapterPhysicalEquiv
    (0x63C4, WebmSupport::Unsupported),     // TagChapterUID
    (0x63C5, WebmSupport::Supported),       // TagTrackUID
    (0x63C6, WebmSupport::Unsupported),     // TagAttachmentUID
    (0x63C9, WebmSupport::Unsupported),     // TagEditionUID
    (0x63CA, WebmSupport::Supported),       // TargetType
    (0x6624, WebmSupport::Unsupported),     // TrackTranslate
    (0x66A5, WebmSupport::Unsupported),     // TrackTranslateTrackID
    (0x66BF, WebmSupport::Unsupported),     // TrackTranslateCodec
    (0x66FC, WebmSupport::Unsupported),     // TrackTranslateEditionUID
    (0x67C8, WebmSupport::Supported),       // SimpleTag
    (0x68CA, WebmSupport::Supported),       // TargetTypeValue
    (0x6911, WebmSupport::Unsupported),     // ChapProcessCommand
    (0x6922, WebmSupport::Unsupported),     // ChapProcessTime
    (0x6924, WebmSupport::Unsupported),     // ChapterTranslate
    (0x6933, WebmSupport::Unsupported),     // ChapProcessData
    (0x6944, WebmSupport::Unsupported),     // ChapProcess
    (0x6955, WebmSupport::Unsupported),     // ChapProcessCodecID
    (0x69A5, WebmSupport::Unsupported),     // ChapterTranslateID
    (0x69BF, WebmSupport::Unsupported),     // ChapterTranslateCodec
    (0x69FC, WebmSupport::Unsupported),     // ChapterTranslateEditionUID
    (0x6D80, WebmSupport::Supported),       // ContentEncodings
    (0x6DE7, WebmSupport::Unsupported),     // MinCache
    (0x6DF8, WebmSupport::Unsupported),     // MaxCache
    (0x6E67, WebmSupport::Unsupported),     // ChapterSegmentUID
    (0x6EBC, WebmSupport::Unsupported),     // ChapterSegmentEditionUID
    (0x6FAB, WebmSupport::Unsupported),     // TrackOverlay
    (0x7373, WebmSupport::Supported),       // Tag
    (0x7384, WebmSupport::Unsupported),     // SegmentFilename
    (0x73A4, WebmSupport::Unsupported),     // SegmentUID
    (0x73C4, WebmSupport::Supported),       // ChapterUID
    (0x73C5, WebmSupport::Supported),       // TrackUID
    (0x7446, WebmSupport::Unsupported),     // AttachmentLink
    (0x75A1, WebmSupport::Supported),       // BlockAdditions
    (0x75A2, WebmSupport::Supported),       // DiscardPadding
    (0x78B5, WebmSupport::Supported),       // OutputSamplingFrequency
    (0x7BA9, WebmSupport::Supported),       // Title
    (0x7D7B, WebmSupport::Unsupported),     // ChannelPositions
    (0x22B59C, WebmSupport::Supported),     // Language
    (0x23314F, WebmSupport::Unsupported),   // TrackTimecodeScale
    (0x234E7A, WebmSupport::Unsupported),   // DefaultDecodedFieldDuration
    (0x2383E3, WebmSupport::Deprecated),    // FrameRate
    (0x23E383, WebmSupport::Supported),     // DefaultDuration
    (0x258688, WebmSupport::Supported),     // CodecName
    (0x26B240, WebmSupport::Unsupported),   // CodecDownloadURL
    (0x2AD7B1, WebmSupport::Supported),     // TimecodeScale
    (0x2EB524, WebmSupport::Unsupported),   // ColourSpace
    (0x2FB523, WebmSupport::Unsupported),   // GammaValue
    (0x3A9697, WebmSupport::Unsupported),   // CodecSettings
    (0x3B4040, WebmSupport::Unsupported),   // CodecInfoURL
    (0x3C83AB, WebmSupport::Unsupported),   // PrevFilename
    (0x3CB923, WebmSupport::Unsupported),   // PrevUID
    (0x3E83BB, WebmSupport::Unsupported),   // NextFilename
    (0x3EB923, WebmSupport::Unsupported),   // NextUID
    (0x1043A770, WebmSupport::Supported),   // Chapters
    (0x114D9B74, WebmSupport::Supported),   // SeekHead
    (0x1254C367, WebmSupport::Supported),   // Tags
    (0x1549A966, WebmSupport::Supported),   // Info
    (0x1654AE6B, WebmSupport::Supported),   // Tracks
    (0x18538067, WebmSupport::Supported),   // Segment
    (0x1941A469, WebmSupport::Unsupported), // Attachments
    (0x1A45DFA3, WebmSupport::Supported),   // EBML
    (0x1C53BB6B, WebmSupport::Supported),   // Cues
    (0x1F43B675, WebmSupport::Supported),   // Cluster
];

/// Look up the WebM-guidelines support status for an Element ID.
///
/// IDs the guidelines table does not list return
/// [`WebmSupport::Unlisted`].
pub fn webm_element_support(id: u32) -> WebmSupport {
    match WEBM_SUPPORT_TABLE.binary_search_by_key(&id, |(i, _)| *i) {
        Ok(idx) => WEBM_SUPPORT_TABLE[idx].1,
        Err(_) => WebmSupport::Unlisted,
    }
}

/// Every Master element of the document schema — the 47 `type: master`
/// entries of RFC 9559 plus the two RFC 8794 EBML-header masters
/// (`EBML`, `DocTypeExtension`). The scanner descends into these and
/// skips the body of everything else. Sorted for binary search.
const MASTERS: &[u32] = &[
    ids::CHAPTER_DISPLAY,          // 0x80
    ids::SLICES,                   // 0x8E
    ids::BLOCK_GROUP,              // 0xA0
    ids::BLOCK_MORE,               // 0xA6
    ids::TRACK_ENTRY,              // 0xAE
    ids::CHAPTER_ATOM,             // 0xB6
    ids::CUE_TRACK_POSITIONS,      // 0xB7
    ids::CUE_POINT,                // 0xBB
    ids::REFERENCE_FRAME,          // 0xC8
    ids::CUE_REFERENCE,            // 0xDB
    ids::VIDEO,                    // 0xE0
    ids::AUDIO,                    // 0xE1
    ids::TRACK_OPERATION,          // 0xE2
    ids::TRACK_COMBINE_PLANES,     // 0xE3
    ids::TRACK_PLANE,              // 0xE4
    ids::TIME_SLICE,               // 0xE8
    ids::TRACK_JOIN_BLOCKS,        // 0xE9
    ids::BLOCK_ADDITION_MAPPING,   // 0x41E4
    ids::DOC_TYPE_EXTENSION,       // 0x4281
    ids::EDITION_ENTRY,            // 0x45B9
    ids::CONTENT_ENC_AES_SETTINGS, // 0x47E7
    ids::SEEK,                     // 0x4DBB
    ids::CONTENT_COMPRESSION,      // 0x5034
    ids::CONTENT_ENCRYPTION,       // 0x5035
    ids::COLOUR,                   // 0x55B0
    ids::MASTERING_METADATA,       // 0x55D0
    ids::SILENT_TRACKS,            // 0x5854
    ids::ATTACHED_FILE,            // 0x61A7
    ids::CONTENT_ENCODING,         // 0x6240
    ids::TARGETS,                  // 0x63C0
    ids::TRACK_TRANSLATE,          // 0x6624
    ids::SIMPLE_TAG,               // 0x67C8
    ids::CHAP_PROCESS_COMMAND,     // 0x6911
    ids::CHAPTER_TRANSLATE,        // 0x6924
    ids::CHAP_PROCESS,             // 0x6944
    ids::CONTENT_ENCODINGS,        // 0x6D80
    ids::TAG,                      // 0x7373
    ids::BLOCK_ADDITIONS,          // 0x75A1
    ids::PROJECTION,               // 0x7670
    ids::CHAPTERS,                 // 0x1043A770
    ids::SEEK_HEAD,                // 0x114D9B74
    ids::TAGS,                     // 0x1254C367
    ids::INFO,                     // 0x1549A966
    ids::TRACKS,                   // 0x1654AE6B
    ids::SEGMENT,                  // 0x18538067
    ids::ATTACHMENTS,              // 0x1941A469
    ids::EBML_HEADER,              // 0x1A45DFA3
    ids::CUES,                     // 0x1C53BB6B
    ids::CLUSTER,                  // 0x1F43B675
];

fn is_master(id: u32) -> bool {
    MASTERS.binary_search(&id).is_ok()
}

/// Top-Level Element IDs (direct children of `Segment`) — the terminator
/// set for walking an unknown-size `Cluster` (RFC 9559 §6.2: an
/// unknown-size element ends where a sibling or parent-sibling begins).
const TOP_LEVEL: &[u32] = &[
    ids::SEEK_HEAD,
    ids::INFO,
    ids::TRACKS,
    ids::CLUSTER,
    ids::CUES,
    ids::ATTACHMENTS,
    ids::CHAPTERS,
    ids::TAGS,
    ids::SEGMENT,
];

/// Maximum master-nesting depth the scanner descends (`ChapterAtom` and
/// `SimpleTag` recurse; a hostile file could nest indefinitely). Beyond
/// the cap a master's body is skipped, not descended.
const MAX_DEPTH: usize = 64;

/// Maximum number of per-occurrence findings recorded before the report
/// switches to counting only ([`WebmConformanceReport::findings_truncated`]).
const MAX_FINDINGS: usize = 4096;

/// One off-profile element occurrence found by [`scan`].
#[derive(Clone, Copy, Debug)]
pub struct WebmFinding {
    /// Absolute file offset of the element's ID byte.
    pub offset: u64,
    /// The element's ID (marker bits included, as in [`ids`]).
    pub id: u32,
    /// Why the occurrence was flagged: [`WebmSupport::Unsupported`] or
    /// [`WebmSupport::Deprecated`].
    pub support: WebmSupport,
}

/// The result of a [`scan`]: per-status occurrence counts, the flagged
/// occurrences, and the document's `DocType`.
#[derive(Clone, Debug, Default)]
pub struct WebmConformanceReport {
    /// The EBML header's `DocType` string, when one was found.
    pub doc_type: Option<String>,
    /// Total elements visited (headers parsed), including masters.
    pub elements_scanned: u64,
    /// Occurrences of guideline-`Supported` elements.
    pub supported: u64,
    /// Occurrences of guideline-`Unsupported` elements.
    pub unsupported: u64,
    /// Occurrences of guideline-`Deprecated` elements.
    pub deprecated: u64,
    /// Occurrences of elements the guidelines table does not list.
    pub unlisted: u64,
    /// Every `Unsupported` / `Deprecated` occurrence, in document order,
    /// capped at [`MAX_FINDINGS`] entries.
    pub findings: Vec<WebmFinding>,
    /// `true` when more findings occurred than [`findings`]
    /// (WebmConformanceReport::findings) records — the counters above
    /// still include them.
    pub findings_truncated: bool,
    /// The distinct `Unlisted` element IDs seen, in first-encounter order
    /// (informational; capped at [`MAX_FINDINGS`] entries).
    pub unlisted_ids: Vec<u32>,
    /// The absolute offset of the first structural inconsistency the walk
    /// could not interpret (a child element overrunning its parent's
    /// declared extent, an unknown-size element that RFC 9559 §6.2 does
    /// not allow to be unknown-size, or a torn element header
    /// mid-document). Walking resumes past a damaged *bounded* child (its
    /// extent is exact), so the counts still cover the rest of the
    /// document; `None` means the whole document walked cleanly.
    pub scan_stopped_at: Option<u64>,
}

impl WebmConformanceReport {
    /// `true` when the `DocType` is exactly `"webm"`.
    pub fn doc_type_is_webm(&self) -> bool {
        self.doc_type.as_deref() == Some("webm")
    }

    /// The headline verdict: the `DocType` is `"webm"`, no
    /// guideline-`Unsupported` or `Deprecated` element occurs anywhere,
    /// and the whole document was structurally walkable. `Unlisted`
    /// elements do not fail conformance (see the module docs).
    pub fn is_conformant(&self) -> bool {
        self.doc_type_is_webm()
            && self.unsupported == 0
            && self.deprecated == 0
            && self.scan_stopped_at.is_none()
    }
}

/// Scan a whole EBML document and classify every element occurrence
/// against the WebM guidelines table.
///
/// The scan is a pure structural walk (headers + master descent; leaf
/// bodies are skipped, never allocated), so it runs in O(file size) with
/// O(depth) memory and is safe on hostile input: allocation is bounded,
/// nesting is capped at [`MAX_DEPTH`], and every step moves the reader
/// forward. Damage does not error the scan — the walk stops at the first
/// structurally-unwalkable byte and reports it via
/// [`WebmConformanceReport::scan_stopped_at`].
///
/// The reader is left at an unspecified position.
pub fn scan<R: Read + Seek>(r: &mut R) -> Result<WebmConformanceReport> {
    let mut report = WebmConformanceReport::default();
    r.seek(SeekFrom::Start(0))?;
    let end = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    walk_children(r, 0, end, None, 0, &mut report)?;
    Ok(report)
}

/// Classify one element occurrence into the report.
fn record(report: &mut WebmConformanceReport, offset: u64, id: u32) {
    report.elements_scanned += 1;
    let support = webm_element_support(id);
    match support {
        WebmSupport::Supported => report.supported += 1,
        WebmSupport::Unlisted => {
            report.unlisted += 1;
            if !report.unlisted_ids.contains(&id) && report.unlisted_ids.len() < MAX_FINDINGS {
                report.unlisted_ids.push(id);
            }
        }
        WebmSupport::Unsupported | WebmSupport::Deprecated => {
            match support {
                WebmSupport::Unsupported => report.unsupported += 1,
                _ => report.deprecated += 1,
            }
            if report.findings.len() < MAX_FINDINGS {
                report.findings.push(WebmFinding {
                    offset,
                    id,
                    support,
                });
            } else {
                report.findings_truncated = true;
            }
        }
    }
}

/// Walk the children of a master whose body spans `start..end`.
///
/// `terminate_on` carries the sibling-ID set that ends an unknown-size
/// master (RFC 9559 §6.2); `None` means the extent is exact. Returns the
/// offset where walking stopped (== `end` on a clean walk; the reader is
/// positioned there unless the walk was terminated by a sibling ID, in
/// which case the reader is positioned at that sibling's first byte).
fn walk_children<R: Read + Seek>(
    r: &mut R,
    start: u64,
    end: u64,
    terminate_on: Option<&[u32]>,
    depth: usize,
    report: &mut WebmConformanceReport,
) -> Result<u64> {
    let mut pos = start;
    while pos < end {
        r.seek(SeekFrom::Start(pos))?;
        let header = match ebml::read_element_header(r) {
            Ok(h) => h,
            Err(_) => {
                // Torn or malformed header. `pos < end` means bytes
                // remain that do not form an element header — damage
                // whether the extent was exact or unknown-size (a clean
                // EOF terminator exits the loop at `pos == end`
                // instead).
                if report.scan_stopped_at.is_none() {
                    report.scan_stopped_at = Some(pos);
                }
                return Ok(pos);
            }
        };
        if let Some(term) = terminate_on {
            if term.contains(&header.id) {
                // The unknown-size master we are walking ends here.
                r.seek(SeekFrom::Start(pos))?;
                return Ok(pos);
            }
        }
        record(report, pos, header.id);
        let body = pos + header.header_len as u64;
        if header.size == ebml::VINT_UNKNOWN_SIZE {
            // RFC 9559 §6.2: only Segment and Cluster may be
            // unknown-size in a Matroska document.
            match header.id {
                id if id == ids::SEGMENT => {
                    // Children are Top-Level Elements; a following
                    // sibling Segment ends it.
                    pos = walk_children(r, body, end, Some(&[ids::SEGMENT]), depth + 1, report)?;
                    continue;
                }
                id if id == ids::CLUSTER => {
                    pos = walk_children(r, body, end, Some(TOP_LEVEL), depth + 1, report)?;
                    continue;
                }
                _ => {
                    report.scan_stopped_at = Some(pos);
                    return Ok(pos);
                }
            }
        }
        let Some(next) = body.checked_add(header.size) else {
            report.scan_stopped_at = Some(pos);
            return Ok(pos);
        };
        if next > end {
            // Child overruns the parent's extent (or the file itself).
            report.scan_stopped_at = Some(pos);
            return Ok(pos);
        }
        if is_master(header.id) && depth < MAX_DEPTH {
            // A bounded master's extent is exact, so even if damage
            // stops the walk somewhere inside it, we can resume at the
            // sibling that follows it.
            walk_children(r, body, next, None, depth + 1, report)?;
        } else if header.id == ids::EBML_DOC_TYPE && depth > 0 && header.size <= 64 {
            // Capture the DocType string (first one wins).
            if report.doc_type.is_none() {
                r.seek(SeekFrom::Start(body))?;
                if let Ok(s) = ebml::read_string(r, header.size as usize) {
                    report.doc_type = Some(s);
                }
            }
        }
        pos = next;
    }
    Ok(pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_table_is_sorted_and_duplicate_free() {
        for w in WEBM_SUPPORT_TABLE.windows(2) {
            assert!(
                w[0].0 < w[1].0,
                "table must be strictly ascending: 0x{:X} then 0x{:X}",
                w[0].0,
                w[1].0
            );
        }
    }

    #[test]
    fn support_table_row_counts() {
        // 239 guideline rows mapped to Element IDs: 137 Supported,
        // 98 Unsupported, 4 Deprecated (see the module docs for the 11
        // name-only rows that could not be keyed to an ID).
        assert_eq!(WEBM_SUPPORT_TABLE.len(), 239);
        let count = |s: WebmSupport| WEBM_SUPPORT_TABLE.iter().filter(|(_, x)| *x == s).count();
        assert_eq!(count(WebmSupport::Supported), 137);
        assert_eq!(count(WebmSupport::Unsupported), 98);
        assert_eq!(count(WebmSupport::Deprecated), 4);
    }

    #[test]
    fn masters_are_sorted_and_duplicate_free() {
        for w in MASTERS.windows(2) {
            assert!(
                w[0] < w[1],
                "MASTERS must be strictly ascending: 0x{:X} then 0x{:X}",
                w[0],
                w[1]
            );
        }
        assert_eq!(MASTERS.len(), 49);
    }

    #[test]
    fn unlisted_lookup() {
        assert_eq!(webm_element_support(ids::PROJECTION), WebmSupport::Unlisted);
        assert_eq!(webm_element_support(0x12345678), WebmSupport::Unlisted);
    }
}
