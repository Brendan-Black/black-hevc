//! HEVC (H.265) video, with no third-party dependencies.
//!
//! So far: [`mp4`] finds a video's keyframes in an MP4 file and reads them out as NAL units,
//! [`params`] reads the parameter sets, and [`slice`] the slice headers. Decoding a keyframe into a picture comes next.

mod bits;
mod cabac;
mod error;
pub mod mp4;
pub mod nal;
pub mod params;
pub mod slice;
pub mod slice_data;

pub use bits::{Syntax, rbsp};
pub use error::{Error, Result};
