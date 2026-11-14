//! Decode keyframes to raw YUV (the shown part of each plane, as ffmpeg's rawvideo writes them,
//! 16-bit little-endian above 8 bits):
//!
//! `decode <video.mp4> <out.yuv> [count]` writes `count` keyframes spread through the video
//! (default all). `decode <video.mp4> --annexb <out.hevc> [count]` writes the same keyframes
//! as a raw HEVC stream, each one a sequence of its own, for ffmpeg to decode as a reference:
//! `ffmpeg -skip_loop_filter all -i out.hevc -f rawvideo ffmpeg.yuv` (without the filters, which
//! aren't done yet). `decode <video.mp4> --compare <ffmpeg.yuv> [count]` reports where they differ.

use std::fs::File;
use std::io::Write;
use std::time::Instant;

use black_hevc::mp4::{Video, nal_units};
use black_hevc::params::ParamSets;
use black_hevc::picture::{Picture, decode};

fn bytes(pic: &Picture) -> Vec<u8> {
	let mut out = Vec::new();
	for c in 0..pic.planes.len() {
		let depth = if c == 0 { pic.bit_depth_luma } else { pic.bit_depth_chroma };
		for s in pic.cropped(c).2 {
			if depth > 8 {
				out.extend(s.to_le_bytes());
			} else {
				out.push(s as u8);
			}
		}
	}
	out
}

/// The first sample that differs: plane, x, y, ours, theirs; and how many differ.
fn first_difference(pic: &Picture, theirs: &[u8]) -> Option<String> {
	let mut at = 0;
	let mut report = None;
	let mut count = 0;
	for c in 0..pic.planes.len() {
		let wide = if c == 0 { pic.bit_depth_luma > 8 } else { pic.bit_depth_chroma > 8 };
		let (w, _, ours) = pic.cropped(c);
		for (i, &s) in ours.iter().enumerate() {
			let t = if wide {
				let v = u16::from_le_bytes([theirs[at], theirs[at + 1]]);
				at += 2;
				v
			} else {
				at += 1;
				u16::from(theirs[at - 1])
			};
			if s != t {
				count += 1;
				report.get_or_insert_with(|| format!("plane {c} at ({}, {}): {s}, ffmpeg {t}", i % w as usize, i / w as usize));
			}
		}
	}
	report.map(|r| format!("{r}; {count} samples differ"))
}
enum Mode {
	Write(File),
	Compare(Vec<u8>),
	AnnexB(File),
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let args: Vec<String> = std::env::args().collect();
	let usage = "usage: decode <video.mp4> (<out.yuv> | --compare <ffmpeg.yuv> | --annexb <out.hevc>) [count]";
	let path = args.get(1).ok_or(usage)?;
	let flag = args.get(2).filter(|a| a.starts_with("--"));
	let target = args.get(if flag.is_some() { 3 } else { 2 }).ok_or(usage)?;
	let count: usize = args.get(if flag.is_some() { 4 } else { 3 }).map_or(Ok(usize::MAX), |c| c.parse())?;
	let mut mode = match flag.map(String::as_str) {
		None => Mode::Write(File::create(target)?),
		Some("--compare") => Mode::Compare(std::fs::read(target)?),
		Some("--annexb") => Mode::AnnexB(File::create(target)?),
		Some(_) => return Err(usage.into()),
	};
	let mut file = File::open(path)?;
	let video = Video::read(&mut file)?;
	let config = video.hevc()?;
	let mut params = ParamSets::new();
	for nal in &config.nal_units {
		params.add(nal)?;
	}
	let keys: Vec<_> = video.samples().iter().filter(|s| s.keyframe).copied().collect();
	let step = (keys.len() / count.clamp(1, keys.len().max(1))).max(1);
	let (mut done, mut matching, mut offset) = (0, 0, 0);
	let start = Instant::now();
	for key in keys.iter().step_by(step).take(count) {
		let data = key.read(&mut file)?;
		let units = nal_units(&data, config.length_size)?;
		if let Mode::AnnexB(out) = &mut mode {
			// Each keyframe on its own: the parameter sets, then the picture with a CRA (21)
			// marked as a BLA (17), which has the same header but always starts a new sequence.
			// ffmpeg still drops one now and then as a duplicate POC; the cross-check test
			// avoids that by decoding each keyframe on its own.
			for nal in config.nal_units.iter().map(Vec::as_slice).chain(units) {
				out.write_all(&[0, 0, 0, 1])?;
				if nal.first().is_some_and(|b| b >> 1 == 21) {
					out.write_all(&[(nal[0] & 0x81) | 17 << 1])?;
					out.write_all(&nal[1..])?;
				} else {
					out.write_all(nal)?;
				}
			}
			done += 1;
			continue;
		}
		let pic = decode(&mut params, &units).map_err(|e| format!("keyframe {done} at byte {}: {e}", key.offset))?;
		let raw = bytes(&pic);
		match &mut mode {
			Mode::Write(out) => out.write_all(&raw)?,
			Mode::Compare(reference) => {
				let theirs = reference.get(offset..offset + raw.len()).ok_or("the reference has fewer keyframes")?;
				match first_difference(&pic, theirs) {
					None => matching += 1,
					Some(d) => {
						println!("keyframe {done}: {d}");
						if std::env::var_os("BLACK_HEVC_MAP").is_some() {
							difference_map(&pic, theirs);
						}
					}
				}
			}
			Mode::AnnexB(_) => unreachable!(),
		}
		offset += raw.len();
		done += 1;
	}
	let elapsed = start.elapsed();
	match mode {
		Mode::Compare(_) => {
			println!("{matching} of {done} keyframes match, decoded in {elapsed:?}")
		}
		Mode::Write(_) => println!("{done} keyframes decoded in {elapsed:?}"),
		Mode::AnnexB(_) => println!("{done} keyframes written"),
	}
	Ok(())
}

/// With BLACK_HEVC_MAP set: an 8x8-block map of where luma differs, `#` for a difference.
pub fn difference_map(pic: &Picture, theirs: &[u8]) {
	let (w, h, ours) = pic.cropped(0);
	for by in 0..h.div_ceil(8) {
		let row: String = (0..w.div_ceil(8))
			.map(|bx| {
				let differs =
					(by * 8..(by * 8 + 8).min(h)).any(|y| (bx * 8..(bx * 8 + 8).min(w)).any(|x| ours[(y * w + x) as usize] != u16::from(theirs[(y * w + x) as usize])));
				if differs { '#' } else { '.' }
			})
			.collect();
		println!("{row}");
	}
}
