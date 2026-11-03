//! Turning an intra picture's slice data into samples: intra prediction (H.265 8.4.4.2),
//! scaling (8.6.2 to 8.6.4) and the inverse transforms, block by block in decoding order.
//! The in-loop filters (deblocking and SAO) aren't applied yet.

use crate::nal::NalHeader;
use crate::params::{ParamSets, Pps, Sps};
use crate::slice::SliceHeader;
use crate::slice_data::{CodingUnit, DC, PictureParser, Sao, Scans, Sink, TransformBlock};
use crate::{Error, Result, rbsp};

/// A decoded picture: Y, Cb and Cr planes at their coded size (gray pictures have only Y).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    /// The coded size in luma samples, before cropping.
    pub width: u32,
    pub height: u32,
    /// 0 gray, 1 4:2:0, 2 4:2:2, 3 4:4:4.
    pub chroma_format: u8,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    /// Luma samples to crop from the left, right, top and bottom for display.
    pub crop: [u32; 4],
    /// Each plane row by row, with no padding.
    pub planes: Vec<Vec<u16>>,
}

impl Picture {
    fn new(sps: &Sps) -> Picture {
        let mut p = Picture {
            width: sps.width,
            height: sps.height,
            chroma_format: sps.chroma_format,
            bit_depth_luma: sps.bit_depth_luma,
            bit_depth_chroma: sps.bit_depth_chroma,
            crop: sps.crop,
            planes: Vec::new(),
        };
        let n = if sps.chroma_format == 0 { 1 } else { 3 };
        p.planes = (0..n)
            .map(|c| {
                let (w, h) = p.plane_size(c);
                vec![0; w as usize * h as usize]
            })
            .collect();
        p
    }

    /// How much smaller chroma is than luma, across and down, as shifts.
    pub fn subsampling(&self, c: usize) -> (u32, u32) {
        match (c, self.chroma_format) {
            (0, _) | (_, 3) => (0, 0),
            (_, 2) => (1, 0),
            _ => (1, 1),
        }
    }

    /// A plane's coded size.
    pub fn plane_size(&self, c: usize) -> (u32, u32) {
        let (sx, sy) = self.subsampling(c);
        (self.width >> sx, self.height >> sy)
    }

    /// The part of a plane that's shown, row by row, and its size.
    pub fn cropped(&self, c: usize) -> (u32, u32, Vec<u16>) {
        let (sx, sy) = self.subsampling(c);
        let (w, h) = self.plane_size(c);
        let [l, r, t, b] = self.crop;
        let (x0, x1) = ((l >> sx) as usize, (w - (r >> sx)) as usize);
        let (y0, y1) = ((t >> sy) as usize, (h - (b >> sy)) as usize);
        let rows = self.planes[c].chunks_exact(w as usize);
        let out = rows
            .skip(y0)
            .take(y1 - y0)
            .flat_map(|row| &row[x0..x1])
            .copied()
            .collect();
        ((x1 - x0) as u32, (y1 - y0) as u32, out)
    }
}

/// Decode the first picture in some NAL units, such as an MP4 sample: it must be an intra picture.
/// Parameter sets among them are added to `params` first.
pub fn decode(params: &mut ParamSets, nal_units: &[&[u8]]) -> Result<Picture> {
    let mut slices = Vec::new();
    for &nal in nal_units {
        let h = NalHeader::parse(nal)?;
        if !h.is_slice() {
            params.add(nal)?;
        } else if h.layer == 0 {
            slices.push(nal);
        }
    }
    let params = &*params;
    let mut state: Option<(PictureParser, Reconstructor)> = None;
    let mut previous: Option<SliceHeader> = None;
    for nal in slices {
        let header = SliceHeader::parse(nal, params, previous.as_ref())?;
        if header.first_in_pic && state.is_some() {
            break;
        }
        if state.is_none() {
            if !header.first_in_pic {
                return Err(Error::corrupt("a picture's first slice is missing"));
            }
            let (pps, sps) = params.get(u32::from(header.pps_id))?;
            state = Some((PictureParser::new(sps, pps)?, Reconstructor::new(sps, pps)));
        }
        let (parser, rec) = state.as_mut().unwrap();
        parser.slice(&header, &rbsp(nal), rec)?;
        previous = Some(header);
    }
    let (parser, rec) = state.ok_or_else(|| Error::corrupt("no slices"))?;
    if !parser.complete() {
        return Err(Error::corrupt("slices are missing from the picture"));
    }
    Ok(rec.pic)
}

/// levelScale (8.6.4.2).
const LEVEL_SCALE: [i64; 6] = [40, 45, 51, 57, 64, 72];

/// intraPredAngle (Table 8-4) for modes 2 to 34, and invAngle (Table 8-5) for modes 11 to 25.
const ANGLE: [i32; 33] = [
    32, 26, 21, 17, 13, 9, 5, 2, 0, -2, -5, -9, -13, -17, -21, -26, -32, -26, -21, -17, -13, -9,
    -5, -2, 0, 2, 5, 9, 13, 17, 21, 26, 32,
];
const INV_ANGLE: [i32; 15] = [
    -4096, -1638, -910, -630, -482, -390, -315, -256, -315, -390, -482, -630, -910, -1638, -4096,
];

/// The DCT's coefficients, 64√2·cos(jπ/64) rounded the way the standard does (8.6.4.2).
const COS: [i32; 33] = [
    64, 90, 90, 90, 89, 88, 87, 85, 83, 82, 80, 78, 75, 73, 70, 67, 64, 61, 57, 54, 50, 46, 43, 38,
    36, 31, 25, 22, 18, 13, 9, 4, 0,
];

/// transMatrix: row k is the k-th basis function of the 32-point DCT. An N-point transform
/// uses every (32/N)-th row and the first N columns.
const DCT: [[i32; 32]; 32] = {
    let mut m = [[0; 32]; 32];
    let mut k = 0;
    while k < 32 {
        let mut n = 0;
        while n < 32 {
            let mut a = (k * (2 * n + 1)) % 128;
            if a > 64 {
                a = 128 - a;
            }
            m[k][n] = if a <= 32 { COS[a] } else { -COS[64 - a] };
            n += 1;
        }
        k += 1;
    }
    m
};

/// The 4x4 DST used for intra luma (8.6.4.2).
const DST: [[i32; 4]; 4] = [
    [29, 55, 74, 84],
    [74, 74, 0, -74],
    [84, -29, -74, 55],
    [55, -84, 74, -29],
];

/// One inverse transform: `used` inputs, `step` apart in `input` (the rest are 0), to `n` outputs.
fn transform_1d(input: &[i32], step: usize, out: &mut [i32], log2: u8, dst: bool, used: usize) {
    let n = 1usize << log2;
    for i in 0..n {
        let mut s = 0;
        for k in 0..used {
            let c = if dst {
                DST[k][i]
            } else {
                DCT[k << (5 - log2)][i]
            };
            s += input[k * step] * c;
        }
        out[i] = s;
    }
}

/// The two-stage inverse transform (8.6.4.2), in place: columns, then rows.
fn inverse_transform(block: &mut [i32], log2: u8, dst: bool, bit_depth: u8) {
    let n = 1usize << log2;
    // Coefficients past the last nonzero row and column contribute nothing.
    let (mut rows, mut cols) = (0, 0);
    for y in 0..n {
        for x in 0..n {
            if block[y * n + x] != 0 {
                rows = rows.max(y + 1);
                cols = cols.max(x + 1);
            }
        }
    }
    let mut tmp = [0i32; 1024];
    let mut column = [0i32; 32];
    for x in 0..cols {
        transform_1d(&block[x..], n, &mut column, log2, dst, rows);
        for i in 0..n {
            tmp[i * n + x] = ((column[i] + 64) >> 7).clamp(-32768, 32767);
        }
    }
    let shift = 20 - u32::from(bit_depth);
    for y in 0..n {
        transform_1d(&tmp[y * n..], 1, &mut column, log2, dst, cols);
        for i in 0..n {
            block[y * n + i] = (column[i] + (1 << (shift - 1))) >> shift;
        }
    }
}

/// Builds the picture as the slice data is read.
pub(crate) struct Reconstructor<'a> {
    sps: &'a Sps,
    pic: Picture,
    /// Per component, in 4x4 units of its own samples: whether it's been reconstructed.
    decoded: Vec<Vec<bool>>,
    w4: Vec<usize>,
    /// Per CTB: its slice's first CTB. And the current CTU's.
    slices: Vec<u32>,
    w_ctb: u32,
    slice: u32,
    /// ScalingFactor for intra blocks by size (4x4 to 32x32) and component, raster order;
    /// `None` when scaling lists are off (a flat 16).
    factors: Option<Vec<Vec<Vec<u8>>>>,
    residual: Vec<i32>,
}

impl<'a> Reconstructor<'a> {
    pub(crate) fn new(sps: &'a Sps, pps: &Pps) -> Reconstructor<'a> {
        let pic = Picture::new(sps);
        let (decoded, w4) = (0..pic.planes.len())
            .map(|c| {
                let (w, h) = pic.plane_size(c);
                let w4 = w.div_ceil(4) as usize;
                (vec![false; w4 * h.div_ceil(4) as usize], w4)
            })
            .unzip();
        let (w_ctb, h_ctb) = sps.ctbs();
        let factors = sps.scaling.as_ref().map(|sps_lists| {
            let lists = pps.scaling.as_ref().unwrap_or(sps_lists);
            let scans = Scans::new();
            (0..4)
                .map(|size| {
                    (0..3)
                        .map(|c| {
                            // 32x32 chroma (4:4:4 only) uses the 16x16 lists (7.4.5).
                            let from = if size == 3 && c > 0 { 2 } else { size };
                            let list = &lists.lists[from][c];
                            let n = 4usize << size;
                            let (grid, log2_grid) = if size == 0 { (4, 2) } else { (8, 3) };
                            let step = n / grid;
                            let inv = &scans.inv[log2_grid][0];
                            let mut f: Vec<u8> = (0..n * n)
                                .map(|i| {
                                    let (x, y) = (i % n / step, i / n / step);
                                    list[inv[y * grid + x] as usize]
                                })
                                .collect();
                            if from >= 2 {
                                f[0] = lists.dc[from - 2][c];
                            }
                            f
                        })
                        .collect()
                })
                .collect()
        });
        Reconstructor {
            sps,
            pic,
            decoded,
            w4,
            slices: vec![u32::MAX; (w_ctb * h_ctb) as usize],
            w_ctb,
            slice: 0,
            factors,
            residual: vec![0; 32 * 32],
        }
    }

    fn bit_depth(&self, c: usize) -> u8 {
        if c == 0 {
            self.sps.bit_depth_luma
        } else {
            self.sps.bit_depth_chroma
        }
    }

    /// Whether a sample of component `c` may be used to predict the current block: it's in
    /// the picture, already reconstructed, and in the same slice (6.4.1).
    fn available(&self, c: usize, x: i64, y: i64) -> bool {
        let (w, h) = self.pic.plane_size(c);
        if x < 0 || y < 0 || x >= i64::from(w) || y >= i64::from(h) {
            return false;
        }
        let (x, y) = (x as u32, y as u32);
        if !self.decoded[c][(y / 4) as usize * self.w4[c] + (x / 4) as usize] {
            return false;
        }
        let (sx, sy) = self.pic.subsampling(c);
        let log2 = self.sps.log2_ctb;
        let ctb = ((y << sy) >> log2) * self.w_ctb + ((x << sx) >> log2);
        self.slices[ctb as usize] == self.slice
    }

    fn mark(&mut self, c: usize, x: u32, y: u32, w: u32, h: u32) {
        let w4 = self.w4[c];
        for row in y / 4..(y + h).div_ceil(4) {
            let at = row as usize * w4;
            self.decoded[c][at + (x / 4) as usize..at + (x + w).div_ceil(4) as usize].fill(true);
        }
    }

    /// The intra prediction of a block (8.4.4.2), into `pred` in raster order.
    fn predict(&self, c: usize, x: u32, y: u32, log2: u8, mode: u8, pred: &mut [i32]) {
        let n = 1usize << log2;
        let bd = self.bit_depth(c);
        let (w, _) = self.pic.plane_size(c);
        let plane = &self.pic.planes[c];
        let sample = |xs: i64, ys: i64| i32::from(plane[ys as usize * w as usize + xs as usize]);

        // The neighbours in one line: the left column from the bottom up, the corner, then
        // the row above from left to right. That's the order of substitution and filtering.
        let count = 4 * n + 1;
        let mut r = [0i32; 129];
        let mut have = [false; 129];
        let (x, y) = (i64::from(x), i64::from(y));
        for (i, (r, have)) in r.iter_mut().zip(&mut have).take(count).enumerate() {
            let (xs, ys) = if i < 2 * n {
                (x - 1, y + (2 * n - 1 - i) as i64)
            } else {
                (x - 1 + (i - 2 * n) as i64, y - 1)
            };
            if self.available(c, xs, ys) {
                *have = true;
                *r = sample(xs, ys);
            }
        }
        // 8.4.4.2.2: missing neighbours copy the one before them.
        match have[..count].iter().position(|&h| h) {
            None => r[..count].fill(1 << (bd - 1)),
            Some(first) => {
                r[0] = r[first];
                for i in 1..count {
                    if !have[i] {
                        r[i] = r[i - 1];
                    }
                }
            }
        }

        // 8.4.4.2.3: smoothing, except for DC, 4x4 blocks and modes near horizontal or vertical.
        let sps = self.sps;
        if (c == 0 || sps.chroma_format == 3)
            && !sps.range.intra_smoothing_disabled
            && mode != DC
            && n != 4
        {
            let m = i32::from(mode);
            let distance = (m - 26).abs().min((m - 10).abs());
            let threshold = match n {
                8 => 7,
                16 => 1,
                _ => 0,
            };
            if distance > threshold {
                let corner = r[2 * n];
                let flat = |a: i32, b: i32, mid: i32| (a + b - 2 * mid).abs() < 1 << (bd - 5);
                if sps.strong_intra_smoothing
                    && c == 0
                    && n == 32
                    && flat(corner, r[4 * n], r[3 * n])
                    && flat(corner, r[0], r[n])
                {
                    let (bottom, right) = (r[0], r[128]);
                    for i in 0..63 {
                        r[63 - i] = ((63 - i as i32) * corner + (i as i32 + 1) * bottom + 32) >> 6;
                        r[65 + i] = ((63 - i as i32) * corner + (i as i32 + 1) * right + 32) >> 6;
                    }
                } else {
                    let mut f = r;
                    for i in 1..count - 1 {
                        f[i] = (r[i - 1] + 2 * r[i] + r[i + 1] + 2) >> 2;
                    }
                    r = f;
                }
            }
        }
        // left[k] is p[-1][k-1] and top[k] p[k-1][-1]; both start at the corner.
        let left = |k: usize| r[2 * n - k];
        let top = |k: usize| r[2 * n + k];
        let max = (1 << bd) - 1;
        let edge_filter = c == 0 && n < 32;

        match mode {
            0 => {
                let (right, bottom) = (top(n + 1), left(n + 1));
                for j in 0..n {
                    for i in 0..n {
                        pred[j * n + i] = ((n - 1 - i) as i32 * left(j + 1)
                            + (i as i32 + 1) * right
                            + (n - 1 - j) as i32 * top(i + 1)
                            + (j as i32 + 1) * bottom
                            + n as i32)
                            >> (log2 + 1);
                    }
                }
            }
            1 => {
                let sum: i32 = (1..=n).map(|k| top(k) + left(k)).sum();
                let dc = (sum + n as i32) >> (log2 + 1);
                pred[..n * n].fill(dc);
                if edge_filter {
                    pred[0] = (left(1) + 2 * dc + top(1) + 2) >> 2;
                    for k in 1..n {
                        pred[k] = (top(k + 1) + 3 * dc + 2) >> 2;
                        pred[k * n] = (left(k + 1) + 3 * dc + 2) >> 2;
                    }
                }
            }
            _ => {
                let vertical = mode >= 18;
                let (main, side): (&dyn Fn(usize) -> i32, &dyn Fn(usize) -> i32) = if vertical {
                    (&top, &left)
                } else {
                    (&left, &top)
                };
                let angle = ANGLE[mode as usize - 2];
                // ref[k] for k from -n to 2n, at reference[32 + k].
                const O: i32 = 32;
                let mut reference = [0i32; 97];
                for k in 0..=n {
                    reference[O as usize + k] = main(k);
                }
                let n32 = n as i32;
                if angle < 0 {
                    let last = (n32 * angle) >> 5;
                    if last < -1 {
                        let inv = INV_ANGLE[mode as usize - 11];
                        for k in last..=-1 {
                            reference[(O + k) as usize] = side(((k * inv + 128) >> 8) as usize);
                        }
                    }
                } else {
                    for k in n + 1..=2 * n {
                        reference[O as usize + k] = main(k);
                    }
                }
                for j in 0..n32 {
                    let pos = (j + 1) * angle;
                    let (idx, fact) = (pos >> 5, pos & 31);
                    for i in 0..n32 {
                        let at = (O + i + idx + 1) as usize;
                        let v = if fact != 0 {
                            ((32 - fact) * reference[at] + fact * reference[at + 1] + 16) >> 5
                        } else {
                            reference[at]
                        };
                        // j runs across the main direction's rows (or columns).
                        let (px, py) = if vertical { (i, j) } else { (j, i) };
                        pred[(py * n32 + px) as usize] = v;
                    }
                }
                if edge_filter && (mode == 26 || mode == 10) {
                    for k in 0..n {
                        let v = (main(1) + ((side(k + 1) - side(0)) >> 1)).clamp(0, max);
                        let at = if vertical { k * n } else { k };
                        pred[at] = v;
                    }
                }
            }
        }
    }

    /// The residual of a block with coefficients (8.6.2 to 8.6.4), into `self.residual`.
    fn residual(&mut self, tb: &TransformBlock, coeffs: &[i32]) {
        let n = 1usize << tb.log2_size;
        let c = tb.c_idx as usize;
        let bd = self.bit_depth(c);
        let res = &mut self.residual[..n * n];
        if tb.transquant_bypass {
            res.copy_from_slice(coeffs);
            return;
        }
        let shift = u32::from(bd) + u32::from(tb.log2_size) - 5;
        let qp = tb.qp as usize;
        let scale = LEVEL_SCALE[qp % 6] << (qp / 6);
        let factors = self
            .factors
            .as_ref()
            .filter(|_| !(tb.transform_skip && n > 4))
            .map(|f| &f[tb.log2_size as usize - 2][c]);
        for (i, (r, &level)) in res.iter_mut().zip(coeffs).enumerate() {
            if level == 0 {
                *r = 0;
                continue;
            }
            let m = factors.map_or(16, |f| i64::from(f[i]));
            let d = (i64::from(level) * m * scale + (1 << (shift - 1))) >> shift;
            *r = d.clamp(-32768, 32767) as i32;
        }
        if tb.transform_skip {
            let ts = 5 + u32::from(tb.log2_size);
            let bd_shift = 20 - u32::from(bd);
            for r in res {
                *r = ((*r << ts) + (1 << (bd_shift - 1))) >> bd_shift;
            }
        } else {
            let dst = c == 0 && n == 4;
            inverse_transform(res, tb.log2_size, dst, bd);
        }
    }
}

impl Sink for Reconstructor<'_> {
    fn ctu(&mut self, x: u32, y: u32, slice: u32, _sao: &Sao) {
        self.slices[(y * self.w_ctb + x) as usize] = slice;
        self.slice = slice;
    }

    fn transform_block(&mut self, tb: &TransformBlock) {
        let c = tb.c_idx as usize;
        let n = 1usize << tb.log2_size;
        let mut pred = [0i32; 1024];
        self.predict(c, tb.x, tb.y, tb.log2_size, tb.mode, &mut pred);
        if let Some(coeffs) = tb.coeffs {
            self.residual(tb, coeffs);
            for (p, r) in pred.iter_mut().zip(&self.residual[..n * n]) {
                *p += r;
            }
        }
        let max = (1 << self.bit_depth(c)) - 1;
        let (w, _) = self.pic.plane_size(c);
        let plane = &mut self.pic.planes[c];
        for (j, row) in pred[..n * n].chunks_exact(n).enumerate() {
            let at = (tb.y as usize + j) * w as usize + tb.x as usize;
            for (s, &v) in plane[at..at + n].iter_mut().zip(row) {
                *s = v.clamp(0, max) as u16;
            }
        }
        self.mark(c, tb.x, tb.y, n as u32, n as u32);
    }

    fn coding_unit(&mut self, cu: &CodingUnit) {
        let Some(samples) = &cu.pcm else {
            return;
        };
        let pcm = self.sps.pcm.unwrap();
        let mut samples = samples.iter();
        for c in 0..self.pic.planes.len() {
            let (sx, sy) = self.pic.subsampling(c);
            let size = 1u32 << cu.log2_size;
            let (x, y, w, h) = (cu.x >> sx, cu.y >> sy, size >> sx, size >> sy);
            let shift = if c == 0 {
                self.sps.bit_depth_luma - pcm.bit_depth_luma
            } else {
                self.sps.bit_depth_chroma - pcm.bit_depth_chroma
            };
            let (pw, _) = self.pic.plane_size(c);
            for row in y..y + h {
                let at = (row * pw + x) as usize;
                for s in &mut self.pic.planes[c][at..at + w as usize] {
                    *s = samples.next().copied().unwrap_or(0) << shift;
                }
            }
            self.mark(c, x, y, w, h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dct_rows() {
        // The 4- and 8-point transforms' rows, as printed in the standard.
        assert_eq!(DCT[8][..4], [83, 36, -36, -83]);
        assert_eq!(DCT[16][..4], [64, -64, -64, 64]);
        assert_eq!(DCT[4][..8], [89, 75, 50, 18, -18, -50, -75, -89]);
        assert_eq!(DCT[12][..8], [75, -18, -89, -50, 50, 89, 18, -75]);
        assert_eq!(
            DCT[1][..16],
            [
                90, 90, 88, 85, 82, 78, 73, 67, 61, 54, 46, 38, 31, 22, 13, 4
            ]
        );
        assert_eq!(DCT[31][..4], [4, -13, 22, -31]);
        assert!(DCT[0].iter().all(|&c| c == 64));
    }

    #[test]
    fn dc_only_blocks_are_flat() {
        // A lone DC coefficient spreads evenly: 64·64·d over 2^(7 + 20 - 8), so 256 becomes 2.
        for log2 in 2..=5u8 {
            let n = 1 << log2;
            let mut b = vec![0; n * n];
            b[0] = 256;
            inverse_transform(&mut b, log2, false, 8);
            assert!(b.iter().all(|&v| v == 2), "{log2}: {:?}", &b[..4]);
        }
    }
}
