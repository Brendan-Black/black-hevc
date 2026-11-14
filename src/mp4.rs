//! MP4 files (ISO/IEC 14496-12 and 14496-15): the first video track's samples (frames),
//! which of them are keyframes, and, for HEVC, the decoder configuration.
//!
//! Only the index (the `moov` box) is read into memory. Video files are often gigabytes with
//! the index at the end, so everything else is skipped over with `Seek`.

use std::io::{Read, Seek, SeekFrom};

use crate::{Error, Result};

/// The biggest index read into memory. An index takes about 20 bytes a frame, so a two-hour
/// film at 60 frames a second needs about 9 MB.
const MAX_INDEX: u64 = 256 << 20;

/// The most samples a track may have: a week at 25 frames a second.
const MAX_SAMPLES: u64 = 1 << 24;

/// The first video track of an MP4 file.
#[derive(Clone, Debug)]
pub struct Video {
	/// Units per second of the samples' times.
	pub timescale: u32,
	/// The codec, as the sample entry names it: `hvc1` or `hev1` for HEVC, `avc1` for H.264.
	pub codec: [u8; 4],
	pub width: u16,
	pub height: u16,
	/// The codec configuration box's contents (`hvcC` or `avcC`), empty if there's none.
	pub config: Vec<u8>,
	samples: Vec<Sample>,
}

/// One frame's compressed data in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
	/// Where its data starts in the file.
	pub offset: u64,
	pub size: u32,
	/// When it's shown, in the track's timescale, with the edit list applied (as ffmpeg
	/// reports it). Frames before the start of the edit are negative.
	pub time: i64,
	/// Whether it decodes on its own (a sync sample).
	pub keyframe: bool,
}

/// An HEVC decoder configuration (`hvcC`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HevcConfig {
	/// The bytes before each NAL unit in a sample that give its length: 1, 2 or 4.
	pub length_size: usize,
	/// The parameter sets (VPS, SPS, PPS, and any SEI), each a whole NAL unit.
	pub nal_units: Vec<Vec<u8>>,
}

impl Video {
	/// Read the first video track's index from an MP4 file.
	pub fn read<R: Read + Seek>(file: &mut R) -> Result<Video> {
		parse_moov(&read_index(file)?)
	}

	/// Every sample, in decoding order.
	pub fn samples(&self) -> &[Sample] {
		&self.samples
	}

	/// The last keyframe shown at or before `seconds` in, or the first keyframe if none is.
	pub fn keyframe_at(&self, seconds: f64) -> Option<Sample> {
		// `as` saturates, and makes NaN 0.
		let t = (seconds * f64::from(self.timescale)).floor() as i64;
		let keys = || self.samples.iter().filter(|s| s.keyframe);
		keys().filter(|s| s.time <= t).max_by_key(|s| s.time).or_else(|| keys().min_by_key(|s| s.time)).copied()
	}

	/// The HEVC decoder configuration; [`Error::Unsupported`] for any other codec.
	pub fn hevc(&self) -> Result<HevcConfig> {
		if !matches!(&self.codec, b"hvc1" | b"hev1") {
			return Err(Error::unsupported(format!("{} video (only HEVC is decoded)", name(self.codec))));
		}
		HevcConfig::parse(&self.config)
	}
}

impl Sample {
	/// Read this sample's data from the file.
	pub fn read<R: Read + Seek>(&self, file: &mut R) -> Result<Vec<u8>> {
		file.seek(SeekFrom::Start(self.offset))?;
		let mut data = vec![0; self.size as usize];
		file.read_exact(&mut data).map_err(|e| match e.kind() {
			std::io::ErrorKind::UnexpectedEof => Error::corrupt("the file ends inside a frame's data"),
			_ => e.into(),
		})?;
		Ok(data)
	}
}

impl HevcConfig {
	/// Parse the contents of an `hvcC` box.
	pub fn parse(hvcc: &[u8]) -> Result<HevcConfig> {
		let mut b = Bytes::new(hvcc, "hvcC");
		if b.u8()? != 1 {
			return Err(Error::unsupported("an unknown hvcC version"));
		}
		b.take(20)?; // profile, level, and the format's limits
		let length_size = (b.u8()? & 3) as usize + 1;
		if length_size == 3 {
			return Err(Error::corrupt("hvcC gives NAL unit lengths 3 bytes long"));
		}
		let mut nal_units = Vec::new();
		for _ in 0..b.u8()? {
			b.u8()?; // completeness and the NAL unit type, which each unit says again
			for _ in 0..b.u16()? {
				let len = b.u16()? as usize;
				nal_units.push(b.take(len)?.to_vec());
			}
		}
		Ok(HevcConfig { length_size, nal_units })
	}
}

/// Split a sample into its NAL units, each preceded by its length in `length_size` bytes.
pub fn nal_units(sample: &[u8], length_size: usize) -> Result<Vec<&[u8]>> {
	let mut b = Bytes::new(sample, "sample");
	let mut out = Vec::new();
	while !b.data.is_empty() {
		let len = b.take(length_size)?.iter().fold(0usize, |n, &x| n << 8 | x as usize);
		if len > b.data.len() {
			return Err(Error::corrupt("a NAL unit runs past the end of its frame"));
		}
		let unit = b.take(len)?;
		if !unit.is_empty() {
			out.push(unit);
		}
	}
	Ok(out)
}

/// Find the top-level `moov` box and read its contents.
fn read_index<R: Read + Seek>(file: &mut R) -> Result<Vec<u8>> {
	let len = file.seek(SeekFrom::End(0))?;
	let mut pos = 0;
	while len - pos >= 8 {
		file.seek(SeekFrom::Start(pos))?;
		let mut head = [0u8; 16];
		file.read_exact(&mut head[..8])?;
		let [s0, s1, s2, s3, k0, k1, k2, k3, ..] = head;
		let kind = [k0, k1, k2, k3];
		if pos == 0 && !kind.iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
			return Err(Error::corrupt("not an MP4 file"));
		}
		let (size, header) = match u32::from_be_bytes([s0, s1, s2, s3]) {
			0 => (len - pos, 8),
			1 if len - pos >= 16 => {
				let mut large = [0u8; 8];
				file.read_exact(&mut large)?;
				(u64::from_be_bytes(large), 16)
			}
			1 => return Err(Error::corrupt("the file is cut short")),
			n => (u64::from(n), 8),
		};
		if size < header {
			return Err(Error::corrupt(format!("the {} box is too small", name(kind))));
		}
		if size > len - pos {
			return Err(Error::corrupt(format!("the {} box runs past the end of the file; is it cut short?", name(kind))));
		}
		if &kind == b"moov" {
			if size - header > MAX_INDEX {
				return Err(Error::unsupported(format!("an index over {MAX_INDEX} bytes")));
			}
			let mut moov = vec![0; (size - header) as usize];
			file.read_exact(&mut moov)?;
			return Ok(moov);
		}
		pos += size;
	}
	Err(Error::corrupt("there's no index (moov box); the file may be cut short, or not an MP4 file"))
}

fn parse_moov(moov: &[u8]) -> Result<Video> {
	if child(moov, b"mvex")?.is_some() {
		return Err(Error::unsupported("fragmented MP4 (moof boxes)"));
	}
	let movie_scale = timescale(need(moov, b"mvhd", "moov")?, "mvhd")?;
	for item in Boxes::new(moov) {
		let (kind, trak) = item?;
		if &kind != b"trak" {
			continue;
		}
		let mdia = need(trak, b"mdia", "trak")?;
		if need(mdia, b"hdlr", "mdia")?.get(8..12) == Some(b"vide") {
			return parse_track(trak, mdia, movie_scale);
		}
	}
	Err(Error::unsupported("an MP4 file with no video track"))
}

fn parse_track(trak: &[u8], mdia: &[u8], movie_scale: u32) -> Result<Video> {
	let timescale = timescale(need(mdia, b"mdhd", "mdia")?, "mdhd")?;
	let stbl = need(need(mdia, b"minf", "mdia")?, b"stbl", "minf")?;
	let (codec, width, height, config) = sample_entry(need(stbl, b"stsd", "stbl")?)?;
	let mut samples: Vec<Sample> = sample_sizes(stbl)?.into_iter().map(|size| Sample { offset: 0, size, time: 0, keyframe: true }).collect();
	sample_offsets(stbl, &mut samples)?;
	decode_times(need(stbl, b"stts", "stbl")?, &mut samples)?;
	if let Some(ctts) = child(stbl, b"ctts")? {
		composition_offsets(ctts, &mut samples)?;
	}
	if let Some(stss) = child(stbl, b"stss")? {
		sync_samples(stss, &mut samples)?;
	}
	let shift = match child(trak, b"edts")? {
		Some(edts) => match child(edts, b"elst")? {
			Some(elst) => edit_shift(elst, movie_scale, timescale)?,
			None => 0,
		},
		None => 0,
	};
	for s in &mut samples {
		s.time = s.time.saturating_add(shift);
	}
	Ok(Video { timescale, codec, width, height, config, samples })
}

/// The timescale in an `mvhd` or `mdhd` box.
fn timescale(data: &[u8], what: &'static str) -> Result<u32> {
	let mut b = Bytes::new(data, what);
	let version = b.u8()?;
	b.take(3)?;
	b.take(if version == 1 { 16 } else { 8 })?; // creation and modification times
	match b.u32()? {
		0 => Err(Error::corrupt(format!("the {what} box gives a timescale of 0"))),
		n => Ok(n),
	}
}

/// The codec, picture size and configuration from the first sample description.
fn sample_entry(stsd: &[u8]) -> Result<([u8; 4], u16, u16, Vec<u8>)> {
	let mut b = Bytes::new(stsd, "stsd");
	b.take(4)?;
	if b.u32()? == 0 {
		return Err(Error::corrupt("the video track has no sample description"));
	}
	let Some(entry) = Boxes::new(b.data).next() else {
		return Err(Error::corrupt("the stsd box is cut short"));
	};
	let (codec, entry) = entry?;
	if matches!(&codec, b"encv" | b"drmi") {
		return Err(Error::unsupported("encrypted video"));
	}
	let mut e = Bytes::new(entry, "visual sample entry");
	e.take(24)?;
	let (width, height) = (e.u16()?, e.u16()?);
	e.take(50)?;
	let mut config = Vec::new();
	for item in Boxes::new(e.data) {
		let (kind, data) = item?;
		if matches!(&kind, b"hvcC" | b"avcC") {
			config = data.to_vec();
			break;
		}
	}
	Ok((codec, width, height, config))
}

fn sample_sizes(stbl: &[u8]) -> Result<Vec<u32>> {
	let count = |b: &mut Bytes| -> Result<usize> {
		let n = u64::from(b.u32()?);
		if n > MAX_SAMPLES {
			return Err(Error::unsupported(format!("a track of more than {MAX_SAMPLES} frames")));
		}
		Ok(n as usize)
	};
	if let Some(stsz) = child(stbl, b"stsz")? {
		let mut b = Bytes::new(stsz, "stsz");
		b.take(4)?;
		let fixed = b.u32()?;
		let n = count(&mut b)?;
		if fixed != 0 {
			return Ok(vec![fixed; n]);
		}
		let table = b.take(n * 4)?;
		return Ok(table.as_chunks::<4>().0.iter().map(|c| u32::from_be_bytes(*c)).collect());
	}
	// The compact form: 4, 8 or 16 bits a sample.
	let mut b = Bytes::new(need(stbl, b"stz2", "stbl")?, "stz2");
	b.take(7)?;
	let bits = b.u8()?;
	let n = count(&mut b)?;
	let table = match bits {
		4 => b.take(n.div_ceil(2))?,
		8 => b.take(n)?,
		16 => b.take(n * 2)?,
		_ => return Err(Error::corrupt(format!("stz2 sizes of {bits} bits"))),
	};
	Ok(
		(0..n)
			.map(|i| match bits {
				4 => u32::from(table[i / 2] >> (4 - i % 2 * 4) & 0xF),
				8 => u32::from(table[i]),
				_ => u32::from(u16::from_be_bytes([table[i * 2], table[i * 2 + 1]])),
			})
			.collect(),
	)
}

/// Each sample's place in the file, from the chunk offsets and how many samples each chunk holds.
fn sample_offsets(stbl: &[u8], samples: &mut [Sample]) -> Result<()> {
	let chunks: Vec<u64> = if let Some(stco) = child(stbl, b"stco")? {
		let mut b = Bytes::new(stco, "stco");
		b.take(4)?;
		let n = b.u32()? as usize;
		b.take(n.checked_mul(4).ok_or_else(|| Error::corrupt("the stco box is cut short"))?)?
			.as_chunks::<4>()
			.0
			.iter()
			.map(|c| u64::from(u32::from_be_bytes(*c)))
			.collect()
	} else {
		let mut b = Bytes::new(need(stbl, b"co64", "stbl")?, "co64");
		b.take(4)?;
		let n = b.u32()? as usize;
		b.take(n.checked_mul(8).ok_or_else(|| Error::corrupt("the co64 box is cut short"))?)?.as_chunks::<8>().0.iter().map(|c| u64::from_be_bytes(*c)).collect()
	};
	let mut b = Bytes::new(need(stbl, b"stsc", "stbl")?, "stsc");
	b.take(4)?;
	let mut runs = Vec::new();
	for _ in 0..b.u32()? {
		let (first, per) = (b.u32()?, b.u32()?);
		b.u32()?; // which sample description: there's only one we read
		if first == 0 || runs.last().is_some_and(|&(prev, _)| first <= prev) {
			return Err(Error::corrupt("the stsc box's chunk numbers are out of order"));
		}
		runs.push((first, per));
	}
	let mut next = samples.iter_mut();
	'runs: for (i, &(first, per)) in runs.iter().enumerate() {
		let end = runs.get(i + 1).map_or(chunks.len() as u64 + 1, |r| u64::from(r.0));
		for chunk in u64::from(first)..end {
			let Some(&base) = chunks.get(chunk as usize - 1) else {
				return Err(Error::corrupt("the stsc box names a chunk the file doesn't have"));
			};
			let mut at = base;
			for _ in 0..per {
				let Some(s) = next.next() else {
					break 'runs;
				};
				s.offset = at;
				at = at.checked_add(u64::from(s.size)).ok_or_else(|| Error::corrupt("a chunk runs past 2^64 bytes"))?;
			}
		}
	}
	if next.next().is_some() {
		return Err(Error::corrupt("the chunks hold fewer frames than the stsz box lists"));
	}
	Ok(())
}

/// Each sample's decoding time, from runs of equal durations.
fn decode_times(stts: &[u8], samples: &mut [Sample]) -> Result<()> {
	let mut b = Bytes::new(stts, "stts");
	b.take(4)?;
	let mut next = samples.iter_mut();
	let mut t: i64 = 0;
	for _ in 0..b.u32()? {
		let (count, delta) = (b.u32()?, b.u32()?);
		for s in next.by_ref().take(count as usize) {
			s.time = t;
			t += i64::from(delta);
		}
	}
	if next.next().is_some() {
		return Err(Error::corrupt("the stts box gives fewer times than there are frames"));
	}
	Ok(())
}

/// Add each sample's composition offset: how much later than its decoding time it's shown.
fn composition_offsets(ctts: &[u8], samples: &mut [Sample]) -> Result<()> {
	let mut b = Bytes::new(ctts, "ctts");
	b.take(4)?;
	let mut next = samples.iter_mut();
	for _ in 0..b.u32()? {
		let count = b.u32()?;
		// Unsigned in version 0 and signed in version 1, but writers use version 0 for negative
		// offsets too, so both are read as signed, as ffmpeg does.
		let offset = b.u32()? as i32;
		for s in next.by_ref().take(count as usize) {
			s.time += i64::from(offset);
		}
	}
	Ok(())
}

/// Mark the keyframes. Without an `stss` box, every sample is one.
fn sync_samples(stss: &[u8], samples: &mut [Sample]) -> Result<()> {
	let mut b = Bytes::new(stss, "stss");
	b.take(4)?;
	for s in samples.iter_mut() {
		s.keyframe = false;
	}
	for _ in 0..b.u32()? {
		let n = b.u32()? as usize;
		let Some(s) = n.checked_sub(1).and_then(|i| samples.get_mut(i)) else {
			return Err(Error::corrupt(format!("the stss box names frame {n}, which doesn't exist")));
		};
		s.keyframe = true;
	}
	Ok(())
}

/// How much the edit list moves the track's times: empty edits delay it, and the first real
/// edit's start is time 0.
fn edit_shift(elst: &[u8], movie_scale: u32, timescale: u32) -> Result<i64> {
	let mut b = Bytes::new(elst, "elst");
	let version = b.u8()?;
	b.take(3)?;
	let mut empty: i128 = 0;
	for _ in 0..b.u32()? {
		let (duration, start) = if version == 1 { (b.u64()? as i128, b.u64()? as i64) } else { (b.u32()? as i128, i64::from(b.u32()? as i32)) };
		b.u32()?; // rate
		if start == -1 {
			empty += duration;
		} else {
			let delay = empty * i128::from(timescale) / i128::from(movie_scale);
			return i64::try_from(delay - i128::from(start)).map_err(|_| Error::corrupt("the edit list moves the video too far"));
		}
	}
	Ok(0)
}

/// Child boxes, as (type, contents).
struct Boxes<'a> {
	rest: &'a [u8],
}

impl<'a> Boxes<'a> {
	fn new(data: &'a [u8]) -> Self {
		Boxes { rest: data }
	}
}

impl<'a> Iterator for Boxes<'a> {
	type Item = Result<([u8; 4], &'a [u8])>;

	fn next(&mut self) -> Option<Self::Item> {
		// Fewer than 8 bytes can't be a box: padding some writers leave.
		let &[s0, s1, s2, s3, k0, k1, k2, k3] = self.rest.first_chunk::<8>()?;
		let kind = [k0, k1, k2, k3];
		let (size, header) = match u32::from_be_bytes([s0, s1, s2, s3]) {
			0 => (self.rest.len() as u64, 8),
			1 => match self.rest.get(8..16).and_then(|s| <[u8; 8]>::try_from(s).ok()) {
				Some(s) => (u64::from_be_bytes(s), 16),
				None => (0, 16),
			},
			n => (u64::from(n), 8),
		};
		if size < header || size > self.rest.len() as u64 {
			self.rest = &[];
			return Some(Err(Error::corrupt(format!("the {} box has a bad size", name(kind)))));
		}
		let (this, rest) = self.rest.split_at(size as usize);
		self.rest = rest;
		Some(Ok((kind, &this[header as usize..])))
	}
}

/// The first child box of this type.
fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Result<Option<&'a [u8]>> {
	for item in Boxes::new(data) {
		let (k, contents) = item?;
		if &k == kind {
			return Ok(Some(contents));
		}
	}
	Ok(None)
}

/// The first child box of this type, which `parent` must have.
fn need<'a>(data: &'a [u8], kind: &[u8; 4], parent: &str) -> Result<&'a [u8]> {
	child(data, kind)?.ok_or_else(|| Error::corrupt(format!("the {parent} box has no {} box", name(*kind))))
}

fn name(kind: [u8; 4]) -> String {
	kind.escape_ascii().to_string()
}

/// Big-endian fields, failing when the box runs out.
struct Bytes<'a> {
	data: &'a [u8],
	what: &'static str,
}

impl<'a> Bytes<'a> {
	fn new(data: &'a [u8], what: &'static str) -> Self {
		Bytes { data, what }
	}

	fn take(&mut self, n: usize) -> Result<&'a [u8]> {
		if self.data.len() < n {
			return Err(Error::corrupt(format!("the {} box is cut short", self.what)));
		}
		let (head, rest) = self.data.split_at(n);
		self.data = rest;
		Ok(head)
	}

	fn u8(&mut self) -> Result<u8> {
		Ok(self.take(1)?[0])
	}

	fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
		Ok(self.take(N)?.try_into().expect("take(n) returns exactly n bytes"))
	}

	fn u16(&mut self) -> Result<u16> {
		Ok(u16::from_be_bytes(self.array()?))
	}

	fn u32(&mut self) -> Result<u32> {
		Ok(u32::from_be_bytes(self.array()?))
	}

	fn u64(&mut self) -> Result<u64> {
		Ok(u64::from_be_bytes(self.array()?))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Cursor;

	fn bx(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
		let body = parts.concat();
		let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
		out.extend(kind);
		out.extend(body);
		out
	}

	/// A box with a version and flags.
	fn full(kind: &[u8; 4], version: u8, fields: &[u32]) -> Vec<u8> {
		let body: Vec<u8> = fields.iter().flat_map(|f| f.to_be_bytes()).collect();
		bx(kind, &[&[version, 0, 0, 0], &body])
	}

	fn hvcc() -> Vec<u8> {
		let mut c = vec![1];
		c.extend([0; 20]);
		c.push(0xF3); // 4-byte lengths
		c.push(2);
		c.extend([0xA0, 0, 1, 0, 3, 0x40, 0x01, 0x0C]); // a VPS
		c.extend([0xA1, 0, 2, 0, 2, 0x42, 0x01, 0, 1, 0x44]); // an SPS, and a 1-byte unit
		c
	}

	fn sample_entry(codec: &[u8; 4], config: &[u8]) -> Vec<u8> {
		let mut e = vec![0; 24];
		e.extend(640u16.to_be_bytes());
		e.extend(360u16.to_be_bytes());
		e.extend([0; 50]);
		bx(codec, &[&e, &bx(b"hvcC", &[config]), &[0, 0, 0, 0]])
	}

	/// Sample i holds `i + 1` repeated; the samples go in three chunks of 1, 3 and 2.
	const SIZES: [u32; 6] = [5, 3, 4, 6, 2, 7];
	const CHUNKS: [u32; 3] = [40, 60, 90];

	fn stbl(extra: &[Vec<u8>]) -> Vec<u8> {
		let mut parts = vec![
			bx(b"stsd", &[&[0; 4], &1u32.to_be_bytes(), &sample_entry(b"hvc1", &hvcc())]),
			full(b"stsz", 0, &[&[0, 6][..], &SIZES].concat()),
			full(b"stco", 0, &[&[3][..], &CHUNKS].concat()),
			full(b"stsc", 0, &[3, 1, 1, 1, 2, 3, 1, 3, 2, 1]),
			full(b"stts", 0, &[2, 4, 100, 2, 200]),
		];
		parts.extend(extra.iter().cloned());
		bx(b"stbl", &parts.iter().map(|p| &p[..]).collect::<Vec<_>>())
	}

	fn moov(stbl: &[u8], trak_extra: &[u8]) -> Vec<u8> {
		let mdhd = full(b"mdhd", 0, &[0, 0, 1000, 0]);
		let hdlr = full(b"hdlr", 0, &[0, u32::from_be_bytes(*b"vide"), 0, 0, 0]);
		let sound = bx(b"trak", &[&bx(b"mdia", &[&full(b"hdlr", 0, &[0, u32::from_be_bytes(*b"soun"), 0, 0, 0])])]);
		let mdia = bx(b"mdia", &[&mdhd, &hdlr, &bx(b"minf", &[stbl])]);
		let trak = bx(b"trak", &[trak_extra, &mdia]);
		bx(b"moov", &[&full(b"mvhd", 0, &[0, 0, 600, 0]), &sound, &trak])
	}

	/// An MP4 file with the index after the samples, as most are.
	fn file(moov: &[u8]) -> Vec<u8> {
		let mut f = bx(b"ftyp", &[b"isom", &[0; 4]]);
		f.extend(bx(b"mdat", &[&[0; 92]]));
		let mut i = 0;
		for (&at, n) in CHUNKS.iter().zip([1, 3, 2]) {
			let mut at = at as usize;
			for _ in 0..n {
				f[at..at + SIZES[i] as usize].fill(i as u8 + 1);
				at += SIZES[i] as usize;
				i += 1;
			}
		}
		f.extend(moov);
		f
	}

	fn standard() -> Vec<u8> {
		// Shown 2, 5, 1, 3, 4, 6 frames after decoding starts; the edit list starts at 100.
		let ctts = full(b"ctts", 0, &[4, 1, 200, 1, 400, 1, 0, 3, 100]);
		let stss = full(b"stss", 0, &[2, 1, 4]);
		let elst = full(b"elst", 0, &[2, 300, u32::MAX, 0x1_0000, 900, 100, 0x1_0000]);
		file(&moov(&stbl(&[ctts, stss]), &bx(b"edts", &[&elst])))
	}

	#[test]
	fn reads_the_video_track() {
		let f = standard();
		let video = Video::read(&mut Cursor::new(&f)).unwrap();
		assert_eq!((video.timescale, &video.codec, video.width, video.height), (1000, b"hvc1", 640, 360));
		let offsets: Vec<u64> = video.samples().iter().map(|s| s.offset).collect();
		assert_eq!(offsets, [40, 60, 63, 67, 90, 92]);
		// Decoded at 0, 100, 200, 300, 400, 600; plus ctts; less 100 for the edit's start, plus
		// 500 (300 at the movie's 600 a second) for the empty edit before it.
		let times: Vec<i64> = video.samples().iter().map(|s| s.time).collect();
		assert_eq!(times, [600, 900, 600, 800, 900, 1100]);
		let keys: Vec<bool> = video.samples().iter().map(|s| s.keyframe).collect();
		assert_eq!(keys, [true, false, false, true, false, false]);
		for (i, s) in video.samples().iter().enumerate() {
			assert_eq!(s.read(&mut Cursor::new(&f)).unwrap(), vec![i as u8 + 1; SIZES[i] as usize]);
		}
	}

	#[test]
	fn finds_keyframes() {
		let video = Video::read(&mut Cursor::new(standard())).unwrap();
		let at = |s: f64| video.keyframe_at(s).map(|k| k.offset);
		assert_eq!(at(0.0), Some(40), "before every keyframe: the first");
		assert_eq!(at(0.6), Some(40));
		assert_eq!(at(0.79), Some(40));
		assert_eq!(at(0.8), Some(67));
		assert_eq!(at(1e9), Some(67));
		assert_eq!(at(f64::NAN), Some(40));
	}

	#[test]
	fn reads_the_hevc_config_and_nal_units() {
		let video = Video::read(&mut Cursor::new(standard())).unwrap();
		let config = video.hevc().unwrap();
		assert_eq!(config.length_size, 4);
		assert_eq!(config.nal_units, [vec![0x40, 0x01, 0x0C], vec![0x42, 0x01], vec![0x44]]);
		let sample = [0, 0, 0, 2, 0x26, 0x01, 0, 0, 0, 0, 0, 0, 0, 1, 0x02];
		assert_eq!(nal_units(&sample, 4).unwrap(), [&[0x26, 0x01][..], &[0x02]]);
		assert_eq!(nal_units(&[0, 3, 1, 2], 2), Err(Error::corrupt("a NAL unit runs past the end of its frame")));
		assert!(nal_units(&[0, 0, 1], 4).is_err());
	}

	#[test]
	fn other_layouts() {
		// Every sample the same size, 64-bit chunk offsets, no ctts, stss or edit list, the
		// index first, and a 64-bit box size.
		let stbl = bx(
			b"stbl",
			&[
				&bx(b"stsd", &[&[0; 4], &1u32.to_be_bytes(), &sample_entry(b"hev1", &hvcc())]),
				&full(b"stsz", 0, &[4, 3]),
				&full(b"co64", 0, &[2, 0, 100, 1, 4]),
				&full(b"stsc", 0, &[1, 1, 2, 1]),
				&full(b"stts", 0, &[1, 3, 10]),
			],
		);
		let mut f = bx(b"ftyp", &[b"isom"]);
		f.extend(moov(&stbl, &[]));
		f.extend([0, 0, 0, 1]);
		f.extend(b"mdat");
		f.extend(16u64.to_be_bytes());
		let video = Video::read(&mut Cursor::new(&f)).unwrap();
		let got: Vec<(u64, u32, i64, bool)> = video.samples().iter().map(|s| (s.offset, s.size, s.time, s.keyframe)).collect();
		assert_eq!(got, [(100, 4, 0, true), (104, 4, 10, true), ((1 << 32) + 4, 4, 20, true)]);
		assert!(video.hevc().is_ok());

		// Compact sizes: 4 bits each.
		let stz2 = bx(b"stz2", &[&[0; 7], &[4], &3u32.to_be_bytes(), &[0x53, 0x40]]);
		let stbl = bx(
			b"stbl",
			&[
				&bx(b"stsd", &[&[0; 4], &1u32.to_be_bytes(), &sample_entry(b"avc1", &[])]),
				&stz2,
				&full(b"stco", 0, &[1, 50]),
				&full(b"stsc", 0, &[1, 1, 3, 1]),
				&full(b"stts", 0, &[1, 3, 10]),
			],
		);
		let video = Video::read(&mut Cursor::new(file(&moov(&stbl, &[])))).unwrap();
		let sizes: Vec<u32> = video.samples().iter().map(|s| s.size).collect();
		assert_eq!(sizes, [5, 3, 4]);
		assert!(matches!(video.hevc(), Err(Error::Unsupported(_))));
	}

	#[test]
	fn rejects_what_it_cant_read() {
		let unsupported = |f: &[u8]| matches!(Video::read(&mut Cursor::new(f)), Err(Error::Unsupported(_)));
		let mvex = bx(b"moov", &[&bx(b"mvex", &[])]);
		assert!(unsupported(&file(&mvex)), "fragmented");
		let audio = bx(b"moov", &[&full(b"mvhd", 0, &[0, 0, 600, 0])]);
		assert!(unsupported(&file(&audio)), "no video");

		let corrupt = |f: &[u8]| matches!(Video::read(&mut Cursor::new(f)), Err(Error::Corrupt(_)));
		assert!(corrupt(b"\x1a\x45\xdf\xa3 an MKV file"));
		assert!(corrupt(&[]));
		let f = standard();
		assert!(corrupt(&f[..f.len() - 1]), "cut short");
		let no_moov = &f[..f.len() - moov(&stbl(&[]), &[]).len() - 100];
		assert!(corrupt(no_moov));
		let bad_stss = file(&moov(&stbl(&[full(b"stss", 0, &[1, 7])]), &[]));
		assert!(corrupt(&bad_stss));
		let short_stts = full(b"stts", 0, &[1, 5, 100]);
		let stbl = stbl(&[]);
		let at = stbl.windows(4).position(|w| w == b"stts").unwrap() - 4;
		let old = full(b"stts", 0, &[2, 4, 100, 2, 200]).len();
		let stbl = [&stbl[..at], &short_stts, &stbl[at + old..]].concat();
		assert!(corrupt(&file(&moov(&bx(b"stbl", &[&stbl[8..]]), &[]))));
	}

	#[test]
	fn never_panics_on_damage() {
		let f = standard();
		let moov_at = f.windows(4).rposition(|w| w == b"moov").unwrap() - 4;
		for cut in 0..f.len() {
			#[expect(clippy::let_underscore_must_use, reason = "only checking it doesn't panic: any result is fine")]
			let _ = Video::read(&mut Cursor::new(&f[..cut]));
		}
		// Change each byte of the index in turn.
		for i in moov_at..f.len() {
			for x in [0x00, 0x01, 0x7F, 0x80, 0xFF] {
				let mut g = f.clone();
				g[i] = x;
				if let Ok(video) = Video::read(&mut Cursor::new(&g)) {
					let _ = video.keyframe_at(1.0);
					#[expect(clippy::let_underscore_must_use, reason = "only checking it doesn't panic: any result is fine")]
					let _ = video.hevc();
				}
			}
		}
	}
}
