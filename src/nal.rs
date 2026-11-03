//! NAL unit headers (H.265 7.3.1.2).

use crate::bits::Bits;
use crate::{Error, Result};

pub const VPS: u8 = 32;
pub const SPS: u8 = 33;
pub const PPS: u8 = 34;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NalHeader {
    /// `nal_unit_type`: 0 to 31 are slices, 32 and up parameter sets, SEI and the like.
    pub kind: u8,
    /// `nuh_layer_id`: 0 for the base layer, the only one a plain HEVC decoder shows.
    pub layer: u8,
    pub temporal_id: u8,
}

impl NalHeader {
    /// The header of a NAL unit: its first two bytes.
    pub fn parse(nal: &[u8]) -> Result<NalHeader> {
        NalHeader::read(&mut Bits::new(nal, false))
    }

    pub(crate) fn read(b: &mut Bits) -> Result<NalHeader> {
        if b.flag("forbidden_zero_bit")? {
            return Err(Error::corrupt("a NAL unit's forbidden bit is set"));
        }
        let kind = b.u(6, "nal_unit_type")? as u8;
        let layer = b.u(6, "nuh_layer_id")? as u8;
        let temporal_id = match b.u(3, "nuh_temporal_id_plus1")? {
            0 => return Err(Error::corrupt("a NAL unit's temporal ID is 0")),
            t => t as u8 - 1,
        };
        Ok(NalHeader {
            kind,
            layer,
            temporal_id,
        })
    }

    /// An intra random access point: a picture that decodes on its own (IDR, CRA or BLA).
    pub fn is_irap(&self) -> bool {
        (16..=23).contains(&self.kind)
    }

    pub fn is_slice(&self) -> bool {
        self.kind < 32
    }
}
