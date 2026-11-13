// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_tx::CommonTx,
    dedicated_tx::{DedicatedTransmitPorts, DedicatedTransmitRuntime},
    hsdpa_packet::control::Commands,
    hsdpa_rx::{
        CarrierAcknowledgements, Evidence, FeedbackMode, Measurement, Observation, Request,
    },
    test_support::{at, count},
};
use umts_dsp::{
    hsdpa::{CellAck, DualHarqAck, FeedbackEvidence},
    scrambling::PrimaryScramblingCode,
};
use umts_types::{hsdpa::CarrierId, time::CHIPS_PER_SLOT};

fn transmitter(source: &CommonTx, codes: [HsdpaCodeSet; 2]) -> Transmit {
    let mut transmit = Transmit::new(count(4));
    for (index, carrier) in [CarrierId::Primary, CarrierId::Secondary]
        .into_iter()
        .enumerate()
    {
        transmit
            .ensure_carrier_calendar(
                carrier,
                codes[index],
                source.next_chip(),
                if index == 0 {
                    source.information().primary_code()
                } else {
                    PrimaryScramblingCode::new(470).unwrap()
                },
            )
            .unwrap();
    }
    transmit
}

fn enqueue_packets(session: &mut Session, now: ChipInstant) {
    for value in 0..8 {
        session.fixture_enqueue_pdu(&[value; 40], 320);
    }
    // Publication tests start with encoded blocks. New-payload staging and
    // fresh-floor selection are exercised separately by the session tests.
    for carrier in [CarrierId::Primary, CarrierId::Secondary] {
        session.fixture_encode_ready_on(carrier, now);
    }
}

fn accept_queued(
    session: &mut Session,
    runtime: &mut DedicatedTransmitRuntime,
    ports: &mut DedicatedTransmitPorts,
    commands: &mut Commands,
) -> Vec<CarrierId> {
    runtime.service().unwrap();
    while let Some(event) = ports.control.try_event() {
        assert!(commands.route(event).is_none());
    }
    let mut carriers = Vec::new();
    while let Some(result) = commands.port(&mut ports.control).take_result() {
        let carrier = session.carrier_for_command(&result).unwrap();
        assert!(matches!(
            session
                .accept_command_on(carrier, result)
                .unwrap_or_else(|error| panic!("{:?}", error.error)),
            CommandProgress::Queued
        ));
        carriers.push(carrier);
    }
    carriers
}

fn acknowledgement(word: DualHarqAck) -> Measurement {
    Measurement::AckCells(Evidence::Measured(Observation {
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

#[test]
fn dual_scheduler_publishes_before_next_preparation_and_refreshes_its_floor() {
    for rotate in [false, true] {
        for staggered in [false, true] {
            Session::with_active_dual_fixture(|fixture| {
                let mut transmit = transmitter(fixture.source, fixture.codes);
                enqueue_packets(fixture.session, fixture.time.now);
                if rotate {
                    assert_eq!(fixture.session.next_scheduling_carrier(2), 0);
                }
                let first = if rotate {
                    CarrierId::Secondary
                } else {
                    CarrierId::Primary
                };
                let second = if rotate {
                    CarrierId::Primary
                } else {
                    CarrierId::Secondary
                };
                let scch = fixture.time.not_before.index - crate::hsdpa_tx::CONTROL_LEAD_CHIPS;
                let mut commands = Commands::new(count(8)).unwrap();
                let mut report = HsdpaSchedulingReport::default();
                let mut reads = 0;
                transmit
                    .schedule_session(
                        fixture.session,
                        fixture.time,
                        || {
                            let read = reads;
                            reads += 1;
                            if read == 2 {
                                // The first queue command must reach TX before the
                                // scheduler reads the second carrier's preparation floor.
                                assert_eq!(fixture.runtime.service().unwrap(), 1);
                            }
                            at(scch + u64::from(staggered && read >= 2))
                        },
                        Some(&mut commands.port(&mut fixture.ports.control)),
                        &mut report,
                    )
                    .unwrap();
                assert_eq!((reads, report.prepared, report.failures), (4, 2, 0));
                assert_eq!(transmit.publication_report.published, 2);
                assert_eq!(transmit.publication_report.stale_cancelled, 0);
                assert_eq!(
                    accept_queued(
                        fixture.session,
                        fixture.runtime,
                        fixture.ports,
                        &mut commands
                    ),
                    [first, second]
                );
                let mut requests = Vec::new();
                for (index, carrier) in [first, second].into_iter().enumerate() {
                    let word = if !staggered {
                        DualHarqAck::Both {
                            primary: CellAck::Ack,
                            secondary: CellAck::Nack,
                        }
                    } else if carrier == CarrierId::Primary {
                        DualHarqAck::Primary(CellAck::Ack)
                    } else {
                        DualHarqAck::Secondary(CellAck::Nack)
                    };
                    let answer = fixture.receiver.fixture_answer_next(acknowledgement(word));
                    let Request::AckCells {
                        start, schedule, ..
                    } = answer.request
                    else {
                        panic!("dual ACK request");
                    };
                    let downlink = at(fixture.time.not_before.index
                        + if staggered && index == 1 {
                            3 * CHIPS_PER_SLOT
                        } else {
                            0
                        });
                    assert_eq!(
                        start,
                        fixture.time.feedback.acknowledgement(downlink).unwrap()
                    );
                    assert_eq!(schedule.mode_at(start).unwrap(), FeedbackMode::Dual);
                    requests.push(answer.id);
                    assert!(
                        fixture
                            .session
                            .poll_classified_feedback(&fixture.policy)
                            .unwrap()
                            .admission
                            .unwrap()
                            .is_ok()
                    );
                }
                assert_ne!(requests[0], requests[1]);
                assert!(
                    fixture
                        .session
                        .poll_classified_feedback(&fixture.policy)
                        .is_none()
                );
                let primary = fixture
                    .session
                    .feedback_snapshot_on(CarrierId::Primary)
                    .outcomes;
                let secondary = fixture
                    .session
                    .feedback_snapshot_on(CarrierId::Secondary)
                    .outcomes;
                assert_eq!(
                    (primary.ack, primary.nack, secondary.ack, secondary.nack),
                    (1, 0, 0, 1)
                );

                // Early publication must not advance calendars to the future cursor,
                // and its counters must appear in exactly one normal service report.
                let mut entries = [None];
                let step = transmit.service(
                    &mut entries,
                    &mut commands.port(&mut fixture.ports.control),
                    count(1),
                    at(fixture.time.now.index + 1),
                );
                assert!(step.failure.is_none());
                assert_eq!(step.published, 2);
                let step = transmit.service(
                    &mut entries,
                    &mut commands.port(&mut fixture.ports.control),
                    count(1),
                    at(fixture.time.now.index + 2),
                );
                assert!(step.failure.is_none());
                assert_eq!(step.published, 0);
            });
        }
    }
}

#[test]
fn stale_second_carrier_does_not_cancel_the_already_published_first_carrier() {
    Session::with_active_dual_fixture(|fixture| {
        let mut transmit = transmitter(fixture.source, fixture.codes);
        enqueue_packets(fixture.session, fixture.time.now);
        let scch = fixture.time.not_before.index - crate::hsdpa_tx::CONTROL_LEAD_CHIPS;
        let mut commands = Commands::new(count(8)).unwrap();
        let mut report = HsdpaSchedulingReport::default();
        let mut reads = 0;
        transmit
            .schedule_session(
                fixture.session,
                fixture.time,
                || {
                    let read = reads;
                    reads += 1;
                    if read == 2 {
                        assert_eq!(fixture.runtime.service().unwrap(), 1);
                    }
                    at(scch + u64::from(read == 3))
                },
                Some(&mut commands.port(&mut fixture.ports.control)),
                &mut report,
            )
            .unwrap();
        assert_eq!(report.prepared, 2);
        assert_eq!(
            (
                transmit.publication_report.published,
                transmit.publication_report.cancelled,
                transmit.publication_report.stale_cancelled
            ),
            (1, 1, 1)
        );
        assert_eq!(transmit.buffers.len(), transmit.capacity - 1);
        assert_eq!(
            accept_queued(
                fixture.session,
                fixture.runtime,
                fixture.ports,
                &mut commands
            ),
            [CarrierId::Primary]
        );
        assert!(
            fixture
                .session
                .pending_transmission_on(CarrierId::Secondary)
                .is_none()
        );
        assert_eq!(fixture.session.active_packets().unwrap().1.outstanding(), 1);
        let answer = fixture
            .receiver
            .fixture_answer_next(acknowledgement(DualHarqAck::Primary(CellAck::Ack)));
        let Request::AckCells {
            start, schedule, ..
        } = answer.request
        else {
            panic!("dual ACK request");
        };
        assert_eq!(schedule.mode_at(start).unwrap(), FeedbackMode::Dual);
        assert!(
            fixture
                .session
                .poll_classified_feedback(&fixture.policy)
                .unwrap()
                .admission
                .unwrap()
                .is_ok()
        );
        assert_eq!(fixture.session.active_packets().unwrap().1.outstanding(), 0);
        assert_eq!(
            fixture
                .session
                .feedback_snapshot_on(CarrierId::Primary)
                .outcomes
                .ack,
            1
        );
        assert_eq!(
            fixture
                .session
                .feedback_snapshot_on(CarrierId::Secondary)
                .outcomes
                .ack,
            0
        );
    });
}

#[test]
fn per_carrier_publication_backpressure_retains_one_feedback_request_and_buffer() {
    Session::with_active_dual_fixture(|fixture| {
        let mut transmit = transmitter(fixture.source, fixture.codes);
        enqueue_packets(fixture.session, fixture.time.now);
        let scch = at(fixture.time.not_before.index - crate::hsdpa_tx::CONTROL_LEAD_CHIPS);
        let mut commands = Commands::new(count(1)).unwrap();
        let mut report = HsdpaSchedulingReport::default();
        transmit
            .schedule_session(
                fixture.session,
                fixture.time,
                || scch,
                Some(&mut commands.port(&mut fixture.ports.control)),
                &mut report,
            )
            .unwrap();
        assert_eq!(report.prepared, 2);
        assert_eq!(
            (
                transmit.publication_report.published,
                transmit.publication_report.publication_backpressure
            ),
            (1, 1)
        );
        let pending = fixture
            .session
            .pending_transmission_on(CarrierId::Secondary)
            .unwrap();
        let request_ids = |session: &mut Session| {
            session
                .active_packets()
                .unwrap()
                .1
                .pending_requests()
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
        };
        let before = request_ids(fixture.session);
        assert_eq!(before.len(), 2);
        transmit.publish_prepared_on(
            fixture.session,
            1,
            &mut commands.port(&mut fixture.ports.control),
            fixture.time.now,
            scch,
        );
        assert_eq!(request_ids(fixture.session), before);
        assert_eq!(
            fixture
                .session
                .pending_transmission_on(CarrierId::Secondary),
            Some(pending)
        );
        assert_eq!(transmit.buffers.len(), transmit.capacity - 2);
        assert_eq!(
            accept_queued(
                fixture.session,
                fixture.runtime,
                fixture.ports,
                &mut commands
            ),
            [CarrierId::Primary]
        );
        transmit.publish_prepared_on(
            fixture.session,
            1,
            &mut commands.port(&mut fixture.ports.control),
            fixture.time.now,
            scch,
        );
        assert_eq!(request_ids(fixture.session), before);
        assert!(
            fixture
                .session
                .pending_transmission_on(CarrierId::Secondary)
                .is_none()
        );
        assert_eq!(
            (
                transmit.publication_report.published,
                transmit.publication_report.publication_backpressure,
                transmit.publication_report.cancelled
            ),
            (2, 2, 0)
        );
        assert_eq!(
            accept_queued(
                fixture.session,
                fixture.runtime,
                fixture.ports,
                &mut commands
            ),
            [CarrierId::Secondary]
        );
    });
}
