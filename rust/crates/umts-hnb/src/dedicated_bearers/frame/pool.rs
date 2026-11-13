// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_tx::{DedicatedTransmitSession, FramePoolTag, FrameStorage, TransmitPhase};
use crate::signalling::{AdmissionError, Lease, ResourceLedger, Resources};

const MAX_PAYLOAD_SLOTS: usize = 16;
const MAX_RECEIPT_SLOTS: usize = 256;

#[derive(Clone, Copy)]
pub struct FramePoolLimits {
    pub payload_slots: NonZeroUsize,
    pub receipt_slots: NonZeroUsize,
    pub maximum_buffer_bytes: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FramePoolError {
    Capacity,
    Identity,
    Pending,
    Admission(AdmissionError),
    Transmit(crate::dedicated_tx::TransmitError),
}
pub struct RejectedFramePool {
    pub error: FramePoolError,
    pub pool: Box<FramePool>,
}
impl std::fmt::Debug for RejectedFramePool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RejectedFramePool")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
#[must_use]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct FramePool {
    owner: Arc<()>,
    identity: ReceiveIdentity,
    plan: Arc<CompiledRel99>,
    maximums: Vec<(TransportId, usize)>,
    frames: Vec<Option<FrameInput>>,
    receipts: Vec<Option<Box<Emission>>>,
    lease: Lease,
    bytes: usize,
}

pub(crate) fn storage_requirements(
    plan: &CompiledRel99,
    limits: FramePoolLimits,
) -> Result<(Vec<(TransportId, usize)>, usize), FramePoolError> {
    if limits.payload_slots.get() > MAX_PAYLOAD_SLOTS
        || limits.receipt_slots.get() > MAX_RECEIPT_SLOTS
    {
        return Err(FramePoolError::Capacity);
    }
    let retained_reports = plan
        .configuration()
        .bearers()
        .iter()
        .filter(|bearer| {
            matches!(
                bearer.rlc,
                umts_rrc::dedicated::Rel99Rlc::Acknowledged { .. }
            )
        })
        .count();
    if limits.receipt_slots.get() < crate::dedicated_tx::MAX_TRACKED_FRAMES + retained_reports + 1 {
        return Err(FramePoolError::Capacity);
    }
    let configured = plan.downlink_configured();
    if configured.transport_count() > MAX_TRANSPORTS {
        return Err(FramePoolError::Capacity);
    }
    let mut maximums = Vec::with_capacity(configured.transport_count());
    let mut payload = 0_usize;
    for (index, id) in configured.transport_ids().enumerate() {
        let definition = configured.definition(id).expect("configured transport");
        let maximum = configured
            .combinations()
            .iter()
            .try_fold(0, |maximum, row| {
                let format = definition.formats[row.formats[index]];
                format
                    .block_bits
                    .checked_mul(format.blocks)
                    .map(|bits| maximum.max(bits))
                    .ok_or(FramePoolError::Capacity)
            })?;
        payload = payload
            .checked_add(maximum)
            .ok_or(FramePoolError::Capacity)?;
        maximums.push((id, maximum));
    }
    let transport_metadata = maximums
        .len()
        .checked_mul(2 * std::mem::size_of::<OwnedTransportPayload>())
        .ok_or(FramePoolError::Capacity)?;
    let each_frame = payload
        .checked_add(transport_metadata)
        .and_then(|bytes| {
            bytes.checked_add(
                std::mem::size_of::<FrameStorage>() + std::mem::size_of::<Option<FrameInput>>(),
            )
        })
        .ok_or(FramePoolError::Capacity)?;
    let each_receipt = std::mem::size_of::<Emission>()
        + std::mem::size_of::<Option<Box<Emission>>>()
        + 3 * std::mem::size_of::<usize>();
    let bytes = each_frame
        .checked_mul(limits.payload_slots.get())
        .and_then(|bytes| {
            each_receipt
                .checked_mul(limits.receipt_slots.get())
                .and_then(|receipts| bytes.checked_add(receipts))
        })
        .and_then(|bytes| {
            bytes.checked_add(
                std::mem::size_of::<FramePool>()
                    + maximums.len() * std::mem::size_of::<(TransportId, usize)>()
                    + 3 * std::mem::size_of::<usize>(),
            )
        })
        .filter(|bytes| *bytes <= limits.maximum_buffer_bytes)
        .ok_or(FramePoolError::Capacity)?;
    Ok((maximums, bytes))
}

impl FramePool {
    pub(crate) fn new(
        identity: ReceiveIdentity,
        plan: Arc<CompiledRel99>,
        port: &DedicatedTransmitPort,
        limits: FramePoolLimits,
        ledger: &mut ResourceLedger,
    ) -> Result<Self, FramePoolError> {
        if !port.available() {
            return Err(FramePoolError::Transmit(
                crate::dedicated_tx::TransmitError::Closed,
            ));
        }
        let (maximums, bytes) = storage_requirements(&plan, limits)?;
        let lease = ledger
            .reserve(
                identity.ue,
                Resources {
                    queue_bytes: u64::try_from(bytes).map_err(|_| FramePoolError::Capacity)?,
                    ..Resources::default()
                },
            )
            .map_err(FramePoolError::Admission)?;
        let owner = Arc::new(());
        let receipts = (0..limits.receipt_slots.get())
            .map(|_| {
                Emission::idle(identity, plan.clone(), port.frame_source(), owner.clone())
                    .map(|emission| Some(Box::new(emission)))
                    .map_err(FramePoolError::Transmit)
            })
            .collect::<Result<Vec<_>, _>>();
        let receipts = match receipts {
            Ok(receipts) => receipts,
            Err(error) => {
                ledger.release(&lease).map_err(FramePoolError::Admission)?;
                return Err(error);
            }
        };
        let frames = (0..limits.payload_slots.get())
            .map(|slot| {
                let spare = maximums
                    .iter()
                    .map(|(transport, maximum)| OwnedTransportPayload {
                        transport: *transport,
                        bits: Vec::with_capacity(*maximum),
                    })
                    .collect();
                Some(FrameInput {
                    identity,
                    start: ChipInstant {
                        epoch: identity.ue.epoch,
                        index: 0,
                    },
                    tfci: Tfci::new(0).expect("placeholder TFCI"),
                    transports: Vec::with_capacity(maximums.len()),
                    emission: None,
                    retry_after: None,
                    storage: Box::new(FrameStorage {
                        spare,
                        reclaimed: None,
                        pool: Some(FramePoolTag {
                            owner: owner.clone(),
                            slot,
                        }),
                    }),
                })
            })
            .collect();
        Ok(Self {
            owner,
            identity,
            plan,
            maximums,
            frames,
            receipts,
            lease,
            bytes,
        })
    }
    pub fn identity(&self) -> ReceiveIdentity {
        self.identity
    }
    #[cfg(test)]
    pub fn storage_bytes(&self) -> usize {
        self.bytes
    }
    #[cfg(test)]
    pub fn available_payloads(&self) -> usize {
        self.frames.iter().flatten().count()
    }
    #[cfg(test)]
    pub fn available_receipts(&self) -> usize {
        self.receipts
            .iter()
            .flatten()
            .filter(|receipt| receipt.reusable())
            .count()
    }
    pub(crate) fn plan(&self) -> &Arc<CompiledRel99> {
        &self.plan
    }
    pub(crate) fn admitted_by(&self, ledger: &ResourceLedger) -> bool {
        ledger.contains(&self.lease)
    }
    pub(crate) fn bound_to(&self, port: &DedicatedTransmitPort) -> bool {
        self.frames.iter().all(Option::is_some)
            && self.receipts.iter().all(|receipt| {
                receipt
                    .as_ref()
                    .is_some_and(|receipt| port.accepts_source(receipt))
            })
    }
    pub(super) fn acquire(
        &mut self,
        start: ChipInstant,
        tfci: Tfci,
        definition: &FrameDefinition,
        port: &DedicatedTransmitPort,
    ) -> Result<(FrameInput, TransportSubmission), FramePoolError> {
        let frame_slot = self
            .frames
            .iter()
            .position(Option::is_some)
            .ok_or(FramePoolError::Capacity)?;
        let receipt_slot = self
            .receipts
            .iter()
            .position(|receipt| receipt.as_ref().is_some_and(|receipt| receipt.reusable()))
            .ok_or(FramePoolError::Capacity)?;
        let submission = self.receipts[receipt_slot]
            .as_mut()
            .expect("available receipt")
            .reset(start, definition.end, tfci, port.frame_source())
            .map_err(FramePoolError::Transmit)?;
        let mut input = self.frames[frame_slot].take().expect("available frame");
        input.start = start;
        input.retry_after = None;
        input.tfci = tfci;
        input.emission = self.receipts[receipt_slot].take();
        for &(id, _, _, bits) in definition.specifications[..definition.count]
            .iter()
            .flatten()
        {
            let at = input
                .storage
                .spare
                .iter()
                .position(|transport| transport.transport == id)
                .expect("reserved transport buffer");
            let mut transport = input.storage.spare.swap_remove(at);
            assert!(
                bits <= transport.bits.capacity(),
                "compiled format must fit its reserved maximum"
            );
            transport.bits.resize(bits, Bit::Zero);
            transport.bits.fill(Bit::Zero);
            input.transports.push(transport);
        }
        Ok((input, submission))
    }
    fn accepts_emission(&self, emission: &Emission) -> bool {
        emission.identity == self.identity
            && Arc::ptr_eq(&emission.plan, &self.plan)
            && emission
                .pool()
                .is_some_and(|pool| Arc::ptr_eq(pool, &self.owner))
    }
    fn insert_emission(&mut self, mut emission: Box<Emission>) {
        emission.fail();
        let at = self
            .receipts
            .iter()
            .position(Option::is_none)
            .expect("returning an owned receipt credit");
        self.receipts[at] = Some(emission);
    }
    pub fn recycle(&mut self, mut input: FrameInput) -> Result<(), FrameInput> {
        let Some(tag) = input.storage.pool.as_ref() else {
            return Err(input);
        };
        if !Arc::ptr_eq(&tag.owner, &self.owner)
            || tag.slot >= self.frames.len()
            || self.frames[tag.slot].is_some()
            || input.identity != self.identity
        {
            return Err(input);
        }
        let returning =
            usize::from(input.emission.is_some()) + usize::from(input.storage.reclaimed.is_some());
        if returning
            > self
                .receipts
                .iter()
                .filter(|receipt| receipt.is_none())
                .count()
            || input
                .emission
                .as_ref()
                .is_some_and(|emission| !self.accepts_emission(emission))
            || input
                .storage
                .reclaimed
                .as_ref()
                .is_some_and(|emission| !self.accepts_emission(emission))
        {
            return Err(input);
        }
        if input.transports.capacity() != self.maximums.len()
            || input.storage.spare.capacity() != self.maximums.len()
            || input.transports.len() + input.storage.spare.len() != self.maximums.len()
            || self.maximums.iter().any(|(id, maximum)| {
                let mut matching = input
                    .transports
                    .iter()
                    .chain(&input.storage.spare)
                    .filter(|transport| transport.transport == *id);
                matching
                    .next()
                    .is_none_or(|transport| transport.bits.capacity() != *maximum)
                    || matching.next().is_some()
            })
        {
            return Err(input);
        }
        let slot = tag.slot;
        if let Some(emission) = input.emission.take() {
            self.insert_emission(emission);
        }
        if let Some(emission) = input.storage.reclaimed.take() {
            self.insert_emission(emission);
        }
        input.storage.spare.append(&mut input.transports);
        self.frames[slot] = Some(input);
        Ok(())
    }
    pub fn reclaim_retired(
        &mut self,
        session: &mut DedicatedTransmitSession,
    ) -> Result<usize, FramePoolError> {
        if session.identity() != self.identity
            || !Arc::ptr_eq(session.plan(), &self.plan)
            || session.phase() != TransmitPhase::Retired
        {
            return Err(FramePoolError::Identity);
        }
        session
            .reclaim_emissions(&self.owner.clone(), |emission| {
                if !self.accepts_emission(&emission) || self.receipts.iter().all(Option::is_some) {
                    return Err(emission);
                }
                self.insert_emission(emission);
                Ok(())
            })
            .map_err(FramePoolError::Transmit)
    }
    pub(crate) fn retire(self, ledger: &mut ResourceLedger) -> Result<(), RejectedFramePool> {
        if self.frames.iter().any(Option::is_none)
            || self
                .receipts
                .iter()
                .any(|receipt| receipt.as_ref().is_none_or(|receipt| !receipt.reusable()))
        {
            return Err(RejectedFramePool {
                error: FramePoolError::Pending,
                pool: Box::new(self),
            });
        }
        match ledger.release(&self.lease) {
            Ok(()) => Ok(()),
            Err(error) => Err(RejectedFramePool {
                error: FramePoolError::Admission(error),
                pool: Box::new(self),
            }),
        }
    }
}
