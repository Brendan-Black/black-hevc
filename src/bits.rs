//! The bits of a NAL unit: removing emulation prevention bytes, then reading fixed-width and
//! Exp-Golomb fields, optionally recording each one as it's read.

use crate::{Error, Result};

/// One syntax element as it was read: where it starts (in bits from the start of the NAL
/// unit, header included, emulation prevention removed), its name as the standard gives it,
/// and its value. ffmpeg's `trace_headers` filter prints the same.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Syntax {
    pub pos: usize,
    pub name: String,
    pub value: i64,
}

/// A NAL unit's payload as the standard reads it: each `00 00 03` loses its `03`, which the
/// encoder put there so the data never looks like a start code.
pub fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

pub(crate) struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    trace: Option<Vec<Syntax>>,
}

impl<'a> Bits<'a> {
    pub(crate) fn new(data: &'a [u8], trace: bool) -> Self {
        Bits {
            data,
            pos: 0,
            trace: trace.then(Vec::new),
        }
    }

    /// How many bits have been read.
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn into_trace(self) -> Vec<Syntax> {
        self.trace.unwrap_or_default()
    }

    fn left(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    fn read(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        if self.left() < n as usize {
            return Err(Error::corrupt("a header is cut short"));
        }
        let mut v = 0u32;
        for _ in 0..n {
            let bit = self.data[self.pos / 8] >> (7 - self.pos % 8) & 1;
            v = v << 1 | u32::from(bit);
            self.pos += 1;
        }
        Ok(v)
    }

    fn record(&mut self, pos: usize, name: &str, value: i64) {
        if let Some(t) = &mut self.trace {
            t.push(Syntax {
                pos,
                name: name.to_string(),
                value,
            });
        }
    }

    /// An `n`-bit unsigned field.
    pub(crate) fn u(&mut self, n: u32, name: &str) -> Result<u32> {
        let pos = self.pos;
        let v = self.read(n)?;
        self.record(pos, name, i64::from(v));
        Ok(v)
    }

    pub(crate) fn flag(&mut self, name: &str) -> Result<bool> {
        Ok(self.u(1, name)? == 1)
    }

    /// Skip `n` bits without recording them.
    pub(crate) fn skip(&mut self, n: usize) -> Result<()> {
        if self.left() < n {
            return Err(Error::corrupt("a header is cut short"));
        }
        self.pos += n;
        Ok(())
    }

    fn exp_golomb(&mut self) -> Result<u64> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(Error::corrupt("an Exp-Golomb code is too long"));
            }
        }
        Ok((1u64 << zeros) - 1 + u64::from(self.read(zeros)?))
    }

    /// An unsigned Exp-Golomb field, which must be at most `max`.
    pub(crate) fn ue(&mut self, name: &str, max: u32) -> Result<u32> {
        let pos = self.pos;
        let v = self.exp_golomb()?;
        self.record(pos, name, v as i64);
        if v > u64::from(max) {
            return Err(Error::corrupt(format!("{name} is {v}, more than {max}")));
        }
        Ok(v as u32)
    }

    /// A signed Exp-Golomb field, which must be in `min..=max`.
    pub(crate) fn se(&mut self, name: &str, min: i32, max: i32) -> Result<i32> {
        let pos = self.pos;
        let k = self.exp_golomb()? as i64;
        let v = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
        self.record(pos, name, v);
        if v < i64::from(min) || v > i64::from(max) {
            return Err(Error::corrupt(format!(
                "{name} is {v}, outside {min} to {max}"
            )));
        }
        Ok(v as i32)
    }

    /// The end of a parameter set: a 1 bit, then zeros.
    pub(crate) fn trailing(&mut self) -> Result<()> {
        if !self.flag("rbsp_stop_one_bit")? {
            return Err(Error::corrupt("a header doesn't end where it should"));
        }
        while !self.pos.is_multiple_of(8) {
            if self.flag("rbsp_alignment_zero_bit")? {
                return Err(Error::corrupt("a header doesn't end where it should"));
            }
        }
        if self.data[self.pos / 8..].iter().any(|&b| b != 0) {
            return Err(Error::corrupt("a header has data after its end"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_emulation_prevention() {
        assert_eq!(rbsp(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3]), [0, 0, 1, 0, 0, 0, 0]);
        // Only after two zeros, and the 03 itself doesn't count as one.
        assert_eq!(rbsp(&[0, 3, 0, 0, 3, 3, 0, 0, 3]), [0, 3, 0, 0, 3, 0, 0]);
    }

    #[test]
    fn reads_fields() {
        // 101 | 1 | 010 | 011 | 00100 | 00101 | 0001000 | 1 then trailing bits.
        let data = [0b1011_0100, 0b1100_1000, 0b0101_0001, 0b0001_1000];
        let mut b = Bits::new(&data, true);
        assert_eq!(b.u(3, "a").unwrap(), 5);
        assert_eq!(b.ue("b", 10).unwrap(), 0);
        assert_eq!(b.ue("c", 10).unwrap(), 1);
        assert_eq!(b.ue("d", 10).unwrap(), 2);
        assert_eq!(b.se("e", -5, 5).unwrap(), 2);
        assert_eq!(b.se("f", -5, 5).unwrap(), -2);
        assert_eq!(b.ue("g", 10).unwrap(), 7);
        assert!(b.flag("h").unwrap());
        b.trailing().unwrap();
        let trace = b.into_trace();
        let got: Vec<(usize, &str, i64)> = trace
            .iter()
            .map(|s| (s.pos, s.name.as_str(), s.value))
            .collect();
        assert_eq!(
            got[..8],
            [
                (0, "a", 5),
                (3, "b", 0),
                (4, "c", 1),
                (7, "d", 2),
                (10, "e", 2),
                (15, "f", -2),
                (20, "g", 7),
                (27, "h", 1)
            ]
        );
        assert_eq!(got[8], (28, "rbsp_stop_one_bit", 1));
        assert_eq!(got.len(), 12);
    }

    #[test]
    fn rejects_bad_fields() {
        let mut b = Bits::new(&[0b0001_0000], false);
        assert!(b.ue("x", 6).is_err(), "7 is more than 6");
        assert!(
            Bits::new(&[0, 0, 0, 0, 0], false)
                .ue("x", u32::MAX)
                .is_err(),
            "too long"
        );
        assert!(
            Bits::new(&[0b1000_0000, 1], false).trailing().is_err(),
            "data after the end"
        );
        assert!(
            Bits::new(&[0b1100_0000], false).trailing().is_err(),
            "a 1 in the alignment"
        );
        assert!(Bits::new(&[0xFF], false).u(9, "x").is_err());
        // The largest value: 31 zeros, a 1, then 31 ones.
        let max = [0, 0, 0, 1, 0xFF, 0xFF, 0xFF, 0xFE];
        assert_eq!(Bits::new(&max, false).ue("x", u32::MAX), Ok(u32::MAX - 1));
    }
}
