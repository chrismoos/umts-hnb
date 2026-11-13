// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroUsize;
use umts_types::bits::{BITS_PER_BYTE, BitError, Bits};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Segmentation {
    Disabled,
    Enabled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorDelivery {
    Discard,
    Indicate,
    NoDetect,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Correct,
    Erroneous,
    NotReported,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TmError {
    Capacity,
    Length,
    Bits(BitError),
}

#[derive(Clone, Copy, Debug)]
pub struct TmdPdu<'a> {
    pub data: Bits<'a>,
    pub erroneous: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct TmSdu<'a> {
    pub data: Bits<'a>,
    pub quality: Quality,
}

#[derive(Clone, Copy, Debug)]
pub struct TmLimits {
    pub maximum_pdus: NonZeroUsize,
    pub maximum_pdu_bits: NonZeroUsize,
    pub maximum_sdu_bits: NonZeroUsize,
}

pub struct TmReceiver {
    segmentation: Segmentation,
    errors: ErrorDelivery,
    limits: TmLimits,
    joined: Vec<u8>,
}

impl TmReceiver {
    pub fn new(segmentation: Segmentation, errors: ErrorDelivery, limits: TmLimits) -> Self {
        Self {
            segmentation,
            errors,
            limits,
            joined: vec![
                0;
                if segmentation == Segmentation::Enabled {
                    limits.maximum_sdu_bits.get().div_ceil(BITS_PER_BYTE)
                } else {
                    0
                }
            ],
        }
    }

    // TS 25.322 4.2.1.1/11.1.3 reassembles only PDUs from this entity in the same complete TTI.
    pub fn receive<'a>(&'a mut self, pdus: &'a [TmdPdu<'a>]) -> Result<TmBatch<'a>, TmError> {
        if pdus.len() > self.limits.maximum_pdus.get() {
            return Err(TmError::Capacity);
        }
        let width = pdus.first().map_or(0, |pdu| pdu.data.len());
        if pdus
            .iter()
            .any(|pdu| pdu.data.is_empty() || pdu.data.len() != width)
        {
            return Err(TmError::Length);
        }
        if width > self.limits.maximum_pdu_bits.get() {
            return Err(TmError::Capacity);
        }
        let joined = if self.segmentation == Segmentation::Enabled && !pdus.is_empty() {
            let bits = width.checked_mul(pdus.len()).ok_or(TmError::Capacity)?;
            if bits > self.limits.maximum_sdu_bits.get() {
                return Err(TmError::Capacity);
            }
            self.joined.fill(0);
            for (index, pdu) in pdus.iter().enumerate() {
                pdu.data
                    .copy_into(&mut self.joined, index * width)
                    .map_err(TmError::Bits)?;
            }
            Some(TmdPdu {
                data: Bits::new(&self.joined, bits).map_err(TmError::Bits)?,
                erroneous: pdus.iter().any(|pdu| pdu.erroneous),
            })
        } else {
            if width > self.limits.maximum_sdu_bits.get() {
                return Err(TmError::Capacity);
            }
            None
        };
        Ok(TmBatch {
            pdus,
            joined,
            errors: self.errors,
            segmentation: self.segmentation,
            position: 0,
        })
    }
}

pub struct TmBatch<'a> {
    pdus: &'a [TmdPdu<'a>],
    joined: Option<TmdPdu<'a>>,
    errors: ErrorDelivery,
    segmentation: Segmentation,
    position: usize,
}

impl<'a> Iterator for TmBatch<'a> {
    type Item = TmSdu<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let pdu = if self.segmentation == Segmentation::Enabled {
                self.joined.take()?
            } else {
                let pdu = *self.pdus.get(self.position)?;
                self.position += 1;
                pdu
            };
            if self.errors == ErrorDelivery::Discard && pdu.erroneous {
                continue;
            }
            let quality = match self.errors {
                ErrorDelivery::NoDetect => Quality::NotReported,
                _ if pdu.erroneous => Quality::Erroneous,
                _ => Quality::Correct,
            };
            return Some(TmSdu {
                data: pdu.data,
                quality,
            });
        }
    }
}

pub struct TmSegments<'a> {
    sdu: Bits<'a>,
    width: usize,
    position: usize,
}

// TS 25.322 4.2.1.1.1 requires exact multiples, equal PDU lengths and no TM header or padding.
// All returned segments must be scheduled together in one TTI, with no other SDU from this entity.
pub fn segment(
    sdu: Bits<'_>,
    segmentation: Segmentation,
    pdu_bits: NonZeroUsize,
    maximum_pdus: NonZeroUsize,
) -> Result<TmSegments<'_>, TmError> {
    let width = pdu_bits.get();
    if sdu.is_empty()
        || !sdu.len().is_multiple_of(width)
        || (segmentation == Segmentation::Disabled && sdu.len() != width)
    {
        return Err(TmError::Length);
    }
    if sdu.len() / width > maximum_pdus.get() {
        return Err(TmError::Capacity);
    }
    Ok(TmSegments {
        sdu,
        width,
        position: 0,
    })
}

impl<'a> Iterator for TmSegments<'a> {
    type Item = Bits<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.position == self.sdu.len() {
            return None;
        }
        let end = self.position + self.width;
        let bits = self
            .sdu
            .subspan(self.position..end)
            .expect("validated segmentation");
        self.position = end;
        Some(bits)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let count = (self.sdu.len() - self.position) / self.width;
        (count, Some(count))
    }
}
impl ExactSizeIterator for TmSegments<'_> {}

#[cfg(test)]
mod tests;
