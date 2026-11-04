# black-hevc

HEVC (H.265) keyframes from MP4 files, with no third-party dependencies. The goal is a still
picture from any point in a video, for BrendanTv's thumbnails, without ffmpeg.

## So far

`still::still` makes a thumbnail: the last keyframe at or before a time, as RGB scaled to fit a
box, as a PNG (via black-png, our own sibling crate).

```rust
let png = black_hevc::still::still(Path::new("episode.mp4"), 240.0, 320)?; // fits in 320x320
```

Colours follow the video's colour description (BT.709, BT.601, BT.2020 and the rest, limited
or full range); a video that doesn't say gets BT.709 if it's 720 lines or more, else BT.601.
Chroma sits where H.265 puts it by default (with the left luma sample across). Scaling uses a
tent filter as wide as an output pixel. `still::rgb` does the conversion from a `Picture`.
Every pixel is within one level of ffmpeg's conversion (given the same matrix, range and chroma
placement), at full size and scaled. black-png and black-zlib must be cloned next to this repo.

For other languages, `cargo build --release` also builds a C library (`black_hevc.dll`,
`libblack_hevc.so`) declared in `include/black_hevc.h`: `black_hevc_still_file` writes the
still to a PNG file, and `black_hevc_still` returns it in memory (given back with
`black_hevc_free`). Each returns a status code and fills a caller's buffer with a message on
failure; panics never cross into the caller. `examples/java/Still.java` calls it through
Java's FFM API:

```sh
java --enable-native-access=ALL-UNNAMED examples/java/Still.java black_hevc.dll episode.mp4 240 still.png
```

`picture::decode` turns a keyframe's NAL units into a picture: Y, Cb and Cr planes at the
coded size, with the crop to apply. It does intra prediction (planar, DC and the 33 angles,
with reference smoothing), scaling with or without scaling lists, the inverse DCT and DST,
transform skip, lossless blocks and PCM, for 4:0:0 to 4:4:4. Tested against ffmpeg at 8 and
10 bits; PCM is untested, since no encoder here writes it. The deblocking
and SAO filters aren't done yet, so block edges can show; ffmpeg gives the same picture with
`-skip_loop_filter all`.

```rust
let mut params = ParamSets::new();
for nal in &config.nal_units { params.add(nal)?; }
let picture = black_hevc::picture::decode(&mut params, &nal_units(&data, config.length_size)?)?;
let (width, height, luma) = picture.cropped(0);
```

`slice` reads slice segment headers (I, P and B), and where each slice's data starts.

`slice_data` reads intra pictures' slice data with CABAC: SAO parameters, coding and transform
trees, intra modes, QPs and coefficients, handed to a `Sink` in decoding order. Tiles and the
range extension's coding tools are reported as unsupported.

`params` reads the sequence and picture parameter sets (SPS and PPS): picture size, bit depth,
block sizes, coding tools, scaling lists, reference picture sets, tiles, and the colour
description. Each can be traced element by element, for comparison with ffmpeg.

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
2. HEVC parameter sets and slice headers *(done)*
3. CABAC: the decoding engine, and the slice data of intra pictures *(done)*
4. Intra prediction and the inverse transforms: a YUV picture *(done)*
5. RGB, scaled, as a PNG (black-png), and a C interface for BrendanTv to call through Java's
   FFM API *(done)*
6. The deblocking and SAO filters

Only keyframes are decoded, since a thumbnail doesn't need the exact frame.

## Tests

```sh
cargo test
winrun --test   # also checks every frame against ffprobe, and every SPS and PPS syntax
                # element against ffmpeg's trace_headers, on Windows with ffmpeg on PATH;
                # every slice header too, and every keyframe's pixels against ffmpeg's
                # (loop filters off), and stills' colours against ffmpeg's. For a video of
                # your own (its slice headers, every keyframe's pixels, and its stills):
                # BLACK_HEVC_VIDEO=<path> <test exe> --ignored
cargo run --release --example keyframe -- video.mp4 240
cargo run --release --example params -- video.mp4
cargo run --release --example keyframes -- video.mp4 20   # read 20 keyframes to the end
cargo run --release --example decode -- video.mp4 out.yuv 20   # 20 keyframes as raw YUV
cargo run --release --example still -- video.mp4 240 still.png 320   # the still at 4:00
```
