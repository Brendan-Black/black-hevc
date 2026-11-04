//! HEVC (H.265) video, with no third-party dependencies.
//!
//! So far: [`mp4`] finds a video's keyframes in an MP4 file and reads them out as NAL units,
//! [`params`] reads the parameter sets, [`slice`] the slice headers, [`slice_data`] the coded
//! blocks of intra pictures, and [`picture`] turns those into samples, and [`still`] into RGB thumbnails and PNGs. The in-loop filters
//! (deblocking and SAO) come next.

mod bits;
mod cabac;
mod error;
pub mod mp4;
pub mod nal;
pub mod params;
pub mod picture;
pub mod slice;
pub mod slice_data;
pub mod still;

pub use bits::{Syntax, rbsp};
pub use error::{Error, Result};
