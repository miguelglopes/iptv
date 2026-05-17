// ADTS (Audio Data Transport Stream) header parser for raw AAC.
//
// Step 10 / Phase 8: the radio measurement path fetches one segment of an
// audio-only HLS stream (raw AAC frames concatenated) and runs the bytes
// through here to extract the sample rate, channel count, and a bitrate
// estimate. Mirrors the TS classifier (`codec::classify_ts_chunk`) shape
// so `probe::measure_once_audio` and `proxy::handle_ts_segment` look
// symmetric across audio / video.
//
// ADTS frame header layout (7 bytes — 9 with CRC, which we ignore):
//
//   bit  0..11  syncword (must be 0xFFF)
//   bit 12      MPEG version (0 = MPEG-4, 1 = MPEG-2)
//   bit 13..14  layer (always 00)
//   bit 15      protection_absent (1 = no CRC)
//   bit 16..17  profile (1 = AAC Main, 2 = LC, 3 = SSR, 4 = LTP — 1-indexed)
//   bit 18..21  sampling_frequency_index (0..15 — table indexed below)
//   bit 22      private_bit (reserved)
//   bit 23..25  channel_configuration (1 = mono, 2 = stereo, ...)
//   bit 26..28  (originality, home, copyright bits — we don't read them)
//   bit 29..41  frame_length (13 bits — total ADTS frame in bytes incl. header)
//   bit 42..52  buffer_fullness (11 bits — ignored)
//   bit 53..54  number_of_raw_data_blocks_in_frame (2 bits — ignored)
//
// Standard ADTS sampling-frequency table:
//   0:96000 1:88200 2:64000 3:48000 4:44100 5:32000 6:24000 7:22050
//   8:16000 9:12000 10:11025 11:8000 12:7350  (13..15 reserved)

const SAMPLING_FREQUENCY_TABLE: &[u32] = &[
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050,
    16000, 12000, 11025, 8000, 7350,
];

/// Subset of the ADTS header fields the rest of the system actually
/// consumes. Other bits (originality, copyright, etc.) are intentionally
/// left out — adding them later costs nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdtsHeader {
    /// 1..=4 per AAC profile enumeration (1 = Main, 2 = LC, 3 = SSR, 4 = LTP).
    pub profile: u8,
    /// 0..=12 — index into `SAMPLING_FREQUENCY_TABLE`.
    pub sampling_frequency_index: u8,
    /// 0..=7. 0 = "carried out-of-band" / unknown; 1 = mono; 2 = stereo;
    /// 3..=7 = various surround layouts.
    pub channel_configuration: u8,
    /// Total ADTS frame length in bytes, **including** the 7-byte header
    /// (and the 2 CRC bytes when `protection_absent == 0`).
    pub frame_length: u16,
}

impl AdtsHeader {
    pub fn sample_rate_hz(&self) -> Option<u32> {
        SAMPLING_FREQUENCY_TABLE
            .get(self.sampling_frequency_index as usize)
            .copied()
    }

    /// Audio channel count. `Some(0)` means "AAC stream marked
    /// channel_configuration = 0" (out-of-band signalling) — we surface
    /// that as `None` upstream because it's not actually a useful value
    /// for ranking.
    pub fn channels(&self) -> Option<u8> {
        match self.channel_configuration {
            1 => Some(1),
            2 => Some(2),
            3 => Some(3),
            4 => Some(4),
            5 => Some(5),
            6 => Some(6),
            7 => Some(8), // 7 = 7.1 / 8-channel per spec
            _ => None,
        }
    }
}

/// Parse a single ADTS header at offset 0 of `bytes`. Returns `None` if
/// the syncword is wrong or `bytes` is too short. We deliberately don't
/// scan for the syncword — radio HLS segments are clean raw ADTS that
/// start with the header byte; if the first 7 bytes don't parse, the
/// segment isn't ADTS and we should bail rather than hunt.
pub fn parse_adts_header(bytes: &[u8]) -> Option<AdtsHeader> {
    if bytes.len() < 7 {
        return None;
    }
    // Syncword check: first 12 bits must be 0xFFF. Byte 0 = 0xFF, byte 1
    // top nibble = 0xF.
    if bytes[0] != 0xFF || (bytes[1] & 0xF0) != 0xF0 {
        return None;
    }
    // profile = bits 16..17 of header (top 2 bits of byte 2). ADTS stores
    // it as 0..3; the AAC profile enumeration is 1..4, so add 1.
    let profile = ((bytes[2] >> 6) & 0x03) + 1;
    let sampling_frequency_index = (bytes[2] >> 2) & 0x0F;
    // channel_configuration spans byte 2 (low bit) + byte 3 (top 2 bits).
    let channel_configuration = ((bytes[2] & 0x01) << 2) | ((bytes[3] >> 6) & 0x03);
    // frame_length spans bytes 3..5: low 2 bits of byte 3, all of byte 4,
    // top 3 bits of byte 5. 13 bits total.
    let frame_length =
        (((bytes[3] & 0x03) as u16) << 11) | ((bytes[4] as u16) << 3) | ((bytes[5] as u16) >> 5);
    Some(AdtsHeader {
        profile,
        sampling_frequency_index,
        channel_configuration,
        frame_length,
    })
}

/// Audio-classification result the measurement layer cares about. `kbps`
/// is computed from `bytes.len() / duration` — slight overestimate (ADTS
/// header overhead) but good enough for ranking and consistent with how
/// TV bitrate is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioClassification {
    pub sample_rate_hz: Option<u32>,
    pub audio_channels: Option<u8>,
    pub kbps: Option<u32>,
}

/// Classify an ADTS chunk: parse the first frame's header for static
/// fields, compute kbps from the buffer length and `duration` seconds.
/// `duration == 0.0` → no kbps (avoids div-by-zero on init segments etc).
pub fn classify_aac_chunk(bytes: &[u8], duration: f64) -> Option<AudioClassification> {
    let header = parse_adts_header(bytes)?;
    let kbps = if duration > 0.0 {
        let kbps_f = (bytes.len() as f64 * 8.0) / (duration * 1000.0);
        // Sanity floor: a real radio stream is at least ~16 kbps. Below
        // that the segment was probably a partial fetch or a placeholder;
        // surface as None rather than feed garbage to the ranker.
        if kbps_f >= 16.0 {
            Some(kbps_f.round() as u32)
        } else {
            None
        }
    } else {
        None
    };
    Some(AudioClassification {
        sample_rate_hz: header.sample_rate_hz(),
        audio_channels: header.channels(),
        kbps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthesise a 7-byte ADTS header so tests don't need a committed
    /// binary fixture. Caller supplies the meta fields; `frame_length`
    /// fills the 13-bit header field including the 7-byte header itself.
    fn synth_adts_header(
        profile_1based: u8,
        sf_idx: u8,
        channel_cfg: u8,
        frame_length: u16,
    ) -> [u8; 7] {
        let profile_field = (profile_1based - 1) & 0x03;
        let mut h = [0u8; 7];
        h[0] = 0xFF;
        // protection_absent = 1 (no CRC); layer = 00; MPEG-4 (version bit = 0).
        h[1] = 0xF0 | 0b0000_0001;
        h[2] = (profile_field << 6) | ((sf_idx & 0x0F) << 2) | ((channel_cfg >> 2) & 0x01);
        h[3] = ((channel_cfg & 0x03) << 6) | (((frame_length >> 11) & 0x03) as u8);
        h[4] = ((frame_length >> 3) & 0xFF) as u8;
        h[5] = (((frame_length & 0x07) << 5) as u8) | 0x1F; // top of buffer_fullness
        h[6] = 0xFC; // rest of buffer_fullness + num_raw_blocks = 0
        h
    }

    #[test]
    fn parses_lc_mono_44k1() {
        // AAC LC (profile 2), 44.1 kHz (sf_idx 4), mono (ch 1), frame_length = 380 bytes.
        let h = synth_adts_header(2, 4, 1, 380);
        let parsed = parse_adts_header(&h).expect("parses");
        assert_eq!(parsed.profile, 2);
        assert_eq!(parsed.sampling_frequency_index, 4);
        assert_eq!(parsed.channel_configuration, 1);
        assert_eq!(parsed.frame_length, 380);
        assert_eq!(parsed.sample_rate_hz(), Some(44100));
        assert_eq!(parsed.channels(), Some(1));
    }

    #[test]
    fn parses_lc_stereo_48k() {
        let h = synth_adts_header(2, 3, 2, 760);
        let parsed = parse_adts_header(&h).expect("parses");
        assert_eq!(parsed.sample_rate_hz(), Some(48000));
        assert_eq!(parsed.channels(), Some(2));
        assert_eq!(parsed.frame_length, 760);
    }

    #[test]
    fn rejects_bad_syncword() {
        let mut h = synth_adts_header(2, 4, 2, 380);
        h[0] = 0x00; // break the syncword
        assert!(parse_adts_header(&h).is_none());
    }

    #[test]
    fn rejects_short_input() {
        assert!(parse_adts_header(&[0xFF, 0xF1, 0x40]).is_none());
    }

    #[test]
    fn classify_chunk_computes_kbps_from_byte_count_and_duration() {
        // 48 kHz stereo header + ~12 KB payload, 1 s segment → ~96 kbps.
        let mut chunk = synth_adts_header(2, 3, 2, 12000).to_vec();
        chunk.resize(12_000, 0); // pad to 12 KB
        let c = classify_aac_chunk(&chunk, 1.0).expect("classifies");
        assert_eq!(c.sample_rate_hz, Some(48000));
        assert_eq!(c.audio_channels, Some(2));
        // 12000 bytes * 8 / 1000 ms = 96 kbps.
        assert_eq!(c.kbps, Some(96));
    }

    #[test]
    fn classify_returns_none_kbps_when_below_floor() {
        // 7-byte header alone in 1 s → 56 bps → well below the 16 kbps floor.
        let chunk = synth_adts_header(2, 4, 1, 7).to_vec();
        let c = classify_aac_chunk(&chunk, 1.0).expect("classifies");
        assert_eq!(c.kbps, None);
        // Static fields still populated.
        assert_eq!(c.sample_rate_hz, Some(44100));
    }

    #[test]
    fn classify_returns_none_kbps_when_duration_zero() {
        let chunk = synth_adts_header(2, 4, 1, 7).to_vec();
        let c = classify_aac_chunk(&chunk, 0.0).expect("classifies");
        assert_eq!(c.kbps, None);
    }

    #[test]
    fn classify_returns_none_when_not_adts() {
        let bytes = b"not adts at all".to_vec();
        assert!(classify_aac_chunk(&bytes, 1.0).is_none());
    }
}
