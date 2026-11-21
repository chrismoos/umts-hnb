// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_tx::CommonTx,
    dedicated_tx::{DedicatedTransmitPorts, DedicatedTransmitRuntime, TransmitRuntimeLimits},
    hsdpa_packet::{ControlPort, control::Commands},
    hsdpa_rx::{
        CarrierAcknowledgements, CarrierCqi, Evidence, EvidenceLimits, FeedbackPolicy,
        FeedbackReceiver, Measurement, Observation, Request,
    },
    hsdpa_session::{
        CellSchedulingBudget, CommandProgress, DownlinkPower, DualCellSettings, Phase, QueueBudget,
        SchedulingBudget, SchedulingOutcome, SchedulingPreparation, SchedulingSettings,
        SchedulingTime, Session, SessionSettings,
    },
    hsdpa_tx::{AllocationCalendar, TransmitBank, WaveformBuffer, WaveformPreparer},
    radio_resources::{RadioResources, SecondaryRadioResources},
    test_support::{amplitudes, count, plan},
};
use num_complex::Complex32;
use umts_dsp::{
    dedicated::UplinkDpchCode,
    hsdpa::{CellAck, CompositeCqi, Cqi, DualHarqAck, FeedbackEvidence},
    pulse_shaping::SamplesPerChip,
    scrambling::PrimaryScramblingCode,
    uplink_scrambling::UplinkScramblingCode,
};
use umts_rrc::hsdpa::{InitialConfiguration, SecondaryCellConfiguration};
use umts_types::{
    hsdpa::{CarrierId, DualCellCategory},
    time::CHIPS_PER_SLOT,
};

pub(crate) struct ActiveDualFixture<'a> {
    pub session: &'a mut Session,
    pub bank: &'a crate::dedicated_bearers::DedicatedBearers,
    pub receiver: &'a mut FeedbackReceiver,
    pub source: &'a mut CommonTx,
    pub runtime: &'a mut DedicatedTransmitRuntime,
    pub ports: &'a mut DedicatedTransmitPorts,
    pub codes: [crate::radio_resources::HsdpaCodeSet; 2],
    pub time: SchedulingTime,
    pub policy: FeedbackPolicy,
}

impl Session {
    pub(crate) fn with_active_dual_fixture(mut test: impl FnMut(ActiveDualFixture<'_>)) {
        dual_lifecycle_with_partial_release(None, false, None, false, false, Some(&mut test));
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

fn results(
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

fn completions(
    session: &mut Session,
    runtime: &mut DedicatedTransmitRuntime,
    ports: &mut DedicatedTransmitPorts,
    commands: &mut Commands,
    expected: usize,
) {
    let mut recycled = 0;
    for _ in 0..4 {
        commands
            .port(&mut ports.control)
            .request_completion()
            .unwrap();
        recycled += results(session, runtime, ports, commands);
    }
    assert_eq!(recycled, expected);
}

fn publish(
    session: &mut Session,
    carrier: CarrierId,
    calendar: &mut AllocationCalendar,
    control: &mut ControlPort<'_>,
) {
    assert!(matches!(
        session
            .publish_packet_on(carrier, control, calendar)
            .unwrap(),
        crate::hsdpa_session::PacketPublication::Queued { .. }
    ));
}

#[test]
fn dual_session_routes_independent_feedback_retries_stale_reports_and_retirement() {
    dual_lifecycle(None, false, None, false);
}

#[test]
fn active_dual_order_failure_recovers_only_the_validated_live_route() {
    for successor in [false, true] {
        dual_lifecycle(Some(successor), false, None, false);
    }
}

#[test]
fn retiring_dual_activation_timeout_does_not_request_signalling_recovery() {
    dual_lifecycle(None, true, None, false);
}

#[test]
fn dual_mac_ehs_without_64qam_grants_and_publishes_octet_rlc_pdus() {
    for category in 21..=24 {
        dual_lifecycle(None, false, Some(category), false);
    }
}

#[test]
fn dual_feedback_service_resumes_after_committed_radio_mapping_rebind() {
    dual_lifecycle(None, false, None, true);
}

fn dual_lifecycle(
    order_failure: Option<bool>,
    retire_before_complete: bool,
    mac_ehs_category: Option<u8>,
    rebind_feedback: bool,
) {
    dual_lifecycle_with_partial_release(
        order_failure,
        retire_before_complete,
        mac_ehs_category,
        rebind_feedback,
        false,
        None,
    );
}

#[test]
fn partial_release_rebinds_real_session_and_feeds_remaining_bearer() {
    dual_lifecycle_with_partial_release(None, false, None, false, true, None);
}

fn dual_lifecycle_with_partial_release(
    order_failure: Option<bool>,
    retire_before_complete: bool,
    mac_ehs_category: Option<u8>,
    rebind_feedback: bool,
    partial_release: bool,
    test: Option<&mut dyn FnMut(ActiveDualFixture<'_>)>,
) {
    let mut c = connected();
    let dedicated = if partial_release {
        with_second_packet_bearer(c.procedure.previous())
    } else {
        c.procedure.previous().clone()
    };
    let origin = at(RELEASE_ACTIVATION_FRAME * CHIPS_PER_FRAME);
    let pdsch_origin = at(origin.index + crate::hsdpa_tx::CONTROL_LEAD_CHIPS);
    let secondary_code = PrimaryScramblingCode::new(470).unwrap();
    let mut source = CommonTx::new(
        plan(true),
        origin,
        SamplesPerChip::Four,
        8,
        amplitudes(),
        count(8),
        100,
    )
    .unwrap();
    source
        .enable_secondary_carrier(
            8,
            secondary_code,
            amplitudes().primary,
            [-2_500_000, 2_500_000],
            0.5,
        )
        .unwrap();
    let mut primary_resources =
        RadioResources::new(c.key.epoch, source.information(), count(2), 0..=1).unwrap();
    let mut secondary_resources =
        SecondaryRadioResources::new(c.key.epoch, source.information(), secondary_code).unwrap();
    let primary_lease = primary_resources
        .reserve_hsdpa(NonZeroU8::new(2).unwrap(), NonZeroU8::MIN)
        .unwrap();
    let secondary_lease = secondary_resources
        .reserve_hsdpa(NonZeroU8::new(2).unwrap(), NonZeroU8::MIN)
        .unwrap();
    let codes = [*primary_lease.codes(), *secondary_lease.codes()];
    let category = DualCellCategory::new(mac_ehs_category.unwrap_or(21))
        .unwrap()
        .with_qam64(false)
        .unwrap();
    let mut sizes = [None; 8];
    sizes[0] = Some(count(320));
    let configuration = Arc::new(
        InitialConfiguration::new_mac_ehs(
            dedicated.clone(),
            ActivationParameters {
                h_rnti: 0x8421,
                harq: HarqConfiguration::implicit(
                    category.per_cell_category(),
                    NonZeroU8::new(6).unwrap(),
                )
                .unwrap(),
                queues: QueueSet::new(
                    (0..if partial_release { 2 } else { 1 })
                        .map(|id| QueueConfiguration::new(id, id, sizes, 100, 32).unwrap())
                        .collect(),
                )
                .unwrap(),
                control: ControlChannels::new(&codes[0].control_codes().collect::<Vec<_>>())
                    .unwrap(),
                measurement: MeasurementFeedback::new(6, FeedbackCycle::Ms2, NonZeroU8::MIN, 8)
                    .unwrap(),
                acknowledgement: AckFeedback::new(8, 8, NonZeroU8::MIN).unwrap(),
            },
            (0..if partial_release { 2 } else { 1 })
                .map(|id| MappedBearer {
                    bearer: 20 + id,
                    flow: id,
                    logical: Some(1 + id),
                })
                .collect(),
        )
        .unwrap()
        .with_secondary_cell(
            SecondaryCellConfiguration::new(
                0x8421,
                10662,
                470,
                ControlChannels::new(&codes[1].control_codes().collect::<Vec<_>>()).unwrap(),
                6,
                false,
            )
            .unwrap(),
        )
        .unwrap(),
    );
    let construction = Arc::new(
        InitialActivation::schedule(configuration.clone(), c.procedure.activation()).unwrap(),
    );
    let procedure = Box::new(
        ActivationProcedure::new(
            c.key,
            c.radio.next_hspa_activation_transaction(c.key).unwrap(),
            construction,
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
            at((RELEASE_PUBLICATION_FRAME + 1) * CHIPS_PER_FRAME),
            NonZeroU64::new(8 * CHIPS_PER_FRAME).unwrap(),
        )
        .unwrap(),
    );
    let code = Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(0x123456).unwrap()).unwrap());
    let mut receiver = FeedbackReceiver::new(c.key, code, 1, origin).unwrap();
    let feedback = receiver.connect(count(16), count(16)).unwrap();
    let cell = CellSchedulingBudget::new(count(32_000_000));
    let limits = EvidenceLimits::new(0.5, 0.1, 0.5, 1.0, 0.01).unwrap();
    let policy = FeedbackPolicy {
        acknowledgement: limits,
        cqi: limits,
    };
    let build = |codes| {
        SchedulingPreparation::new(
            configuration.clone(),
            codes,
            SchedulingSettings {
                power: DownlinkPower::new(0.1, 0.1).unwrap(),
                maximum_cqi_age: NonZeroU64::new(1_000_000).unwrap(),
                selection_budget: count(4096),
                cqi_offset_limit: 0,
                cqi_target_bler_percent: 10,
            },
            SchedulingBudget {
                maximum_formats: count(1024),
                maximum_coded_bits: count(100_000),
                maximum_plan_bytes: count(16_000_000),
                maximum_buffer_bytes: count(2_000_000),
                maximum_encoder_bytes: count(1_000_000),
                maximum_total_bytes: count(16_000_000),
            },
            &cell,
        )
        .unwrap_or_else(|error| panic!("{error:?}"))
        .build()
        .unwrap_or_else(|error| panic!("{error:?}"))
    };
    let mut session = Session::new_prepared_dual(
        c.old,
        procedure,
        feedback,
        SessionSettings {
            incarnation: NonZeroU64::MIN,
            downlink_origin: pdsch_origin,
            queues: QueueBudget {
                pdus_per_queue: count(8),
                total_bytes: count(65536),
            },
            codes: codes[0],
            scheduling: None,
        },
        Some((build(codes[0]), policy)),
        DualCellSettings {
            category,
            incarnation: NonZeroU64::new(2).unwrap(),
            codes: codes[1],
            resources: build(codes[1]),
            feedback: policy,
        },
    )
    .unwrap_or_else(|rejected| panic!("{:?}", rejected.error));
    let mut mapping = radio_release::bank(&c, c.old, dedicated);
    session
        .publish(
            &mut c.radio,
            &mut mapping,
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
        )
        .unwrap();
    let receipt = session.procedure().receipt().unwrap();
    publish_release(&mut c, receipt);
    if retire_before_complete {
        assert_eq!(session.phase(), Phase::AwaitingComplete);
        assert!(
            !c.radio
                .registry
                .get(c.key)
                .unwrap()
                .protection
                .blocks_status()
        );
        session.begin_retirement();
        let timeout = at((RELEASE_PUBLICATION_FRAME + 9) * CHIPS_PER_FRAME);
        assert!(session.expire(&mut c.radio, timeout).unwrap());
        assert_eq!(
            session.procedure().phase(),
            ActivationPhase::RecoveryRequired
        );
        assert_eq!(session.phase(), Phase::Retiring);
        assert!(
            !c.radio
                .registry
                .get(c.key)
                .unwrap()
                .protection
                .blocks_status()
        );
        // A repeated tick must also ignore the already failed activation during retirement.
        assert!(
            !session
                .expire(&mut c.radio, at(timeout.index + CHIPS_PER_FRAME))
                .unwrap()
        );
        assert_eq!(session.phase(), Phase::Retiring);
        assert!(
            !c.radio
                .registry
                .get(c.key)
                .unwrap()
                .protection
                .blocks_status()
        );
        assert_eq!(session.active_recovery_identity(), None);
        session.drain_retirement(count(16)).unwrap();
        return;
    }
    let transaction = session.procedure().transaction();
    let provenance = ReceiveProvenance::Path(c.old.configuration);
    receive(&mut c, complete(transaction), provenance);
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(origin.index + 20),
            security_time(),
            |message, mut security| session
                .response(&message, &mut security, |_, _| Ok(()))
                .map(|_| ())
        ),
        Some(Ok(c.key))
    );
    session
        .commit(&mut c.radio, |_| Ok::<_, SignallingError>(()))
        .unwrap();
    acknowledge(
        &mut c.radio,
        c.key,
        &mut c.sender,
        receipt,
        origin.index + 100,
    );
    session.finish_command(&mut c.radio).unwrap();
    assert_eq!(session.phase(), Phase::Active);
    let timing = session.feedback_timing(&mapping).unwrap();
    session
        .configure_secondary_control(timing.acknowledgement(pdsch_origin).unwrap())
        .unwrap();
    if partial_release {
        use crate::dedicated_bearers::BearerId;
        use umts_security::{
            Key,
            ciphering::{BearerCipherConfiguration, Configuration, CountAnchor, Position, Scope},
            contexts::{CipherMaterial, KeyOrigin, KeySet},
            counters::CipherMode,
        };
        let key_origin = KeyOrigin {
            ue: c.key,
            domain: umts_types::CnDomain::Ps,
            generation: NonZeroU64::MIN,
        };
        let cipher = KeySet::uia1(
            key_origin,
            CipherMaterial::Uea1(Key::new([0x22; 16])),
            Key::new([0x33; 16]),
        )
        .split()
        .0;
        let start = umts_security::counters::Start::new(0).unwrap();
        let anchor = CountAnchor::from_start(
            CipherMode::Acknowledged,
            Position::Sequence(u64::from(start.initial_count())),
            start,
        )
        .unwrap();
        c.radio
            .with_bearer_access(c.key, |ciphers, _, _| {
                ciphers
                    .register_initial_with(
                        [BearerCipherConfiguration {
                            bearer: 20,
                            scope: Scope::User(umts_types::CnDomain::Ps),
                            transmit: anchor,
                            receive: anchor,
                            initial: Configuration::Configured(cipher),
                        }],
                        &[],
                        || Ok::<_, ()>(()),
                    )
                    .unwrap();
            })
            .unwrap();
        session.fixture_seed_partial_release();
        for iteration in 0..2 {
            let frame = RELEASE_ACTIVATION_FRAME + 12 + iteration * 4;
            let now = at(frame * CHIPS_PER_FRAME);
            let identity = crate::dedicated_rx::ReceiveIdentity {
                configuration: NonZeroU64::new(c.new.configuration.get() + iteration).unwrap(),
                ..c.new
            };
            let mut successor =
                radio_release::bank_at(&c, identity, c.procedure.previous().clone(), frame);
            successor.fixture_inherit_hsdpa_route(&mapping);
            let code = Arc::new(
                UplinkDpchCode::new(UplinkScramblingCode::new(0x123456).unwrap()).unwrap(),
            );
            let mut next_receiver = FeedbackReceiver::new(c.key, code, 1, now).unwrap();
            let next_feedback = next_receiver.connect(count(16), count(16)).unwrap();
            drop(std::mem::replace(&mut receiver, next_receiver));
            session.begin_radio_change(identity).unwrap();
            assert!(session.drain_radio_change(count(16)).unwrap());
            assert!(session.settle_radio_change_orders(now, false).unwrap());
            let old_feedback = session
                .finish_radio_change(&successor, next_feedback)
                .unwrap_or_else(|(error, _)| panic!("partial release rebind: {error:?}"));
            drop(old_feedback);
            assert_eq!(session.identity(), identity);
            assert_eq!(session.phase(), Phase::Active);
            session.fixture_assert_partial_release();
            assert!(
                successor
                    .has_active_hsdpa_bearer(identity, BearerId::new(20).unwrap())
                    .unwrap()
            );
            assert!(
                !successor
                    .has_active_hsdpa_bearer(identity, BearerId::new(21).unwrap())
                    .unwrap()
            );
            if iteration == 0 {
                successor
                    .enqueue_sdu(BearerId::new(20).unwrap(), &[0x55; 20])
                    .unwrap();
            }
            c.radio
                .with_bearer_access(c.key, |ciphers, _, _| {
                    let fed = session
                        .feed_queues(&mut successor, now, ciphers, count(8))
                        .unwrap();
                    if iteration == 0 {
                        assert!(fed.enqueued > 0);
                    }
                })
                .unwrap();
            session.fixture_assert_partial_release();
            mapping = successor;
        }
        let identity = crate::dedicated_rx::ReceiveIdentity {
            configuration: NonZeroU64::new(session.identity().configuration.get() + 1).unwrap(),
            ..session.identity()
        };
        let mut released = radio_release::bank_at(
            &c,
            identity,
            super::super::super::configuration(&c.radio.retained_signalling(c.key).unwrap()),
            RELEASE_ACTIVATION_FRAME + 20,
        );
        released.fixture_inherit_hsdpa_route(&mapping);
        assert!(released.hsdpa_route(identity).unwrap().is_none());
        assert!(session.retire_released_route(&released).unwrap());
        assert_eq!(session.phase(), Phase::Retiring);
        assert!(session.detached_route());
        assert!(Box::new(session).try_retire().is_err());
        return;
    }
    if rebind_feedback {
        // The retained 40ms DCH TTI requires a frame divisible by four.
        let frame = RELEASE_ACTIVATION_FRAME + 12;
        let now = at(frame * CHIPS_PER_FRAME);
        let mut successor =
            radio_release::bank_at(&c, c.new, c.procedure.previous().clone(), frame);
        successor.fixture_inherit_hsdpa_route(&mapping);
        let code =
            Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(0x123456).unwrap()).unwrap());
        let mut next_receiver = FeedbackReceiver::new(c.key, code, 1, now).unwrap();
        let next_feedback = next_receiver.connect(count(16), count(16)).unwrap();
        drop(receiver);
        session.begin_radio_change(c.new).unwrap();
        assert!(session.drain_radio_change(count(16)).unwrap());
        assert!(session.settle_radio_change_orders(now, false).unwrap());
        let retired = session
            .finish_radio_change(&successor, next_feedback)
            .unwrap_or_else(|(error, _)| panic!("{error:?}"));
        drop(retired);
        assert_eq!(session.identity(), c.new);
        assert_eq!(session.phase(), Phase::Active);
        let horizon = at(now.index + 3 * CHIPS_PER_SLOT);
        let first = session.service_feedback(&successor, now, horizon).unwrap();
        assert_eq!((first.reports, first.requested, first.failures), (0, 1, 0));
        let report = next_receiver.fixture_answer_next(Measurement::CqiCells(evidence(
            CarrierCqi::Dual(CompositeCqi {
                primary: Cqi::new(8).unwrap(),
                secondary: Cqi::new(9).unwrap(),
            }),
        )));
        let Request::CqiCells { start, .. } = report.request else {
            panic!("retained dual feedback request");
        };
        let current = session.feedback_timing(&successor).unwrap();
        assert_eq!(
            start,
            current
                .acknowledgement(current.next_pdsch(now).unwrap())
                .unwrap()
        );
        assert!(start.index > now.index);
        let next = session
            .service_feedback(&successor, horizon, at(horizon.index + 3 * CHIPS_PER_SLOT))
            .unwrap();
        assert_eq!((next.reports, next.requested, next.failures), (1, 1, 0));
        assert_eq!(
            session.feedback_snapshot_on(CarrierId::Primary).cqi_reports[8],
            1
        );
        assert_eq!(
            session
                .feedback_snapshot_on(CarrierId::Secondary)
                .cqi_reports[9],
            1
        );
        return;
    }
    if mac_ehs_category.is_some() {
        use umts_security::{
            Key,
            ciphering::{
                BearerCipherConfiguration, CipherUse, Configuration, CountAnchor, Position, Scope,
            },
            contexts::{CipherMaterial, KeyOrigin, KeySet},
            counters::CipherMode,
        };
        let key_origin = KeyOrigin {
            ue: c.key,
            domain: umts_types::CnDomain::Ps,
            generation: NonZeroU64::MIN,
        };
        let cipher = KeySet::uia1(
            key_origin,
            CipherMaterial::Uea1(Key::new([0x22; 16])),
            Key::new([0x33; 16]),
        )
        .split()
        .0;
        let start = umts_security::counters::Start::new(0).unwrap();
        let anchor = CountAnchor::from_start(
            CipherMode::Acknowledged,
            Position::Sequence(u64::from(start.initial_count())),
            start,
        )
        .unwrap();
        c.radio
            .with_bearer_access(c.key, |ciphers, _, _| {
                ciphers
                    .register_initial_with(
                        [BearerCipherConfiguration {
                            bearer: 20,
                            scope: Scope::User(umts_types::CnDomain::Ps),
                            transmit: anchor,
                            receive: anchor,
                            initial: Configuration::Configured(cipher),
                        }],
                        &[],
                        || Ok::<_, ()>(()),
                    )
                    .unwrap();
            })
            .unwrap();
        assert!(
            session
                .procedure()
                .construction()
                .configuration()
                .uses_mac_ehs()
        );
        assert!(!category.per_cell_limits().qam64);
        let bearer = crate::dedicated_bearers::BearerId::new(20).unwrap();
        let grant = mapping.hsdpa_grant(c.old, bearer, origin).unwrap();
        assert_eq!(grant.queue, 0);
        assert_eq!(grant.size_index, 0);
        mapping.enqueue_sdu(bearer, &[0x55; 20]).unwrap();
        c.radio
            .with_bearer_access(c.key, |ciphers, _, _| {
                let published = mapping
                    .publish_hsdpa(grant, origin, ciphers, |pdu| {
                        // MAC-ehs identifies the logical channel in its own header. The RLC PDU
                        // remains exactly 40 octets, with no 4-bit C/T prefix.
                        assert_eq!(pdu.data.len(), 320);
                        let mut bytes = [0u8; 40];
                        pdu.data.copy_to(&mut bytes).unwrap();
                        let encrypted = bytes;
                        let deciphered = ciphers
                            .apply(
                                key_origin.ue,
                                20,
                                umts_types::Direction::Downlink,
                                Position::Sequence(0),
                                &mut bytes,
                                320,
                            )
                            .unwrap();
                        assert_eq!(deciphered, pdu.cipher);
                        assert_ne!(bytes, encrypted);
                        assert!(bytes.windows(20).any(|window| window == [0x55; 20]));
                        assert_eq!(
                            pdu.cipher,
                            CipherUse::Data {
                                origin: Some(key_origin),
                                count: 0,
                            }
                        );
                        assert!(matches!(
                            pdu.acknowledged_kind,
                            Some(umts_rlc::acknowledged::PublicationKind::Data {
                                retransmission: false,
                                ..
                            })
                        ));
                        Ok::<_, ()>(())
                    })
                    .unwrap();
                assert_eq!(published, Some(()));
            })
            .unwrap();
        return;
    }
    if let Some(successor) = order_failure {
        let before = c.radio.bearer_setup_readiness(c.key).unwrap();
        assert!(!before.bearer_change_pending);
        assert!(!before.protection_blocks_reconfiguration);
        session
            .request_secondary_order(umts_dsp::hsdpa::SecondaryCellOrder::Deactivate)
            .unwrap();
        let scch = at(pdsch_origin.index - crate::hsdpa_tx::CONTROL_LEAD_CHIPS);
        let tx = session
            .prepare_secondary_order(scch, timing.acknowledgement(pdsch_origin).unwrap())
            .unwrap();
        session.accept_secondary_order(tx).unwrap();
        if successor {
            session.begin_radio_change(c.new).unwrap();
        }
        session.secondary_order_not_transmitted(tx);
        assert_eq!(session.phase(), Phase::RecoveryRequired);
        assert_eq!(
            session.active_recovery_identity(),
            Some(if successor { c.new } else { c.old })
        );
        // The original completed activation is not a pending recovery transaction.
        assert!(
            c.radio
                .require_hspa_activation_recovery(session.procedure())
                .is_err()
        );
        session.expire(&mut c.radio, tx.codebook_effective).unwrap();
        let wrong = radio_release::bank(&c, c.new, c.procedure.previous().clone());
        assert!(session.recover_active_route(&mut c.radio, &wrong).is_err());
        assert!(
            !c.radio
                .bearer_setup_readiness(c.key)
                .unwrap()
                .protection_blocks_reconfiguration
        );
        if successor {
            assert!(
                session
                    .recover_active_route(&mut c.radio, &mapping)
                    .is_err()
            );
            mapping = radio_release::bank(&c, c.new, c.procedure.previous().clone());
            mapping
                .reserve_hsdpa_route(c.new, session.procedure().construction().clone())
                .unwrap();
            // A matching bank alone cannot authorize recovery before signaling commits it.
            assert!(
                session
                    .recover_active_route(&mut c.radio, &mapping)
                    .is_err()
            );
            c.radio.registry.get_mut(c.key).unwrap().dedicated_route = Some(c.new);
        }
        session
            .recover_active_route(&mut c.radio, &mapping)
            .unwrap();
        assert!(
            c.radio
                .bearer_setup_readiness(c.key)
                .unwrap()
                .protection_blocks_reconfiguration
        );
        // Repeated controller service is idempotent and does not restart activation.
        session
            .recover_active_route(&mut c.radio, &mapping)
            .unwrap();
        assert!(
            !c.radio
                .bearer_setup_readiness(c.key)
                .unwrap()
                .bearer_change_pending
        );
        assert!(!session.data_publication_allowed(CarrierId::Primary));
        assert!(!session.data_publication_allowed(CarrierId::Secondary));
        session.begin_retirement();
        assert_eq!(session.phase(), Phase::Retiring);
        assert_eq!(session.active_recovery_identity(), None);
        assert_eq!(
            session.recover_active_route(&mut c.radio, &mapping),
            Err(crate::hsdpa_session::Error::Phase)
        );
        session.drain_retirement(count(16)).unwrap();
        assert_eq!(session.active_recovery_identity(), None);
        return;
    }
    assert_eq!(
        session
            .request_periodic_cqi(timing, pdsch_origin, pdsch_origin, count(1))
            .unwrap()
            .requested,
        1
    );
    let cqi_report = receiver.fixture_answer_next(Measurement::CqiCells(evidence(
        CarrierCqi::Dual(CompositeCqi {
            primary: Cqi::new(8).unwrap(),
            secondary: Cqi::new(8).unwrap(),
        }),
    )));
    let Request::CqiCells { start, .. } = cqi_report.request else {
        panic!("dual CQI request");
    };
    assert!(session.poll_classified_feedback(&policy).is_some());
    let now = at(start.index + CHIPS_PER_SLOT);
    let not_before = timing
        .next_pdsch(at(now.index + crate::hsdpa_tx::CONTROL_LEAD_CHIPS))
        .unwrap();
    let (mut runtime, mut ports) = DedicatedTransmitRuntime::new(
        &source,
        count(2),
        count(4),
        count(1_000_000),
        TransmitRuntimeLimits {
            command_capacity: count(16),
            commands_per_step: count(16),
            feedback_capacity: count(16),
            feedback_per_step: count(16),
            maximum_frame_payload_bits: count(100_000),
            maximum_inflight_retained_bits: count(20_000_000),
        },
    )
    .unwrap();
    for lease in [primary_lease, secondary_lease] {
        let primary = PrimaryScramblingCode::new(lease.primary_code()).unwrap();
        let bank = TransmitBank::for_carrier(&source, primary, lease, count(4), count(4_000_000))
            .unwrap_or_else(|_| panic!("transmit bank"));
        runtime
            .attach_hsdpa(bank)
            .unwrap_or_else(|_| panic!("attach bank"));
    }
    if let Some(test) = test {
        test(ActiveDualFixture {
            session: &mut session,
            bank: &mapping,
            receiver: &mut receiver,
            source: &mut source,
            runtime: &mut runtime,
            ports: &mut ports,
            codes,
            time: SchedulingTime {
                now,
                not_before,
                feedback: timing,
            },
            policy,
        });
        return;
    }
    let mut writer = source.submission_writer().unwrap();
    let mut commands = Commands::new(count(16)).unwrap();
    let mut calendars =
        codes.map(|codes| AllocationCalendar::new(codes, origin, count(16)).unwrap());
    let mut preparers = [
        WaveformPreparer::new(codes[0], source.information().primary_code()).unwrap(),
        WaveformPreparer::new(codes[1], secondary_code).unwrap(),
    ];
    let mut transmissions = Vec::new();
    for (index, carrier) in [CarrierId::Primary, CarrierId::Secondary]
        .into_iter()
        .enumerate()
    {
        session.fixture_enqueue_pdu(&[index as u8 + 1; 40], 320);
        assert!(matches!(
            session.schedule_packet_on(
                carrier,
                &mut calendars[index],
                &mut preparers[index],
                SchedulingTime {
                    now,
                    not_before,
                    feedback: timing
                },
                WaveformBuffer::default(),
                || at(not_before.index - crate::hsdpa_tx::CONTROL_LEAD_CHIPS),
            ),
            SchedulingOutcome::Prepared
        ));
        let tx = session.pending_transmission_on(carrier).unwrap();
        assert_eq!(
            session
                .fixture_packet(carrier, tx.key.process())
                .unwrap()
                .sequence,
            index as u8
        );
        transmissions.push(tx);
        publish(
            &mut session,
            carrier,
            &mut calendars[index],
            &mut commands.port(&mut ports.control),
        );
    }
    assert_eq!(transmissions[0].downlink, transmissions[1].downlink);
    assert_ne!(transmissions[0].key, transmissions[1].key);
    assert_eq!(
        results(&mut session, &mut runtime, &mut ports, &mut commands),
        0
    );
    let mut samples = vec![Complex32::default(); source.samples_per_slot()];
    while source.next_chip().index < transmissions[0].expected_ack.index + 3 * CHIPS_PER_SLOT {
        assert!(writer.accepted(runtime.generate_slot(&mut source, &mut samples).unwrap()));
    }
    completions(&mut session, &mut runtime, &mut ports, &mut commands, 2);
    let ack_nack =
        Measurement::AckCells(evidence(CarrierAcknowledgements::Dual(DualHarqAck::Both {
            primary: CellAck::Ack,
            secondary: CellAck::Nack,
        })));
    let old_primary_report = receiver.fixture_answer_next(ack_nack);
    assert!(
        session
            .poll_classified_feedback(&policy)
            .unwrap()
            .admission
            .unwrap()
            .is_ok()
    );
    let old_secondary_report = receiver.fixture_answer_next(ack_nack);
    assert!(
        session
            .poll_classified_feedback(&policy)
            .unwrap()
            .admission
            .unwrap()
            .is_ok()
    );
    let resolved = session.resolve_packets(count(16)).unwrap();
    assert_eq!((resolved.delivered, resolved.retry), (1, 1));
    assert!(
        session
            .fixture_packet(CarrierId::Primary, transmissions[0].key.process())
            .is_none()
    );
    assert_eq!(
        session
            .fixture_packet(CarrierId::Secondary, transmissions[1].key.process())
            .unwrap()
            .sequence,
        1
    );
    let retry_now = source.next_chip();
    let retry_floor = timing
        .next_pdsch(at(retry_now.index + crate::hsdpa_tx::CONTROL_LEAD_CHIPS))
        .unwrap();
    assert!(matches!(
        session.schedule_packet_on(
            CarrierId::Secondary,
            &mut calendars[1],
            &mut preparers[1],
            SchedulingTime {
                now: retry_now,
                not_before: retry_floor,
                feedback: timing
            },
            WaveformBuffer::default(),
            || panic!("ready retry must not re-encode"),
        ),
        SchedulingOutcome::Prepared
    ));
    let retry = session
        .pending_transmission_on(CarrierId::Secondary)
        .unwrap();
    assert_ne!(retry.key, transmissions[1].key);
    assert_eq!(retry.new_data, transmissions[1].new_data);
    assert_eq!(
        session
            .fixture_packet(CarrierId::Secondary, retry.key.process())
            .unwrap()
            .sequence,
        1
    );
    publish(
        &mut session,
        CarrierId::Secondary,
        &mut calendars[1],
        &mut commands.port(&mut ports.control),
    );
    results(&mut session, &mut runtime, &mut ports, &mut commands);
    assert!(session.fixture_rejects_stale_feedback(&old_primary_report));
    assert!(session.fixture_rejects_stale_feedback(&old_secondary_report));
    while source.next_chip().index < retry.expected_ack.index + 3 * CHIPS_PER_SLOT {
        assert!(writer.accepted(runtime.generate_slot(&mut source, &mut samples).unwrap()));
    }
    completions(&mut session, &mut runtime, &mut ports, &mut commands, 1);
    let retry_report = receiver.fixture_answer_next(Measurement::AckCells(evidence(
        CarrierAcknowledgements::Dual(DualHarqAck::Secondary(CellAck::Ack)),
    )));
    assert_ne!(retry_report.id, old_secondary_report.id);
    assert!(
        session
            .poll_classified_feedback(&policy)
            .unwrap()
            .admission
            .unwrap()
            .is_ok()
    );
    assert_eq!(session.resolve_packets(count(16)).unwrap().delivered, 1);
    session.begin_retirement();
    assert!(!session.data_publication_allowed(CarrierId::Primary));
    assert!(!session.data_publication_allowed(CarrierId::Secondary));
    for _ in 0..4 {
        session.drain_retirement(count(16)).unwrap();
        session
            .service_retirement(&mut commands.port(&mut ports.control))
            .unwrap();
        results(&mut session, &mut runtime, &mut ports, &mut commands);
    }
    let retired = Box::new(session)
        .try_retire()
        .unwrap_or_else(|_| panic!("both banks drained"));
    assert!(cell.available_bytes() < 32_000_000);
    drop(retired);
    // Shared HSDPA plan tables stay with the cell.
    assert_eq!(
        cell.available_bytes() + cell.tables().retained_bytes(),
        32_000_000
    );
}

pub(super) fn with_second_packet_bearer(
    original: &Arc<Rel99Configuration>,
) -> Arc<Rel99Configuration> {
    use umts_rrc::dedicated::{DchDirection, DchTfcs};
    let packet = original.bearer(20).unwrap();
    let extend = |source: &DchTfcs, direction: DchDirection, source_id: u8| {
        let index = source
            .transports()
            .iter()
            .position(|transport| transport.id == source_id)
            .unwrap();
        let mut transports = source.transports().to_vec();
        let id = transports
            .iter()
            .map(|transport| transport.id)
            .max()
            .unwrap()
            + 1;
        let mut extra = transports[index].clone();
        extra.id = id;
        transports.push(extra);
        let mut combinations = Vec::new();
        for original in source.combinations() {
            let mut row = original.clone();
            row.formats.push(0);
            combinations.push(row);
            // Retain the existing DCH load envelope. The extra bearer gets the
            // packet slot only when the original packet transport is empty.
            if original.formats[index] == 0 {
                let mut row = original.clone();
                row.formats.push(1);
                combinations.push(row);
            }
        }
        (
            DchTfcs::new(direction, transports, combinations).unwrap(),
            id,
        )
    };
    let (uplink, ul) = extend(
        original.uplink(),
        DchDirection::Uplink,
        packet.mapping.uplink[0].transport,
    );
    let (downlink, dl) = extend(
        original.downlink(),
        DchDirection::Downlink,
        packet.mapping.downlink[0].transport,
    );
    let mut bearers = original.bearers().to_vec();
    let mut extra = packet.clone();
    extra.id = 21;
    extra.mapping = crate::test_support::rel99::mapping(ul, dl, None);
    bearers.push(extra);
    Arc::new(
        Rel99Configuration::new(uplink, downlink, bearers, original.physical().clone()).unwrap(),
    )
}
