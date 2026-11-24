// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_tx::{DedicatedTransmitPort, FrameInput, OwnedTransportPayload, evidence::Emission},
    rel99_plan::CompiledRel99,
    signalling::{CommonSignalling, SignallingError},
    tx_evidence::TransportSubmission,
};
use umts_dsp::{bits::Bit, tfci::Tfci};
use umts_security::ciphering::BearerCiphers;

mod pool;
mod traffic;
pub(crate) use pool::storage_requirements;
pub use pool::{FramePool, FramePoolError, FramePoolLimits, RejectedFramePool};
pub use traffic::ControlAmServiceFloor;
#[cfg(test)]
pub use traffic::EmptyTransparentTraffic;
pub use traffic::{TrafficFrame, TransparentOffer, TransparentTraffic};

#[derive(Clone, Copy, Debug)]
pub struct DownlinkClock {
    pub anchor: FrameAnchor,
    pub first_frame: u64,
    pub next_frame: ChipInstant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameBuildError {
    Bearer(BearerError),
    Acknowledged {
        bearer: BearerId,
        error: umts_rlc::acknowledged::AmError,
        diagnostics: Box<umts_rlc::acknowledged::AmDiagnostics>,
    },
    Signalling(SignallingError),
    Transmit(crate::dedicated_tx::TransmitError),
    Pool(FramePoolError),
}
impl From<BearerError> for FrameBuildError {
    fn from(error: BearerError) -> Self {
        Self::Bearer(error)
    }
}
#[derive(Clone, Copy, Default)]
struct TransportSlot {
    format: usize,
    block_bits: usize,
    blocks: usize,
    filled: usize,
}
pub struct DownlinkFrame<'a> {
    bank: &'a mut DedicatedBearers,
    mapping: usize,
    input: Option<FrameInput>,
    slots: [TransportSlot; MAX_TRANSPORTS],
    slot_count: usize,
    formats: [usize; MAX_TRANSPORTS],
    format_count: usize,
    submission: TransportSubmission,
    admitted: bool,
    pool: Option<&'a mut FramePool>,
}
type Specification = (
    TransportId,
    usize,
    umts_dsp::coding::CodedBlockFormat,
    usize,
);
struct FrameDefinition {
    specifications: [Option<Specification>; MAX_TRANSPORTS],
    count: usize,
    formats: [usize; MAX_TRANSPORTS],
    format_count: usize,
    end: ChipInstant,
}

impl DedicatedBearers {
    pub(crate) fn reset_evidence(
        &self,
    ) -> impl Iterator<Item = (BearerId, umts_rlc::acknowledged::ResetStart)> + '_ {
        self.endpoints
            .iter()
            .filter_map(|endpoint| match &endpoint.entity {
                Entity::Acknowledged(entity) => entity
                    .diagnostics()
                    .history
                    .reset_started
                    .map(|reset| (endpoint.id, reset)),
                _ => None,
            })
    }

    fn frame_error(&self, bearer: BearerId, error: BearerError) -> FrameBuildError {
        match error {
            BearerError::Am(error) => {
                if let Some(index) = self.lookup[bearer.index()]
                    && let Entity::Acknowledged(entity) = &self.endpoints[index].entity
                {
                    return FrameBuildError::Acknowledged {
                        bearer,
                        error,
                        diagnostics: Box::new(entity.diagnostics()),
                    };
                }
                FrameBuildError::Bearer(BearerError::Am(error))
            }
            error => FrameBuildError::Bearer(error),
        }
    }
    pub fn downlink_clock(&self, identity: ReceiveIdentity) -> Result<DownlinkClock, BearerError> {
        let index = self.mapping_index(identity)?;
        let map = self.maps[index].as_ref().ok_or(BearerError::Mapping)?;
        let downlink = map.downlink.as_ref().ok_or(BearerError::Mapping)?;
        Ok(DownlinkClock {
            anchor: downlink.anchor,
            first_frame: downlink.first_frame,
            next_frame: downlink.next_frame,
        })
    }

    fn frame_definition(
        &self,
        identity: ReceiveIdentity,
        plan: &Arc<CompiledRel99>,
        port: &DedicatedTransmitPort,
        start: ChipInstant,
        tfci: Tfci,
    ) -> Result<FrameDefinition, FrameBuildError> {
        if self.phase != BearerPhase::Active {
            return Err(BearerError::Phase.into());
        }
        let mapping = self.maps[self.mapping_index(identity)?]
            .as_ref()
            .expect("registered mapping");
        let downlink = mapping.downlink.as_ref().ok_or(BearerError::Mapping)?;
        if self.downlink_recovery_required || downlink.recovery_required {
            return Err(BearerError::RecoveryRequired.into());
        }
        if !port.available() {
            return Err(FrameBuildError::Transmit(
                crate::dedicated_tx::TransmitError::Closed,
            ));
        }
        if !Arc::ptr_eq(&downlink.formats, plan.downlink_configured())
            || !Arc::ptr_eq(&mapping.uplink, plan.uplink_configured())
        {
            return Err(BearerError::Mapping.into());
        }
        if start != downlink.next_frame || start.index.checked_add(CHIPS_PER_FRAME).is_none() {
            return Err(BearerError::Time.into());
        }
        let delta = start
            .index
            .checked_sub(downlink.anchor.time.index)
            .ok_or(BearerError::Time)?;
        let frame = downlink
            .first_frame
            .checked_add(delta / CHIPS_PER_FRAME)
            .ok_or(BearerError::Time)?;
        let row = downlink
            .formats
            .combination(tfci)
            .ok_or(BearerError::Mapping)?;
        if row.formats.len() > MAX_TRANSPORTS {
            return Err(BearerError::Capacity.into());
        }
        let mut specifications = [None; MAX_TRANSPORTS];
        let mut count = 0;
        let mut total = 0_usize;
        let mut longest = 1;
        for ((id, &format), previous) in downlink
            .formats
            .transport_ids()
            .zip(&row.formats)
            .zip(&downlink.active_formats)
        {
            let definition = downlink
                .formats
                .definition(id)
                .expect("configured transport");
            let frames = definition.interval.frames();
            if frame.is_multiple_of(frames as u64) {
                let block = definition.formats[format];
                if block.blocks != 0 && !downlink.dch_transport_available(id, start) {
                    return Err(BearerError::Mapping.into());
                }
                let bits = block
                    .block_bits
                    .checked_mul(block.blocks)
                    .ok_or(BearerError::Capacity)?;
                total = total
                    .checked_add(bits)
                    .filter(|bits| *bits <= port.maximum_frame_payload_bits())
                    .ok_or(BearerError::Capacity)?;
                if downlink
                    .formats
                    .block_plan(id, format)
                    .ok_or(BearerError::Mapping)?
                    .coded_bits()
                    != 0
                {
                    longest = longest.max(frames);
                }
                specifications[count] = Some((id, format, block, bits));
                count += 1;
            } else if *previous != Some(format) {
                return Err(BearerError::Transport.into());
            }
        }
        let end = ChipInstant {
            index: start
                .index
                .checked_add(longest as u64 * CHIPS_PER_FRAME)
                .ok_or(BearerError::Time)?,
            ..start
        };
        if matches!(mapping.phase, MappingPhase::Draining { downlink_end, .. } if end.index > downlink_end.index)
        {
            return Err(BearerError::Time.into());
        }
        let mut formats = [0; MAX_TRANSPORTS];
        formats[..row.formats.len()].copy_from_slice(&row.formats);
        Ok(FrameDefinition {
            specifications,
            count,
            formats,
            format_count: row.formats.len(),
            end,
        })
    }
    #[cfg(test)]
    pub fn begin_frame(
        &mut self,
        plan: Arc<CompiledRel99>,
        port: &DedicatedTransmitPort,
        start: ChipInstant,
        tfci: Tfci,
    ) -> Result<DownlinkFrame<'_>, FrameBuildError> {
        let identity = self.identity();
        let mapping = self.current;
        let definition = self.frame_definition(identity, &plan, port, start, tfci)?;
        let (emission, submission) = Emission::new(
            identity,
            start,
            definition.end,
            tfci,
            plan,
            port.frame_source(),
        )
        .map_err(FrameBuildError::Transmit)?;
        let mut transports = Vec::with_capacity(definition.count);
        let mut slots = [TransportSlot::default(); MAX_TRANSPORTS];
        for (index, (transport, format, block, bits)) in definition.specifications
            [..definition.count]
            .iter()
            .flatten()
            .copied()
            .enumerate()
        {
            transports.push(OwnedTransportPayload {
                transport,
                bits: vec![Bit::Zero; bits],
            });
            slots[index] = TransportSlot {
                format,
                block_bits: block.block_bits,
                blocks: block.blocks,
                filled: if bits == 0 { block.blocks } else { 0 },
            };
        }
        let input = FrameInput {
            identity,
            start,
            tfci,
            transports,
            emission: Some(Box::new(emission)),
            storage: Box::default(),
            retry_after: None,
        };
        Ok(DownlinkFrame {
            bank: self,
            mapping,
            input: Some(input),
            slots,
            slot_count: definition.count,
            formats: definition.formats,
            format_count: definition.format_count,
            submission,
            admitted: false,
            pool: None,
        })
    }
    pub fn begin_pooled_frame<'a>(
        &'a mut self,
        pool: &'a mut FramePool,
        port: &DedicatedTransmitPort,
        start: ChipInstant,
        tfci: Tfci,
    ) -> Result<DownlinkFrame<'a>, FrameBuildError> {
        let mapping = self.mapping_index(pool.identity())?;
        let definition = self.frame_definition(pool.identity(), pool.plan(), port, start, tfci)?;
        let (input, submission) = pool
            .acquire(start, tfci, &definition, port)
            .map_err(FrameBuildError::Pool)?;
        let mut slots = [TransportSlot::default(); MAX_TRANSPORTS];
        for (index, (_, format, block, bits)) in definition.specifications[..definition.count]
            .iter()
            .flatten()
            .copied()
            .enumerate()
        {
            slots[index] = TransportSlot {
                format,
                block_bits: block.block_bits,
                blocks: block.blocks,
                filled: if bits == 0 { block.blocks } else { 0 },
            };
        }
        Ok(DownlinkFrame {
            bank: self,
            mapping,
            input: Some(input),
            slots,
            slot_count: definition.count,
            formats: definition.formats,
            format_count: definition.format_count,
            submission,
            admitted: false,
            pool: Some(pool),
        })
    }
}

impl DownlinkFrame<'_> {
    #[cfg(test)]
    pub(crate) fn prepared_input(&self) -> &FrameInput {
        self.input.as_ref().expect("reserved frame")
    }

    pub fn identity(&self) -> ReceiveIdentity {
        self.input.as_ref().expect("reserved frame").identity
    }
    fn target(&self, grant: DownlinkGrant) -> Result<usize, BearerError> {
        if !self.submission.viable() {
            return Err(BearerError::RecoveryRequired);
        }
        let input = self.input.as_ref().expect("reserved frame");
        if grant.identity != input.identity {
            return Err(BearerError::Owner);
        }
        if grant.start != input.start {
            return Err(BearerError::Time);
        }
        let index = input
            .transports
            .iter()
            .position(|transport| transport.transport == grant.transport)
            .ok_or(BearerError::Mapping)?;
        let slot = &self.slots[index];
        if slot.format != grant.format || slot.filled >= slot.blocks || slot.block_bits == 0 {
            return Err(BearerError::Transport);
        }
        Ok(index)
    }
    pub fn complete(&self) -> bool {
        self.slots[..self.slot_count]
            .iter()
            .all(|slot| slot.filled == slot.blocks)
    }
    pub fn submission(&self) -> TransportSubmission {
        self.submission.clone()
    }

    pub fn publish_sequenced(
        &mut self,
        grant: DownlinkGrant,
        now: ChipInstant,
        ciphers: &BearerCiphers,
    ) -> Result<bool, FrameBuildError> {
        let index = self.target(grant)?;
        let input = self.input.as_mut().expect("reserved frame");
        let slot = &mut self.slots[index];
        let published = self
            .bank
            .publish_sequenced(grant, now, ciphers, |pdu| {
                store(
                    &mut input.transports[index],
                    slot,
                    pdu.data,
                    pdu.blocks,
                    pdu.block_bits,
                )
            })
            .map(|value| value.is_some())
            .map_err(|error| match error {
                TransmitError::Bearer(error) | TransmitError::Admission(error) => {
                    self.bank.frame_error(grant.bearer, error)
                }
            })?;
        self.admitted |= published;
        Ok(published)
    }
    pub fn publish_transparent(
        &mut self,
        grant: DownlinkGrant,
        sdus: &[Bits<'_>],
        ciphers: &BearerCiphers,
    ) -> Result<(), FrameBuildError> {
        let index = self.target(grant)?;
        let input = self.input.as_mut().expect("reserved frame");
        let slot = &mut self.slots[index];
        self.bank
            .publish_transparent(grant, sdus, ciphers, |pdu| {
                store(
                    &mut input.transports[index],
                    slot,
                    pdu.data,
                    pdu.blocks,
                    pdu.block_bits,
                )
            })
            .map_err(|error| match error {
                TransmitError::Bearer(error) | TransmitError::Admission(error) => {
                    FrameBuildError::Bearer(error)
                }
            })?;
        self.admitted = true;
        Ok(())
    }
    pub fn publish_signalling(
        &mut self,
        grant: DownlinkGrant,
        now: ChipInstant,
        signalling: &mut CommonSignalling,
    ) -> Result<bool, FrameBuildError> {
        let index = self.target(grant)?;
        let transmit::ResolvedGrant {
            endpoint,
            format,
            mapping,
            logical,
            ..
        } = self.bank.grant(grant)?;
        if !matches!(self.bank.endpoints[endpoint].entity, Entity::Signalling(_)) {
            return Err(BearerError::Mapping.into());
        }
        let bearer = [
            umts_rrc::nas::SignallingBearer::Unacknowledged,
            umts_rrc::nas::SignallingBearer::ControlAm,
            umts_rrc::nas::SignallingBearer::NasAm,
        ]
        .into_iter()
        .find(|bearer| bearer.identity() == grant.bearer.get())
        .ok_or(BearerError::Mapping)?;
        let payload_bits = format.block_bits - if logical.is_some() { C_T_BITS } else { 0 };
        let bytes =
            NonZeroUsize::new(payload_bits / BITS_PER_BYTE).ok_or(BearerError::Transport)?;
        let input = self.input.as_mut().expect("reserved frame");
        let slot = &mut self.slots[index];
        let scratch = &mut self.bank.endpoints[endpoint].mac;
        let submission = &self.submission;
        let published = signalling
            .publish_srb_payload(self.bank.owner, bearer, now, bytes, |payload| {
                let bits = umts_mac::encode_dedicated(
                    mapping,
                    logical,
                    Bits::new(payload, payload.len() * BITS_PER_BYTE)
                        .map_err(|_| SignallingError::Message)?,
                    scratch,
                )
                .map_err(|_| SignallingError::Mapping)?;
                store(
                    &mut input.transports[index],
                    slot,
                    Bits::new(scratch, bits).expect("encoded signaling MAC"),
                    1,
                    bits,
                )
                .map_err(|_| SignallingError::ConfigurationFailure)?;
                Ok(submission.clone())
            })
            .map_err(FrameBuildError::Signalling)?;
        self.admitted |= published;
        Ok(published)
    }
    pub fn finish(mut self) -> Result<FrameInput, Box<Self>> {
        if !self.complete() {
            return Err(Box::new(self));
        }
        let input = self.input.take().expect("reserved frame");
        let next = input
            .start
            .index
            .checked_add(CHIPS_PER_FRAME)
            .expect("reserved frame end");
        let downlink = self.bank.maps[self.mapping]
            .as_mut()
            .expect("reserved mapping")
            .downlink
            .as_mut()
            .expect("reserved downlink");
        downlink.next_frame = ChipInstant {
            index: next,
            ..input.start
        };
        for (previous, format) in downlink
            .active_formats
            .iter_mut()
            .zip(&self.formats[..self.format_count])
        {
            *previous = Some(*format);
        }
        Ok(input)
    }
}

impl Drop for DownlinkFrame<'_> {
    fn drop(&mut self) {
        if self.input.is_some() && self.admitted {
            self.bank.downlink_recovery_required = true;
            self.bank.maps[self.mapping]
                .as_mut()
                .expect("reserved mapping")
                .downlink
                .as_mut()
                .expect("reserved downlink")
                .recovery_required = true;
        }
        if let (Some(pool), Some(input)) = (&mut self.pool, self.input.take()) {
            let result = pool.recycle(input);
            assert!(result.is_ok(), "owned draft must retain its pool credit");
        }
    }
}

fn store(
    target: &mut OwnedTransportPayload,
    slot: &mut TransportSlot,
    bits: Bits<'_>,
    blocks: usize,
    block_bits: usize,
) -> Result<(), BearerError> {
    if block_bits != slot.block_bits
        || blocks == 0
        || blocks > slot.blocks - slot.filled
        || bits.len() != blocks * block_bits
    {
        return Err(BearerError::Transport);
    }
    let offset = slot.filled * block_bits;
    for (index, output) in target.bits[offset..offset + bits.len()]
        .iter_mut()
        .enumerate()
    {
        *output = Bit::from(bits.bit(index).expect("validated frame bit"));
    }
    slot.filled += blocks;
    Ok(())
}
