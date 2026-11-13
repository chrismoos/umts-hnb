// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_tx::{DedicatedTransmitPorts, DedicatedTransmitRuntime},
    hsdpa_packet::control::Commands,
    hsdpa_rx::{CarrierAcknowledgements, CarrierCqi, Evidence, Measurement, Observation, Request},
    test_support::{at, count},
};
use num_complex::Complex32;
use umts_dsp::{
    hsdpa::{CellAck, CompositeCqi, Cqi, DualHarqAck, FeedbackEvidence},
    scrambling::PrimaryScramblingCode,
};
use umts_mac::hsdpa::{ProcessId, ProcessState};
use umts_types::{hsdpa::CarrierId, time::CHIPS_PER_SLOT};

fn stage(session: &mut Session, carrier: CarrierId, now: ChipInstant) -> bool {
    let mut resources = session.links[carrier.index()].scheduling.take().unwrap();
    let encoded = session
        .stage_one_payload_on(carrier, &mut resources, now)
        .unwrap();
    session.links[carrier.index()].scheduling = Some(resources);
    encoded
}

fn assert_staging_owner(session: &Session, owner: Option<CarrierId>) {
    assert_eq!(session.staged_buffer_carriers[0], owner);
    for carrier in [CarrierId::Primary, CarrierId::Secondary] {
        assert_eq!(
            session.links[carrier.index()]
                .scheduling
                .as_ref()
                .unwrap()
                .staging_buffers[0]
                .is_some(),
            owner != Some(carrier)
        );
    }
}

fn evidence<T>(value: T) -> Evidence<T> {
    Evidence::Measured(Observation {
        evidence: FeedbackEvidence {
            candidate: Some(value),
            normalized_correlation: 1.0,
            normalized_margin: 1.0,
            mean_soft_power: 1.0,
        },
        minimum_pilot_fraction: 1.0,
        mean_relative_pilot_disturbance: 0.0,
    })
}

fn service_commands(
    session: &mut Session,
    runtime: &mut DedicatedTransmitRuntime,
    ports: &mut DedicatedTransmitPorts,
    commands: &mut Commands,
) -> usize {
    runtime.service().unwrap();
    while let Some(event) = ports.control.try_event() {
        assert!(commands.route(event).is_none());
    }
    let mut recycled = 0;
    while let Some(result) = commands.port(&mut ports.control).take_result() {
        let carrier = session
            .carrier_for_command(&result)
            .unwrap_or(CarrierId::Primary);
        if let CommandProgress::Recycled {
            buffer, rejection, ..
        } = session
            .accept_command_on(carrier, result)
            .unwrap_or_else(|error| panic!("{:?}", error.error))
        {
            assert!(rejection.is_none());
            drop(buffer);
            recycled += 1;
        }
    }
    recycled
}

#[test]
fn staged_prefix_survives_backlog_growth_on_either_carrier() {
    for consumer in [CarrierId::Primary, CarrierId::Secondary] {
        Session::with_active_dual_fixture(|fixture| {
            fixture.session.fixture_enqueue_pdu(&[0x10; 40], 320);
            assert!(stage(fixture.session, CarrierId::Primary, fixture.time.now));
            let staged = fixture.session.queues.staged_packet(0).unwrap();
            assert_eq!((staged.sequence, staged.pdus), (0, 1));
            assert_staging_owner(fixture.session, Some(CarrierId::Primary));
            for value in 0x11..=0x17 {
                fixture.session.fixture_enqueue_pdu(&[value; 40], 320);
            }
            assert_eq!(fixture.session.queues.queued(0), 8);
            let encoded_before = [CarrierId::Primary, CarrierId::Secondary].map(|carrier| {
                fixture.session.links[carrier.index()]
                    .feedback_snapshot
                    .staging
                    .calls
            });
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
                    fixture.time,
                    WaveformBuffer::default(),
                    || panic!("an admissible staged prefix must not be re-encoded"),
                ),
                SchedulingOutcome::Prepared
            ));
            let first = fixture.session.pending_transmission_on(consumer).unwrap();
            assert_eq!(
                fixture
                    .session
                    .fixture_packet(consumer, first.key.process()),
                Some(staged)
            );
            assert_eq!(fixture.session.queues.queued(0), 7);
            assert!(fixture.session.queues.staged_packet(0).is_none());
            assert_staging_owner(fixture.session, None);
            assert_eq!(
                [CarrierId::Primary, CarrierId::Secondary].map(|carrier| {
                    fixture.session.links[carrier.index()]
                        .feedback_snapshot
                        .staging
                        .calls
                }),
                encoded_before
            );

            let mut commands = Commands::new(count(16)).unwrap();
            assert!(matches!(
                fixture
                    .session
                    .publish_packet_on(
                        consumer,
                        &mut commands.port(&mut fixture.ports.control),
                        &mut calendar,
                    )
                    .unwrap(),
                PacketPublication::Queued { .. }
            ));
            let next_time = SchedulingTime {
                not_before: at(first.downlink.index + 3 * CHIPS_PER_SLOT),
                ..fixture.time
            };
            let mut encodings = 0;
            assert!(matches!(
                fixture.session.schedule_packet_on(
                    consumer,
                    &mut calendar,
                    &mut preparer,
                    next_time,
                    WaveformBuffer::default(),
                    || {
                        encodings += 1;
                        at(next_time.not_before.index - crate::hsdpa_tx::CONTROL_LEAD_CHIPS)
                    },
                ),
                SchedulingOutcome::Prepared
            ));
            assert_eq!(encodings, 1);
            let next = fixture.session.pending_transmission_on(consumer).unwrap();
            assert_ne!(next.key.process(), first.key.process());
            let grown = fixture
                .session
                .fixture_packet(consumer, next.key.process())
                .unwrap();
            assert_eq!(grown.sequence, staged.sequence + 1);
            assert!(grown.transport_bits > staged.transport_bits);
            assert!(grown.pdus > staged.pdus);
            assert_eq!(
                fixture.session.queues.queued(0),
                7 - grown.pdus + usize::from(grown.end_fragment_octets != 0)
            );
            assert_eq!(
                fixture
                    .session
                    .fixture_packet(consumer, first.key.process()),
                Some(staged)
            );
            assert!(fixture.session.queues.staged_packet(0).is_none());
            assert_staging_owner(fixture.session, None);
            let resources = fixture.session.links[consumer.index()]
                .scheduling
                .as_ref()
                .unwrap();
            assert!(resources.buffers[usize::from(first.key.process().value())].is_none());
            assert!(resources.buffers[usize::from(next.key.process().value())].is_none());
        });
    }
}

#[test]
fn busy_lower_cqi_carrier_preserves_sibling_stage_until_a_process_is_released() {
    Session::with_active_dual_fixture(|fixture| {
        assert_eq!(
            fixture
                .session
                .request_periodic_cqi(
                    fixture.time.feedback,
                    fixture.time.not_before,
                    fixture.time.not_before,
                    count(1),
                )
                .unwrap()
                .requested,
            1
        );
        let report = fixture
            .receiver
            .fixture_answer_next(Measurement::CqiCells(evidence(CarrierCqi::Dual(
                CompositeCqi {
                    primary: Cqi::new(8).unwrap(),
                    secondary: Cqi::new(6).unwrap(),
                },
            ))));
        let Request::CqiCells { start, .. } = report.request else {
            panic!("dual CQI request");
        };
        assert!(
            fixture
                .session
                .poll_classified_feedback(&fixture.policy)
                .is_some()
        );
        let now = at(start.index + CHIPS_PER_SLOT);
        let mut time = SchedulingTime {
            now,
            not_before: fixture
                .time
                .feedback
                .next_pdsch(at(now.index + crate::hsdpa_tx::CONTROL_LEAD_CHIPS))
                .unwrap(),
            ..fixture.time
        };
        assert_eq!(
            fixture.session.latest_cqi_on(
                CarrierId::Secondary,
                now,
                NonZeroU64::new(1_000_000).unwrap()
            ),
            Some(Cqi::new(6).unwrap())
        );
        let lower_maximum = fixture.session.links[1]
            .rates
            .adapted_candidates(Cqi::new(6).unwrap(), 0)
            .map(|rate| rate.reference.transport_bits.get())
            .max()
            .unwrap();
        let mut calendar = AllocationCalendar::new(fixture.codes[1], time.now, count(16)).unwrap();
        let mut preparer =
            WaveformPreparer::new(fixture.codes[1], PrimaryScramblingCode::new(470).unwrap())
                .unwrap();
        let mut commands = Commands::new(count(16)).unwrap();
        let mut writer = fixture.source.submission_writer().unwrap();
        let mut samples = vec![Complex32::default(); fixture.source.samples_per_slot()];
        let processes = fixture.session.links[1].queue_commands.len();
        assert_eq!(processes, 6);
        let mut transmissions = Vec::with_capacity(processes);
        for index in 0..processes {
            fixture.session.fixture_enqueue_pdu(&[index as u8; 40], 320);
            calendar.advance(time.now).unwrap();
            assert!(matches!(
                fixture.session.schedule_packet_on(
                    CarrierId::Secondary,
                    &mut calendar,
                    &mut preparer,
                    time,
                    WaveformBuffer::default(),
                    || at(time.not_before.index - crate::hsdpa_tx::CONTROL_LEAD_CHIPS),
                ),
                SchedulingOutcome::Prepared
            ));
            let transmission = fixture
                .session
                .pending_transmission_on(CarrierId::Secondary)
                .unwrap();
            let packet = fixture
                .session
                .fixture_packet(CarrierId::Secondary, transmission.key.process())
                .unwrap();
            assert_eq!((packet.sequence, packet.pdus), (index as u8, 1));
            assert_eq!(fixture.session.queues.queued(0), 0);
            transmissions.push(transmission);
            assert!(matches!(
                fixture
                    .session
                    .publish_packet_on(
                        CarrierId::Secondary,
                        &mut commands.port(&mut fixture.ports.control),
                        &mut calendar,
                    )
                    .unwrap(),
                PacketPublication::Queued { .. }
            ));
            assert_eq!(
                service_commands(
                    fixture.session,
                    fixture.runtime,
                    fixture.ports,
                    &mut commands
                ),
                0
            );
            let through = transmission.expected_ack.index + 3 * CHIPS_PER_SLOT;
            for _ in 0..512 {
                if fixture.source.next_chip().index >= through {
                    break;
                }
                assert!(
                    writer.accepted(
                        fixture
                            .runtime
                            .generate_slot(fixture.source, &mut samples)
                            .unwrap()
                    )
                );
            }
            assert!(fixture.source.next_chip().index >= through);
            let mut recycled = 0;
            for _ in 0..4 {
                commands
                    .port(&mut fixture.ports.control)
                    .request_completion()
                    .unwrap();
                recycled += service_commands(
                    fixture.session,
                    fixture.runtime,
                    fixture.ports,
                    &mut commands,
                );
            }
            assert_eq!(recycled, 1);
            time.now = fixture.source.next_chip();
            time.not_before = time
                .feedback
                .next_pdsch(at(time.now.index + crate::hsdpa_tx::CONTROL_LEAD_CHIPS))
                .unwrap();
        }
        assert!(
            fixture.session.links[1]
                .queue_commands
                .iter()
                .all(Option::is_none)
        );
        for index in 0..processes {
            assert_eq!(
                fixture.session.links[1]
                    .packets
                    .process_state(ProcessId::new(index as u8).unwrap()),
                Ok(ProcessState::Busy)
            );
        }
        for value in 0x20..=0x27 {
            fixture.session.fixture_enqueue_pdu(&[value; 40], 320);
        }
        assert!(stage(fixture.session, CarrierId::Primary, time.now));
        let original = fixture.session.queues.staged_packet(0).unwrap();
        assert_eq!(original.sequence, processes as u8);
        assert!(original.transport_bits > lower_maximum);
        assert_staging_owner(fixture.session, Some(CarrierId::Primary));
        let encoded_before = fixture.session.links[1].feedback_snapshot.staging.calls;
        calendar.advance(time.now).unwrap();
        for _ in 0..2 {
            assert!(matches!(
                fixture.session.schedule_packet_on(
                    CarrierId::Secondary,
                    &mut calendar,
                    &mut preparer,
                    time,
                    WaveformBuffer::default(),
                    || panic!("a carrier with no empty process must leave sibling staging alone"),
                ),
                SchedulingOutcome::Deferred {
                    budget_exhausted: false,
                    ..
                }
            ));
            assert_eq!(fixture.session.queues.staged_packet(0), Some(original));
            assert_eq!(fixture.session.queues.queued(0), 8);
            assert_staging_owner(fixture.session, Some(CarrierId::Primary));
            assert_eq!(
                fixture.session.links[1].feedback_snapshot.staging.calls,
                encoded_before
            );
        }

        fixture
            .receiver
            .fixture_answer_next(Measurement::AckCells(evidence(
                CarrierAcknowledgements::Dual(DualHarqAck::Both {
                    primary: CellAck::Ack,
                    secondary: CellAck::Ack,
                }),
            )));
        let admitted = fixture
            .session
            .poll_classified_feedback(&fixture.policy)
            .unwrap();
        assert!(admitted.admission.unwrap().is_ok());
        assert_eq!(
            admitted.admitted_transmissions[1].unwrap().key,
            transmissions[0].key
        );
        assert_eq!(
            fixture
                .session
                .resolve_packets(count(16))
                .unwrap()
                .delivered,
            1
        );
        assert_eq!(
            fixture.session.links[1]
                .packets
                .process_state(transmissions[0].key.process()),
            Ok(ProcessState::Empty)
        );

        assert!(stage(fixture.session, CarrierId::Secondary, time.now));
        let restaged = fixture.session.queues.staged_packet(0).unwrap();
        assert_eq!(restaged.sequence, original.sequence);
        assert!(restaged.transport_bits < original.transport_bits);
        assert!(restaged.transport_bits <= lower_maximum);
        assert_eq!(fixture.session.queues.queued(0), 8);
        assert_staging_owner(fixture.session, Some(CarrierId::Secondary));
        assert_eq!(
            fixture.session.links[1].feedback_snapshot.staging.calls,
            encoded_before + 1
        );
        assert!(matches!(
            fixture.session.schedule_packet_on(
                CarrierId::Secondary,
                &mut calendar,
                &mut preparer,
                time,
                WaveformBuffer::default(),
                || panic!("the replacement prefix is already encoded"),
            ),
            SchedulingOutcome::Prepared
        ));
        let transmission = fixture
            .session
            .pending_transmission_on(CarrierId::Secondary)
            .unwrap();
        assert_eq!(transmission.key.process(), transmissions[0].key.process());
        assert_eq!(
            fixture
                .session
                .fixture_packet(CarrierId::Secondary, transmission.key.process()),
            Some(restaged)
        );
        assert_eq!(
            fixture.session.queues.queued(0),
            8 - restaged.pdus + usize::from(restaged.end_fragment_octets != 0)
        );
        assert!(fixture.session.queues.staged_packet(0).is_none());
        assert_staging_owner(fixture.session, None);
    });
}
