// H.264 Picture Parameter Set (PPS) parser, paralleling `sps.rs`.
//
// Phase 0 only needs enough of the PPS to feed the slice-header walker that
// flags variants whose slice headers reference more frames than the SPS
// `num_ref_frames` declares. The full PPS has additional fields (slice group
// map types, entropy_coding_mode_flag, quant offsets, etc.) we don't need.
//
// Inputs:
//   - `parse_h264_pps(rbsp)` takes an emulation-prevention-decoded RBSP byte
//     slice (PPS NAL payload sans the 1-byte NAL header) and returns a
//     `PpsInfo`.
//   - `find_pps_nal(es_bytes)` walks an Annex-B elementary stream, locates
//     each PPS NAL (nal_unit_type 8), strips the emulation-prevention bytes,
//     and returns the decoded RBSP payloads keyed by `pic_parameter_set_id`.

use std::collections::HashMap;

use crate::sps::{nal_unit_ranges, rbsp_unescape_bytes, BitReader};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PpsInfo {
    pub pic_parameter_set_id: u64,
    pub seq_parameter_set_id: u64,
    pub num_ref_idx_l0_default_active_minus1: u64,
    pub num_ref_idx_l1_default_active_minus1: u64,
    pub bottom_field_pic_order_in_frame_present_flag: u8,
    pub redundant_pic_cnt_present_flag: u8,
}

/// Walk the Annex-B elementary stream and return every PPS we find, keyed
/// by `pic_parameter_set_id`. Returns an empty map when no PPS NAL is
/// present in the chunk — same fallback as `find_sps_nal`.
pub fn find_pps_nals(es_bytes: &[u8]) -> HashMap<u64, PpsInfo> {
    let mut out: HashMap<u64, PpsInfo> = HashMap::new();
    for (start, end) in nal_unit_ranges(es_bytes) {
        if end <= start {
            continue;
        }
        let nal_type = es_bytes[start] & 0x1F;
        if nal_type != 8 {
            continue;
        }
        // Strip the 1-byte H.264 NAL header before RBSP decode.
        if start + 1 > end {
            continue;
        }
        let rbsp = rbsp_unescape_bytes(&es_bytes[start + 1..end]);
        if let Some(pps) = parse_h264_pps(&rbsp) {
            out.insert(pps.pic_parameter_set_id, pps);
        }
    }
    out
}

pub fn parse_h264_pps(rbsp: &[u8]) -> Option<PpsInfo> {
    let mut r = BitReader::new(rbsp);
    let pic_parameter_set_id = r.read_ue()?;
    let seq_parameter_set_id = r.read_ue()?;
    let _entropy_coding_mode_flag = r.read_bit()?;
    let bottom_field_pic_order_in_frame_present_flag = r.read_bit()?;
    let num_slice_groups_minus1 = r.read_ue()?;
    if num_slice_groups_minus1 > 0 {
        let slice_group_map_type = r.read_ue()?;
        match slice_group_map_type {
            0 => {
                for _ in 0..=num_slice_groups_minus1 {
                    let _run_length_minus1 = r.read_ue()?;
                }
            }
            2 => {
                for _ in 0..num_slice_groups_minus1 {
                    let _top_left = r.read_ue()?;
                    let _bottom_right = r.read_ue()?;
                }
            }
            3 | 4 | 5 => {
                let _slice_group_change_direction_flag = r.read_bit()?;
                let _slice_group_change_rate_minus1 = r.read_ue()?;
            }
            6 => {
                let pic_size_in_map_units_minus1 = r.read_ue()?;
                let bits_for_id = bits_needed_for(num_slice_groups_minus1);
                for _ in 0..=pic_size_in_map_units_minus1 {
                    let _id = r.read_bits(bits_for_id as usize)?;
                }
            }
            _ => {}
        }
    }
    let num_ref_idx_l0_default_active_minus1 = r.read_ue()?;
    let num_ref_idx_l1_default_active_minus1 = r.read_ue()?;
    let _weighted_pred_flag = r.read_bit()?;
    let _weighted_bipred_idc = r.read_bits(2)?;
    let _pic_init_qp_minus26 = r.read_se()?;
    let _pic_init_qs_minus26 = r.read_se()?;
    let _chroma_qp_index_offset = r.read_se()?;
    let _deblocking_filter_control_present_flag = r.read_bit()?;
    let _constrained_intra_pred_flag = r.read_bit()?;
    let redundant_pic_cnt_present_flag = r.read_bit()?;

    Some(PpsInfo {
        pic_parameter_set_id,
        seq_parameter_set_id,
        num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1,
        bottom_field_pic_order_in_frame_present_flag,
        redundant_pic_cnt_present_flag,
    })
}

fn bits_needed_for(value: u64) -> u32 {
    // ceil(log2(value + 1)). For value=0, return 1 (PPS clause 7.3.2.2
    // assumes at least one bit per ID).
    if value == 0 {
        return 1;
    }
    64 - value.leading_zeros()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tiny bit-writer copied from `sps.rs::tests::BitWriter`. Local copy avoids
    // a crate-internal `pub(crate)` leak just for tests.
    struct BitWriter {
        bytes: Vec<u8>,
        bit_pos: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self { bytes: Vec::new(), bit_pos: 0 }
        }
        fn write_bit(&mut self, b: u8) {
            if self.bit_pos.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let byte = self.bit_pos / 8;
            let bit = 7 - (self.bit_pos % 8);
            self.bytes[byte] |= (b & 1) << bit;
            self.bit_pos += 1;
        }
        fn write_bits(&mut self, v: u64, n: usize) {
            for i in (0..n).rev() {
                self.write_bit(((v >> i) & 1) as u8);
            }
        }
        fn write_ue(&mut self, v: u64) {
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
        fn write_se(&mut self, v: i64) {
            let k = if v <= 0 { (-2 * v) as u64 } else { (2 * v - 1) as u64 };
            self.write_ue(k);
        }
        fn finish(mut self) -> Vec<u8> {
            self.write_bit(1);
            while !self.bit_pos.is_multiple_of(8) {
                self.write_bit(0);
            }
            self.bytes
        }
    }

    fn build_pps(pps_id: u64, sps_id: u64, l0: u64, l1: u64) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_ue(pps_id);
        w.write_ue(sps_id);
        w.write_bit(0); // entropy_coding_mode_flag
        w.write_bit(1); // bottom_field_pic_order_in_frame_present_flag
        w.write_ue(0); // num_slice_groups_minus1 = 0
        w.write_ue(l0); // num_ref_idx_l0_default_active_minus1
        w.write_ue(l1); // num_ref_idx_l1_default_active_minus1
        w.write_bit(0); // weighted_pred_flag
        w.write_bits(0, 2); // weighted_bipred_idc
        w.write_se(0); // pic_init_qp_minus26
        w.write_se(0); // pic_init_qs_minus26
        w.write_se(0); // chroma_qp_index_offset
        w.write_bit(1); // deblocking_filter_control_present_flag
        w.write_bit(0); // constrained_intra_pred_flag
        w.write_bit(0); // redundant_pic_cnt_present_flag
        w.finish()
    }

    #[test]
    fn parse_pps_extracts_default_active_minus1() {
        let rbsp = build_pps(0, 0, 1, 0);
        let pps = parse_h264_pps(&rbsp).unwrap();
        assert_eq!(pps.pic_parameter_set_id, 0);
        assert_eq!(pps.seq_parameter_set_id, 0);
        assert_eq!(pps.num_ref_idx_l0_default_active_minus1, 1);
        assert_eq!(pps.num_ref_idx_l1_default_active_minus1, 0);
        assert_eq!(pps.bottom_field_pic_order_in_frame_present_flag, 1);
        assert_eq!(pps.redundant_pic_cnt_present_flag, 0);
    }

    #[test]
    fn find_pps_nals_extracts_pps_from_annex_b_stream() {
        let rbsp = build_pps(0, 0, 3, 2);
        let mut stream = vec![0u8, 0, 0, 1, 0x68];
        // Reuse the same escape-on-double-zero trick the sps fixture uses.
        for &b in &rbsp {
            if b == 0 && stream.last() == Some(&0) && stream.iter().rev().nth(1) == Some(&0) {
                stream.push(0x03);
            }
            stream.push(b);
        }
        let map = find_pps_nals(&stream);
        let pps = map.get(&0).unwrap();
        assert_eq!(pps.num_ref_idx_l0_default_active_minus1, 3);
        assert_eq!(pps.num_ref_idx_l1_default_active_minus1, 2);
    }
}
