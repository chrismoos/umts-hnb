// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    common_transport::{FachError, FachTransport, rach_pdu_size},
    common_tx::CommonTxTarget,
};
use std::num::NonZeroUsize;
use umts_mac::{CommonHeader, CommonIdentity, CommonPdu, LogicalChannelId};
use umts_rlc::unacknowledged::{
    ReceivedUm, UmConfig, UmError, UmLimits, UmPreparedReceiveError, UmPublication, UmPublishError,
    UmReceiver, UmTransmitter,
};
use umts_types::{
    UeKey,
    bits::{BITS_PER_BYTE, Bits},
    time::ChipInstant,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommonUmError {
    Origin,
    Mapping,
    Payload,
    Profile,
    MalformedPdu(UmError),
    Rlc(UmError),
    Fach(FachError),
}
#[derive(Debug, PartialEq, Eq)]
pub enum CommonUmPreparedError<E> {
    Bearer(CommonUmError),
    Preparation(E),
}
#[derive(Debug, PartialEq, Eq)]
pub enum CommonUmPreparedReceiveError<P, A> {
    Bearer(CommonUmError),
    Preparation(P),
    Admission(A),
}
#[derive(Debug, PartialEq, Eq)]
pub enum UmPayloadPublishError<P, A> {
    Bearer(CommonUmError),
    Preparation(P),
    Admission(A),
}
pub struct CommonUmBearer {
    key: UeKey,
    header: CommonHeader,
    fach: FachTransport,
    utran: Option<FachTransport>,
    address_utran: bool,
    tx: UmTransmitter,
    rx: UmReceiver,
    input: Vec<u8>,
    prepared_rx: Vec<u8>,
    prepared_tx: Vec<u8>,
    rrc: Vec<u8>,
}
impl CommonUmBearer {
    pub fn publication_ready(&self) -> bool {
        self.tx.has_pending()
    }
    pub(crate) fn payload_limits(&self) -> (usize, usize, usize, usize) {
        (
            self.input.len(),
            self.tx.minimum_pdu_bytes(),
            self.tx.limits().maximum_pdu_bytes.get(),
            self.tx.limits().maximum_sdu_bytes.get(),
        )
    }
    pub fn preparation_bytes(
        tx_limits: UmLimits,
        rx_limits: UmLimits,
    ) -> Result<usize, CommonUmError> {
        tx_limits
            .maximum_pdu_bytes
            .get()
            .checked_add(rx_limits.maximum_pdu_bytes.get())
            .ok_or(CommonUmError::Profile)
    }
    pub fn new(
        source: &dyn CommonTxTarget,
        channel: usize,
        key: UeKey,
        identity: u16,
        config: UmConfig,
        tx_limits: UmLimits,
        rx_limits: UmLimits,
    ) -> Result<Self, CommonUmError> {
        let logical =
            LogicalChannelId::new(umts_rrc::nas::SignallingBearer::Unacknowledged.identity())
                .map_err(|_| CommonUmError::Profile)?;
        let header = CommonHeader::Dedicated {
            identity: CommonIdentity::Cell(identity),
            channel: logical,
        };
        let mut fach = FachTransport::new(source, channel, header).map_err(CommonUmError::Fach)?;
        if fach.epoch() != key.epoch {
            return Err(CommonUmError::Origin);
        }
        fach.bind_ue(key).map_err(CommonUmError::Fach)?;
        if fach.pdu_bytes() != tx_limits.maximum_pdu_bytes
            || rach_pdu_size(source, header).map_err(CommonUmError::Fach)?
                != rx_limits.maximum_pdu_bytes
        {
            return Err(CommonUmError::Profile);
        }
        Ok(Self {
            key,
            header,
            fach,
            utran: None,
            address_utran: false,
            tx: UmTransmitter::new(config, tx_limits).map_err(CommonUmError::Rlc)?,
            rx: UmReceiver::new(config, rx_limits).map_err(CommonUmError::Rlc)?,
            input: vec![0; rx_limits.maximum_pdu_bytes.get()],
            prepared_rx: vec![0; rx_limits.maximum_pdu_bytes.get()],
            prepared_tx: vec![0; tx_limits.maximum_pdu_bytes.get()],
            rrc: vec![0; tx_limits.maximum_sdu_bytes.get()],
        })
    }
    /// Prepares downlink addressing by U-RNTI. TS 25.321 9.2.1.1 allows it for DCCH on UM
    /// only, and a UE that left CELL_PCH or URA_PCH has no C-RNTI until it is given one.
    pub fn with_utran_identity(
        mut self,
        source: &dyn CommonTxTarget,
        channel: usize,
        urnti: u32,
    ) -> Result<Self, CommonUmError> {
        let CommonHeader::Dedicated {
            channel: logical, ..
        } = self.header
        else {
            return Err(CommonUmError::Profile);
        };
        let mut utran = FachTransport::new(
            source,
            channel,
            CommonHeader::Dedicated {
                identity: CommonIdentity::Utran(urnti),
                channel: logical,
            },
        )
        .map_err(CommonUmError::Fach)?;
        utran.bind_ue(self.key).map_err(CommonUmError::Fach)?;
        self.utran = Some(utran);
        Ok(self)
    }
    pub fn address_by_urnti(&mut self, enabled: bool) -> Result<(), CommonUmError> {
        if enabled && self.utran.is_none() {
            return Err(CommonUmError::Profile);
        }
        self.address_utran = enabled;
        Ok(())
    }
    pub fn enqueue(&mut self, key: UeKey, encoded: Bits<'_>) -> Result<(), CommonUmError> {
        if key != self.key {
            return Err(CommonUmError::Origin);
        }
        if encoded.is_empty() {
            return Err(CommonUmError::Payload);
        }
        let output = self
            .rrc
            .get_mut(..encoded.len().div_ceil(BITS_PER_BYTE))
            .ok_or(CommonUmError::Payload)?;
        encoded
            .copy_to(output)
            .map_err(|_| CommonUmError::Payload)?;
        self.tx.enqueue(output).map_err(CommonUmError::Rlc)
    }
    // The admission callback reserves every output SDU before any receiver mutation.
    #[cfg(test)]
    fn receive_input(&mut self, key: UeKey, pdu: CommonPdu<'_>) -> Result<(), CommonUmError> {
        self.validate_common(key, pdu)?;
        self.receive_payload(key, pdu.payload).map(|_| ())
    }
    pub(crate) fn validate_common(
        &self,
        key: UeKey,
        pdu: CommonPdu<'_>,
    ) -> Result<(), CommonUmError> {
        if key != self.key {
            return Err(CommonUmError::Origin);
        }
        if pdu.header != self.header {
            return Err(CommonUmError::Mapping);
        }
        if pdu.payload.len() != self.input.len() * BITS_PER_BYTE {
            return Err(CommonUmError::Payload);
        }
        Ok(())
    }
    fn receive_payload(&mut self, key: UeKey, payload: Bits<'_>) -> Result<usize, CommonUmError> {
        if key != self.key {
            return Err(CommonUmError::Origin);
        }
        if payload.is_empty() || !payload.len().is_multiple_of(BITS_PER_BYTE) {
            return Err(CommonUmError::Payload);
        }
        let length = payload.len() / BITS_PER_BYTE;
        let output = self.input.get_mut(..length).ok_or(CommonUmError::Payload)?;
        payload
            .copy_to(output)
            .map_err(|_| CommonUmError::Payload)?;
        Ok(length)
    }
    #[cfg(test)]
    pub fn receive_prepared_admitted<P, A>(
        &mut self,
        key: UeKey,
        pdu: CommonPdu<'_>,
        prepare: impl FnOnce(u64, &mut [u8]) -> Result<(), P>,
        admit: impl FnOnce(usize, usize) -> Result<(), A>,
    ) -> Result<ReceivedUm<'_>, CommonUmPreparedReceiveError<P, A>> {
        let fail = CommonUmPreparedReceiveError::Bearer;
        self.receive_input(key, pdu).map_err(fail)?;
        self.receive_staged(self.input.len(), prepare, admit)
    }
    /// MAC identity and binding must already be checked by the single UE control owner.
    pub fn receive_payload_prepared_admitted<P, A>(
        &mut self,
        key: UeKey,
        payload: Bits<'_>,
        prepare: impl FnOnce(u64, &mut [u8]) -> Result<(), P>,
        admit: impl FnOnce(usize, usize) -> Result<(), A>,
    ) -> Result<ReceivedUm<'_>, CommonUmPreparedReceiveError<P, A>> {
        let length = self
            .receive_payload(key, payload)
            .map_err(CommonUmPreparedReceiveError::Bearer)?;
        self.receive_staged(length, prepare, admit)
    }
    fn receive_staged<P, A>(
        &mut self,
        length: usize,
        prepare: impl FnOnce(u64, &mut [u8]) -> Result<(), P>,
        admit: impl FnOnce(usize, usize) -> Result<(), A>,
    ) -> Result<ReceivedUm<'_>, CommonUmPreparedReceiveError<P, A>> {
        let fail = CommonUmPreparedReceiveError::Bearer;
        let raw = &self.input[..length];
        let plain = &mut self.prepared_rx[..length];
        self.rx
            .receive_prepared_admitted(
                raw,
                |position| {
                    plain.copy_from_slice(raw);
                    prepare(position, plain)?;
                    Ok(&*plain)
                },
                admit,
            )
            .map_err(|error| match error {
                UmPreparedReceiveError::MalformedPdu(error) => {
                    fail(CommonUmError::MalformedPdu(error))
                }
                UmPreparedReceiveError::Rlc(error) => fail(CommonUmError::Rlc(error)),
                UmPreparedReceiveError::Preparation(error) => {
                    CommonUmPreparedReceiveError::Preparation(error)
                }
                UmPreparedReceiveError::Admission(error) => {
                    CommonUmPreparedReceiveError::Admission(error)
                }
            })
    }
    pub fn transmit_position(&self) -> u64 {
        self.tx.next_position()
    }
    pub fn receive_position(&self) -> Result<u64, CommonUmError> {
        self.rx.next_position().map_err(CommonUmError::Rlc)
    }
    pub fn publish_frame_prepared<E>(
        &mut self,
        source: &mut dyn CommonTxTarget,
        start: ChipInstant,
        prepare: impl FnOnce(UmPublication<'_>, &mut [u8]) -> Result<(), E>,
    ) -> Result<bool, CommonUmPreparedError<E>> {
        let fail = CommonUmPreparedError::Bearer;
        let fach = match self.utran.as_mut().filter(|_| self.address_utran) {
            Some(utran) => utran,
            None => &mut self.fach,
        };
        fach.validate(source, start)
            .map_err(|error| fail(CommonUmError::Fach(error)))?;
        publish_payload_prepared(
            &mut self.tx,
            &mut self.prepared_tx,
            fach.pdu_bytes().get(),
            prepare,
            |bytes| fach.publish(source, start, bytes),
        )
        .map_err(|error| match error {
            UmPayloadPublishError::Bearer(error) => fail(error),
            UmPayloadPublishError::Preparation(error) => CommonUmPreparedError::Preparation(error),
            UmPayloadPublishError::Admission(error) => fail(CommonUmError::Fach(error)),
        })
    }
    pub fn publish_payload_prepared<P, A>(
        &mut self,
        key: UeKey,
        pdu_bytes: NonZeroUsize,
        prepare: impl FnOnce(UmPublication<'_>, &mut [u8]) -> Result<(), P>,
        admit: impl FnOnce(&[u8]) -> Result<(), A>,
    ) -> Result<bool, UmPayloadPublishError<P, A>> {
        if key != self.key {
            return Err(UmPayloadPublishError::Bearer(CommonUmError::Origin));
        }
        publish_payload_prepared(
            &mut self.tx,
            &mut self.prepared_tx,
            pdu_bytes.get(),
            prepare,
            admit,
        )
    }
}

fn publish_payload_prepared<P, A>(
    transmit: &mut UmTransmitter,
    buffer: &mut [u8],
    pdu_bytes: usize,
    prepare: impl FnOnce(UmPublication<'_>, &mut [u8]) -> Result<(), P>,
    admit: impl FnOnce(&[u8]) -> Result<(), A>,
) -> Result<bool, UmPayloadPublishError<P, A>> {
    let bytes = buffer
        .get_mut(..pdu_bytes)
        .ok_or(UmPayloadPublishError::Bearer(CommonUmError::Profile))?;
    transmit
        .transmit_positioned_with(pdu_bytes, |publication| {
            bytes.copy_from_slice(publication.bytes);
            let header = publication.bytes[0];
            prepare(publication, bytes).map_err(UmPayloadPublishError::Preparation)?;
            if bytes[0] != header {
                return Err(UmPayloadPublishError::Bearer(CommonUmError::Payload));
            }
            admit(bytes).map_err(UmPayloadPublishError::Admission)
        })
        .map(|result| result.is_some())
        .map_err(|error| match error {
            UmPublishError::Rlc(error) => UmPayloadPublishError::Bearer(CommonUmError::Rlc(error)),
            UmPublishError::Downstream(error) => error,
        })
}
