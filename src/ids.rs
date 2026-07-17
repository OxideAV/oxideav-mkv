//! Matroska element IDs we care about.
//!
//! Reference: <https://www.matroska.org/technical/elements.html>

#![allow(dead_code)]

// EBML root + EBML metadata.
pub const EBML_HEADER: u32 = 0x1A45DFA3;
pub const EBML_VERSION: u32 = 0x4286;
pub const EBML_READ_VERSION: u32 = 0x42F7;
pub const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
pub const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
pub const EBML_DOC_TYPE: u32 = 0x4282;
pub const EBML_DOC_TYPE_VERSION: u32 = 0x4287;
pub const EBML_DOC_TYPE_READ_VERSION: u32 = 0x4285;

// DocTypeExtension (RFC 8794 §11.2.9..§11.2.11): an EBML-header master that
// adds extra Elements to the main DocType+DocTypeVersion tuple — used to
// iterate experimental elements before they integrate into a regular
// DocTypeVersion. `minOccurs: 0`, unbounded (several may appear). Each carries
// a mandatory `DocTypeExtensionName` (string, length >0, MUST be unique within
// the header) and a mandatory `DocTypeExtensionVersion` (uinteger, range "not
// 0"). An EBML Reader MAY know these extra elements; the container surfaces the
// (name, version) declarations verbatim so a consumer can decide whether it
// understands an extension before relying on its elements.
pub const DOC_TYPE_EXTENSION: u32 = 0x4281;
pub const DOC_TYPE_EXTENSION_NAME: u32 = 0x4283;
pub const DOC_TYPE_EXTENSION_VERSION: u32 = 0x4284;

// Top-level Segment.
pub const SEGMENT: u32 = 0x18538067;

// Within Segment.
pub const SEEK_HEAD: u32 = 0x114D9B74;
pub const SEEK: u32 = 0x4DBB;
pub const SEEK_ID: u32 = 0x53AB;
pub const SEEK_POSITION: u32 = 0x53AC;
pub const INFO: u32 = 0x1549A966;
pub const TRACKS: u32 = 0x1654AE6B;
pub const CLUSTER: u32 = 0x1F43B675;
pub const CUES: u32 = 0x1C53BB6B;
pub const ATTACHMENTS: u32 = 0x1941A469;
pub const CHAPTERS: u32 = 0x1043A770;
pub const TAGS: u32 = 0x1254C367;
pub const VOID: u32 = 0xEC;
pub const CRC32: u32 = 0xBF;

// Info.
pub const TIMECODE_SCALE: u32 = 0x2AD7B1;
/// `TimestampScale` (RFC 9559 §5.1.2.9) — the current spec name for the
/// `0x2AD7B1` element [`TIMECODE_SCALE`] carries; "Timecode" was renamed to
/// "Timestamp" throughout RFC 9559 (the on-wire id is unchanged). Alias for
/// spec-name-oriented callers.
pub const TIMESTAMP_SCALE: u32 = TIMECODE_SCALE;
pub const DURATION: u32 = 0x4489;
pub const SEGMENT_UID: u32 = 0x73A4;
/// `SegmentUUID` (RFC 9559 §5.1.2.1) — the current spec name for the
/// `0x73A4` element [`SEGMENT_UID`] carries (renamed UID → UUID in RFC 9559;
/// on-wire id unchanged). Alias for spec-name-oriented callers.
pub const SEGMENT_UUID: u32 = SEGMENT_UID;
pub const MUXING_APP: u32 = 0x4D80;
pub const WRITING_APP: u32 = 0x5741;
pub const TITLE: u32 = 0x7BA9;
pub const DATE_UTC: u32 = 0x4461;

// Linked-Segment Info elements (RFC 9559 §5.1.2.2..§5.1.2.8). These tie a
// Segment to the other Segments of a Linked Segment (Section 17): the
// previous / next Segment in a Hard-Linked chain (PrevUUID / NextUUID,
// 16-byte binary), a human-readable filename for display convenience
// (SegmentFilename / PrevFilename / NextFilename, utf-8), and the
// SegmentFamily UID (16-byte binary) all Segments of a Linked Segment
// share. ChapterTranslate is a master that maps this Segment's
// SegmentUUID to the internal segment value a Chapter Codec uses, so a
// file can be remuxed without rewriting its chapter-codec data.
pub const SEGMENT_FILENAME: u32 = 0x7384;
pub const PREV_UID: u32 = 0x3CB923;
/// `PrevUUID` (RFC 9559 §5.1.2.3) — current spec name for [`PREV_UID`]
/// (`0x3CB923`, renamed UID → UUID; id unchanged). Alias.
pub const PREV_UUID: u32 = PREV_UID;
pub const PREV_FILENAME: u32 = 0x3C83AB;
pub const NEXT_UID: u32 = 0x3EB923;
/// `NextUUID` (RFC 9559 §5.1.2.5) — current spec name for [`NEXT_UID`]
/// (`0x3EB923`, renamed UID → UUID; id unchanged). Alias.
pub const NEXT_UUID: u32 = NEXT_UID;
pub const NEXT_FILENAME: u32 = 0x3E83BB;
pub const SEGMENT_FAMILY: u32 = 0x4444;
pub const CHAPTER_TRANSLATE: u32 = 0x6924;
pub const CHAPTER_TRANSLATE_ID: u32 = 0x69A5;
pub const CHAPTER_TRANSLATE_CODEC: u32 = 0x69BF;
pub const CHAPTER_TRANSLATE_EDITION_UID: u32 = 0x69FC;

// Tags (Segment\Tags\Tag\SimpleTag).
pub const TAG: u32 = 0x7373;
pub const TARGETS: u32 = 0x63C0;
// Children of Targets (RFC 9559 §5.1.8.1.1.x). UID children all default to 0
// which is the "applies to everything in the segment" sentinel.
pub const TARGET_TYPE_VALUE: u32 = 0x68CA;
pub const TARGET_TYPE: u32 = 0x63CA;
pub const TAG_TRACK_UID: u32 = 0x63C5;
pub const TAG_EDITION_UID: u32 = 0x63C9;
pub const TAG_CHAPTER_UID: u32 = 0x63C4;
pub const TAG_ATTACHMENT_UID: u32 = 0x63C6;
pub const SIMPLE_TAG: u32 = 0x67C8;
pub const TAG_NAME: u32 = 0x45A3;
pub const TAG_STRING: u32 = 0x4487;
pub const TAG_LANGUAGE: u32 = 0x447A;
// `TagLanguageBCP47` (RFC 9559 §5.1.8.1.2.3), `TagDefault` (§5.1.8.1.2.4)
// and `TagBinary` (§5.1.8.1.2.6) — needed by the typed [`Tag`] surface so
// consumers can see per-language and binary-payload SimpleTags without
// having to re-parse the file.
pub const TAG_LANGUAGE_BCP47: u32 = 0x447B;
pub const TAG_DEFAULT: u32 = 0x4484;
pub const TAG_BINARY: u32 = 0x4485;
/// `TagDefaultBogus` (RFC 9559 Appendix A.43, uinteger, id 0x44B4) — a
/// reclaimed variant of [`TAG_DEFAULT`] that was written with a "bogus"
/// element ID by some historical Writers (see §5.1.8.1.2.4). Treated as a
/// synonym for `TagDefault` on read so a `SimpleTag` carrying the
/// mis-encoded id still surfaces its default flag.
pub const TAG_DEFAULT_BOGUS: u32 = 0x44B4;

// Tracks > TrackEntry.
pub const TRACK_ENTRY: u32 = 0xAE;
pub const TRACK_NUMBER: u32 = 0xD7;
pub const TRACK_UID: u32 = 0x73C5;
pub const TRACK_TYPE: u32 = 0x83;
pub const FLAG_ENABLED: u32 = 0xB9;
pub const FLAG_DEFAULT: u32 = 0x88;
pub const FLAG_LACING: u32 = 0x9C;

// TrackEntry timing elements (RFC 9559 §5.1.4.1.13..§5.1.4.1.15). The nominal
// frame interval (`DefaultDuration`), the field-output interval
// (`DefaultDecodedFieldDuration`), and the per-track timestamp scale factor
// (`TrackTimestampScale`). The first two are `uinteger` nanoseconds with a
// "not 0" range and no default; the third is a `float` with default `1.0`.
pub const DEFAULT_DURATION: u32 = 0x23E383;
pub const DEFAULT_DECODED_FIELD_DURATION: u32 = 0x234E7A;
pub const TRACK_TIMESTAMP_SCALE: u32 = 0x23314F;

// TrackEntry "audience" flags (RFC 9559 §5.1.4.1.6..§5.1.4.1.11). Each is a
// 0-or-1 uinteger that hints at how the track should be presented to a
// particular kind of user — subtitle-forced-display, hearing/visual
// impairment accessibility, text-only-descriptions, originality vs dubbing,
// commentary. Only `FlagForced` (§5.1.4.1.6) has a spec default (0); the
// other five (`minver: 4`) carry no spec default and surface as `Option`.
pub const FLAG_FORCED: u32 = 0x55AA;
pub const FLAG_HEARING_IMPAIRED: u32 = 0x55AB;
pub const FLAG_VISUAL_IMPAIRED: u32 = 0x55AC;
pub const FLAG_TEXT_DESCRIPTIONS: u32 = 0x55AD;
pub const FLAG_ORIGINAL: u32 = 0x55AE;
pub const FLAG_COMMENTARY: u32 = 0x55AF;
pub const NAME: u32 = 0x536E;
pub const LANGUAGE: u32 = 0x22B59C;
/// `LanguageBCP47` (RFC 9559 §5.1.4.1.20, `minver: 4`). The track's language
/// in [RFC5646] (BCP-47) form. When present, any `Language` element in the
/// same `TrackEntry` MUST be ignored.
pub const LANGUAGE_BCP47: u32 = 0x22B59D;
pub const CODEC_ID: u32 = 0x86;
pub const CODEC_PRIVATE: u32 = 0x63A2;
pub const CODEC_NAME: u32 = 0x258688;
/// `AttachmentLink` (RFC 9559 §5.1.4.1.24, `maxver: 3`). The `FileUID`
/// (§5.1.6.5) of an attachment this track's codec uses. Range "not 0".
pub const ATTACHMENT_LINK: u32 = 0x7446;
pub const CODEC_DELAY: u32 = 0x56AA;
pub const SEEK_PRE_ROLL: u32 = 0x56BB;
pub const VIDEO: u32 = 0xE0;
pub const AUDIO: u32 = 0xE1;

// Reclaimed Appendix-A `TrackEntry`-level legacy elements (RFC 9559
// Appendix A.19..A.23 + A.28..A.32). These are historical Matroska
// `TrackEntry` children the core spec body no longer documents but whose IDs
// remain reserved; the demuxer surfaces them verbatim (typed `TrackLegacy`
// record) so a faithful re-mux round-trips them. None carries a spec default
// or range — the appendix gives only type/id/path/documentation.
//
// `CodecSettings` (A.19, utf-8): a string describing the encoding settings.
// `CodecInfoURL` (A.20, string): a URL with information about the codec.
// `CodecDownloadURL` (A.21, string): a URL to download the codec.
// `CodecDecodeAll` (A.22, uinteger): 1 if the codec can decode damaged data.
// `TrackOverlay` (A.23, uinteger): the TrackNumber of a track to use when this
//   track has a gap on `SilentTracks`. Multiple values are ordered — the first
//   is preferred, then the second, etc.
pub const CODEC_SETTINGS: u32 = 0x3A9697;
pub const CODEC_INFO_URL: u32 = 0x3B4040;
pub const CODEC_DOWNLOAD_URL: u32 = 0x26B240;
pub const CODEC_DECODE_ALL: u32 = 0xAA;
pub const TRACK_OVERLAY: u32 = 0x6FAB;
// Three more reclaimed TrackEntry-level elements (RFC 9559 Appendix
// A.16..A.18). `MinCache` (A.16, uinteger): the minimum number of frames a
// player should be able to cache during playback (0 = the reference
// pseudo-cache system is not used). `MaxCache` (A.17, uinteger): the maximum
// cache size necessary to store referenced frames and the current frame (0 =
// no cache needed). `TrackOffset` (A.18, integer): a value, in Matroska Ticks
// (nanoseconds), to add to the Block's Timestamp to adjust the track's
// playback offset. Surfaced verbatim on `TrackLegacy` for a faithful re-mux.
pub const MIN_CACHE: u32 = 0x6DE7;
pub const MAX_CACHE: u32 = 0x6DF8;
pub const TRACK_OFFSET: u32 = 0x537F;
// Three reclaimed elements nested in the Video / Audio masters (RFC 9559
// Appendix A.25..A.27). `GammaValue` (A.25, float, Video): the gamma value.
// `FrameRate` (A.26, float, Video): informational frames-per-second for a
// constant-frame-rate track (not to be used for VFR). `ChannelPositions`
// (A.27, binary, Audio): a table of horizontal angles for each successive
// channel. Surfaced verbatim on `TrackLegacy` for a faithful re-mux; the
// container interprets none of them.
pub const GAMMA_VALUE: u32 = 0x2FB523;
pub const FRAME_RATE: u32 = 0x2383E3;
pub const CHANNEL_POSITIONS: u32 = 0x7D7B;

// DivXTrickTrack pairing quintet (RFC 9559 Appendix A.28..A.32). These tie a
// video track to its Smooth FF/RW companion in a paired EBML structure. The
// `*UID` fields are `uinteger` TrackUIDs; the `*SegmentUID` fields are 16-byte
// SegmentUUID binaries; `TrickTrackFlag` is a 0-or-1 uinteger marking this
// track itself as the Smooth FF/RW track. Surfaced verbatim for re-mux.
pub const TRICK_TRACK_UID: u32 = 0xC0;
pub const TRICK_TRACK_SEGMENT_UID: u32 = 0xC1;
pub const TRICK_TRACK_FLAG: u32 = 0xC6;
pub const TRICK_MASTER_TRACK_UID: u32 = 0xC7;
pub const TRICK_MASTER_TRACK_SEGMENT_UID: u32 = 0xC4;

// TrackTranslate (RFC 9559 §5.1.4.1.27): a per-TrackEntry master that maps
// this track to a track value addressed by a Chapter Codec. A Chapter Codec
// (e.g. DVD menu, Matroska Script) may need to reference a specific track but
// does not know how Matroska identifies tracks; this mapping lets a file be
// remuxed (acquiring new TrackNumbers/TrackUIDs) without rewriting the opaque
// chapter-codec command data — only the mapping changes. The element is
// unbounded (a TrackEntry can carry several). `TrackTranslateTrackID`
// (§5.1.4.1.27.1, binary, minOccurs 1) is the value the chapter codec uses to
// name this track; its format depends on the ChapProcessCodecID (Table 31,
// §5.1.7.1.4.15). `TrackTranslateCodec` (§5.1.4.1.27.2, uinteger, minOccurs 1)
// names that chapter codec. `TrackTranslateEditionUID` (§5.1.4.1.27.3,
// uinteger, unbounded) lists the chapter editions the mapping applies to — an
// empty list means "all editions using the given codec."
pub const TRACK_TRANSLATE: u32 = 0x6624;
pub const TRACK_TRANSLATE_TRACK_ID: u32 = 0x66A5;
pub const TRACK_TRANSLATE_CODEC: u32 = 0x66BF;
pub const TRACK_TRANSLATE_EDITION_UID: u32 = 0x66FC;

// TrackOperation (RFC 9559 §5.1.4.1.30): describes a virtual track built
// by combining other tracks — either combining video planes into one 3D
// track (TrackCombinePlanes) or joining several tracks' Blocks into one
// timeline (TrackJoinBlocks). The plane / join references point at other
// tracks by their TrackUID.
pub const TRACK_OPERATION: u32 = 0xE2;
pub const TRACK_COMBINE_PLANES: u32 = 0xE3;
pub const TRACK_PLANE: u32 = 0xE4;
pub const TRACK_PLANE_UID: u32 = 0xE5;
pub const TRACK_PLANE_TYPE: u32 = 0xE6;
pub const TRACK_JOIN_BLOCKS: u32 = 0xE9;
pub const TRACK_JOIN_UID: u32 = 0xED;

// BlockAdditionMapping (RFC 9559 §5.1.4.1.17): a per-TrackEntry container
// that describes how to interpret BlockAdditional data (carried via the
// `BlockMore.BlockAddID` field on a `BlockGroup`, §5.1.3.5.2.3). Each
// mapping is identified by `BlockAddIDValue` (>=2) and tagged with an IANA-
// registered `BlockAddIDType` (defaults to 0 — codec-defined); the optional
// `BlockAddIDName` is a human-readable label and `BlockAddIDExtraData` is
// per-track binary state the type may consult. The element is unbounded —
// a TrackEntry can carry several mappings, one per (BlockAddIDType,
// BlockAddIDValue) pair. The container surfaces these *headers*; semantic
// interpretation of the per-frame `BlockAdditional` bytes lives in the
// codec / track-format extension that owns each `BlockAddIDType` value.
pub const BLOCK_ADDITION_MAPPING: u32 = 0x41E4;
pub const BLOCK_ADD_ID_VALUE: u32 = 0x41F0;
pub const BLOCK_ADD_ID_NAME: u32 = 0x41A4;
pub const BLOCK_ADD_ID_TYPE: u32 = 0x41E7;
pub const BLOCK_ADD_ID_EXTRA_DATA: u32 = 0x41ED;

// MaxBlockAdditionID (RFC 9559 §5.1.4.1.16): per-TrackEntry uinteger
// declaring the maximum `BlockAddID` (§5.1.3.5.2.3) value any of the
// track's Blocks may carry. The spec default `0` means "there is no
// BlockAdditions for this track."
pub const MAX_BLOCK_ADDITION_ID: u32 = 0x55EE;

// ContentEncodings (RFC 9559 §5.1.4.1.31): a per-track ordered list of
// transformations (compression / encryption) applied to frame data, the
// CodecPrivate, or both, before the bytes were placed in Blocks. The
// container surfaces these *headers* only — it never decompresses or
// decrypts a frame.
pub const CONTENT_ENCODINGS: u32 = 0x6D80;
pub const CONTENT_ENCODING: u32 = 0x6240;
pub const CONTENT_ENCODING_ORDER: u32 = 0x5031;
pub const CONTENT_ENCODING_SCOPE: u32 = 0x5032;
pub const CONTENT_ENCODING_TYPE: u32 = 0x5033;
pub const CONTENT_COMPRESSION: u32 = 0x5034;
pub const CONTENT_COMP_ALGO: u32 = 0x4254;
pub const CONTENT_COMP_SETTINGS: u32 = 0x4255;
pub const CONTENT_ENCRYPTION: u32 = 0x5035;
pub const CONTENT_ENC_ALGO: u32 = 0x47E1;
pub const CONTENT_ENC_KEY_ID: u32 = 0x47E2;
pub const CONTENT_ENC_AES_SETTINGS: u32 = 0x47E7;
pub const AES_SETTINGS_CIPHER_MODE: u32 = 0x47E8;

// Reclaimed content-signing quartet inside ContentEncryption
// (RFC 9559 Appendix A.33..A.36).
pub const CONTENT_SIGNATURE: u32 = 0x47E3;
pub const CONTENT_SIG_KEY_ID: u32 = 0x47E4;
pub const CONTENT_SIG_ALGO: u32 = 0x47E5;
pub const CONTENT_SIG_HASH_ALGO: u32 = 0x47E6;

pub const PIXEL_WIDTH: u32 = 0xB0;
pub const PIXEL_HEIGHT: u32 = 0xBA;

// AlphaMode (RFC 9559 §5.1.4.1.28.4): signals that a track's BlockAdditional
// element with `BlockAddID=1` carries alpha-channel data — used by VP8 / VP9
// alpha-bearing variants in WebM. Spec default `0` (no alpha) per
// §5.1.4.1.28.4; Table 6 only enumerates `0` and `1`. §27.8 leaves the
// "Matroska Alpha Modes" registry open for future additions.
pub const ALPHA_MODE: u32 = 0x53C0;
pub const ALPHA_MODE_NONE: u64 = 0;
pub const ALPHA_MODE_PRESENT: u64 = 1;

// AspectRatioType (RFC 9559 Appendix A.24, "Reclaimed"): a uinteger that
// "specifies the possible modifications to the aspect ratio". The reclaimed
// appendix lists no enumerated values, so the typed surface returns the raw
// integer rather than synthesising an enum.
pub const ASPECT_RATIO_TYPE: u32 = 0x54B3;

// UncompressedFourCC (RFC 9559 §5.1.4.1.28.15): a fixed 4-byte FourCC that
// identifies the uncompressed pixel layout — only meaningful when
// `CodecID = "V_UNCOMPRESSED"` (§5.1.4.1.28.15 Table 11). The spec mentions
// no registry; the typed surface exposes the raw 4 bytes plus a UTF-8 lossy
// FourCC string.
pub const UNCOMPRESSED_FOURCC: u32 = 0x2EB524;

// Video geometry quartet (RFC 9559 §5.1.4.1.28.8..§5.1.4.1.28.14).
// PixelCrop* (default 0) carve the visible window out of the encoded
// PixelWidth × PixelHeight buffer. DisplayWidth / DisplayHeight describe
// the target render size, in units selected by DisplayUnit
// (`0` pixels / `1` cm / `2` inches / `3` DAR / `4` unknown — Table 10).
// Defaults: when DisplayUnit is absent (0) DisplayWidth defaults to
// PixelWidth - PixelCropLeft - PixelCropRight and DisplayHeight to
// PixelHeight - PixelCropTop - PixelCropBottom; otherwise there is no
// default (Tables 8 + 9).
pub const PIXEL_CROP_BOTTOM: u32 = 0x54AA;
pub const PIXEL_CROP_TOP: u32 = 0x54BB;
pub const PIXEL_CROP_LEFT: u32 = 0x54CC;
pub const PIXEL_CROP_RIGHT: u32 = 0x54DD;
pub const DISPLAY_WIDTH: u32 = 0x54B0;
pub const DISPLAY_HEIGHT: u32 = 0x54BA;
pub const DISPLAY_UNIT: u32 = 0x54B2;

// DisplayUnit values (RFC 9559 §5.1.4.1.28.14, Table 10).
pub const DISPLAY_UNIT_PIXELS: u64 = 0;
pub const DISPLAY_UNIT_CENTIMETERS: u64 = 1;
pub const DISPLAY_UNIT_INCHES: u64 = 2;
pub const DISPLAY_UNIT_DAR: u64 = 3;
pub const DISPLAY_UNIT_UNKNOWN: u64 = 4;

// Video interlacing (RFC 9559 §5.1.4.1.28.1 + §5.1.4.1.28.2).
// `FlagInterlaced` (spec default 0 = undetermined) marks whether the track's
// frames are interlaced. `FieldOrder` (spec default 2 = undetermined) selects
// the field ordering and MUST be ignored unless `FlagInterlaced == 1`.
pub const FLAG_INTERLACED: u32 = 0x9A;
pub const FIELD_ORDER: u32 = 0x9D;

// Stereo-3D mode (RFC 9559 §5.1.4.1.28.3). Single-track variant of 3D
// carriage — the TrackOperation/TrackCombinePlanes path (§5.1.4.1.30.1) is
// the multi-track alternative. Spec default is `0` (mono).
pub const STEREO_MODE: u32 = 0x53B8;

// OldStereoMode (RFC 9559 §5.1.4.1.28.5, maxver 2): the "bogus" StereoMode
// value libmatroska prior to 0.9.0 wrote at id 0x53B9 instead of the correct
// 0x53B8 (`StereoMode`). §18.10 records the bug: "There was also a bug in
// [libmatroska] prior to 0.9.0 that would save/read it as 0x53B9 instead of
// 0x53B8". The element has its **own** value space (Table 7) that is NOT
// compatible with the modern `StereoMode` Table 5: only 0 (mono), 1 (right
// eye), 2 (left eye), 3 (both eyes) ever appear here. Writers MUST NOT emit
// it; Readers MAY support legacy files by checking for it. The container
// surfaces it verbatim through a separate typed enum so a legacy file's 3D
// intent is observable, and the muxer can round-trip it for a faithful
// re-mux of such a file (the only situation in which a Writer touches it).
pub const OLD_STEREO_MODE: u32 = 0x53B9;

// OldStereoMode values (RFC 9559 §5.1.4.1.28.5, Table 7). Distinct value
// space from the modern StereoMode (Table 5) — these four are the only
// values that can appear in OldStereoMode.
pub const OLD_STEREO_MODE_MONO: u64 = 0;
pub const OLD_STEREO_MODE_RIGHT_EYE: u64 = 1;
pub const OLD_STEREO_MODE_LEFT_EYE: u64 = 2;
pub const OLD_STEREO_MODE_BOTH_EYES: u64 = 3;

// Video > Colour master (RFC 9559 §5.1.4.1.28.16): the BT.709/2020/2100-style
// colour-format description applied to the encoded video — chroma sub-sampling
// and siting, signal range, transfer characteristics, colour primaries,
// matrix-derivation, plus the SMPTE ST 2086 / CTA-861.3 HDR mastering display
// metadata (MasteringMetadata) and the MaxCLL / MaxFALL light-level pair.
// All children are stream-copy True (§8) — semantics live in the bitstream,
// the container only surfaces them.
pub const COLOUR: u32 = 0x55B0;
pub const MATRIX_COEFFICIENTS: u32 = 0x55B1;
pub const BITS_PER_CHANNEL: u32 = 0x55B2;
pub const CHROMA_SUBSAMPLING_HORZ: u32 = 0x55B3;
pub const CHROMA_SUBSAMPLING_VERT: u32 = 0x55B4;
pub const CB_SUBSAMPLING_HORZ: u32 = 0x55B5;
pub const CB_SUBSAMPLING_VERT: u32 = 0x55B6;
pub const CHROMA_SITING_HORZ: u32 = 0x55B7;
pub const CHROMA_SITING_VERT: u32 = 0x55B8;
pub const COLOUR_RANGE: u32 = 0x55B9;
pub const TRANSFER_CHARACTERISTICS: u32 = 0x55BA;
pub const PRIMARIES: u32 = 0x55BB;
pub const MAX_CLL: u32 = 0x55BC;
pub const MAX_FALL: u32 = 0x55BD;
pub const MASTERING_METADATA: u32 = 0x55D0;
pub const PRIMARY_R_CHROMATICITY_X: u32 = 0x55D1;
pub const PRIMARY_R_CHROMATICITY_Y: u32 = 0x55D2;
pub const PRIMARY_G_CHROMATICITY_X: u32 = 0x55D3;
pub const PRIMARY_G_CHROMATICITY_Y: u32 = 0x55D4;
pub const PRIMARY_B_CHROMATICITY_X: u32 = 0x55D5;
pub const PRIMARY_B_CHROMATICITY_Y: u32 = 0x55D6;
pub const WHITE_POINT_CHROMATICITY_X: u32 = 0x55D7;
pub const WHITE_POINT_CHROMATICITY_Y: u32 = 0x55D8;
pub const LUMINANCE_MAX: u32 = 0x55D9;
pub const LUMINANCE_MIN: u32 = 0x55DA;

// ChromaSitingHorz values (RFC 9559 §5.1.4.1.28.23, Table 13).
// ChromaSitingVert (§5.1.4.1.28.24, Table 14) shares the unspecified=0
// value but uses `top collocated` for `1` instead of `left collocated`.
pub const CHROMA_SITING_UNSPECIFIED: u64 = 0;
pub const CHROMA_SITING_HORZ_LEFT_COLLOCATED: u64 = 1;
pub const CHROMA_SITING_VERT_TOP_COLLOCATED: u64 = 1;
pub const CHROMA_SITING_HALF: u64 = 2;

// Color Range values (RFC 9559 §5.1.4.1.28.25, Table 15).
pub const COLOUR_RANGE_UNSPECIFIED: u64 = 0;
pub const COLOUR_RANGE_BROADCAST: u64 = 1;
pub const COLOUR_RANGE_FULL: u64 = 2;
pub const COLOUR_RANGE_DEFINED_BY_MATRIX_AND_TRANSFER: u64 = 3;
pub const SAMPLING_FREQUENCY: u32 = 0xB5;
pub const OUTPUT_SAMPLING_FREQUENCY: u32 = 0x78B5;
pub const CHANNELS: u32 = 0x9F;
pub const BIT_DEPTH: u32 = 0x6264;

// Cluster.
pub const TIMECODE: u32 = 0xE7;
/// `Timestamp` (RFC 9559 §5.1.3.1) — current spec name for the Cluster's
/// `0xE7` element [`TIMECODE`] carries ("Timecode" renamed to "Timestamp"
/// in RFC 9559; on-wire id unchanged). Alias for spec-name-oriented callers.
pub const TIMESTAMP: u32 = TIMECODE;
pub const POSITION: u32 = 0xA7;
pub const PREV_SIZE: u32 = 0xAB;
pub const SIMPLE_BLOCK: u32 = 0xA3;
pub const BLOCK_GROUP: u32 = 0xA0;
pub const BLOCK: u32 = 0xA1;
pub const BLOCK_DURATION: u32 = 0x9B;
pub const REFERENCE_BLOCK: u32 = 0xFB;
// ReferencePriority (RFC 9559 §5.1.3.5.4, uinteger, default 0): cache
// priority of a referenced frame. CodecState (§5.1.3.5.6, binary,
// minver 2): a new codec state private to the codec. DiscardPadding
// (§5.1.3.5.7, integer, minver 4): nanoseconds of silent padding added
// to the Block (positive = end, negative = beginning), discarded during
// playback.
pub const REFERENCE_PRIORITY: u32 = 0xFA;
pub const CODEC_STATE: u32 = 0xA4;
pub const DISCARD_PADDING: u32 = 0x75A2;
// SilentTracks (RFC 9559 Appendix A.1, master, id 0x5854) and
// SilentTrackNumber (A.2, uinteger, id 0x58D7): the Cluster-level list of
// track numbers not used in that part of the stream. Deprecated
// (maxver 0) but still emitted by historical Writers, so the demuxer
// surfaces them and the muxer can round-trip them.
pub const SILENT_TRACKS: u32 = 0x5854;
pub const SILENT_TRACK_NUMBER: u32 = 0x58D7;
/// `EncryptedBlock` (RFC 9559 Appendix A.15, binary, id 0xAF) — a
/// Cluster-level reclaimed element, "similar to SimpleBlock but the data
/// inside the Block are Transformed (encrypted and/or signed)". The
/// container surfaces its raw payload on [`crate::demux::ClusterRecord`]
/// for faithful re-mux; it does not decrypt or interpret the contents.
pub const ENCRYPTED_BLOCK: u32 = 0xAF;

// Deprecated DivX trick-track / old-lacing BlockGroup children
// (RFC 9559 Appendix A.3..A.14). The RFC 9559 core body no longer
// documents these, but their Element IDs stay reserved in the registry
// and historical Writers still emit them, so the demuxer surfaces them
// (and the muxer can round-trip them) for a faithful re-mux. None is
// interpreted by the container.
//
// BlockVirtual (A.3, binary): a Block with no data, stored at the place
// the real Block would be in display order. ReferenceVirtual (A.4,
// integer): the Segment Position of the data that would otherwise be in
// the position of the virtual Block.
pub const BLOCK_VIRTUAL: u32 = 0xA2;
pub const REFERENCE_VIRTUAL: u32 = 0xFD;
// Slices (A.5, master) > TimeSlice (A.6, master): extra per-frame time
// information about the data in the Block; interpreting it is not
// required for playback. TimeSlice children: LaceNumber (A.7, uinteger —
// the reverse frame number in the lace, 0 = last frame), FrameNumber
// (A.8, uinteger — number of the frame to generate from this lace),
// TimeSliceBlockAdditionID (A.9, uinteger — id of the BlockAdditional,
// 0 = main Block), Delay (A.10, uinteger — Track-Tick delay), and
// SliceDuration (A.11, uinteger — Track-Tick duration).
pub const SLICES: u32 = 0x8E;
pub const TIME_SLICE: u32 = 0xE8;
pub const LACE_NUMBER: u32 = 0xCC;
pub const FRAME_NUMBER: u32 = 0xCD;
pub const TIME_SLICE_BLOCK_ADDITION_ID: u32 = 0xCB;
pub const DELAY: u32 = 0xCE;
pub const SLICE_DURATION: u32 = 0xCF;
// ReferenceFrame (A.12, master) for Smooth FF/RW DivX trick tracks >
// ReferenceOffset (A.13, uinteger — relative byte offset from the
// previous BlockGroup to this one) + ReferenceTimestamp (A.14, uinteger —
// Track-Tick timestamp of the BlockGroup pointed to by ReferenceOffset).
pub const REFERENCE_FRAME: u32 = 0xC8;
pub const REFERENCE_OFFSET: u32 = 0xC9;
pub const REFERENCE_TIMESTAMP: u32 = 0xCA;

// BlockAdditions (RFC 9559 §5.1.3.5.2): per-BlockGroup side channel of
// additional binary data completing the Block. Each `BlockMore`
// (§5.1.3.5.2.1) pairs one `BlockAdditional` payload (§5.1.3.5.2.2) with
// a `BlockAddID` (§5.1.3.5.2.3, uinteger, default `1`, range "not 0")
// that selects the interpretation: `1` = codec-defined, any other value
// is described by the matching TrackEntry `BlockAdditionMapping`
// (§5.1.4.1.17). BlockAddID values MUST be unique between the BlockMore
// elements of one BlockAdditions master.
pub const BLOCK_ADDITIONS: u32 = 0x75A1;
pub const BLOCK_MORE: u32 = 0xA6;
pub const BLOCK_ADDITIONAL: u32 = 0xA5;
pub const BLOCK_ADD_ID: u32 = 0xEE;

// Cues (seek index).
pub const CUE_POINT: u32 = 0xBB;
pub const CUE_TIME: u32 = 0xB3;
pub const CUE_TRACK_POSITIONS: u32 = 0xB7;
pub const CUE_TRACK: u32 = 0xF7;
pub const CUE_CLUSTER_POSITION: u32 = 0xF1;
pub const CUE_RELATIVE_POSITION: u32 = 0xF0;
pub const CUE_DURATION: u32 = 0xB2;
pub const CUE_BLOCK_NUMBER: u32 = 0x5378;
pub const CUE_CODEC_STATE: u32 = 0xEA;
pub const CUE_REFERENCE: u32 = 0xDB;
pub const CUE_REF_TIME: u32 = 0x96;
pub const CUE_REF_CLUSTER: u32 = 0x97;
pub const CUE_REF_NUMBER: u32 = 0x535F;
pub const CUE_REF_CODEC_STATE: u32 = 0xEB;

// Chapters (Segment\Chapters\EditionEntry\ChapterAtom\ChapterDisplay\ChapString).
pub const EDITION_ENTRY: u32 = 0x45B9;
pub const EDITION_UID: u32 = 0x45BC;
pub const EDITION_FLAG_DEFAULT: u32 = 0x45DB;
pub const EDITION_FLAG_ORDERED: u32 = 0x45DD;
pub const CHAPTER_ATOM: u32 = 0xB6;
pub const CHAPTER_UID: u32 = 0x73C4;
pub const CHAPTER_STRING_UID: u32 = 0x5654;
pub const CHAPTER_TIME_START: u32 = 0x91;
pub const CHAPTER_TIME_END: u32 = 0x92;
pub const CHAPTER_FLAG_HIDDEN: u32 = 0x98;
// Legacy pre-RFC Matroska schema element: RFC 9559 dropped
// ChapterFlagEnabled (Table 53 leaves 0x4598 unassigned; the ChapterAtom
// sections jump from ChapterFlagHidden to ChapterSegmentUUID). Historical
// files still carry it, so it is read and round-tripped as an ecosystem
// element (historical default 1 = enabled).
pub const CHAPTER_FLAG_ENABLED: u32 = 0x4598;
pub const CHAPTER_SEGMENT_UUID: u32 = 0x6E67;
pub const CHAPTER_SEGMENT_EDITION_UID: u32 = 0x6EBC;
pub const CHAPTER_PHYSICAL_EQUIV: u32 = 0x63C3;
pub const CHAPTER_DISPLAY: u32 = 0x80;
pub const CHAP_STRING: u32 = 0x85;
pub const CHAP_LANGUAGE: u32 = 0x437C;
pub const CHAP_LANGUAGE_BCP47: u32 = 0x437D;
pub const CHAP_COUNTRY: u32 = 0x437E;

// ChapProcess sub-tree (RFC 9559 §5.1.7.1.4.14–19): per-ChapterAtom
// chapter-codec commands that drive DVD-menu / Matroska-Script chapter
// actions. `ChapProcess` is a master containing the codec id, optional
// private data, and zero or more `ChapProcessCommand` masters (each a
// when-to-run timing plus a binary command payload).
pub const CHAP_PROCESS: u32 = 0x6944;
pub const CHAP_PROCESS_CODEC_ID: u32 = 0x6955;
pub const CHAP_PROCESS_PRIVATE: u32 = 0x450D;
pub const CHAP_PROCESS_COMMAND: u32 = 0x6911;
pub const CHAP_PROCESS_TIME: u32 = 0x6922;
pub const CHAP_PROCESS_DATA: u32 = 0x6933;

// ChapProcessCodecID values (RFC 9559 §5.1.7.1.4.15, Table 31).
pub const CHAP_PROCESS_CODEC_MATROSKA_SCRIPT: u64 = 0;
pub const CHAP_PROCESS_CODEC_DVD_MENU: u64 = 1;

// ChapProcessTime values (RFC 9559 §5.1.7.1.4.18, Table 32).
pub const CHAP_PROCESS_TIME_DURING: u64 = 0;
pub const CHAP_PROCESS_TIME_BEFORE: u64 = 1;
pub const CHAP_PROCESS_TIME_AFTER: u64 = 2;

// Attachments (Segment\Attachments\AttachedFile\...).
pub const ATTACHED_FILE: u32 = 0x61A7;
pub const FILE_DESCRIPTION: u32 = 0x467E;
pub const FILE_NAME: u32 = 0x466E;
pub const FILE_MIME_TYPE: u32 = 0x4660;
/// `FileMediaType` (RFC 9559 §5.1.6.1.3) — the current spec name for the
/// `0x4660` element [`FILE_MIME_TYPE`] carries; the historical "MimeType"
/// label was renamed to "MediaType" in RFC 9559 (the on-wire id is
/// unchanged). Provided as an alias so spec-name-oriented callers resolve
/// the same constant.
pub const FILE_MEDIA_TYPE: u32 = FILE_MIME_TYPE;
pub const FILE_DATA: u32 = 0x465C;
pub const FILE_UID: u32 = 0x46AE;
// Reclaimed DivX-font AttachedFile children (RFC 9559 Appendix A.40..A.42).
// These three legacy elements survive on old DivX "optimized font" streams
// and are read/written verbatim for faithful re-mux; the container assigns
// them no playback semantics.
/// `FileReferral` (RFC 9559 Appendix A.40, binary) — a binary value a
/// track/codec can refer to when the attachment is needed.
pub const FILE_REFERRAL: u32 = 0x4675;
/// `FileUsedStartTime` (RFC 9559 Appendix A.41, uinteger) — Segment-Tick
/// timestamp at which an optimized font attachment comes into context.
pub const FILE_USED_START_TIME: u32 = 0x4661;
/// `FileUsedEndTime` (RFC 9559 Appendix A.42, uinteger) — Segment-Tick
/// timestamp at which an optimized font attachment goes out of context.
pub const FILE_USED_END_TIME: u32 = 0x4662;

// TrackType values.
pub const TRACK_TYPE_VIDEO: u64 = 1;
pub const TRACK_TYPE_AUDIO: u64 = 2;
pub const TRACK_TYPE_SUBTITLE: u64 = 17;

// FlagInterlaced values (RFC 9559 §5.1.4.1.28.1, Table 3).
pub const FLAG_INTERLACED_UNDETERMINED: u64 = 0;
pub const FLAG_INTERLACED_INTERLACED: u64 = 1;
pub const FLAG_INTERLACED_PROGRESSIVE: u64 = 2;

// FieldOrder values (RFC 9559 §5.1.4.1.28.2, Table 4). All 0..=14 values
// the spec defines are listed; values outside the set pass through via the
// typed enum's `Other` variant.
pub const FIELD_ORDER_PROGRESSIVE: u64 = 0;
pub const FIELD_ORDER_TFF: u64 = 1;
pub const FIELD_ORDER_UNDETERMINED: u64 = 2;
pub const FIELD_ORDER_BFF: u64 = 6;
pub const FIELD_ORDER_TFF_INTERLEAVED: u64 = 9;
pub const FIELD_ORDER_BFF_INTERLEAVED: u64 = 14;

// Video > Projection master (RFC 9559 §5.1.4.1.28.41..§5.1.4.1.28.46): the
// per-track spherical-/VR-video projection description plus a yaw/pitch/roll
// rotation triple. The pose floats are stream-copy (§8) and bounded to the
// ±180 / ±90 / ±180 degree ranges in §5.1.4.1.28.44..46.
pub const PROJECTION: u32 = 0x7670;
pub const PROJECTION_TYPE: u32 = 0x7671;
pub const PROJECTION_PRIVATE: u32 = 0x7672;
pub const PROJECTION_POSE_YAW: u32 = 0x7673;
pub const PROJECTION_POSE_PITCH: u32 = 0x7674;
pub const PROJECTION_POSE_ROLL: u32 = 0x7675;

// ProjectionType values (RFC 9559 §5.1.4.1.28.42, Table 18). §27.15 leaves the
// registry open for future additions; values outside the four registered
// labels pass through the typed enum's `Other(u64)` variant.
pub const PROJECTION_TYPE_RECTANGULAR: u64 = 0;
pub const PROJECTION_TYPE_EQUIRECTANGULAR: u64 = 1;
pub const PROJECTION_TYPE_CUBEMAP: u64 = 2;
pub const PROJECTION_TYPE_MESH: u64 = 3;

// StereoMode values (RFC 9559 §5.1.4.1.28.3, Table 5). All 0..=14 values the
// spec defines are listed; §27.7 leaves the registry open for future
// additions, so values outside this set pass through the typed enum's
// `Other(u64)` variant.
pub const STEREO_MODE_MONO: u64 = 0;
pub const STEREO_MODE_SIDE_BY_SIDE_LEFT_FIRST: u64 = 1;
pub const STEREO_MODE_TOP_BOTTOM_RIGHT_FIRST: u64 = 2;
pub const STEREO_MODE_TOP_BOTTOM_LEFT_FIRST: u64 = 3;
pub const STEREO_MODE_CHECKBOARD_RIGHT_FIRST: u64 = 4;
pub const STEREO_MODE_CHECKBOARD_LEFT_FIRST: u64 = 5;
pub const STEREO_MODE_ROW_INTERLEAVED_RIGHT_FIRST: u64 = 6;
pub const STEREO_MODE_ROW_INTERLEAVED_LEFT_FIRST: u64 = 7;
pub const STEREO_MODE_COLUMN_INTERLEAVED_RIGHT_FIRST: u64 = 8;
pub const STEREO_MODE_COLUMN_INTERLEAVED_LEFT_FIRST: u64 = 9;
pub const STEREO_MODE_ANAGLYPH_CYAN_RED: u64 = 10;
pub const STEREO_MODE_SIDE_BY_SIDE_RIGHT_FIRST: u64 = 11;
pub const STEREO_MODE_ANAGLYPH_GREEN_MAGENTA: u64 = 12;
pub const STEREO_MODE_BOTH_EYES_LACED_LEFT_FIRST: u64 = 13;
pub const STEREO_MODE_BOTH_EYES_LACED_RIGHT_FIRST: u64 = 14;

// TrackPlaneType values (RFC 9559 §5.1.4.1.30.4, Table 20). Values
// 3..=u64::MAX are "First Come First Served" registrations (§27.17).
pub const TRACK_PLANE_TYPE_LEFT_EYE: u64 = 0;
pub const TRACK_PLANE_TYPE_RIGHT_EYE: u64 = 1;
pub const TRACK_PLANE_TYPE_BACKGROUND: u64 = 2;

// ContentEncodingType values (RFC 9559 §5.1.4.1.31.4, Table 22).
pub const CONTENT_ENCODING_TYPE_COMPRESSION: u64 = 0;
pub const CONTENT_ENCODING_TYPE_ENCRYPTION: u64 = 1;

// ContentEncodingScope bit field (RFC 9559 §5.1.4.1.31.3, Table 21).
// Values are big-endian and can be OR'ed; default is 0x1 (Block).
pub const CONTENT_ENCODING_SCOPE_BLOCK: u64 = 0x1;
pub const CONTENT_ENCODING_SCOPE_PRIVATE: u64 = 0x2;
pub const CONTENT_ENCODING_SCOPE_NEXT: u64 = 0x4;

// ContentCompAlgo values (RFC 9559 §5.1.4.1.31.6, Table 23).
pub const CONTENT_COMP_ALGO_ZLIB: u64 = 0;
pub const CONTENT_COMP_ALGO_BZLIB: u64 = 1;
pub const CONTENT_COMP_ALGO_LZO1X: u64 = 2;
pub const CONTENT_COMP_ALGO_HEADER_STRIPPING: u64 = 3;

// ContentEncAlgo values (RFC 9559 §5.1.4.1.31.9, Table 24).
pub const CONTENT_ENC_ALGO_NONE: u64 = 0;
pub const CONTENT_ENC_ALGO_DES: u64 = 1;
pub const CONTENT_ENC_ALGO_3DES: u64 = 2;
pub const CONTENT_ENC_ALGO_TWOFISH: u64 = 3;
pub const CONTENT_ENC_ALGO_BLOWFISH: u64 = 4;
pub const CONTENT_ENC_ALGO_AES: u64 = 5;

// AESSettingsCipherMode values (RFC 9559 §5.1.4.1.31.12, Table 26).
pub const AES_CIPHER_MODE_CTR: u64 = 1;
pub const AES_CIPHER_MODE_CBC: u64 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    /// Modern RFC 9559 element-name aliases must resolve to the same
    /// on-wire ids as the legacy-named constants they wrap (the spec
    /// renamed Timecode→Timestamp and UID→UUID without changing any id).
    #[test]
    fn modern_name_aliases_match_legacy_ids() {
        assert_eq!(TIMESTAMP_SCALE, TIMECODE_SCALE);
        assert_eq!(TIMESTAMP_SCALE, 0x2AD7B1);
        assert_eq!(TIMESTAMP, TIMECODE);
        assert_eq!(TIMESTAMP, 0xE7);
        assert_eq!(SEGMENT_UUID, SEGMENT_UID);
        assert_eq!(SEGMENT_UUID, 0x73A4);
        assert_eq!(PREV_UUID, PREV_UID);
        assert_eq!(PREV_UUID, 0x3CB923);
        assert_eq!(NEXT_UUID, NEXT_UID);
        assert_eq!(NEXT_UUID, 0x3EB923);
        assert_eq!(FILE_MEDIA_TYPE, FILE_MIME_TYPE);
        assert_eq!(FILE_MEDIA_TYPE, 0x4660);
    }
}
