//! Slice segment headers (H.265 7.3.6): how a slice's data is to be decoded.

use crate::bits::{Bits, Syntax, rbsp};
use crate::nal::NalHeader;
use crate::params::{Deblocking, ParamSets, Pps, ShortTermRps, Sps, short_term_ref_pic_set};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceType {
    B,
    P,
    I,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SliceHeader {
    pub nal: NalHeader,
    pub first_in_pic: bool,
    pub no_output_of_prior_pics: bool,
    pub pps_id: u8,
    /// A dependent segment carries on the slice before it, whose settings it shares.
    pub dependent: bool,
    /// The first CTB, in raster order within the picture (tile scan order when there are tiles).
    pub address: u32,
    pub kind: SliceType,
    pub pic_output: bool,
    pub colour_plane: u8,
    pub poc_lsb: u32,
    /// `None` for IDR pictures, which refer to nothing.
    pub short_term_rps: Option<ShortTermRps>,
    pub temporal_mvp: bool,
    pub sao_luma: bool,
    pub sao_chroma: bool,
    /// Active references in lists 0 and 1.
    pub num_ref_idx: [u8; 2],
    pub cabac_init: bool,
    pub max_num_merge_cand: u8,
    /// SliceQpY: the QP the slice starts with.
    pub qp: i32,
    pub cb_qp_offset: i8,
    pub cr_qp_offset: i8,
    pub cu_chroma_qp_offset: bool,
    pub deblocking: Deblocking,
    pub loop_filter_across_slices: bool,
    /// Where each substream after the first starts (tiles or wavefront rows), in bytes from
    /// the start of the slice data, counting emulation prevention bytes as the NAL unit has them.
    pub entry_points: Vec<u64>,
    /// Where the slice data starts, in bytes into the NAL unit's RBSP (emulation prevention removed).
    pub data_offset: usize,
}

impl SliceHeader {
    /// Parse a slice NAL unit's header, as it's stored (with emulation prevention bytes).
    /// `previous` is the slice segment before it in the picture, which a dependent segment needs.
    pub fn parse(
        nal: &[u8],
        params: &ParamSets,
        previous: Option<&SliceHeader>,
    ) -> Result<SliceHeader> {
        let data = rbsp(nal);
        SliceHeader::read(&mut Bits::new(&data, false), params, previous)
    }

    /// [`SliceHeader::parse`], also giving every syntax element read.
    pub fn trace(
        nal: &[u8],
        params: &ParamSets,
        previous: Option<&SliceHeader>,
    ) -> (Result<SliceHeader>, Vec<Syntax>) {
        let data = rbsp(nal);
        let mut b = Bits::new(&data, true);
        let result = SliceHeader::read(&mut b, params, previous);
        (result, b.into_trace())
    }

    fn read(
        b: &mut Bits,
        params: &ParamSets,
        previous: Option<&SliceHeader>,
    ) -> Result<SliceHeader> {
        let nal = NalHeader::read(b)?;
        if !nal.is_slice() {
            return Err(Error::corrupt(format!(
                "NAL unit type {} isn't a slice",
                nal.kind
            )));
        }
        if nal.layer != 0 {
            return Err(Error::unsupported("layers other than the base layer"));
        }
        let first_in_pic = b.flag("first_slice_segment_in_pic_flag")?;
        let no_output_of_prior_pics = nal.is_irap() && b.flag("no_output_of_prior_pics_flag")?;
        let pps_id = b.ue("slice_pic_parameter_set_id", 63)?;
        let (pps, sps) = params.get(pps_id)?;
        let (w, h) = sps.ctbs();
        let ctbs = w * h;
        let (mut dependent, mut address) = (false, 0);
        if !first_in_pic {
            if pps.dependent_slice_segments {
                dependent = b.flag("dependent_slice_segment_flag")?;
            }
            address = b.u(ceil_log2(ctbs), "slice_segment_address")?;
            if address == 0 || address >= ctbs {
                return Err(Error::corrupt(format!(
                    "a slice starts at CTB {address} of {ctbs}"
                )));
            }
        }
        let mut header = if dependent {
            let Some(prev) = previous else {
                return Err(Error::corrupt(
                    "a dependent slice segment has no slice before it",
                ));
            };
            prev.clone()
        } else {
            independent(b, nal, sps, pps)?
        };
        header.nal = nal;
        header.first_in_pic = first_in_pic;
        header.no_output_of_prior_pics = no_output_of_prior_pics;
        header.pps_id = pps_id as u8;
        header.dependent = dependent;
        header.address = address;
        header.entry_points.clear();
        if pps.tiles.is_some() || pps.entropy_coding_sync {
            let n = b.ue("num_entry_point_offsets", ctbs - 1)?;
            if n > 0 {
                let len = b.ue("offset_len_minus1", 31)? + 1;
                let mut at = 0;
                for i in 0..n {
                    at += u64::from(b.u(len, &format!("entry_point_offset_minus1[{i}]"))?) + 1;
                    header.entry_points.push(at);
                }
            }
        }
        if pps.slice_header_extension_present {
            for i in 0..b.ue("slice_segment_header_extension_length", 256)? {
                b.u(8, &format!("slice_segment_header_extension_data_byte[{i}]"))?;
            }
        }
        if !b.flag("alignment_bit_equal_to_one")? {
            return Err(Error::corrupt("a slice header doesn't end where it should"));
        }
        while !b.pos().is_multiple_of(8) {
            if b.flag("alignment_bit_equal_to_zero")? {
                return Err(Error::corrupt("a slice header doesn't end where it should"));
            }
        }
        header.data_offset = b.pos() / 8;
        Ok(header)
    }
}

/// The part of the header a dependent slice segment takes from the slice before it.
fn independent(b: &mut Bits, nal: NalHeader, sps: &Sps, pps: &Pps) -> Result<SliceHeader> {
    for i in 0..pps.num_extra_slice_header_bits {
        b.flag(&format!("slice_reserved_flag[{i}]"))?;
    }
    let kind = match b.ue("slice_type", 2)? {
        0 => SliceType::B,
        1 => SliceType::P,
        _ => SliceType::I,
    };
    let pic_output = !pps.output_flag_present || b.flag("pic_output_flag")?;
    let colour_plane = if sps.separate_colour_planes {
        b.u(2, "colour_plane_id")? as u8
    } else {
        0
    };
    if colour_plane > 2 {
        return Err(Error::corrupt("a slice is for colour plane 3"));
    }
    let chroma = !sps.separate_colour_planes && sps.chroma_format != 0;
    let (mut poc_lsb, mut short_term_rps, mut long_term_used, mut temporal_mvp) =
        (0, None, 0, false);
    // IDR pictures have no picture order count or references.
    if !matches!(nal.kind, 19 | 20) {
        poc_lsb = b.u(u32::from(sps.log2_max_poc_lsb), "slice_pic_order_cnt_lsb")?;
        let sets = &sps.short_term_rps;
        let set = if !b.flag("short_term_ref_pic_set_sps_flag")? {
            short_term_ref_pic_set(b, sets.len(), sets, sets.len())?
        } else {
            let idx = if sets.len() > 1 {
                b.u(ceil_log2(sets.len() as u32), "short_term_ref_pic_set_idx")? as usize
            } else {
                0
            };
            sets.get(idx).cloned().ok_or_else(|| {
                Error::corrupt("a slice uses a reference picture set the SPS doesn't have")
            })?
        };
        short_term_rps = Some(set);
        if let Some(listed) = &sps.long_term {
            let from_sps = if listed.is_empty() {
                0
            } else {
                b.ue("num_long_term_sps", listed.len() as u32)?
            };
            let own = b.ue("num_long_term_pics", 32 - from_sps)?;
            for i in 0..from_sps + own {
                let used = if i < from_sps {
                    let idx = if listed.len() > 1 {
                        b.u(ceil_log2(listed.len() as u32), &format!("lt_idx_sps[{i}]"))?
                    } else {
                        0
                    };
                    listed
                        .get(idx as usize)
                        .ok_or_else(|| {
                            Error::corrupt("a slice uses a long-term picture the SPS doesn't list")
                        })?
                        .1
                } else {
                    b.u(u32::from(sps.log2_max_poc_lsb), &format!("poc_lsb_lt[{i}]"))?;
                    b.flag(&format!("used_by_curr_pic_lt_flag[{i}]"))?
                };
                long_term_used += u32::from(used);
                if b.flag(&format!("delta_poc_msb_present_flag[{i}]"))? {
                    b.ue(&format!("delta_poc_msb_cycle_lt[{i}]"), u32::MAX - 1)?;
                }
            }
        }
        if sps.temporal_mvp {
            temporal_mvp = b.flag("slice_temporal_mvp_enabled_flag")?;
        }
    }
    let (mut sao_luma, mut sao_chroma) = (false, false);
    if sps.sao {
        sao_luma = b.flag("slice_sao_luma_flag")?;
        if chroma {
            sao_chroma = b.flag("slice_sao_chroma_flag")?;
        }
    }
    let (mut num_ref_idx, mut cabac_init, mut max_num_merge_cand) = ([0, 0], false, 5);
    if kind != SliceType::I {
        let lists = if kind == SliceType::B { 2 } else { 1 };
        num_ref_idx = pps.num_ref_idx_default;
        if b.flag("num_ref_idx_active_override_flag")? {
            for (l, n) in num_ref_idx.iter_mut().enumerate().take(lists) {
                *n = b.ue(&format!("num_ref_idx_l{l}_active_minus1"), 14)? as u8 + 1;
            }
        }
        if kind == SliceType::P {
            num_ref_idx[1] = 0;
        }
        let used = short_term_rps.as_ref().map_or(0, |s: &ShortTermRps| {
            s.negative.iter().chain(&s.positive).filter(|p| p.1).count() as u32
        });
        let total = used + long_term_used;
        if pps.lists_modification_present && total > 1 {
            for (l, &n) in num_ref_idx.iter().enumerate().take(lists) {
                if b.flag(&format!("ref_pic_list_modification_flag_l{l}"))? {
                    for i in 0..n {
                        if b.u(ceil_log2(total), &format!("list_entry_l{l}[{i}]"))? >= total {
                            return Err(Error::corrupt(
                                "a reference list names a picture that isn't there",
                            ));
                        }
                    }
                }
            }
        }
        if kind == SliceType::B {
            b.flag("mvd_l1_zero_flag")?;
        }
        if pps.cabac_init_present {
            cabac_init = b.flag("cabac_init_flag")?;
        }
        if temporal_mvp {
            let from_l0 = kind != SliceType::B || b.flag("collocated_from_l0_flag")?;
            let n = num_ref_idx[if from_l0 { 0 } else { 1 }];
            if n > 1 {
                b.ue("collocated_ref_idx", u32::from(n) - 1)?;
            }
        }
        if (pps.weighted_pred && kind == SliceType::P)
            || (pps.weighted_bipred && kind == SliceType::B)
        {
            pred_weight_table(b, sps, chroma, &num_ref_idx[..lists])?;
        }
        max_num_merge_cand = 5 - b.ue("five_minus_max_num_merge_cand", 4)? as u8;
    }
    let qp_bd_offset = 6 * (i32::from(sps.bit_depth_luma) - 8);
    let qp = pps.init_qp + b.se("slice_qp_delta", -(52 + qp_bd_offset), 52 + qp_bd_offset)?;
    if qp < -qp_bd_offset || qp > 51 {
        return Err(Error::corrupt(format!("a slice's QP is {qp}")));
    }
    let (mut cb_qp_offset, mut cr_qp_offset) = (0, 0);
    if pps.slice_chroma_qp_offsets_present {
        cb_qp_offset = b.se("slice_cb_qp_offset", -12, 12)? as i8;
        cr_qp_offset = b.se("slice_cr_qp_offset", -12, 12)? as i8;
        if !(-12..=12).contains(&(pps.cb_qp_offset + cb_qp_offset))
            || !(-12..=12).contains(&(pps.cr_qp_offset + cr_qp_offset))
        {
            return Err(Error::corrupt(
                "a slice's chroma QP offsets are out of range",
            ));
        }
    }
    let cu_chroma_qp_offset =
        pps.range.chroma_qp_offset_list.is_some() && b.flag("cu_chroma_qp_offset_enabled_flag")?;
    let mut deblocking = pps.deblocking;
    if pps.deblocking.override_enabled && b.flag("deblocking_filter_override_flag")? {
        deblocking.disabled = b.flag("slice_deblocking_filter_disabled_flag")?;
        if !deblocking.disabled {
            deblocking.beta_offset_div2 = b.se("slice_beta_offset_div2", -6, 6)? as i8;
            deblocking.tc_offset_div2 = b.se("slice_tc_offset_div2", -6, 6)? as i8;
        }
    }
    let loop_filter_across_slices =
        if pps.loop_filter_across_slices && (sao_luma || sao_chroma || !deblocking.disabled) {
            b.flag("slice_loop_filter_across_slices_enabled_flag")?
        } else {
            pps.loop_filter_across_slices
        };
    Ok(SliceHeader {
        nal,
        first_in_pic: false,
        no_output_of_prior_pics: false,
        pps_id: 0,
        dependent: false,
        address: 0,
        kind,
        pic_output,
        colour_plane,
        poc_lsb,
        short_term_rps,
        temporal_mvp,
        sao_luma,
        sao_chroma,
        num_ref_idx,
        cabac_init,
        max_num_merge_cand,
        qp,
        cb_qp_offset,
        cr_qp_offset,
        cu_chroma_qp_offset,
        deblocking,
        loop_filter_across_slices,
        entry_points: Vec::new(),
        data_offset: 0,
    })
}

/// pred_weight_table (7.3.6.3), read only to get past it: weighted prediction is for P and B
/// pictures, which only keyframes' neighbours are.
fn pred_weight_table(b: &mut Bits, sps: &Sps, chroma: bool, num_ref_idx: &[u8]) -> Result<()> {
    let luma_denom = b.ue("luma_log2_weight_denom", 7)? as i32;
    if chroma && !(0..=7).contains(&(luma_denom + b.se("delta_chroma_log2_weight_denom", -7, 7)?)) {
        return Err(Error::corrupt(
            "a chroma weight denominator is out of range",
        ));
    }
    let half = |depth: u8| {
        if sps.range.high_precision_offsets {
            1 << (depth - 1)
        } else {
            128
        }
    };
    let (luma_half, chroma_half) = (half(sps.bit_depth_luma), half(sps.bit_depth_chroma));
    for (l, &n) in num_ref_idx.iter().enumerate() {
        let luma: Vec<bool> = (0..n)
            .map(|i| b.flag(&format!("luma_weight_l{l}_flag[{i}]")))
            .collect::<Result<_>>()?;
        let chroma: Vec<bool> = (0..n)
            .map(|i| {
                if chroma {
                    b.flag(&format!("chroma_weight_l{l}_flag[{i}]"))
                } else {
                    Ok(false)
                }
            })
            .collect::<Result<_>>()?;
        for i in 0..n as usize {
            if luma[i] {
                b.se(&format!("delta_luma_weight_l{l}[{i}]"), -128, 127)?;
                b.se(&format!("luma_offset_l{l}[{i}]"), -luma_half, luma_half - 1)?;
            }
            if chroma[i] {
                for j in 0..2 {
                    b.se(&format!("delta_chroma_weight_l{l}[{i}][{j}]"), -128, 127)?;
                    // The standard's delta_chroma_offset, named as ffmpeg traces it.
                    let name = format!("chroma_offset_l{l}[{i}][{j}]");
                    b.se(&name, -4 * chroma_half, 4 * chroma_half - 1)?;
                }
            }
        }
    }
    Ok(())
}

/// Ceil(Log2(n)): the bits needed to number `n` things.
fn ceil_log2(n: u32) -> u32 {
    32 - n.saturating_sub(1).leading_zeros()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The parameter sets and the start of a keyframe (a CRA picture) from a real film.
    const SPS: [u8; 41] = [
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x78, 0xA0, 0x03, 0xC0, 0x80, 0x10, 0xE5, 0x96, 0x56, 0x69, 0x24, 0xCA, 0xF0,
        0x10, 0x10, 0x00, 0x00, 0x3E, 0x90, 0x00, 0x07, 0x53, 0x00, 0x80,
    ];
    const PPS: [u8; 7] = [0x44, 0x01, 0xC1, 0x72, 0xB4, 0x62, 0x40];

    fn params() -> ParamSets {
        let mut p = ParamSets::new();
        p.add(&SPS).unwrap();
        p.add(&PPS).unwrap();
        p
    }

    /// Write fields MSB first.
    #[derive(Default)]
    struct Writer {
        bits: Vec<bool>,
    }

    impl Writer {
        fn u(&mut self, n: u32, v: u32) -> &mut Self {
            self.bits.extend((0..n).rev().map(|i| v >> i & 1 == 1));
            self
        }
        fn ue(&mut self, v: u32) -> &mut Self {
            let n = 32 - (v + 1).leading_zeros();
            self.u(n - 1, 0).u(n, v + 1)
        }
        fn se(&mut self, v: i32) -> &mut Self {
            self.ue(if v > 0 {
                2 * v as u32 - 1
            } else {
                (-2 * v) as u32
            })
        }
        /// Byte alignment, then some slice data.
        fn finish(&mut self) -> Vec<u8> {
            self.u(1, 1);
            while !self.bits.len().is_multiple_of(8) {
                self.bits.push(false);
            }
            let mut out: Vec<u8> = self
                .bits
                .chunks(8)
                .map(|c| c.iter().fold(0, |b, &x| b << 1 | x as u8))
                .collect();
            out.extend([0xAB, 0xCD]);
            out
        }
    }

    #[test]
    fn reads_a_keyframes_header() {
        // CRA, first in the picture, PPS 0, I slice, POC LSB 255, its own reference set of
        // four earlier pictures (deltas 3, 2, 2, 2), none used; then SAO, QP delta +2, and 16
        // wavefront entry points of 14 bits.
        let mut w = Writer::default();
        w.u(16, 0x2A01)
            .u(1, 1)
            .u(1, 0)
            .ue(0)
            .ue(2)
            .u(8, 255)
            .u(1, 0)
            .ue(4)
            .ue(0);
        for d in [2, 1, 1, 1] {
            w.ue(d).u(1, 0);
        }
        w.u(1, 1).u(1, 1).u(1, 1).se(2).u(1, 1).ue(16).ue(13);
        for i in 0..16 {
            w.u(14, 1000 + i);
        }
        let nal = w.finish();
        let h = SliceHeader::parse(&nal, &params(), None).unwrap();
        assert_eq!(
            (h.nal.kind, h.first_in_pic, h.kind, h.poc_lsb, h.qp),
            (21, true, SliceType::I, 255, 28)
        );
        let rps = h.short_term_rps.as_ref().unwrap();
        assert_eq!(
            rps.negative,
            [(-3, false), (-5, false), (-7, false), (-9, false)]
        );
        assert!(h.temporal_mvp && h.sao_luma && h.sao_chroma && h.loop_filter_across_slices);
        assert_eq!(h.entry_points.len(), 16);
        assert_eq!(h.entry_points[..2], [1001, 2003]);
        assert_eq!(&nal[h.data_offset..], [0xAB, 0xCD]);
    }

    #[test]
    fn idr_pictures_have_no_references() {
        let mut w = Writer::default();
        w.u(16, 0x2601)
            .u(1, 1)
            .u(1, 0)
            .ue(0)
            .ue(2)
            .u(1, 0)
            .u(1, 0)
            .se(-3)
            .u(1, 0)
            .ue(0);
        let h = SliceHeader::parse(&w.finish(), &params(), None).unwrap();
        assert_eq!(
            (
                h.nal.kind,
                h.short_term_rps,
                h.qp,
                h.sao_luma,
                h.entry_points.len()
            ),
            (19, None, 23, false, 0)
        );
    }

    #[test]
    fn dependent_and_later_slices() {
        // The PPS above with dependent slice segments turned on.
        let mut params = params();
        let mut w = Writer::default();
        w.u(16, 0x4401)
            .ue(1)
            .ue(0)
            .u(1, 1)
            .u(1, 0)
            .u(3, 0)
            .u(1, 1)
            .u(1, 0)
            .ue(0)
            .ue(0)
            .se(0);
        w.u(1, 0)
            .u(1, 0)
            .u(1, 0)
            .se(0)
            .se(0)
            .u(1, 0)
            .u(1, 0)
            .u(1, 0)
            .u(1, 0)
            .u(1, 0)
            .u(1, 0);
        w.u(1, 1).u(1, 0).u(1, 0).u(1, 0).ue(0).u(1, 0).u(1, 0);
        let mut pps = w.finish();
        pps.truncate(pps.len() - 2);
        params.add(&pps).unwrap();
        // The first slice: an IDR's I slice.
        let mut w = Writer::default();
        w.u(16, 0x2801)
            .u(1, 1)
            .u(1, 0)
            .ue(1)
            .ue(2)
            .u(1, 0)
            .u(1, 0)
            .se(5)
            .u(1, 0);
        let first = SliceHeader::parse(&w.finish(), &params, None).unwrap();
        // A dependent segment from CTB 30 (the address takes 9 bits for 510 CTBs).
        let mut w = Writer::default();
        w.u(16, 0x2801).u(1, 0).u(1, 0).ue(1).u(1, 1).u(9, 30);
        let dep = SliceHeader::parse(&w.finish(), &params, Some(&first)).unwrap();
        assert_eq!(
            (dep.dependent, dep.address, dep.qp, dep.kind),
            (true, 30, first.qp, SliceType::I)
        );
        assert!(
            SliceHeader::parse(&w.finish(), &params, None).is_err(),
            "no slice before it"
        );
        // Address 0 or past the end isn't allowed after the first slice.
        for address in [0, 510] {
            let mut w = Writer::default();
            w.u(16, 0x2801).u(1, 0).u(1, 0).ue(1).u(1, 1).u(9, address);
            assert!(
                SliceHeader::parse(&w.finish(), &params, Some(&first)).is_err(),
                "address {address}"
            );
        }
        // An unknown PPS.
        let mut w = Writer::default();
        w.u(16, 0x2801).u(1, 1).u(1, 0).ue(9).ue(2);
        assert!(SliceHeader::parse(&w.finish(), &params, None).is_err());
    }

    #[test]
    fn ceil_log2_counts_bits() {
        assert_eq!(
            [1, 2, 3, 4, 5, 510, 512, 513].map(ceil_log2),
            [0, 1, 2, 2, 3, 9, 9, 10]
        );
    }
}
