// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_bearers::{BearerError, DedicatedBearers, HsdpaGrant, TransmitError};
use crate::hsdpa_tx::{CompletedTransmission, PreparedTransmission, WaveformBuffer};
use std::num::{NonZeroU8, NonZeroU64};
use umts_mac::hsdpa::{Feedback, ScheduledTransmission, TransmissionKey};
use umts_security::ciphering::BearerCiphers;
use umts_types::{UeKey, hsdpa::Category, time::ChipInstant};

const MAC_HS_QUEUES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerError {
    Harq(EntityError),
    Flight(FlightError),
    Retiring,
    Process,
    Queue(MacError),
}

pub struct PacketQueues {
    ue: UeKey,
    retiring: bool,
    queues: Box<[Option<PacketQueue>; MAC_HS_QUEUES]>,
    staged: Box<[Option<CodedBlock>; MAC_HS_QUEUES]>,
}

impl PacketQueues {
    pub fn new(ue: UeKey) -> Self {
        Self {
            ue,
            retiring: false,
            queues: Box::new(std::array::from_fn(|_| None)),
            staged: Box::new(std::array::from_fn(|_| None)),
        }
    }

    pub fn begin_retirement(&mut self) {
        self.retiring = true;
    }

    pub fn cancel_all_staged(&mut self) -> Result<(), OwnerError> {
        for queue in 0..MAC_HS_QUEUES {
            self.cancel_staged(queue as u8)?;
        }
        Ok(())
    }

    pub fn queued(&self, queue: u8) -> usize {
        self.queues
            .get(usize::from(queue))
            .and_then(Option::as_ref)
            .map_or(0, PacketQueue::len)
    }

    pub fn pending_packet_bits(&self, queue: u8) -> usize {
        self.queues
            .get(usize::from(queue))
            .and_then(Option::as_ref)
            .map_or(0, PacketQueue::pending_packet_bits)
    }

    pub fn add_queue(&mut self, queue: Box<PacketQueue>) -> Result<(), Box<PacketQueue>> {
        if self.retiring || !queue.is_empty() {
            return Err(queue);
        }
        let Some(slot) = self.queues.get_mut(usize::from(queue.id())) else {
            return Err(queue);
        };
        if slot.is_some() {
            return Err(queue);
        }
        *slot = Some(*queue);
        Ok(())
    }

    pub fn enqueue(
        &mut self,
        bank: &mut DedicatedBearers,
        grant: HsdpaGrant,
        now: ChipInstant,
        ciphers: &BearerCiphers,
    ) -> Result<Option<()>, TransmitError<MacError>> {
        if self.retiring {
            return Err(BearerError::Phase.into());
        }
        if grant.identity.ue != self.ue || grant.identity != bank.identity() {
            return Err(BearerError::Owner.into());
        }
        let queue = self
            .queues
            .get_mut(usize::from(grant.queue))
            .and_then(Option::as_mut)
            .ok_or(BearerError::Mapping)?;
        if queue.available() == 0 {
            return Err(TransmitError::Admission(MacError::Capacity));
        }
        bank.enqueue_hsdpa(grant, now, ciphers, queue)
    }

    #[cfg(test)]
    pub(crate) fn enqueue_fixture(&mut self, queue: u8, pdu: umts_mac::hsdpa::MacdPdu<'_>) {
        self.queues[usize::from(queue)]
            .as_mut()
            .expect("fixture queue")
            .enqueue(pdu)
            .expect("fixture capacity");
    }

    pub fn staged_packet(&self, queue: u8) -> Option<PackedPdu> {
        self.staged
            .get(usize::from(queue))
            .and_then(Option::as_ref)
            .map(CodedBlock::packet)
    }

    pub fn prepare_queue(
        &mut self,
        encoder: &mut PacketEncoder,
        category: Category,
        queue: u8,
        buffer: BlockBuffer,
    ) -> Result<Option<BlockBuffer>, RejectedBlock> {
        let index = usize::from(queue);
        if self.retiring || self.staged.get(index).is_none_or(Option::is_some) {
            return Err(RejectedBlock {
                error: Error::Phase,
                buffer,
            });
        }
        let Some(queue) = self.queues.get_mut(index).and_then(Option::as_mut) else {
            return Err(RejectedBlock {
                error: Error::Mac(MacError::Mapping),
                buffer,
            });
        };
        match encoder.prepare_payload(queue, category, buffer)? {
            PayloadPreparation::Prepared(block) => {
                self.staged[index] = Some(block);
                Ok(None)
            }
            PayloadPreparation::Empty(buffer) => Ok(Some(buffer)),
        }
    }

    pub(crate) fn can_discard_queue(&self, queue: u8) -> bool {
        let index = usize::from(queue);
        self.queues
            .get(index)
            .and_then(Option::as_ref)
            .is_some_and(|queue| {
                queue.can_discard(self.staged[index].as_ref().map(CodedBlock::packet))
            })
    }

    pub(crate) fn discard_queue(&mut self, queue: u8) -> Result<Option<BlockBuffer>, OwnerError> {
        let buffer = self.cancel_staged(queue)?;
        self.queues
            .get_mut(usize::from(queue))
            .and_then(Option::as_mut)
            .ok_or(OwnerError::Process)?
            .discard()
            .map_err(OwnerError::Queue)?;
        Ok(buffer)
    }

    pub fn cancel_staged(&mut self, queue: u8) -> Result<Option<BlockBuffer>, OwnerError> {
        let slot = self
            .staged
            .get_mut(usize::from(queue))
            .ok_or(OwnerError::Process)?;
        let Some(packet) = slot.as_ref().map(CodedBlock::packet) else {
            return Ok(None);
        };
        self.queues
            .get_mut(usize::from(queue))
            .and_then(Option::as_mut)
            .ok_or(OwnerError::Process)?
            .cancel_staged(packet)
            .map_err(OwnerError::Queue)?;
        Ok(slot.take().map(CodedBlock::recycle))
    }
}

pub struct PacketOwner {
    ue: UeKey,
    harq: HarqEntity<CodedBlock>,
    flights: Box<[Option<Flight>]>,
    retiring: bool,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct RetiredPackets {
    ue: UeKey,
    harq: HarqEntity<CodedBlock>,
}

impl RetiredPackets {
    #[cfg(test)]
    pub fn into_parts(self) -> (UeKey, impl Iterator<Item = CodedBlock>) {
        (self.ue, self.harq.into_payloads())
    }
}

impl PacketOwner {
    pub fn category(&self) -> Category {
        self.harq.category()
    }

    pub(crate) const fn staging_metadata_bytes() -> usize {
        std::mem::size_of::<[Option<CodedBlock>; MAC_HS_QUEUES]>()
    }

    pub fn new(
        ue: UeKey,
        incarnation: NonZeroU64,
        category: Category,
        origin: ChipInstant,
        memory: &[NonZeroUsize],
        repetitions: NonZeroU8,
    ) -> Result<Self, OwnerError> {
        let harq = HarqEntity::new(ue, incarnation, category, origin, memory, repetitions)
            .map_err(OwnerError::Harq)?;
        Ok(Self {
            ue,
            harq,
            flights: (0..memory.len()).map(|_| None).collect(),
            retiring: false,
        })
    }

    pub fn owns_transmission(&self, key: TransmissionKey) -> bool {
        self.flights
            .get(usize::from(key.process().value()))
            .and_then(Option::as_ref)
            .is_some_and(|flight| flight.transmission().key == key)
    }

    pub fn process_state(
        &self,
        process: ProcessId,
    ) -> Result<umts_mac::hsdpa::ProcessState, OwnerError> {
        if self
            .flights
            .get(usize::from(process.value()))
            .is_some_and(Option::is_some)
        {
            return Ok(umts_mac::hsdpa::ProcessState::Busy);
        }
        self.harq.process_state(process).map_err(OwnerError::Harq)
    }

    pub fn next_downlink(&self, not_before: ChipInstant) -> Result<ChipInstant, OwnerError> {
        if self.retiring {
            return Err(OwnerError::Retiring);
        }
        self.harq
            .next_downlink(not_before)
            .map_err(OwnerError::Harq)
    }

    pub(crate) fn cancel_idle_payload(
        &mut self,
        process: ProcessId,
    ) -> Result<Option<BlockBuffer>, OwnerError> {
        if self
            .flights
            .get(usize::from(process.value()))
            .is_none_or(Option::is_some)
        {
            return Err(OwnerError::Process);
        }
        self.harq
            .cancel(process)
            .map(|block| block.map(CodedBlock::recycle))
            .map_err(OwnerError::Harq)
    }

    pub fn payload(&self, process: ProcessId) -> Result<Option<&CodedBlock>, OwnerError> {
        self.harq.payload(process).map_err(OwnerError::Harq)
    }

    pub fn bind_staged(
        &mut self,
        shared: &mut PacketQueues,
        queue: u8,
        process: ProcessId,
    ) -> Result<bool, OwnerError> {
        if shared.ue != self.ue {
            return Err(OwnerError::Process);
        }
        if self.retiring || shared.retiring {
            return Err(OwnerError::Retiring);
        }
        if self.process_state(process)? != umts_mac::hsdpa::ProcessState::Empty {
            return Err(OwnerError::Process);
        }
        let slot = shared
            .staged
            .get_mut(usize::from(queue))
            .ok_or(OwnerError::Process)?;
        let Some(packet) = slot.as_ref().map(CodedBlock::packet) else {
            return Ok(false);
        };
        if packet.transport_bits > self.harq.category().limits().maximum_transport_bits {
            return Err(OwnerError::Process);
        }
        let queue = shared
            .queues
            .get_mut(usize::from(queue))
            .and_then(Option::as_mut)
            .ok_or(OwnerError::Process)?;
        queue
            .commit_staged(packet, || {
                let block = slot.take().expect("reserved staged payload");
                self.harq.insert(process, block).map_err(|rejected| {
                    *slot = Some(rejected.payload);
                    rejected.error
                })
            })
            .map_err(|error| match error {
                QueuePublishError::Mac(error) => OwnerError::Queue(error),
                QueuePublishError::Admission(error) => OwnerError::Harq(error),
            })?;
        Ok(true)
    }

    pub fn soft_bits(&self, process: ProcessId) -> Result<NonZeroUsize, OwnerError> {
        self.harq.soft_bits(process).map_err(OwnerError::Harq)
    }

    pub fn transmitted_count(&self, process: ProcessId) -> Result<u64, OwnerError> {
        self.harq
            .transmitted_count(process)
            .map_err(OwnerError::Harq)
    }

    pub fn encode(
        &mut self,
        shared: &mut PacketQueues,
        encoder: &mut PacketEncoder,
        queue: u8,
        process: ProcessId,
        buffer: BlockBuffer,
    ) -> Result<Option<BlockBuffer>, RejectedBlock> {
        if self.retiring
            || shared.retiring
            || shared.ue != self.ue
            || shared.staged_packet(queue).is_some()
        {
            return Err(RejectedBlock {
                error: Error::Phase,
                buffer,
            });
        }
        let Some(queue) = shared
            .queues
            .get_mut(usize::from(queue))
            .and_then(Option::as_mut)
        else {
            return Err(RejectedBlock {
                error: Error::Mac(MacError::Mapping),
                buffer,
            });
        };
        encoder.admit(queue, &mut self.harq, process, buffer)
    }

    pub fn reserve(
        &mut self,
        process: ProcessId,
        downlink: ChipInstant,
        expected_ack: ChipInstant,
    ) -> Result<ScheduledTransmission, OwnerError> {
        if self.retiring {
            return Err(OwnerError::Retiring);
        }
        let slot = self
            .flights
            .get_mut(usize::from(process.value()))
            .ok_or(OwnerError::Process)?;
        if slot.is_some() {
            return Err(OwnerError::Process);
        }
        let transmission = self
            .harq
            .schedule(process, downlink, expected_ack)
            .map_err(OwnerError::Harq)?;
        match Flight::new(transmission, &self.harq) {
            Ok(flight) => *slot = Some(flight),
            Err(error) => {
                self.harq.lost(transmission.key).map_err(OwnerError::Harq)?;
                return Err(OwnerError::Flight(error));
            }
        }
        Ok(transmission)
    }

    pub fn complete_transmit(
        &mut self,
        completion: CompletedTransmission,
    ) -> Result<WaveformBuffer, RejectedCompletion> {
        let key = completion.transmission.transmission().key;
        match self.flight_mut(key.process()) {
            Ok(flight) => flight.complete_transmit(completion),
            Err(_) => Err(RejectedCompletion {
                error: FlightError::Identity,
                completion,
            }),
        }
    }

    // Only the synchronous preparation path may cancel without an original returned waveform.
    pub(super) fn cancel_preparation(&mut self, key: TransmissionKey) -> Result<(), OwnerError> {
        let flight = self.flight_mut(key.process())?;
        flight
            .record_transmit(key, false)
            .map_err(OwnerError::Flight)?;
        self.resolve(key.process())?;
        Ok(())
    }

    pub fn reject_transmit(
        &mut self,
        transmission: PreparedTransmission,
    ) -> Result<WaveformBuffer, RejectedCompletion> {
        self.complete_transmit(CompletedTransmission {
            transmission,
            submitted: false,
        })
    }

    #[cfg(test)]
    pub fn feedback(
        &mut self,
        key: TransmissionKey,
        observed: ChipInstant,
        feedback: Feedback,
    ) -> Result<(), OwnerError> {
        self.flight_mut(key.process())?
            .record_feedback(key, observed, feedback)
            .map_err(OwnerError::Flight)
    }

    pub fn request_feedback_cells(
        &mut self,
        process: ProcessId,
        port: &mut crate::hsdpa_rx::FeedbackPort,
        schedule: crate::hsdpa_rx::FeedbackModeSchedule,
    ) -> Result<crate::hsdpa_rx::RequestId, FeedbackError> {
        if self.retiring {
            return Err(FeedbackError::Flight(FlightError::State));
        }
        self.flight_mut(process)
            .map_err(|_| FeedbackError::Flight(FlightError::Identity))?
            .request_feedback_cells(port, schedule)
    }

    pub fn admit_feedback_cells(
        &mut self,
        report: &crate::hsdpa_rx::Report,
        carrier: umts_types::hsdpa::CarrierId,
        policy: &crate::hsdpa_rx::FeedbackPolicy,
    ) -> Option<Result<(ScheduledTransmission, Option<Feedback>), FeedbackError>> {
        let flight = self
            .flights
            .iter_mut()
            .flatten()
            .find(|flight| flight.matches_feedback_report(report))?;
        Some(
            flight
                .admit_feedback_cells(report, carrier, policy)
                .map(|feedback| (flight.transmission(), feedback)),
        )
    }

    pub fn request_feedback(
        &mut self,
        process: ProcessId,
        port: &mut crate::hsdpa_rx::FeedbackPort,
    ) -> Result<crate::hsdpa_rx::RequestId, FeedbackError> {
        if self.retiring {
            return Err(FeedbackError::Flight(FlightError::State));
        }
        self.flight_mut(process)
            .map_err(|_| FeedbackError::Flight(FlightError::Identity))?
            .request_feedback(port)
    }

    pub fn admit_feedback_report(
        &mut self,
        report: &crate::hsdpa_rx::Report,
        classify: impl FnOnce(&crate::hsdpa_rx::Observation<umts_dsp::hsdpa::HarqAck>) -> Feedback,
    ) -> Result<(), FeedbackError> {
        let crate::hsdpa_rx::Request::Ack { transmission, .. } = report.request else {
            return Err(FeedbackError::Request);
        };
        self.flight_mut(transmission.key.process())
            .map_err(|_| FeedbackError::Flight(FlightError::Identity))?
            .admit_feedback_report(report, classify)
    }

    pub fn resolve(
        &mut self,
        process: ProcessId,
    ) -> Result<Option<Resolution<CodedBlock>>, OwnerError> {
        let slot = self
            .flights
            .get_mut(usize::from(process.value()))
            .ok_or(OwnerError::Process)?;
        let result = slot
            .as_mut()
            .ok_or(OwnerError::Process)?
            .resolve(&mut self.harq)
            .map_err(OwnerError::Flight)?;
        if result.is_some() {
            *slot = None
        }
        Ok(result)
    }

    pub fn resolve_if_pending(
        &mut self,
        process: ProcessId,
    ) -> Result<Option<Resolution<CodedBlock>>, OwnerError> {
        if self
            .flights
            .get(usize::from(process.value()))
            .ok_or(OwnerError::Process)?
            .is_none()
        {
            return Ok(None);
        }
        self.resolve(process)
    }

    pub fn begin_retirement(&mut self) {
        self.retiring = true
    }

    /// RX feedback can be abandoned only after the original TX completion has returned its waveform.
    pub fn retire_process(&mut self, process: ProcessId) -> Result<bool, OwnerError> {
        if !self.retiring {
            return Err(OwnerError::Retiring);
        }
        self.release_flight(process)
    }

    /// The previous feedback receiver must be closed and its reports drained before abandoning an
    /// ACK.
    pub(crate) fn release_closed_feedback(
        &mut self,
        process: ProcessId,
    ) -> Result<bool, OwnerError> {
        if self
            .flights
            .get(usize::from(process.value()))
            .and_then(Option::as_ref)
            .is_some_and(Flight::has_feedback)
        {
            return Ok(false);
        }
        self.release_flight(process)
    }

    fn release_flight(&mut self, process: ProcessId) -> Result<bool, OwnerError> {
        let slot = self
            .flights
            .get_mut(usize::from(process.value()))
            .ok_or(OwnerError::Process)?;
        if let Some(flight) = slot {
            if !flight.transmit_result_received() {
                return Ok(false);
            }
            self.harq
                .lost(flight.transmission().key)
                .map_err(OwnerError::Harq)?;
            *slot = None;
        }
        Ok(true)
    }

    pub(crate) fn retirement_ready(&self) -> bool {
        self.retiring && self.flights_drained()
    }

    pub(crate) fn flights_drained(&self) -> bool {
        self.flights.iter().all(Option::is_none)
    }

    pub fn try_retire(self) -> Result<RetiredPackets, Self> {
        if !self.retirement_ready() {
            return Err(self);
        }
        Ok(RetiredPackets {
            ue: self.ue,
            harq: self.harq,
        })
    }

    fn flight_mut(&mut self, process: ProcessId) -> Result<&mut Flight, OwnerError> {
        self.flights
            .get_mut(usize::from(process.value()))
            .and_then(Option::as_mut)
            .ok_or(OwnerError::Process)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{at, count};
    use umts_mac::hsdpa::{MacdPdu, QueueFormat};
    use umts_types::bits::Bits;

    fn fixture() -> (PacketOwner, PacketQueues, PacketEncoder) {
        fixture_at(at(0))
    }

    fn fixture_at(origin: ChipInstant) -> (PacketOwner, PacketQueues, PacketEncoder) {
        let owner = PacketOwner::new(
            UeKey {
                epoch: at(0).epoch,
                slot: 0,
                generation: NonZeroU64::MIN,
            },
            NonZeroU64::MIN,
            Category::new(10).unwrap(),
            origin,
            &[count(28_800); 6],
            NonZeroU8::MIN,
        )
        .unwrap();
        let mut shared = PacketQueues::new(owner.ue);
        let mut sizes = [None; MAC_HS_QUEUES];
        sizes[0] = Some(count(8));
        let queue =
            PacketQueue::new(QueueFormat::new(0, sizes).unwrap(), count(2), count(4096)).unwrap();
        assert!(shared.add_queue(Box::new(queue)).is_ok());
        let encoder = PacketEncoder::new(count(137), count(100_000), CrcMethod::Method1).unwrap();
        (owner, shared, encoder)
    }

    fn enqueue(shared: &mut PacketQueues, value: u8) {
        shared.queues[0]
            .as_mut()
            .unwrap()
            .enqueue(MacdPdu {
                size_index: 0,
                bits: Bits::new(&[value], 8).unwrap(),
            })
            .unwrap();
    }

    #[test]
    fn two_carrier_harq_owners_share_one_mac_ehs_sequence() {
        let (mut primary, mut queues, _) = fixture();
        let mut secondary = PacketOwner::new(
            primary.ue,
            NonZeroU64::new(2).unwrap(),
            primary.category(),
            at(0),
            &[count(28_800); 6],
            NonZeroU8::MIN,
        )
        .unwrap();
        let mut sizes = [None; MAC_HS_QUEUES];
        sizes[0] = Some(count(8));
        queues.queues[0] = Some(
            PacketQueue::new(
                QueueFormat::ehs(0, 1, sizes).unwrap(),
                count(2),
                count(4096),
            )
            .unwrap(),
        );
        let mut encoder =
            PacketEncoder::new(count(144), count(100_000), CrcMethod::Method1).unwrap();
        let process = ProcessId::new(0).unwrap();
        for (sequence, owner) in [&mut primary, &mut secondary].into_iter().enumerate() {
            enqueue(&mut queues, sequence as u8);
            let buffer = encoder.buffer();
            assert!(
                owner
                    .encode(&mut queues, &mut encoder, 0, process, buffer)
                    .unwrap_or_else(|_| panic!("shared queue encoding"))
                    .is_none()
            );
            assert_eq!(
                owner.payload(process).unwrap().unwrap().packet().sequence,
                sequence as u8
            );
        }
        assert_eq!(queues.queued(0), 0);
        assert!(primary.payload(process).unwrap().is_some());
        assert!(secondary.payload(process).unwrap().is_some());
    }

    #[test]
    fn a_smaller_staged_block_keeps_uncommitted_pdus_and_sequence_order() {
        let (mut owner, mut shared, mut smaller) = fixture();
        let mut sizes = [None; MAC_HS_QUEUES];
        sizes[0] = Some(count(100));
        shared.queues[0] = Some(
            PacketQueue::new(QueueFormat::new(0, sizes).unwrap(), count(2), count(4096)).unwrap(),
        );
        for value in [0x5a, 0xa5] {
            shared.queues[0]
                .as_mut()
                .unwrap()
                .enqueue(MacdPdu {
                    size_index: 0,
                    bits: Bits::new(&[value; 13], 100).unwrap(),
                })
                .unwrap();
        }
        let mut larger =
            PacketEncoder::new(count(233), count(100_000), CrcMethod::Method1).unwrap();
        let buffer = larger.buffer();
        assert!(
            shared
                .prepare_queue(&mut larger, owner.category(), 0, buffer)
                .unwrap_or_else(|_| panic!("larger payload preparation"))
                .is_none()
        );
        assert_eq!(shared.staged_packet(0).unwrap().pdus, 2);
        assert_eq!(shared.queued(0), 2);
        let buffer = shared.cancel_staged(0).unwrap().unwrap();
        assert!(
            shared
                .prepare_queue(&mut smaller, owner.category(), 0, buffer)
                .unwrap_or_else(|_| panic!("smaller payload preparation"))
                .is_none()
        );
        let first = shared.staged_packet(0).unwrap();
        assert_eq!(first.pdus, 1);
        assert_eq!(first.sequence, 0);
        assert_eq!(shared.queued(0), 2);
        let process = ProcessId::new(0).unwrap();
        assert_eq!(
            owner.process_state(process),
            Ok(umts_mac::hsdpa::ProcessState::Empty)
        );
        assert!(owner.payload(process).unwrap().is_none());
        assert_eq!(owner.bind_staged(&mut shared, 0, process), Ok(true));
        assert_eq!(shared.queued(0), 1);
        let buffer = smaller.buffer();
        assert!(
            shared
                .prepare_queue(&mut smaller, owner.category(), 0, buffer)
                .unwrap_or_else(|_| panic!("remaining payload preparation"))
                .is_none()
        );
        let second = shared.staged_packet(0).unwrap();
        assert_eq!(second.pdus, 1);
        assert_eq!(second.sequence, 1);
        assert_eq!(
            owner.bind_staged(&mut shared, 0, ProcessId::new(1).unwrap()),
            Ok(true)
        );
        assert_eq!(shared.queued(0), 0);
    }

    #[test]
    fn staging_retains_payload_when_binding_is_busy_and_retires_unbound_bytes() {
        let (mut owner, mut shared, mut encoder) = fixture();
        let process = ProcessId::new(0).unwrap();
        enqueue(&mut shared, 0x5a);
        let buffer = encoder.buffer();
        assert!(
            owner
                .encode(&mut shared, &mut encoder, 0, process, buffer)
                .unwrap_or_else(|_| panic!("encode"))
                .is_none()
        );
        enqueue(&mut shared, 0xa5);
        let buffer = encoder.buffer();
        assert!(
            shared
                .prepare_queue(&mut encoder, owner.category(), 0, buffer)
                .unwrap_or_else(|_| panic!("stage"))
                .is_none()
        );
        let packet = shared.staged_packet(0).unwrap();
        assert_eq!(shared.queued(0), 1);
        assert_eq!(
            owner.bind_staged(&mut shared, 0, process),
            Err(OwnerError::Process)
        );
        assert_eq!(shared.staged_packet(0), Some(packet));
        owner.begin_retirement();
        assert_eq!(
            owner.bind_staged(&mut shared, 0, ProcessId::new(1).unwrap()),
            Err(OwnerError::Retiring)
        );
        let retired = owner.try_retire().unwrap_or_else(|_| panic!("retire"));
        let (_, blocks) = retired.into_parts();
        shared.cancel_all_staged().unwrap();
        let queues = (*shared.queues).into_iter().flatten();
        assert_eq!(blocks.count(), 1);
        assert_eq!(queues.map(|queue| queue.len()).sum::<usize>(), 1);
    }

    #[test]
    fn staging_binds_once_without_consuming_a_process_while_encoding() {
        let (mut owner, mut shared, mut encoder) = fixture();
        let process = ProcessId::new(0).unwrap();
        enqueue(&mut shared, 0x5a);
        let buffer = encoder.buffer();
        assert!(
            shared
                .prepare_queue(&mut encoder, owner.category(), 0, buffer)
                .unwrap_or_else(|_| panic!("stage"))
                .is_none()
        );
        assert_eq!(
            owner.process_state(process),
            Ok(umts_mac::hsdpa::ProcessState::Empty)
        );
        let packet = shared.staged_packet(0).unwrap();
        assert_eq!(owner.bind_staged(&mut shared, 0, process), Ok(true));
        assert!(shared.staged_packet(0).is_none());
        assert_eq!(owner.payload(process).unwrap().unwrap().packet(), packet);
        assert_eq!(
            owner.bind_staged(&mut shared, 0, ProcessId::new(1).unwrap()),
            Ok(false)
        );
    }

    #[test]
    fn waveform_preparation_reuses_harq_payload_and_rolls_back_only_unpublished_work() {
        use crate::{
            hsdpa_packet::{PrepareError, TransmitRequest},
            hsdpa_tx::{Amplitudes, WaveformBuffer, WaveformPreparer},
            radio_resources::RadioResources,
            test_support::source,
        };
        use umts_dsp::{
            hsdpa::{
                Modulation, ScchAssignment, ScchEncoder, TransmissionPlan, TransmissionVersion,
            },
            scrambling::PrimaryScramblingCode,
        };
        use umts_types::time::CHIPS_PER_SLOT;
        let origin = at(2 * CHIPS_PER_SLOT);
        let (mut owner, mut shared, mut encoder) = fixture_at(origin);
        let process = ProcessId::new(2).unwrap();
        enqueue(&mut shared, 0x5a);
        let coded = encoder.buffer();
        assert!(
            owner
                .encode(&mut shared, &mut encoder, 0, process, coded)
                .is_ok()
        );
        let source = source(true);
        let mut resources =
            RadioResources::new(origin.epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::MIN, NonZeroU8::MIN)
            .unwrap();
        let pool = *lease.codes();
        let mut preparer =
            WaveformPreparer::new(pool, PrimaryScramblingCode::new(0).unwrap()).unwrap();
        let scch = ScchEncoder::new(77).unwrap();
        let mut plan = TransmissionPlan::new(
            ScchAssignment::new(
                pool.first_data(),
                1,
                Modulation::Qpsk,
                0,
                0,
                TransmissionVersion::new(0).unwrap(),
                true,
            )
            .unwrap(),
            Category::new(10).unwrap(),
            &scch,
            count(28_800),
            count(100_000),
        )
        .unwrap();
        assert_eq!(
            plan.transport_bits(),
            owner.payload(process).unwrap().unwrap().transport_bits()
        );
        let rejected = match owner.prepare_waveform(
            &mut preparer,
            TransmitRequest {
                process,
                downlink: origin,
                expected_ack: at(30_000),
                plan: &mut plan,
                control_code: u16::MAX,
                amplitudes: Amplitudes {
                    control: 0.1,
                    data: 0.1,
                },
            },
            WaveformBuffer::default(),
        ) {
            Ok(_) => panic!("unallocated control code"),
            Err(rejected) => rejected,
        };
        assert_eq!(
            rejected.error,
            PrepareError::Waveform(crate::hsdpa_tx::Error::Codes)
        );
        assert!(owner.flights[usize::from(process.value())].is_none());
        assert_eq!(owner.payload(process).unwrap().unwrap().packet().pdus, 1);
        let prepared = owner
            .prepare_waveform(
                &mut preparer,
                TransmitRequest {
                    process,
                    downlink: at(origin.index + 3 * CHIPS_PER_SLOT),
                    expected_ack: at(40_000),
                    plan: &mut plan,
                    control_code: pool.control_codes().next().unwrap().index(),
                    amplitudes: Amplitudes {
                        control: 0.1,
                        data: 0.1,
                    },
                },
                rejected.buffer,
            )
            .unwrap_or_else(|rejected| panic!("{:?}", rejected.error));
        assert_eq!(plan.assignment().harq_process(), process.value());
        assert_eq!(
            plan.assignment().new_data(),
            prepared.transmission().new_data
        );
        assert_eq!(
            plan.control_bits(),
            &scch.encode(plan.assignment()).unwrap()
        );
        let rejected_key = prepared.transmission().key;
        assert!(owner.owns_transmission(rejected_key));
        assert!(owner.reject_transmit(prepared).is_ok());
        assert!(owner.owns_transmission(rejected_key));
        assert!(matches!(
            owner.resolve(process),
            Ok(Some(Resolution::NotTransmitted))
        ));
        assert!(!owner.owns_transmission(rejected_key));
        assert!(owner.payload(process).unwrap().is_some());
        let retry = owner
            .reserve(process, at(origin.index + 6 * CHIPS_PER_SLOT), at(50_000))
            .unwrap();
        assert!(owner.owns_transmission(retry.key));
        assert!(!owner.owns_transmission(rejected_key));
        owner.cancel_preparation(retry.key).unwrap();
        resources.release_hsdpa(lease).unwrap();
    }

    #[test]
    fn closed_feedback_does_not_discard_an_ack_waiting_for_bounded_resolution() {
        let (mut owner, mut shared, mut encoder) = fixture();
        let id = ProcessId::new(0).unwrap();
        enqueue(&mut shared, 0x5a);
        let buffer = encoder.buffer();
        assert!(
            owner
                .encode(&mut shared, &mut encoder, 0, id, buffer)
                .is_ok()
        );
        let scheduled = owner.reserve(id, at(0), at(30_000)).unwrap();
        let flight = owner.flight_mut(id).unwrap();
        flight.record_transmit(scheduled.key, true).unwrap();
        flight
            .record_feedback(scheduled.key, scheduled.expected_ack, Feedback::Ack)
            .unwrap();
        assert!(!owner.release_closed_feedback(id).unwrap());
        assert!(matches!(
            owner.resolve(id),
            Ok(Some(Resolution::Delivered(_)))
        ));
        assert!(owner.flights_drained());
        assert!(owner.payload(id).unwrap().is_none());
    }

    #[test]
    fn closed_feedback_preserves_payload_and_ndi_without_resetting_mac_hs() {
        let (mut owner, mut shared, mut encoder) = fixture();
        let id = ProcessId::new(0).unwrap();
        enqueue(&mut shared, 0x5a);
        let buffer = encoder.buffer();
        assert!(
            owner
                .encode(&mut shared, &mut encoder, 0, id, buffer)
                .is_ok()
        );
        let scheduled = owner.reserve(id, at(0), at(30_000)).unwrap();
        enqueue(&mut shared, 0xa5);
        assert!(!owner.release_closed_feedback(id).unwrap());
        assert!(owner.owns_transmission(scheduled.key));
        owner
            .flight_mut(id)
            .unwrap()
            .record_transmit(scheduled.key, true)
            .unwrap();
        assert!(owner.release_closed_feedback(id).unwrap());
        assert!(owner.flights_drained());
        assert_eq!(shared.queued(0), 1);
        assert_eq!(owner.payload(id).unwrap().unwrap().packet().pdus, 1);
        let retry = owner.reserve(id, at(38_400), at(68_400)).unwrap();
        assert_eq!(retry.new_data, scheduled.new_data);
        assert_ne!(retry.key, scheduled.key);
        assert!(!owner.owns_transmission(scheduled.key));
        assert!(owner.owns_transmission(retry.key));
    }

    #[test]
    fn retirement_keeps_queued_and_coded_data_until_original_tx_returns() {
        let (mut owner, mut shared, mut encoder) = fixture();
        let id = ProcessId::new(0).unwrap();
        enqueue(&mut shared, 0x5a);
        let buffer = encoder.buffer();
        assert!(
            owner
                .encode(&mut shared, &mut encoder, 0, id, buffer)
                .unwrap_or_else(|_| panic!("encoded"))
                .is_none()
        );
        let scheduled = owner.reserve(id, at(0), at(30_000)).unwrap();
        enqueue(&mut shared, 0xa5);
        owner.begin_retirement();
        assert_eq!(
            owner.reserve(id, at(38_400), at(68_400)),
            Err(OwnerError::Retiring)
        );
        assert!(!owner.retire_process(id).unwrap());
        owner = match owner.try_retire() {
            Err(owner) => owner,
            Ok(_) => panic!("pending TX retains owner"),
        };
        owner
            .flight_mut(id)
            .unwrap()
            .record_transmit(scheduled.key, false)
            .unwrap();
        assert!(owner.retire_process(id).unwrap());
        let retired = owner.try_retire().unwrap_or_else(|_| panic!("resolved TX"));
        let (_, mut blocks) = retired.into_parts();
        shared.cancel_all_staged().unwrap();
        let mut queues = (*shared.queues).into_iter().flatten();
        assert_eq!(blocks.next().unwrap().packet().pdus, 1);
        assert!(blocks.next().is_none());
        assert_eq!(queues.next().unwrap().len(), 1);
        assert!(queues.next().is_none());
    }

    #[test]
    fn completion_resolution_preserves_payload_until_ack_and_prevents_double_reservation() {
        let (mut owner, mut shared, mut encoder) = fixture();
        let id = ProcessId::new(0).unwrap();
        enqueue(&mut shared, 0x5a);
        let buffer = encoder.buffer();
        assert!(
            owner
                .encode(&mut shared, &mut encoder, 0, id, buffer)
                .is_ok()
        );
        let scheduled = owner.reserve(id, at(0), at(30_000)).unwrap();
        assert_eq!(
            owner.reserve(id, at(38_400), at(68_400)),
            Err(OwnerError::Process)
        );
        owner
            .feedback(scheduled.key, scheduled.expected_ack, Feedback::Ack)
            .unwrap();
        assert!(owner.resolve(id).unwrap().is_none());
        owner
            .flight_mut(id)
            .unwrap()
            .record_transmit(scheduled.key, true)
            .unwrap();
        assert!(matches!(
            owner.resolve(id).unwrap(),
            Some(Resolution::Delivered(_))
        ));
        assert!(owner.payload(id).unwrap().is_none());
    }
}
