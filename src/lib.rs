//! HEVC (H.265) video, with no third-party dependencies.
//!
//! So far: [`mp4`] finds a video's keyframes in an MP4 file and reads them out as HEVC NAL
//! units. Decoding a keyframe into a picture comes next.

mod error;
pub mod mp4;

pub use error::{Error, Result};
