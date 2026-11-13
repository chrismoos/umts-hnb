// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    hsdpa_packet::{PacketEncoder, TransmitRequest},
    hsdpa_rx::{CarrierAcknowledgements, CarrierCqi, FeedbackMode, FeedbackModeSchedule},
    hsdpa_tx::{Amplitudes, CompletedTransmission, WaveformBuffer, WaveformPreparer},
    test_support::{at, count},
};
use umts_dsp::{
    hsdpa::{
        CellAck, CompositeCqi, CrcMethod, DualHarqAck, FeedbackEvidence, ScchEncoder,
        TransmissionPlan,
    },
    scrambling::PrimaryScramblingCode,
};
use umts_mac::hsdpa::{ProcessId, ProcessState, ScheduledTransmission};
use umts_types::{hsdpa::CarrierId, time::CHIPS_PER_SLOT};

fn submitted(session: &mut Session, carrier: CarrierId, process: u8) -> ScheduledTransmission {
    let link = &mut session.links[carrier.index()];
    // The abstract session fixture uses origin zero. Actual waveforms require
    // PDSCH two slots after the SCCH TTI boundary.
    let parameters = session.procedure.construction().parameters();
    link.packets = crate::hsdpa_packet::PacketOwner::new(
        session.identity.ue,
        link.incarnation,
        parameters.harq.category(),
        at(crate::hsdpa_tx::CONTROL_LEAD_CHIPS),
        parameters.harq.process_memory(),
        parameters.acknowledgement.repetition(),
    )
    .unwrap();
    let downlink = link
        .packets
        .next_downlink(session.procedure.construction().activation().time())
        .unwrap();
    submitted_on_existing_owner(
        session,
        carrier,
        process,
        downlink,
        at(downlink.index + 30_000),
        PrimaryScramblingCode::new(0).unwrap(),
    )
}

fn submitted_on_existing_owner(
    session: &mut Session,
    carrier: CarrierId,
    process: u8,
    downlink: ChipInstant,
    expected_ack: ChipInstant,
    code: PrimaryScramblingCode,
) -> ScheduledTransmission {
    session.fixture_enqueue_pdu(&[0x5a; 40], 320);
    let link = &mut session.links[carrier.index()];
    let process = ProcessId::new(process).unwrap();
    let pool = link.rates.codes();
    let rate = link
        .rates
        .all_candidates()
        .find(|rate| {
            rate.reference.transport_bits.get().is_multiple_of(8)
                && rate.reference.transport_bits.get() >= 400
        })
        .unwrap();
    let mut encoder = PacketEncoder::new(
        rate.reference.transport_bits,
        count(100_000),
        CrcMethod::Method1,
    )
    .unwrap();
    let buffer = encoder.buffer();
    assert!(
        link.packets
            .encode(&mut session.queues, &mut encoder, 0, process, buffer)
            .unwrap_or_else(|_| panic!("encode fixture packet"))
            .is_none()
    );
    let mut plan = TransmissionPlan::new(
        rate.reference
            .assignment(pool.first_data(), process.value(), true)
            .unwrap(),
        link.packets.category(),
        &ScchEncoder::new(0x8421).unwrap(),
        link.packets.soft_bits(process).unwrap(),
        count(100_000),
    )
    .unwrap();
    let mut preparer = WaveformPreparer::new(pool, code).unwrap();
    let prepared = link
        .packets
        .prepare_waveform(
            &mut preparer,
            TransmitRequest {
                process,
                downlink,
                expected_ack,
                plan: &mut plan,
                control_code: pool.control_codes().next().unwrap().index(),
                amplitudes: Amplitudes {
                    control: 0.1,
                    data: 0.1,
                },
            },
            WaveformBuffer::default(),
        )
        .unwrap_or_else(|error| panic!("{:?}", error.error));
    let tx = prepared.transmission();
    link.packets
        .complete_transmit(CompletedTransmission {
            transmission: prepared,
            submitted: true,
        })
        .unwrap_or_else(|_| panic!("fixture transmit completion"));
    tx
}

fn measured(word: DualHarqAck) -> Measurement {
    Measurement::AckCells(Evidence::Measured(crate::hsdpa_rx::Observation {
        evidence: FeedbackEvidence {
            candidate: Some(CarrierAcknowledgements::Dual(word)),
            normalized_correlation: 1.0,
            normalized_margin: 1.0,
            mean_soft_power: 1.0,
        },
        minimum_pilot_fraction: 1.0,
        mean_relative_pilot_disturbance: 0.0,
    }))
}

fn measured_cqi(primary: u8, secondary: u8) -> Measurement {
    Measurement::CqiCells(Evidence::Measured(crate::hsdpa_rx::Observation {
        evidence: FeedbackEvidence {
            candidate: Some(CarrierCqi::Dual(CompositeCqi {
                primary: Cqi::new(primary).unwrap(),
                secondary: Cqi::new(secondary).unwrap(),
            })),
            normalized_correlation: 1.0,
            normalized_margin: 1.0,
            mean_soft_power: 1.0,
        },
        minimum_pilot_fraction: 1.0,
        mean_relative_pilot_disturbance: 0.0,
    }))
}

#[test]
fn full_feedback_service_drains_paired_acks_and_cqi_with_a_bounded_tail() {
    for cqi_first in [true, false] {
        Session::with_active_dual_fixture(|fixture| {
            let downlink = fixture.time.not_before;
            let expected_ack = fixture.time.feedback.acknowledgement(downlink).unwrap();
            let transmissions = [
                submitted_on_existing_owner(
                    fixture.session,
                    CarrierId::Primary,
                    4,
                    downlink,
                    expected_ack,
                    fixture.source.information().primary_code(),
                ),
                submitted_on_existing_owner(
                    fixture.session,
                    CarrierId::Secondary,
                    5,
                    downlink,
                    expected_ack,
                    PrimaryScramblingCode::new(470).unwrap(),
                ),
            ];
            let schedule = fixture.session.feedback_mode.unwrap();
            let cqi_request = Request::CqiCells {
                start: expected_ack,
                repetitions: 1,
                schedule,
            };
            if cqi_first {
                fixture.session.feedback.request(cqi_request).unwrap();
            }
            for (index, transmission) in transmissions.iter().enumerate() {
                fixture.session.links[index]
                    .packets
                    .request_feedback_cells(
                        transmission.key.process(),
                        &mut fixture.session.feedback,
                        schedule,
                    )
                    .unwrap();
            }
            if !cqi_first {
                fixture.session.feedback.request(cqi_request).unwrap();
            }
            let tail_id = fixture
                .session
                .feedback
                .request(Request::CqiCells {
                    start: at(expected_ack.index + 3 * CHIPS_PER_SLOT),
                    repetitions: 1,
                    schedule,
                })
                .unwrap();
            let acknowledgement = measured(DualHarqAck::Both {
                primary: CellAck::Ack,
                secondary: CellAck::Ack,
            });
            let sequence = if cqi_first {
                [measured_cqi(7, 9), acknowledgement, acknowledgement]
            } else {
                [acknowledgement, acknowledgement, measured_cqi(7, 9)]
            };
            let reports =
                sequence.map(|measurement| fixture.receiver.fixture_answer_next(measurement));
            let primary_report = reports[usize::from(cqi_first)];
            let secondary_report = reports[usize::from(cqi_first) + 1];
            let tail = fixture.receiver.fixture_answer_next(measured_cqi(10, 11));
            assert_eq!(tail.id, tail_id);
            let now = at(expected_ack.index + 4 * CHIPS_PER_SLOT);
            let horizon = at(now.index + 3 * CHIPS_PER_SLOT);

            let step = fixture
                .session
                .service_feedback(fixture.bank, now, horizon)
                .unwrap();
            assert_eq!(
                (step.reports, step.delivered, step.retry, step.failures),
                (3, 2, 0, 0)
            );
            assert_eq!(step.not_transmitted, 0);
            assert!(step.requested <= 2);
            assert!(!step.backpressure);
            for (index, transmission) in transmissions.iter().enumerate() {
                assert_eq!(
                    fixture.session.links[index]
                        .packets
                        .process_state(transmission.key.process()),
                    Ok(ProcessState::Empty)
                );
                assert!(
                    fixture.session.links[index].recycled_blocks
                        [usize::from(transmission.key.process().value())]
                    .is_some()
                );
                assert!(
                    !fixture.session.links[index]
                        .packets
                        .owns_transmission(transmission.key)
                );
            }
            for (carrier, accepted, deferred) in
                [(CarrierId::Primary, 7, 10), (CarrierId::Secondary, 9, 11)]
            {
                let counts = fixture.session.feedback_snapshot_on(carrier).cqi_reports;
                assert_eq!(counts[accepted], 1);
                assert_eq!(counts[deferred], 0);
            }
            assert!(
                fixture
                    .session
                    .feedback
                    .pending_requests()
                    .any(|(id, _)| id == tail_id)
            );

            // Old feedback must not release a later transmission on the same HARQ process.
            drop(
                fixture
                    .session
                    .take_coded_buffer_on(CarrierId::Secondary, transmissions[1].key.process())
                    .unwrap(),
            );
            let next_downlink = fixture
                .time
                .feedback
                .next_pdsch(at((downlink.index + 18 * CHIPS_PER_SLOT)
                    .max(now.index + crate::hsdpa_tx::CONTROL_LEAD_CHIPS)))
                .unwrap();
            let successor = submitted_on_existing_owner(
                fixture.session,
                CarrierId::Secondary,
                5,
                next_downlink,
                fixture
                    .time
                    .feedback
                    .acknowledgement(next_downlink)
                    .unwrap(),
                PrimaryScramblingCode::new(470).unwrap(),
            );
            assert_ne!(successor.key, transmissions[1].key);
            fixture.session.links[1]
                .packets
                .request_feedback_cells(
                    successor.key.process(),
                    &mut fixture.session.feedback,
                    schedule,
                )
                .unwrap();
            let retained = fixture
                .session
                .fixture_packet(CarrierId::Secondary, successor.key.process())
                .unwrap();
            assert!(
                fixture
                    .session
                    .fixture_rejects_stale_feedback(&primary_report)
            );
            assert!(
                fixture
                    .session
                    .fixture_rejects_stale_feedback(&secondary_report)
            );
            assert!(
                fixture.session.links[1]
                    .packets
                    .owns_transmission(successor.key)
            );
            assert_eq!(
                fixture.session.links[1]
                    .packets
                    .process_state(successor.key.process()),
                Ok(ProcessState::Busy)
            );
            assert!(fixture.session.links[1].recycled_blocks[5].is_none());

            let next = fixture
                .session
                .service_feedback(fixture.bank, now, horizon)
                .unwrap();
            assert_eq!(
                (next.reports, next.delivered, next.retry, next.failures),
                (1, 0, 0, 0)
            );
            assert_eq!(next.requested, 0);
            assert!(
                !fixture
                    .session
                    .feedback
                    .pending_requests()
                    .any(|(id, _)| id == tail_id)
            );
            assert_eq!(
                fixture
                    .session
                    .feedback_snapshot_on(CarrierId::Primary)
                    .cqi_reports[10],
                1
            );
            assert_eq!(
                fixture
                    .session
                    .feedback_snapshot_on(CarrierId::Secondary)
                    .cqi_reports[11],
                1
            );
            assert_eq!(
                fixture
                    .session
                    .fixture_packet(CarrierId::Secondary, successor.key.process()),
                Some(retained)
            );
            assert_eq!(
                fixture.session.links[1]
                    .packets
                    .process_state(successor.key.process()),
                Ok(ProcessState::Busy)
            );
        });
    }
}

#[test]
fn admitted_dual_reports_reuse_exact_processes_without_round_robin_delay() {
    for (word, expected) in [
        (
            DualHarqAck::Both {
                primary: CellAck::Ack,
                secondary: CellAck::Ack,
            },
            [ProcessState::Empty; 2],
        ),
        (
            DualHarqAck::Both {
                primary: CellAck::Nack,
                secondary: CellAck::Ack,
            },
            [ProcessState::Ready, ProcessState::Empty],
        ),
        (
            DualHarqAck::Primary(CellAck::Ack),
            [ProcessState::Empty, ProcessState::Ready],
        ),
    ] {
        let (mut session, mut receiver, _cell) = super::super::dual_tests::prepared_dual();
        // Different process numbers catch accidental primary-process reuse on the secondary.
        let transmissions = [
            submitted(&mut session, CarrierId::Primary, 4),
            submitted(&mut session, CarrierId::Secondary, 5),
        ];
        let schedule =
            FeedbackModeSchedule::fixed(transmissions[0].expected_ack, FeedbackMode::Dual);
        session.feedback_mode = Some(schedule);
        for (index, tx) in transmissions.iter().enumerate() {
            session.links[index]
                .packets
                .request_feedback_cells(tx.key.process(), &mut session.feedback, schedule)
                .unwrap();
        }
        for (index, tx) in transmissions.iter().enumerate() {
            let report = receiver.fixture_answer_next(measured(word));
            let step = session.service_feedback_reports(count(1)).unwrap();
            assert_eq!(step.reports, 1);
            assert_eq!(step.failures, 0);
            assert_eq!(
                step.delivered,
                usize::from(expected[index] == ProcessState::Empty)
            );
            assert_eq!(
                step.retry,
                usize::from(expected[index] == ProcessState::Ready)
            );
            assert_eq!(
                session.links[index]
                    .packets
                    .process_state(tx.key.process())
                    .unwrap(),
                expected[index]
            );
            assert!(!session.links[index].packets.owns_transmission(tx.key));
            assert_eq!(
                session.links[index].recycled_blocks[usize::from(tx.key.process().value())]
                    .is_some(),
                expected[index] == ProcessState::Empty
            );
            if index == 0 {
                assert_eq!(
                    session.links[1]
                        .packets
                        .process_state(transmissions[1].key.process())
                        .unwrap(),
                    ProcessState::Busy
                );
            }
            // Duplicate request identity no longer associates with any retained flight.
            assert!(session.fixture_rejects_stale_feedback(&report));
        }
        assert_eq!(session.resolution_link_cursor, 0);
        assert!(session.links.iter().all(|link| link.resolution_cursor == 0));
    }
}

#[test]
fn rejected_or_stale_dual_evidence_cannot_release_sibling_process() {
    for stale_schedule in [false, true] {
        let (mut session, mut receiver, _cell) = super::super::dual_tests::prepared_dual();
        let primary = submitted(&mut session, CarrierId::Primary, 4);
        let secondary = submitted(&mut session, CarrierId::Secondary, 5);
        let schedule = FeedbackModeSchedule::fixed(primary.expected_ack, FeedbackMode::Dual);
        session.feedback_mode = Some(schedule);
        for (index, tx) in [primary, secondary].iter().enumerate() {
            session.links[index]
                .packets
                .request_feedback_cells(tx.key.process(), &mut session.feedback, schedule)
                .unwrap();
        }
        receiver.fixture_answer_next(measured(DualHarqAck::Both {
            primary: CellAck::Ack,
            secondary: CellAck::Ack,
        }));
        let mut admitted = session.poll_feedback(|_| Feedback::Dtx).unwrap();
        // Aggregate failure must never hide a successful exact carrier association.
        // Requests currently have distinct IDs, but the resolution contract also
        // supports future composite association with mixed per-carrier results.
        admitted.admission = Some(Err(FeedbackError::Request));
        let mut resolved = ResolutionStep::default();
        session
            .resolve_admitted_feedback(&admitted, &mut resolved)
            .unwrap();
        assert_eq!(resolved.delivered, 1);
        let evidence = if stale_schedule {
            session.feedback_mode = Some(FeedbackModeSchedule::fixed(
                primary.expected_ack,
                FeedbackMode::Single,
            ));
            measured(DualHarqAck::Both {
                primary: CellAck::Ack,
                secondary: CellAck::Ack,
            })
        } else {
            Measurement::Cqi(Evidence::Missing)
        };
        receiver.fixture_answer_next(evidence);
        let step = session.service_feedback_reports(count(1)).unwrap();
        assert_eq!((step.failures, step.delivered, step.retry), (1, 0, 0));
        assert_eq!(
            session.links[0]
                .packets
                .process_state(primary.key.process())
                .unwrap(),
            ProcessState::Empty
        );
        assert_eq!(
            session.links[1]
                .packets
                .process_state(secondary.key.process())
                .unwrap(),
            ProcessState::Busy
        );
        assert!(session.links[1].packets.owns_transmission(secondary.key));
        assert!(session.links[1].recycled_blocks[5].is_none());
    }
}

#[test]
fn composite_admission_preserves_rejection_in_either_carrier_order() {
    for results in [
        [Ok(()), Err(FeedbackError::Request)],
        [Err(FeedbackError::Request), Ok(())],
    ] {
        let mut aggregate = None;
        for result in results {
            record_admission(&mut aggregate, result);
        }
        assert_eq!(aggregate, Some(Err(FeedbackError::Request)));
    }
}
