//! Dev helper: print every stream and packet of a Matroska / WebM file
//! in a line format that is easy to diff against another reader's
//! report (`cargo run --example dump_packets <file.mkv> [--resilient]`).
//!
//! One `S` line per stream (index, media type, codec id, wire tag, time
//! base, dimensions or rate / channels, language, CodecPrivate-derived
//! extradata length and CRC-32), one `M` line per metadata entry, then
//! one `P` line per packet in file order: stream, pts, dts, duration
//! (stream time base), flags (`K` key frame, `D` discard), payload
//! size and CRC-32 (ISO 3309, the checksum RFC 8794 §11.3.1 names).

use oxideav_core::{Demuxer, Error, NullCodecResolver, ReadSeek};

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (!(crc & 1)).wrapping_add(1));
        }
    }
    !crc
}

fn opt(v: Option<i64>) -> String {
    v.map_or_else(|| "-".to_string(), |v| v.to_string())
}

fn main() {
    let mut path = None;
    let mut resilient = false;
    for a in std::env::args().skip(1) {
        if a == "--resilient" {
            resilient = true;
        } else {
            path = Some(a);
        }
    }
    let Some(path) = path else {
        eprintln!("usage: dump_packets <file.mkv> [--resilient]");
        std::process::exit(2);
    };
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(2);
        }
    };
    let reader: Box<dyn ReadSeek> = Box::new(file);
    let opened = if resilient {
        oxideav_mkv::demux::open_resilient_typed(reader, &NullCodecResolver)
    } else {
        oxideav_mkv::demux::open_typed(reader, &NullCodecResolver)
    };
    let mut dmx = match opened {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{path}: open failed: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "F format={} duration_us={}",
        dmx.format_name(),
        opt(dmx.duration_micros())
    );
    for s in dmx.streams() {
        let p = &s.params;
        println!(
            "S {} {:?} codec={} tag={:?} tb={}/{} start={} dur={} w={} h={} rate={} ch={} lang={} extra={}:{:08x}",
            s.index,
            p.media_type,
            p.codec_id.as_str(),
            p.tag,
            s.time_base.0.num,
            s.time_base.0.den,
            opt(s.start_time),
            opt(s.duration),
            opt(p.width.map(i64::from)),
            opt(p.height.map(i64::from)),
            opt(p.sample_rate.map(i64::from)),
            opt(p.channels.map(i64::from)),
            p.language.as_deref().unwrap_or("-"),
            p.extradata.len(),
            crc32(&p.extradata),
        );
    }
    for (k, v) in dmx.metadata() {
        println!("M {k}={v}");
    }
    let mut count = 0u64;
    loop {
        match dmx.next_packet() {
            Ok(pkt) => {
                count += 1;
                println!(
                    "P {} {} {} {} {}{} {} {:08x}",
                    pkt.stream_index,
                    opt(pkt.pts),
                    opt(pkt.dts),
                    opt(pkt.duration),
                    if pkt.flags.keyframe { "K" } else { "_" },
                    if pkt.flags.discard { "D" } else { "_" },
                    pkt.data.len(),
                    crc32(&pkt.data),
                );
            }
            Err(Error::Eof) => break,
            Err(e) => {
                println!("E after {count} packets: {e}");
                std::process::exit(1);
            }
        }
    }
    println!("END {count}");
}
