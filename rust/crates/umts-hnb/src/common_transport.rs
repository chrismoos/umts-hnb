// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::common_tx::{CommonTxError, CommonTxTarget};
use std::num::NonZeroUsize;
use umts_dsp::{
    bits::{Bit, unpack_msb},
    scrambling::PrimaryScramblingCode,
};
use umts_mac::{CommonHeader, FddCommonChannel, MacError, common_header_bits, encode_common};
use umts_rrc::common_channels::TransportRole;
use umts_types::{
    RadioEpoch,
    bits::{BITS_PER_BYTE, Bits},
    channel_codes::OvsfCode,
    time::ChipInstant,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FachError {
    Profile,
    Payload,
    Mac(MacError),
    Transmit(CommonTxError),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Binding {
    epoch: RadioEpoch,
    primary: PrimaryScramblingCode,
    code: OvsfCode,
    block_bits: usize,
}
fn binding(source: &dyn CommonTxTarget, channel: usize) -> Result<Binding, FachError> {
    let information = source.information();
    let physical = information
        .sib5()
        .secondary_channels()
        .get(channel)
        .ok_or(FachError::Profile)?;
    let [transport] = physical.transports() else {
        return Err(FachError::Profile);
    };
    if transport.role != TransportRole::ForwardAccess || transport.formats.groups.len() != 1 {
        return Err(FachError::Profile);
    }
    let block_bits = usize::from(transport.formats.groups[0].transport_block_bits);
    if !block_bits.is_multiple_of(BITS_PER_BYTE) {
        return Err(FachError::Profile);
    }
    Ok(Binding {
        epoch: source.next_chip().epoch,
        primary: information.primary_code(),
        code: physical.settings().code,
        block_bits,
    })
}

pub(crate) struct FachTransport {
    binding: Binding,
    channel: usize,
    header: CommonHeader,
    mac: Vec<u8>,
    transport: Vec<Bit>,
    pdu_bytes: NonZeroUsize,
    owner: Option<umts_types::UeKey>,
}
impl FachTransport {
    pub fn pdu_size(
        source: &dyn CommonTxTarget,
        channel: usize,
        header: CommonHeader,
    ) -> Result<NonZeroUsize, FachError> {
        let binding = binding(source, channel)?;
        let header_bits =
            common_header_bits(FddCommonChannel::Fach, header).map_err(FachError::Mac)?;
        if !header_bits.is_multiple_of(BITS_PER_BYTE) {
            return Err(FachError::Profile);
        }
        binding
            .block_bits
            .checked_sub(header_bits)
            .map(|bits| bits / BITS_PER_BYTE)
            .and_then(NonZeroUsize::new)
            .ok_or(FachError::Profile)
    }
    pub fn retained_bytes(source: &dyn CommonTxTarget, channel: usize) -> Result<usize, FachError> {
        let binding = binding(source, channel)?;
        Ok(binding.block_bits / BITS_PER_BYTE + binding.block_bits * std::mem::size_of::<Bit>())
    }
    pub fn new(
        source: &dyn CommonTxTarget,
        channel: usize,
        header: CommonHeader,
    ) -> Result<Self, FachError> {
        let binding = binding(source, channel)?;
        Ok(Self {
            binding,
            channel,
            header,
            pdu_bytes: Self::pdu_size(source, channel, header)?,
            owner: None,
            mac: vec![0; binding.block_bits / BITS_PER_BYTE],
            transport: vec![Bit::Zero; binding.block_bits],
        })
    }
    pub fn epoch(&self) -> RadioEpoch {
        self.binding.epoch
    }
    pub fn bind_ue(&mut self, key: umts_types::UeKey) -> Result<(), FachError> {
        if key.epoch != self.binding.epoch {
            return Err(FachError::Profile);
        }
        self.owner = Some(key);
        Ok(())
    }
    pub fn pdu_bytes(&self) -> NonZeroUsize {
        self.pdu_bytes
    }
    pub fn validate(
        &self,
        source: &dyn CommonTxTarget,
        start: ChipInstant,
    ) -> Result<(), FachError> {
        if !source.available() {
            return Err(FachError::Transmit(CommonTxError::Poisoned));
        }
        if binding(source, self.channel)? != self.binding || start.epoch != self.binding.epoch {
            return Err(FachError::Profile);
        }
        Ok(())
    }
    pub fn publish(
        &mut self,
        source: &mut dyn CommonTxTarget,
        start: ChipInstant,
        pdu: &[u8],
    ) -> Result<(), FachError> {
        self.publish_tracked(source, start, pdu).map(|_| ())
    }
    pub fn publish_tracked(
        &mut self,
        source: &mut dyn CommonTxTarget,
        start: ChipInstant,
        pdu: &[u8],
    ) -> Result<crate::tx_evidence::TransportSubmission, FachError> {
        self.validate(source, start)?;
        if pdu.len() != self.pdu_bytes.get() {
            return Err(FachError::Payload);
        }
        let payload = Bits::new(pdu, pdu.len() * BITS_PER_BYTE).map_err(|_| FachError::Payload)?;
        let length = encode_common(FddCommonChannel::Fach, self.header, payload, &mut self.mac)
            .map_err(FachError::Mac)?;
        if length != self.transport.len() {
            return Err(FachError::Payload);
        }
        unpack_msb(&self.mac, &mut self.transport).map_err(|_| FachError::Payload)?;
        source
            .publish_transport(start, self.channel, &self.transport, &[], self.owner)
            .map_err(FachError::Transmit)
    }
}

pub(crate) fn rach_pdu_size(
    source: &dyn CommonTxTarget,
    header: CommonHeader,
) -> Result<NonZeroUsize, FachError> {
    let [prach] = source.information().sib5().prach() else {
        return Err(FachError::Profile);
    };
    let [group] = prach.transport().groups.as_slice() else {
        return Err(FachError::Profile);
    };
    let bits = usize::from(group.transport_block_bits);
    let header_bits = common_header_bits(FddCommonChannel::Rach, header).map_err(FachError::Mac)?;
    if !bits.is_multiple_of(BITS_PER_BYTE) || !header_bits.is_multiple_of(BITS_PER_BYTE) {
        return Err(FachError::Profile);
    }
    bits.checked_sub(header_bits)
        .map(|bits| bits / BITS_PER_BYTE)
        .and_then(NonZeroUsize::new)
        .ok_or(FachError::Profile)
}
