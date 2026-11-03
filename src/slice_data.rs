//! Slice data (H.265 7.3.8) of intra pictures: each CTU's SAO parameters, coding quadtree,
//! prediction modes and residual coefficients, read with CABAC and handed to a [`Sink`] in
//! decoding order, which is the order the picture must be reconstructed in.

use crate::bits::Bits;
use crate::cabac::{Cabac, Contexts, ctx};
use crate::params::{Pps, Sps};
use crate::slice::{SliceHeader, SliceType};
use crate::{Error, Result};

pub const PLANAR: u8 = 0;
pub const DC: u8 = 1;

/// Table 8-3: chroma modes for 4:2:2, whose chroma blocks are twice as tall as wide.
const MODE_422: [u8; 35] = [
    0, 1, 2, 2, 2, 2, 3, 5, 7, 8, 10, 12, 13, 15, 17, 18, 19, 20, 21, 22, 23, 23, 24, 24, 25, 25,
    26, 27, 27, 28, 28, 29, 29, 30, 31,
];

/// Table 9-50 (ctxIdxMap): sig_coeff_flag contexts in a 4x4 transform block, by position.
const CTX_IDX_MAP: [u8; 16] = [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 8];

/// Where a CTB not yet decoded is in [`PictureParser`]'s slice map.
const NO_SLICE: u32 = u32::MAX;

/// One component's sample adaptive offset (7.4.9.3.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SaoKind {
    #[default]
    Off,
    /// Band offset, from this band (of 32).
    Band(u8),
    /// Edge offset in this direction: 0 horizontal, 1 vertical, 2 and 3 diagonal.
    Edge(u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SaoParams {
    pub kind: SaoKind,
    /// Signed, before `log2_sao_offset_scale` is applied.
    pub offsets: [i8; 4],
}

/// SAO for Y, Cb and Cr.
pub type Sao = [SaoParams; 3];

/// A coding unit, given to the sink once it's fully read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodingUnit {
    /// Top-left, in luma samples.
    pub x: u32,
    pub y: u32,
    pub log2_size: u8,
    /// Split into four prediction blocks (NxN), each with its own luma mode.
    pub nxn: bool,
    /// The luma mode of each prediction block in raster order (all four the same unless NxN).
    pub luma_modes: [u8; 4],
    /// The chroma modes likewise (they differ only in 4:4:4 with NxN).
    pub chroma_modes: [u8; 4],
    pub transquant_bypass: bool,
    /// QpY, for deblocking.
    pub qp_y: i32,
    /// For a PCM block, its samples as stored: luma, then Cb, then Cr, each in raster order.
    pub pcm: Option<Vec<u16>>,
}

/// A transform block: a square of one component, predicted from its neighbours and then
/// corrected by its residual, if it has one. Every block of a coding unit comes before the unit itself.
#[derive(Clone, Copy, Debug)]
pub struct TransformBlock<'a> {
    /// 0 for Y, 1 for Cb, 2 for Cr.
    pub c_idx: u8,
    /// Top-left, in this component's samples.
    pub x: u32,
    pub y: u32,
    pub log2_size: u8,
    /// The intra prediction mode (0 planar, 1 DC, 2 to 34 angular).
    pub mode: u8,
    pub qp_y: i32,
    pub transquant_bypass: bool,
    pub transform_skip: bool,
    /// The coefficient levels in raster order, or `None` when every one is 0.
    pub coeffs: Option<&'a [i32]>,
}

/// Receives a picture's contents as they're read.
pub trait Sink {
    /// A CTU starts: its position in CTBs, the address of the first CTB of its slice, and its SAO.
    fn ctu(&mut self, _x: u32, _y: u32, _slice: u32, _sao: &Sao) {}
    fn transform_block(&mut self, tb: &TransformBlock);
    fn coding_unit(&mut self, cu: &CodingUnit);
}

/// Where a slice's arithmetic codes were found, for checking against its header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SliceStats {
    /// Where each substream (wavefront row) starts, in bytes of the RBSP.
    pub substreams: Vec<usize>,
    /// Where the slice data ends, after its last byte-aligned bit.
    pub end: usize,
    pub ctus: u32,
}

/// Scan orders (6.5.3 to 6.5.5): diagonal, horizontal and vertical, for 1x1 to 8x8.
struct Scans {
    /// `pos[log2][scan][i]`: the i-th position.
    pos: [[Vec<(u8, u8)>; 3]; 4],
    /// `inv[log2][scan][y * size + x]`: the index of a position.
    inv: [[Vec<u8>; 3]; 4],
}

impl Scans {
    fn new() -> Scans {
        let mut pos: [[Vec<(u8, u8)>; 3]; 4] = Default::default();
        let mut inv: [[Vec<u8>; 3]; 4] = Default::default();
        for log2 in 0..4 {
            let n = 1i32 << log2;
            let mut diag = Vec::new();
            let (mut x, mut y) = (0i32, 0i32);
            while diag.len() < (n * n) as usize {
                while y >= 0 {
                    if x < n && y < n {
                        diag.push((x as u8, y as u8));
                    }
                    y -= 1;
                    x += 1;
                }
                y = x;
                x = 0;
            }
            let horizontal = (0..n)
                .flat_map(|y| (0..n).map(move |x| (x as u8, y as u8)))
                .collect();
            let vertical = (0..n)
                .flat_map(|x| (0..n).map(move |y| (x as u8, y as u8)))
                .collect();
            pos[log2] = [diag, horizontal, vertical];
            for scan in 0..3 {
                let mut table = vec![0; (n * n) as usize];
                for (i, &(x, y)) in pos[log2][scan].iter().enumerate() {
                    table[y as usize * n as usize + x as usize] = i as u8;
                }
                inv[log2][scan] = table;
            }
        }
        Scans { pos, inv }
    }
}

/// Reads the slices of one intra picture.
pub struct PictureParser<'a> {
    sps: &'a Sps,
    pps: &'a Pps,
    w_ctb: u32,
    h_ctb: u32,
    /// The picture in 4x4 units, for the maps below.
    w4: usize,
    /// Per 4x4: the luma intra mode, the coding quadtree depth, and QpY.
    modes: Vec<u8>,
    depths: Vec<u8>,
    qps: Vec<i8>,
    /// Per CTB: the address of its slice's first CTB, and its SAO.
    slices: Vec<u32>,
    sao: Vec<Sao>,
    scans: Scans,
    /// Contexts saved after the second CTB of a row, for the row below (wavefronts).
    wpp: Option<Contexts>,
    /// Contexts at the end of the last slice segment, for a dependent one.
    dependent: Option<Contexts>,
    /// The current slice: its first CTB, and QpY of the last coding unit read.
    slice_addr: u32,
    last_qp: i32,
}

impl<'a> PictureParser<'a> {
    pub fn new(sps: &'a Sps, pps: &'a Pps) -> Result<PictureParser<'a>> {
        if pps.tiles.is_some() {
            return Err(Error::unsupported("tiles"));
        }
        if sps.separate_colour_planes {
            return Err(Error::unsupported("separately coded colour planes"));
        }
        let r = &sps.range;
        if r.transform_skip_rotation
            || r.transform_skip_context
            || r.implicit_rdpcm
            || r.explicit_rdpcm
            || r.extended_precision
            || r.persistent_rice_adaptation
            || r.cabac_bypass_alignment
            || pps.range.cross_component_prediction
            || pps.range.chroma_qp_offset_list.is_some()
        {
            return Err(Error::unsupported("the range extension's coding tools"));
        }
        let (w_ctb, h_ctb) = sps.ctbs();
        let w4 = (sps.width / 4) as usize;
        let n4 = w4 * (sps.height / 4) as usize;
        let ctbs = (w_ctb * h_ctb) as usize;
        Ok(PictureParser {
            sps,
            pps,
            w_ctb,
            h_ctb,
            w4,
            modes: vec![DC; n4],
            depths: vec![0; n4],
            qps: vec![0; n4],
            slices: vec![NO_SLICE; ctbs],
            sao: vec![Sao::default(); ctbs],
            scans: Scans::new(),
            wpp: None,
            dependent: None,
            slice_addr: 0,
            last_qp: 0,
        })
    }

    /// Whether every CTB of the picture has been read.
    pub fn complete(&self) -> bool {
        self.slices.iter().all(|&s| s != NO_SLICE)
    }

    /// Read one slice segment's data: `rbsp` is its NAL unit with emulation prevention removed.
    pub fn slice<S: Sink>(
        &mut self,
        h: &SliceHeader,
        rbsp: &[u8],
        sink: &mut S,
    ) -> Result<SliceStats> {
        if h.kind != SliceType::I {
            return Err(Error::unsupported(
                "P and B slices (only keyframes are decoded)",
            ));
        }
        if h.pps_id != self.pps.id {
            return Err(Error::corrupt("the slices of a picture use different PPSs"));
        }
        let total = self.w_ctb * self.h_ctb;
        let mut addr = h.address;
        if self.slices[addr as usize] != NO_SLICE {
            return Err(Error::corrupt(format!("two slices cover CTB {addr}")));
        }
        if !h.dependent {
            self.slice_addr = h.address;
        } else if addr == 0 || self.slices[addr as usize - 1] == NO_SLICE {
            return Err(Error::corrupt(
                "a dependent slice segment doesn't follow the one before it",
            ));
        }
        let wpp = self.pps.entropy_coding_sync;
        let w = self.w_ctb;
        let mut run = Run {
            cabac: Cabac::new(rbsp, h.data_offset)?,
            c: Contexts::new(h.qp),
            h,
            sink,
            first_qg: !h.dependent,
            qg_pred: h.qp,
            qp_y: h.qp,
            delta_coded: false,
            coeffs: vec![0; 32 * 32],
            p: self,
        };
        if wpp && addr.is_multiple_of(w) {
            run.sync_wpp(addr / w)?;
        } else if h.dependent {
            run.c = run.p.dependent.clone().ok_or_else(|| {
                Error::corrupt("a dependent slice segment has nothing to continue")
            })?;
        }
        let mut stats = SliceStats {
            substreams: vec![h.data_offset],
            end: 0,
            ctus: 0,
        };
        loop {
            run.ctu(addr)?;
            stats.ctus += 1;
            let end = run.cabac.terminate();
            if wpp && addr % w == 1 {
                run.p.wpp = Some(run.c.clone());
            }
            if run.cabac.overrun() {
                return Err(Error::corrupt("slice data is cut short"));
            }
            addr += 1;
            if end {
                break;
            }
            if addr >= total {
                return Err(Error::corrupt("a slice runs past the end of the picture"));
            }
            if wpp && addr.is_multiple_of(w) {
                if !run.cabac.terminate() {
                    return Err(Error::corrupt(
                        "a wavefront row doesn't end where it should",
                    ));
                }
                let next = run.cabac.align()?;
                run.cabac.restart(next)?;
                stats.substreams.push(next);
                run.sync_wpp(addr / w)?;
            }
        }
        if run.p.pps.dependent_slice_segments {
            run.p.dependent = Some(run.c.clone());
        }
        let end = run.cabac.align()?;
        if run.cabac.overrun() || rbsp[end..].iter().any(|&b| b != 0) {
            return Err(Error::corrupt("slice data doesn't end where it should"));
        }
        stats.end = end;
        Ok(stats)
    }

    fn at(&self, x: u32, y: u32) -> usize {
        (y as usize / 4) * self.w4 + x as usize / 4
    }

    /// Whether the luma sample at (x, y) is in the picture and in the current slice: whether
    /// a block may use it as its left or upper neighbour (6.4.1).
    fn available(&self, x: i64, y: i64) -> bool {
        if x < 0 || y < 0 || x >= i64::from(self.sps.width) || y >= i64::from(self.sps.height) {
            return false;
        }
        let log2 = self.sps.log2_ctb;
        let ctb = (y as u32 >> log2) * self.w_ctb + (x as u32 >> log2);
        self.slices[ctb as usize] == self.slice_addr
    }

    /// Set a square of a 4x4 map.
    fn fill(map: &mut [u8], w4: usize, x: u32, y: u32, size: u32, v: u8) {
        let (x4, y4, n) = (x as usize / 4, y as usize / 4, (size as usize / 4).max(1));
        for row in y4..y4 + n {
            map[row * w4 + x4..row * w4 + x4 + n].fill(v);
        }
    }
}

/// Reading one slice segment.
struct Run<'r, 'a, 'd, S: Sink> {
    p: &'r mut PictureParser<'a>,
    cabac: Cabac<'d>,
    c: Contexts,
    h: &'r SliceHeader,
    sink: &'r mut S,
    /// The next quantization group is the first of its slice or wavefront row.
    first_qg: bool,
    /// qPY_PRED of the current quantization group, and QpY now.
    qg_pred: i32,
    qp_y: i32,
    delta_coded: bool,
    coeffs: Vec<i32>,
}

impl<S: Sink> Run<'_, '_, '_, S> {
    fn bin(&mut self, ctx: usize) -> bool {
        self.cabac.decision(&mut self.c.ctx[ctx])
    }

    fn chroma_format(&self) -> u8 {
        self.p.sps.chroma_format
    }

    /// At the start of a wavefront row: the contexts after the second CTB of the row above if
    /// it's in this slice, else fresh ones (9.3.1).
    fn sync_wpp(&mut self, row: u32) -> Result<()> {
        let p = &self.p;
        let above_right =
            row > 0 && p.w_ctb > 1 && p.slices[((row - 1) * p.w_ctb + 1) as usize] == p.slice_addr;
        self.c = if above_right {
            p.wpp
                .clone()
                .ok_or_else(|| Error::corrupt("a wavefront row has nothing to continue"))?
        } else {
            Contexts::new(self.h.qp)
        };
        self.first_qg = true;
        Ok(())
    }

    fn ctu(&mut self, addr: u32) -> Result<()> {
        let w = self.p.w_ctb;
        let (cx, cy) = (addr % w, addr / w);
        let slice = self.p.slice_addr;
        self.p.slices[addr as usize] = slice;
        let sao = if self.h.sao_luma || self.h.sao_chroma {
            self.sao(addr, cx, cy)?
        } else {
            Sao::default()
        };
        self.p.sao[addr as usize] = sao;
        self.sink.ctu(cx, cy, slice, &sao);
        let log2 = self.p.sps.log2_ctb;
        self.quadtree(cx << log2, cy << log2, log2, 0)
    }

    /// sao (7.3.8.3).
    fn sao(&mut self, addr: u32, cx: u32, cy: u32) -> Result<Sao> {
        let (slices, w) = (&self.p.slices, self.p.w_ctb);
        let slice = self.p.slice_addr;
        let left = cx > 0 && slices[addr as usize - 1] == slice;
        let up = cy > 0 && slices[(addr - w) as usize] == slice;
        if left && self.bin(ctx::SAO_MERGE) {
            return Ok(self.p.sao[addr as usize - 1]);
        }
        if up && self.bin(ctx::SAO_MERGE) {
            return Ok(self.p.sao[(addr - w) as usize]);
        }
        let mut sao = Sao::default();
        let components = if self.chroma_format() == 0 { 1 } else { 3 };
        for c in 0..components {
            if (c == 0 && !self.h.sao_luma) || (c > 0 && !self.h.sao_chroma) {
                continue;
            }
            let kind = if c == 2 {
                sao[1].kind
            } else if !self.bin(ctx::SAO_TYPE) {
                SaoKind::Off
            } else if self.cabac.bypass() {
                SaoKind::Edge(0)
            } else {
                SaoKind::Band(0)
            };
            if kind == SaoKind::Off {
                continue;
            }
            let depth = if c == 0 {
                self.p.sps.bit_depth_luma
            } else {
                self.p.sps.bit_depth_chroma
            };
            let max = (1 << (depth.min(10) - 5)) - 1;
            let mut offsets = [0i8; 4];
            for o in &mut offsets {
                while *o < max && self.cabac.bypass() {
                    *o += 1;
                }
            }
            sao[c].kind = match kind {
                SaoKind::Band(_) => {
                    for o in &mut offsets {
                        if *o != 0 && self.cabac.bypass() {
                            *o = -*o;
                        }
                    }
                    SaoKind::Band(self.cabac.bypass_bits(5) as u8)
                }
                _ => {
                    offsets[2] = -offsets[2];
                    offsets[3] = -offsets[3];
                    SaoKind::Edge(if c == 2 {
                        if let SaoKind::Edge(class) = sao[1].kind {
                            class
                        } else {
                            0
                        }
                    } else {
                        self.cabac.bypass_bits(2) as u8
                    })
                }
            };
            sao[c].offsets = offsets;
        }
        Ok(sao)
    }

    /// coding_quadtree (7.3.8.4).
    fn quadtree(&mut self, x: u32, y: u32, log2: u8, depth: u8) -> Result<()> {
        let sps = self.p.sps;
        let size = 1u32 << log2;
        let split = if x + size <= sps.width && y + size <= sps.height && log2 > sps.log2_min_cb {
            let deeper = |p: &PictureParser, xn: i64, yn: i64| {
                p.available(xn, yn) && p.depths[p.at(xn as u32, yn as u32)] > depth
            };
            let inc = usize::from(deeper(self.p, i64::from(x) - 1, i64::from(y)))
                + usize::from(deeper(self.p, i64::from(x), i64::from(y) - 1));
            self.bin(ctx::SPLIT_CU + inc)
        } else {
            log2 > sps.log2_min_cb
        };
        let qg_log2 = sps.log2_ctb - self.p.pps.cu_qp_delta_depth.unwrap_or(0);
        if log2 >= qg_log2 {
            self.start_quantization_group(x, y);
        }
        if split {
            let half = size / 2;
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (x1, y1) = (x + dx * half, y + dy * half);
                if x1 < sps.width && y1 < sps.height {
                    self.quadtree(x1, y1, log2 - 1, depth + 1)?;
                }
            }
            Ok(())
        } else {
            self.coding_unit(x, y, log2, depth)
        }
    }

    /// qPY_PRED (8.6.1), from the groups to the left and above when they're in this CTB.
    fn start_quantization_group(&mut self, x: u32, y: u32) {
        let prev = if self.first_qg {
            self.h.qp
        } else {
            self.p.last_qp
        };
        self.first_qg = false;
        let mask = (1 << self.p.sps.log2_ctb) - 1;
        let a = if x & mask != 0 {
            i32::from(self.p.qps[self.p.at(x - 1, y)])
        } else {
            prev
        };
        let b = if y & mask != 0 {
            i32::from(self.p.qps[self.p.at(x, y - 1)])
        } else {
            prev
        };
        self.qg_pred = (a + b + 1) >> 1;
        self.qp_y = self.qg_pred;
        self.delta_coded = false;
    }

    /// coding_unit (7.3.8.5), intra only.
    fn coding_unit(&mut self, x: u32, y: u32, log2: u8, depth: u8) -> Result<()> {
        let sps = self.p.sps;
        let size = 1u32 << log2;
        let w4 = self.p.w4;
        PictureParser::fill(&mut self.p.depths, w4, x, y, size, depth);
        let transquant_bypass = self.p.pps.transquant_bypass && self.bin(ctx::TRANSQUANT_BYPASS);
        let nxn = log2 == sps.log2_min_cb && !self.bin(ctx::PART_MODE);
        let mut cu = CodingUnit {
            x,
            y,
            log2_size: log2,
            nxn,
            luma_modes: [DC; 4],
            chroma_modes: [DC; 4],
            transquant_bypass,
            qp_y: 0,
            pcm: None,
        };
        let pcm = !nxn
            && sps
                .pcm
                .is_some_and(|p| (p.log2_min_size..=p.log2_max_size).contains(&log2))
            && self.cabac.terminate();
        if pcm {
            cu.pcm = Some(self.pcm_samples(log2)?);
            PictureParser::fill(&mut self.p.modes, w4, x, y, size, DC);
        } else {
            let parts = if nxn { 4 } else { 1 };
            let pb = if nxn { size / 2 } else { size };
            let mut flags = [false; 4];
            for f in flags.iter_mut().take(parts) {
                *f = self.bin(ctx::PREV_INTRA_LUMA_PRED);
            }
            let mut values = [0u8; 4];
            for i in 0..parts {
                values[i] = if flags[i] {
                    if !self.cabac.bypass() {
                        0
                    } else if !self.cabac.bypass() {
                        1
                    } else {
                        2
                    }
                } else {
                    self.cabac.bypass_bits(5) as u8
                };
            }
            for i in 0..parts {
                let (xp, yp) = (x + (i as u32 % 2) * pb, y + (i as u32 / 2) * pb);
                let mode = self.luma_mode(xp, yp, flags[i], values[i]);
                PictureParser::fill(&mut self.p.modes, w4, xp, yp, pb, mode);
                cu.luma_modes[i] = mode;
            }
            if !nxn {
                cu.luma_modes = [cu.luma_modes[0]; 4];
            }
            let format = self.chroma_format();
            if format == 3 {
                for i in 0..parts {
                    let idc = self.chroma_idc();
                    cu.chroma_modes[i] = chroma_mode(idc, cu.luma_modes[i], format);
                }
                if !nxn {
                    cu.chroma_modes = [cu.chroma_modes[0]; 4];
                }
            } else if format != 0 {
                let idc = self.chroma_idc();
                cu.chroma_modes = [chroma_mode(idc, cu.luma_modes[0], format); 4];
            }
            let max_depth = sps.max_transform_depth_intra + u8::from(nxn);
            self.transform_tree(&cu, x, y, x, y, log2, 0, 0, max_depth, [false; 4])?;
        }
        cu.qp_y = self.qp_y;
        let qp = self.qp_y as i8;
        let qps = &mut self.p.qps;
        let (x4, y4, n) = (x as usize / 4, y as usize / 4, size as usize / 4);
        for row in y4..y4 + n {
            qps[row * w4 + x4..row * w4 + x4 + n].fill(qp);
        }
        self.p.last_qp = self.qp_y;
        self.sink.coding_unit(&cu);
        Ok(())
    }

    fn chroma_idc(&mut self) -> u8 {
        if !self.bin(ctx::INTRA_CHROMA_PRED_MODE) {
            4
        } else {
            self.cabac.bypass_bits(2) as u8
        }
    }

    /// IntraPredModeY (8.4.2): from the three most probable modes, built from the blocks to
    /// the left and above, or one of the other 32.
    fn luma_mode(&self, x: u32, y: u32, most_probable: bool, value: u8) -> u8 {
        let p = &self.p;
        let candidate = |xn: i64, yn: i64, above: bool| {
            // The CTB row above isn't used, so a decoder needs only one row of modes.
            if !p.available(xn, yn)
                || (above && yn < i64::from((y >> p.sps.log2_ctb) << p.sps.log2_ctb))
            {
                DC
            } else {
                p.modes[p.at(xn as u32, yn as u32)]
            }
        };
        let a = candidate(i64::from(x) - 1, i64::from(y), false);
        let b = candidate(i64::from(x), i64::from(y) - 1, true);
        let mut list = if a == b {
            if a < 2 {
                [PLANAR, DC, 26]
            } else {
                [a, 2 + (a + 29) % 32, 2 + (a - 2 + 1) % 32]
            }
        } else {
            let c = if a != PLANAR && b != PLANAR {
                PLANAR
            } else if a != DC && b != DC {
                DC
            } else {
                26
            };
            [a, b, c]
        };
        if most_probable {
            return list[value as usize];
        }
        list.sort_unstable();
        let mut mode = value;
        for m in list {
            if mode >= m {
                mode += 1;
            }
        }
        mode
    }

    /// pcm_sample (7.3.8.7), then the arithmetic decoder starts again after it.
    fn pcm_samples(&mut self, log2: u8) -> Result<Vec<u16>> {
        let sps = self.p.sps;
        let pcm = sps.pcm.unwrap();
        let start = self.cabac.align()?;
        let data = self
            .cabac
            .data()
            .get(start..)
            .ok_or_else(|| Error::corrupt("PCM samples are cut short"))?;
        let mut b = Bits::new(data, false);
        let luma = 1usize << (2 * log2);
        let chroma = match self.chroma_format() {
            0 => 0,
            1 => luma / 4,
            2 => luma / 2,
            _ => luma,
        };
        let mut samples = Vec::with_capacity(luma + 2 * chroma);
        for i in 0..luma + 2 * chroma {
            let depth = if i < luma {
                pcm.bit_depth_luma
            } else {
                pcm.bit_depth_chroma
            };
            samples.push(b.u(u32::from(depth), "pcm_sample")? as u16);
        }
        self.cabac.restart(start + b.pos().div_ceil(8))?;
        Ok(samples)
    }

    /// transform_tree (7.3.8.8). `parent` holds the parent's cbf_cb, cbf_cr and, for 4:2:2,
    /// the second cbf_cb and cbf_cr.
    #[allow(clippy::too_many_arguments)]
    fn transform_tree(
        &mut self,
        cu: &CodingUnit,
        x: u32,
        y: u32,
        xb: u32,
        yb: u32,
        log2: u8,
        depth: u8,
        blk: u8,
        max_depth: u8,
        parent: [bool; 4],
    ) -> Result<()> {
        let sps = self.p.sps;
        let split = if log2 <= sps.log2_max_tb
            && log2 > sps.log2_min_tb
            && depth < max_depth
            && !(cu.nxn && depth == 0)
        {
            self.bin(ctx::SPLIT_TRANSFORM + 5 - log2 as usize)
        } else {
            log2 > sps.log2_max_tb || (cu.nxn && depth == 0)
        };
        let format = self.chroma_format();
        let mut cbf = [false; 4];
        if (log2 > 2 && format != 0) || format == 3 {
            for k in 0..2 {
                if depth == 0 || parent[k] {
                    cbf[k] = self.bin(ctx::CBF_CHROMA + depth as usize);
                    if format == 2 && (!split || log2 == 3) {
                        cbf[k + 2] = self.bin(ctx::CBF_CHROMA + depth as usize);
                    }
                }
            }
        }
        if split {
            let half = 1 << (log2 - 1);
            for i in 0..4u8 {
                let (x1, y1) = (x + u32::from(i % 2) * half, y + u32::from(i / 2) * half);
                self.transform_tree(cu, x1, y1, x, y, log2 - 1, depth + 1, i, max_depth, cbf)?;
            }
            return Ok(());
        }
        let luma = self.bin(ctx::CBF_LUMA + usize::from(depth == 0));
        self.transform_unit(cu, x, y, xb, yb, log2, blk, luma, cbf, parent)
    }

    /// transform_unit (7.3.8.10).
    #[allow(clippy::too_many_arguments)]
    fn transform_unit(
        &mut self,
        cu: &CodingUnit,
        x: u32,
        y: u32,
        xb: u32,
        yb: u32,
        log2: u8,
        blk: u8,
        luma: bool,
        cbf: [bool; 4],
        parent: [bool; 4],
    ) -> Result<()> {
        let format = self.chroma_format();
        // 4x4 luma blocks in 4:2:0 and 4:2:2 share one chroma block, coded with the last of them.
        let shared = format != 3 && log2 == 2;
        let chroma_cbf = if shared { parent } else { cbf };
        if (luma || chroma_cbf.iter().any(|&f| f))
            && self.p.pps.cu_qp_delta_depth.is_some()
            && !self.delta_coded
        {
            self.cu_qp_delta()?;
        }
        self.block(cu, 0, x, y, log2, luma)?;
        if format == 0 || (shared && blk != 3) {
            return Ok(());
        }
        let (sub_w, sub_h) = if format == 1 {
            (2, 2)
        } else if format == 2 {
            (2, 1)
        } else {
            (1, 1)
        };
        let log2c = if shared {
            2
        } else {
            log2 - u8::from(format != 3)
        };
        let (xc, yc) = if shared {
            (xb / sub_w, yb / sub_h)
        } else {
            (x / sub_w, y / sub_h)
        };
        let halves = if format == 2 { 2 } else { 1 };
        for c in 1..3u8 {
            for t in 0..halves {
                let coded = chroma_cbf[c as usize - 1 + 2 * t];
                self.block(cu, c, xc, yc + ((t as u32) << log2c), log2c, coded)?;
            }
        }
        Ok(())
    }

    /// cu_qp_delta_abs and cu_qp_delta_sign_flag, and QpY from them (8.6.1).
    fn cu_qp_delta(&mut self) -> Result<()> {
        let mut abs = 0;
        while abs < 5 && self.bin(ctx::CU_QP_DELTA_ABS + usize::from(abs > 0)) {
            abs += 1;
        }
        if abs == 5 {
            abs += self.exp_golomb(0)?;
        }
        let bd = 6 * (i32::from(self.p.sps.bit_depth_luma) - 8);
        let delta = if abs > 0 && self.cabac.bypass() {
            -(abs as i32)
        } else {
            abs as i32
        };
        if delta < -(26 + bd / 2) || delta > 25 + bd / 2 {
            return Err(Error::corrupt(format!("a QP change of {delta}")));
        }
        self.delta_coded = true;
        self.qp_y = (self.qg_pred + delta + 52 + 2 * bd).rem_euclid(52 + bd) - bd;
        Ok(())
    }

    /// k-th order Exp-Golomb in bypass bins.
    fn exp_golomb(&mut self, mut k: u32) -> Result<u32> {
        let mut v = 0u32;
        while self.cabac.bypass() {
            v = v
                .checked_add(1 << k)
                .ok_or_else(|| Error::corrupt("an Exp-Golomb code is too long"))?;
            k += 1;
            if k > 31 {
                return Err(Error::corrupt("an Exp-Golomb code is too long"));
            }
        }
        v.checked_add(self.cabac.bypass_bits(k))
            .ok_or_else(|| Error::corrupt("an Exp-Golomb code is too long"))
    }

    /// One transform block: its residual if it has one, then to the sink.
    fn block(
        &mut self,
        cu: &CodingUnit,
        c: u8,
        x: u32,
        y: u32,
        log2: u8,
        coded: bool,
    ) -> Result<()> {
        // The prediction block holding (x, y): one of four with NxN. 4:4:4 chroma is in luma units too.
        let half = 1 << (cu.log2_size - 1);
        let (xl, yl) = if c == 0 || self.chroma_format() == 3 {
            (x, y)
        } else {
            (cu.x, cu.y)
        };
        let part = if cu.nxn {
            usize::from(yl >= cu.y + half) * 2 + usize::from(xl >= cu.x + half)
        } else {
            0
        };
        let mode = if c == 0 {
            cu.luma_modes[part]
        } else {
            cu.chroma_modes[part]
        };
        let transform_skip = if coded {
            self.residual(c, log2, mode, cu.transquant_bypass)?
        } else {
            false
        };
        let n = 1usize << (2 * log2);
        let tb = TransformBlock {
            c_idx: c,
            x,
            y,
            log2_size: log2,
            mode,
            qp_y: self.qp_y,
            transquant_bypass: cu.transquant_bypass,
            transform_skip,
            coeffs: coded.then(|| &self.coeffs[..n]),
        };
        self.sink.transform_block(&tb);
        Ok(())
    }

    /// residual_coding (7.3.8.11) into `self.coeffs`; returns transform_skip_flag.
    fn residual(&mut self, c: u8, log2: u8, mode: u8, bypass: bool) -> Result<bool> {
        let size = 1usize << log2;
        self.coeffs[..size * size].fill(0);
        let pps = self.p.pps;
        let chroma = c > 0;
        let transform_skip = pps.transform_skip
            && !bypass
            && log2 <= pps.range.log2_max_transform_skip_size
            && self.bin(ctx::TRANSFORM_SKIP + usize::from(chroma));

        // The last significant coefficient's position.
        let (offset, shift) = if chroma {
            (15, log2 - 2)
        } else {
            (3 * (log2 - 2) + ((log2 - 1) >> 2), (log2 + 1) >> 2)
        };
        let max = (log2 << 1) - 1;
        let prefix = |run: &mut Self, base: usize| {
            let mut i = 0;
            while i < max && run.bin(base + offset as usize + (i >> shift) as usize) {
                i += 1;
            }
            i
        };
        let px = prefix(self, ctx::LAST_X_PREFIX);
        let py = prefix(self, ctx::LAST_Y_PREFIX);
        let suffix = |run: &mut Self, p: u8| -> u32 {
            if p > 3 {
                let bits = u32::from(p >> 1) - 1;
                (1 << bits) * (2 + u32::from(p & 1)) + run.cabac.bypass_bits(bits)
            } else {
                u32::from(p)
            }
        };
        let mut last_x = suffix(self, px) as usize;
        let mut last_y = suffix(self, py) as usize;
        let scan = scan_idx(mode, log2, c, self.chroma_format());
        if scan == 2 {
            std::mem::swap(&mut last_x, &mut last_y);
        }
        if last_x >= size || last_y >= size {
            return Err(Error::corrupt("a block's last coefficient is outside it"));
        }

        let log2_sb = (log2 - 2) as usize;
        let n_sb = 1usize << log2_sb;
        let scans = &self.p.scans;
        let i_last = scans.inv[log2_sb][scan][(last_y >> 2) * n_sb + (last_x >> 2)] as usize;
        let n_last = scans.inv[2][scan][(last_y & 3) * 4 + (last_x & 3)] as usize;
        let sub_pos: Vec<(u8, u8)> = scans.pos[log2_sb][scan].clone();
        let pos: [(u8, u8); 16] = scans.pos[2][scan][..].try_into().unwrap();

        let mut coded_sb = [[false; 8]; 8];
        let mut greater1_ctx = 1u8;
        let sign_hiding = pps.sign_data_hiding;
        for i in (0..=i_last).rev() {
            let (xs, ys) = (sub_pos[i].0 as usize, sub_pos[i].1 as usize);
            let right = xs + 1 < n_sb && coded_sb[xs + 1][ys];
            let below = ys + 1 < n_sb && coded_sb[xs][ys + 1];
            let mut infer_dc = false;
            let coded = if i < i_last && i > 0 {
                let f = self.bin(
                    ctx::CODED_SUB_BLOCK + usize::from(right || below) + if chroma { 2 } else { 0 },
                );
                infer_dc = true;
                f
            } else {
                true
            };
            coded_sb[xs][ys] = coded;

            // Which coefficients are significant, in decreasing scan order.
            let mut sig = [false; 16];
            let start = if i == i_last {
                sig[n_last] = true;
                n_last as i32 - 1
            } else {
                15
            };
            if coded {
                for n in (0..=start).rev() {
                    let n = n as usize;
                    if n > 0 || !infer_dc {
                        let (xc, yc) = (xs * 4 + pos[n].0 as usize, ys * 4 + pos[n].1 as usize);
                        let sig_ctx = sig_ctx(
                            xc,
                            yc,
                            log2,
                            chroma,
                            scan,
                            xs,
                            ys,
                            u8::from(right) + 2 * u8::from(below),
                        );
                        sig[n] = self.bin(ctx::SIG_COEFF + sig_ctx);
                        if sig[n] {
                            infer_dc = false;
                        }
                    } else {
                        sig[n] = true;
                    }
                }
            }
            let order: Vec<usize> = (0..16).rev().filter(|&n| sig[n]).collect();
            if order.is_empty() {
                continue;
            }

            // Greater than 1 (for the first eight), greater than 2 (for the first of those).
            let mut ctx_set = if i == 0 || chroma { 0 } else { 2 };
            if i != i_last && greater1_ctx == 0 {
                ctx_set += 1;
            }
            greater1_ctx = 1;
            let mut greater1 = [false; 16];
            let mut first_greater1 = None;
            for &n in order.iter().take(8) {
                let inc = ctx_set * 4 + greater1_ctx.min(3) as usize + if chroma { 16 } else { 0 };
                let f = self.bin(ctx::GREATER1 + inc);
                greater1[n] = f;
                if f {
                    greater1_ctx = 0;
                    first_greater1.get_or_insert(n);
                } else if greater1_ctx > 0 && greater1_ctx < 3 {
                    greater1_ctx += 1;
                }
            }
            let mut greater2 = [false; 16];
            if let Some(n) = first_greater1 {
                greater2[n] = self.bin(ctx::GREATER2 + ctx_set + if chroma { 4 } else { 0 });
            }
            let (last_sig, first_sig) = (order[0], *order.last().unwrap());
            let sign_hidden = sign_hiding && !bypass && last_sig - first_sig > 3;
            let mut negative = [false; 16];
            for &n in &order {
                if !sign_hidden || n != first_sig {
                    negative[n] = self.cabac.bypass();
                }
            }

            // The rest of each level, with the Rice parameter adapting as it goes.
            let mut rice = 0u32;
            let mut sum = 0u32;
            for (k, &n) in order.iter().enumerate() {
                let base = 1 + u32::from(greater1[n]) + u32::from(greater2[n]);
                let threshold = if k < 8 {
                    if Some(n) == first_greater1 { 3 } else { 2 }
                } else {
                    1
                };
                let mut level = base;
                if base == threshold {
                    level += self.level_remaining(rice)?;
                    if level > 3 << rice {
                        rice = (rice + 1).min(4);
                    }
                }
                if level > 1 << 15 {
                    return Err(Error::corrupt("a coefficient is too large"));
                }
                let mut v = if negative[n] {
                    -(level as i32)
                } else {
                    level as i32
                };
                if sign_hidden {
                    sum += level;
                    if n == first_sig && sum % 2 == 1 {
                        v = -v;
                    }
                }
                let (xc, yc) = (xs * 4 + pos[n].0 as usize, ys * 4 + pos[n].1 as usize);
                self.coeffs[yc * size + xc] = v;
            }
        }
        Ok(transform_skip)
    }

    /// coeff_abs_level_remaining (9.3.3.11): a unary prefix, then a Rice or Exp-Golomb suffix.
    fn level_remaining(&mut self, rice: u32) -> Result<u32> {
        let mut prefix = 0;
        while prefix < 32 && self.cabac.bypass() {
            prefix += 1;
        }
        if prefix < 3 {
            return Ok((prefix << rice) + self.cabac.bypass_bits(rice));
        }
        let k = prefix - 3 + rice;
        if k > 22 {
            return Err(Error::corrupt("a coefficient is too large"));
        }
        Ok((((1 << (prefix - 3)) + 2) << rice) + self.cabac.bypass_bits(k))
    }
}

/// Table 8-2 (and 8-3 for 4:2:2): the chroma mode from intra_chroma_pred_mode and the luma mode.
fn chroma_mode(idc: u8, luma: u8, format: u8) -> u8 {
    let mut mode = match idc {
        0 => PLANAR,
        1 => 26,
        2 => 10,
        3 => DC,
        _ => luma,
    };
    if idc < 4 && mode == luma {
        mode = 34;
    }
    if format == 2 {
        MODE_422[mode as usize]
    } else {
        mode
    }
}

/// scanIdx (7.4.9.11): small intra blocks predicted near horizontally are scanned vertically,
/// and the other way round.
fn scan_idx(mode: u8, log2: u8, c: u8, format: u8) -> usize {
    if log2 == 2 || (log2 == 3 && (c == 0 || format == 3)) {
        match mode {
            6..=14 => 2,
            22..=30 => 1,
            _ => 0,
        }
    } else {
        0
    }
}

/// sig_coeff_flag's context (9.3.4.2.5). `prev` says which of the sub-blocks to the right (1)
/// and below (2) have coefficients.
#[allow(clippy::too_many_arguments)]
fn sig_ctx(
    xc: usize,
    yc: usize,
    log2: u8,
    chroma: bool,
    scan: usize,
    xs: usize,
    ys: usize,
    prev: u8,
) -> usize {
    let s = if log2 == 2 {
        CTX_IDX_MAP[(yc << 2) + xc] as usize
    } else if xc + yc == 0 {
        0
    } else {
        let (xp, yp) = (xc & 3, yc & 3);
        let mut s = match prev {
            0 => match xp + yp {
                0 => 2,
                1 | 2 => 1,
                _ => 0,
            },
            1 => 2 - yp.min(2),
            2 => 2 - xp.min(2),
            _ => 2,
        };
        if !chroma {
            if (xs, ys) != (0, 0) {
                s += 3;
            }
            s += if log2 == 3 {
                if scan == 0 { 9 } else { 15 }
            } else {
                21
            };
        } else {
            s += if log2 == 3 { 9 } else { 12 };
        }
        s
    };
    if chroma { 27 + s } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_orders() {
        let s = Scans::new();
        assert_eq!(s.pos[1][0], [(0, 0), (0, 1), (1, 0), (1, 1)]);
        assert_eq!(
            s.pos[2][0][..6],
            [(0, 0), (0, 1), (1, 0), (0, 2), (1, 1), (2, 0)]
        );
        assert_eq!(*s.pos[2][0].last().unwrap(), (3, 3));
        assert_eq!(s.pos[1][1], [(0, 0), (1, 0), (0, 1), (1, 1)]);
        assert_eq!(s.pos[1][2], [(0, 0), (0, 1), (1, 0), (1, 1)]);
        for log2 in 0..4 {
            for scan in 0..3 {
                let n = 1 << log2;
                for (i, &(x, y)) in s.pos[log2][scan].iter().enumerate() {
                    assert_eq!(s.inv[log2][scan][y as usize * n + x as usize] as usize, i);
                }
            }
        }
    }

    #[test]
    fn chroma_modes() {
        assert_eq!(chroma_mode(4, 17, 1), 17, "same as luma");
        assert_eq!(
            chroma_mode(1, 26, 1),
            34,
            "vertical taken by luma: 34 instead"
        );
        assert_eq!(chroma_mode(1, 10, 1), 26);
        assert_eq!(chroma_mode(0, 3, 2), 0);
        assert_eq!(chroma_mode(2, 26, 2), MODE_422[10]);
        assert_eq!(chroma_mode(4, 34, 2), 31);
    }

    #[test]
    fn significance_contexts() {
        // 4x4: the fixed map; chroma's contexts come after luma's 27.
        assert_eq!(sig_ctx(0, 0, 2, false, 0, 0, 0, 0), 0);
        assert_eq!(sig_ctx(3, 2, 2, false, 0, 0, 0, 0), 8);
        assert_eq!(sig_ctx(1, 0, 2, true, 0, 0, 0, 0), 28);
        // 8x8 luma, DC of the block, then elsewhere in the first and later sub-blocks.
        assert_eq!(sig_ctx(0, 0, 3, false, 0, 0, 0, 0), 0);
        assert_eq!(sig_ctx(1, 0, 3, false, 0, 0, 0, 0), 1 + 9);
        assert_eq!(sig_ctx(1, 0, 3, false, 1, 0, 0, 0), 1 + 15);
        assert_eq!(sig_ctx(4, 0, 3, false, 0, 1, 0, 0), 2 + 3 + 9);
        // 16x16 luma with both neighbours coded: always 2.
        assert_eq!(sig_ctx(7, 6, 4, false, 0, 1, 1, 3), 2 + 3 + 21);
        assert_eq!(sig_ctx(5, 5, 4, true, 0, 1, 1, 1), 27 + 1 + 12);
    }
}
