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
pub const DURATION: u32 = 0x4489;
pub const SEGMENT_UID: u32 = 0x73A4;
pub const MUXING_APP: u32 = 0x4D80;
pub const WRITING_APP: u32 = 0x5741;
pub const TITLE: u32 = 0x7BA9;
pub const DATE_UTC: u32 = 0x4461;

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

// Tracks > TrackEntry.
pub const TRACK_ENTRY: u32 = 0xAE;
pub const TRACK_NUMBER: u32 = 0xD7;
pub const TRACK_UID: u32 = 0x73C5;
pub const TRACK_TYPE: u32 = 0x83;
pub const FLAG_ENABLED: u32 = 0xB9;
pub const FLAG_DEFAULT: u32 = 0x88;
pub const FLAG_LACING: u32 = 0x9C;
pub const NAME: u32 = 0x536E;
pub const LANGUAGE: u32 = 0x22B59C;
pub const CODEC_ID: u32 = 0x86;
pub const CODEC_PRIVATE: u32 = 0x63A2;
pub const CODEC_NAME: u32 = 0x258688;
pub const CODEC_DELAY: u32 = 0x56AA;
pub const SEEK_PRE_ROLL: u32 = 0x56BB;
pub const VIDEO: u32 = 0xE0;
pub const AUDIO: u32 = 0xE1;

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

pub const PIXEL_WIDTH: u32 = 0xB0;
pub const PIXEL_HEIGHT: u32 = 0xBA;

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
pub const SIMPLE_BLOCK: u32 = 0xA3;
pub const BLOCK_GROUP: u32 = 0xA0;
pub const BLOCK: u32 = 0xA1;
pub const BLOCK_DURATION: u32 = 0x9B;
pub const REFERENCE_BLOCK: u32 = 0xFB;

// Cues (seek index).
pub const CUE_POINT: u32 = 0xBB;
pub const CUE_TIME: u32 = 0xB3;
pub const CUE_TRACK_POSITIONS: u32 = 0xB7;
pub const CUE_TRACK: u32 = 0xF7;
pub const CUE_CLUSTER_POSITION: u32 = 0xF1;
pub const CUE_RELATIVE_POSITION: u32 = 0xF0;
pub const CUE_DURATION: u32 = 0xB2;
pub const CUE_BLOCK_NUMBER: u32 = 0x5378;

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
pub const FILE_DATA: u32 = 0x465C;
pub const FILE_UID: u32 = 0x46AE;

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
