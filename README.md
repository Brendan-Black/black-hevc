# black-hevc

HEVC (H.265) keyframes from MP4 files, with no third-party dependencies. The goal is a still
picture from any point in a video, for BrendanTv's thumbnails, without ffmpeg.

## So far

`mp4` reads the first video track's index: every frame's place, size, time and whether it's a
keyframe, plus the HEVC decoder configuration (`hvcC`). Only the index is read into memory, so
a multi-gigabyte file with its index at the end costs a few milliseconds.

```rust
use black_hevc::mp4::{Video, nal_units};

let mut file = std::fs::File::open("episode.mp4")?;
let video = Video::read(&mut file)?;
let key = video.keyframe_at(240.0).unwrap(); // the last keyframe at or before 4:00
let config = video.hevc()?;                  // VPS, SPS and PPS
let data = key.read(&mut file)?;
for nal in nal_units(&data, config.length_size)? { /* ... */ }
```

Fragmented MP4 and encrypted video are reported as unsupported.

## Plan

1. MP4: find the keyframe *(done)*
2. HEVC parameter sets and slice headers
3. CABAC: the slice data
4. Intra prediction and the inverse transforms: a YUV picture
5. RGB, scaled, as a PNG (black-png); a C interface for BrendanTv to call through Java's FFM API
6. The deblocking and SAO filters

Only keyframes are decoded, since a thumbnail doesn't need the exact frame.

## Tests

```sh
cargo test
winrun --test   # also checks every frame against ffprobe, on Windows with ffmpeg on PATH
cargo run --release --example keyframe -- video.mp4 240
```
