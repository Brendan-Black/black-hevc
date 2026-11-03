//! Checks the MP4 reader against ffmpeg's. ffmpeg writes test videos in several layouts, and
//! for each one, every frame's place, size, time and keyframe flag must match what ffprobe
//! reports. Runs on Windows, where ffmpeg is installed; skips when ffmpeg isn't on PATH.

#![cfg(windows)]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use black_hevc::Error;
use black_hevc::mp4::{Video, nal_units};

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
