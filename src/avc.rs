//! Annex-B → AVCC repacking for H.264 / `V_MPEG4/ISO/AVC`.
//!
//! Blu-ray (and MPEG-TS in general) ships H.264 as the Annex-B byte
//! stream — NAL units separated by `00 00 00 01` (or `00 00 01`)
//! start-code prefixes, no length headers. Matroska's
//! `V_MPEG4/ISO/AVC` track wants the opposite shape: length-prefixed
//! NAL units (the "AVCC" or "avcC" packetisation), with a per-track
//! `CodecPrivate` payload carrying an `AVCDecoderConfigurationRecord`
//! that names the profile/level and the SPS/PPS NAL units the rest of
//! the stream depends on.
//!
//! The [`annexb_to_avcc`] helper here does the byte-level repack so
//! callers (the MPEG-TS → MKV bridge in particular) don't have to
//! re-implement the wheel. It does **not** parse the SPS RBSP — it
//! just lifts profile_idc / constraint_flags / level_idc from the
//! SPS's three NAL-header-following bytes (which is what the spec
//! requires the configuration record to mirror anyway, ISO/IEC
//! 14496-15 §5.2.4.1.1) and packetises the rest verbatim.
//!
//! References:
//! * ITU-T H.264 Annex B (start-code byte stream format)
//! * ISO/IEC 14496-15 §5.2.4.1 (AVCDecoderConfigurationRecord)
//! * Matroska codec spec, `V_MPEG4/ISO/AVC` row
//!   (<https://www.matroska.org/technical/codec_specs.html>)

/// Result of repacking an Annex-B stream into MKV-shaped AVCC.
///
/// `config_record` is the `AVCDecoderConfigurationRecord` blob the
/// caller writes into the track's `CodecPrivate`. `packetized` is the
/// AVCC-framed elementary stream: every NAL unit prefixed with its
/// 4-byte big-endian length and concatenated. Both buffers are
/// independently usable — the SPS/PPS NAL units appear **only** in
/// `config_record` (not duplicated in `packetized`), matching the
/// shape ffmpeg and mkvmerge produce.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AvccRepack {
    /// Bytes of `AVCDecoderConfigurationRecord` (ISO/IEC 14496-15
    /// §5.2.4.1). Empty when no SPS NAL unit was found in the input
    /// (e.g. an inter-frame-only stream slice fed without the
    /// preceding parameter sets) — callers should bail or look up
    /// the SPS elsewhere.
    pub config_record: Vec<u8>,
    /// Length-prefixed (4-byte BE) NAL units in stream order, with
    /// the SPS and PPS NAL units stripped (they live in
    /// `config_record`). This is what the caller hands the muxer as
    /// each H.264 packet's payload.
    pub packetized: Vec<u8>,
}

/// NAL unit type field (lower 5 bits of the NAL header byte).
const NAL_TYPE_SPS: u8 = 7;
const NAL_TYPE_PPS: u8 = 8;

/// Repack an Annex-B H.264 elementary stream into MKV `V_MPEG4/ISO/AVC`
/// shape: a 4-byte-length-prefixed packetised stream plus an
/// `AVCDecoderConfigurationRecord` for `CodecPrivate`.
///
/// SPS and PPS NAL units found in the stream are collected into the
/// configuration record (one record per unique parameter set, in
/// first-seen order) and **not** re-emitted into the packetised
/// output — this matches the shape ffmpeg / mkvmerge produce and is
/// what `V_MPEG4/ISO/AVC` decoders expect.
///
/// `lengthSizeMinusOne` is always `3` (i.e. 4-byte length prefixes)
/// in the emitted record — the maximum-width choice eliminates any
/// length-overflow class of bugs for files with large NAL units, and
/// every conforming decoder accepts it.
///
/// If no SPS is present the returned [`AvccRepack::config_record`] is
/// empty — callers should treat that as "need to source parameter
/// sets elsewhere" rather than emit a truncated 7-byte record.
pub fn annexb_to_avcc(stream: &[u8]) -> AvccRepack {
    let mut sps_list: Vec<&[u8]> = Vec::new();
    let mut pps_list: Vec<&[u8]> = Vec::new();
    let mut packetized = Vec::with_capacity(stream.len());

    for nal in split_annex_b(stream) {
        if nal.is_empty() {
            continue;
        }
        let nal_type = nal[0] & 0x1F;
        match nal_type {
            NAL_TYPE_SPS => {
                if !sps_list.contains(&nal) {
                    sps_list.push(nal);
                }
            }
            NAL_TYPE_PPS => {
                if !pps_list.contains(&nal) {
                    pps_list.push(nal);
                }
            }
            _ => {
                // 4-byte big-endian length prefix per ISO/IEC 14496-15
                // §5.3.4.2.1 with lengthSizeMinusOne == 3.
                packetized.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                packetized.extend_from_slice(nal);
            }
        }
    }

    let config_record = build_configuration_record(&sps_list, &pps_list);
    AvccRepack {
        config_record,
        packetized,
    }
}

/// Build an `AVCDecoderConfigurationRecord` from the collected SPS and
/// PPS NAL units. Returns an empty buffer when `sps_list` is empty —
/// the record requires at least one SPS to be meaningful (the first
/// SPS supplies profile_idc / level_idc).
fn build_configuration_record(sps_list: &[&[u8]], pps_list: &[&[u8]]) -> Vec<u8> {
    if sps_list.is_empty() {
        return Vec::new();
    }
    // First SPS NAL: byte 0 is the NAL header, bytes 1..4 are
    // profile_idc, constraint_set flags + reserved, level_idc.
    // ISO/IEC 14496-15 §5.2.4.1.1 says these MUST match the active
    // SPS. We require at least 4 bytes for that read; if the SPS is
    // shorter than that it's not a real H.264 SPS and we bail out.
    let first_sps = sps_list[0];
    if first_sps.len() < 4 {
        return Vec::new();
    }
    let profile_idc = first_sps[1];
    let profile_compat = first_sps[2];
    let level_idc = first_sps[3];

    // Cap SPS / PPS counts at 31 — `numOfSequenceParameterSets` is a
    // 5-bit field. In practice nobody ships more than one or two.
    let n_sps = sps_list.len().min(31) as u8;
    let n_pps = pps_list.len().min(255) as u8;

    let mut out = Vec::with_capacity(
        16 + sps_list.iter().map(|s| s.len() + 2).sum::<usize>()
            + pps_list.iter().map(|p| p.len() + 2).sum::<usize>(),
    );
    out.push(1); // configurationVersion
    out.push(profile_idc);
    out.push(profile_compat);
    out.push(level_idc);
    // reserved (6 bits = 111111) | lengthSizeMinusOne (2 bits = 11 → 4-byte lengths)
    out.push(0xFF);
    // reserved (3 bits = 111) | numOfSequenceParameterSets (5 bits)
    out.push(0xE0 | (n_sps & 0x1F));
    for sps in sps_list.iter().take(n_sps as usize) {
        out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        out.extend_from_slice(sps);
    }
    out.push(n_pps);
    for pps in pps_list.iter().take(n_pps as usize) {
        out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        out.extend_from_slice(pps);
    }
    out
}

/// Minimal Annex-B NAL unit splitter (H.264 Annex B). Yields each
/// NAL unit's payload (NAL header + RBSP), without the leading
/// `00 00 [00] 01` start-code prefix.
///
/// Trailing zero-byte padding inside each NAL is stripped (per
/// H.264 §B.1's `trailing_zero_8bits`) so the returned slice ends at
/// the real last byte of the NAL.
fn split_annex_b(data: &[u8]) -> AnnexBIter<'_> {
    AnnexBIter { data, pos: 0 }
}

struct AnnexBIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for AnnexBIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let start_offset = find_start_code(&self.data[self.pos..])?;
        let prefix_pos = self.pos + start_offset;
        let after_prefix = prefix_pos + 3;
        let next_prefix = find_start_code(&self.data[after_prefix..])
            .map(|off| after_prefix + off)
            .unwrap_or(self.data.len());
        let mut nal_end = next_prefix;
        while nal_end > after_prefix && self.data[nal_end - 1] == 0 {
            nal_end -= 1;
        }
        self.pos = next_prefix;
        Some(&self.data[after_prefix..nal_end])
    }
}

/// Find the next `00 00 01` (or `00 00 00 01`) start-code prefix.
/// Returns the offset of the first `00` byte of the 3-byte
/// `00 00 01` core — callers add 3 to skip past it to the NAL byte.
/// The optional leading `00` (turning the prefix into 4 bytes) is
/// handled by the caller's trailing-zero strip on the previous NAL.
fn find_start_code(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 3 {
        return None;
    }
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake SPS NAL: header byte 0x67 (forbidden_zero_bit=0,
    /// nal_ref_idc=3, nal_unit_type=7), then profile_idc / flags /
    /// level_idc bytes the configuration record copies verbatim.
    fn fake_sps(profile_idc: u8, level_idc: u8) -> Vec<u8> {
        vec![0x67, profile_idc, 0x00, level_idc, 0xDE, 0xAD]
    }
    fn fake_pps() -> Vec<u8> {
        // header byte 0x68 (nal_unit_type=8), followed by arbitrary RBSP.
        vec![0x68, 0xEE, 0x3C, 0x80]
    }
    fn fake_idr() -> Vec<u8> {
        // header byte 0x65 (nal_unit_type=5, IDR slice), followed by payload.
        vec![0x65, 0x88, 0x84, 0x00, 0x10]
    }

    fn annex_b_stream(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend_from_slice(&[0, 0, 0, 1]); // 4-byte start code
            out.extend_from_slice(nal);
        }
        out
    }

    /// Smoke test: SPS + PPS + one VCL NAL produces a configuration
    /// record that round-trips the SPS/PPS bytes, and a packetised
    /// stream that contains only the VCL NAL (length-prefixed).
    #[test]
    fn sps_pps_and_idr_split_into_record_and_packets() {
        let sps = fake_sps(0x64, 0x28); // High profile, level 4.0
        let pps = fake_pps();
        let idr = fake_idr();
        let stream = annex_b_stream(&[&sps, &pps, &idr]);
        let out = annexb_to_avcc(&stream);

        // packetized: 4-byte BE length + IDR bytes.
        let mut expected_pkt = Vec::new();
        expected_pkt.extend_from_slice(&(idr.len() as u32).to_be_bytes());
        expected_pkt.extend_from_slice(&idr);
        assert_eq!(out.packetized, expected_pkt);

        // config_record header.
        assert_eq!(out.config_record[0], 1, "configurationVersion");
        assert_eq!(out.config_record[1], 0x64, "profile_idc");
        assert_eq!(out.config_record[2], 0x00, "profile_compatibility");
        assert_eq!(out.config_record[3], 0x28, "level_idc");
        assert_eq!(
            out.config_record[4], 0xFF,
            "reserved + lengthSizeMinusOne = 0xFF (4-byte lengths)"
        );
        assert_eq!(
            out.config_record[5] & 0x1F,
            1,
            "exactly one SPS in the record"
        );
        // SPS length-prefixed body.
        let sps_len_offset = 6;
        let sps_len = u16::from_be_bytes([
            out.config_record[sps_len_offset],
            out.config_record[sps_len_offset + 1],
        ]) as usize;
        assert_eq!(sps_len, sps.len());
        let sps_body_start = sps_len_offset + 2;
        assert_eq!(
            &out.config_record[sps_body_start..sps_body_start + sps_len],
            &sps[..]
        );
        // PPS count then length-prefixed PPS body.
        let pps_count_offset = sps_body_start + sps_len;
        assert_eq!(out.config_record[pps_count_offset], 1);
        let pps_len = u16::from_be_bytes([
            out.config_record[pps_count_offset + 1],
            out.config_record[pps_count_offset + 2],
        ]) as usize;
        assert_eq!(pps_len, pps.len());
        let pps_body_start = pps_count_offset + 3;
        assert_eq!(
            &out.config_record[pps_body_start..pps_body_start + pps_len],
            &pps[..]
        );
    }

    /// Without an SPS in the stream, the configuration record can't
    /// be built — return an empty buffer rather than a partial /
    /// invalid record so the caller can detect the gap.
    #[test]
    fn no_sps_yields_empty_config_record() {
        let idr = fake_idr();
        let stream = annex_b_stream(&[&idr]);
        let out = annexb_to_avcc(&stream);
        assert!(out.config_record.is_empty());
        // Packetised stream still emits the IDR.
        assert_eq!(out.packetized.len(), 4 + idr.len());
    }

    /// Three-byte (`00 00 01`) start codes are accepted alongside
    /// the canonical four-byte form.
    #[test]
    fn three_byte_start_codes_are_recognised() {
        let sps = fake_sps(0x42, 0x14);
        let idr = fake_idr();
        let mut stream = Vec::new();
        stream.extend_from_slice(&[0, 0, 1]); // 3-byte prefix
        stream.extend_from_slice(&sps);
        stream.extend_from_slice(&[0, 0, 0, 1]); // 4-byte prefix
        stream.extend_from_slice(&idr);
        let out = annexb_to_avcc(&stream);
        assert_eq!(out.config_record[1], 0x42, "profile_idc from SPS");
        assert_eq!(out.config_record[3], 0x14, "level_idc from SPS");
        assert_eq!(out.packetized.len(), 4 + idr.len());
    }

    /// Duplicated SPS / PPS NAL units in the stream do not produce
    /// duplicate parameter-set entries in the configuration record.
    #[test]
    fn duplicate_parameter_sets_are_deduplicated() {
        let sps = fake_sps(0x64, 0x1F);
        let pps = fake_pps();
        let idr = fake_idr();
        let stream = annex_b_stream(&[&sps, &pps, &sps, &pps, &idr, &sps]);
        let out = annexb_to_avcc(&stream);
        // n_sps lives in the low 5 bits of byte 5.
        assert_eq!(
            out.config_record[5] & 0x1F,
            1,
            "duplicate SPS NALs collapse to one"
        );
        // PPS count lives at offset 6+2+sps.len() (header + SPS section).
        let pps_count = out.config_record[6 + 2 + sps.len()];
        assert_eq!(pps_count, 1, "duplicate PPS NALs collapse to one");
    }

    /// Packetised stream NAL order matches the order the NAL units
    /// appeared in the input (minus stripped SPS / PPS).
    #[test]
    fn vcl_nal_order_is_preserved() {
        let sps = fake_sps(0x64, 0x28);
        let pps = fake_pps();
        let idr = fake_idr();
        // A second non-IDR slice to confirm two VCL NALs both appear in order.
        let p_slice = vec![0x61, 0xE0, 0x12, 0x34];
        let stream = annex_b_stream(&[&sps, &pps, &idr, &p_slice]);
        let out = annexb_to_avcc(&stream);
        // First length-prefixed NAL.
        let l1 = u32::from_be_bytes(out.packetized[0..4].try_into().unwrap()) as usize;
        assert_eq!(&out.packetized[4..4 + l1], &idr[..]);
        // Second length-prefixed NAL.
        let off2 = 4 + l1;
        let l2 = u32::from_be_bytes(out.packetized[off2..off2 + 4].try_into().unwrap()) as usize;
        assert_eq!(&out.packetized[off2 + 4..off2 + 4 + l2], &p_slice[..]);
        assert_eq!(out.packetized.len(), off2 + 4 + l2);
    }
}
