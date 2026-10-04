//! AAC (`A_AAC`) carriage in the Matroska muxer and demuxer.
//!
//! * Frames are bare access units with the AudioSpecificConfig in
//!   `CodecPrivate`: ADTS-framed packets (the framework AAC encoders'
//!   output) are stripped, and a stream without extradata gets the
//!   AAC-LC ASC its geometry implies.
//! * HE-AAC: `SamplingFrequency` is the core rate and
//!   `OutputSamplingFrequency` the SBR rate; the demuxer reports the
//!   output rate (what the decoder emits) — reporting the core rate made
//!   a decode to WAV play at half speed.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer, NullCodecResolver, Packet, StreamInfo, TimeBase,
};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Cursor<Vec<u8>>>>);

impl std::io::Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for Sink {
    fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(p)
    }
}

fn stream(rate: u32, extradata: Vec<u8>) -> StreamInfo {
    let mut params = CodecParameters::audio(CodecId::new("aac"));
    params.sample_rate = Some(rate);
    params.channels = Some(2);
    params.extradata = extradata;
    StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, 1000),
        duration: None,
        start_time: Some(0),
        params,
    }
}

/// ADTS frame (AAC-LC, 44.1 kHz, stereo, no CRC) around `au`.
fn adts(au: &[u8]) -> Vec<u8> {
    let fl = au.len() + 7;
    let mut v = vec![
        0xFF,
        0xF1,
        (1 << 6) | (4 << 2),
        (2 << 6) | ((fl >> 11) as u8 & 0x03),
        (fl >> 3) as u8,
        ((fl as u8 & 0x07) << 5) | 0x1F,
        0xFC,
    ];
    v.extend_from_slice(au);
    v
}

fn access_units(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| (0..(16 + i)).map(|j| (0x21 + i + j) as u8).collect())
        .collect()
}

fn roundtrip(s: &StreamInfo, packets: &[Vec<u8>]) -> (Box<dyn Demuxer>, Vec<Vec<u8>>) {
    let sink = Sink::default();
    let mut m = oxideav_mkv::mux::open(Box::new(sink.clone()), std::slice::from_ref(s)).unwrap();
    m.write_header().unwrap();
    for (i, d) in packets.iter().enumerate() {
        let p = Packet::new(0, s.time_base, d.clone())
            .with_pts(i as i64 * 23)
            .with_keyframe(true);
        m.write_packet(&p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    let bytes = sink.0.lock().unwrap().get_ref().clone();
    let mut d = oxideav_mkv::demux::open(Box::new(Cursor::new(bytes)), &NullCodecResolver).unwrap();
    let mut out = Vec::new();
    while let Ok(p) = d.next_packet() {
        out.push(p.data);
    }
    (d, out)
}

#[test]
fn adts_packets_become_bare_frames_with_a_codec_private_asc() {
    let aus = access_units(4);
    let packets: Vec<_> = aus.iter().map(|a| adts(a)).collect();
    let (d, got) = roundtrip(&stream(44_100, Vec::new()), &packets);
    let p = &d.streams()[0].params;
    assert_eq!(p.codec_id, CodecId::new("aac"));
    assert_eq!(p.extradata, vec![0x12, 0x10]);
    assert_eq!(p.sample_rate, Some(44_100));
    assert_eq!(got, aus);
}

#[test]
fn bare_access_units_pass_through() {
    let aus = access_units(3);
    let (d, got) = roundtrip(&stream(44_100, vec![0x12, 0x10]), &aus);
    assert_eq!(d.streams()[0].params.extradata, vec![0x12, 0x10]);
    assert_eq!(got, aus);
}

#[test]
fn he_aac_writes_core_and_output_rates_and_demuxes_the_output_rate() {
    // Backward-compatible HE-AAC ASC: 22.05 kHz core, 44.1 kHz SBR.
    let asc = vec![0x13, 0x90, 0x56, 0xE5, 0xA0];
    let aus = access_units(2);
    let sink = Sink::default();
    let s = stream(44_100, asc.clone());
    let mut m = oxideav_mkv::mux::open(Box::new(sink.clone()), std::slice::from_ref(&s)).unwrap();
    m.write_header().unwrap();
    for a in &aus {
        m.write_packet(
            &Packet::new(0, s.time_base, a.clone())
                .with_pts(0)
                .with_keyframe(true),
        )
        .unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    let bytes = sink.0.lock().unwrap().get_ref().clone();
    let d =
        oxideav_mkv::demux::open_typed(Box::new(Cursor::new(bytes)), &NullCodecResolver).unwrap();
    let audio = d.track_audio(0).expect("audio master");
    assert_eq!(audio.sampling_frequency(), 22_050.0);
    assert_eq!(audio.output_sampling_frequency_explicit(), Some(44_100.0));
    assert_eq!(d.streams()[0].params.sample_rate, Some(44_100));
    assert_eq!(d.streams()[0].params.extradata, asc);
}
