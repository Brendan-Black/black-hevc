//! Write a video's still as a PNG: `still <video.mp4> <seconds> <out.png> [max]`, fitting in
//! `max` by `max` pixels (default 320; 0 for full size).

use std::path::Path;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let (Some(video), Some(seconds), Some(out)) = (args.get(1), args.get(2), args.get(3)) else {
        return Err("usage: still <video.mp4> <seconds> <out.png> [max]".into());
    };
    let max = args.get(4).map_or(Ok(320), |m| m.parse())?;
    let start = Instant::now();
    let png = black_hevc::still::still(Path::new(video), seconds.parse()?, max)?;
    let took = start.elapsed();
    std::fs::write(out, &png)?;
    println!("{} bytes in {took:?}", png.len());
    Ok(())
}
