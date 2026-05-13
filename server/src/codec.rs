use std::collections::HashMap;

use parking_lot::RwLock;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    Hevc,
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct Classification {
    pub video_codec: Option<VideoCodec>,
    pub video_pid: Option<u16>,
    pub pmt_pid: Option<u16>,
    pub pcr_pid: Option<u16>,
    pub subtitle_pids: Vec<u16>,
}

impl Classification {
    /// LG webOS 9.x B4 chipset can't decode HEVC (h.265). Skip these sources entirely.
    pub fn unplayable_on_webos_b4(&self) -> bool {
        matches!(self.video_codec, Some(VideoCodec::Hevc))
    }

    /// DVB subtitle PIDs stall the webOS demuxer. Strip them in-flight, but only
    /// if the PCR PID isn't one of them (in which case stripping would break
    /// timing — leave the stream alone and let it be demoted instead).
    pub fn strippable_subtitle_pids(&self) -> Vec<u16> {
        if self.subtitle_pids.is_empty() {
            return Vec::new();
        }
        if let Some(pcr) = self.pcr_pid {
            if self.subtitle_pids.contains(&pcr) {
                return Vec::new();
            }
        }
        self.subtitle_pids.clone()
    }
}

#[derive(Default)]
pub struct StreamClassifier {
    inner: RwLock<HashMap<u64, Classification>>,
}

impl StreamClassifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, stream_id: u64) -> Option<Classification> {
        self.inner.read().get(&stream_id).cloned()
    }

    pub fn set(&self, stream_id: u64, c: Classification) {
        self.inner.write().insert(stream_id, c);
    }

    pub fn snapshot(&self) -> Vec<(u64, Classification)> {
        let g = self.inner.read();
        let mut v: Vec<(u64, Classification)> = g.iter().map(|(k, v)| (*k, v.clone())).collect();
        v.sort_by_key(|(k, _)| *k);
        v
    }

    pub fn clear(&self) {
        self.inner.write().clear();
    }
}

// --- MPEG-TS parsing ---------------------------------------------------------

const TS_PACKET_LEN: usize = 188;
const SYNC: u8 = 0x47;

/// Find the offset where the first 188-byte-aligned run of sync bytes starts.
/// Catch-up segments from this provider start with a 70–150-byte opaque prefix
/// before the actual TS stream.
fn ts_alignment(bytes: &[u8]) -> Option<usize> {
    let max = bytes.len().min(512);
    for start in 0..max {
        if bytes[start] != SYNC {
            continue;
        }
        // Require at least one follow-up sync to consider this aligned.
        let next = start + TS_PACKET_LEN;
        if next < bytes.len() && bytes[next] == SYNC {
            return Some(start);
        }
        // Single-packet streams (very short test inputs) are still useful.
        if next >= bytes.len() && start + TS_PACKET_LEN <= bytes.len() {
            return Some(start);
        }
    }
    None
}

struct TsPacketView<'a> {
    pid: u16,
    payload_unit_start: bool,
    has_payload: bool,
    payload_offset: usize,
    bytes: &'a [u8],
}

fn read_packet(bytes: &[u8]) -> Option<TsPacketView<'_>> {
    if bytes.len() < TS_PACKET_LEN || bytes[0] != SYNC {
        return None;
    }
    let pusi = bytes[1] & 0x40 != 0;
    let pid = (((bytes[1] & 0x1F) as u16) << 8) | (bytes[2] as u16);
    let afc = (bytes[3] >> 4) & 0x03;
    let has_af = afc & 0x02 != 0;
    let has_payload = afc & 0x01 != 0;
    let payload_offset = if has_af {
        let af_len = bytes[4] as usize;
        let off = 5 + af_len;
        if off > TS_PACKET_LEN {
            return None;
        }
        off
    } else {
        4
    };
    Some(TsPacketView {
        pid,
        payload_unit_start: pusi,
        has_payload,
        payload_offset,
        bytes,
    })
}

/// PAT packet → PMT PID (first program, ignoring program_number 0 which is NIT).
fn parse_pat_pmt_pid(payload: &[u8]) -> Option<u16> {
    if payload.is_empty() {
        return None;
    }
    let pointer_field = payload[0] as usize;
    let section_start = 1 + pointer_field;
    if section_start + 12 > payload.len() {
        return None;
    }
    if payload[section_start] != 0x00 {
        return None; // not PAT
    }
    let section_length = (((payload[section_start + 1] as usize) & 0x0F) << 8)
        | (payload[section_start + 2] as usize);
    let section_end = section_start + 3 + section_length;
    if section_end > payload.len() {
        return None;
    }
    // Program loop starts after the 5-byte header that follows section_length:
    //   transport_stream_id (2), version (1), section_number (1), last_section_number (1)
    let mut i = section_start + 8;
    let crc_start = section_end - 4;
    while i + 4 <= crc_start {
        let program_number = ((payload[i] as u16) << 8) | (payload[i + 1] as u16);
        let pid = (((payload[i + 2] as u16) & 0x1F) << 8) | (payload[i + 3] as u16);
        if program_number != 0 {
            return Some(pid);
        }
        i += 4;
    }
    None
}

struct PmtSummary {
    pcr_pid: u16,
    streams: Vec<(u8, u16, Vec<u8>)>, // (stream_type, elementary_pid, ES descriptors)
}

fn parse_pmt(payload: &[u8]) -> Option<PmtSummary> {
    if payload.is_empty() {
        return None;
    }
    let pointer_field = payload[0] as usize;
    let section_start = 1 + pointer_field;
    if section_start + 12 > payload.len() {
        return None;
    }
    if payload[section_start] != 0x02 {
        return None; // not PMT
    }
    let section_length = (((payload[section_start + 1] as usize) & 0x0F) << 8)
        | (payload[section_start + 2] as usize);
    let section_end = section_start + 3 + section_length;
    if section_end > payload.len() {
        return None;
    }
    let pcr_pid = (((payload[section_start + 8] as u16) & 0x1F) << 8)
        | (payload[section_start + 9] as u16);
    let program_info_length = (((payload[section_start + 10] as usize) & 0x0F) << 8)
        | (payload[section_start + 11] as usize);
    let mut i = section_start + 12 + program_info_length;
    let crc_start = section_end - 4;
    if i > crc_start {
        return None;
    }
    let mut streams = Vec::new();
    while i + 5 <= crc_start {
        let stream_type = payload[i];
        let pid = (((payload[i + 1] as u16) & 0x1F) << 8) | (payload[i + 2] as u16);
        let es_info_length = (((payload[i + 3] as usize) & 0x0F) << 8)
            | (payload[i + 4] as usize);
        let es_end = i + 5 + es_info_length;
        if es_end > crc_start {
            break;
        }
        let descriptors = payload[i + 5..es_end].to_vec();
        streams.push((stream_type, pid, descriptors));
        i = es_end;
    }
    Some(PmtSummary { pcr_pid, streams })
}

fn has_subtitling_descriptor(descriptors: &[u8]) -> bool {
    let mut i = 0;
    while i + 2 <= descriptors.len() {
        let tag = descriptors[i];
        let len = descriptors[i + 1] as usize;
        if i + 2 + len > descriptors.len() {
            return false;
        }
        // subtitling_descriptor = 0x59, teletext_descriptor = 0x56
        if tag == 0x59 || tag == 0x56 {
            return true;
        }
        i += 2 + len;
    }
    false
}

/// Classify a TS chunk by parsing its PAT and PMT.
pub fn classify_ts_chunk(bytes: &[u8]) -> Option<Classification> {
    let start = ts_alignment(bytes)?;
    let mut pmt_pid: Option<u16> = None;
    let mut i = start;
    while i + TS_PACKET_LEN <= bytes.len() {
        let pkt = read_packet(&bytes[i..i + TS_PACKET_LEN])?;
        if pkt.pid == 0x0000 && pkt.payload_unit_start && pkt.has_payload {
            if let Some(p) = parse_pat_pmt_pid(&pkt.bytes[pkt.payload_offset..]) {
                pmt_pid = Some(p);
                break;
            }
        }
        i += TS_PACKET_LEN;
    }
    let pmt_pid = pmt_pid?;

    let mut i = start;
    while i + TS_PACKET_LEN <= bytes.len() {
        let pkt = read_packet(&bytes[i..i + TS_PACKET_LEN])?;
        if pkt.pid == pmt_pid && pkt.payload_unit_start && pkt.has_payload {
            if let Some(summary) = parse_pmt(&pkt.bytes[pkt.payload_offset..]) {
                return Some(summarize(pmt_pid, summary));
            }
        }
        i += TS_PACKET_LEN;
    }
    None
}

fn summarize(pmt_pid: u16, s: PmtSummary) -> Classification {
    let mut video_codec = None;
    let mut video_pid = None;
    let mut subtitle_pids = Vec::new();
    for (stype, pid, desc) in &s.streams {
        match *stype {
            0x1B => {
                if video_codec.is_none() {
                    video_codec = Some(VideoCodec::H264);
                    video_pid = Some(*pid);
                }
            }
            0x24 | 0x27 => {
                if video_codec.is_none() {
                    video_codec = Some(VideoCodec::Hevc);
                    video_pid = Some(*pid);
                }
            }
            0x01 | 0x02 | 0x10 => {
                if video_codec.is_none() {
                    video_codec = Some(VideoCodec::Other);
                    video_pid = Some(*pid);
                }
            }
            0x06 => {
                if has_subtitling_descriptor(desc) {
                    subtitle_pids.push(*pid);
                }
            }
            _ => {}
        }
    }
    Classification {
        video_codec,
        video_pid,
        pmt_pid: Some(pmt_pid),
        pcr_pid: Some(s.pcr_pid),
        subtitle_pids,
    }
}

// --- TS rewrite: strip subtitle PIDs ----------------------------------------

/// Drop packets matching `subtitle_pids` and rewrite the PMT (at `pmt_pid`) so
/// the dropped ES entries are removed and the section's CRC32 recomputed.
/// Returns the modified TS bytes.
pub fn strip_subtitle_pids(bytes: &[u8], pmt_pid: u16, subtitle_pids: &[u16]) -> Vec<u8> {
    if subtitle_pids.is_empty() {
        return bytes.to_vec();
    }
    let Some(start) = ts_alignment(bytes) else {
        return bytes.to_vec();
    };
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&bytes[..start]);
    let mut i = start;
    while i + TS_PACKET_LEN <= bytes.len() {
        let pkt = &bytes[i..i + TS_PACKET_LEN];
        if pkt[0] != SYNC {
            out.extend_from_slice(&bytes[i..]);
            return out;
        }
        let pid = (((pkt[1] & 0x1F) as u16) << 8) | (pkt[2] as u16);
        if subtitle_pids.contains(&pid) {
            i += TS_PACKET_LEN;
            continue;
        }
        let pusi = pkt[1] & 0x40 != 0;
        if pid == pmt_pid && pusi {
            match rewrite_pmt_packet(pkt, subtitle_pids) {
                Some(rewritten) => out.extend_from_slice(&rewritten),
                None => out.extend_from_slice(pkt),
            }
        } else {
            out.extend_from_slice(pkt);
        }
        i += TS_PACKET_LEN;
    }
    if i < bytes.len() {
        out.extend_from_slice(&bytes[i..]);
    }
    out
}

fn rewrite_pmt_packet(pkt: &[u8], pids_to_strip: &[u16]) -> Option<[u8; TS_PACKET_LEN]> {
    let afc = (pkt[3] >> 4) & 0x03;
    let has_af = afc & 0x02 != 0;
    let payload_start = if has_af {
        let af_len = pkt[4] as usize;
        let off = 5 + af_len;
        if off >= TS_PACKET_LEN {
            return None;
        }
        off
    } else {
        4
    };
    let pointer_field = pkt[payload_start] as usize;
    let section_start = payload_start + 1 + pointer_field;
    if section_start + 12 > TS_PACKET_LEN {
        return None;
    }
    if pkt[section_start] != 0x02 {
        return None;
    }
    let section_length = (((pkt[section_start + 1] as usize) & 0x0F) << 8)
        | (pkt[section_start + 2] as usize);
    let section_end = section_start + 3 + section_length;
    if section_end > TS_PACKET_LEN {
        return None;
    }
    let program_info_length = (((pkt[section_start + 10] as usize) & 0x0F) << 8)
        | (pkt[section_start + 11] as usize);
    let es_loop_start = section_start + 12 + program_info_length;
    let crc_start = section_end - 4;
    if es_loop_start > crc_start {
        return None;
    }

    let mut kept: Vec<(usize, usize)> = Vec::new();
    let mut j = es_loop_start;
    while j + 5 <= crc_start {
        let pid = (((pkt[j + 1] as u16) & 0x1F) << 8) | (pkt[j + 2] as u16);
        let es_info_length = (((pkt[j + 3] as usize) & 0x0F) << 8)
            | (pkt[j + 4] as usize);
        let entry_end = j + 5 + es_info_length;
        if entry_end > crc_start {
            break;
        }
        if !pids_to_strip.contains(&pid) {
            kept.push((j, entry_end));
        }
        j = entry_end;
    }

    let kept_bytes: usize = kept.iter().map(|(s, e)| e - s).sum();
    let new_section_length = 9 + program_info_length + kept_bytes + 4;
    if new_section_length > section_length {
        return None; // we only ever shrink
    }

    let mut out = [0xFFu8; TS_PACKET_LEN];
    out[..payload_start].copy_from_slice(&pkt[..payload_start]);
    out[payload_start] = pkt[payload_start];
    if pointer_field > 0 {
        out[payload_start + 1..section_start].copy_from_slice(&pkt[payload_start + 1..section_start]);
    }
    out[section_start] = pkt[section_start];
    out[section_start + 1] = (pkt[section_start + 1] & 0xF0) | (((new_section_length >> 8) & 0x0F) as u8);
    out[section_start + 2] = (new_section_length & 0xFF) as u8;
    let header_and_pd = 9 + program_info_length;
    out[section_start + 3..section_start + 3 + header_and_pd]
        .copy_from_slice(&pkt[section_start + 3..section_start + 3 + header_and_pd]);
    let mut dst = section_start + 3 + header_and_pd;
    for (s, e) in &kept {
        out[dst..dst + (e - s)].copy_from_slice(&pkt[*s..*e]);
        dst += e - s;
    }
    let crc = crc32_mpeg2(&out[section_start..dst]);
    out[dst] = (crc >> 24) as u8;
    out[dst + 1] = (crc >> 16) as u8;
    out[dst + 2] = (crc >> 8) as u8;
    out[dst + 3] = (crc & 0xFF) as u8;
    Some(out)
}

/// CRC-32/MPEG-2: poly 0x04C11DB7, init 0xFFFFFFFF, no reflection, no XOR-out.
pub fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            if crc & 0x80000000 != 0 {
                crc = (crc << 1) ^ 0x04C11DB7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad_to_188(mut v: Vec<u8>) -> Vec<u8> {
        v.resize(TS_PACKET_LEN, 0xFF);
        v
    }

    /// Build a PAT packet pointing at a single PMT.
    fn build_pat(pmt_pid: u16) -> Vec<u8> {
        // TS header
        let mut p = vec![
            SYNC,
            0x40, 0x00, // PUSI=1, PID=0x0000
            0x10,       // AFC=01, CC=0
        ];
        // Payload: pointer_field + PAT section
        p.push(0x00); // pointer_field
        // section: collect, then prepend table_id + length, then append CRC
        let mut sec = Vec::new();
        sec.push(0x00); // table_id
        // section_length placeholder (2 bytes)
        sec.push(0xB0);
        sec.push(0x00);
        sec.push(0x00); // transport_stream_id hi
        sec.push(0x01); // ts_id lo
        sec.push(0xC1); // version=0, current_next=1
        sec.push(0x00); // section_number
        sec.push(0x00); // last_section_number
        // program loop
        sec.push(0x00); // program_number hi (1)
        sec.push(0x01);
        sec.push(0xE0 | ((pmt_pid >> 8) as u8 & 0x1F));
        sec.push((pmt_pid & 0xFF) as u8);
        // CRC placeholder (4 bytes)
        let crc_pos = sec.len();
        sec.extend_from_slice(&[0; 4]);
        // section_length is the byte count after section_length field, including CRC.
        let section_length = sec.len() - 3;
        sec[1] = 0xB0 | (((section_length >> 8) & 0x0F) as u8);
        sec[2] = (section_length & 0xFF) as u8;
        let crc = crc32_mpeg2(&sec[0..crc_pos]);
        sec[crc_pos] = (crc >> 24) as u8;
        sec[crc_pos + 1] = (crc >> 16) as u8;
        sec[crc_pos + 2] = (crc >> 8) as u8;
        sec[crc_pos + 3] = (crc & 0xFF) as u8;
        p.extend(sec);
        pad_to_188(p)
    }

    /// Build a PMT packet listing the given (stream_type, pid, descriptors) entries.
    fn build_pmt(pmt_pid: u16, pcr_pid: u16, entries: &[(u8, u16, Vec<u8>)]) -> Vec<u8> {
        let mut p = vec![
            SYNC,
            0x40 | ((pmt_pid >> 8) as u8 & 0x1F),
            (pmt_pid & 0xFF) as u8,
            0x10,
            0x00, // pointer_field
        ];
        let mut sec = Vec::new();
        sec.push(0x02); // table_id PMT
        sec.push(0xB0);
        sec.push(0x00);
        sec.push(0x00); // program_number hi
        sec.push(0x01); // program_number lo
        sec.push(0xC1);
        sec.push(0x00);
        sec.push(0x00);
        sec.push(0xE0 | ((pcr_pid >> 8) as u8 & 0x1F));
        sec.push((pcr_pid & 0xFF) as u8);
        sec.push(0xF0); // reserved + program_info_length hi
        sec.push(0x00); // program_info_length lo (no program descriptors)
        for (stype, pid, desc) in entries {
            sec.push(*stype);
            sec.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
            sec.push((pid & 0xFF) as u8);
            sec.push(0xF0 | (((desc.len() >> 8) & 0x0F) as u8));
            sec.push((desc.len() & 0xFF) as u8);
            sec.extend_from_slice(desc);
        }
        let crc_pos = sec.len();
        sec.extend_from_slice(&[0; 4]);
        let section_length = sec.len() - 3;
        sec[1] = 0xB0 | (((section_length >> 8) & 0x0F) as u8);
        sec[2] = (section_length & 0xFF) as u8;
        let crc = crc32_mpeg2(&sec[0..crc_pos]);
        sec[crc_pos] = (crc >> 24) as u8;
        sec[crc_pos + 1] = (crc >> 16) as u8;
        sec[crc_pos + 2] = (crc >> 8) as u8;
        sec[crc_pos + 3] = (crc & 0xFF) as u8;
        p.extend(sec);
        pad_to_188(p)
    }

    fn dummy_packet(pid: u16) -> Vec<u8> {
        let mut p = vec![
            SYNC,
            0x40 | ((pid >> 8) as u8 & 0x1F),
            (pid & 0xFF) as u8,
            0x10,
        ];
        pad_to_188(p.clone())
    }

    #[test]
    fn crc32_mpeg2_known_vector() {
        // Check against the standard test vector "123456789" → 0x0376E6E7.
        assert_eq!(crc32_mpeg2(b"123456789"), 0x0376E6E7);
    }

    #[test]
    fn classify_h264_with_dvb_subs() {
        let pat = build_pat(0x0020);
        let subtitling_desc = vec![0x59, 0x03, b'p', b'o', b'r'];
        let pmt = build_pmt(
            0x0020,
            0x0021,
            &[
                (0x1B, 0x0021, vec![]),                  // h264
                (0x0F, 0x0022, vec![]),                  // ADTS AAC
                (0x06, 0x0023, subtitling_desc),         // private with subtitling
            ],
        );
        let mut bytes = pat;
        bytes.extend(pmt);
        let c = classify_ts_chunk(&bytes).expect("classify");
        assert_eq!(c.video_codec, Some(VideoCodec::H264));
        assert_eq!(c.video_pid, Some(0x0021));
        assert_eq!(c.pmt_pid, Some(0x0020));
        assert_eq!(c.pcr_pid, Some(0x0021));
        assert_eq!(c.subtitle_pids, vec![0x0023]);
        assert!(!c.unplayable_on_webos_b4());
        assert_eq!(c.strippable_subtitle_pids(), vec![0x0023]);
    }

    #[test]
    fn classify_hevc() {
        let pat = build_pat(0x0020);
        let pmt = build_pmt(0x0020, 0x0021, &[(0x24, 0x0021, vec![]), (0x0F, 0x0022, vec![])]);
        let mut bytes = pat;
        bytes.extend(pmt);
        let c = classify_ts_chunk(&bytes).expect("classify");
        assert_eq!(c.video_codec, Some(VideoCodec::Hevc));
        assert!(c.unplayable_on_webos_b4());
        assert!(c.subtitle_pids.is_empty());
    }

    #[test]
    fn classify_skips_prefix_then_finds_alignment() {
        // Catch-up segments have an opaque prefix before TS data starts.
        let mut bytes = vec![0xAA; 96];
        bytes.extend(build_pat(0x0020));
        bytes.extend(build_pmt(0x0020, 0x0021, &[(0x1B, 0x0021, vec![]), (0x0F, 0x0022, vec![])]));
        let c = classify_ts_chunk(&bytes).expect("classify");
        assert_eq!(c.video_codec, Some(VideoCodec::H264));
    }

    #[test]
    fn classify_returns_none_for_garbage() {
        let bytes = vec![0xAA; 1024];
        assert!(classify_ts_chunk(&bytes).is_none());
    }

    #[test]
    fn classify_skips_strip_when_pcr_is_a_subtitle_pid() {
        // PCR_PID == subtitle PID. We refuse to strip in this case.
        let pat = build_pat(0x0020);
        let subtitling = vec![0x59, 0x03, b'p', b'o', b'r'];
        let pmt = build_pmt(
            0x0020,
            0x0023, // PCR_PID = the subtitle PID
            &[
                (0x1B, 0x0021, vec![]),
                (0x06, 0x0023, subtitling),
            ],
        );
        let mut bytes = pat;
        bytes.extend(pmt);
        let c = classify_ts_chunk(&bytes).unwrap();
        assert_eq!(c.subtitle_pids, vec![0x0023]);
        assert!(c.strippable_subtitle_pids().is_empty());
    }

    #[test]
    fn strip_drops_subtitle_packets() {
        let pat = build_pat(0x0020);
        let subtitling = vec![0x59, 0x03, b'p', b'o', b'r'];
        let pmt = build_pmt(
            0x0020,
            0x0021,
            &[
                (0x1B, 0x0021, vec![]),
                (0x0F, 0x0022, vec![]),
                (0x06, 0x0023, subtitling),
            ],
        );
        let video = dummy_packet(0x0021);
        let audio = dummy_packet(0x0022);
        let sub = dummy_packet(0x0023);

        let mut bytes = Vec::new();
        bytes.extend(&pat);
        bytes.extend(&pmt);
        bytes.extend(&video);
        bytes.extend(&audio);
        bytes.extend(&sub);
        bytes.extend(&video);
        bytes.extend(&sub);
        bytes.extend(&audio);

        let stripped = strip_subtitle_pids(&bytes, 0x0020, &[0x0023]);
        // 8 packets in, subtitle PID appeared twice → 6 out.
        assert_eq!(stripped.len(), TS_PACKET_LEN * 6);
        // None of the remaining packets carry PID 0x0023.
        for i in (0..stripped.len()).step_by(TS_PACKET_LEN) {
            let pid = (((stripped[i + 1] & 0x1F) as u16) << 8) | (stripped[i + 2] as u16);
            assert_ne!(pid, 0x0023);
        }
    }

    #[test]
    fn strip_rewrites_pmt_with_valid_crc_and_removed_pid() {
        let pat = build_pat(0x0020);
        let subtitling = vec![0x59, 0x03, b'p', b'o', b'r'];
        let pmt = build_pmt(
            0x0020,
            0x0021,
            &[
                (0x1B, 0x0021, vec![]),
                (0x0F, 0x0022, vec![]),
                (0x06, 0x0023, subtitling),
            ],
        );
        let mut bytes = pat;
        bytes.extend(pmt);
        let stripped = strip_subtitle_pids(&bytes, 0x0020, &[0x0023]);
        // Re-classify the result: subtitle PID should be gone, h264/aac kept,
        // and the PMT's CRC has to validate or parse_pmt would have refused. We
        // verify by running through classify_ts_chunk again.
        let c = classify_ts_chunk(&stripped).expect("re-classify");
        assert_eq!(c.video_codec, Some(VideoCodec::H264));
        assert!(c.subtitle_pids.is_empty(), "subtitles must be removed");

        // Verify the PMT section's CRC32 against its bytes.
        let pmt_pkt = &stripped[TS_PACKET_LEN..TS_PACKET_LEN * 2];
        let payload_start = 4; // no AF
        let pointer = pmt_pkt[payload_start] as usize;
        let section_start = payload_start + 1 + pointer;
        let section_length = (((pmt_pkt[section_start + 1] as usize) & 0x0F) << 8)
            | (pmt_pkt[section_start + 2] as usize);
        let section_end = section_start + 3 + section_length;
        let crc_start = section_end - 4;
        let crc_in_packet = u32::from_be_bytes([
            pmt_pkt[crc_start],
            pmt_pkt[crc_start + 1],
            pmt_pkt[crc_start + 2],
            pmt_pkt[crc_start + 3],
        ]);
        let recomputed = crc32_mpeg2(&pmt_pkt[section_start..crc_start]);
        assert_eq!(crc_in_packet, recomputed);
    }

    #[test]
    fn strip_noop_when_no_pids_listed() {
        let bytes = build_pat(0x0020);
        let out = strip_subtitle_pids(&bytes, 0x0020, &[]);
        assert_eq!(out, bytes);
    }

    #[test]
    fn classifier_get_set_clear() {
        let c = StreamClassifier::new();
        assert!(c.get(42).is_none());
        c.set(
            42,
            Classification {
                video_codec: Some(VideoCodec::Hevc),
                video_pid: Some(0x0100),
                pmt_pid: Some(0x0020),
                pcr_pid: Some(0x0100),
                subtitle_pids: vec![],
            },
        );
        assert!(c.get(42).unwrap().unplayable_on_webos_b4());
        assert_eq!(c.snapshot().len(), 1);
        c.clear();
        assert!(c.get(42).is_none());
    }
}
