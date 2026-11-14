//! Print the keyframe a still would come from: `keyframe <video.mp4> <seconds>`.

use std::fs::File;
use std::time::Instant;

use black_hevc::mp4::Video;

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let args: Vec<String> = std::env::args().collect();
	let [_, path, seconds] = &args[..] else {
		return Err("usage: keyframe <video.mp4> <seconds>".into());
	};
	let start = Instant::now();
	let video = Video::read(&mut File::open(path)?)?;
	let took = start.elapsed();
	let keys = video.samples().iter().filter(|s| s.keyframe).count();
	println!("{} {}x{}, {} frames, {keys} keyframes, index read in {took:?}", video.codec.escape_ascii(), video.width, video.height, video.samples().len());
	let k = video.keyframe_at(seconds.parse()?).ok_or("no keyframes")?;
	println!("keyframe at {:.3} s: {} bytes at offset {}", k.time as f64 / f64::from(video.timescale), k.size, k.offset);
	Ok(())
}
