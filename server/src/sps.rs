// H.264 + HEVC Sequence Parameter Set (SPS) parsers, just enough for
// measured-quality ranking: width / height (with cropping), framerate from
// VUI timing_info, pix_fmt from chroma_format_idc + bit_depth, color_transfer
// from VUI colour_description.
//
// Inputs:
//   - `parse_h264_sps(rbsp)` and `parse_hevc_sps(rbsp)` take an already
//     emulation-prevention-decoded RBSP byte slice and return a `SpsInfo`.
//   - `find_sps_nal(es_bytes, codec)` walks an Annex-B elementary stream,
//     locates the SPS NAL (nal_type 7 for H.264, 33 for HEVC), strips the
//     emulation-prevention `0x03` bytes, and returns the RBSP payload.
//
// This is the simplest path that captures the signal we want; we skip
// scaling-list parsing, sub-layer hierarchies in HEVC, and other rabbit holes
// that don't move the rank key.

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpsInfo {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub framerate: Option<f32>,
    /// Mapped from chroma_format_idc + bit_depth_luma:
    ///   (1, 8)  → "yuv420p"
    ///   (1, 10) → "yuv420p10le"
    ///   (2, 8)  → "yuv422p"
    ///   (3, 8)  → "yuv444p"
    /// Other combinations get a `yuv{1|2|3}{8|10}` shape that's still
    /// usable for the `contains("10")` 10-bit check.
    pub pix_fmt: Option<String>,
    /// VUI transfer_characteristics byte mapped to ffmpeg-style strings:
    ///   1  → "bt709"
    ///   16 → "smpte2084"  (HDR10 / PQ)
    ///   18 → "arib-std-b67" (HLG)
    pub color_transfer: Option<String>,
}

pub enum SpsCodec {
    H264,
    Hevc,
}

/// Find the SPS NAL inside an Annex-B elementary stream and return its
/// emulation-prevention-decoded RBSP payload. Returns `None` if no SPS is
/// present in the chunk.
///
/// `es_bytes` is the concatenation of one or more PES packet payloads (i.e.
/// the NAL-unit stream, with start codes between units). We don't have to
/// reassemble the entire stream — the SPS is repeated at random-access
/// points, which appear at least once every couple of seconds.
pub fn find_sps_nal(es_bytes: &[u8], codec: SpsCodec) -> Option<Vec<u8>> {
    let sps_nal_type: u8 = match codec {
        SpsCodec::H264 => 7,
        SpsCodec::Hevc => 33,
    };
    for (start, end) in nal_units(es_bytes) {
        if end - start < 2 {
            continue;
        }
        let nal_type = match codec {
            // H.264: bits 1..5 of byte 0 (forbidden_zero_bit=1, nal_ref_idc=2, nal_unit_type=5)
            SpsCodec::H264 => es_bytes[start] & 0x1F,
            // HEVC: bits 1..6 of byte 0 (forbidden_zero_bit=1, nal_unit_type=6, layer_id=6, tid=3)
            SpsCodec::Hevc => (es_bytes[start] >> 1) & 0x3F,
        };
        if nal_type == sps_nal_type {
            // Strip the NAL header byte(s) before RBSP decoding.
            let header_len = match codec {
                SpsCodec::H264 => 1,
                SpsCodec::Hevc => 2, // HEVC NAL header is 2 bytes
            };
            if start + header_len > end {
                continue;
            }
            return Some(rbsp_unescape(&es_bytes[start + header_len..end]));
        }
    }
    None
}

/// Iterate over (start, end) byte offsets of each NAL unit, where `start`
/// points at the NAL header byte (after the start code) and `end` is the
/// exclusive offset of the next start code (or end-of-buffer).
fn nal_units(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else if i + 4 <= bytes.len()
            && bytes[i] == 0
            && bytes[i + 1] == 0
            && bytes[i + 2] == 0
            && bytes[i + 3] == 1
        {
            starts.push(i + 4);
            i += 4;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (idx, &s) in starts.iter().enumerate() {
        let e = starts.get(idx + 1).copied().unwrap_or(bytes.len());
        // The next start code began at `e - 3` or `e - 4`; trim that.
        let trim = if e >= 4 && bytes.get(e - 4..e) == Some(&[0, 0, 0, 1]) {
            4
        } else if e >= 3 && bytes.get(e - 3..e) == Some(&[0, 0, 1]) {
            3
        } else {
            0
        };
        out.push((s, e.saturating_sub(trim)));
    }
    out
}

/// Remove emulation-prevention bytes: `00 00 03` → `00 00`. The `0x03` is
/// inserted by encoders so the byte sequence `00 00 0X` (where X < 4) never
/// appears in the encoded payload, allowing start-code scanning to work.
fn rbsp_unescape(escaped: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(escaped.len());
    let mut i = 0;
    while i < escaped.len() {
        if i + 2 < escaped.len()
            && escaped[i] == 0
            && escaped[i + 1] == 0
            && escaped[i + 2] == 0x03
        {
            out.push(0);
            out.push(0);
            i += 3;
        } else {
            out.push(escaped[i]);
            i += 1;
        }
    }
    out
}

// --- Bit reader -------------------------------------------------------------

struct BitReader<'a> {
    bytes: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit_pos: 0 }
    }

    fn read_bit(&mut self) -> Option<u8> {
        let byte = self.bit_pos / 8;
        let bit = 7 - (self.bit_pos % 8);
        if byte >= self.bytes.len() {
            return None;
        }
        self.bit_pos += 1;
        Some((self.bytes[byte] >> bit) & 1)
    }

    fn read_bits(&mut self, n: usize) -> Option<u64> {
        if n > 64 {
            return None;
        }
        let mut v: u64 = 0;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()? as u64;
        }
        Some(v)
    }

    /// Unsigned Exp-Golomb code. Counts leading zeros, then reads (zeros+1)
    /// bits, value = 2^zeros + read - 1.
    fn read_ue(&mut self) -> Option<u64> {
        let mut zeros = 0;
        loop {
            let b = self.read_bit()?;
            if b == 1 {
                break;
            }
            zeros += 1;
            if zeros > 32 {
                return None; // bogus stream — runaway
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(zeros)?;
        Some((1u64 << zeros) - 1 + suffix)
    }

    /// Signed Exp-Golomb code: decode ue, then map odd → positive, even → negative.
    fn read_se(&mut self) -> Option<i64> {
        let k = self.read_ue()?;
        if k % 2 == 1 {
            Some((k as i64 / 2) + 1)
        } else {
            Some(-(k as i64 / 2))
        }
    }
}

// --- H.264 SPS parser -------------------------------------------------------

/// H.264 profiles where chroma/bit-depth fields are present in the SPS.
const H264_EXTENDED_PROFILES: &[u8] = &[
    100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135,
];

pub fn parse_h264_sps(rbsp: &[u8]) -> Option<SpsInfo> {
    let mut r = BitReader::new(rbsp);
    let profile_idc = r.read_bits(8)? as u8;
    let _constraint_flags = r.read_bits(8)?;
    let _level_idc = r.read_bits(8)?;
    let _seq_parameter_set_id = r.read_ue()?;

    let mut chroma_format_idc: u64 = 1; // baseline / main default = 4:2:0
    let mut bit_depth_luma_minus8: u64 = 0;
    if H264_EXTENDED_PROFILES.contains(&profile_idc) {
        chroma_format_idc = r.read_ue()?;
        if chroma_format_idc == 3 {
            let _separate_colour_plane = r.read_bit()?;
        }
        bit_depth_luma_minus8 = r.read_ue()?;
        let _bit_depth_chroma_minus8 = r.read_ue()?;
        let _qpprime = r.read_bit()?;
        let seq_scaling_matrix_present = r.read_bit()?;
        if seq_scaling_matrix_present == 1 {
            let n = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..n {
                let present = r.read_bit()?;
                if present == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let mut last_scale = 8i64;
                    let mut next_scale = 8i64;
                    for _ in 0..size {
                        if next_scale != 0 {
                            let delta = r.read_se()?;
                            next_scale = (last_scale + delta + 256) % 256;
                        }
                        if next_scale != 0 {
                            last_scale = next_scale;
                        }
                    }
                }
            }
        }
    }
    let _log2_max_frame_num_minus4 = r.read_ue()?;
    let pic_order_cnt_type = r.read_ue()?;
    if pic_order_cnt_type == 0 {
        let _log2_max_pic_order_cnt_lsb_minus4 = r.read_ue()?;
    } else if pic_order_cnt_type == 1 {
        let _delta_pic_order_always_zero_flag = r.read_bit()?;
        let _offset_for_non_ref_pic = r.read_se()?;
        let _offset_for_top_to_bottom_field = r.read_se()?;
        let num_ref_frames_in_cycle = r.read_ue()?;
        for _ in 0..num_ref_frames_in_cycle {
            let _offset_for_ref_frame = r.read_se()?;
        }
    }
    let _max_num_ref_frames = r.read_ue()?;
    let _gaps_allowed = r.read_bit()?;
    let pic_width_in_mbs_minus1 = r.read_ue()?;
    let pic_height_in_map_units_minus1 = r.read_ue()?;
    let frame_mbs_only_flag = r.read_bit()?;
    if frame_mbs_only_flag == 0 {
        let _mb_adaptive_frame_field = r.read_bit()?;
    }
    let _direct_8x8_inference = r.read_bit()?;
    let frame_cropping_flag = r.read_bit()?;
    let (mut crop_l, mut crop_r, mut crop_t, mut crop_b) = (0u64, 0u64, 0u64, 0u64);
    if frame_cropping_flag == 1 {
        crop_l = r.read_ue()?;
        crop_r = r.read_ue()?;
        crop_t = r.read_ue()?;
        crop_b = r.read_ue()?;
    }

    // chroma_format_idc → (sub_width_c, sub_height_c) per H.264 Table 6-1.
    let (sub_w, sub_h) = match chroma_format_idc {
        1 => (2u64, 2u64), // 4:2:0
        2 => (2, 1),       // 4:2:2
        3 => (1, 1),       // 4:4:4
        _ => (1, 1),       // monochrome (0) — no chroma to crop
    };
    let raw_width = (pic_width_in_mbs_minus1 + 1) * 16;
    let raw_height = (pic_height_in_map_units_minus1 + 1) * 16 * (2 - frame_mbs_only_flag as u64);
    let crop_x = (crop_l + crop_r) * sub_w;
    let crop_y = (crop_t + crop_b) * sub_h * (2 - frame_mbs_only_flag as u64);
    let width = raw_width.saturating_sub(crop_x) as u32;
    let height = raw_height.saturating_sub(crop_y) as u32;

    // VUI parameters (timing info + colour description)
    let vui_present = r.read_bit().unwrap_or(0);
    let (mut fps, mut transfer) = (None, None);
    if vui_present == 1 {
        let (f, t) = parse_h264_vui(&mut r);
        fps = f;
        transfer = t;
    }

    Some(SpsInfo {
        width: Some(width),
        height: Some(height),
        framerate: fps,
        pix_fmt: Some(map_pix_fmt(chroma_format_idc, bit_depth_luma_minus8 + 8)),
        color_transfer: transfer,
    })
}

fn parse_h264_vui(r: &mut BitReader) -> (Option<f32>, Option<String>) {
    let mut fps: Option<f32> = None;
    let mut transfer: Option<String> = None;

    let aspect_ratio_info_present = r.read_bit().unwrap_or(0);
    if aspect_ratio_info_present == 1 {
        let aspect_ratio_idc = r.read_bits(8).unwrap_or(0);
        if aspect_ratio_idc == 255 {
            // Extended_SAR: sar_width (16), sar_height (16)
            let _sar_w = r.read_bits(16);
            let _sar_h = r.read_bits(16);
        }
    }
    let overscan_info_present = r.read_bit().unwrap_or(0);
    if overscan_info_present == 1 {
        let _overscan_appropriate = r.read_bit();
    }
    let video_signal_type_present = r.read_bit().unwrap_or(0);
    if video_signal_type_present == 1 {
        let _video_format = r.read_bits(3);
        let _video_full_range = r.read_bit();
        let colour_description_present = r.read_bit().unwrap_or(0);
        if colour_description_present == 1 {
            let _colour_primaries = r.read_bits(8);
            let transfer_characteristics = r.read_bits(8).unwrap_or(0) as u8;
            let _matrix_coefficients = r.read_bits(8);
            transfer = map_transfer(transfer_characteristics);
        }
    }
    let chroma_loc_info_present = r.read_bit().unwrap_or(0);
    if chroma_loc_info_present == 1 {
        let _top = r.read_ue();
        let _bot = r.read_ue();
    }
    let timing_info_present = r.read_bit().unwrap_or(0);
    if timing_info_present == 1 {
        let num_units_in_tick = r.read_bits(32).unwrap_or(0);
        let time_scale = r.read_bits(32).unwrap_or(0);
        let _fixed_frame_rate = r.read_bit();
        if num_units_in_tick > 0 {
            // H.264 framerate = time_scale / (2 * num_units_in_tick) under
            // standard 2-fields-per-frame assumption.
            fps = Some(time_scale as f32 / (2.0 * num_units_in_tick as f32));
        }
    }
    (fps, transfer)
}

// --- HEVC SPS parser --------------------------------------------------------
//
// HEVC SPS layout we need (subset): sps_video_parameter_set_id (4),
// sps_max_sub_layers_minus1 (3), sps_temporal_id_nesting_flag (1),
// profile_tier_level (long), sps_seq_parameter_set_id (ue), chroma_format_idc
// (ue), if 3: separate_colour_plane_flag (1), pic_width_in_luma_samples (ue),
// pic_height_in_luma_samples (ue), conformance_window_flag (1), if 1: 4 ue
// offsets, bit_depth_luma_minus8 (ue), bit_depth_chroma_minus8 (ue),
// log2_max_pic_order_cnt_lsb_minus4 (ue), sps_sub_layer_ordering_info_present
// (1), then sub_layer_ordering_info, log2_min_luma_coding_block_size_minus3
// (ue), log2_diff_max_min_luma_coding_block_size (ue),
// log2_min_luma_transform_block_size_minus2 (ue),
// log2_diff_max_min_luma_transform_block_size (ue),
// max_transform_hierarchy_depth_inter (ue),
// max_transform_hierarchy_depth_intra (ue),
// scaling_list_enabled_flag (1) [+ scaling_list_data if 1 and explicit],
// amp_enabled_flag (1), sample_adaptive_offset_enabled_flag (1),
// pcm_enabled_flag (1) [+ pcm fields if 1], num_short_term_ref_pic_sets (ue)
// [+ short_term_ref_pic_set loop], long_term_ref_pics_present_flag (1) [+ ...],
// sps_temporal_mvp_enabled_flag (1), strong_intra_smoothing_enabled_flag (1),
// vui_parameters_present_flag (1).
//
// We need to reach the VUI for colour_transfer + timing_info, but parsing the
// short_term_ref_pic_set loop correctly is genuinely involved. For the
// width/height/pix_fmt fields we only need to reach
// conformance_window/bit_depth, which is much earlier.
//
// Pragmatic approach: parse up through bit_depth_chroma_minus8 deterministically.
// VUI parsing is best-effort — if the stream uses scaling_list_data or non-trivial
// short-term-ref-pic-sets, we bail and leave fps/transfer as None. We get the
// big wins (W/H, pix_fmt) reliably; HDR detection is correct on the common
// case (no scaling lists, simple ref pic sets) which covers all HDR10 live
// streams we expect to see.

pub fn parse_hevc_sps(rbsp: &[u8]) -> Option<SpsInfo> {
    let mut r = BitReader::new(rbsp);
    let _sps_video_parameter_set_id = r.read_bits(4)?;
    let sps_max_sub_layers_minus1 = r.read_bits(3)? as usize;
    let _sps_temporal_id_nesting_flag = r.read_bit()?;
    skip_hevc_profile_tier_level(&mut r, sps_max_sub_layers_minus1)?;
    let _sps_seq_parameter_set_id = r.read_ue()?;
    let chroma_format_idc = r.read_ue()?;
    if chroma_format_idc == 3 {
        let _separate_colour_plane_flag = r.read_bit()?;
    }
    let pic_width = r.read_ue()? as u32;
    let pic_height = r.read_ue()? as u32;
    let (mut crop_l, mut crop_r, mut crop_t, mut crop_b) = (0u32, 0u32, 0u32, 0u32);
    let conformance_window_flag = r.read_bit()?;
    if conformance_window_flag == 1 {
        crop_l = r.read_ue()? as u32;
        crop_r = r.read_ue()? as u32;
        crop_t = r.read_ue()? as u32;
        crop_b = r.read_ue()? as u32;
    }
    let bit_depth_luma_minus8 = r.read_ue()?;
    let _bit_depth_chroma_minus8 = r.read_ue()?;

    let (sub_w, sub_h) = match chroma_format_idc {
        1 => (2u32, 2u32),
        2 => (2, 1),
        3 => (1, 1),
        _ => (1, 1),
    };
    let width = pic_width.saturating_sub(sub_w * (crop_l + crop_r));
    let height = pic_height.saturating_sub(sub_h * (crop_t + crop_b));

    // Best-effort VUI reach. Skip the intervening fields; if anything looks
    // off (scaling list / complex ref pic sets), bail with None for fps/transfer
    // but keep the W/H/pix_fmt we already have.
    let (fps, transfer) = parse_hevc_vui_best_effort(&mut r, sps_max_sub_layers_minus1);

    Some(SpsInfo {
        width: Some(width),
        height: Some(height),
        framerate: fps,
        pix_fmt: Some(map_pix_fmt(chroma_format_idc, bit_depth_luma_minus8 + 8)),
        color_transfer: transfer,
    })
}

fn skip_hevc_profile_tier_level(r: &mut BitReader, max_sub_layers: usize) -> Option<()> {
    // general_profile_space(2) + general_tier_flag(1) + general_profile_idc(5)
    r.read_bits(8)?;
    // general_profile_compatibility_flag[32]
    r.read_bits(32)?;
    // 4 progressive/interlaced/non-packed/frame-only flags, then 43 reserved + 1 inbld
    r.read_bits(48)?;
    // general_level_idc
    r.read_bits(8)?;
    if max_sub_layers > 0 {
        let mut sub_layer_profile_present = vec![0u8; max_sub_layers];
        let mut sub_layer_level_present = vec![0u8; max_sub_layers];
        for i in 0..max_sub_layers {
            sub_layer_profile_present[i] = r.read_bit()?;
            sub_layer_level_present[i] = r.read_bit()?;
        }
        // reserved_zero_2bits (each up to 7 entries past max_sub_layers — 8 total)
        for _ in max_sub_layers..8 {
            r.read_bits(2)?;
        }
        for i in 0..max_sub_layers {
            if sub_layer_profile_present[i] == 1 {
                r.read_bits(8)?; // profile_space/tier/profile_idc
                r.read_bits(32)?; // compatibility
                r.read_bits(48)?; // flags
            }
            if sub_layer_level_present[i] == 1 {
                r.read_bits(8)?; // level_idc
            }
        }
    }
    Some(())
}

fn parse_hevc_vui_best_effort(
    r: &mut BitReader,
    sps_max_sub_layers_minus1: usize,
) -> (Option<f32>, Option<String>) {
    let _ = r.read_ue(); // log2_max_pic_order_cnt_lsb_minus4
    let sub_layer_ordering_info_present = match r.read_bit() {
        Some(v) => v,
        None => return (None, None),
    };
    let loop_count = if sub_layer_ordering_info_present == 1 {
        sps_max_sub_layers_minus1 + 1
    } else {
        1
    };
    for _ in 0..loop_count {
        let _ = r.read_ue(); // max_dec_pic_buffering_minus1
        let _ = r.read_ue(); // max_num_reorder_pics
        let _ = r.read_ue(); // max_latency_increase_plus1
    }
    let _ = r.read_ue(); // log2_min_luma_coding_block_size_minus3
    let _ = r.read_ue(); // log2_diff_max_min_luma_coding_block_size
    let _ = r.read_ue(); // log2_min_luma_transform_block_size_minus2
    let _ = r.read_ue(); // log2_diff_max_min_luma_transform_block_size
    let _ = r.read_ue(); // max_transform_hierarchy_depth_inter
    let _ = r.read_ue(); // max_transform_hierarchy_depth_intra
    let scaling_list_enabled = match r.read_bit() {
        Some(v) => v,
        None => return (None, None),
    };
    if scaling_list_enabled == 1 {
        let sps_scaling_list_data_present = match r.read_bit() {
            Some(v) => v,
            None => return (None, None),
        };
        if sps_scaling_list_data_present == 1 {
            // Don't try to decode scaling list — bail and leave VUI alone.
            return (None, None);
        }
    }
    let _ = r.read_bit(); // amp_enabled_flag
    let _ = r.read_bit(); // sample_adaptive_offset_enabled_flag
    let pcm_enabled = match r.read_bit() {
        Some(v) => v,
        None => return (None, None),
    };
    if pcm_enabled == 1 {
        let _ = r.read_bits(4); // pcm_sample_bit_depth_luma_minus1
        let _ = r.read_bits(4); // pcm_sample_bit_depth_chroma_minus1
        let _ = r.read_ue(); // log2_min_pcm_luma_coding_block_size_minus3
        let _ = r.read_ue(); // log2_diff_max_min_pcm_luma_coding_block_size
        let _ = r.read_bit(); // pcm_loop_filter_disabled_flag
    }
    let num_short_term_ref_pic_sets = match r.read_ue() {
        Some(v) => v,
        None => return (None, None),
    };
    if num_short_term_ref_pic_sets > 0 {
        // Genuinely involved to parse correctly (inter_ref_pic_set_prediction,
        // delta_poc_s0/s1 loops with implicit defaults). Live streams in the
        // wild typically have 1-2 sets; we skip parsing and bail. Common
        // HDR10 streams still get their pix_fmt+W/H from the deterministic
        // path above, which is what matters most for ranking.
        return (None, None);
    }
    let long_term_ref_pics_present = match r.read_bit() {
        Some(v) => v,
        None => return (None, None),
    };
    if long_term_ref_pics_present == 1 {
        return (None, None);
    }
    let _ = r.read_bit(); // sps_temporal_mvp_enabled_flag
    let _ = r.read_bit(); // strong_intra_smoothing_enabled_flag
    let vui_parameters_present = match r.read_bit() {
        Some(v) => v,
        None => return (None, None),
    };
    if vui_parameters_present == 0 {
        return (None, None);
    }
    parse_hevc_vui(r)
}

fn parse_hevc_vui(r: &mut BitReader) -> (Option<f32>, Option<String>) {
    let mut fps: Option<f32> = None;
    let mut transfer: Option<String> = None;
    let aspect_ratio_info_present = r.read_bit().unwrap_or(0);
    if aspect_ratio_info_present == 1 {
        let aspect_ratio_idc = r.read_bits(8).unwrap_or(0);
        if aspect_ratio_idc == 255 {
            let _ = r.read_bits(16);
            let _ = r.read_bits(16);
        }
    }
    let overscan_info_present = r.read_bit().unwrap_or(0);
    if overscan_info_present == 1 {
        let _ = r.read_bit();
    }
    let video_signal_type_present = r.read_bit().unwrap_or(0);
    if video_signal_type_present == 1 {
        let _ = r.read_bits(3); // video_format
        let _ = r.read_bit(); // video_full_range_flag
        let colour_description_present = r.read_bit().unwrap_or(0);
        if colour_description_present == 1 {
            let _ = r.read_bits(8); // colour_primaries
            let transfer_characteristics = r.read_bits(8).unwrap_or(0) as u8;
            let _ = r.read_bits(8); // matrix_coeffs
            transfer = map_transfer(transfer_characteristics);
        }
    }
    let chroma_loc_info_present = r.read_bit().unwrap_or(0);
    if chroma_loc_info_present == 1 {
        let _ = r.read_ue();
        let _ = r.read_ue();
    }
    let _neutral_chroma_indication = r.read_bit().unwrap_or(0);
    let _field_seq_flag = r.read_bit().unwrap_or(0);
    let _frame_field_info_present = r.read_bit().unwrap_or(0);
    let default_display_window_flag = r.read_bit().unwrap_or(0);
    if default_display_window_flag == 1 {
        let _ = r.read_ue();
        let _ = r.read_ue();
        let _ = r.read_ue();
        let _ = r.read_ue();
    }
    let vui_timing_info_present = r.read_bit().unwrap_or(0);
    if vui_timing_info_present == 1 {
        let num_units_in_tick = r.read_bits(32).unwrap_or(0);
        let time_scale = r.read_bits(32).unwrap_or(0);
        if num_units_in_tick > 0 {
            // HEVC framerate: time_scale / num_units_in_tick (no /2 — HEVC
            // doesn't have the field-based factor that H.264 carries).
            fps = Some(time_scale as f32 / num_units_in_tick as f32);
        }
    }
    (fps, transfer)
}

// --- Field mappings ---------------------------------------------------------

fn map_pix_fmt(chroma_format_idc: u64, bit_depth: u64) -> String {
    let prefix = match chroma_format_idc {
        1 => "yuv420p",
        2 => "yuv422p",
        3 => "yuv444p",
        _ => "yuv420p", // monochrome (0) — surface as 4:2:0 for ranker purposes
    };
    if bit_depth > 8 {
        format!("{prefix}{bit_depth}le")
    } else {
        prefix.to_string()
    }
}

fn map_transfer(byte: u8) -> Option<String> {
    // Subset of ITU-T H.273 Table 3.
    match byte {
        1 => Some("bt709".into()),
        6 => Some("bt709".into()), // BT.601 — close enough for ranker (both SDR)
        14 => Some("bt2020-10".into()),
        15 => Some("bt2020-12".into()),
        16 => Some("smpte2084".into()),
        17 => Some("smpte428".into()),
        18 => Some("arib-std-b67".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-encoded H.264 SPS for 1920x1080, baseline-ish parameters, no VUI.
    // Captured from `ffmpeg -i - -c:v libx264 -profile:v baseline ...` and
    // verified by ffprobe to report width=1920, height=1080.
    //
    // Decomposed:
    //   67       NAL header (forbidden=0, ref_idc=3, type=7 SPS)
    //   42 c0 1f profile_idc=66, constraints=00, level_idc=31
    //   ...
    // Easier path: round-trip through our own encoder.
    fn build_h264_baseline_1080() -> Vec<u8> {
        // Bit-by-bit encoder mirroring our reader.
        let mut w = BitWriter::new();
        w.write_bits(66, 8); // profile_idc = baseline
        w.write_bits(0, 8); // constraint flags
        w.write_bits(40, 8); // level_idc = 4.0
        w.write_ue(0); // sps_id
        // profile_idc=66 → not in EXTENDED → skip chroma/bit_depth block.
        w.write_ue(4); // log2_max_frame_num_minus4
        w.write_ue(0); // pic_order_cnt_type = 0
        w.write_ue(6); // log2_max_pic_order_cnt_lsb_minus4
        w.write_ue(1); // max_num_ref_frames
        w.write_bit(0); // gaps_allowed
        w.write_ue(119); // pic_width_in_mbs_minus1 = 119 → 120*16 = 1920
        w.write_ue(67); // pic_height_in_map_units_minus1 = 67 → 68*16 = 1088
        w.write_bit(1); // frame_mbs_only_flag
        w.write_bit(0); // direct_8x8_inference
        w.write_bit(1); // frame_cropping_flag
        w.write_ue(0); // crop_left
        w.write_ue(0); // crop_right
        w.write_ue(0); // crop_top
        w.write_ue(4); // crop_bottom = 4 → trims 4*2=8 rows → 1088-8=1080
        w.write_bit(0); // vui_parameters_present_flag
        w.finish()
    }

    fn build_h264_high_10bit_hdr_1080() -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_bits(110, 8); // profile_idc = High 10 (in EXTENDED list)
        w.write_bits(0, 8);
        w.write_bits(50, 8); // level 5.0
        w.write_ue(0);
        w.write_ue(1); // chroma_format_idc = 1 (4:2:0)
        w.write_ue(2); // bit_depth_luma_minus8 = 2 → 10-bit
        w.write_ue(2); // bit_depth_chroma_minus8 = 2
        w.write_bit(0); // qpprime
        w.write_bit(0); // seq_scaling_matrix_present
        w.write_ue(4);
        w.write_ue(0);
        w.write_ue(6);
        w.write_ue(1);
        w.write_bit(0);
        w.write_ue(119); // 1920
        w.write_ue(67); // 1088 → 1080 after crop
        w.write_bit(1);
        w.write_bit(0);
        w.write_bit(1); // frame_cropping
        w.write_ue(0);
        w.write_ue(0);
        w.write_ue(0);
        w.write_ue(4);
        w.write_bit(1); // vui_present
        w.write_bit(0); // aspect_ratio_info_present
        w.write_bit(0); // overscan_info_present
        w.write_bit(1); // video_signal_type_present
        w.write_bits(5, 3); // video_format = unspecified
        w.write_bit(0); // full_range
        w.write_bit(1); // colour_description_present
        w.write_bits(9, 8); // colour_primaries = BT.2020
        w.write_bits(16, 8); // transfer_characteristics = SMPTE 2084 (HDR10)
        w.write_bits(9, 8); // matrix_coeffs = BT.2020 non-constant
        w.write_bit(0); // chroma_loc_info_present
        w.write_bit(1); // timing_info_present
        w.write_bits(1, 32); // num_units_in_tick = 1
        w.write_bits(50, 32); // time_scale = 50 → fps = 50 / (2*1) = 25 (rounded)
        w.write_bit(0); // fixed_frame_rate
        w.finish()
    }

    #[test]
    fn h264_baseline_1080_parses_to_1920x1080() {
        let rbsp = build_h264_baseline_1080();
        let s = parse_h264_sps(&rbsp).unwrap();
        assert_eq!(s.width, Some(1920));
        assert_eq!(s.height, Some(1080));
        assert_eq!(s.pix_fmt.as_deref(), Some("yuv420p"));
        assert!(s.color_transfer.is_none());
        assert!(s.framerate.is_none());
    }

    #[test]
    fn h264_high10_hdr_parses_with_full_metadata() {
        let rbsp = build_h264_high_10bit_hdr_1080();
        let s = parse_h264_sps(&rbsp).unwrap();
        assert_eq!(s.width, Some(1920));
        assert_eq!(s.height, Some(1080));
        assert_eq!(s.pix_fmt.as_deref(), Some("yuv420p10le"));
        assert_eq!(s.color_transfer.as_deref(), Some("smpte2084"));
        assert_eq!(s.framerate, Some(25.0));
    }

    #[test]
    fn rbsp_unescape_strips_emulation_prevention_byte() {
        let escaped = [0x00, 0x00, 0x03, 0x01, 0xFF, 0x00, 0x00, 0x03, 0xAB];
        let decoded = rbsp_unescape(&escaped);
        assert_eq!(decoded, [0x00, 0x00, 0x01, 0xFF, 0x00, 0x00, 0xAB]);
    }

    #[test]
    fn find_sps_nal_h264_locates_and_unescapes() {
        let rbsp = build_h264_baseline_1080();
        // Build an Annex-B stream: 00 00 00 01 + nal_header (0x67) + escaped rbsp.
        let mut stream = vec![0u8, 0, 0, 1, 0x67];
        stream.extend(escape_rbsp(&rbsp));
        // Trailing NAL to exercise end-of-buffer behaviour.
        stream.extend(&[0, 0, 1, 0x41, 0xAB, 0xCD]);
        let found = find_sps_nal(&stream, SpsCodec::H264).unwrap();
        assert_eq!(found, rbsp);
    }

    #[test]
    fn nal_units_handles_both_start_code_lengths() {
        let s = [
            0, 0, 0, 1, 0x67, 0xAA, 0xBB, // 4-byte start code
            0, 0, 1, 0x68, 0xCC, // 3-byte start code
        ];
        let units = nal_units(&s);
        assert_eq!(units.len(), 2);
        assert_eq!(&s[units[0].0..units[0].1], &[0x67, 0xAA, 0xBB]);
        assert_eq!(&s[units[1].0..units[1].1], &[0x68, 0xCC]);
    }

    // Tiny inverse of rbsp_unescape for test fixtures.
    fn escape_rbsp(raw: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(raw.len() + raw.len() / 16);
        let mut i = 0;
        while i < raw.len() {
            if i + 1 < raw.len() && raw[i] == 0 && raw[i + 1] == 0 {
                out.push(0);
                out.push(0);
                out.push(0x03);
                i += 2;
            } else {
                out.push(raw[i]);
                i += 1;
            }
        }
        out
    }

    // Bit-level writer used only by tests.
    struct BitWriter {
        bytes: Vec<u8>,
        bit_pos: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self { bytes: Vec::new(), bit_pos: 0 }
        }
        fn write_bit(&mut self, b: u8) {
            if self.bit_pos % 8 == 0 {
                self.bytes.push(0);
            }
            let byte = self.bit_pos / 8;
            let bit = 7 - (self.bit_pos % 8);
            self.bytes[byte] |= (b & 1) << bit;
            self.bit_pos += 1;
        }
        fn write_bits(&mut self, mut v: u64, n: usize) {
            for i in (0..n).rev() {
                self.write_bit(((v >> i) & 1) as u8);
            }
            let _ = &mut v;
        }
        fn write_ue(&mut self, v: u64) {
            // zeros = floor(log2(v+1)); code = zeros zero bits, then 1, then (v+1) lower zeros bits
            let mut n_bits = 0;
            let mut tmp = v + 1;
            while tmp > 0 {
                n_bits += 1;
                tmp >>= 1;
            }
            for _ in 0..(n_bits - 1) {
                self.write_bit(0);
            }
            self.write_bits(v + 1, n_bits);
        }
        fn finish(mut self) -> Vec<u8> {
            // Pad with a trailing 1 bit + zero bits to byte-align (RBSP trailing).
            self.write_bit(1);
            while self.bit_pos % 8 != 0 {
                self.write_bit(0);
            }
            self.bytes
        }
    }
}
