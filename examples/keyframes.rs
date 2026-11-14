//! Read every slice of some keyframes, checking each one ends where it should:
//! `keyframes <video.mp4> [count]` reads `count` keyframes spread through the video.

use std::fs::File;
use std::time::Instant;

use black_hevc::mp4::{Video, nal_units};
use black_hevc::nal::NalHeader;
use black_hevc::params::ParamSets;
use black_hevc::rbsp;
use black_hevc::slice::SliceHeader;
use black_hevc::slice_data::{CodingUnit, PictureParser, Sink, TransformBlock};

#[derive(Default)]
struct Count {
	cus: u64,
	cu_area: u64,
	tbs: u64,
	luma_area: u64,
	coded: u64,
}

impl Sink for Count {
	fn transform_block(&mut self, tb: &TransformBlock) {
		self.tbs += 1;
		if tb.c_idx == 0 {
			self.luma_area += 1 << (2 * tb.log2_size);
		}
		self.coded += u64::from(tb.coeffs.is_some());
	}

	fn coding_unit(&mut self, cu: &CodingUnit) {
		self.cus += 1;
		self.cu_area += 1 << (2 * cu.log2_size);
	}
}

/// RBSP index of each NAL unit byte that survives emulation prevention removal.
fn rbsp_of_nal(nal: &[u8]) -> Vec<usize> {
	let (mut map, mut zeros, mut r) = (Vec::new(), 0, 0);
	for &b in nal {
		map.push(r);
		if zeros >= 2 && b == 3 {
			zeros = 0;
			continue;
		}
		zeros = if b == 0 { zeros + 1 } else { 0 };
		r += 1;
	}
	map.push(r);
	map
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let args: Vec<String> = std::env::args().collect();
	let path = args.get(1).ok_or("usage: keyframes <video.mp4> [count]")?;
	let count: usize = args.get(2).map_or(Ok(10), |c| c.parse())?;
	let mut file = File::open(path)?;
	let video = Video::read(&mut file)?;
	let config = video.hevc()?;
	let mut params = ParamSets::new();
	for nal in &config.nal_units {
		params.add(nal)?;
	}
	let keys: Vec<_> = video.samples().iter().filter(|s| s.keyframe).copied().collect();
	let step = (keys.len() / count.max(1)).max(1);
	let (mut slices, mut total) = (0, Count::default());
	let start = Instant::now();
	for key in keys.iter().step_by(step).take(count) {
		let data = key.read(&mut file)?;
		let mut sink = Count::default();
		let mut picture: Option<PictureParser> = None;
		let mut previous: Option<SliceHeader> = None;
		let units = nal_units(&data, config.length_size)?;
		// Parameter sets first, so the slices can borrow them.
		for nal in &units {
			if !NalHeader::parse(nal)?.is_slice() {
				params.add(nal)?;
			}
		}
		for nal in units {
			let h = NalHeader::parse(nal)?;
			if !h.is_slice() || h.layer != 0 {
				continue;
			}
			let header = SliceHeader::parse(nal, &params, previous.as_ref())?;
			let (pps, sps) = params.get(u32::from(header.pps_id))?;
			let parser = picture.get_or_insert(PictureParser::new(sps, pps)?);
			let rbsp = rbsp(nal);
			let stats = parser.slice(&header, &rbsp, &mut sink).map_err(|e| format!("keyframe at {}: slice at CTB {}: {e}", key.offset, header.address))?;
			let map = rbsp_of_nal(nal);
			let nal_start = map.iter().position(|&r| r == header.data_offset).ok_or("no data start")?;
			let want: Vec<usize> = std::iter::once(header.data_offset).chain(header.entry_points.iter().map(|&e| map[nal_start + e as usize])).collect();
			if stats.substreams != want {
				return Err(format!("keyframe at {}: substreams start at {:?}, entry points say {want:?}", key.offset, stats.substreams).into());
			}
			slices += 1;
			previous = Some(header);
		}
		let parser = picture.ok_or("a keyframe with no slices")?;
		let (pps, sps) = params.get(u32::from(previous.as_ref().ok_or("a keyframe with no slices")?.pps_id))?;
		let _ = pps;
		let area = u64::from(sps.width) * u64::from(sps.height);
		if !parser.complete() || sink.cu_area != area || sink.luma_area != area {
			return Err(format!("keyframe at {}: CUs cover {} and luma blocks {} of {area} samples", key.offset, sink.cu_area, sink.luma_area).into());
		}
		total.cus += sink.cus;
		total.tbs += sink.tbs;
		total.coded += sink.coded;
	}
	let n = keys.len().min(count);
	println!(
		"{n} keyframes, {slices} slices read to their last byte in {:?}: {} CUs, {} transform blocks ({} with coefficients)",
		start.elapsed(),
		total.cus,
		total.tbs,
		total.coded
	);
	Ok(())
}
