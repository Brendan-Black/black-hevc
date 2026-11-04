//! Checks the MP4 reader against ffmpeg's. ffmpeg writes test videos in several layouts, and
//! for each one, every frame's place, size, time and keyframe flag must match what ffprobe
//! reports. Runs on Windows, where ffmpeg is installed; skips when ffmpeg isn't on PATH.

#![cfg(windows)]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use black_hevc::mp4::{Video, nal_units};
use black_hevc::nal::NalHeader;
use black_hevc::params::{ParamSets, Pps, Sps};
use black_hevc::picture::{self, Picture};
use black_hevc::slice::SliceHeader;
use black_hevc::slice_data::{CodingUnit, PictureParser, Sink, TransformBlock};
use black_hevc::still;
use black_hevc::{Error, Syntax};

fn have_ffmpeg() -> bool {
    let found = Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        eprintln!("skipped: no ffmpeg on PATH");
    }
    found
}

/// Make an 8-second test video with ffmpeg; `args` choose the codec and layout.
fn make(name: &str, args: &[&str]) -> PathBuf {
    let path = std::env::temp_dir().join(format!("black-hevc-{}-{name}.mp4", std::process::id()));
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=192x108:rate=24",
            "-t",
            "8",
        ])
        .args(args)
        .args(["-y".as_ref(), path.as_os_str()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffmpeg failed on {name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

fn ffprobe(path: &Path, entries: &str) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            entries,
            "-of",
            "compact=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The value of `key` in one line of ffprobe's `key=value|key=value` output.
fn field<'a>(line: &'a str, key: &str) -> &'a str {
    line.split('|')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
        .unwrap_or_else(|| panic!("no {key} in {line}"))
}

/// Compare every sample with ffprobe's packets, and return the video.
fn check(path: &Path) -> Video {
    let video = Video::read(&mut File::open(path).unwrap()).unwrap();
    let time_base = ffprobe(path, "stream=time_base");
    assert_eq!(
        field(time_base.trim(), "time_base"),
        format!("1/{}", video.timescale)
    );
    let packets = ffprobe(path, "packet=pts,dts,size,pos,flags");
    let want: Vec<(u64, u32, i64, bool)> = packets
        .lines()
        .map(|p| {
            (
                field(p, "pos").parse().unwrap(),
                field(p, "size").parse().unwrap(),
                field(p, "pts").parse().unwrap(),
                field(p, "flags").contains('K'),
            )
        })
        .collect();
    let got: Vec<(u64, u32, i64, bool)> = video
        .samples()
        .iter()
        .map(|s| (s.offset, s.size, s.time, s.keyframe))
        .collect();
    assert!(want.len() > 100, "{} packets", want.len());
    assert_eq!(got, want, "{}", path.display());
    video
}

/// Each HEVC keyframe holds an IRAP picture (NAL unit types 16 to 23), and no other frame does.
fn check_hevc_keyframes(path: &Path, video: &Video) {
    let config = video.hevc().unwrap();
    let types: Vec<u8> = config.nal_units.iter().map(|n| n[0] >> 1 & 0x3F).collect();
    assert!(
        types.contains(&32) && types.contains(&33) && types.contains(&34),
        "VPS, SPS and PPS: {types:?}"
    );
    let mut file = File::open(path).unwrap();
    for s in video.samples() {
        let data = s.read(&mut file).unwrap();
        let irap = nal_units(&data, config.length_size)
            .unwrap()
            .iter()
            .any(|n| (16..=23).contains(&(n[0] >> 1 & 0x3F)));
        assert_eq!(irap, s.keyframe, "the frame at {}", s.offset);
    }
}

#[test]
fn reads_what_ffmpeg_writes() {
    if !have_ffmpeg() {
        return;
    }
    let x265 = |params: &str| {
        ["-c:v", "libx265", "-x265-params"]
            .into_iter()
            .map(String::from)
            .chain([format!("log-level=error:{params}")])
            .collect::<Vec<_>>()
    };
    let cases: Vec<(&str, Vec<String>, &[&str])> = vec![
        // Open GOPs (CRA keyframes) and B-frames, index at the end: like the films BrendanTv plays.
        (
            "open-gop",
            x265("keyint=48:min-keyint=12:bframes=4"),
            &["-tag:v", "hvc1"],
        ),
        // Closed GOPs (IDR keyframes), index first, parameter sets also in the frames.
        (
            "closed-gop",
            x265("keyint=30:open-gop=0:bframes=2"),
            &["-tag:v", "hev1", "-movflags", "+faststart"],
        ),
        // Starting 1.5 seconds in: an empty edit before the video.
        (
            "delayed",
            x265("keyint=24"),
            &["-tag:v", "hvc1", "-output_ts_offset", "1.5"],
        ),
    ];
    for (name, codec, layout) in &cases {
        let args: Vec<&str> = codec
            .iter()
            .map(String::as_str)
            .chain(layout.iter().copied())
            .collect();
        let path = make(name, &args);
        let video = check(&path);
        assert_eq!((video.width, video.height), (192, 108));
        check_hevc_keyframes(&path, &video);
        let first_key = video
            .samples()
            .iter()
            .filter(|s| s.keyframe)
            .map(|s| s.time)
            .min()
            .unwrap();
        assert_eq!(video.keyframe_at(0.0).unwrap().time, first_key);
        let k = video.keyframe_at(5.0).unwrap();
        assert!(
            k.keyframe && k.time <= 5 * video.timescale as i64,
            "{name}: {k:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    // H.264: the samples read the same, but it isn't HEVC.
    let path = make("h264", &["-c:v", "libx264", "-g", "40"]);
    let video = check(&path);
    assert_eq!(&video.codec, b"avc1");
    assert!(matches!(video.hevc(), Err(Error::Unsupported(_))));
    let _ = std::fs::remove_file(&path);

    let path = make(
        "fragmented",
        &[
            "-c:v",
            "libx265",
            "-x265-params",
            "log-level=error",
            "-movflags",
            "frag_keyframe+empty_moov",
        ],
    );
    assert!(matches!(
        Video::read(&mut File::open(&path).unwrap()),
        Err(Error::Unsupported(_))
    ));
    let _ = std::fs::remove_file(&path);
}

/// Syntax elements as ffmpeg traces them: bit position, name, value.
type Trace = Vec<(usize, String, i64)>;

/// What ffmpeg's `trace_headers` filter reads from the first `frames` frames, by section
/// (such as "Sequence Parameter Set"): each syntax element's bit position, name and value.
fn ffmpeg_trace(path: &Path, frames: usize) -> Vec<(String, Trace)> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v",
            "-c",
            "copy",
            "-frames:v",
            &frames.min(1 << 30).to_string(),
        ])
        .args(["-bsf:v", "trace_headers", "-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ffmpeg's trace failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut sections: Vec<(String, Trace)> = Vec::new();
    for line in String::from_utf8_lossy(&out.stderr).lines() {
        let Some(rest) = line
            .strip_prefix("[trace_headers @ ")
            .and_then(|l| l.split_once("] "))
            .map(|(_, r)| r)
        else {
            continue;
        };
        let words: Vec<&str> = rest.split_whitespace().collect();
        match words[0].parse::<usize>() {
            Ok(pos) => sections.last_mut().unwrap().1.push((
                pos,
                words[1].to_string(),
                words.last().unwrap().parse().unwrap(),
            )),
            Err(_) => sections.push((rest.trim().to_string(), Vec::new())),
        }
    }
    sections
}

/// Every element we read must be one ffmpeg read at the same bit, with the same value, and we
/// must read every element ffmpeg did, except the profile's constraint flags, which we skip.
fn compare(what: &str, ours: &[Syntax], theirs: &[(usize, String, i64)]) {
    let base = |n: &str| n.split('[').next().unwrap().to_string();
    let by_pos: std::collections::HashMap<usize, &(usize, String, i64)> =
        theirs.iter().map(|t| (t.0, t)).collect();
    for s in ours {
        let Some(t) = by_pos.get(&s.pos) else {
            panic!(
                "{what}: we read {} at bit {}, where ffmpeg read nothing",
                s.name, s.pos
            )
        };
        assert_eq!(
            (base(&s.name), s.value),
            (base(&t.1), t.2),
            "{what}: at bit {}",
            s.pos
        );
    }
    let ours_at: std::collections::HashSet<usize> = ours.iter().map(|s| s.pos).collect();
    for t in theirs.iter().filter(|t| !ours_at.contains(&t.0)) {
        assert!(
            t.1.starts_with("general_") || t.1.starts_with("sub_layer_"),
            "{what}: ffmpeg read {} at bit {}, which we skipped",
            t.1,
            t.0
        );
    }
}

#[test]
fn parameter_sets_match_ffmpegs_trace() {
    if !have_ffmpeg() {
        return;
    }
    let x265 = |params: &str, pix: &str| -> Vec<String> {
        ["-pix_fmt", pix, "-c:v", "libx265", "-x265-params"]
            .into_iter()
            .map(String::from)
            .chain([format!("log-level=error:{params}")])
            .collect()
    };
    let hardware = |codec: &str, extra: &[&str]| -> Vec<String> {
        ["-c:v", codec]
            .iter()
            .chain(extra)
            .map(|s| s.to_string())
            .collect()
    };
    let cases: Vec<(&str, Vec<String>, bool)> = vec![
        ("x265", x265("", "yuv420p"), true),
        (
            "x265-tools",
            x265(
                "scaling-list=default:amp=1:rect=1:sao=0:signhide=0:strong-intra-smoothing=0:tskip=1:constrained-intra=1",
                "yuv420p",
            ),
            true,
        ),
        (
            "x265-sizes",
            x265(
                "ctu=32:min-cu-size=16:tu-intra-depth=3:tu-inter-depth=2:max-tu-size=16:wpp=0:cu-lossless=1",
                "yuv420p",
            ),
            true,
        ),
        (
            "x265-small-ctu",
            x265(
                "ctu=16:min-cu-size=8:max-tu-size=8:deblock=-2,3:cbqpoffs=3:crqpoffs=-4",
                "yuv420p",
            ),
            true,
        ),
        (
            "x265-vui",
            x265(
                "hrd=1:vbv-bufsize=2000:vbv-maxrate=1000:colorprim=bt709:transfer=bt709:colormatrix=bt709:range=full:sar=3:overscan=show:chromaloc=2:videoformat=pal",
                "yuv420p",
            ),
            true,
        ),
        ("x265-lossless", x265("lossless=1", "yuv420p"), true),
        ("x265-10bit", x265("", "yuv420p10le"), true),
        ("x265-422", x265("", "yuv422p10le"), true),
        ("x265-444", x265("", "yuv444p"), true),
        ("x265-gray", x265("", "gray"), true),
        ("nvenc", hardware("hevc_nvenc", &[]), false),
        (
            "nvenc-10bit",
            hardware("hevc_nvenc", &["-pix_fmt", "p010le"]),
            false,
        ),
        ("amf", hardware("hevc_amf", &[]), false),
        (
            "vulkan",
            hardware(
                "hevc_vulkan",
                &["-init_hw_device", "vulkan", "-vf", "format=nv12,hwupload"],
            ),
            false,
        ),
    ];
    for (name, args, required) in cases {
        let path =
            std::env::temp_dir().join(format!("black-hevc-{}-{name}.mp4", std::process::id()));
        let out = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=256x144:rate=24",
                "-t",
                "0.5",
            ])
            .args(&args)
            .args(["-y".as_ref(), path.as_os_str()])
            .output()
            .unwrap();
        if !out.status.success() {
            assert!(
                !required,
                "ffmpeg failed on {name}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            eprintln!(
                "skipped {name}: {}",
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .last()
                    .unwrap_or("")
            );
            continue;
        }
        let config = Video::read(&mut File::open(&path).unwrap())
            .unwrap()
            .hevc()
            .unwrap();
        let theirs = ffmpeg_trace(&path, 1);
        let section = |title: &str| {
            &theirs
                .iter()
                .find(|s| s.0 == title)
                .unwrap_or_else(|| panic!("{name}: no {title} in ffmpeg's trace"))
                .1
        };
        let nal = |kind: u8| {
            config
                .nal_units
                .iter()
                .find(|n| n[0] >> 1 & 0x3F == kind)
                .unwrap()
        };
        let (sps, trace) = Sps::trace(nal(33));
        let sps = sps.unwrap_or_else(|e| panic!("{name}: {e}"));
        compare(
            &format!("{name} SPS"),
            &trace,
            section("Sequence Parameter Set"),
        );
        let (pps, trace) = Pps::trace(nal(34));
        pps.unwrap_or_else(|e| panic!("{name}: {e}"));
        compare(
            &format!("{name} PPS"),
            &trace,
            section("Picture Parameter Set"),
        );
        let [left, right, top, bottom] = sps.crop;
        assert_eq!(
            (sps.width - left - right, sps.height - top - bottom),
            (256, 144),
            "{name}: the picture after cropping"
        );
        eprintln!(
            "{name}: {} SPS and {} PPS elements match",
            section("Sequence Parameter Set").len(),
            section("Picture Parameter Set").len()
        );
        let _ = std::fs::remove_file(&path);
    }
}

/// Compare the slice headers of the first `frames` frames with ffmpeg's; returns how many.
fn check_slices(name: &str, path: &Path, frames: usize) -> usize {
    let video = Video::read(&mut File::open(path).unwrap()).unwrap();
    let config = video.hevc().unwrap();
    let mut params = ParamSets::new();
    for nal in &config.nal_units {
        params.add(nal).unwrap();
    }
    let theirs: Vec<Trace> = ffmpeg_trace(path, frames)
        .into_iter()
        .filter(|s| s.0 == "Slice Segment Header")
        .map(|s| s.1)
        .collect();
    let mut file = File::open(path).unwrap();
    let mut ours = Vec::new();
    for s in video.samples().iter().take(frames) {
        let data = s.read(&mut file).unwrap();
        let mut previous: Option<SliceHeader> = None;
        for nal in nal_units(&data, config.length_size).unwrap() {
            let h = NalHeader::parse(nal).unwrap();
            if h.is_slice() && h.layer == 0 {
                let (header, trace) = SliceHeader::trace(nal, &params, previous.as_ref());
                previous =
                    Some(header.unwrap_or_else(|e| panic!("{name}: slice {}: {e}", ours.len())));
                ours.push(trace);
            } else {
                params.add(nal).unwrap(); // parameter sets sent with the frames
            }
        }
    }
    assert_eq!(ours.len(), theirs.len(), "{name}: slice count");
    for (i, (o, t)) in ours.iter().zip(&theirs).enumerate() {
        compare(&format!("{name} slice {i}"), o, t);
    }
    ours.len()
}

#[test]
fn slice_headers_match_ffmpegs_trace() {
    if !have_ffmpeg() {
        return;
    }
    let x265 = |params: &str| -> Vec<String> {
        ["-c:v", "libx265", "-x265-params"]
            .into_iter()
            .map(String::from)
            .chain([format!("log-level=error:{params}")])
            .collect()
    };
    let hardware = |codec: &str, extra: &[&str]| -> Vec<String> {
        ["-c:v", codec]
            .iter()
            .chain(extra)
            .map(|s| s.to_string())
            .collect()
    };
    let cases: Vec<(&str, Vec<String>, bool)> = vec![
        ("x265", x265("keyint=24:bframes=4"), true),
        (
            "x265-idr",
            x265("keyint=12:open-gop=0:bframes=0:ref=3"),
            true,
        ),
        (
            "x265-slices",
            x265("slices=3:weightb=1:max-merge=2:temporal-mvp=0"),
            true,
        ),
        (
            "x265-no-wpp",
            x265("wpp=0:sao=0:deblock=-1,1:b-pyramid=0:bframes=8:ref=5"),
            true,
        ),
        (
            "x265-headers",
            x265("repeat-headers=1:no-deblock=1:lossless=1"),
            true,
        ),
        (
            "x265-10bit",
            [
                &["-pix_fmt".to_string(), "yuv420p10le".to_string()][..],
                &x265("keyint=20"),
            ]
            .concat(),
            true,
        ),
        ("nvenc", hardware("hevc_nvenc", &["-bf", "3"]), false),
        ("amf", hardware("hevc_amf", &[]), false),
        (
            "vulkan",
            hardware(
                "hevc_vulkan",
                &["-init_hw_device", "vulkan", "-vf", "format=nv12,hwupload"],
            ),
            false,
        ),
    ];
    for (name, args, required) in cases {
        let path = std::env::temp_dir().join(format!(
            "black-hevc-{}-slices-{name}.mp4",
            std::process::id()
        ));
        let out = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=256x144:rate=24",
                "-t",
                "3",
            ])
            .args(&args)
            .args(["-y".as_ref(), path.as_os_str()])
            .output()
            .unwrap();
        if !out.status.success() {
            assert!(
                !required,
                "ffmpeg failed on {name}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            eprintln!(
                "skipped {name}: {}",
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .last()
                    .unwrap_or("")
            );
            continue;
        }
        let n = check_slices(name, &path, usize::MAX);
        eprintln!("{name}: {n} slice headers match");
        let _ = std::fs::remove_file(&path);
    }
}

/// The same for a video of your own: `BLACK_HEVC_VIDEO=<path> ... --ignored`, first 1000 frames.
#[test]
#[ignore]
fn slice_headers_of_a_real_video() {
    let path = std::env::var("BLACK_HEVC_VIDEO").expect("set BLACK_HEVC_VIDEO to a video's path");
    let n = check_slices("the video", Path::new(&path), 1000);
    eprintln!("{n} slice headers match");
}

#[derive(Default)]
struct Coverage {
    cu_area: u64,
    luma_area: u64,
    blocks: u64,
}

impl Sink for Coverage {
    fn transform_block(&mut self, tb: &TransformBlock) {
        self.blocks += 1;
        if tb.c_idx == 0 {
            self.luma_area += 1 << (2 * tb.log2_size);
        }
    }

    fn coding_unit(&mut self, cu: &CodingUnit) {
        self.cu_area += 1 << (2 * cu.log2_size);
    }
}

/// Each NAL unit byte's index in the RBSP.
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

/// Read every keyframe's slice data: each slice must end at its last byte, each wavefront row
/// start at its entry point, and the coding units and luma blocks cover the picture once.
/// Returns how many keyframes and transform blocks.
fn check_keyframes(name: &str, path: &Path) -> (usize, u64) {
    let video = Video::read(&mut File::open(path).unwrap()).unwrap();
    let config = video.hevc().unwrap();
    let mut params = ParamSets::new();
    for nal in &config.nal_units {
        params.add(nal).unwrap();
    }
    let mut file = File::open(path).unwrap();
    let (mut pictures, mut blocks) = (0, 0);
    for key in video.samples().iter().filter(|s| s.keyframe) {
        let data = key.read(&mut file).unwrap();
        let units = nal_units(&data, config.length_size).unwrap();
        for nal in &units {
            if !NalHeader::parse(nal).unwrap().is_slice() {
                params.add(nal).unwrap();
            }
        }
        let mut sink = Coverage::default();
        let mut picture: Option<PictureParser> = None;
        let mut previous: Option<SliceHeader> = None;
        for nal in units {
            let h = NalHeader::parse(nal).unwrap();
            if !h.is_slice() || h.layer != 0 {
                continue;
            }
            let header = SliceHeader::parse(nal, &params, previous.as_ref()).unwrap();
            let (pps, sps) = params.get(u32::from(header.pps_id)).unwrap();
            let parser = picture.get_or_insert_with(|| PictureParser::new(sps, pps).unwrap());
            let rbsp = black_hevc::rbsp(nal);
            let stats = parser
                .slice(&header, &rbsp, &mut sink)
                .unwrap_or_else(|e| panic!("{name}: keyframe at {}: {e}", key.offset));
            let map = rbsp_of_nal(nal);
            let start = map.iter().position(|&r| r == header.data_offset).unwrap();
            let want: Vec<usize> = std::iter::once(header.data_offset)
                .chain(header.entry_points.iter().map(|&e| map[start + e as usize]))
                .collect();
            assert_eq!(stats.substreams, want, "{name}: wavefront rows");
            previous = Some(header);
        }
        let (_, sps) = params.get(u32::from(previous.unwrap().pps_id)).unwrap();
        let area = u64::from(sps.width) * u64::from(sps.height);
        assert!(picture.unwrap().complete(), "{name}: every CTB");
        assert_eq!(
            (sink.cu_area, sink.luma_area),
            (area, area),
            "{name}: coverage"
        );
        pictures += 1;
        blocks += sink.blocks;
    }
    (pictures, blocks)
}

/// The encoder setups for the slice data and pixel checks: name, ffmpeg arguments, and whether
/// the encoder must be there (the hardware ones may not be, or may refuse the tiny size).
fn encoder_cases() -> Vec<(&'static str, Vec<String>, bool)> {
    let x265 = |params: &str, pix: &str| -> Vec<String> {
        ["-pix_fmt", pix, "-c:v", "libx265", "-x265-params"]
            .into_iter()
            .map(String::from)
            .chain([format!("log-level=error:keyint=4:{params}")])
            .collect()
    };
    let hardware = |codec: &str, extra: &[&str]| -> Vec<String> {
        ["-c:v", codec, "-g", "4"]
            .iter()
            .chain(extra)
            .map(|s| s.to_string())
            .collect()
    };
    vec![
        ("x265", x265("", "yuv420p"), true),
        (
            "slices",
            x265("open-gop=0:slices=3:signhide=0:tskip=1", "yuv420p"),
            true,
        ),
        ("no-wpp", x265("wpp=0:open-gop=0", "yuv420p"), true),
        (
            "small-blocks",
            x265(
                "ctu=16:min-cu-size=8:tu-intra-depth=3:tu-inter-depth=1:max-tu-size=8",
                "yuv420p",
            ),
            true,
        ),
        (
            "quant-groups",
            x265(
                "ctu=32:qg-size=8:aq-mode=3:aq-strength=2:rdoq-level=2:psy-rdoq=4",
                "yuv420p",
            ),
            true,
        ),
        ("lossless", x265("lossless=1", "yuv420p"), true),
        ("cu-lossless", x265("cu-lossless=1:crf=10", "yuv420p"), true),
        (
            "scaling-lists",
            x265("scaling-list=default:sao=0:crf=5", "yuv420p"),
            true,
        ),
        (
            "chroma-offsets",
            x265("cbqpoffs=-5:crqpoffs=7:tskip=1:crf=8", "yuv420p"),
            true,
        ),
        (
            "no-strong-smoothing",
            x265("strong-intra-smoothing=0:crf=35", "yuv420p"),
            true,
        ),
        ("422", x265("tskip=1", "yuv422p"), true),
        ("444", x265("tu-intra-depth=3:cbqpoffs=3", "yuv444p"), true),
        ("gray", x265("", "gray"), true),
        ("10bit", x265("crf=2", "yuv420p10le"), true),
        ("444-10bit", x265("", "yuv444p10le"), true),
        ("nvenc", hardware("hevc_nvenc", &[]), false),
        ("amf", hardware("hevc_amf", &[]), false),
        (
            "vulkan",
            hardware(
                "hevc_vulkan",
                &[
                    "-init_hw_device",
                    "vulkan",
                    "-vf",
                    "noise=alls=30:allf=t,format=nv12,hwupload",
                ],
            ),
            false,
        ),
    ]
}

/// Encode a second of noisy test pattern at 200x120, or `None` if an optional encoder failed.
/// (The GPU uploads add their own noise.)
fn encode_case(name: &str, args: &[String], required: bool) -> Option<PathBuf> {
    let gpu = args.iter().any(|a| a.contains("hwupload"));
    encode(
        name,
        args,
        required,
        (!gpu).then_some("noise=alls=30:allf=t"),
    )
}

/// [`encode_case`], with the test pattern through `filter` if there is one.
fn encode(name: &str, args: &[String], required: bool, filter: Option<&str>) -> Option<PathBuf> {
    let path =
        std::env::temp_dir().join(format!("black-hevc-{}-keys-{name}.mp4", std::process::id()));
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=200x120:rate=24",
            "-t",
            "1",
        ])
        .args(filter.map(|f| ["-vf", f]).iter().flatten())
        .args(args)
        .args(["-y".as_ref(), path.as_os_str()])
        .output()
        .unwrap();
    if !out.status.success() {
        assert!(
            !required,
            "ffmpeg failed on {name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        eprintln!(
            "skipped {name}: {}",
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .unwrap_or("")
        );
        return None;
    }
    Some(path)
}

#[test]
fn keyframes_read_to_the_end() {
    if !have_ffmpeg() {
        return;
    }
    for (name, args, required) in encoder_cases() {
        let Some(path) = encode_case(name, &args, required) else {
            continue;
        };
        let (pictures, blocks) = check_keyframes(name, &path);
        assert!(pictures >= 4, "{name}: {pictures} keyframes");
        eprintln!("{name}: {pictures} keyframes, {blocks} transform blocks, read to the end");
        let _ = std::fs::remove_file(&path);
    }
}

/// ffmpeg's pixel format for a picture's layout.
fn pix_fmt(pic: &Picture) -> String {
    let base = ["gray", "yuv420p", "yuv422p", "yuv444p"][pic.chroma_format as usize];
    match pic.bit_depth_luma {
        8 => base.to_string(),
        d => format!("{base}{d}le"),
    }
}

/// Decode every keyframe, and write each one out as a raw HEVC stream of its own for ffmpeg (a
/// CRA marked as a BLA, so it starts a new sequence). `check` gets the keyframe's number, our
/// picture and the stream's path. Returns how many keyframes.
fn each_keyframe(name: &str, path: &Path, mut check: impl FnMut(usize, &Picture, &Path)) -> usize {
    let video = Video::read(&mut File::open(path).unwrap()).unwrap();
    let config = video.hevc().unwrap();
    let mut params = ParamSets::new();
    for nal in &config.nal_units {
        params.add(nal).unwrap();
    }
    let mut file = File::open(path).unwrap();
    let mut count = 0;
    for key in video.samples().iter().filter(|s| s.keyframe) {
        let data = key.read(&mut file).unwrap();
        let units = nal_units(&data, config.length_size).unwrap();
        let pic = picture::decode(&mut params, &units)
            .unwrap_or_else(|e| panic!("{name}: keyframe {count}: {e}"));
        let mut stream = Vec::new();
        for nal in config.nal_units.iter().map(Vec::as_slice).chain(units) {
            stream.extend([0, 0, 0, 1]);
            if nal[0] >> 1 == 21 {
                stream.push((nal[0] & 0x81) | 17 << 1);
                stream.extend(&nal[1..]);
            } else {
                stream.extend(nal);
            }
        }
        let hevc = path.with_extension(format!("{count}.hevc"));
        std::fs::write(&hevc, stream).unwrap();
        check(count, &pic, &hevc);
        let _ = std::fs::remove_file(&hevc);
        count += 1;
    }
    count
}

/// ffmpeg's decode of one keyframe's stream, with its loop filters skipped (ours aren't done
/// yet), through `filter` if there is one, as `pix_fmt` raw video.
fn ffmpeg_decode(name: &str, hevc: &Path, filter: Option<&str>, pix_fmt: &str) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(["-skip_loop_filter", "all", "-i"])
        .arg(hevc)
        .args(filter.map(|f| ["-vf", f]).iter().flatten())
        .args(["-f", "rawvideo", "-pix_fmt", pix_fmt, "-"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{name}: ffmpeg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Every keyframe's shown samples must match ffmpeg's exactly. Returns how many keyframes.
fn check_pixels(name: &str, path: &Path) -> usize {
    each_keyframe(name, path, |count, pic, hevc| {
        let out = ffmpeg_decode(name, hevc, None, &pix_fmt(pic));
        let mut theirs = out.as_slice();
        for c in 0..pic.planes.len() {
            let (w, h, ours) = pic.cropped(c);
            let wide = if c == 0 {
                pic.bit_depth_luma
            } else {
                pic.bit_depth_chroma
            } > 8;
            let size = ours.len() * if wide { 2 } else { 1 };
            assert!(
                theirs.len() >= size,
                "{name}: keyframe {count}: ffmpeg's is short"
            );
            let (plane, rest) = theirs.split_at(size);
            theirs = rest;
            for (i, &s) in ours.iter().enumerate() {
                let t = if wide {
                    u16::from_le_bytes([plane[2 * i], plane[2 * i + 1]])
                } else {
                    u16::from(plane[i])
                };
                assert_eq!(
                    s,
                    t,
                    "{name}: keyframe {count}, plane {c} of {w}x{h} at ({}, {})",
                    i % w as usize,
                    i / w as usize
                );
            }
        }
        assert!(
            theirs.is_empty(),
            "{name}: ffmpeg decoded more than one picture"
        );
    })
}

/// Every keyframe as RGB, fitting in `max` by `max`, must be close to ffmpeg's conversion with
/// the same matrix and range, chroma placed where H.265 puts it by default (ffmpeg would
/// centre it across) and its bilinear scaling: returns the largest difference in any channel
/// of any pixel, and the mean difference, over all keyframes.
fn check_colours(name: &str, path: &Path, max: u32) -> (u8, f64) {
    let (mut largest, mut total, mut n) = (0, 0u64, 0u64);
    each_keyframe(name, path, |count, pic, hevc| {
        let ours = still::rgb(pic, max);
        let matrix = match pic.colour.matrix {
            1 => "bt709",
            4 => "fcc",
            5 | 6 => "bt601",
            7 => "smpte240m",
            9 | 10 => "bt2020",
            _ if pic.height >= 720 => "bt709",
            _ => "bt601",
        };
        let range = if pic.colour.full_range { "pc" } else { "tv" };
        let filter = format!(
            "scale={}:{}:flags=bilinear+accurate_rnd+full_chroma_int:in_color_matrix={matrix}:in_range={range}:in_chroma_loc=left",
            ours.width, ours.height
        );
        let theirs = ffmpeg_decode(name, hevc, Some(&filter), "rgb24");
        assert_eq!(
            theirs.len(),
            ours.pixels.len(),
            "{name}: keyframe {count}: sizes differ"
        );
        for (i, (&a, &b)) in ours.pixels.iter().zip(&theirs).enumerate() {
            let d = a.abs_diff(b);
            if d > largest {
                largest = d;
                eprintln!(
                    "{name}: keyframe {count}: {d} at ({}, {}) channel {}: {a}, ffmpeg {b}",
                    i / 3 % ours.width as usize,
                    i / 3 / ours.width as usize,
                    i % 3
                );
            }
            total += u64::from(d);
        }
        n += theirs.len() as u64;
    });
    (largest, total as f64 / n as f64)
}

#[test]
fn stills_match_ffmpegs_colours() {
    if !have_ffmpeg() {
        return;
    }
    // Without noise, which makes any two ways of interpolating chroma differ a lot.
    let cases = [
        ("rgb-420", "yuv420p", ""),
        ("rgb-422", "yuv422p", ""),
        ("rgb-444", "yuv444p", ""),
        ("rgb-gray", "gray", ""),
        ("rgb-10bit", "yuv420p10le", ""),
        (
            "rgb-full-709",
            "yuv420p",
            "-color_range pc -colorspace bt709",
        ),
        ("rgb-2020", "yuv420p10le", "-colorspace bt2020nc"),
        ("rgb-gbr", "gbrp", ""),
    ];
    for (name, pix, colour) in cases {
        let args: Vec<String> = ["-pix_fmt", pix, "-c:v", "libx265", "-x265-params"]
            .into_iter()
            .chain(["log-level=error:keyint=4:crf=8"])
            .chain(colour.split_whitespace())
            .map(String::from)
            .collect();
        let path = encode(name, &args, true, None).unwrap();
        // Full size (chroma scaled up), then scaled down.
        for max in [0, 90] {
            let (largest, mean) = check_colours(name, &path, max);
            eprintln!("{name} at {max}: differ by {largest} at most, {mean:.3} on average");
            // ffmpeg scales planar RGB its own way; 4:4:4 YUV checks our scaling instead.
            if !(name == "rgb-gbr" && max > 0) {
                assert!(largest <= 1, "{name} at {max}: differ by {largest}");
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn keyframes_match_ffmpegs_pixels() {
    if !have_ffmpeg() {
        return;
    }
    for (name, args, required) in encoder_cases() {
        let Some(path) = encode_case(name, &args, required) else {
            continue;
        };
        let pictures = check_pixels(name, &path);
        assert!(pictures >= 4, "{name}: {pictures} keyframes");
        eprintln!("{name}: {pictures} keyframes match ffmpeg's");
        let _ = std::fs::remove_file(&path);
    }
}

/// With BLACK_HEVC_VIDEO set to a video of your own: every keyframe matches ffmpeg's.
#[test]
#[ignore]
fn pixels_of_a_real_video() {
    let path =
        std::env::var_os("BLACK_HEVC_VIDEO").expect("set BLACK_HEVC_VIDEO to a video's path");
    let n = check_pixels("the video", Path::new(&path));
    eprintln!("{n} keyframes match");
}

/// With BLACK_HEVC_VIDEO set to a video of your own: its stills are close to ffmpeg's.
#[test]
#[ignore]
fn stills_of_a_real_video() {
    let path =
        std::env::var_os("BLACK_HEVC_VIDEO").expect("set BLACK_HEVC_VIDEO to a video's path");
    for max in [0, 320] {
        let (largest, mean) = check_colours("the video", Path::new(&path), max);
        eprintln!("at {max}: differ by {largest} at most, {mean:.3} on average");
        assert!(largest <= 1, "at {max}: differ by {largest}");
    }
}

/// The C interface's PNG, in memory and as a file, holds the same pixels as `still::rgb`.
#[test]
fn c_interface_gives_the_still() {
    use black_hevc::ffi::{OK, black_hevc_free, black_hevc_still, black_hevc_still_file};
    use std::ffi::CString;
    if !have_ffmpeg() {
        return;
    }
    let args: Vec<String> = [
        "-c:v",
        "libx265",
        "-x265-params",
        "log-level=error:keyint=12",
    ]
    .map(String::from)
    .into();
    let path = encode("c-interface", &args, true, None).unwrap();
    let video = CString::new(path.to_str().unwrap()).unwrap();
    let (mut png, mut len) = (std::ptr::null_mut(), 0);
    let code = unsafe {
        black_hevc_still(
            video.as_ptr(),
            0.6,
            90,
            &mut png,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    assert_eq!(code, OK);
    let bytes = unsafe { std::slice::from_raw_parts(png, len) }.to_vec();
    unsafe { black_hevc_free(png, len) };
    let out = path.with_extension("png");
    let out_c = CString::new(out.to_str().unwrap()).unwrap();
    let code = unsafe {
        black_hevc_still_file(
            video.as_ptr(),
            0.6,
            90,
            out_c.as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    assert_eq!(code, OK);
    assert_eq!(std::fs::read(&out).unwrap(), bytes);

    // The keyframe at or before 0.6 s: with one every 12 frames at 24 a second, the second.
    let mut file = File::open(&path).unwrap();
    let mp4 = Video::read(&mut file).unwrap();
    let key = mp4.keyframe_at(0.6).unwrap();
    assert!(key.time > 0);
    let config = mp4.hevc().unwrap();
    let mut params = ParamSets::new();
    for nal in &config.nal_units {
        params.add(nal).unwrap();
    }
    let data = key.read(&mut file).unwrap();
    let pic = picture::decode(&mut params, &nal_units(&data, config.length_size).unwrap()).unwrap();
    let rgb = still::rgb(&pic, 90);
    let image = black_png::decode(&bytes, 1 << 20).unwrap();
    assert_eq!((image.width, image.height), (rgb.width, rgb.height));
    assert_eq!(image.pixels, rgb.pixels);
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&path);
}

/// Damaged keyframes (flipped bits, overwritten runs of bytes, cut-off NAL units, in the
/// parameter sets as well as the slices) decode to a picture or an error: never a panic, and
/// never slowly. Run it in a debug build too, where arithmetic overflow panics.
#[test]
fn damaged_keyframes_never_panic() {
    if !have_ffmpeg() {
        return;
    }
    const TRIALS: usize = 400;
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut random = move |below: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % below.max(1) as u64) as usize
    };
    let cases = [
        "x265",
        "slices",
        "small-blocks",
        "quant-groups",
        "cu-lossless",
        "scaling-lists",
        "422",
        "gray",
        "444-10bit",
    ];
    let mut panics = Vec::new();
    let mut decoded = 0;
    for (name, args, _) in encoder_cases().into_iter().filter(|c| cases.contains(&c.0)) {
        let path = encode_case(name, &args, true).unwrap();
        let mut file = File::open(&path).unwrap();
        let video = Video::read(&mut file).unwrap();
        let config = video.hevc().unwrap();
        for key in video.samples().iter().filter(|s| s.keyframe).take(2) {
            let data = key.read(&mut file).unwrap();
            let units = nal_units(&data, config.length_size).unwrap();
            let nals: Vec<Vec<u8>> = config
                .nal_units
                .iter()
                .cloned()
                .chain(units.iter().map(|n| n.to_vec()))
                .collect();
            for trial in 0..TRIALS {
                let mut damaged = nals.clone();
                let nal = &mut damaged[random(nals.len())];
                let how = random(3);
                match how {
                    0 => {
                        for _ in 0..1 + random(4) {
                            let at = random(nal.len());
                            nal[at] ^= 1 << random(8);
                        }
                    }
                    1 => {
                        let at = random(nal.len());
                        let end = (at + 1 + random(16)).min(nal.len());
                        for b in &mut nal[at..end] {
                            *b = random(256) as u8;
                        }
                    }
                    _ => nal.truncate(random(nal.len())),
                }
                let refs: Vec<&[u8]> = damaged.iter().map(Vec::as_slice).collect();
                let start = std::time::Instant::now();
                let result = std::panic::catch_unwind(|| {
                    if let Ok(pic) = picture::decode(&mut ParamSets::new(), &refs) {
                        still::rgb(&pic, 90);
                        true
                    } else {
                        false
                    }
                });
                match result {
                    Ok(true) => decoded += 1,
                    Ok(false) => {}
                    Err(e) => panics.push(format!(
                        "{name}, keyframe at {}, trial {trial} (damage {how}): {}",
                        key.time,
                        e.downcast_ref::<String>()
                            .cloned()
                            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                            .unwrap_or_default()
                    )),
                }
                let took = start.elapsed();
                assert!(took.as_secs() < 5, "{name}, trial {trial}: took {took:?}");
            }
        }
        let _ = std::fs::remove_file(&path);
    }
    eprintln!(
        "{decoded} of {} damaged keyframes still decoded; {} panicked",
        cases.len() * 2 * TRIALS,
        panics.len()
    );
    assert!(panics.is_empty(), "{}", panics.join("\n"));
}
