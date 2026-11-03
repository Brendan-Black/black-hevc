//! CABAC (H.265 9.3): the arithmetic decoder that slice data is coded with, and the context
//! variables (adaptive probabilities) of each syntax element in I slices.

// The slice data decoder (the next piece) is what uses most of this.

use crate::{Error, Result};

/// rangeTabLps (Table 9-52): the less probable symbol's share of the range, by probability
/// state and the range's quarter.
const LPS_RANGE: [[u8; 4]; 64] = [
    [128, 176, 208, 240],
    [128, 167, 197, 227],
    [128, 158, 187, 216],
    [123, 150, 178, 205],
    [116, 142, 169, 195],
    [111, 135, 160, 185],
    [105, 128, 152, 175],
    [100, 122, 144, 166],
    [95, 116, 137, 158],
    [90, 110, 130, 150],
    [85, 104, 123, 142],
    [81, 99, 117, 135],
    [77, 94, 111, 128],
    [73, 89, 105, 122],
    [69, 85, 100, 116],
    [66, 80, 95, 110],
    [62, 76, 90, 104],
    [59, 72, 86, 99],
    [56, 69, 81, 94],
    [53, 65, 77, 89],
    [51, 62, 73, 85],
    [48, 59, 69, 80],
    [46, 56, 66, 76],
    [43, 53, 63, 72],
    [41, 50, 59, 69],
    [39, 48, 56, 65],
    [37, 45, 54, 62],
    [35, 43, 51, 59],
    [33, 41, 48, 56],
    [32, 39, 46, 53],
    [30, 37, 43, 50],
    [29, 35, 41, 48],
    [27, 33, 39, 45],
    [26, 31, 37, 43],
    [24, 30, 35, 41],
    [23, 28, 33, 39],
    [22, 27, 32, 37],
    [21, 26, 30, 35],
    [20, 24, 29, 33],
    [19, 23, 27, 31],
    [18, 22, 26, 30],
    [17, 21, 25, 28],
    [16, 20, 23, 27],
    [15, 19, 22, 25],
    [14, 18, 21, 24],
    [14, 17, 20, 23],
    [13, 16, 19, 22],
    [12, 15, 18, 21],
    [12, 14, 17, 20],
    [11, 14, 16, 19],
    [11, 13, 15, 18],
    [10, 12, 15, 17],
    [10, 12, 14, 16],
    [9, 11, 13, 15],
    [9, 11, 12, 14],
    [8, 10, 12, 14],
    [8, 9, 11, 13],
    [7, 9, 11, 12],
    [7, 9, 10, 12],
    [7, 8, 10, 11],
    [6, 8, 9, 11],
    [6, 7, 9, 10],
    [6, 7, 8, 9],
    [2, 2, 2, 2],
];

/// transIdxLps (Table 9-53): the state after a less probable symbol. After a more probable
/// one it's the next state up, stopping at 62 (63 is only for terminating).
const NEXT_LPS: [u8; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12, 13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21,
    21, 22, 22, 23, 24, 24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33, 33, 33, 34,
    34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

/// One context variable: a probability state (0 is even odds, 62 the most skewed) and which
/// value is the more probable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Context {
    state: u8,
    mps: bool,
}

impl Context {
    /// Initialize from a table's initValue at the slice's QP (9.3.2.2).
    pub(crate) fn new(init_value: u8, qp: i32) -> Context {
        let m = i32::from(init_value >> 4) * 5 - 45;
        let n = (i32::from(init_value & 15) << 3) - 16;
        let pre = ((m * qp.clamp(0, 51)) >> 4) + n;
        let pre = pre.clamp(1, 126);
        if pre <= 63 {
            Context {
                state: (63 - pre) as u8,
                mps: false,
            }
        } else {
            Context {
                state: (pre - 64) as u8,
                mps: true,
            }
        }
    }
}

/// Where each syntax element's contexts start, in the order of Table 9-4, and how many it has.
/// Only what I slices use: keyframes are intra pictures.
pub(crate) mod ctx {
    pub const SAO_MERGE: usize = 0;
    pub const SAO_TYPE: usize = 1;
    pub const SPLIT_CU: usize = 2; // 3
    pub const TRANSQUANT_BYPASS: usize = 5;
    pub const CU_QP_DELTA_ABS: usize = 6; // 2
    pub const PART_MODE: usize = 8;
    pub const PREV_INTRA_LUMA_PRED: usize = 9;
    pub const INTRA_CHROMA_PRED_MODE: usize = 10;
    pub const SPLIT_TRANSFORM: usize = 11; // 3
    pub const CBF_LUMA: usize = 14; // 2
    pub const CBF_CHROMA: usize = 16; // 5: the fifth for 4:2:2's deeper chroma split
    pub const TRANSFORM_SKIP: usize = 21; // luma, chroma
    pub const LAST_X_PREFIX: usize = 23; // 18
    pub const LAST_Y_PREFIX: usize = 41; // 18
    pub const CODED_SUB_BLOCK: usize = 59; // 4
    pub const SIG_COEFF: usize = 63; // 44: 42, then luma and chroma with transform_skip_context
    pub const GREATER1: usize = 107; // 24
    pub const GREATER2: usize = 131; // 6
    // The range extension's cross-component prediction and chroma QP offsets, not supported yet.
    #[allow(dead_code)]
    pub const LOG2_RES_SCALE_ABS: usize = 137; // 8
    #[allow(dead_code)]
    pub const RES_SCALE_SIGN: usize = 145; // 2
    #[allow(dead_code)]
    pub const CU_CHROMA_QP_OFFSET_FLAG: usize = 147;
    #[allow(dead_code)]
    pub const CU_CHROMA_QP_OFFSET_IDX: usize = 148;
    pub const COUNT: usize = 149;
}

/// initValue for each context in I slices (initType 0), from Tables 9-5 to 9-37.
const INIT_I: [u8; ctx::COUNT] = [
    153, // sao_merge_left_flag and sao_merge_up_flag
    200, // sao_type_idx_luma and _chroma
    139, 141, 157, // split_cu_flag
    154, // cu_transquant_bypass_flag
    154, 154, // cu_qp_delta_abs
    184, // part_mode
    184, // prev_intra_luma_pred_flag
    63,  // intra_chroma_pred_mode
    153, 138, 138, // split_transform_flag
    111, 141, // cbf_luma
    94, 138, 182, 154, 154, // cbf_cb and cbf_cr
    139, 139, // transform_skip_flag
    110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123,
    63, // last_sig_coeff_x_prefix
    110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123,
    63, // last_sig_coeff_y_prefix
    91, 171, 134, 141, // coded_sub_block_flag
    111, 111, 125, 110, 110, 94, 124, 108, 124, 107, 125, 141, 179, 153, 125, 107, 125, 141, 179,
    153, 125, 107, 125, 141, 179, 153, 125, 140, 139, 182, 182, 152, 136, 152, 136, 153, 136, 139,
    111, 136, 139, 111, // sig_coeff_flag
    141, 111, // sig_coeff_flag with transform_skip_context_enabled_flag
    140, 92, 137, 138, 140, 152, 138, 139, 153, 74, 149, 92, 139, 107, 122, 152, 140, 179, 166,
    182, 140, 227, 122, 197, // greater1
    138, 153, 136, 167, 152, 152, // coeff_abs_level_greater2_flag
    154, 154, 154, 154, 154, 154, 154, 154, // log2_res_scale_abs_plus1
    154, 154, // res_scale_sign_flag
    154, // cu_chroma_qp_offset_flag
    154, // cu_chroma_qp_offset_idx
];

/// Every context of a slice, plus the Rice parameter statistics (StatCoeff) that the range
/// extension keeps alongside them. Wavefronts save and restore the whole set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Contexts {
    pub(crate) ctx: [Context; ctx::COUNT],
    pub(crate) stat_coeff: [u8; 4],
}

impl Contexts {
    pub(crate) fn new(qp: i32) -> Contexts {
        Contexts {
            ctx: INIT_I.map(|v| Context::new(v, qp)),
            stat_coeff: [0; 4],
        }
    }
}

/// The arithmetic decoding engine (9.3.4.3), reading slice data from a byte position.
pub(crate) struct Cabac<'a> {
    data: &'a [u8],
    /// The next bit to read.
    pos: usize,
    range: u32,
    offset: u32,
}

impl<'a> Cabac<'a> {
    /// Start decoding at byte `start` of `data` (9.3.2.5).
    pub(crate) fn new(data: &'a [u8], start: usize) -> Result<Cabac<'a>> {
        let mut c = Cabac {
            data,
            pos: start * 8,
            range: 510,
            offset: 0,
        };
        c.start()?;
        Ok(c)
    }

    fn start(&mut self) -> Result<()> {
        self.range = 510;
        self.offset = 0;
        for _ in 0..9 {
            self.offset = self.offset << 1 | self.bit();
        }
        if self.offset >= 510 {
            return Err(Error::corrupt(
                "slice data starts with an invalid arithmetic code",
            ));
        }
        Ok(())
    }

    /// The next bit; zeros past the end, which [`Cabac::overrun`] reports.
    fn bit(&mut self) -> u32 {
        let byte = self.data.get(self.pos / 8).copied().unwrap_or(0);
        let b = byte >> (7 - self.pos % 8) & 1;
        self.pos += 1;
        u32::from(b)
    }

    /// The data being decoded.
    pub(crate) fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Whether decoding has read past the end of the data: the slice is cut short or corrupt.
    pub(crate) fn overrun(&self) -> bool {
        self.pos > self.data.len() * 8
    }

    /// DecodeDecision (9.3.4.3.2): one bin with an adaptive probability.
    pub(crate) fn decision(&mut self, c: &mut Context) -> bool {
        let lps = u32::from(LPS_RANGE[c.state as usize][(self.range >> 6 & 3) as usize]);
        self.range -= lps;
        let bin;
        if self.offset >= self.range {
            bin = !c.mps;
            self.offset -= self.range;
            self.range = lps;
            if c.state == 0 {
                c.mps = !c.mps;
            }
            c.state = NEXT_LPS[c.state as usize];
        } else {
            bin = c.mps;
            if c.state < 62 {
                c.state += 1;
            }
        }
        while self.range < 256 {
            self.range <<= 1;
            self.offset = self.offset << 1 | self.bit();
        }
        bin
    }

    /// DecodeBypass (9.3.4.3.4): one bin with even odds.
    pub(crate) fn bypass(&mut self) -> bool {
        self.offset = self.offset << 1 | self.bit();
        if self.offset >= self.range {
            self.offset -= self.range;
            true
        } else {
            false
        }
    }

    /// `n` bypass bins as a number, first bin most significant.
    pub(crate) fn bypass_bits(&mut self, n: u32) -> u32 {
        (0..n).fold(0, |v, _| v << 1 | u32::from(self.bypass()))
    }

    /// DecodeTerminate (9.3.4.3.5): end_of_slice_segment_flag, end_of_subset_one_bit and
    /// pcm_flag. After a 1 the arithmetic code has ended, its last bit read.
    pub(crate) fn terminate(&mut self) -> bool {
        self.range -= 2;
        if self.offset >= self.range {
            return true;
        }
        while self.range < 256 {
            self.range <<= 1;
            self.offset = self.offset << 1 | self.bit();
        }
        false
    }

    /// After a terminating 1: the zero bits up to the next byte, then where the data goes on.
    pub(crate) fn align(&mut self) -> Result<usize> {
        while !self.pos.is_multiple_of(8) {
            if self.bit() != 0 {
                return Err(Error::corrupt(
                    "slice data isn't byte-aligned where it should be",
                ));
            }
        }
        Ok(self.pos / 8)
    }

    /// After a terminating 1: start a new arithmetic code at the next byte, as each wavefront
    /// row or tile does (and decoding does after PCM samples, from `byte`).
    pub(crate) fn restart(&mut self, byte: usize) -> Result<()> {
        self.pos = byte * 8;
        self.start()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arithmetic encoder (as H.264 9.3.4 describes it, which HEVC's matches).
    struct Encoder {
        out: Vec<bool>,
        low: u32,
        range: u32,
        outstanding: u32,
        first: bool,
    }

    impl Encoder {
        fn new() -> Encoder {
            Encoder {
                out: Vec::new(),
                low: 0,
                range: 510,
                outstanding: 0,
                first: true,
            }
        }

        fn put(&mut self, b: bool) {
            if self.first {
                self.first = false;
            } else {
                self.out.push(b);
            }
            for _ in 0..self.outstanding {
                self.out.push(!b);
            }
            self.outstanding = 0;
        }

        fn renorm(&mut self) {
            while self.range < 256 {
                if self.low < 256 {
                    self.put(false);
                } else if self.low >= 512 {
                    self.low -= 512;
                    self.put(true);
                } else {
                    self.low -= 256;
                    self.outstanding += 1;
                }
                self.range <<= 1;
                self.low <<= 1;
            }
        }

        fn decision(&mut self, c: &mut Context, bin: bool) {
            let lps = u32::from(LPS_RANGE[c.state as usize][(self.range >> 6 & 3) as usize]);
            self.range -= lps;
            if bin != c.mps {
                self.low += self.range;
                self.range = lps;
                if c.state == 0 {
                    c.mps = !c.mps;
                }
                c.state = NEXT_LPS[c.state as usize];
            } else if c.state < 62 {
                c.state += 1;
            }
            self.renorm();
        }

        fn bypass(&mut self, bin: bool) {
            self.low <<= 1;
            if bin {
                self.low += self.range;
            }
            if self.low >= 1024 {
                self.put(true);
                self.low -= 1024;
            } else if self.low < 512 {
                self.put(false);
            } else {
                self.low -= 512;
                self.outstanding += 1;
            }
        }

        fn terminate(&mut self, bin: bool) {
            self.range -= 2;
            if bin {
                self.low += self.range;
                // Flush: the last bit written is a 1 (the stop bit), then pad to a byte.
                self.range = 2;
                self.renorm();
                self.put(self.low >> 9 & 1 == 1);
                self.out.push(self.low >> 8 & 1 == 1);
                self.out.push(true);
                while !self.out.len().is_multiple_of(8) {
                    self.out.push(false);
                }
                // A new arithmetic code follows, as at a wavefront row.
                (self.low, self.range, self.outstanding, self.first) = (0, 510, 0, true);
            } else {
                self.renorm();
            }
        }

        fn bytes(&self) -> Vec<u8> {
            self.out
                .chunks(8)
                .map(|c| {
                    c.iter()
                        .enumerate()
                        .fold(0, |b, (i, &x)| b | (x as u8) << (7 - i))
                })
                .collect()
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Bin {
        Decision(usize, bool),
        Bypass(bool),
        Terminate(bool),
    }

    fn bins(seed: u64, n: usize) -> Vec<Bin> {
        let mut s = seed;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        (0..n)
            .map(|i| {
                let r = next();
                // Mostly skewed decisions, as real data is, so states climb and fall.
                let ctx = (r % 7) as usize;
                let skewed = (r >> 8) % 16 < if ctx < 4 { 14 } else { 8 };
                match (r >> 20) % 20 {
                    0..=13 => Bin::Decision(ctx, skewed == ctx.is_multiple_of(2)),
                    14..=17 => Bin::Bypass(r >> 30 & 1 == 1),
                    _ => Bin::Terminate(i % 97 == 96),
                }
            })
            .collect()
    }

    #[test]
    fn round_trips_through_the_encoder() {
        for (seed, qp) in [(1, 26), (2, 0), (3, 51), (0x9E37_79B9_7F4A_7C15, 37)] {
            let script = bins(seed, 20_000);
            let inits = [154, 63, 200, 139, 111, 227, 1];
            let mut enc = Encoder::new();
            let mut ectx = inits.map(|v| Context::new(v, qp));
            for &b in &script {
                match b {
                    Bin::Decision(c, bin) => enc.decision(&mut ectx[c], bin),
                    Bin::Bypass(bin) => enc.bypass(bin),
                    Bin::Terminate(bin) => enc.terminate(bin),
                }
            }
            enc.terminate(true);
            let data = enc.bytes();
            let mut dec = Cabac::new(&data, 0).unwrap();
            let mut dctx = inits.map(|v| Context::new(v, qp));
            for (i, &b) in script.iter().enumerate() {
                let got = match b {
                    Bin::Decision(c, _) => dec.decision(&mut dctx[c]),
                    Bin::Bypass(_) => dec.bypass(),
                    Bin::Terminate(_) => dec.terminate(),
                };
                let want = match b {
                    Bin::Decision(_, v) | Bin::Bypass(v) | Bin::Terminate(v) => v,
                };
                assert_eq!(got, want, "bin {i} ({b:?}), seed {seed}");
                if matches!(b, Bin::Terminate(true)) {
                    let next = dec.align().unwrap();
                    dec.restart(next).unwrap();
                }
            }
            assert!(dec.terminate());
            assert_eq!(
                dec.align().unwrap(),
                data.len(),
                "the data ends with the last code"
            );
            assert!(!dec.overrun());
            assert_eq!(dctx, ectx, "both sides end in the same states");
        }
    }

    #[test]
    fn a_lone_terminate_is_nine_bits() {
        // 111111101: the shortest arithmetic code, and its last bit is the stop bit.
        let data = [0b1111_1110, 0b1000_0000];
        let mut dec = Cabac::new(&data, 0).unwrap();
        assert!(dec.terminate());
        assert_eq!(dec.align(), Ok(2));
        assert!(Cabac::new(&[0xFF, 0x80], 0).is_err(), "an offset of 511");
        assert!(Cabac::new(&[0x12], 0).unwrap().overrun(), "only 8 bits");
        assert_eq!(
            Cabac::new(&[0xFE, 0xC0], 0)
                .map(|mut d| (d.terminate(), d.align()))
                .unwrap(),
            (
                true,
                Err(Error::corrupt(
                    "slice data isn't byte-aligned where it should be"
                ))
            )
        );
    }

    #[test]
    fn initializes_contexts() {
        // 154 is even odds at any QP. 63 leans towards 0; 227 leans towards 0 at QP 26 and to 1 at 51.
        for qp in [0, 26, 51, -5, 70] {
            assert_eq!(
                Context::new(154, qp),
                Context {
                    state: 0,
                    mps: true
                },
                "QP {qp}"
            );
        }
        assert_eq!(
            Context::new(63, 26),
            Context {
                state: 8,
                mps: false
            }
        );
        assert_eq!(
            Context::new(227, 26),
            Context {
                state: 15,
                mps: false
            }
        );
        assert_eq!(
            Context::new(227, 51),
            Context {
                state: 23,
                mps: true
            }
        );
        assert_eq!(
            Context::new(1, 0),
            Context {
                state: 62,
                mps: false
            },
            "clipped to state 62"
        );
        let all = Contexts::new(30);
        assert_eq!(all.ctx[ctx::SPLIT_CU], Context::new(139, 30));
        assert_eq!(all.ctx[ctx::CU_CHROMA_QP_OFFSET_IDX], Context::new(154, 30));
        assert_eq!(all.ctx[ctx::GREATER2 + 5], Context::new(152, 30));
        assert_eq!(all.ctx[ctx::SIG_COEFF + 43], Context::new(111, 30));
    }
}
