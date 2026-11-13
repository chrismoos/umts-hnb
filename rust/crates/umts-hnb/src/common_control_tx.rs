// SPDX-License-Identifier: AGPL-3.0-or-later

#[cfg(test)]
use crate::common_tx::CommonTx;
use crate::{
    common_transport::{FachError, FachTransport},
    common_tx::{CommonTxError, CommonTxTarget},
};
use std::num::NonZeroUsize;
use umts_mac::{CommonHeader, MacError};
use umts_rlc::unacknowledged::{
    EInterpretation, LiWidth, UmConfig, UmError, UmLimits, UmPublishError, UmTransmitter,
};
use umts_types::{
    bits::{BITS_PER_BYTE, Bits},
    time::ChipInstant,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommonControlTxError {
    Profile,
    Capacity,
    Payload,
    Rlc(UmError),
    Mac(MacError),
    Transmit(CommonTxError),
}
impl From<FachError> for CommonControlTxError {
    fn from(error: FachError) -> Self {
        match error {
            FachError::Profile => Self::Profile,
            FachError::Payload => Self::Payload,
            FachError::Mac(error) => Self::Mac(error),
            FachError::Transmit(error) => Self::Transmit(error),
        }
    }
}

pub struct CommonControlDownlink {
    fach: FachTransport,
    rlc: UmTransmitter,
    rrc: Vec<u8>,
}
impl CommonControlDownlink {
    pub fn new(
        source: &dyn CommonTxTarget,
        channel: usize,
        limits: UmLimits,
    ) -> Result<Self, CommonControlTxError> {
        let fach = FachTransport::new(source, channel, CommonHeader::CommonControl)?;
        if limits.maximum_pdu_bytes != fach.pdu_bytes() {
            return Err(CommonControlTxError::Profile);
        }
        Ok(Self {
            fach,
            // TS 25.331 13.6 fixes RB0 downlink UM to 7-bit LIs with normal E-bit interpretation.
            rlc: UmTransmitter::new(
                UmConfig {
                    li_width: LiWidth::Seven,
                    interpretation: EInterpretation::Normal,
                },
                limits,
            )
            .map_err(CommonControlTxError::Rlc)?,
            rrc: vec![0; limits.maximum_sdu_bytes.get()],
        })
    }
    #[cfg(test)]
    pub fn pending_sdus(&self) -> usize {
        self.rlc.pending_sdus()
    }
    pub fn has_pending(&self) -> bool {
        self.rlc.has_pending()
    }
    #[cfg(test)]
    pub fn sequence(&self) -> u8 {
        self.rlc.sequence()
    }
    pub(crate) fn validate_source(
        &self,
        source: &dyn CommonTxTarget,
        now: ChipInstant,
    ) -> Result<(), CommonControlTxError> {
        self.fach.validate(source, now).map_err(Into::into)
    }
    // TS 25.331 12.1.3 pads the RRC production to an octet boundary before UM. TM padding rules do not apply.
    pub fn enqueue(&mut self, encoded: Bits<'_>) -> Result<(), CommonControlTxError> {
        if encoded.is_empty() {
            return Err(CommonControlTxError::Payload);
        }
        let bytes = encoded.len().div_ceil(BITS_PER_BYTE);
        let output = self
            .rrc
            .get_mut(..bytes)
            .ok_or(CommonControlTxError::Capacity)?;
        encoded
            .copy_to(output)
            .map_err(|_| CommonControlTxError::Payload)?;
        self.rlc.enqueue(output).map_err(CommonControlTxError::Rlc)
    }
    pub fn publish_frame(
        &mut self,
        source: &mut dyn CommonTxTarget,
        start: ChipInstant,
    ) -> Result<bool, CommonControlTxError> {
        self.fach.validate(source, start)?;
        let fach = &mut self.fach;
        match self.rlc.transmit_with(fach.pdu_bytes().get(), |pdu| {
            fach.publish(source, start, pdu)
                .map_err(CommonControlTxError::from)
        }) {
            Ok(value) => Ok(value.is_some()),
            Err(UmPublishError::Rlc(error)) => Err(CommonControlTxError::Rlc(error)),
            Err(UmPublishError::Downstream(error)) => Err(error),
        }
    }
    pub fn maximum_pdu_bytes(
        source: &dyn CommonTxTarget,
        channel: usize,
    ) -> Result<NonZeroUsize, CommonControlTxError> {
        FachTransport::pdu_size(source, channel, CommonHeader::CommonControl).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests;
