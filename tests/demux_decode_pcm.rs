//! End-to-end demux → decode pipeline check.
//!
//! Writes a PCM S16LE stream to a Matroska file via this crate's muxer,
//! then re-opens it via `ContainerRegistry::open` + walks the packets
//! through `oxideav-basic`'s PCM decoder. The audio data that comes out
//! of the decoder must bit-exactly match the bytes we fed to the muxer —
//! which proves that demux packet slicing, codec ID mapping, and audio
//! track parameters all survive a round trip.

use oxideav_codec::CodecRegistry;
use oxideav_container::{ContainerRegistry, ReadSeek, WriteSeek};
use oxideav_core::{CodecId, CodecParameters, Frame, Packet, SampleFormat, StreamInfo, TimeBase};

fn pcm_stream() -> StreamInfo {
    let mut p = CodecParameters::audio(CodecId::new("pcm_s16le"));
    p.channels = Some(2);
    p.sample_rate = Some(48_000);
    p.sample_format = Some(SampleFormat::S16);
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 48_000),
        duration: None,
        start_time: Some(0),
        params: p,
    }
}

/// Stereo 48 kHz S16LE ramp, `n_frames` samples.
fn ramp_pcm(n_frames: usize, seed: i16) -> Vec<u8> {
    let mut out = Vec::with_capacity(n_frames * 4);
    for i in 0..n_frames {
        let l = (i as i16).wrapping_mul(3).wrapping_add(seed);
        let r = (i as i16).wrapping_mul(5).wrapping_add(seed);
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    out
}

#[test]
fn pcm_mkv_demux_decode_roundtrip() {
    let stream = pcm_stream();
    let frames_per_packet: i64 = 960; // 20 ms @ 48k
    let n_packets = 8usize;
    let packets: Vec<Vec<u8>> = (0..n_packets)
        .map(|i| ramp_pcm(frames_per_packet as usize, i as i16 * 17))
        .collect();
    let sent_samples_total: Vec<u8> = packets.iter().flat_map(|p| p.iter().copied()).collect();

    let tmp = std::env::temp_dir().join("oxideav-mkv-pcm-pipeline.mkv");
    {
        let f = std::fs::File::create(&tmp).unwrap();
        let ws: Box<dyn WriteSeek> = Box::new(f);
        let mut mux = oxideav_mkv::mux::open(ws, std::slice::from_ref(&stream)).unwrap();
        mux.write_header().unwrap();
        for (i, payload) in packets.iter().enumerate() {
            let mut pkt = Packet::new(0, stream.time_base, payload.clone());
            pkt.pts = Some((i as i64) * frames_per_packet);
            pkt.duration = Some(frames_per_packet);
            pkt.flags.keyframe = true;
            mux.write_packet(&pkt).unwrap();
        }
        mux.write_trailer().unwrap();
    }

    // Drive the full pipeline: registry.open → decoder.send_packet → receive_frame.
    let mut containers = ContainerRegistry::new();
    oxideav_mkv::register(&mut containers);

    let mut codecs = CodecRegistry::new();
    oxideav_basic::register_codecs(&mut codecs);

    let rs: Box<dyn ReadSeek> = Box::new(std::fs::File::open(&tmp).unwrap());
    let mut dmx = containers
        .open_demuxer("matroska", rs, &oxideav_core::NullCodecResolver)
        .expect("open mkv");
    assert_eq!(dmx.streams().len(), 1, "exactly one track expected");
    let stream_info = dmx.streams()[0].clone();
    assert_eq!(
        stream_info.params.codec_id,
        CodecId::new("pcm_s16le"),
        "codec id must round-trip as pcm_s16le"
    );
    assert_eq!(stream_info.params.sample_rate, Some(48_000));
    assert_eq!(stream_info.params.channels, Some(2));

    let mut decoder = codecs
        .make_decoder(&stream_info.params)
        .expect("pcm decoder available");

    let mut decoded_interleaved_s16le: Vec<u8> = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(pkt) => {
                decoder.send_packet(&pkt).expect("send_packet");
                while let Ok(frame) = decoder.receive_frame() {
                    match frame {
                        Frame::Audio(af) => {
                            decoded_interleaved_s16le.extend_from_slice(&af.data[0]);
                        }
                        _ => panic!("expected audio frame"),
                    }
                }
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error: {e:?}"),
        }
    }
    // Flush: drain any frames the decoder was buffering.
    decoder.flush().ok();
    while let Ok(Frame::Audio(af)) = decoder.receive_frame() {
        decoded_interleaved_s16le.extend_from_slice(&af.data[0]);
    }

    assert_eq!(
        decoded_interleaved_s16le,
        sent_samples_total,
        "decoded S16LE bytes must match what the muxer received ({} vs {} bytes)",
        decoded_interleaved_s16le.len(),
        sent_samples_total.len()
    );

    let _ = std::fs::remove_file(&tmp);
}
