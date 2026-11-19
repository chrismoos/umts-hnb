// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{at, count, rel99};
use umts_dsp::uplink_scrambling::UplinkScramblingCode;
use umts_rrc::{
    dedicated::ScheduledActivation,
    hsupa::{
        ActivationParameters, ControlConfiguration, DownlinkControl, ReportingPeriod,
        ScheduledFlow, SchedulingInformation, TransportConfiguration,
    },
};
use umts_types::{
    UeKey,
    channel_codes::OvsfCode,
    hsupa::{CodeLayout, Etfci, PuncturingLimit, Rel6Category, TransportBlockTable, Tti},
};

fn configuration(interval: Tti, voice: bool, control_only_power: u8) -> Arc<UplinkActivation> {
    crate::hsupa_session::build_coexistence_activation(
        rel99::configuration(voice, true, 10),
        parameters(interval, control_only_power),
        None,
        ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap(),
    )
    .unwrap()
}

fn parameters(interval: Tti, control_only_power: u8) -> ActivationParameters {
    ActivationParameters {
        e_rnti: 0x8421,
        transport: TransportConfiguration::new(
            interval,
            Rel6Category::new(6).unwrap(),
            TransportBlockTable::Zero,
            CodeLayout::new(&[16]).unwrap(),
            PuncturingLimit::new(44).unwrap(),
        )
        .unwrap(),
        control: ControlConfiguration::new(8, 20).unwrap(),
        downlink: DownlinkControl::new(
            OvsfCode::new(256, 10).unwrap(),
            OvsfCode::new(128, 6).unwrap(),
            2,
            None,
            &[],
        )
        .unwrap(),
        references: umts_rrc::hsupa::ReferencePowers::single(Etfci::new(10).unwrap(), 20).unwrap(),
        minimum: Etfci::new(0).unwrap(),
        scheduling: SchedulingInformation {
            without_grant: Some(ReportingPeriod::EveryTti),
            with_grant: None,
            power_offset: control_only_power,
        },
        rv_zero_only: false,
        flows: vec![ScheduledFlow::new(0, 0, 3).unwrap()],
    }
}

fn inputs(configuration: &UplinkActivation) -> ReceiveInputs {
    ReceiveInputs {
        identity: ReceiveIdentity {
            ue: UeKey {
                epoch: at(0).epoch,
                slot: 0,
                generation: NonZeroU64::MIN,
            },
            configuration: NonZeroU64::MIN,
        },
        code: Arc::new(
            UplinkDpchCode::new(
                UplinkScramblingCode::new(configuration.dedicated().physical().uplink_scrambling)
                    .unwrap(),
            )
            .unwrap(),
        ),
        uplink: FrameAnchor {
            time: at(1024),
            cfn: 0,
        },
        common_frame: at(0),
        control_search: crate::dedicated_rx::ControlSearchGeometry::new(
            umts_dsp::pulse_shaping::SamplesPerChip::Two,
            64,
        )
        .unwrap(),
    }
}

fn settings() -> ReceiveSettings {
    ReceiveSettings {
        likelihood: LikelihoodSettings::new(64.0).unwrap(),
        control_admission: ControlAdmission::new(0.5, 0.05).unwrap(),
        iterations: NonZeroU8::new(4).unwrap(),
        maximum_format_bits: count(100_000),
        maximum_catalog_bytes: count(64 * 1024 * 1024),
        retained_sample_bytes: count(4 * 1024 * 1024),
        soft_buffer_bytes: count(4 * 1024 * 1024),
        scratch_bytes: count(4 * 1024 * 1024),
        maximum_receive_bytes: count(128 * 1024 * 1024),
        report_capacity: count(4),
        report_bytes: count(1024 * 1024),
        packet_capacity: count(33),
        packet_bytes: count(1024 * 1024),
        reorder_hold_ttis: NonZeroU64::new(32).unwrap(),
        feedback_capacity: count(4),
        feedback_amplitude: 0.1,
    }
}

#[test]
fn dedicated_bearer_builders_reject_reserved_ccch_flow() {
    let dedicated = rel99::configuration(false, true, 10);
    for edch_only in [false, true] {
        let mut parameters = parameters(Tti::TwoMilliseconds, 0);
        parameters.flows = vec![ScheduledFlow::new(7, 0, 3).unwrap()];
        let construct = if edch_only {
            crate::hsupa_session::build_edch_only_activation
        } else {
            crate::hsupa_session::build_coexistence_activation
        };
        assert!(
            construct(
                dedicated.clone(),
                parameters,
                None,
                ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap()
            )
            .is_err()
        );
    }
}

#[test]
fn complete_edch_only_catalog_retains_signalling_and_prebuilt_control_resources() {
    let dedicated = rel99::configuration(false, true, 10);
    let mut parameters = parameters(Tti::TwoMilliseconds, 0);
    parameters.transport = TransportConfiguration::new(
        Tti::TwoMilliseconds,
        Rel6Category::new(6).unwrap(),
        TransportBlockTable::Zero,
        CodeLayout::new(&[2, 2, 4, 4]).unwrap(),
        PuncturingLimit::new(44).unwrap(),
    )
    .unwrap();
    let configuration = crate::hsupa_session::build_edch_only_activation(
        dedicated.clone(),
        parameters,
        None,
        ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(configuration.maximum_dpdch(), 0);
    assert_eq!(configuration.mappings().len(), dedicated.bearers().len());
    for bearer in dedicated.bearers() {
        let mapping = configuration
            .mappings()
            .iter()
            .find(|mapping| mapping.bearer == bearer.id)
            .unwrap();
        let umts_rrc_schema::information_elements::ULLogicalChannelMappingsR6::oneLogicalChannel(
            wire,
        ) = mapping.uplink.wire().unwrap()
        else {
            panic!("single retained logical channel");
        };
        assert_eq!(
            wire.mac_logical_channel_priority.0,
            bearer.mapping.uplink[0].priority
        );
    }
    let input = inputs(&configuration);
    let mut budget = settings();
    budget.packet_capacity = count(129);
    budget.packet_bytes = count(8 * 1024 * 1024);
    let mut resources = ReceiveResources::build(configuration.clone(), input, budget).unwrap();
    let prepared_bytes = resources.receiver.retained_bytes().unwrap();
    assert!(resources.receiver.prepare_initial_suffixes().is_err());
    assert_eq!(resources.receiver.retained_bytes().unwrap(), prepared_bytes);
    assert!(resources.receiver.discard_collection_buffer());
    let suffix_bytes = resources.receiver.retained_bytes().unwrap();
    let mut suffix_budget = budget;
    suffix_budget.maximum_receive_bytes = count(suffix_bytes);
    let mut suffix_fallback =
        ReceiveResources::build(configuration.clone(), inputs(&configuration), suffix_budget)
            .unwrap();
    assert_eq!(
        suffix_fallback.receiver.retained_bytes().unwrap(),
        suffix_bytes
    );
    assert!(!suffix_fallback.receiver.discard_collection_buffer());
    assert!(suffix_fallback.receiver.discard_initial_suffixes());
    assert!(resources.receiver.discard_initial_suffixes());
    let legacy_bytes = resources.receiver.retained_bytes().unwrap();
    let mut tight = budget;
    tight.maximum_receive_bytes = count(legacy_bytes);
    let mut fallback =
        ReceiveResources::build(configuration.clone(), inputs(&configuration), tight).unwrap();
    assert_eq!(fallback.receiver.retained_bytes().unwrap(), legacy_bytes);
    assert!(!fallback.receiver.discard_initial_suffixes());
    assert!(!fallback.receiver.discard_collection_buffer());
    assert_eq!(resources.receiver.receiver().maximum_dpdch(), 0);
    assert!(
        resources.receiver.retained_bytes().unwrap()
            > resources.receiver.receiver().retained_bytes().unwrap()
    );
}

#[test]
fn reorder_hold_uses_the_packet_delivery_chip_clock() {
    let mut settings = settings();
    assert_eq!(
        settings
            .reorder_hold_chips(Tti::TwoMilliseconds)
            .unwrap()
            .get(),
        245_760
    );
    assert_eq!(
        settings
            .reorder_hold_chips(Tti::TenMilliseconds)
            .unwrap()
            .get(),
        1_228_800
    );
    settings.reorder_hold_ttis = NonZeroU64::new(u64::MAX).unwrap();
    assert!(settings.reorder_hold_chips(Tti::TwoMilliseconds).is_err());
}

#[test]
fn asynchronous_preparation_retimes_only_the_exact_immutable_definition() {
    use crate::hsupa_session::receive_preparation::{PreparationWorker, Request};
    let initial = configuration(Tti::TwoMilliseconds, false, 0);
    let identity = inputs(&initial).identity;
    let mut worker = PreparationWorker::new().unwrap();
    assert!(
        worker
            .submit(Request {
                configuration: initial.clone(),
                inputs: inputs(&initial),
                settings: settings(),
            })
            .is_ok()
    );
    let rejected = worker
        .submit(Request {
            configuration: initial.clone(),
            inputs: inputs(&initial),
            settings: settings(),
        })
        .expect_err("single in-flight construction");
    assert_eq!(
        rejected.error,
        crate::hsupa_session::receive_preparation::Error::Busy
    );
    assert!(Arc::ptr_eq(&rejected.request.configuration, &initial));
    let began = std::time::Instant::now();
    let prepared = loop {
        worker.poll().unwrap();
        if let Some(prepared) = worker.take() {
            break prepared;
        }
        assert!(began.elapsed() < std::time::Duration::from_secs(10));
        std::thread::yield_now();
    };
    assert_eq!(prepared.identity, identity);
    assert!(worker.identity().is_none());
    let mut resources = prepared.resources.unwrap();
    let format_count = resources.format_count();
    let later = Arc::new(
        initial
            .rescheduled(
                ScheduledActivation::new(
                    at(4 * umts_types::time::CHIPS_PER_FRAME),
                    4,
                    NonZeroU8::new(4).unwrap(),
                )
                .unwrap(),
            )
            .unwrap(),
    );
    resources.reschedule(later.clone()).unwrap();
    assert!(Arc::ptr_eq(resources.configuration(), &later));
    assert_eq!(
        resources.ingress.activation(),
        at(later.activation().time().index + inputs(&later).uplink.time.index)
    );
    assert_eq!(resources.format_count(), format_count);
    assert!(
        resources
            .reschedule(configuration(Tti::TwoMilliseconds, false, 0))
            .is_err()
    );
    assert!(Arc::ptr_eq(resources.configuration(), &later));
}

#[test]
fn receiver_catalog_and_packet_ingress_derive_from_the_same_rrc_construction() {
    for interval in [Tti::TwoMilliseconds, Tti::TenMilliseconds] {
        for voice in [false, true] {
            let configuration = configuration(interval, voice, 0);
            let inputs = inputs(&configuration);
            let code = inputs.code.clone();
            let identity = inputs.identity;
            let resources =
                ReceiveResources::build(configuration.clone(), inputs, settings()).unwrap();
            assert!(Arc::ptr_eq(&resources.configuration, &configuration));
            assert_eq!(resources.receiver.identity(), identity);
            assert_eq!(resources.receiver.receiver().interval(), interval);
            assert!(resources.receiver.receiver().uses_code(&code));
            assert!(resources.receiver.receiver().has_output());
            assert!(!resources.receiver.receiver().is_started());
            assert_eq!(resources.ingress.identity(), identity);
            assert_eq!(
                resources.ingress.activation(),
                at(configuration.activation().time().index
                    + resources.receiver.receiver().anchor().time.index)
            );
            assert_eq!(resources.ingress.interval(), interval);
            assert!(resources.format_count > 1);
            assert!(resources.catalog_bytes <= settings().maximum_catalog_bytes.get());
            assert!(
                resources.receiver.retained_bytes().unwrap()
                    <= resources.maximum_receive_bytes.get()
            );
        }
    }
}

#[test]
fn receiver_build_rejects_wrong_code_budget_and_unmodelled_power_profiles() {
    let configuration = configuration(Tti::TwoMilliseconds, false, 0);
    let mut wrong = inputs(&configuration);
    wrong.code = Arc::new(
        UplinkDpchCode::new(
            UplinkScramblingCode::new(configuration.dedicated().physical().uplink_scrambling ^ 1)
                .unwrap(),
        )
        .unwrap(),
    );
    assert!(ReceiveResources::build(configuration.clone(), wrong, settings()).is_err());
    let mut small = settings();
    small.maximum_catalog_bytes = count(1);
    assert!(ReceiveResources::build(configuration.clone(), inputs(&configuration), small).is_err());
    let mut small = settings();
    small.maximum_receive_bytes = count(1);
    assert!(ReceiveResources::build(configuration.clone(), inputs(&configuration), small).is_err());
    let different = self::configuration(Tti::TwoMilliseconds, false, 1);
    assert!(ReceiveResources::build(different.clone(), inputs(&different), settings()).is_err());
}

#[test]
fn receiver_build_accepts_multiple_signalled_references() {
    use umts_rrc::hsupa::{PowerReference, ReferencePowers};
    let mut parameters = parameters(Tti::TwoMilliseconds, 0);
    parameters.references = ReferencePowers::new(vec![
        PowerReference {
            format: Etfci::new(0).unwrap(),
            power_index: 4,
        },
        PowerReference {
            format: Etfci::new(10).unwrap(),
            power_index: 20,
        },
    ])
    .unwrap();
    let configuration = crate::hsupa_session::build_coexistence_activation(
        rel99::configuration(false, true, 10),
        parameters,
        None,
        ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap(),
    )
    .unwrap();
    let resources =
        ReceiveResources::build(configuration.clone(), inputs(&configuration), settings()).unwrap();
    assert!(resources.format_count() > 1);
    assert_eq!(
        resources
            .configuration()
            .parameters()
            .references
            .as_slice()
            .len(),
        2
    );
}

#[test]
fn session_assembly_returns_every_owner_on_rejection_and_can_retry() {
    use crate::hsupa_session::{Phase, SessionAssemblyError};
    use umts_rrc::{establishment::TransactionId, hsupa::ActivationProcedure};
    use umts_types::time::CHIPS_PER_FRAME;
    let source = crate::test_support::source(true);
    let crate::hsupa_tx::tests::ConfiguredUsers {
        resources: mut pool,
        mut bank,
        radios,
        identities,
        configurations,
    } = crate::hsupa_tx::tests::setup_with_configurations(&source);
    bank.begin_retirement(identities[0].ue).unwrap();
    let lease = bank.take_retired(identities[0].ue, at(0)).unwrap().unwrap();
    let mut parameters = parameters(Tti::TwoMilliseconds, 0);
    parameters.e_rnti = lease.e_rnti();
    parameters.downlink = lease.controls();
    let configuration = crate::hsupa_session::build_coexistence_activation(
        configurations[0].clone(),
        parameters,
        None,
        ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap(),
    )
    .unwrap();
    let mut inputs = inputs(&configuration);
    inputs.identity = identities[0];
    let receive = ReceiveResources::build(configuration.clone(), inputs, settings()).unwrap();
    let procedure = Box::new(
        ActivationProcedure::new(
            identities[0].ue,
            TransactionId::new(0).unwrap(),
            configuration.clone(),
            at(0),
            at(CHIPS_PER_FRAME),
            NonZeroU64::new(10 * CHIPS_PER_FRAME).unwrap(),
        )
        .unwrap(),
    );
    let rejected = receive
        .into_session(lease, procedure, u8::MAX, u8::MAX, 0.1, None)
        .err()
        .expect("invalid grant returned owners");
    assert!(matches!(rejected.error, SessionAssemblyError::Grant(_)));
    assert!(Arc::ptr_eq(
        rejected.resources.configuration(),
        &configuration
    ));
    assert_eq!(rejected.lease.key(), identities[0].ue);
    assert_eq!(rejected.procedure.key(), identities[0].ue);
    let RejectedReceiveSession {
        resources,
        lease,
        procedure,
        ..
    } = *rejected;
    let mut session = resources
        .into_session(lease, procedure, 10, 10, 0.1, None)
        .unwrap_or_else(|_| panic!("valid assembly"));
    assert_eq!(session.phase(), Phase::Preparing);
    session.abort_unpublished(at(0)).unwrap();
    assert_eq!(session.phase(), Phase::Cancelled);
    let (_, lease) = (*session)
        .into_cancelled()
        .unwrap_or_else(|_| panic!("local ownership"));
    pool.release_hsupa_user(lease).unwrap();
    bank.begin_retirement(identities[1].ue).unwrap();
    pool.release_hsupa_user(bank.take_retired(identities[1].ue, at(0)).unwrap().unwrap())
        .unwrap();
    for radio in radios {
        pool.release(radio).unwrap();
    }
    pool.release_hsupa(bank.release().unwrap_or_else(|_| panic!("empty bank")))
        .unwrap();
    assert_eq!(pool.outstanding(), 0);
}
