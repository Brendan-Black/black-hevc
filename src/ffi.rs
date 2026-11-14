//! A C interface, for calling from other languages (such as Java, through its FFM API).
//! `include/black_hevc.h` declares it.
//!
//! Every function returns one of the status codes below, and on failure writes a message to
//! the caller's `error` buffer (UTF-8, cut to fit, always NUL-terminated) when it's given one.
//! Panics are caught and reported as [`PANIC`]; they never cross into the caller.

use std::ffi::{CStr, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use crate::Error;
use crate::still::still;

/// It worked.
pub const OK: i32 = 0;
/// The video couldn't be read, or the PNG couldn't be written.
pub const IO: i32 = 1;
/// The video is damaged, or isn't what it claims to be.
pub const CORRUPT: i32 = 2;
/// The video uses something black-hevc can't decode (another codec, an unsupported tool).
pub const UNSUPPORTED: i32 = 3;
/// A pointer argument was null, or a path wasn't UTF-8.
pub const ARGUMENT: i32 = 4;
/// A bug in black-hevc.
pub const PANIC: i32 = 5;

type Failure = (i32, String);

impl From<Error> for Failure {
	fn from(e: Error) -> Failure {
		let code = match e {
			Error::Io(_) => IO,
			Error::Corrupt(_) => CORRUPT,
			Error::Unsupported(_) => UNSUPPORTED,
		};
		(code, e.to_string())
	}
}

/// Run `f`, turning its failure or panic into a status code and a message in `error`.
///
/// # Safety
/// `error` is null or points to `error_cap` writable bytes.
unsafe fn run(error: *mut c_char, error_cap: usize, f: impl FnOnce() -> Result<(), Failure>) -> i32 {
	let (code, message) = match catch_unwind(AssertUnwindSafe(f)) {
		Ok(Ok(())) => return OK,
		Ok(Err(failure)) => failure,
		Err(panic) => {
			let what = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_default();
			(PANIC, format!("black-hevc failed (a bug): {what}"))
		}
	};
	if !error.is_null() && error_cap > 0 {
		// Cut at a character boundary, leaving room for the NUL.
		let mut n = message.len().min(error_cap - 1);
		while !message.is_char_boundary(n) {
			n -= 1;
		}
		// SAFETY: the caller gives `error_cap` bytes, and n + 1 <= error_cap.
		unsafe {
			std::ptr::copy_nonoverlapping(message.as_ptr(), error.cast(), n);
			*error.add(n) = 0;
		}
	}
	code
}

/// A NUL-terminated UTF-8 path argument.
///
/// # Safety
/// `p` is null or a NUL-terminated string that outlives `'a`.
unsafe fn path<'a>(p: *const c_char, what: &str) -> Result<&'a Path, Failure> {
	if p.is_null() {
		return Err((ARGUMENT, format!("the {what} path is null")));
	}
	// SAFETY: the caller's promise.
	let s = unsafe { CStr::from_ptr(p) };
	s.to_str().map(Path::new).map_err(|_| (ARGUMENT, format!("the {what} path isn't UTF-8")))
}

/// Write the still at `seconds` into `video` (an MP4 file) to `out` as a PNG fitting in `max` by
/// `max` pixels (0: full size), replacing any file there. See [`crate::still::still`].
///
/// # Safety
/// `video` and `out` are NUL-terminated UTF-8 paths; `error` is null or points to `error_cap`
/// writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn black_hevc_still_file(video: *const c_char, seconds: f64, max: u32, out: *const c_char, error: *mut c_char, error_cap: usize) -> i32 {
	// SAFETY: the caller's promises, passed on.
	unsafe {
		run(error, error_cap, || {
			let (video, out) = (path(video, "video")?, path(out, "PNG")?);
			let png = still(video, seconds, max)?;
			std::fs::write(out, png).map_err(|e| (IO, format!("writing {} failed: {e}", out.display())))
		})
	}
}

/// [`black_hevc_still_file`], with the PNG returned in memory instead: `*png` and `*png_len` are
/// set to a buffer that must be given back to [`black_hevc_free`]. They're left alone on
/// failure.
///
/// # Safety
/// `video` is a NUL-terminated UTF-8 path; `png` and `png_len` are writable; `error` is null or
/// points to `error_cap` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn black_hevc_still(
	video: *const c_char,
	seconds: f64,
	max: u32,
	png: *mut *mut u8,
	png_len: *mut usize,
	error: *mut c_char,
	error_cap: usize,
) -> i32 {
	// SAFETY: the caller's promises, passed on.
	unsafe {
		run(error, error_cap, || {
			if png.is_null() || png_len.is_null() {
				return Err((ARGUMENT, "png or png_len is null".into()));
			}
			let bytes = still(path(video, "video")?, seconds, max)?.into_boxed_slice();
			*png_len = bytes.len();
			*png = Box::into_raw(bytes).cast();
			Ok(())
		})
	}
}

/// Give back a buffer from [`black_hevc_still`]. Null does nothing.
///
/// # Safety
/// `png` and `len` are what [`black_hevc_still`] gave, not freed already, or `png` is null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn black_hevc_free(png: *mut u8, len: usize) {
	if !png.is_null() {
		// SAFETY: it came from Box::into_raw of a boxed slice this long.
		drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(png, len)) });
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::ffi::CString;

	fn message(buf: &[u8]) -> &str {
		CStr::from_bytes_until_nul(buf).unwrap().to_str().unwrap()
	}

	#[test]
	fn failures_come_back_as_codes_and_messages() {
		let missing = CString::new("/no/such/video.mp4").unwrap();
		let (mut png, mut len) = (std::ptr::null_mut(), 0);
		let mut error = [0xFFu8; 200];
		let code = unsafe { black_hevc_still(missing.as_ptr(), 1.0, 320, &mut png, &mut len, error.as_mut_ptr().cast(), error.len()) };
		assert_eq!(code, IO);
		assert!(png.is_null() && len == 0);
		assert!(message(&error).starts_with("reading the file failed"));

		// Null paths, and a message cut to fit (at a character boundary).
		let mut small = [0xFFu8; 8];
		let code = unsafe { black_hevc_still_file(std::ptr::null(), 0.0, 0, missing.as_ptr(), small.as_mut_ptr().cast(), small.len()) };
		assert_eq!(code, ARGUMENT);
		assert_eq!(message(&small), "the vid");
		let code = unsafe { black_hevc_still(missing.as_ptr(), 0.0, 0, &mut png, std::ptr::null_mut(), std::ptr::null_mut(), 0) };
		assert_eq!(code, ARGUMENT);

		// Not an MP4.
		let dir = std::env::temp_dir().join(format!("black-hevc-ffi-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let junk = dir.join("junk.mp4");
		std::fs::write(&junk, b"not a video at all").unwrap();
		let junk = CString::new(junk.to_str().unwrap()).unwrap();
		let code = unsafe { black_hevc_still(junk.as_ptr(), 0.0, 0, &mut png, &mut len, std::ptr::null_mut(), 0) };
		assert_eq!(code, CORRUPT);
		#[expect(clippy::let_underscore_must_use, reason = "best-effort cleanup of a temporary folder; the test has already passed")]
		let _ = std::fs::remove_dir_all(&dir);
		unsafe { black_hevc_free(std::ptr::null_mut(), 0) };
	}

	#[test]
	fn panics_stay_inside() {
		let mut error = [0u8; 100];
		let code = unsafe { run(error.as_mut_ptr().cast(), error.len(), || panic!("on purpose")) };
		assert_eq!(code, PANIC);
		assert_eq!(message(&error), "black-hevc failed (a bug): on purpose");
	}
}
