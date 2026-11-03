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
use black_hevc::slice::SliceHeader;
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
