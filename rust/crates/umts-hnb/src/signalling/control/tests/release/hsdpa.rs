// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::signalling::HspaCommitError;
use umts_rrc::hsdpa::{
    AckFeedback, ActivationParameters, ActivationPhase, ActivationProcedure, ActivationResponse,
    ControlChannels, FeedbackCycle, HarqConfiguration, InitialActivation, MappedBearer,
    MeasurementFeedback, QueueConfiguration, QueueSet,
};
use umts_types::{channel_codes::OvsfCode, hsdpa::Category};

mod dual_admission;
mod dual_lifecycle;
mod radio_release;

#[test]
fn fifteen_code_seven_process_catalog_fits_trial_budget() {
    use crate::hsdpa_session::{
        CellSchedulingBudget, DownlinkPower, SchedulingBudget, SchedulingPreparation,
        SchedulingSettings,
    };
    use crate::test_support::count;

    let c = connected();
    let construction = activation_with_processes(&c, 7).construction().clone();
    let source = crate::test_support::source(true);
    let mut resources = crate::radio_resources::RadioResources::new(
        c.key.epoch,
        source.information(),
        count(2),
        0..=1,
    )
    .unwrap();
    let lease = resources
        .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::MIN)
        .unwrap();
    let config = umts_config::Config::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../rust/test-support/fixtures/config/hsdpa-fifteen-code.yaml"),
    )
    .unwrap();
    let configured = config.hspa.downlink.resources.scheduling.unwrap();
    let cell = CellSchedulingBudget::new(count(configured.maximum_cell_bytes as usize));
    let prepare = |maximum_plan_bytes| {
        SchedulingPreparation::new(
            construction.configuration().clone(),
            *lease.codes(),
            SchedulingSettings {
                power: DownlinkPower::new(0.1, 0.1).unwrap(),
                maximum_cqi_age: NonZeroU64::new(CHIPS_PER_FRAME).unwrap(),
                selection_budget: count(65536),
                cqi_offset_limit: 0,
                cqi_target_bler_percent: 10,
            },
            SchedulingBudget {
                maximum_formats: count(65536),
                maximum_coded_bits: count(100_000),
                maximum_plan_bytes: count(maximum_plan_bytes),
                maximum_buffer_bytes: count(4_000_000),
                maximum_encoder_bytes: count(16_000_000),
                maximum_total_bytes: count(256_000_000),
            },
            &cell,
        )
        .unwrap_or_else(|error| panic!("{error:?}"))
    };
    let prepared = prepare(configured.maximum_plan_bytes as usize)
        .build()
        .unwrap_or_else(|error| panic!("{error:?}"));
    assert!(prepared.retained_bytes() <= configured.maximum_ue_bytes as usize);
    // The per-UE plan budget covers plan state and control words. The source tables are
    // shared and charged to the cell.
    let minimum_plan_bytes = prepared.plan_formats().count()
        * std::mem::size_of::<(
            crate::hsdpa_packet::PlanFormat,
            umts_dsp::hsdpa::TransmissionPlan,
        )>()
        + std::mem::size_of::<crate::hsdpa_packet::PlanBank>();
    assert!(cell.tables().retained_bytes() > minimum_plan_bytes * 10);
    drop(prepared);
    assert!(matches!(
        prepare(minimum_plan_bytes - 1).build(),
        Err(crate::hsdpa_session::Error::Rate(
            umts_dsp::DspError::Capacity
        ))
    ));
    assert_eq!(
        cell.available_bytes() + cell.tables().retained_bytes(),
        configured.maximum_cell_bytes as usize
    );
}

#[test]
fn fifteen_code_six_process_catalog_fits_trial_budget_with_full_rate() {
    use crate::hsdpa_session::{
        CellSchedulingBudget, DownlinkPower, SchedulingBudget, SchedulingPreparation,
        SchedulingSettings,
    };
    use crate::test_support::count;

    let c = connected();
    let construction = activation_with_processes(&c, 6).construction().clone();
    let source = crate::test_support::source(true);
    let mut resources = crate::radio_resources::RadioResources::new(
        c.key.epoch,
        source.information(),
        count(2),
        0..=1,
    )
    .unwrap();
    let lease = resources
        .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::MIN)
        .unwrap();
    let config = umts_config::Config::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../rust/test-support/fixtures/config/hsdpa-fifteen-code.yaml"),
    )
    .unwrap();
    let configured = config.hspa.downlink.resources.scheduling.unwrap();
    let cell = CellSchedulingBudget::new(count(configured.maximum_cell_bytes as usize));
    let prepared = SchedulingPreparation::new(
        construction.configuration().clone(),
        *lease.codes(),
        SchedulingSettings {
            power: DownlinkPower::new(0.1, 0.1).unwrap(),
            maximum_cqi_age: NonZeroU64::new(CHIPS_PER_FRAME).unwrap(),
            selection_budget: count(65536),
            cqi_offset_limit: 0,
            cqi_target_bler_percent: 10,
        },
        SchedulingBudget {
            maximum_formats: count(65536),
            maximum_coded_bits: count(100_000),
            maximum_plan_bytes: count(configured.maximum_plan_bytes as usize),
            maximum_buffer_bytes: count(4_000_000),
            maximum_encoder_bytes: count(16_000_000),
            maximum_total_bytes: count(256_000_000),
        },
        &cell,
    )
    .unwrap_or_else(|error| panic!("{error:?}"))
    .build()
    .unwrap_or_else(|error| panic!("{error:?}"));
    assert!(prepared.retained_bytes() <= configured.maximum_ue_bytes as usize);
    // Six partitions of Category 10 memory equal the CQI reference buffer, so no second-stage
    // repetition is needed.
    let sizes: Vec<_> = prepared
        .plan_formats()
        .map(|format| (format.transport_bits.get(), format.codes))
        .collect();
    assert!(
        sizes.contains(&(25_558, 15)),
        "CQI 30 block missing: {sizes:?}"
    );
    drop(prepared);
    assert_eq!(
        cell.available_bytes() + cell.tables().retained_bytes(),
        configured.maximum_cell_bytes as usize
    );
}

#[test]
fn session_admission_preserves_rejected_resources_and_gates_packets() {
    use crate::{
        hsdpa_rx::FeedbackReceiver,
        hsdpa_session::{Error, Phase, QueueBudget, Session},
    };
    use umts_dsp::{dedicated::UplinkDpchCode, uplink_scrambling::UplinkScramblingCode};

    let c = connected();
    let source = crate::test_support::source(true);
    let mut resources = crate::radio_resources::RadioResources::new(
        c.key.epoch,
        source.information(),
        NonZeroUsize::new(2).unwrap(),
        0..=1,
    )
    .unwrap();
    let lease = resources
        .reserve_hsdpa(NonZeroU8::new(2).unwrap(), NonZeroU8::MIN)
        .unwrap();
    let code = Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(0x123456).unwrap()).unwrap());
    let mut receiver = FeedbackReceiver::new(c.key, code, 1, at(0)).unwrap();
    let feedback = receiver
        .connect(NonZeroUsize::MIN, NonZeroUsize::MIN)
        .unwrap();
    let procedure = Box::new(activation(&c));
    let construction = procedure.construction().clone();
    let mut wrong_identity = c.old;
    wrong_identity.ue.generation = NonZeroU64::new(c.key.generation.get() + 1).unwrap();
    let budget = QueueBudget {
        pdus_per_queue: NonZeroUsize::new(8).unwrap(),
        total_bytes: NonZeroUsize::new(65536).unwrap(),
    };
    let rejected = match Session::new(
        wrong_identity,
        procedure,
        feedback,
        crate::hsdpa_session::SessionSettings {
            incarnation: NonZeroU64::MIN,
            downlink_origin: at(0),
            queues: budget,
            codes: *lease.codes(),
            scheduling: None,
        },
    ) {
        Ok(_) => panic!("wrong UE accepted"),
        Err(rejected) => rejected,
    };
    assert_eq!(rejected.error, Error::Identity);
    assert!(Arc::ptr_eq(
        rejected.procedure.construction(),
        &construction
    ));
    assert!(rejected.feedback.available());
    let count = |value| NonZeroUsize::new(value).unwrap();
    let limits = crate::hsdpa_rx::EvidenceLimits::new(0.5, 0.1, 0.5, 1.0, 0.01).unwrap();
    let mut scheduling = crate::hsdpa_session::SchedulingAdmission {
        settings: crate::hsdpa_session::SchedulingSettings {
            power: crate::hsdpa_session::DownlinkPower::new(0.1, 0.1).unwrap(),
            maximum_cqi_age: NonZeroU64::new(CHIPS_PER_FRAME).unwrap(),
            selection_budget: count(1024),
            cqi_offset_limit: 0,
            cqi_target_bler_percent: 10,
        },
        budget: crate::hsdpa_session::SchedulingBudget {
            maximum_formats: count(1),
            maximum_coded_bits: count(100_000),
            maximum_plan_bytes: count(16_000_000),
            maximum_buffer_bytes: count(2_000_000),
            maximum_encoder_bytes: count(1_000_000),
            maximum_total_bytes: count(16_000_000),
        },
        feedback: crate::hsdpa_rx::FeedbackPolicy {
            acknowledgement: limits,
            cqi: limits,
        },
        cell: crate::hsdpa_session::CellSchedulingBudget::new(count(32_000_000)),
    };
    let rejected = match Session::new(
        c.old,
        rejected.procedure,
        rejected.feedback,
        crate::hsdpa_session::SessionSettings {
            incarnation: NonZeroU64::MIN,
            downlink_origin: at(0),
            queues: budget,
            codes: *lease.codes(),
            scheduling: Some(&scheduling),
        },
    ) {
        Ok(_) => panic!("insufficient plan budget accepted"),
        Err(rejected) => rejected,
    };
    assert_eq!(rejected.error, Error::Capacity);
    // Shared HSDPA plan tables stay with the cell.
    assert_eq!(
        scheduling.cell.available_bytes() + scheduling.cell.tables().retained_bytes(),
        32_000_000
    );
    assert!(Arc::ptr_eq(
        rejected.procedure.construction(),
        &construction
    ));
    assert!(rejected.feedback.available());
    scheduling.budget.maximum_formats = count(1024);
    let preparation = crate::hsdpa_session::SchedulingPreparation::new(
        rejected.procedure.construction().configuration().clone(),
        *lease.codes(),
        scheduling.settings,
        scheduling.budget,
        &scheduling.cell,
    )
    .unwrap_or_else(|error| panic!("{error:?}"));
    assert_eq!(scheduling.cell.available_bytes(), 16_000_000);
    drop(preparation);
    // Shared HSDPA plan tables stay with the cell.
    assert_eq!(
        scheduling.cell.available_bytes() + scheduling.cell.tables().retained_bytes(),
        32_000_000
    );
    let preparation = crate::hsdpa_session::SchedulingPreparation::new(
        rejected.procedure.construction().configuration().clone(),
        *lease.codes(),
        scheduling.settings,
        scheduling.budget,
        &scheduling.cell,
    )
    .unwrap_or_else(|error| panic!("{error:?}"));
    let prepared = preparation
        .build()
        .unwrap_or_else(|error| panic!("{error:?}"));
    let mut session = Session::new_prepared(
        c.old,
        rejected.procedure,
        rejected.feedback,
        crate::hsdpa_session::SessionSettings {
            incarnation: NonZeroU64::MIN,
            downlink_origin: at(0),
            queues: budget,
            codes: *lease.codes(),
            scheduling: None,
        },
        Some((prepared, scheduling.feedback)),
    )
    .unwrap_or_else(|rejected| panic!("{:?}", rejected.error));
    assert_eq!(session.phase(), Phase::Prepared);
    assert_eq!(session.begin_radio_change(c.new), Err(Error::Phase));
    assert_eq!(session.drain_radio_change(count(1)), Err(Error::Phase));
    assert!(scheduling.cell.available_bytes() < 32_000_000);
    assert!(!session.scheduler_ready());
    assert!(matches!(session.active_packets(), Err(Error::Phase)));
    assert!(matches!(session.retiring_packets(), Err(Error::Phase)));
    session.begin_retirement();
    assert_eq!(session.phase(), Phase::Retiring);
    assert!(session.retiring_packets().is_ok());
    assert!(matches!(session.active_packets(), Err(Error::Phase)));
    let step = session.drain_retirement(count(1)).unwrap();
    assert_eq!(step.processes, 1);
    assert_eq!(step.reports, 0);
    let mut session = match Box::new(session).try_retire() {
        Err(session) => session,
        Ok(_) => panic!("missing TX retirement receipt accepted"),
    };
    let (mut runtime, mut ports) = crate::dedicated_tx::DedicatedTransmitRuntime::new(
        &source,
        count(2),
        count(4),
        count(100_000),
        crate::dedicated_tx::TransmitRuntimeLimits {
            command_capacity: count(4),
            commands_per_step: count(4),
            feedback_capacity: count(4),
            feedback_per_step: count(4),
            maximum_frame_payload_bits: count(100_000),
            maximum_inflight_retained_bits: count(100_000),
        },
    )
    .unwrap();
    let bank = crate::hsdpa_tx::TransmitBank::new(&source, lease, count(2), count(1_000_000))
        .unwrap_or_else(|_| panic!("shared TX bank"));
    runtime
        .attach_hsdpa(bank)
        .unwrap_or_else(|_| panic!("paired TX bank"));
    let mut commands = crate::hsdpa_packet::control::Commands::new(count(4)).unwrap();
    let peers: Vec<_> = (1..=crate::hsdpa_tx::MAX_PENDING)
        .map(|offset| umts_types::UeKey {
            slot: c.key.slot + u32::try_from(offset).unwrap(),
            ..c.key
        })
        .collect();
    for &peer in &peers {
        runtime.hsdpa_mut().unwrap().begin_retirement(peer).unwrap();
    }
    session
        .service_retirement(&mut commands.port(&mut ports.control))
        .unwrap();
    runtime.service().unwrap();
    assert!(commands.route(ports.control.try_event().unwrap()).is_none());
    let result = commands.port(&mut ports.control).take_result().unwrap();
    assert!(matches!(
        session.accept_command(result),
        Ok(crate::hsdpa_session::CommandProgress::RetirementBackpressure)
    ));
    for peer in peers {
        assert!(
            runtime
                .hsdpa_mut()
                .unwrap()
                .finish_retirement(peer)
                .unwrap()
        );
    }
    for _ in 0..2 {
        session
            .service_retirement(&mut commands.port(&mut ports.control))
            .unwrap();
        runtime.service().unwrap();
        let event = ports.control.try_event().expect("retirement reply");
        assert!(commands.route(event).is_none());
        let result = commands.port(&mut ports.control).take_result().unwrap();
        session
            .accept_command(result)
            .unwrap_or_else(|_| panic!("matched retirement reply"));
    }
    let retired = session
        .try_retire()
        .unwrap_or_else(|_| panic!("settled retirement"));
    assert!(scheduling.cell.available_bytes() < 32_000_000);
    drop(retired);
    // Shared HSDPA plan tables stay with the cell.
    assert_eq!(
        scheduling.cell.available_bytes() + scheduling.cell.tables().retained_bytes(),
        32_000_000
    );
    runtime.close();
    let (lease, pending) = runtime.take_closed_hsdpa().unwrap().into_retained();
    assert_eq!(pending.count(), 0);
    resources.release_hsdpa(lease).unwrap();
}

fn activation(c: &Connected) -> ActivationProcedure {
    activation_with_processes(c, 6)
}

fn activation_with_processes(c: &Connected, processes: u8) -> ActivationProcedure {
    let mut sizes = [None; 8];
    sizes[0] = NonZeroUsize::new(320);
    let configuration = Arc::new(
        InitialActivation::new(
            c.procedure.previous().clone(),
            ActivationParameters {
                h_rnti: 0x8421,
                harq: HarqConfiguration::implicit(
                    Category::new(10).unwrap(),
                    NonZeroU8::new(processes).unwrap(),
                )
                .unwrap(),
                queues: QueueSet::new(vec![QueueConfiguration::new(0, 0, sizes, 100, 32).unwrap()])
                    .unwrap(),
                control: ControlChannels::new(&[OvsfCode::new(128, 12).unwrap()]).unwrap(),
                measurement: MeasurementFeedback::new(6, FeedbackCycle::Ms2, NonZeroU8::MIN, 8)
                    .unwrap(),
                acknowledgement: AckFeedback::new(8, 8, NonZeroU8::MIN).unwrap(),
            },
            vec![MappedBearer {
                bearer: 20,
                flow: 0,
                logical: None,
            }],
            c.procedure.activation(),
        )
        .unwrap(),
    );
    ActivationProcedure::new(
        c.key,
        c.radio.next_hspa_activation_transaction(c.key).unwrap(),
        configuration,
        at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
        at((RELEASE_PUBLICATION_FRAME + 1) * CHIPS_PER_FRAME),
        NonZeroU64::new(8 * CHIPS_PER_FRAME).unwrap(),
    )
    .unwrap()
}

fn complete(transaction: TransactionId) -> ULDCCHMessageType {
    ULDCCHMessageType::radioBearerReconfigurationComplete(
        pdu::RadioBearerReconfigurationComplete::new(
            ie::RRCTransactionIdentifier(transaction.get()),
            None,
            None,
            None,
            None,
            None,
            None,
        ),
    )
}

#[test]
fn protected_hspa_activation_retains_rel99_route_and_commits_once_after_admission() {
    let c = connected();
    let procedure = activation(&c);
    protected_activation(c, procedure);
}

fn protected_activation<C: crate::signalling::HspaConfiguration>(
    mut c: Connected,
    mut procedure: umts_rrc::dedicated::ActivationProcedure<C>,
) {
    let receipt = c
        .radio
        .publish_hspa_activation_on(
            &mut procedure,
            c.old,
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
        )
        .unwrap();
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
    assert_eq!(
        c.radio.publish_bearer_release_on(
            &mut c.procedure,
            c.old,
            c.new,
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
        ),
        Err(SignallingError::Capacity)
    );
    publish_release(&mut c, receipt);
    let provenance = ReceiveProvenance::Path(c.old.configuration);
    receive(&mut c, complete(procedure.transaction()), provenance);
    let now = CHIPS_PER_FRAME * RELEASE_ACTIVATION_FRAME;
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(now + 20),
            security_time(),
            |message, mut security| {
                message
                    .hspa_activation_response_with(&mut procedure, &mut security, |_, _| {
                        Err::<(), _>(SignallingError::Capacity)
                    })
                    .map(|_| ())
            }
        ),
        Some(Err(SignallingError::Capacity))
    );
    assert_eq!(procedure.phase(), ActivationPhase::AwaitingComplete);
    let mut accepted = None;
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(now + 30),
            security_time(),
            |message, mut security| {
                let (response, value) = message.hspa_activation_response_with(
                    &mut procedure,
                    &mut security,
                    |counters, _| {
                        assert_eq!(counters.domain_starts(), Default::default());
                        assert_eq!(counters.transparent_cfn(), None);
                        Ok(7)
                    },
                )?;
                assert_eq!(response, ActivationResponse::Complete);
                accepted = value;
                Ok(())
            }
        ),
        Some(Ok(c.key))
    );
    let (accepted, result) = accepted.unwrap();
    assert_eq!(result, 7);
    assert_eq!(accepted.identity(), c.old);
    assert!(Arc::ptr_eq(
        accepted.configuration(),
        procedure.construction()
    ));
    assert_eq!(accepted.time().received(), at(now));
    assert!(c.radio.release_hspa_activation_receipt(&accepted).is_err());
    assert_eq!(
        c.radio
            .commit_hspa_activation_with(&accepted, |_| Err::<(), _>("busy")),
        Err(HspaCommitError::Commit("busy"))
    );
    assert_eq!(
        c.radio
            .commit_hspa_activation_with(&accepted, |_| Ok::<_, ()>(9)),
        Ok(9)
    );
    assert_eq!(
        c.radio
            .commit_hspa_activation_with(&accepted, |_| -> Result<(), ()> {
                panic!("committed once")
            }),
        Err(HspaCommitError::Signalling(SignallingError::Phase))
    );
    acknowledge(&mut c.radio, c.key, &mut c.sender, receipt, now + 100);
    c.radio.release_hspa_activation_receipt(&accepted).unwrap();
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
    assert!(c.radio.registry.get(c.key).unwrap().bearer_change.is_none());
    assert_ne!(
        c.radio.next_hspa_activation_transaction(c.key).unwrap(),
        procedure.transaction()
    );
}

fn uplink_activation(c: &Connected) -> umts_rrc::hsupa::ActivationProcedure {
    use umts_rrc::hsupa as ul;
    use umts_types::hsupa::{
        CodeLayout, Etfci, PuncturingLimit, Rel6Category, TransportBlockTable, Tti,
    };
    let configuration = Arc::new(
        ul::UplinkActivation::coexistence(
            c.procedure.previous().clone(),
            ul::ActivationParameters {
                e_rnti: 0x8421,
                transport: ul::TransportConfiguration::new(
                    Tti::TwoMilliseconds,
                    Rel6Category::new(6).unwrap(),
                    TransportBlockTable::Zero,
                    CodeLayout::new(&[2, 2]).unwrap(),
                    PuncturingLimit::new(44).unwrap(),
                )
                .unwrap(),
                control: ul::ControlConfiguration::new(8, 20).unwrap(),
                downlink: ul::DownlinkControl::new(
                    OvsfCode::new(256, 10).unwrap(),
                    OvsfCode::new(128, 6).unwrap(),
                    2,
                    None,
                    &[],
                )
                .unwrap(),
                references: ul::ReferencePowers::single(Etfci::new(50).unwrap(), 20).unwrap(),
                minimum: Etfci::new(0).unwrap(),
                scheduling: ul::SchedulingInformation {
                    without_grant: Some(ul::ReportingPeriod::EveryTti),
                    with_grant: None,
                    power_offset: 0,
                },
                rv_zero_only: false,
                flows: vec![ul::ScheduledFlow::new(0, 0, 3).unwrap()],
            },
            vec![ul::MappedBearer {
                bearer: 20,
                uplink: ul::UplinkMapping::new(1, 0, 1, 320, 1, true).unwrap(),
                downlink: ul::RetainedDownlink::Dedicated,
            }],
            c.procedure.activation(),
            false,
        )
        .unwrap(),
    );
    umts_rrc::hsupa::ActivationProcedure::new(
        c.key,
        c.radio.next_hspa_activation_transaction(c.key).unwrap(),
        configuration,
        at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
        at((RELEASE_PUBLICATION_FRAME + 1) * CHIPS_PER_FRAME),
        NonZeroU64::new(8 * CHIPS_PER_FRAME).unwrap(),
    )
    .unwrap()
}

#[test]
fn protected_hsupa_activation_uses_the_same_security_and_one_time_commit() {
    let c = connected();
    let procedure = uplink_activation(&c);
    protected_activation(c, procedure);
}

#[test]
fn pending_downlink_activation_cannot_admit_an_uplink_completion() {
    let mut c = connected();
    let mut downlink = activation(&c);
    let mut uplink = uplink_activation(&c);
    let receipt = c
        .radio
        .publish_hspa_activation_on(
            &mut downlink,
            c.old,
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
        )
        .unwrap();
    publish_release(&mut c, receipt);
    let provenance = ReceiveProvenance::Path(c.old.configuration);
    receive(&mut c, complete(downlink.transaction()), provenance);
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(RELEASE_ACTIVATION_FRAME * CHIPS_PER_FRAME + 20),
            security_time(),
            |message, mut security| message
                .hspa_activation_response_with(
                    &mut uplink,
                    &mut security,
                    |_, _| -> Result<(), SignallingError> { panic!("wrong construction admitted") },
                )
                .map(|_| ()),
        ),
        Some(Err(SignallingError::Mapping))
    );
    assert_eq!(downlink.phase(), ActivationPhase::AwaitingComplete);
    assert_eq!(uplink.phase(), ActivationPhase::Prepared);
}

#[test]
fn activation_rejects_wrong_radio_provenance_and_timeout_requires_explicit_recovery() {
    let mut c = connected();
    let mut procedure = activation(&c);
    let receipt = c
        .radio
        .publish_hspa_activation_on(
            &mut procedure,
            c.old,
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
        )
        .unwrap();
    publish_release(&mut c, receipt);
    let provenance = ReceiveProvenance::Path(c.new.configuration);
    receive(&mut c, complete(procedure.transaction()), provenance);
    let now = CHIPS_PER_FRAME * RELEASE_ACTIVATION_FRAME;
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(now + 20),
            security_time(),
            |message, mut security| {
                message
                    .hspa_activation_response_with(
                        &mut procedure,
                        &mut security,
                        |_, _| -> Result<(), SignallingError> {
                            panic!("foreign radio generation")
                        },
                    )
                    .map(|_| ())
            }
        ),
        Some(Err(SignallingError::Mapping))
    );
    assert_eq!(procedure.phase(), ActivationPhase::AwaitingComplete);
    assert!(
        c.radio
            .require_hspa_activation_recovery(&procedure)
            .is_err()
    );
    assert!(procedure.expire(at(24 * CHIPS_PER_FRAME)).unwrap());
    c.radio
        .require_hspa_activation_recovery(&procedure)
        .unwrap();
    assert!(
        c.radio
            .registry
            .get(c.key)
            .unwrap()
            .protection
            .blocks_status()
    );
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
    assert!(c.radio.registry.get(c.key).unwrap().bearer_change.is_some());
}
