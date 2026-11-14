//! Parameter sets (H.265 7.3.2): the sequence's coding settings (SPS) and the picture's (PPS).
//! The video parameter set (VPS) isn't needed to decode the base layer, so it isn't read.

use crate::bits::{Bits, Syntax, rbsp};
use crate::nal::{self, NalHeader};
use crate::{Error, Result};

/// The largest picture side the standard allows (level 6.2).
const MAX_SIDE: u32 = 16888;
/// MaxLumaPs at level 6.2 (Table A.8).
const MAX_LUMA_SAMPLES: u64 = 35_651_584;

/// The most pictures a reference picture set may name.
const MAX_REFS: usize = 16;

/// A sequence parameter set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sps {
	pub id: u8,
	pub max_sub_layers: u8,
	pub profile_idc: u8,
	pub level_idc: u8,
	/// 0 monochrome, 1 4:2:0, 2 4:2:2, 3 4:4:4.
	pub chroma_format: u8,
	/// 4:4:4 coded as three monochrome pictures.
	pub separate_colour_planes: bool,
	/// The coded picture size in luma samples.
	pub width: u32,
	pub height: u32,
	/// Luma samples to crop from the left, right, top and bottom for display.
	pub crop: [u32; 4],
	pub bit_depth_luma: u8,
	pub bit_depth_chroma: u8,
	pub log2_max_poc_lsb: u8,
	/// For the highest sub-layer.
	pub max_dec_pic_buffering: u8,
	pub max_num_reorder: u8,
	pub log2_min_cb: u8,
	pub log2_ctb: u8,
	pub log2_min_tb: u8,
	pub log2_max_tb: u8,
	pub max_transform_depth_inter: u8,
	pub max_transform_depth_intra: u8,
	/// `None`: scaling lists are off, so every coefficient is scaled by 16.
	pub scaling: Option<ScalingLists>,
	pub amp: bool,
	pub sao: bool,
	pub pcm: Option<Pcm>,
	pub short_term_rps: Vec<ShortTermRps>,
	/// `None`: no long-term reference pictures. Otherwise the ones the SPS lists, as
	/// (POC LSBs, used by the current picture).
	pub long_term: Option<Vec<(u32, bool)>>,
	pub temporal_mvp: bool,
	pub strong_intra_smoothing: bool,
	pub colour: Colour,
	pub range: SpsRange,
}

/// How samples map to colours (from the VUI), with the standard's "unspecified" defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colour {
	/// Samples use the full range (0 to 255 at 8 bits) rather than 16 to 235.
	pub full_range: bool,
	pub primaries: u8,
	pub transfer: u8,
	/// The YUV to RGB matrix: 1 BT.709, 5 or 6 BT.601, 9 BT.2020; 2 unspecified.
	pub matrix: u8,
}

impl Default for Colour {
	fn default() -> Self {
		Colour { full_range: false, primaries: 2, transfer: 2, matrix: 2 }
	}
}

/// PCM: blocks stored as raw samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pcm {
	pub bit_depth_luma: u8,
	pub bit_depth_chroma: u8,
	pub log2_min_size: u8,
	pub log2_max_size: u8,
	pub loop_filter_disabled: bool,
}

/// The range extension's coding tools (7.3.2.2.2), all off unless the SPS turns them on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpsRange {
	pub transform_skip_rotation: bool,
	pub transform_skip_context: bool,
	pub implicit_rdpcm: bool,
	pub explicit_rdpcm: bool,
	pub extended_precision: bool,
	pub intra_smoothing_disabled: bool,
	pub high_precision_offsets: bool,
	pub persistent_rice_adaptation: bool,
	pub cabac_bypass_alignment: bool,
}

/// Scaling lists (7.3.4), in the order they're coded (up-right diagonal), for each size (4x4,
/// 8x8, 16x16, 32x32) and matrix (intra Y, Cb, Cr, then inter Y, Cb, Cr). 4x4 lists use only
/// their first 16 entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScalingLists {
	pub lists: [[[u8; 64]; 6]; 4],
	/// The DC coefficient's factor, for 16x16 and 32x32 (`dc[0]` and `dc[1]`).
	pub dc: [[u8; 6]; 2],
}

/// A short-term reference picture set (7.3.7): the pictures before (`negative`) and after
/// (`positive`) the current one, each as (POC difference, used by the current picture).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShortTermRps {
	pub negative: Vec<(i32, bool)>,
	pub positive: Vec<(i32, bool)>,
}

/// A picture parameter set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pps {
	pub id: u8,
	pub sps_id: u8,
	pub dependent_slice_segments: bool,
	pub output_flag_present: bool,
	pub num_extra_slice_header_bits: u8,
	pub sign_data_hiding: bool,
	pub cabac_init_present: bool,
	/// The default number of active references in list 0 and list 1.
	pub num_ref_idx_default: [u8; 2],
	pub init_qp: i32,
	pub constrained_intra_pred: bool,
	pub transform_skip: bool,
	/// `None`: no QP changes within a slice. Otherwise how many levels below the CTB a
	/// quantization group is.
	pub cu_qp_delta_depth: Option<u8>,
	pub cb_qp_offset: i8,
	pub cr_qp_offset: i8,
	pub slice_chroma_qp_offsets_present: bool,
	pub weighted_pred: bool,
	pub weighted_bipred: bool,
	pub transquant_bypass: bool,
	pub tiles: Option<Tiles>,
	/// Wavefront parallel processing: each CTB row starts from the contexts two CTBs into the row above.
	pub entropy_coding_sync: bool,
	pub loop_filter_across_slices: bool,
	pub deblocking: Deblocking,
	/// Lists replacing the SPS's, if any.
	pub scaling: Option<ScalingLists>,
	pub lists_modification_present: bool,
	pub log2_parallel_merge_level: u8,
	pub slice_header_extension_present: bool,
	pub range: PpsRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tiles {
	pub columns: u32,
	pub rows: u32,
	/// `None`: spaced evenly. Otherwise the width (or height) in CTBs of each but the last.
	pub column_widths: Option<Vec<u32>>,
	pub row_heights: Option<Vec<u32>>,
	pub loop_filter_across_tiles: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Deblocking {
	pub override_enabled: bool,
	pub disabled: bool,
	pub beta_offset_div2: i8,
	pub tc_offset_div2: i8,
}

/// The range extension's picture settings (7.3.2.3.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PpsRange {
	pub log2_max_transform_skip_size: u8,
	pub cross_component_prediction: bool,
	/// (depth, the Cb and Cr offsets) when chroma QP offset lists are on.
	pub chroma_qp_offset_list: Option<(u8, Vec<(i8, i8)>)>,
	pub log2_sao_offset_scale_luma: u8,
	pub log2_sao_offset_scale_chroma: u8,
}

impl Default for PpsRange {
	fn default() -> Self {
		PpsRange {
			log2_max_transform_skip_size: 2,
			cross_component_prediction: false,
			chroma_qp_offset_list: None,
			log2_sao_offset_scale_luma: 0,
			log2_sao_offset_scale_chroma: 0,
		}
	}
}

/// Parse a NAL unit with tracing, for checking against other decoders.
fn traced<T>(nal: &[u8], kind: u8, read: impl FnOnce(&mut Bits) -> Result<T>) -> (Result<T>, Vec<Syntax>) {
	let data = rbsp(nal);
	let mut b = Bits::new(&data, true);
	let result = header(&mut b, kind).and_then(|()| read(&mut b));
	(result, b.into_trace())
}

fn header(b: &mut Bits, kind: u8) -> Result<()> {
	let h = NalHeader::read(b)?;
	if h.kind != kind {
		return Err(Error::corrupt(format!("expected NAL unit type {kind}, not {}", h.kind)));
	}
	if h.layer != 0 {
		return Err(Error::unsupported("layers other than the base layer"));
	}
	Ok(())
}

impl Sps {
	/// Parse an SPS NAL unit, as it's stored (with emulation prevention bytes).
	pub fn parse(nal: &[u8]) -> Result<Sps> {
		let data = rbsp(nal);
		let mut b = Bits::new(&data, false);
		header(&mut b, nal::SPS)?;
		Sps::read(&mut b)
	}

	/// [`Sps::parse`], also giving every syntax element read.
	pub fn trace(nal: &[u8]) -> (Result<Sps>, Vec<Syntax>) {
		traced(nal, nal::SPS, Sps::read)
	}

	fn read(b: &mut Bits) -> Result<Sps> {
		b.u(4, "sps_video_parameter_set_id")?;
		let max_sub_layers = b.u(3, "sps_max_sub_layers_minus1")? as u8 + 1;
		if max_sub_layers > 7 {
			return Err(Error::corrupt("an SPS has 8 sub-layers"));
		}
		b.flag("sps_temporal_id_nesting_flag")?;
		let (profile_idc, level_idc) = profile_tier_level(b, max_sub_layers)?;
		let id = b.ue("sps_seq_parameter_set_id", 15)? as u8;
		let chroma_format = b.ue("chroma_format_idc", 3)? as u8;
		let separate_colour_planes = chroma_format == 3 && b.flag("separate_colour_plane_flag")?;
		let width = b.ue("pic_width_in_luma_samples", MAX_SIDE)?;
		let height = b.ue("pic_height_in_luma_samples", MAX_SIDE)?;
		// The largest picture any level allows (8192x4320 at level 6.2), so a damaged SPS can't
		// ask for a gigabyte.
		if u64::from(width) * u64::from(height) > MAX_LUMA_SAMPLES {
			return Err(Error::unsupported(format!("a {width}x{height} picture (larger than any HEVC level allows)")));
		}
		let (sub_w, sub_h) = match (chroma_format, separate_colour_planes) {
			(1, _) => (2, 2),
			(2, _) => (2, 1),
			_ => (1, 1),
		};
		let mut crop = [0; 4];
		if b.flag("conformance_window_flag")? {
			for (i, name) in ["conf_win_left_offset", "conf_win_right_offset", "conf_win_top_offset", "conf_win_bottom_offset"].into_iter().enumerate() {
				crop[i] = b.ue(name, MAX_SIDE)? * if i < 2 { sub_w } else { sub_h };
			}
			if crop[0] + crop[1] >= width || crop[2] + crop[3] >= height {
				return Err(Error::corrupt("the conformance window crops the whole picture"));
			}
		}
		let bit_depth_luma = b.ue("bit_depth_luma_minus8", 8)? as u8 + 8;
		let bit_depth_chroma = b.ue("bit_depth_chroma_minus8", 8)? as u8 + 8;
		let log2_max_poc_lsb = b.ue("log2_max_pic_order_cnt_lsb_minus4", 12)? as u8 + 4;
		let ordering = b.flag("sps_sub_layer_ordering_info_present_flag")?;
		let (mut max_dec_pic_buffering, mut max_num_reorder) = (0, 0);
		for i in if ordering { 0 } else { max_sub_layers - 1 }..max_sub_layers {
			max_dec_pic_buffering = b.ue(&format!("sps_max_dec_pic_buffering_minus1[{i}]"), 15)? as u8 + 1;
			max_num_reorder = b.ue(&format!("sps_max_num_reorder_pics[{i}]"), u32::from(max_dec_pic_buffering) - 1)? as u8;
			b.ue(&format!("sps_max_latency_increase_plus1[{i}]"), u32::MAX - 1)?;
		}
		let log2_min_cb = b.ue("log2_min_luma_coding_block_size_minus3", 3)? as u8 + 3;
		let log2_ctb = log2_min_cb + b.ue("log2_diff_max_min_luma_coding_block_size", 3)? as u8;
		if !(4..=6).contains(&log2_ctb) {
			return Err(Error::corrupt(format!("a coding tree block of {0}x{0}", 1 << log2_ctb)));
		}
		if width % (1 << log2_min_cb) != 0 || height % (1 << log2_min_cb) != 0 || width == 0 || height == 0 {
			return Err(Error::corrupt(format!("a {width}x{height} picture isn't made of {0}x{0} coding blocks", 1 << log2_min_cb)));
		}
		let log2_min_tb = b.ue("log2_min_luma_transform_block_size_minus2", 3)? as u8 + 2;
		let log2_max_tb = log2_min_tb + b.ue("log2_diff_max_min_luma_transform_block_size", 3)? as u8;
		if log2_min_tb >= log2_min_cb || log2_max_tb > log2_ctb.min(5) {
			return Err(Error::corrupt("transform block sizes don't fit the coding blocks"));
		}
		let depth = u32::from(log2_ctb - log2_min_tb);
		let max_transform_depth_inter = b.ue("max_transform_hierarchy_depth_inter", depth)? as u8;
		let max_transform_depth_intra = b.ue("max_transform_hierarchy_depth_intra", depth)? as u8;
		let scaling = if b.flag("scaling_list_enabled_flag")? {
			Some(if b.flag("sps_scaling_list_data_present_flag")? { scaling_list_data(b, chroma_format)? } else { ScalingLists::default_lists() })
		} else {
			None
		};
		let amp = b.flag("amp_enabled_flag")?;
		let sao = b.flag("sample_adaptive_offset_enabled_flag")?;
		let pcm = if b.flag("pcm_enabled_flag")? {
			let (sps_luma, sps_chroma) = (bit_depth_luma, bit_depth_chroma);
			let bit_depth_luma = b.u(4, "pcm_sample_bit_depth_luma_minus1")? as u8 + 1;
			let bit_depth_chroma = b.u(4, "pcm_sample_bit_depth_chroma_minus1")? as u8 + 1;
			let log2_min_size = b.ue("log2_min_pcm_luma_coding_block_size_minus3", 2)? as u8 + 3;
			let log2_max_size = log2_min_size + b.ue("log2_diff_max_min_pcm_luma_coding_block_size", 2)? as u8;
			if bit_depth_luma > sps_luma || bit_depth_chroma > sps_chroma || log2_max_size > log2_ctb.min(5) || log2_min_size < log2_min_cb.min(5) {
				return Err(Error::corrupt("bad PCM settings"));
			}
			let loop_filter_disabled = b.flag("pcm_loop_filter_disabled_flag")?;
			Some(Pcm { bit_depth_luma, bit_depth_chroma, log2_min_size, log2_max_size, loop_filter_disabled })
		} else {
			None
		};
		let count = b.ue("num_short_term_ref_pic_sets", 64)? as usize;
		let mut short_term_rps = Vec::with_capacity(count);
		for i in 0..count {
			let set = short_term_ref_pic_set(b, i, &short_term_rps, count)?;
			short_term_rps.push(set);
		}
		let long_term = if b.flag("long_term_ref_pics_present_flag")? {
			let n = b.ue("num_long_term_ref_pics_sps", 32)?;
			let mut list = Vec::new();
			for i in 0..n {
				let lsb = b.u(u32::from(log2_max_poc_lsb), &format!("lt_ref_pic_poc_lsb_sps[{i}]"))?;
				list.push((lsb, b.flag(&format!("used_by_curr_pic_lt_sps_flag[{i}]"))?));
			}
			Some(list)
		} else {
			None
		};
		let temporal_mvp = b.flag("sps_temporal_mvp_enabled_flag")?;
		let strong_intra_smoothing = b.flag("strong_intra_smoothing_enabled_flag")?;
		let colour = if b.flag("vui_parameters_present_flag")? { vui(b, max_sub_layers)? } else { Colour::default() };
		let mut range = SpsRange::default();
		let mut sps = Sps {
			id,
			max_sub_layers,
			profile_idc,
			level_idc,
			chroma_format,
			separate_colour_planes,
			width,
			height,
			crop,
			bit_depth_luma,
			bit_depth_chroma,
			log2_max_poc_lsb,
			max_dec_pic_buffering,
			max_num_reorder,
			log2_min_cb,
			log2_ctb,
			log2_min_tb,
			log2_max_tb,
			max_transform_depth_inter,
			max_transform_depth_intra,
			scaling,
			amp,
			sao,
			pcm,
			short_term_rps,
			long_term,
			temporal_mvp,
			strong_intra_smoothing,
			colour,
			range,
		};
		if b.flag("sps_extension_present_flag")? {
			let has_range = b.flag("sps_range_extension_flag")?;
			let multilayer = b.flag("sps_multilayer_extension_flag")?;
			let three_d = b.flag("sps_3d_extension_flag")?;
			let scc = b.flag("sps_scc_extension_flag")?;
			let more = b.u(4, "sps_extension_4bits")? != 0;
			if has_range {
				range = SpsRange {
					transform_skip_rotation: b.flag("transform_skip_rotation_enabled_flag")?,
					transform_skip_context: b.flag("transform_skip_context_enabled_flag")?,
					implicit_rdpcm: b.flag("implicit_rdpcm_enabled_flag")?,
					explicit_rdpcm: b.flag("explicit_rdpcm_enabled_flag")?,
					extended_precision: b.flag("extended_precision_processing_flag")?,
					intra_smoothing_disabled: b.flag("intra_smoothing_disabled_flag")?,
					high_precision_offsets: b.flag("high_precision_offsets_enabled_flag")?,
					persistent_rice_adaptation: b.flag("persistent_rice_adaptation_enabled_flag")?,
					cabac_bypass_alignment: b.flag("cabac_bypass_alignment_enabled_flag")?,
				};
				sps.range = range;
			}
			if multilayer {
				b.flag("inter_view_mv_vert_constraint_flag")?;
			}
			if scc {
				return Err(Error::unsupported("screen content coding"));
			}
			// The 3D extension and extension data are for other layers: the base layer's
			// settings are all read, so stop here.
			if three_d || more {
				return Ok(sps);
			}
		}
		b.trailing()?;
		Ok(sps)
	}

	/// The width and height in coding tree blocks.
	pub fn ctbs(&self) -> (u32, u32) {
		(self.width.div_ceil(1 << self.log2_ctb), self.height.div_ceil(1 << self.log2_ctb))
	}
}

/// profile_tier_level (7.3.3): only the general profile and level are kept.
fn profile_tier_level(b: &mut Bits, max_sub_layers: u8) -> Result<(u8, u8)> {
	b.u(2, "general_profile_space")?;
	b.flag("general_tier_flag")?;
	let profile = b.u(5, "general_profile_idc")? as u8;
	for j in 0..32 {
		b.flag(&format!("general_profile_compatibility_flag[{j}]"))?;
	}
	b.flag("general_progressive_source_flag")?;
	b.flag("general_interlaced_source_flag")?;
	b.flag("general_non_packed_constraint_flag")?;
	b.flag("general_frame_only_constraint_flag")?;
	b.skip(44)?; // constraint flags that depend on the profile, then one more
	let level = b.u(8, "general_level_idc")? as u8;
	let mut present = Vec::new();
	for i in 0..max_sub_layers - 1 {
		let profile = b.flag(&format!("sub_layer_profile_present_flag[{i}]"))?;
		let level = b.flag(&format!("sub_layer_level_present_flag[{i}]"))?;
		present.push((profile, level));
	}
	if max_sub_layers > 1 {
		b.skip(2 * (9 - max_sub_layers as usize))?; // reserved_zero_2bits
	}
	for (profile, level) in present {
		if profile {
			b.skip(88)?;
		}
		if level {
			b.skip(8)?;
		}
	}
	Ok((profile, level))
}

/// The VUI (E.2.1): only the colour description is kept, but everything is read to get past it.
fn vui(b: &mut Bits, max_sub_layers: u8) -> Result<Colour> {
	let mut colour = Colour::default();
	if b.flag("aspect_ratio_info_present_flag")? && b.u(8, "aspect_ratio_idc")? == 255 {
		b.u(16, "sar_width")?;
		b.u(16, "sar_height")?;
	}
	if b.flag("overscan_info_present_flag")? {
		b.flag("overscan_appropriate_flag")?;
	}
	if b.flag("video_signal_type_present_flag")? {
		b.u(3, "video_format")?;
		colour.full_range = b.flag("video_full_range_flag")?;
		if b.flag("colour_description_present_flag")? {
			colour.primaries = b.u(8, "colour_primaries")? as u8;
			colour.transfer = b.u(8, "transfer_characteristics")? as u8;
			colour.matrix = b.u(8, "matrix_coefficients")? as u8;
		}
	}
	if b.flag("chroma_loc_info_present_flag")? {
		b.ue("chroma_sample_loc_type_top_field", 5)?;
		b.ue("chroma_sample_loc_type_bottom_field", 5)?;
	}
	b.flag("neutral_chroma_indication_flag")?;
	b.flag("field_seq_flag")?;
	b.flag("frame_field_info_present_flag")?;
	if b.flag("default_display_window_flag")? {
		for name in ["def_disp_win_left_offset", "def_disp_win_right_offset", "def_disp_win_top_offset", "def_disp_win_bottom_offset"] {
			b.ue(name, MAX_SIDE)?;
		}
	}
	if b.flag("vui_timing_info_present_flag")? {
		b.u(32, "vui_num_units_in_tick")?;
		b.u(32, "vui_time_scale")?;
		if b.flag("vui_poc_proportional_to_timing_flag")? {
			b.ue("vui_num_ticks_poc_diff_one_minus1", u32::MAX - 1)?;
		}
		if b.flag("vui_hrd_parameters_present_flag")? {
			hrd_parameters(b, max_sub_layers)?;
		}
	}
	if b.flag("bitstream_restriction_flag")? {
		b.flag("tiles_fixed_structure_flag")?;
		b.flag("motion_vectors_over_pic_boundaries_flag")?;
		b.flag("restricted_ref_pic_lists_flag")?;
		b.ue("min_spatial_segmentation_idc", 4095)?;
		b.ue("max_bytes_per_pic_denom", 16)?;
		b.ue("max_bits_per_min_cu_denom", 16)?;
		b.ue("log2_max_mv_length_horizontal", 15)?;
		b.ue("log2_max_mv_length_vertical", 15)?;
	}
	Ok(colour)
}

/// hrd_parameters (E.2.2), read only to get past them.
fn hrd_parameters(b: &mut Bits, max_sub_layers: u8) -> Result<()> {
	let nal = b.flag("nal_hrd_parameters_present_flag")?;
	let vcl = b.flag("vcl_hrd_parameters_present_flag")?;
	let mut sub_pic = false;
	if nal || vcl {
		sub_pic = b.flag("sub_pic_hrd_params_present_flag")?;
		if sub_pic {
			b.u(8, "tick_divisor_minus2")?;
			b.u(5, "du_cpb_removal_delay_increment_length_minus1")?;
			b.flag("sub_pic_cpb_params_in_pic_timing_sei_flag")?;
			b.u(5, "dpb_output_delay_du_length_minus1")?;
		}
		b.u(4, "bit_rate_scale")?;
		b.u(4, "cpb_size_scale")?;
		if sub_pic {
			b.u(4, "cpb_size_du_scale")?;
		}
		b.u(5, "initial_cpb_removal_delay_length_minus1")?;
		b.u(5, "au_cpb_removal_delay_length_minus1")?;
		b.u(5, "dpb_output_delay_length_minus1")?;
	}
	for i in 0..max_sub_layers {
		let general = b.flag(&format!("fixed_pic_rate_general_flag[{i}]"))?;
		let within_cvs = general || b.flag(&format!("fixed_pic_rate_within_cvs_flag[{i}]"))?;
		let low_delay = if within_cvs {
			b.ue(&format!("elemental_duration_in_tc_minus1[{i}]"), 2047)?;
			false
		} else {
			b.flag(&format!("low_delay_hrd_flag[{i}]"))?
		};
		let cpb_count = if low_delay { 1 } else { b.ue(&format!("cpb_cnt_minus1[{i}]"), 31)? + 1 };
		for _ in (0..2).filter(|&k| [nal, vcl][k]) {
			for j in 0..cpb_count {
				b.ue(&format!("bit_rate_value_minus1[{j}]"), u32::MAX - 1)?;
				b.ue(&format!("cpb_size_value_minus1[{j}]"), u32::MAX - 1)?;
				if sub_pic {
					b.ue(&format!("cpb_size_du_value_minus1[{j}]"), u32::MAX - 1)?;
					b.ue(&format!("bit_rate_du_value_minus1[{j}]"), u32::MAX - 1)?;
				}
				b.flag(&format!("cbr_flag[{j}]"))?;
			}
		}
	}
	Ok(())
}

/// st_ref_pic_set (7.3.7), with the sets it's predicted from worked out (7.4.8). `idx` is its
/// place in `sets`, or `count` (the number in the SPS) for a slice header's own set.
pub(crate) fn short_term_ref_pic_set(b: &mut Bits, idx: usize, sets: &[ShortTermRps], count: usize) -> Result<ShortTermRps> {
	if idx != 0 && b.flag("inter_ref_pic_set_prediction_flag")? {
		let delta_idx = if idx == count { b.ue("delta_idx_minus1", idx as u32 - 1)? as usize + 1 } else { 1 };
		let reference = &sets[idx - delta_idx];
		let sign = b.flag("delta_rps_sign")?;
		let abs = b.ue("abs_delta_rps_minus1", (1 << 15) - 1)? as i32 + 1;
		let delta_rps = if sign { -abs } else { abs };
		// For each picture of the reference set, and the reference picture itself: whether
		// it's used by the current picture, and whether it's in the set at all.
		let n = reference.negative.len() + reference.positive.len();
		let mut used = Vec::with_capacity(n + 1);
		for j in 0..=n {
			let used_by_curr = b.flag(&format!("used_by_curr_pic_flag[{j}]"))?;
			let use_delta = used_by_curr || b.flag(&format!("use_delta_flag[{j}]"))?;
			used.push((used_by_curr, use_delta));
		}
		let neg = &reference.negative;
		let pos = &reference.positive;
		let (n_neg, n_all) = (neg.len(), n);
		let mut set = ShortTermRps::default();
		// Equations 7-61 and 7-62.
		for (j, &(d, _)) in pos.iter().enumerate().rev() {
			let (u, keep) = used[n_neg + j];
			if d + delta_rps < 0 && keep {
				set.negative.push((d + delta_rps, u));
			}
		}
		if delta_rps < 0 && used[n_all].1 {
			set.negative.push((delta_rps, used[n_all].0));
		}
		for (j, &(d, _)) in neg.iter().enumerate() {
			let (u, keep) = used[j];
			if d + delta_rps < 0 && keep {
				set.negative.push((d + delta_rps, u));
			}
		}
		for (j, &(d, _)) in neg.iter().enumerate().rev() {
			let (u, keep) = used[j];
			if d + delta_rps > 0 && keep {
				set.positive.push((d + delta_rps, u));
			}
		}
		if delta_rps > 0 && used[n_all].1 {
			set.positive.push((delta_rps, used[n_all].0));
		}
		for (j, &(d, _)) in pos.iter().enumerate() {
			let (u, keep) = used[n_neg + j];
			if d + delta_rps > 0 && keep {
				set.positive.push((d + delta_rps, u));
			}
		}
		if set.negative.len() + set.positive.len() > MAX_REFS {
			return Err(Error::corrupt("a reference picture set names more than 16 pictures"));
		}
		return Ok(set);
	}
	let negatives = b.ue("num_negative_pics", MAX_REFS as u32)? as usize;
	let positives = b.ue("num_positive_pics", (MAX_REFS - negatives) as u32)? as usize;
	let mut set = ShortTermRps::default();
	let mut poc = 0;
	for i in 0..negatives {
		poc -= b.ue(&format!("delta_poc_s0_minus1[{i}]"), (1 << 15) - 1)? as i32 + 1;
		set.negative.push((poc, b.flag(&format!("used_by_curr_pic_s0_flag[{i}]"))?));
	}
	poc = 0;
	for i in 0..positives {
		poc += b.ue(&format!("delta_poc_s1_minus1[{i}]"), (1 << 15) - 1)? as i32 + 1;
		set.positive.push((poc, b.flag(&format!("used_by_curr_pic_s1_flag[{i}]"))?));
	}
	Ok(set)
}

/// Table 7-6: the default 8x8 (and larger) lists, intra then inter.
const DEFAULT_INTRA: [u8; 64] = [
	16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18, 17, 18, 18, 17, 18, 21, 19, 20, 21, 20, 19, 21, 24, 22, 22, 24, 24, 22, 22, 24, 25, 25, 27,
	30, 27, 25, 25, 29, 31, 35, 35, 31, 29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115,
];
const DEFAULT_INTER: [u8; 64] = [
	16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18, 18, 18, 18, 18, 18, 20, 20, 20, 20, 20, 20, 20, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25,
	25, 25, 25, 25, 28, 28, 28, 28, 28, 28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91,
];

impl ScalingLists {
	/// The lists used when scaling lists are on but none are sent (Tables 7-5 and 7-6).
	pub fn default_lists() -> ScalingLists {
		let mut s = ScalingLists { lists: [[[16; 64]; 6]; 4], dc: [[16; 6]; 2] };
		for size in 1..4 {
			for matrix in 0..6 {
				s.lists[size][matrix] = default_list(size, matrix);
			}
		}
		s
	}
}

fn default_list(size: usize, matrix: usize) -> [u8; 64] {
	match (size, matrix) {
		(0, _) => [16; 64],
		(_, 0..3) => DEFAULT_INTRA,
		_ => DEFAULT_INTER,
	}
}

/// scaling_list_data (7.3.4).
fn scaling_list_data(b: &mut Bits, chroma_format: u8) -> Result<ScalingLists> {
	let mut s = ScalingLists::default_lists();
	for size in 0..4 {
		let step = if size == 3 { 3 } else { 1 };
		for matrix in (0..6).step_by(step) {
			let coefs = 64.min(1 << (4 + 2 * size));
			if !b.flag(&format!("scaling_list_pred_mode_flag[{size}][{matrix}]"))? {
				let delta = b.ue(&format!("scaling_list_pred_matrix_id_delta[{size}][{matrix}]"), (matrix / step) as u32)? as usize;
				if delta == 0 {
					s.lists[size][matrix] = default_list(size, matrix);
					if size > 1 {
						s.dc[size - 2][matrix] = 16;
					}
				} else {
					let from = matrix - delta * step;
					s.lists[size][matrix] = s.lists[size][from];
					if size > 1 {
						s.dc[size - 2][matrix] = s.dc[size - 2][from];
					}
				}
			} else {
				let mut next: i32 = 8;
				if size > 1 {
					next = b.se(&format!("scaling_list_dc_coef_minus8[{}][{matrix}]", size - 2), -7, 247)? + 8;
					s.dc[size - 2][matrix] = next as u8;
				}
				for i in 0..coefs {
					let delta = b.se(&format!("scaling_list_delta_coef[{size}][{matrix}][{i}]"), -128, 127)?;
					next = (next + delta + 256) % 256;
					if next == 0 {
						return Err(Error::corrupt("a scaling list factor is 0"));
					}
					s.lists[size][matrix][i] = next as u8;
				}
			}
		}
	}
	// 4:4:4 chroma at 32x32 uses the 16x16 chroma lists (7.4.5).
	if chroma_format == 3 {
		for matrix in [1, 2, 4, 5] {
			s.lists[3][matrix] = s.lists[2][matrix];
			s.dc[1][matrix] = s.dc[0][matrix];
		}
	}
	Ok(s)
}

/// The parameter sets seen so far, by ID: later ones replace earlier ones.
#[derive(Clone, Debug)]
pub struct ParamSets {
	sps: Vec<Option<Sps>>,
	pps: Vec<Option<Pps>>,
}

impl Default for ParamSets {
	fn default() -> Self {
		ParamSets::new()
	}
}

impl ParamSets {
	pub fn new() -> ParamSets {
		ParamSets { sps: vec![None; 16], pps: vec![None; 64] }
	}

	/// Keep an SPS or PPS NAL unit; any other kind is ignored.
	pub fn add(&mut self, nal: &[u8]) -> Result<()> {
		match NalHeader::parse(nal)? {
			NalHeader { kind: nal::SPS, layer: 0, .. } => {
				let sps = Sps::parse(nal)?;
				let id = sps.id as usize;
				self.sps[id] = Some(sps);
			}
			NalHeader { kind: nal::PPS, layer: 0, .. } => {
				let pps = Pps::parse(nal)?;
				let id = pps.id as usize;
				self.pps[id] = Some(pps);
			}
			_ => {}
		}
		Ok(())
	}

	/// The PPS with this ID and the SPS it refers to.
	pub fn get(&self, pps_id: u32) -> Result<(&Pps, &Sps)> {
		let missing = || Error::corrupt(format!("a slice refers to PPS {pps_id}, which hasn't been sent"));
		let pps = self.pps.get(pps_id as usize).and_then(Option::as_ref).ok_or_else(missing)?;
		let sps = self.sps.get(pps.sps_id as usize).and_then(Option::as_ref);
		let sps = sps.ok_or_else(|| Error::corrupt(format!("PPS {pps_id} refers to SPS {}, which hasn't been sent", pps.sps_id)))?;
		Ok((pps, sps))
	}
}

impl Pps {
	/// Parse a PPS NAL unit, as it's stored (with emulation prevention bytes).
	pub fn parse(nal: &[u8]) -> Result<Pps> {
		let data = rbsp(nal);
		let mut b = Bits::new(&data, false);
		header(&mut b, nal::PPS)?;
		Pps::read(&mut b)
	}

	/// [`Pps::parse`], also giving every syntax element read.
	pub fn trace(nal: &[u8]) -> (Result<Pps>, Vec<Syntax>) {
		traced(nal, nal::PPS, Pps::read)
	}

	fn read(b: &mut Bits) -> Result<Pps> {
		let id = b.ue("pps_pic_parameter_set_id", 63)? as u8;
		let sps_id = b.ue("pps_seq_parameter_set_id", 15)? as u8;
		let dependent_slice_segments = b.flag("dependent_slice_segments_enabled_flag")?;
		let output_flag_present = b.flag("output_flag_present_flag")?;
		let num_extra_slice_header_bits = b.u(3, "num_extra_slice_header_bits")? as u8;
		let sign_data_hiding = b.flag("sign_data_hiding_enabled_flag")?;
		let cabac_init_present = b.flag("cabac_init_present_flag")?;
		let l0 = b.ue("num_ref_idx_l0_default_active_minus1", 14)? as u8 + 1;
		let l1 = b.ue("num_ref_idx_l1_default_active_minus1", 14)? as u8 + 1;
		// At most 25; at least -26 less the extra range of deeper samples, checked against the SPS later.
		let init_qp = 26 + b.se("init_qp_minus26", -(26 + 6 * 8), 25)?;
		let constrained_intra_pred = b.flag("constrained_intra_pred_flag")?;
		let transform_skip = b.flag("transform_skip_enabled_flag")?;
		let cu_qp_delta_depth = if b.flag("cu_qp_delta_enabled_flag")? { Some(b.ue("diff_cu_qp_delta_depth", 3)? as u8) } else { None };
		let cb_qp_offset = b.se("pps_cb_qp_offset", -12, 12)? as i8;
		let cr_qp_offset = b.se("pps_cr_qp_offset", -12, 12)? as i8;
		let slice_chroma_qp_offsets_present = b.flag("pps_slice_chroma_qp_offsets_present_flag")?;
		let weighted_pred = b.flag("weighted_pred_flag")?;
		let weighted_bipred = b.flag("weighted_bipred_flag")?;
		let transquant_bypass = b.flag("transquant_bypass_enabled_flag")?;
		let tiles_enabled = b.flag("tiles_enabled_flag")?;
		let entropy_coding_sync = b.flag("entropy_coding_sync_enabled_flag")?;
		let tiles = if tiles_enabled {
			let columns = b.ue("num_tile_columns_minus1", 63)? + 1;
			let rows = b.ue("num_tile_rows_minus1", 63)? + 1;
			let (mut column_widths, mut row_heights) = (None, None);
			if !b.flag("uniform_spacing_flag")? {
				let mut sizes = |n: u32, name: &str| -> Result<Vec<u32>> { (0..n - 1).map(|i| Ok(b.ue(&format!("{name}[{i}]"), MAX_SIDE)? + 1)).collect() };
				column_widths = Some(sizes(columns, "column_width_minus1")?);
				row_heights = Some(sizes(rows, "row_height_minus1")?);
			}
			let loop_filter_across_tiles = b.flag("loop_filter_across_tiles_enabled_flag")?;
			Some(Tiles { columns, rows, column_widths, row_heights, loop_filter_across_tiles })
		} else {
			None
		};
		let loop_filter_across_slices = b.flag("pps_loop_filter_across_slices_enabled_flag")?;
		let mut deblocking = Deblocking::default();
		if b.flag("deblocking_filter_control_present_flag")? {
			deblocking.override_enabled = b.flag("deblocking_filter_override_enabled_flag")?;
			deblocking.disabled = b.flag("pps_deblocking_filter_disabled_flag")?;
			if !deblocking.disabled {
				deblocking.beta_offset_div2 = b.se("pps_beta_offset_div2", -6, 6)? as i8;
				deblocking.tc_offset_div2 = b.se("pps_tc_offset_div2", -6, 6)? as i8;
			}
		}
		// The chroma format only matters for 4:4:4's 32x32 chroma lists, which the decoder
		// takes from the 16x16 ones itself when it needs them.
		let scaling = if b.flag("pps_scaling_list_data_present_flag")? { Some(scaling_list_data(b, 1)?) } else { None };
		let lists_modification_present = b.flag("lists_modification_present_flag")?;
		let log2_parallel_merge_level = b.ue("log2_parallel_merge_level_minus2", 4)? as u8 + 2;
		let slice_header_extension_present = b.flag("slice_segment_header_extension_present_flag")?;
		let mut pps = Pps {
			id,
			sps_id,
			dependent_slice_segments,
			output_flag_present,
			num_extra_slice_header_bits,
			sign_data_hiding,
			cabac_init_present,
			num_ref_idx_default: [l0, l1],
			init_qp,
			constrained_intra_pred,
			transform_skip,
			cu_qp_delta_depth,
			cb_qp_offset,
			cr_qp_offset,
			slice_chroma_qp_offsets_present,
			weighted_pred,
			weighted_bipred,
			transquant_bypass,
			tiles,
			entropy_coding_sync,
			loop_filter_across_slices,
			deblocking,
			scaling,
			lists_modification_present,
			log2_parallel_merge_level,
			slice_header_extension_present,
			range: PpsRange::default(),
		};
		if b.flag("pps_extension_present_flag")? {
			let has_range = b.flag("pps_range_extension_flag")?;
			let multilayer = b.flag("pps_multilayer_extension_flag")?;
			let three_d = b.flag("pps_3d_extension_flag")?;
			let scc = b.flag("pps_scc_extension_flag")?;
			let more = b.u(4, "pps_extension_4bits")? != 0;
			if has_range {
				let r = &mut pps.range;
				if pps.transform_skip {
					r.log2_max_transform_skip_size = b.ue("log2_max_transform_skip_block_size_minus2", 3)? as u8 + 2;
				}
				r.cross_component_prediction = b.flag("cross_component_prediction_enabled_flag")?;
				if b.flag("chroma_qp_offset_list_enabled_flag")? {
					let depth = b.ue("diff_cu_chroma_qp_offset_depth", 3)? as u8;
					let len = b.ue("chroma_qp_offset_list_len_minus1", 5)? + 1;
					let mut list = Vec::new();
					for i in 0..len {
						let cb = b.se(&format!("cb_qp_offset_list[{i}]"), -12, 12)? as i8;
						list.push((cb, b.se(&format!("cr_qp_offset_list[{i}]"), -12, 12)? as i8));
					}
					r.chroma_qp_offset_list = Some((depth, list));
				}
				r.log2_sao_offset_scale_luma = b.ue("log2_sao_offset_scale_luma", 6)? as u8;
				r.log2_sao_offset_scale_chroma = b.ue("log2_sao_offset_scale_chroma", 6)? as u8;
			}
			if scc {
				return Err(Error::unsupported("screen content coding"));
			}
			// Extensions for other layers: the base layer's settings are all read.
			if multilayer || three_d || more {
				return Ok(pps);
			}
		}
		b.trailing()?;
		Ok(pps)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// The parameter sets of a real film's HEVC track (1920x1080, x265 2.1).
	const SPS: [u8; 41] = [
		0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x78, 0xA0, 0x03, 0xC0, 0x80, 0x10, 0xE5, 0x96, 0x56,
		0x69, 0x24, 0xCA, 0xF0, 0x10, 0x10, 0x00, 0x00, 0x3E, 0x90, 0x00, 0x07, 0x53, 0x00, 0x80,
	];
	const PPS: [u8; 7] = [0x44, 0x01, 0xC1, 0x72, 0xB4, 0x62, 0x40];

	#[test]
	fn reads_a_real_pps() {
		let pps = Pps::parse(&PPS).unwrap();
		assert_eq!((pps.id, pps.sps_id, pps.init_qp, pps.cu_qp_delta_depth), (0, 0, 26, Some(1)));
		assert!(pps.sign_data_hiding && pps.entropy_coding_sync && pps.weighted_pred && pps.loop_filter_across_slices);
		assert!(pps.tiles.is_none() && !pps.transform_skip && !pps.dependent_slice_segments);
		assert_eq!(pps.deblocking, Deblocking::default());
		assert_eq!(Pps::trace(&PPS).1.last().map(|s| s.pos), Some(55));
	}

	#[test]
	fn reads_a_real_sps() {
		let sps = Sps::parse(&SPS).unwrap();
		assert_eq!((sps.width, sps.height, sps.crop), (1920, 1080, [0; 4]));
		assert_eq!((sps.chroma_format, sps.bit_depth_luma, sps.bit_depth_chroma), (1, 8, 8));
		assert_eq!((sps.log2_min_cb, sps.log2_ctb, sps.log2_min_tb, sps.log2_max_tb), (3, 6, 2, 5));
		assert_eq!((sps.profile_idc, sps.level_idc), (1, 120));
		assert!(sps.sao && sps.strong_intra_smoothing && sps.temporal_mvp && !sps.amp);
		assert_eq!(sps.scaling, None);
		assert_eq!(sps.ctbs(), (30, 17));
		let (again, trace) = Sps::trace(&SPS);
		assert_eq!(again.unwrap(), sps);
		let at = |name: &str| trace.iter().find(|s| s.name == name).map(|s| (s.pos, s.value));
		assert_eq!(at("pic_width_in_luma_samples"), Some((124, 1920)));
		assert_eq!(at("vui_time_scale"), Some((260, 30000)));
		assert_eq!(at("rbsp_stop_one_bit"), Some((296, 1)));
	}

	/// Write fields MSB first, for building parameter sets by hand.
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
			self.ue(if v > 0 { 2 * v as u32 - 1 } else { (-2 * v) as u32 })
		}
		fn finish(&mut self) -> Vec<u8> {
			self.u(1, 1);
			while !self.bits.len().is_multiple_of(8) {
				self.bits.push(false);
			}
			self.bits.chunks(8).map(|c| c.iter().fold(0, |b, &x| b << 1 | x as u8)).collect()
		}
	}

	#[test]
	fn reads_tiles_deblocking_and_range_extension() {
		let mut w = Writer::default();
		w.u(16, 0x4401).ue(3).ue(1).u(1, 1).u(1, 0).u(3, 2).u(1, 0).u(1, 1).ue(2).ue(0).se(-4);
		w.u(1, 0).u(1, 1).u(1, 1).ue(2).se(-3).se(5).u(1, 1).u(1, 0).u(1, 1).u(1, 0);
		// Tiles and wavefronts; 3 columns of 5, 2 and the rest, 2 rows of 7 and the rest.
		w.u(1, 1).u(1, 1).ue(2).ue(1).u(1, 0).ue(4).ue(1).ue(6).u(1, 1);
		w.u(1, 0).u(1, 1).u(1, 1).u(1, 0).se(-2).se(3); // deblocking
		w.u(1, 0).u(1, 1).ue(1).u(1, 0);
		w.u(1, 1).u(1, 1).u(3, 0).u(4, 0); // the range extension only
		w.ue(1).u(1, 1).u(1, 1).ue(1).ue(1).se(2).se(-1).se(-5).se(6).ue(2).ue(1);
		let (pps, trace) = Pps::trace(&w.finish());
		let pps = pps.unwrap();
		assert_eq!((pps.id, pps.sps_id, pps.num_extra_slice_header_bits, pps.num_ref_idx_default, pps.init_qp), (3, 1, 2, [3, 1], 22));
		assert!(pps.dependent_slice_segments && pps.cabac_init_present && pps.transform_skip && pps.weighted_bipred);
		assert_eq!((pps.cu_qp_delta_depth, pps.cb_qp_offset, pps.cr_qp_offset), (Some(2), -3, 5));
		let tiles = pps.tiles.unwrap();
		assert_eq!((tiles.columns, tiles.rows, tiles.column_widths, tiles.row_heights), (3, 2, Some(vec![5, 2]), Some(vec![7])));
		assert!(tiles.loop_filter_across_tiles && pps.entropy_coding_sync && !pps.loop_filter_across_slices);
		assert_eq!(pps.deblocking, Deblocking { override_enabled: true, disabled: false, beta_offset_div2: -2, tc_offset_div2: 3 });
		assert!(pps.lists_modification_present);
		assert_eq!(pps.log2_parallel_merge_level, 3);
		let r = pps.range;
		assert_eq!((r.log2_max_transform_skip_size, r.cross_component_prediction), (3, true));
		assert_eq!(r.chroma_qp_offset_list, Some((1, vec![(2, -1), (-5, 6)])));
		assert_eq!((r.log2_sao_offset_scale_luma, r.log2_sao_offset_scale_chroma), (2, 1));
		assert_eq!(trace.last().unwrap().name, "rbsp_alignment_zero_bit");
	}

	#[test]
	fn predicts_reference_picture_sets() {
		// Set 0: -1 and -3 (both used) and +2 (not used). Set 1 is predicted from it moved by -1:
		// -2 and -4 from the negatives, +1 from +2, and the reference picture itself at -1.
		let mut w = Writer::default();
		w.ue(2).ue(2).ue(1).ue(0).u(1, 1).ue(1).u(1, 1).ue(1).u(1, 0);
		w.u(1, 1).u(1, 1).ue(0); // inter prediction, minus, |delta| 1
		// used_by_curr_pic for -1, -3, +2 and the reference: yes, no (but kept), no (dropped), yes.
		w.u(1, 1).u(1, 0).u(1, 1).u(1, 0).u(1, 0).u(1, 1);
		let data = w.finish();
		let mut b = Bits::new(&data, false);
		let count = b.ue("n", 64).unwrap() as usize;
		let mut sets = Vec::new();
		for i in 0..count {
			let s = short_term_ref_pic_set(&mut b, i, &sets, count).unwrap();
			sets.push(s);
		}
		assert_eq!(sets[0], ShortTermRps { negative: vec![(-1, true), (-3, true)], positive: vec![(2, false)] });
		assert_eq!(sets[1], ShortTermRps { negative: vec![(-1, true), (-2, true), (-4, false)], positive: vec![] });
		b.trailing().unwrap();
	}

	#[test]
	fn reads_scaling_lists() {
		let mut w = Writer::default();
		// 4x4 intra Y explicit (9, 11, 13 ...), 16x16 intra Cb explicit (DC 18, all 18), and
		// copies and defaults for the rest.
		for size in 0..4 {
			let step = if size == 3 { 3 } else { 1 };
			for matrix in (0..6).step_by(step) {
				if (size, matrix) == (0, 0) {
					w.u(1, 1);
					w.se(1);
					for _ in 1..16 {
						w.se(2);
					}
				} else if (size, matrix) == (2, 1) {
					w.u(1, 1).se(10); // DC 18
					for _ in 0..64 {
						w.se(0); // all 18
					}
				} else if (size, matrix) == (2, 2) {
					w.u(1, 0).ue(1); // copy matrix 1
				} else if (size, matrix) == (0, 3) {
					w.u(1, 0).ue(3); // copy 4x4 intra Y
				} else {
					w.u(1, 0).ue(0); // default
				}
			}
		}
		let data = w.finish();
		let mut b = Bits::new(&data, false);
		let s = scaling_list_data(&mut b, 1).unwrap();
		b.trailing().unwrap();
		let first: Vec<u8> = (0..16).map(|i| 9 + 2 * i).collect();
		assert_eq!(s.lists[0][0][..16], first[..]);
		assert_eq!(s.lists[0][3][..16], first[..]);
		assert_eq!(s.lists[0][1][..16], [16; 16]);
		assert_eq!((s.lists[2][1], s.dc[0][1]), ([18; 64], 18));
		assert_eq!((s.lists[2][2], s.dc[0][2]), ([18; 64], 18));
		assert_eq!((s.lists[2][0], s.dc[0][0]), (DEFAULT_INTRA, 16));
		assert_eq!(s.lists[3][3], DEFAULT_INTER);
	}

	#[test]
	fn rejects_bad_parameter_sets() {
		assert!(matches!(Sps::parse(&SPS[..20]), Err(Error::Corrupt(_))), "cut short");
		assert!(matches!(Pps::parse(&SPS), Err(Error::Corrupt(_))), "an SPS isn't a PPS");
		let mut sps = SPS;
		sps[0] = 0x42 | 0x80;
		assert!(Sps::parse(&sps).is_err(), "forbidden bit");
		// Never panic on damage.
		for i in 0..SPS.len() * 8 {
			let mut s = SPS;
			s[i / 8] ^= 0x80 >> (i % 8);
			let _ = Sps::parse(&s);
			let _ = Pps::parse(&s);
		}
	}
}
