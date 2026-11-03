//! Print a video's HEVC parameter sets: `params <video.mp4>...`, or `params --hex <video.mp4>`
//! for their bytes.

use std::fs::File;

use black_hevc::mp4::Video;
use black_hevc::params::{Pps, Sps};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let hex = args.first().is_some_and(|a| a == "--hex");
    if hex {
        args.remove(0);
    }
    if args.is_empty() {
        return Err("usage: params [--hex] <video.mp4>...".into());
    }
    for path in &args {
        let config = Video::read(&mut File::open(path)?)?.hevc()?;
        for nal in &config.nal_units {
            let kind = nal[0] >> 1 & 0x3F;
            if hex {
                let bytes: Vec<String> = nal.iter().map(|b| format!("0x{b:02X}")).collect();
                println!("type {kind}, {} bytes: {}", nal.len(), bytes.join(", "));
            } else if kind == 33 {
                let s = Sps::parse(nal)?;
                let c = s.colour;
                println!(
                    "{path}: SPS {}x{} crop {:?}, {}-bit, chroma {}, CTB {}, scaling {}, colour {}/{}/{} full {}",
                    s.width,
                    s.height,
                    s.crop,
                    s.bit_depth_luma,
                    s.chroma_format,
                    1 << s.log2_ctb,
                    s.scaling.is_some(),
                    c.primaries,
                    c.transfer,
                    c.matrix,
                    c.full_range
                );
            } else if kind == 34 {
                let p = Pps::parse(nal)?;
                println!(
                    "{path}: PPS tiles {}, wavefronts {}, sign hiding {}",
                    p.tiles.is_some(),
                    p.entropy_coding_sync,
                    p.sign_data_hiding
                );
            }
        }
    }
    Ok(())
}
