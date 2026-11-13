// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_mac::hsdpa::ProcessId;

pub(super) struct RadioChange {
    pub(super) identity: ReceiveIdentity,
    cursor: usize,
    drained: bool,
    feedback_closed_and_drained: bool,
}

impl Session {
    fn validate_released_queues(&self, removed: &[bool; 8]) -> Result<(), Error> {
        for (index, remove) in removed.iter().copied().enumerate() {
            if !remove {
                continue;
            }
            if !self.queues.can_discard_queue(index as u8) {
                return Err(Error::Phase);
            }
            match (
                self.queues.staged_packet(index as u8),
                self.staged_buffer_carriers[index],
            ) {
                (None, None) => (),
                (Some(_), Some(owner)) => {
                    let slot = self
                        .links
                        .get(owner.index())
                        .and_then(|link| link.scheduling.as_ref())
                        .and_then(|resources| resources.staging_buffers.get(index))
                        .ok_or(Error::Phase)?;
                    if slot.is_some() {
                        return Err(Error::Phase);
                    }
                }
                _ => return Err(Error::Phase),
            }
        }
        for link in &self.links {
            if link.pending.is_some()
                || link.queue_commands.iter().any(Option::is_some)
                || !link.packets.flights_drained()
            {
                return Err(Error::Phase);
            }
            for index in 0..link.recycled_blocks.len() {
                let process = ProcessId::new(index as u8).expect("admitted HARQ process");
                let Some(block) = link.packets.payload(process).map_err(Error::Packet)? else {
                    continue;
                };
                if removed[usize::from(block.packet().queue)]
                    && (link.packets.process_state(process).map_err(Error::Packet)?
                        != umts_mac::hsdpa::ProcessState::Ready
                        || link.recycled_blocks[index].is_some())
                {
                    return Err(Error::Phase);
                }
            }
        }
        Ok(())
    }

    fn discard_released_queues(&mut self, removed: &[bool; 8]) {
        // Every fallible ownership check is complete before the first mutation.
        for (index, remove) in removed.iter().copied().enumerate() {
            if !remove {
                continue;
            }
            if let Some(buffer) = self
                .queues
                .discard_queue(index as u8)
                .expect("released queue ownership prevalidated")
            {
                let owner = self.staged_buffer_carriers[index]
                    .take()
                    .expect("released staged buffer owner prevalidated");
                self.links[owner.index()]
                    .scheduling
                    .as_mut()
                    .expect("released staged resource owner prevalidated")
                    .staging_buffers[index] = Some(buffer);
            }
            for link in &mut self.links {
                link.staged_feedback_training[index] = adaptation::FeedbackTraining::None;
            }
        }
        for link in &mut self.links {
            for index in 0..link.recycled_blocks.len() {
                let process = ProcessId::new(index as u8).expect("admitted HARQ process");
                let release = link
                    .packets
                    .payload(process)
                    .expect("released process prevalidated")
                    .is_some_and(|block| removed[usize::from(block.packet().queue)]);
                if release {
                    link.recycled_blocks[index] = link
                        .packets
                        .cancel_idle_payload(process)
                        .expect("released HARQ payload ownership prevalidated");
                }
            }
        }
    }

    pub(crate) fn retire_released_route(&mut self, bank: &DedicatedBearers) -> Result<bool, Error> {
        if !matches!(self.phase(), Phase::Active | Phase::ChangingRadio) {
            return Err(Error::Phase);
        }
        if bank.identity().ue != self.identity.ue || bank.identity() == self.identity {
            return Err(Error::Identity);
        }
        if let Some(route) = bank.hsdpa_route(bank.identity()).map_err(Error::Bearer)? {
            return if Arc::ptr_eq(route, self.procedure.construction()) {
                Ok(false)
            } else {
                Err(Error::Identity)
            };
        }
        // TS 25.331 8.5.25 resets MAC-hs when the final HS-DSCH bearer mapping is removed.
        self.detached_route = true;
        self.begin_retirement();
        Ok(true)
    }

    pub(crate) fn begin_radio_change(&mut self, next: ReceiveIdentity) -> Result<(), Error> {
        if next.ue != self.identity.ue || next == self.identity {
            return Err(Error::Identity);
        }
        if !matches!(self.phase(), Phase::Active | Phase::ChangingRadio) {
            return Err(Error::Phase);
        }
        // The committed mapping has already replaced this receiver, so keep its previous identity
        // until its orders settle.
        match &mut self.radio_change {
            Some(change) => change.identity = next,
            None => {
                self.radio_change = Some(RadioChange {
                    identity: next,
                    cursor: 0,
                    drained: false,
                    feedback_closed_and_drained: false,
                })
            }
        }
        Ok(())
    }

    pub(crate) fn settle_radio_change_orders(
        &mut self,
        now: ChipInstant,
        command_pending: bool,
    ) -> Result<bool, Error> {
        if self.phase() != Phase::ChangingRadio {
            return Err(Error::Phase);
        }
        // This includes a queued TX command whose acceptance has not reached the
        // Session yet. Its old receive identity must remain valid for collection.
        if command_pending {
            return Ok(false);
        }
        let progress = self.advance_secondary_order(now)?;
        if progress == OrderProgress::Idle && !self.secondary_order_feedback_pending() {
            return Ok(true);
        }
        if progress != OrderProgress::Confirmed
            && self
                .radio_change
                .as_ref()
                .is_some_and(|change| change.feedback_closed_and_drained)
        {
            // The previous ACK source is closed and drained. Neither a retry on that
            // mapping nor a guessed codebook on the new mapping is safe.
            self.secondary_control = SecondaryControl::Recovery;
            return Err(Error::Phase);
        }
        Ok(false)
    }

    pub(crate) fn drain_radio_change(&mut self, budget: NonZeroUsize) -> Result<bool, Error> {
        if self.phase() != Phase::ChangingRadio {
            return Err(Error::Phase);
        }
        // Acquire closure before draining so a producer cannot publish after the empty observation.
        let closed = !self.feedback.available();
        let mut drained = false;
        for _ in 0..budget.get() {
            let report = if let Some(policy) = self.links[0].feedback_policy {
                self.poll_classified_feedback(&policy)
            } else {
                self.poll_feedback(|_| Feedback::Dtx)
            };
            if report.is_none() {
                drained = true;
                break;
            }
        }
        self.radio_change
            .as_mut()
            .expect("changing radio")
            .feedback_closed_and_drained = closed && drained;
        self.resolve_packets(budget)?;
        if self
            .links
            .iter()
            .any(|link| link.pending.is_some() || link.queue_commands.iter().any(Option::is_some))
        {
            return Ok(false);
        }
        if closed && drained {
            let total = self
                .links
                .iter()
                .map(|link| link.queue_commands.len())
                .sum::<usize>();
            for _ in 0..budget.get().min(total) {
                let change = self.radio_change.as_mut().expect("changing radio");
                let index = change.cursor;
                change.cursor = (index + 1) % total;
                let mut offset = index;
                for link in &mut self.links {
                    if offset < link.queue_commands.len() {
                        link.packets
                            .release_closed_feedback(
                                ProcessId::new(offset as u8).expect("admitted process"),
                            )
                            .map_err(Error::Packet)?;
                        break;
                    }
                    offset -= link.queue_commands.len();
                }
            }
        }
        let ready = self.links.iter().all(|link| link.packets.flights_drained())
            && (self.feedback.outstanding() == 0 || (closed && drained));
        self.radio_change.as_mut().expect("changing radio").drained = ready;
        Ok(ready)
    }

    pub(crate) fn finish_radio_change(
        &mut self,
        bank: &DedicatedBearers,
        feedback: FeedbackPort,
    ) -> Result<FeedbackPort, (Error, FeedbackPort)> {
        let validate = || {
            if self.phase() != Phase::ChangingRadio
                || self
                    .radio_change
                    .as_ref()
                    .is_none_or(|change| change.identity != bank.identity() || !change.drained)
                || feedback.ue() != self.identity.ue
            {
                return Err(Error::Identity);
            }
            if !feedback.available()
                || feedback.outstanding() != 0
                || self.links.iter().any(|link| {
                    link.pending.is_some()
                        || link.queue_commands.iter().any(Option::is_some)
                        || !link.packets.flights_drained()
                })
                || (self.feedback.available() && self.feedback.outstanding() != 0)
            {
                return Err(Error::Phase);
            }
            let route = bank.hsdpa_route(bank.identity()).map_err(Error::Bearer)?;
            if !route.is_some_and(|route| Arc::ptr_eq(route, self.procedure.construction())) {
                return Err(Error::Identity);
            }
            let downlink = bank
                .downlink_clock(bank.identity())
                .map_err(Error::Bearer)?;
            let construction = self.procedure.construction();
            let mut retained_flows = [false; 8];
            let mut released_flows = [false; 8];
            for mapping in construction.mappings() {
                let bearer = crate::dedicated_bearers::BearerId::new(mapping.bearer)
                    .map_err(Error::Bearer)?;
                if bank
                    .has_active_hsdpa_bearer(bank.identity(), bearer)
                    .map_err(Error::Bearer)?
                {
                    bank.hsdpa_grant(bank.identity(), bearer, downlink.anchor.time)
                        .map_err(Error::Bearer)?;
                    retained_flows[usize::from(mapping.flow)] = true;
                } else {
                    released_flows[usize::from(mapping.flow)] = true;
                }
            }
            // Queue bytes have no per-bearer ownership tag. A mixed flow must
            // have been rejected before publishing the RRC release.
            if retained_flows
                .iter()
                .zip(released_flows)
                .any(|(kept, removed)| *kept && removed)
            {
                return Err(Error::Identity);
            }
            let mut removed = [false; 8];
            for queue in construction.parameters().queues.queues() {
                removed[usize::from(queue.id())] = !retained_flows[usize::from(queue.flow())];
            }
            self.validate_released_queues(&removed)?;
            let timing =
                FeedbackTiming::new(downlink.anchor, bank.frame_anchor(), self.downlink_origin)
                    .map_err(Error::Timing)?;
            let first = timing
                .next_pdsch(downlink.anchor.time)
                .map_err(Error::Timing)?;
            let origin = timing.acknowledgement(first).map_err(Error::Timing)?;
            if self.order_progress() != OrderProgress::Idle
                || self.secondary_order_feedback_pending()
            {
                return Err(Error::Phase);
            }
            Ok((origin, removed))
        };
        let (next_origin, removed) = match validate() {
            Ok(result) => result,
            Err(error) => return Err((error, feedback)),
        };
        if let SecondaryControl::Dual {
            origin, procedure, ..
        } = &mut self.secondary_control
        {
            if let Err(error) = procedure.rebase_feedback_origin(next_origin) {
                return Err((Error::Timing(error), feedback));
            }
            *origin = next_origin;
            let mode = if procedure.state() == SecondaryState::Active {
                crate::hsdpa_rx::FeedbackMode::Dual
            } else {
                crate::hsdpa_rx::FeedbackMode::Single
            };
            self.feedback_mode = Some(crate::hsdpa_rx::FeedbackModeSchedule::fixed(
                next_origin,
                mode,
            ));
        }
        // TS 25.331 8.6.5.8 deletes only removed queues. Surviving queues keep TSNs.
        self.discard_released_queues(&removed);
        // TS 25.331 8.6.6.27 does not reset MAC-hs for this retained same-cell configuration.
        self.identity = bank.identity();
        self.radio_change = None;
        for link in &mut self.links {
            link.quality = feedback::Quality::default();
            link.feedback_snapshot = FeedbackSnapshot::default();
        }
        Ok(std::mem::replace(&mut self.feedback, feedback))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::at;

    #[test]
    fn partial_queue_release_recycles_both_cells_and_preserves_retained_tsn_and_peer() {
        use crate::hsdpa_packet::PacketEncoder;
        use crate::test_support::count;
        use umts_dsp::hsdpa::CrcMethod;
        use umts_mac::hsdpa::MacdPdu;
        use umts_types::{bits::Bits, hsdpa::CarrierId};

        let (mut session, _receiver, _cell) =
            super::super::dual_tests::prepared_dual_with_bearers(true);
        let (mut peer, _peer_receiver, _peer_cell) = super::super::dual_tests::prepared_dual();
        peer.fixture_enqueue_pdu(&[0x77; 40], 320);
        let mut encoder =
            PacketEncoder::new(count(352), count(100_000), CrcMethod::Method1).unwrap();
        for carrier in [CarrierId::Primary, CarrierId::Secondary] {
            for queue in 0..2 {
                session.queues.enqueue_fixture(
                    queue,
                    MacdPdu {
                        size_index: 0,
                        bits: Bits::new(&[queue + 1; 40], 320).unwrap(),
                    },
                );
                let buffer = encoder.buffer();
                assert!(
                    session.links[carrier.index()]
                        .packets
                        .encode(
                            &mut session.queues,
                            &mut encoder,
                            queue,
                            ProcessId::new(queue).unwrap(),
                            buffer,
                        )
                        .unwrap_or_else(|_| panic!("fixture payload encoding"))
                        .is_none()
                );
            }
        }
        for queue in 0..2 {
            session.queues.enqueue_fixture(
                queue,
                MacdPdu {
                    size_index: 0,
                    bits: Bits::new(&[queue + 3; 40], 320).unwrap(),
                },
            );
        }
        let owner = CarrierId::Secondary;
        let buffer = session.links[owner.index()]
            .scheduling
            .as_mut()
            .unwrap()
            .staging_buffers[0]
            .take()
            .unwrap();
        assert!(
            session
                .queues
                .prepare_queue(&mut encoder, session.links[0].packets.category(), 0, buffer)
                .unwrap_or_else(|_| panic!("fixture staged payload"))
                .is_none()
        );
        let removed = [true, false, false, false, false, false, false, false];
        // A missing producer must reject before any queued ciphertext or HARQ payload changes.
        assert!(session.validate_released_queues(&removed).is_err());
        assert_eq!(session.queues.queued(0), 1);
        assert!(
            session.links[0]
                .packets
                .payload(ProcessId::new(0).unwrap())
                .unwrap()
                .is_some()
        );
        session.staged_buffer_carriers[0] = Some(owner);
        session.validate_released_queues(&removed).unwrap();
        session.discard_released_queues(&removed);
        assert_eq!(session.queues.queued(0), 0);
        assert_eq!(session.queues.pending_packet_bits(0), 0);
        assert!(session.queues.staged_packet(0).is_none());
        assert_eq!(session.staged_buffer_carriers[0], None);
        assert!(
            session.links[owner.index()]
                .scheduling
                .as_ref()
                .unwrap()
                .staging_buffers[0]
                .is_some()
        );
        assert_eq!(session.queues.queued(1), 1);
        for (index, link) in session.links.iter().enumerate() {
            assert!(
                link.packets
                    .payload(ProcessId::new(0).unwrap())
                    .unwrap()
                    .is_none()
            );
            assert!(link.recycled_blocks[0].is_some());
            assert_eq!(
                link.packets
                    .payload(ProcessId::new(1).unwrap())
                    .unwrap()
                    .unwrap()
                    .packet()
                    .sequence,
                index as u8
            );
        }
        let buffer = encoder.buffer();
        assert!(
            session
                .queues
                .prepare_queue(&mut encoder, session.links[0].packets.category(), 1, buffer)
                .unwrap_or_else(|_| panic!("retained queue staging"))
                .is_none()
        );
        assert_eq!(session.queues.staged_packet(1).unwrap().sequence, 2);
        assert_eq!(peer.queues.queued(0), 1);
        // A later mapping change can encounter an already deleted queue safely.
        session.validate_released_queues(&removed).unwrap();
        session.discard_released_queues(&removed);
        assert_eq!(session.queues.staged_packet(1).unwrap().sequence, 2);
        session.begin_retirement();
        assert_eq!(session.phase(), Phase::Retiring);
        assert!(!peer.retirement.started());
        assert_eq!(peer.queues.queued(0), 1);
    }

    #[test]
    fn closed_old_receiver_is_not_retryable_order_backpressure() {
        let (mut session, receiver, _budget) = super::super::dual_tests::prepared_dual();
        session.configure_secondary_control(at(0)).unwrap();
        drop(receiver);
        let transmission = OrderTransmission {
            generation: 1,
            ordinal: 1,
            order: umts_dsp::hsdpa::SecondaryCellOrder::Deactivate,
            scch_start: at(0),
            feedback_start: at(12 * umts_types::time::CHIPS_PER_SLOT),
            effective: at(15 * umts_types::time::CHIPS_PER_SLOT),
            codebook_effective: at(18 * umts_types::time::CHIPS_PER_SLOT),
        };
        // Collection can mark the previous flight resolved/recovery rather than
        // retaining it forever as if a consumer could free request capacity.
        assert_eq!(
            session.request_secondary_order_feedback(transmission),
            Err(Error::Phase)
        );
        assert!(!session.secondary_order_feedback_pending());
    }
}
