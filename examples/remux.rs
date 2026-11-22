//! Dev helper: read a Matroska / WebM file with this crate's demuxer
//! and write it back with this crate's muxer
//! (`cargo run --example remux <in> <out> [options]`).
//!
//! Options: `--webm` (WebM profile), `--live` (unknown-size Segment, no
//! Cues, forward-only writes), `--front-cues <bytes>` (index before the
//! first Cluster), `--cluster-ms <n>` / `--cluster-bytes <n>` (Cluster
//! limits), `--lace` (Xiph lacing of audio), `--plain` (streams, flat
//! metadata and packets only: no track-header copy), `--as-written`
//! (Block timestamps on both sides instead of §11.2 packet timestamps).

use oxideav_core::{Demuxer, Error, Muxer, NullCodecResolver, ReadSeek, StreamInfo};
use oxideav_mkv::demux::open_typed;
use oxideav_mkv::mux::{LacingMode, MkvMuxer};

fn run() -> Result<u64, Box<dyn std::error::Error>> {
    let mut paths = Vec::new();
    let (mut webm, mut live, mut plain, mut lace, mut as_written) =
        (false, false, false, false, false);
    let (mut front_cues, mut cluster_ms, mut cluster_bytes) = (None, None, None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut number = |what: &str| -> Result<u64, Box<dyn std::error::Error>> {
            Ok(args
                .next()
                .ok_or(format!("{what} needs a value"))?
                .parse()?)
        };
        match a.as_str() {
            "--webm" => webm = true,
            "--live" => live = true,
            "--plain" => plain = true,
            "--lace" => lace = true,
            "--as-written" => as_written = true,
            "--front-cues" => front_cues = Some(number("--front-cues")?),
            "--cluster-ms" => cluster_ms = Some(number("--cluster-ms")?),
            "--cluster-bytes" => cluster_bytes = Some(number("--cluster-bytes")?),
            _ => paths.push(a),
        }
    }
    let [input, output] = paths.as_slice() else {
        return Err(
            "usage: remux <in> <out> [--webm] [--live] [--front-cues N] \
                    [--cluster-ms N] [--cluster-bytes N] [--lace] [--plain] [--as-written]"
                .into(),
        );
    };
    let reader: Box<dyn ReadSeek> = Box::new(std::fs::File::open(input)?);
    let mut dmx = open_typed(reader, &NullCodecResolver)?;
    dmx.set_timestamps_as_written(as_written);
    let streams: Vec<StreamInfo> = dmx.streams().to_vec();
    let out = Box::new(std::fs::File::create(output)?);
    let mut mux = if webm {
        MkvMuxer::new_webm(out, &streams)?
    } else {
        MkvMuxer::new_matroska(out, &streams)?
    };
    mux.set_timestamps_as_written(as_written);
    if live {
        mux.with_live_streaming()?;
    }
    if let Some(bytes) = front_cues {
        mux.with_front_cues(bytes as u32)?;
    }
    if cluster_ms.is_some() || cluster_bytes.is_some() {
        mux.with_cluster_limits(
            cluster_ms.unwrap_or(5000) as u32,
            cluster_bytes.unwrap_or(5 * 1024 * 1024),
        )?;
    }
    if lace {
        mux.with_block_lacing(LacingMode::Xiph)?;
    }
    // `--plain` is what a caller that only knows the `Demuxer` / `Muxer`
    // traits can do: streams, flat metadata, packets.
    mux.set_metadata(dmx.metadata())?;
    if !plain {
        for i in 0..streams.len() {
            mux.copy_track_header(i, &dmx, i as u32)?;
        }
        // The Segment Duration as the source states it (the measured
        // one cannot know a last frame that was cut short).
        if let Some(us) = dmx.duration_micros().filter(|us| *us > 0)
            && !live
        {
            mux.set_duration(std::time::Duration::from_micros(us as u64))?;
        }
    }
    mux.write_header()?;
    let mut packets = 0u64;
    loop {
        match dmx.next_packet() {
            Ok(p) => {
                if plain {
                    mux.write_packet(&p)?;
                } else {
                    mux.write_packet_from(&p, &dmx)?;
                }
                packets += 1;
            }
            Err(Error::Eof) => break,
            Err(e) => return Err(e.into()),
        }
    }
    mux.write_trailer()?;
    Ok(packets)
}

fn main() {
    match run() {
        Ok(n) => println!("{n} packets"),
        Err(e) => {
            eprintln!("remux: {e}");
            std::process::exit(1);
        }
    }
}
