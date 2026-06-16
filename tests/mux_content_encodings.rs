//! Round-trip tests for the muxer's `ContentEncodings` (RFC 9559
//! §5.1.4.1.31) write path.
//!
//! Drives `MkvMuxer::set_track_content_encodings` against the public Muxer
//! trait, then re-opens the bytes through the typed
//! [`oxideav_mkv::demux::open_typed`] and confirms
//! `MkvDemuxer::content_encodings(stream_index)` decodes the exact chain
//! handed to the muxer — every `ContentEncodingOrder` / `ContentEncodingScope`
//! / `ContentEncodingType`, the `ContentCompression` (`ContentCompAlgo` +
//! `ContentCompSettings`) and `ContentEncryption` (`ContentEncAlgo` +
//! `ContentEncKeyID` + `ContentEncAESSettings` > `AESSettingsCipherMode`)
//! sub-masters.
//!
//! Spec contracts pinned here:
//!
//! 1. A Header-Stripping compression chain (§5.1.4.1.31.5..§5.1.4.1.31.7)
//!    round-trips, including its `ContentCompSettings` stripped bytes.
//! 2. An AES-CTR encryption step (§5.1.4.1.31.8..§5.1.4.1.31.12) round-trips,
//!    including its `ContentEncKeyID` and the nested `AESSettingsCipherMode`.
//! 3. A multi-step chain re-sorts into descending decode order on read
//!    (§5.1.4.1.31.2), regardless of on-disk order.
//! 4. The `Other(u64)` forward-compat variants on every enum survive verbatim.
//! 5. Omitting the call keeps the `ContentEncodings` master off-disk so the
//!    demuxer surfaces `None`.
//! 6. The setter rejects calls after `write_header`, out-of-range stream
//!    indices, an empty chain, duplicate orders, a zero scope, a non-AES
//!    cipher-mode pairing, and a zero AES cipher mode.
//! 7. The on-disk bytes carry the `ContentEncodings` (`0x6D80`) element id
//!    only when the API was called.
//!
//! These tests use the production demuxer to walk the muxed buffer — no
//! third-party Matroska code is consulted.

use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    CodecId, CodecParameters, Error, Muxer, Packet, ReadSeek, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mkv::demux::{
    AesCipherMode, ContentCompAlgo, ContentEncAlgo, ContentEncoding, ContentEncodingScope,
    ContentEncodingTransform, ContentEncodings,
};
use oxideav_mkv::mux::MkvMuxer;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxideav-mkv-r322-contenc-{}-{}-{n}.mkv",
        tag,
        std::process::id()
    ))
}

fn video_stream(index: u32) -> StreamInfo {
    let mut p = CodecParameters::video(CodecId::new("vp9"));
    p.width = Some(320);
    p.height = Some(240);
    StreamInfo {
        index,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

fn keyframe_packet(stream: u32, pts: i64) -> Packet {
    let mut p = Packet::new(stream, TimeBase::new(1, 1000), vec![0xAA; 16]);
    p.pts = Some(pts);
    p.flags.keyframe = true;
    p
}

/// Mux a two-video-track MKV. `configure` runs between construction and
/// `write_header`, so a test can queue `set_track_content_encodings`.
fn mux_two_tracks<F>(configure: F) -> Vec<u8>
where
    F: FnOnce(&mut MkvMuxer),
{
    let tmp = tmp_path("rt");
    {
        let f = std::fs::File::create(&tmp).expect("create tmp");
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let streams = vec![video_stream(0), video_stream(1)];
        let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer construct");
        configure(&mut mx);
        mx.write_header().expect("write_header");
        mx.write_packet(&keyframe_packet(0, 0)).expect("write 0");
        mx.write_packet(&keyframe_packet(1, 0)).expect("write 1");
        mx.write_trailer().expect("write_trailer");
    }
    let bytes = std::fs::read(&tmp).expect("re-read");
    let _ = std::fs::remove_file(&tmp);
    bytes
}

fn demux_typed(bytes: Vec<u8>) -> oxideav_mkv::demux::MkvDemuxer {
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_mkv::demux::open_typed(rs, &oxideav_core::NullCodecResolver).expect("demux open_typed")
}

fn comp_step(order: u64, scope: u64, algo: ContentCompAlgo, settings: Vec<u8>) -> ContentEncoding {
    ContentEncoding {
        order,
        scope: ContentEncodingScope(scope),
        transform: ContentEncodingTransform::Compression { algo, settings },
    }
}

fn enc_step(
    order: u64,
    scope: u64,
    algo: ContentEncAlgo,
    key_id: Vec<u8>,
    aes_cipher_mode: Option<AesCipherMode>,
) -> ContentEncoding {
    ContentEncoding {
        order,
        scope: ContentEncodingScope(scope),
        transform: ContentEncodingTransform::Encryption {
            algo,
            key_id,
            aes_cipher_mode,
        },
    }
}

#[test]
fn roundtrip_header_stripping_compression() {
    // A single Block-scoped Header-Stripping step with stripped bytes.
    let stripped = vec![0x00, 0x00, 0x01, 0xB6];
    let bytes = mux_two_tracks(|mx| {
        let enc = ContentEncodings {
            encodings: vec![comp_step(
                0,
                0x1,
                ContentCompAlgo::HeaderStripping,
                stripped.clone(),
            )],
        };
        mx.set_track_content_encodings(0, enc)
            .expect("set_track_content_encodings");
    });
    let dmx = demux_typed(bytes);
    let enc = dmx
        .content_encodings(0)
        .expect("stream 0 carries ContentEncodings");
    assert_eq!(enc.encodings.len(), 1);
    let e = &enc.encodings[0];
    assert_eq!(e.order, 0);
    assert!(e.scope.block());
    match &e.transform {
        ContentEncodingTransform::Compression { algo, settings } => {
            assert_eq!(*algo, ContentCompAlgo::HeaderStripping);
            assert_eq!(*settings, stripped);
        }
        other => panic!("expected compression, got {other:?}"),
    }
    // Stream 1 had no call.
    assert!(dmx.content_encodings(1).is_none());
}

#[test]
fn roundtrip_zlib_compression_no_settings() {
    // zlib with no settings: the empty ContentCompSettings stays off-disk
    // and the demuxer materialises the default empty Vec.
    let bytes = mux_two_tracks(|mx| {
        let enc = ContentEncodings {
            encodings: vec![comp_step(0, 0x1, ContentCompAlgo::Zlib, Vec::new())],
        };
        mx.set_track_content_encodings(0, enc).expect("set");
    });
    let dmx = demux_typed(bytes);
    let enc = dmx.content_encodings(0).expect("carries ContentEncodings");
    match &enc.encodings[0].transform {
        ContentEncodingTransform::Compression { algo, settings } => {
            assert_eq!(*algo, ContentCompAlgo::Zlib);
            assert!(settings.is_empty());
        }
        other => panic!("expected compression, got {other:?}"),
    }
}

#[test]
fn roundtrip_aes_ctr_encryption() {
    // An AES-CTR encryption step with a key id and the nested cipher mode.
    let key_id = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
    let bytes = mux_two_tracks(|mx| {
        let enc = ContentEncodings {
            encodings: vec![enc_step(
                0,
                0x1,
                ContentEncAlgo::Aes,
                key_id.clone(),
                Some(AesCipherMode::Ctr),
            )],
        };
        mx.set_track_content_encodings(0, enc).expect("set");
    });
    let dmx = demux_typed(bytes);
    let enc = dmx.content_encodings(0).expect("carries ContentEncodings");
    match &enc.encodings[0].transform {
        ContentEncodingTransform::Encryption {
            algo,
            key_id: kid,
            aes_cipher_mode,
        } => {
            assert_eq!(*algo, ContentEncAlgo::Aes);
            assert_eq!(*kid, key_id);
            assert_eq!(*aes_cipher_mode, Some(AesCipherMode::Ctr));
        }
        other => panic!("expected encryption, got {other:?}"),
    }
}

#[test]
fn roundtrip_non_aes_encryption_no_aes_settings() {
    // A Twofish step carries no ContentEncAESSettings (Table 25 forbids it
    // on non-AES algos); the demuxer surfaces aes_cipher_mode == None.
    let bytes = mux_two_tracks(|mx| {
        let enc = ContentEncodings {
            encodings: vec![enc_step(0, 0x1, ContentEncAlgo::Twofish, Vec::new(), None)],
        };
        mx.set_track_content_encodings(0, enc).expect("set");
    });
    let dmx = demux_typed(bytes);
    match &dmx.content_encodings(0).unwrap().encodings[0].transform {
        ContentEncodingTransform::Encryption {
            algo,
            key_id,
            aes_cipher_mode,
        } => {
            assert_eq!(*algo, ContentEncAlgo::Twofish);
            assert!(key_id.is_empty());
            assert_eq!(*aes_cipher_mode, None);
        }
        other => panic!("expected encryption, got {other:?}"),
    }
}

#[test]
fn roundtrip_multi_step_decode_order() {
    // A compression step (order 0) and an encryption step (order 1). On
    // write the muxer emits ascending order; on read the demuxer re-sorts
    // into descending DECODE order (highest first), so encryption (1) comes
    // before compression (0) — §5.1.4.1.31.2.
    let bytes = mux_two_tracks(|mx| {
        let enc = ContentEncodings {
            encodings: vec![
                comp_step(0, 0x3, ContentCompAlgo::HeaderStripping, vec![0xAB, 0xCD]),
                enc_step(
                    1,
                    0x1,
                    ContentEncAlgo::Aes,
                    vec![0x11],
                    Some(AesCipherMode::Cbc),
                ),
            ],
        };
        mx.set_track_content_encodings(0, enc).expect("set");
    });
    let dmx = demux_typed(bytes);
    let chain = &dmx.content_encodings(0).unwrap().encodings;
    assert_eq!(chain.len(), 2);
    // Decode order: highest order (1, the encryption) first.
    assert_eq!(chain[0].order, 1);
    assert!(matches!(
        chain[0].transform,
        ContentEncodingTransform::Encryption { .. }
    ));
    assert_eq!(chain[1].order, 0);
    assert!(matches!(
        chain[1].transform,
        ContentEncodingTransform::Compression { .. }
    ));
    // The compression step's combined Block+Private scope survives.
    assert!(chain[1].scope.block());
    assert!(chain[1].scope.private());
}

#[test]
fn roundtrip_other_forward_compat_variants() {
    // Future registry values pass through verbatim on both the comp-algo
    // and enc-algo enums, and on the AES cipher mode.
    let bytes = mux_two_tracks(|mx| {
        let enc = ContentEncodings {
            encodings: vec![
                comp_step(0, 0x1, ContentCompAlgo::Other(42), vec![0x09]),
                enc_step(
                    1,
                    0x2,
                    ContentEncAlgo::Other(99),
                    Vec::new(),
                    // Other(7) on a non-AES algo would be rejected; but the
                    // cipher mode pin is AES-only, so leave it off here.
                    None,
                ),
            ],
        };
        mx.set_track_content_encodings(0, enc).expect("set");
    });
    let dmx = demux_typed(bytes);
    let chain = &dmx.content_encodings(0).unwrap().encodings;
    // Decode order: order 1 (enc) first.
    match &chain[0].transform {
        ContentEncodingTransform::Encryption { algo, .. } => {
            assert_eq!(*algo, ContentEncAlgo::Other(99));
        }
        other => panic!("expected encryption, got {other:?}"),
    }
    match &chain[1].transform {
        ContentEncodingTransform::Compression { algo, settings } => {
            assert_eq!(*algo, ContentCompAlgo::Other(42));
            assert_eq!(*settings, vec![0x09]);
        }
        other => panic!("expected compression, got {other:?}"),
    }
}

#[test]
fn roundtrip_aes_other_cipher_mode() {
    // An AES algo with a forward-compat Other cipher mode (non-zero).
    let bytes = mux_two_tracks(|mx| {
        let enc = ContentEncodings {
            encodings: vec![enc_step(
                0,
                0x1,
                ContentEncAlgo::Aes,
                vec![0x42],
                Some(AesCipherMode::Other(7)),
            )],
        };
        mx.set_track_content_encodings(0, enc).expect("set");
    });
    let dmx = demux_typed(bytes);
    match &dmx.content_encodings(0).unwrap().encodings[0].transform {
        ContentEncodingTransform::Encryption {
            aes_cipher_mode, ..
        } => assert_eq!(*aes_cipher_mode, Some(AesCipherMode::Other(7))),
        other => panic!("expected encryption, got {other:?}"),
    }
}

#[test]
fn omitting_call_keeps_master_off_disk() {
    let bytes = mux_two_tracks(|_mx| {});
    // Element id 0x6D80 must not appear anywhere in the file.
    assert!(
        !contains_id(&bytes, &[0x6D, 0x80]),
        "ContentEncodings master must be absent when the API was not called"
    );
    let dmx = demux_typed(bytes);
    assert!(dmx.content_encodings(0).is_none());
    assert!(dmx.content_encodings(1).is_none());
}

#[test]
fn on_disk_carries_element_when_called() {
    let bytes = mux_two_tracks(|mx| {
        let enc = ContentEncodings {
            encodings: vec![comp_step(0, 0x1, ContentCompAlgo::Zlib, Vec::new())],
        };
        mx.set_track_content_encodings(0, enc).expect("set");
    });
    assert!(
        contains_id(&bytes, &[0x6D, 0x80]),
        "ContentEncodings master id 0x6D80 must appear on disk"
    );
}

/// Naive scan for a two-byte element id in the raw buffer. Good enough for
/// the off-disk assertions: the muxer never emits 0x6D80 except as the
/// ContentEncodings master id, and a false positive in frame payload (all
/// 0xAA) is impossible.
fn contains_id(buf: &[u8], id: &[u8]) -> bool {
    buf.windows(id.len()).any(|w| w == id)
}

#[test]
fn rejects_after_write_header() {
    let tmp = tmp_path("late");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
    mx.write_header().expect("write_header");
    let enc = ContentEncodings {
        encodings: vec![comp_step(0, 0x1, ContentCompAlgo::Zlib, Vec::new())],
    };
    let err = mx
        .set_track_content_encodings(0, enc)
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(err, Error::Other(_)), "got {err:?}");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_out_of_range_stream() {
    let tmp = tmp_path("oor");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
    let enc = ContentEncodings {
        encodings: vec![comp_step(0, 0x1, ContentCompAlgo::Zlib, Vec::new())],
    };
    let err = mx
        .set_track_content_encodings(9, enc)
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(err, Error::InvalidData(_)), "got {err:?}");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_empty_chain() {
    let tmp = tmp_path("empty");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
    let enc = ContentEncodings {
        encodings: Vec::new(),
    };
    let err = mx
        .set_track_content_encodings(0, enc)
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(err, Error::InvalidData(_)), "got {err:?}");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_duplicate_order() {
    let tmp = tmp_path("dup");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
    let enc = ContentEncodings {
        encodings: vec![
            comp_step(5, 0x1, ContentCompAlgo::Zlib, Vec::new()),
            comp_step(5, 0x2, ContentCompAlgo::Lzo1x, Vec::new()),
        ],
    };
    let err = mx
        .set_track_content_encodings(0, enc)
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(err, Error::InvalidData(_)), "got {err:?}");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_zero_scope() {
    let tmp = tmp_path("scope0");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
    let enc = ContentEncodings {
        encodings: vec![comp_step(0, 0x0, ContentCompAlgo::Zlib, Vec::new())],
    };
    let err = mx
        .set_track_content_encodings(0, enc)
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(err, Error::InvalidData(_)), "got {err:?}");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_cipher_mode_on_non_aes() {
    let tmp = tmp_path("noaes");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
    let enc = ContentEncodings {
        encodings: vec![enc_step(
            0,
            0x1,
            ContentEncAlgo::Twofish,
            Vec::new(),
            Some(AesCipherMode::Ctr),
        )],
    };
    let err = mx
        .set_track_content_encodings(0, enc)
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(err, Error::InvalidData(_)), "got {err:?}");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn rejects_zero_aes_cipher_mode() {
    let tmp = tmp_path("mode0");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
    let enc = ContentEncodings {
        encodings: vec![enc_step(
            0,
            0x1,
            ContentEncAlgo::Aes,
            Vec::new(),
            Some(AesCipherMode::Other(0)),
        )],
    };
    let err = mx
        .set_track_content_encodings(0, enc)
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(err, Error::InvalidData(_)), "got {err:?}");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn accessor_reads_back_queued_hint() {
    let tmp = tmp_path("acc");
    let f = std::fs::File::create(&tmp).expect("create tmp");
    let ws: Box<dyn WriteSeek> = Box::new(f);
    let streams = vec![video_stream(0)];
    let mut mx = MkvMuxer::new_matroska(ws, &streams).expect("muxer");
    let enc = ContentEncodings {
        encodings: vec![comp_step(3, 0x1, ContentCompAlgo::Zlib, vec![0x01])],
    };
    mx.set_track_content_encodings(0, enc).expect("set");
    let got = mx.content_encodings(0).expect("read back");
    assert_eq!(got.encodings.len(), 1);
    assert_eq!(got.encodings[0].order, 3);
    let _ = std::fs::remove_file(&tmp);
}
