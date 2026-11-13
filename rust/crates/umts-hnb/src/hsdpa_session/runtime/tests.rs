// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{hsdpa_packet::PlanFormat, test_support::count};
use umts_dsp::hsdpa::{Cqi, CrcMethod};
use umts_types::hsdpa::Category;

fn resources(
    cell: &CellSchedulingBudget,
    limit: NonZeroUsize,
) -> Result<SchedulingResources, umts_dsp::DspError> {
    let reservation = cell.reserve(limit).unwrap();
    let category = Category::new(10).unwrap();
    let rate = Cqi::new(6).unwrap().reference(category).unwrap();
    let encoders = EncoderBank::new(
        &[rate.transport_bits],
        count(1),
        count(100_000),
        CrcMethod::Method1,
        count(1_000_000),
    )?;
    let plans = PlanBank::new(
        &[PlanFormat::new(
            rate.assignment(1, 0, false)?,
            rate.transport_bits,
            count(28_800),
        )],
        category,
        1,
        count(1),
        count(100_000),
        count(1_000_000),
    )?;
    SchedulingResources::new(
        encoders,
        plans,
        std::num::NonZeroU8::new(2).unwrap(),
        SchedulingSettings {
            power: DownlinkPower::new(0.1, 0.1)?,
            maximum_cqi_age: NonZeroU64::MIN,
            selection_budget: count(1),
            cqi_offset_limit: 0,
            cqi_target_bler_percent: 10,
        },
        limit,
        reservation,
    )
}

#[test]
fn resource_owner_returns_cell_charge_on_rejection_and_drop() {
    let cell = CellSchedulingBudget::new(count(1_000_000));
    assert!(resources(&cell, count(1)).is_err());
    assert_eq!(cell.available_bytes(), 1_000_000);
    let owner = resources(&cell, count(1_000_000)).unwrap();
    assert_eq!(cell.available_bytes(), 1_000_000 - owner.retained_bytes());
    assert_eq!(owner.buffers.len(), 2);
    assert!(owner.buffers.iter().all(Option::is_some));
    drop(owner);
    assert_eq!(cell.available_bytes(), 1_000_000);
}

#[test]
fn secondary_binding_returns_spare_to_primary_staging_owner() {
    use umts_mac::hsdpa::{MacdPdu, ProcessId};
    use umts_types::{bits::Bits, hsdpa::CarrierId};
    let (mut session, _receiver, _cell) = super::super::dual_tests::prepared_dual();
    let mut primary = session.links[0].scheduling.take().unwrap();
    let rate = session.links[0]
        .rates
        .all_candidates()
        .find(|rate| {
            rate.reference.transport_bits.get().is_multiple_of(8)
                && rate.reference.transport_bits.get() >= 320
        })
        .unwrap();
    session.queues.enqueue_fixture(
        0,
        MacdPdu {
            size_index: 0,
            bits: Bits::new(&[0x5a; 40], 320).unwrap(),
        },
    );
    let buffer = primary.staging_buffers[0].take().unwrap();
    assert!(
        session
            .queues
            .prepare_queue(
                primary
                    .encoders
                    .get_mut(rate.reference.transport_bits)
                    .unwrap(),
                session.links[0].packets.category(),
                0,
                buffer
            )
            .unwrap_or_else(|_| panic!("staging"))
            .is_none()
    );
    session.staged_buffer_carriers[0] = Some(CarrierId::Primary);
    session.links[0].scheduling = Some(primary);
    // Primary deferred with its packet still staged. Secondary binds that same prefix.
    let staged = session.queues.staged_packet(0).unwrap();
    let mut secondary = session.links[1].scheduling.take().unwrap();
    assert!(secondary.staging_buffers[0].is_some());
    assert!(
        session.links[1]
            .packets
            .bind_staged(&mut session.queues, 0, ProcessId::new(0).unwrap())
            .unwrap()
    );
    let spare = secondary.buffers[0].take().unwrap();
    session.return_spare_buffer_on(CarrierId::Secondary, &mut secondary, 0, Some(0), spare);
    assert!(
        session.links[0]
            .scheduling
            .as_ref()
            .unwrap()
            .staging_buffers[0]
            .is_some()
    );
    assert!(secondary.staging_buffers[0].is_some());
    assert!(secondary.buffers[0].is_none());
    assert_eq!(
        session.links[1]
            .packets
            .payload(ProcessId::new(0).unwrap())
            .unwrap()
            .unwrap()
            .packet(),
        staged
    );
    assert!(session.staged_buffer_carriers[0].is_none());
}

#[test]
fn secondary_cancel_returns_buffer_without_overwriting_its_own_staging_slot() {
    use umts_mac::hsdpa::MacdPdu;
    use umts_types::{bits::Bits, hsdpa::CarrierId};
    let (mut session, _receiver, _cell) = super::super::dual_tests::prepared_dual();
    let mut primary = session.links[0].scheduling.take().unwrap();
    let rate = session.links[0]
        .rates
        .all_candidates()
        .find(|rate| {
            rate.reference.transport_bits.get().is_multiple_of(8)
                && rate.reference.transport_bits.get() >= 320
        })
        .unwrap();
    session.queues.enqueue_fixture(
        0,
        MacdPdu {
            size_index: 0,
            bits: Bits::new(&[0x5a; 40], 320).unwrap(),
        },
    );
    let buffer = primary.staging_buffers[0].take().unwrap();
    assert!(
        session
            .queues
            .prepare_queue(
                primary
                    .encoders
                    .get_mut(rate.reference.transport_bits)
                    .unwrap(),
                session.links[0].packets.category(),
                0,
                buffer
            )
            .unwrap_or_else(|_| panic!("staging"))
            .is_none()
    );
    session.staged_buffer_carriers[0] = Some(CarrierId::Primary);
    session.links[0].scheduling = Some(primary);
    let sequence = session.queues.staged_packet(0).unwrap().sequence;
    let mut secondary = session.links[1].scheduling.take().unwrap();
    let buffer = session.queues.cancel_staged(0).unwrap().unwrap();
    session.return_staged_buffer_on(CarrierId::Secondary, &mut secondary, 0, buffer);
    assert!(
        session.links[0]
            .scheduling
            .as_ref()
            .unwrap()
            .staging_buffers[0]
            .is_some()
    );
    assert!(secondary.staging_buffers[0].is_some());
    assert_eq!(session.queues.queued(0), 1);
    let buffer = secondary.staging_buffers[0].take().unwrap();
    assert!(
        session
            .queues
            .prepare_queue(
                secondary
                    .encoders
                    .get_mut(rate.reference.transport_bits)
                    .unwrap(),
                session.links[1].packets.category(),
                0,
                buffer
            )
            .unwrap_or_else(|_| panic!("restaging"))
            .is_none()
    );
    assert_eq!(session.queues.staged_packet(0).unwrap().sequence, sequence);
}

#[test]
fn two_ues_with_one_available_waveform_each_rotate_carriers_independently() {
    let (mut first, _rx1, _cell1) = super::super::dual_tests::prepared_dual();
    let (mut second, _rx2, _cell2) = super::super::dual_tests::prepared_dual();
    let mut admitted = [[0; 2]; 2];
    for _ in 0..4 {
        // With capacity one, only the first visited carrier can acquire a waveform.
        admitted[0][first.next_scheduling_carrier(2)] += 1;
        admitted[1][second.next_scheduling_carrier(2)] += 1;
    }
    assert_eq!(admitted, [[2, 2], [2, 2]]);
}

#[test]
fn padded_mac_ehs_block_keeps_backlog_based_upward_adaptation() {
    use umts_mac::hsdpa::{MacdPdu, PacketQueue, QueueFormat};
    use umts_types::bits::Bits;
    let (mut session, _receiver, _cell) = super::super::dual_tests::prepared_dual();
    let mut resources = session.links[0].scheduling.take().unwrap();
    // 95-byte SDUs leave one byte of padding at the 792-bit CQI8 size:
    // adding the next two-byte SDU header cannot fit despite queued backlog.
    let mut sizes = [None; 8];
    sizes[0] = Some(count(760));
    session.queues = crate::hsdpa_packet::PacketQueues::new(session.identity().ue);
    session
        .queues
        .add_queue(Box::new(
            PacketQueue::new(
                QueueFormat::ehs(0, 1, sizes).unwrap(),
                count(8),
                count(65536),
            )
            .unwrap(),
        ))
        .unwrap_or_else(|_| panic!("replacement test queue"));
    let cqi = Cqi::new(8).unwrap();
    let rate = session.links[0]
        .rates
        .adapted_candidates(cqi, 0)
        .next()
        .unwrap();
    for _ in 0..8 {
        session.queues.enqueue_fixture(
            0,
            MacdPdu {
                size_index: 0,
                bits: Bits::new(&[0x5a; 95], 760).unwrap(),
            },
        );
    }
    let buffer = resources.staging_buffers[0].take().unwrap();
    assert!(
        session
            .queues
            .prepare_queue(
                resources
                    .encoders
                    .get_mut(rate.reference.transport_bits)
                    .unwrap(),
                session.links[0].packets.category(),
                0,
                buffer
            )
            .unwrap_or_else(|_| panic!("staging"))
            .is_none()
    );
    let packet = session.queues.staged_packet(0).unwrap();
    assert!(packet.header_bits + packet.payload_bits < rate.reference.transport_bits.get());
    assert!(session.links[0].rates.rate_limited(
        cqi,
        0,
        rate,
        session.queues.pending_packet_bits(0)
    ));
    assert!(!session.links[0].rates.rate_limited(cqi, 0, rate, 0));
    assert!(!session.links[0].rates.rate_limited(cqi, 0, rate, 320));
}

#[test]
fn two_resource_owners_retain_no_more_than_the_split_ue_budget() {
    let aggregate = 2_000_000;
    let cell = CellSchedulingBudget::new(count(aggregate));
    let first = resources(&cell, count(aggregate / 2)).unwrap();
    let second = resources(&cell, count(aggregate / 2)).unwrap();
    let retained = first.retained_bytes() + second.retained_bytes();
    assert!(retained <= aggregate);
    assert_eq!(cell.available_bytes(), aggregate - retained);
    drop(first);
    drop(second);
    assert_eq!(cell.available_bytes(), aggregate);
}

impl Session {
    pub(crate) fn fixture_seed_partial_release(&mut self) {
        use crate::hsdpa_packet::PacketEncoder;
        use umts_mac::hsdpa::{MacdPdu, ProcessId};
        use umts_types::{bits::Bits, hsdpa::CarrierId};

        let mut encoder =
            PacketEncoder::new(count(352), count(100_000), CrcMethod::Method1).unwrap();
        for carrier in [CarrierId::Primary, CarrierId::Secondary] {
            for queue in 0..2 {
                self.queues.enqueue_fixture(
                    queue,
                    MacdPdu {
                        size_index: 0,
                        bits: Bits::new(&[queue + 1; 40], 320).unwrap(),
                    },
                );
                let buffer = self.links[carrier.index()]
                    .scheduling
                    .as_mut()
                    .unwrap()
                    .buffers[usize::from(queue)]
                .take()
                .unwrap();
                assert!(
                    self.links[carrier.index()]
                        .packets
                        .encode(
                            &mut self.queues,
                            &mut encoder,
                            queue,
                            ProcessId::new(queue).unwrap(),
                            buffer,
                        )
                        .unwrap_or_else(|_| panic!("fixture partial-release payload"))
                        .is_none()
                );
            }
        }
        for queue in 0..2 {
            self.queues.enqueue_fixture(
                queue,
                MacdPdu {
                    size_index: 0,
                    bits: Bits::new(&[queue + 3; 40], 320).unwrap(),
                },
            );
        }
        let owner = CarrierId::Secondary;
        let buffer = self.links[owner.index()]
            .scheduling
            .as_mut()
            .unwrap()
            .staging_buffers[1]
            .take()
            .unwrap();
        assert!(
            self.queues
                .prepare_queue(&mut encoder, self.links[0].packets.category(), 1, buffer)
                .unwrap_or_else(|_| panic!("fixture removed-queue staging"))
                .is_none()
        );
        self.staged_buffer_carriers[1] = Some(owner);
    }

    pub(crate) fn fixture_assert_partial_release(&self) {
        use umts_mac::hsdpa::ProcessId;
        use umts_types::hsdpa::CarrierId;
        assert_eq!(self.queues.queued(1), 0);
        assert_eq!(self.queues.pending_packet_bits(1), 0);
        assert!(self.queues.staged_packet(1).is_none());
        assert_eq!(self.staged_buffer_carriers[1], None);
        assert!(
            self.links[CarrierId::Secondary.index()]
                .scheduling
                .as_ref()
                .unwrap()
                .staging_buffers[1]
                .is_some()
        );
        assert!(self.queues.queued(0) > 0);
        for (index, link) in self.links.iter().enumerate() {
            assert!(
                link.packets
                    .payload(ProcessId::new(1).unwrap())
                    .unwrap()
                    .is_none()
            );
            assert!(link.recycled_blocks[1].is_some());
            assert_eq!(
                link.packets
                    .payload(ProcessId::new(0).unwrap())
                    .unwrap()
                    .unwrap()
                    .packet()
                    .sequence,
                index as u8
            );
        }
    }
}

#[test]
fn new_payload_refreshes_its_floor_in_the_same_scheduler_visit() {
    use crate::test_support::at;
    use umts_types::{hsdpa::CarrierId, time::CHIPS_PER_SLOT};
    for advance in [false, true] {
        Session::with_active_dual_fixture(|fixture| {
            fixture.session.fixture_enqueue_pdu(&[0x5a; 40], 320);
            let mut calendar =
                AllocationCalendar::new(fixture.codes[0], fixture.time.now, count(16)).unwrap();
            let mut preparer = WaveformPreparer::new(
                fixture.codes[0],
                fixture.source.information().primary_code(),
            )
            .unwrap();
            let floor = if advance {
                at(fixture.time.not_before.index + 3 * CHIPS_PER_SLOT)
            } else {
                fixture.time.not_before
            };
            let cursor = if advance {
                at(floor.index - crate::hsdpa_tx::CONTROL_LEAD_CHIPS)
            } else {
                at(0)
            };
            let mut calls = 0;
            assert!(matches!(
                fixture.session.schedule_packet_on(
                    CarrierId::Primary,
                    &mut calendar,
                    &mut preparer,
                    fixture.time,
                    WaveformBuffer::default(),
                    || {
                        calls += 1;
                        cursor
                    },
                ),
                SchedulingOutcome::Prepared
            ));
            assert_eq!(calls, 1);
            let tx = fixture
                .session
                .pending_transmission_on(CarrierId::Primary)
                .unwrap();
            assert_eq!(tx.downlink, floor);
            assert_eq!(
                fixture
                    .session
                    .fixture_packet(CarrierId::Primary, tx.key.process())
                    .unwrap()
                    .sequence,
                0
            );
            assert!(fixture.session.queues.staged_packet(0).is_none());
            assert_eq!(
                fixture.session.links[0]
                    .feedback_snapshot
                    .selection
                    .selected,
                1
            );
        });
    }
}

#[test]
fn invalid_post_encoding_cursor_retains_staging_before_harq_binding() {
    use crate::test_support::at;
    use umts_dsp::scrambling::PrimaryScramblingCode;
    use umts_types::{hsdpa::CarrierId, time::CHIPS_PER_SLOT};

    for overflow in [false, true] {
        for consumer in [CarrierId::Primary, CarrierId::Secondary] {
            Session::with_active_dual_fixture(|fixture| {
                fixture.session.fixture_enqueue_pdu(&[0x5a; 40], 320);
                let mut primary_calendar =
                    AllocationCalendar::new(fixture.codes[0], fixture.time.now, count(16)).unwrap();
                let mut primary_preparer = WaveformPreparer::new(
                    fixture.codes[0],
                    fixture.source.information().primary_code(),
                )
                .unwrap();
                assert!(matches!(
                    fixture.session.schedule_packet_on(
                        CarrierId::Primary,
                        &mut primary_calendar,
                        &mut primary_preparer,
                        fixture.time,
                        WaveformBuffer::default(),
                        || if overflow {
                            at(u64::MAX)
                        } else {
                            ChipInstant {
                                epoch: umts_types::RadioEpoch::new(
                                    NonZeroU64::new(fixture.time.now.epoch.get() + 1).unwrap(),
                                ),
                                ..fixture.time.now
                            }
                        },
                    ),
                    SchedulingOutcome::Failed {
                        error: DispatchError::Configuration,
                        ..
                    }
                ));
                let staged = fixture.session.queues.staged_packet(0).unwrap();
                assert_eq!(
                    fixture.session.staged_buffer_carriers[0],
                    Some(CarrierId::Primary)
                );
                for link in &fixture.session.links {
                    assert!(link.pending.is_none());
                    assert_eq!(link.feedback_snapshot.selection.selected, 0);
                    assert_eq!(
                        link.feedback_snapshot.formats.airtime_summary().lead_count,
                        0
                    );
                    for index in 0..link.queue_commands.len() {
                        assert_eq!(
                            link.packets.process_state(
                                umts_mac::hsdpa::ProcessId::new(index as u8).unwrap()
                            ),
                            Ok(umts_mac::hsdpa::ProcessState::Empty)
                        );
                    }
                }
                let fresh_floor = at(fixture.time.not_before.index + 3 * CHIPS_PER_SLOT);
                let mut calendar = AllocationCalendar::new(
                    fixture.codes[consumer.index()],
                    fixture.time.now,
                    count(16),
                )
                .unwrap();
                let code = if consumer == CarrierId::Primary {
                    fixture.source.information().primary_code()
                } else {
                    PrimaryScramblingCode::new(470).unwrap()
                };
                let mut preparer =
                    WaveformPreparer::new(fixture.codes[consumer.index()], code).unwrap();
                assert!(matches!(
                    fixture.session.schedule_packet_on(
                        consumer,
                        &mut calendar,
                        &mut preparer,
                        SchedulingTime {
                            not_before: fresh_floor,
                            ..fixture.time
                        },
                        WaveformBuffer::default(),
                        || panic!("retained staging does not re-encode"),
                    ),
                    SchedulingOutcome::Prepared
                ));
                let transmission = fixture.session.pending_transmission_on(consumer).unwrap();
                assert!(transmission.downlink.index >= fresh_floor.index);
                let payload = fixture.session.links[consumer.index()]
                    .packets
                    .payload(transmission.key.process())
                    .unwrap()
                    .unwrap();
                assert_eq!(payload.packet(), staged);
                assert!(fixture.session.queues.staged_packet(0).is_none());
                assert!(fixture.session.staged_buffer_carriers[0].is_none());
                assert!(
                    fixture.session.links[0]
                        .scheduling
                        .as_ref()
                        .unwrap()
                        .staging_buffers[0]
                        .is_some()
                );
            });
        }
    }
}

impl Session {
    pub(crate) fn fixture_encode_ready_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        now: ChipInstant,
    ) {
        let mut resources = self.links[carrier.index()].scheduling.take().unwrap();
        assert!(
            self.stage_one_payload_on(carrier, &mut resources, now)
                .unwrap()
        );
        let process = umts_mac::hsdpa::ProcessId::new(0).unwrap();
        assert!(
            self.links[carrier.index()]
                .packets
                .bind_staged(&mut self.queues, 0, process,)
                .unwrap()
        );
        let spare = resources.buffers[0].take().unwrap();
        self.return_spare_buffer_on(carrier, &mut resources, 0, Some(0), spare);
        self.links[carrier.index()].scheduling = Some(resources);
    }
}
