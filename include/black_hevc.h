/* black-hevc's C interface: stills from HEVC videos in MP4 files, as PNGs.
 *
 * Build the library with `cargo build --release`: black_hevc.dll on Windows,
 * libblack_hevc.so on Linux. Paths are NUL-terminated UTF-8. Every function returns
 * one of the status codes below; on failure it writes a message to `error` (UTF-8,
 * cut to fit `error_cap` bytes, always NUL-terminated) unless `error` is NULL. */

#ifndef BLACK_HEVC_H
#define BLACK_HEVC_H

#include <stddef.h>
#include <stdint.h>

#define BLACK_HEVC_OK 0
#define BLACK_HEVC_IO 1          /* the video couldn't be read, or the PNG written */
#define BLACK_HEVC_CORRUPT 2     /* the video is damaged */
#define BLACK_HEVC_UNSUPPORTED 3 /* another codec, or a tool black-hevc can't decode */
#define BLACK_HEVC_ARGUMENT 4    /* a NULL pointer, or a path that isn't UTF-8 */
#define BLACK_HEVC_PANIC 5       /* a bug in black-hevc */

/* Write the still at `seconds` into `video` to `out` as a PNG fitting in `max` by
 * `max` pixels (0: full size), replacing any file there. The still is the last
 * keyframe at or before `seconds`, or the first keyframe if none is. */
int32_t black_hevc_still_file(const char *video, double seconds, uint32_t max,
                              const char *out, char *error, size_t error_cap);

/* The same, with the PNG returned in memory: `*png` and `*png_len` are set to a
 * buffer that must be given back to black_hevc_free. Left alone on failure. */
int32_t black_hevc_still(const char *video, double seconds, uint32_t max,
                         uint8_t **png, size_t *png_len, char *error, size_t error_cap);

/* Give back a buffer from black_hevc_still. NULL does nothing. */
void black_hevc_free(uint8_t *png, size_t png_len);

#endif
