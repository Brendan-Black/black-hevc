//! HEVC (H.265) video, with no third-party dependencies.
//!
//! So far: [`mp4`] finds a video's keyframes in an MP4 file and reads them out as NAL units,
//! and [`params`] reads the parameter sets. Decoding a keyframe into a picture comes next.

mod bits;
mod error;
pub mod mp4;
pub mod nal;
pub mod params;

pub use bits::{Syntax, rbsp};
pub use error::{Error, Result};
