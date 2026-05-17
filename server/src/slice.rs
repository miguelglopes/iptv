// H.264 slice-header walker for the `h264_excess_refs` predicate.
//
// Looks at every slice NAL in an elementary-stream chunk, decodes just enough
// of the header to extract `num_ref_idx_l0_active_minus1` / `_l1_...`, and
// checks whether their `+1` exceeds the SPS's `num_ref_frames`. Returns
// `Some(true)` if any slice flags excess, `Some(false)` if none do, `None`
// when the segment can't be parsed as H.264 (no SPS / no PPS).
//
// This is a heuristic — we don't build the full DPB or run
// `ref_pic_list_modification`. The exact case Chromium MSE refuses is
// "active ref list > SPS num_ref_frames", which is what this catches.

use std::collections::HashMap;

use crate::pps::PpsInfo;
use crate::sps::{nal_unit_ranges, rbsp_unescape_bytes, BitReader, SpsInfo};

/// Per H.264 spec slice_type values modulo 5: 0/5=P, 1/6=B, 2/7=I, 3/8=SP, 4/9=SI.
fn is_b_slice(slice_type: u64) -> bool {
    matches!(slice_type % 5, 1)
}
fn is_p_or_b_slice(slice_type: u64) -> bool {
    matches!(slice_type % 5, 0 | 1)
}

/// Slice NAL types that carry coded slice data (not parameter sets, AUD,
/// SEI, end-of-stream, etc.). H.264 NAL types 1 (non-IDR), 5 (IDR), and the
/// auxiliary 19/20 are slice carriers.
fn is_slice_nal(nal_type: u8) -> bool {
    matches!(nal_type, 1 | 5 | 19 | 20)
}

/// Walk the elementary stream once, returning `Some(true)` if any slice
/// references more frames than the SPS declared, `Some(false)` if every
/// slice fits, `None` if there's no SPS or no slice in the chunk (so we
/// can't make a decision either way).
///
/// R2 round-1: takes a map of SPS keyed by `seq_parameter_set_id` instead
/// of a single SPS. A chunk with multiple SPS (broadcast streams that
/// rebase mid-stream) was previously classified against whichever SPS the
/// caller happened to grab first; now the slice header's PPS lookup
/// chains to the right SPS via `pps.seq_parameter_set_id`.
pub fn h264_excess_refs(
    es_bytes: &[u8],
    sps_set: &HashMap<u64, SpsInfo>,
    pps_set: &HashMap<u64, PpsInfo>,
) -> Option<bool> {
    if sps_set.is_empty() || pps_set.is_empty() {
        return None;
    }
    let mut saw_slice = false;

    for (start, end) in nal_unit_ranges(es_bytes) {
        if end <= start {
            continue;
        }
        let nal_header = es_bytes[start];
        let nal_ref_idc = (nal_header >> 5) & 0x03;
        let nal_type = nal_header & 0x1F;
        if !is_slice_nal(nal_type) {
            continue;
        }
        if start + 1 > end {
            continue;
        }
        let rbsp = rbsp_unescape_bytes(&es_bytes[start + 1..end]);
        if let Some(excess) = slice_header_excess(&rbsp, sps_set, pps_set, nal_type, nal_ref_idc) {
            saw_slice = true;
            if excess {
                // Short-circuit: once any slice is over the line we can stop.
                return Some(true);
            }
        }
    }
    if saw_slice {
        Some(false)
    } else {
        None
    }
}

fn slice_header_excess(
    rbsp: &[u8],
    sps_set: &HashMap<u64, SpsInfo>,
    pps_set: &HashMap<u64, PpsInfo>,
    nal_type: u8,
    _nal_ref_idc: u8,
) -> Option<bool> {
    let mut r = BitReader::new(rbsp);
    let _first_mb_in_slice = r.read_ue()?;
    let slice_type = r.read_ue()?;
    let pic_parameter_set_id = r.read_ue()?;
    let pps = pps_set.get(&pic_parameter_set_id)?;
    // Chain to the SPS the PPS references. If the chunk doesn't contain
    // that SPS we treat this slice as undecidable.
    let sps = sps_set.get(&pps.seq_parameter_set_id)?;
    let num_ref_frames = sps.num_ref_frames?;

    // separate_colour_plane_flag controls an extra 2-bit color_plane_id.
    if sps.separate_colour_plane_flag.unwrap_or(0) == 1 {
        r.read_bits(2)?;
    }

    // frame_num
    let frame_num_bits = sps.log2_max_frame_num_minus4.unwrap_or(0) as usize + 4;
    r.read_bits(frame_num_bits)?;

    // field_pic_flag / bottom_field_flag
    let mut field_pic_flag: u8 = 0;
    if sps.frame_mbs_only_flag.unwrap_or(1) == 0 {
        field_pic_flag = r.read_bit()?;
        if field_pic_flag == 1 {
            r.read_bit()?; // bottom_field_flag
        }
    }

    // idr_pic_id for IDR slices
    if nal_type == 5 {
        r.read_ue()?;
    }

    // POC fields
    let poc_type = sps.pic_order_cnt_type.unwrap_or(0);
    if poc_type == 0 {
        let log2_lsb = sps.log2_max_pic_order_cnt_lsb_minus4.unwrap_or(0) as usize + 4;
        r.read_bits(log2_lsb)?;
        if pps.bottom_field_pic_order_in_frame_present_flag == 1 && field_pic_flag == 0 {
            r.read_se()?; // delta_pic_order_cnt_bottom
        }
    } else if poc_type == 1 && sps.delta_pic_order_always_zero_flag.unwrap_or(1) == 0 {
        r.read_se()?; // delta_pic_order_cnt[0]
        if pps.bottom_field_pic_order_in_frame_present_flag == 1 && field_pic_flag == 0 {
            r.read_se()?; // delta_pic_order_cnt[1]
        }
    }

    if pps.redundant_pic_cnt_present_flag == 1 {
        r.read_ue()?; // redundant_pic_cnt
    }

    // B-only direct_spatial_mv_pred_flag
    if is_b_slice(slice_type) {
        r.read_bit()?;
    }

    // num_ref_idx_active_override_flag + overrides apply only to P, SP, B slices.
    let (l0_active, l1_active) = if is_p_or_b_slice(slice_type) || matches!(slice_type % 5, 3) {
        let override_flag = r.read_bit()?;
        if override_flag == 1 {
            let l0 = r.read_ue()?;
            let l1 = if is_b_slice(slice_type) { r.read_ue()? } else { 0 };
            (l0, l1)
        } else {
            (
                pps.num_ref_idx_l0_default_active_minus1,
                pps.num_ref_idx_l1_default_active_minus1,
            )
        }
    } else {
        // I/SI slices don't reference any frames.
        return Some(false);
    };

    let active_max = l0_active.max(if is_b_slice(slice_type) { l1_active } else { 0 });
    // active_max+1 exceeds num_ref_frames → Chromium-MSE refuses.
    Some(active_max + 1 > num_ref_frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pps::PpsInfo;
    use crate::sps::SpsInfo;

    fn fake_sps_id_num_refs(id: u64, n: u64) -> SpsInfo {
        SpsInfo {
            width: Some(1920),
            height: Some(1080),
            framerate: None,
            pix_fmt: None,
            color_transfer: None,
            seq_parameter_set_id: Some(id),
            num_ref_frames: Some(n),
            log2_max_frame_num_minus4: Some(0),
            pic_order_cnt_type: Some(2),
            log2_max_pic_order_cnt_lsb_minus4: None,
            delta_pic_order_always_zero_flag: None,
            frame_mbs_only_flag: Some(1),
            separate_colour_plane_flag: Some(0),
        }
    }

    fn fake_sps_num_refs(n: u64) -> HashMap<u64, SpsInfo> {
        let mut m = HashMap::new();
        m.insert(0, fake_sps_id_num_refs(0, n));
        m
    }

    fn fake_pps_with_sps(pps_id: u64, sps_id: u64, l0: u64, l1: u64) -> PpsInfo {
        PpsInfo {
            pic_parameter_set_id: pps_id,
            seq_parameter_set_id: sps_id,
            num_ref_idx_l0_default_active_minus1: l0,
            num_ref_idx_l1_default_active_minus1: l1,
            bottom_field_pic_order_in_frame_present_flag: 0,
            redundant_pic_cnt_present_flag: 0,
        }
    }

    fn fake_pps(l0: u64, l1: u64) -> HashMap<u64, PpsInfo> {
        let mut m = HashMap::new();
        m.insert(0, fake_pps_with_sps(0, 0, l0, l1));
        m
    }

    // Tiny bit-writer kept local to this test module (mirrors sps.rs).
    struct BitWriter {
        bytes: Vec<u8>,
        bit_pos: usize,
    }
    impl BitWriter {
        fn new() -> Self { Self { bytes: Vec::new(), bit_pos: 0 } }
        fn write_bit(&mut self, b: u8) {
            if self.bit_pos.is_multiple_of(8) { self.bytes.push(0); }
            let byte = self.bit_pos / 8;
            let bit = 7 - (self.bit_pos % 8);
            self.bytes[byte] |= (b & 1) << bit;
            self.bit_pos += 1;
        }
        fn write_bits(&mut self, v: u64, n: usize) {
            for i in (0..n).rev() { self.write_bit(((v >> i) & 1) as u8); }
        }
        fn write_ue(&mut self, v: u64) {
            let mut n_bits = 0;
            let mut tmp = v + 1;
            while tmp > 0 { n_bits += 1; tmp >>= 1; }
            for _ in 0..(n_bits - 1) { self.write_bit(0); }
            self.write_bits(v + 1, n_bits);
        }
        fn finish(mut self) -> Vec<u8> {
            self.write_bit(1);
            while !self.bit_pos.is_multiple_of(8) { self.write_bit(0); }
            self.bytes
        }
    }

    /// Build an Annex-B stream with one non-IDR slice whose override flag is
    /// set, returning l0/l1 active counts to the walker.
    fn build_slice_stream(slice_type: u64, override_flag: u8, l0_active: u64, l1_active: u64) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_ue(0); // first_mb_in_slice
        w.write_ue(slice_type);
        w.write_ue(0); // pic_parameter_set_id
        w.write_bits(0, 4); // frame_num (log2_max_frame_num_minus4=0 → 4 bits)
        // frame_mbs_only_flag=1 → no field_pic_flag.
        // poc_type=2 → no POC fields.
        // redundant_pic_cnt_present_flag=0.
        // B-slice direct_spatial_mv_pred_flag
        if super::is_b_slice(slice_type) { w.write_bit(0); }
        w.write_bit(override_flag);
        if override_flag == 1 {
            w.write_ue(l0_active);
            if super::is_b_slice(slice_type) { w.write_ue(l1_active); }
        }
        let rbsp = w.finish();
        let mut stream = vec![0u8, 0, 0, 1, 0x21]; // slice NAL, nal_ref_idc=1, type=1 (non-IDR)
        stream.extend(rbsp);
        stream
    }

    #[test]
    fn flags_excess_when_override_l0_exceeds_sps() {
        // SPS says 2 ref frames. Slice overrides to 5 (l0_active_minus1=4 → 5 active).
        let stream = build_slice_stream(0 /* P */, 1, 4, 0);
        let sps = fake_sps_num_refs(2);
        let pps_set = fake_pps(0, 0);
        assert_eq!(h264_excess_refs(&stream, &sps, &pps_set), Some(true));
    }

    #[test]
    fn ok_when_override_within_sps_budget() {
        let stream = build_slice_stream(0 /* P */, 1, 0, 0); // 1 active ≤ 2
        let sps = fake_sps_num_refs(2);
        let pps_set = fake_pps(0, 0);
        assert_eq!(h264_excess_refs(&stream, &sps, &pps_set), Some(false));
    }

    #[test]
    fn flags_excess_using_pps_default_when_no_override() {
        let stream = build_slice_stream(0 /* P */, 0, 0, 0);
        let sps = fake_sps_num_refs(2);
        let pps_set = fake_pps(4, 0); // pps default = 5 active
        assert_eq!(h264_excess_refs(&stream, &sps, &pps_set), Some(true));
    }

    #[test]
    fn none_when_no_slice_in_chunk() {
        let stream = vec![0u8, 0, 0, 1, 0x09, 0x10]; // AUD only
        let sps = fake_sps_num_refs(2);
        let pps_set = fake_pps(0, 0);
        assert!(h264_excess_refs(&stream, &sps, &pps_set).is_none());
    }

    #[test]
    fn b_slice_uses_max_of_l0_l1() {
        // l0_active_minus1=0, l1_active_minus1=4 → 5 active.
        let stream = build_slice_stream(1 /* B */, 1, 0, 4);
        let sps = fake_sps_num_refs(2);
        let pps_set = fake_pps(0, 0);
        assert_eq!(h264_excess_refs(&stream, &sps, &pps_set), Some(true));
    }

    /// Phase 0 validation gate: VIP-RAW-like broken segment (SPS num_ref=2,
    /// slice override active_max+1=5) → Some(true). Synthetic but matches
    /// the predicate the real-world IPTV defect triggers.
    #[test]
    fn fixture_vip_raw_like_segment_flags_excess_refs() {
        let stream = build_slice_stream(0 /* P */, 1, 4, 0);
        let sps = fake_sps_num_refs(2);
        let pps_set = fake_pps(0, 0);
        assert_eq!(h264_excess_refs(&stream, &sps, &pps_set), Some(true));
    }

    /// Phase 0 validation gate: VIP-HD-like conformant segment
    /// (slice override active_max+1 ≤ SPS num_ref) → Some(false).
    #[test]
    fn fixture_vip_hd_like_segment_is_clean() {
        let stream = build_slice_stream(0 /* P */, 1, 1, 0);
        let sps = fake_sps_num_refs(2);
        let pps_set = fake_pps(0, 0);
        assert_eq!(h264_excess_refs(&stream, &sps, &pps_set), Some(false));
    }

    /// R2 round-1: multi-SPS chunk. Two SPS are present (id 0 and id 1)
    /// with different `num_ref_frames`; the slice's PPS points at SPS id 1.
    /// The walker must resolve to SPS id 1 (not id 0) when checking the
    /// budget; otherwise a stream that rebases mid-chunk gets classified
    /// against the wrong reference set.
    #[test]
    fn multi_sps_chunk_resolves_via_pps_seq_parameter_set_id() {
        let mut sps_set: HashMap<u64, SpsInfo> = HashMap::new();
        sps_set.insert(0, fake_sps_id_num_refs(0, 16)); // tolerant SPS
        sps_set.insert(1, fake_sps_id_num_refs(1, 2));  // strict SPS
        let mut pps_set: HashMap<u64, PpsInfo> = HashMap::new();
        // pps id 0 → SPS id 1 (the strict one).
        pps_set.insert(0, fake_pps_with_sps(0, 1, 4, 0));
        let stream = build_slice_stream(0 /* P */, 0, 0, 0);
        // Override off → uses PPS default (5 active) → exceeds strict SPS's 2.
        // If the walker mistakenly used the tolerant SPS (16) the result
        // would be Some(false) and this regression would slip through.
        assert_eq!(h264_excess_refs(&stream, &sps_set, &pps_set), Some(true));
    }

    /// Phase 0 boundary test: chunk arrives with a partial NAL at the
    /// start (PES boundary mid-slice). The walker must not panic; it
    /// should report whatever it can decode from the suffix.
    #[test]
    fn boundary_test_partial_leading_nal_does_not_panic() {
        // Build a normal slice, then prepend a "garbage" partial NAL
        // (the kind of leftover the sweep's fetch window could carry
        // when the slice header straddles a PES packet).
        let mut stream: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE];
        stream.extend(build_slice_stream(0, 1, 1, 0));
        let sps = fake_sps_num_refs(2);
        let pps_set = fake_pps(0, 0);
        let result = h264_excess_refs(&stream, &sps, &pps_set);
        // We don't assert the value — the boundary test just confirms
        // we don't panic when junk precedes the start code.
        let _ = result;
    }
}
