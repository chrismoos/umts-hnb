// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::rel99_planner::{InitialOwnerSettings, OwnerBuildError, PreparedExisting};
use std::num::NonZeroU8;
use umts_dsp::{coding::DecodeBudget, dedicated_transport::TransportPlanLimits};
use umts_rrc::dedicated::ScheduledActivation;
use umts_rrc_codec::decode_downlink_dedicated;
use umts_rrc_schema::class_definitions::DLDCCHMessageType;

mod scheduled;

fn owner_settings(
    env: &InitialEnvironment,
    activation: ScheduledActivation,
    now: ChipInstant,
) -> InitialOwnerSettings {
    let mut settings = owners::settings(env.key.epoch);
    let frame = activation.time().index / CHIPS_PER_FRAME;
    settings.timing.uplink.time = activation.time();
    settings.timing.uplink.cfn = activation.cfn();
    settings.timing.uplink_frame = frame;
    settings.timing.downlink = settings.timing.uplink;
    settings.timing.downlink_frame = frame;
    settings.samples.first_receive = umts_dsp::fractional_sampling::SamplePosition::new(
        umts_types::time::SampleInstant {
            epoch: env.key.epoch,
            index: activation.time().index * settings.samples.rate.get() as u64 + 32,
        },
        0.0,
    )
    .unwrap();
    settings.now = now;
    settings.publication_deadline = ChipInstant {
        index: activation.time().index - CHIPS_PER_FRAME,
        ..activation.time()
    };
    settings
}

fn policy(env: &InitialEnvironment) -> ConfiguredPolicy {
    policy_for_packet_modes(env, None)
}

fn policy_for_packet_modes(
    env: &InitialEnvironment,
    modes: Option<Vec<umts_config::PacketMode>>,
) -> ConfiguredPolicy {
    let mut config: umts_config::Config = serde_yaml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-support/fixtures/config/rel99.yaml"
    )))
    .unwrap();
    let retained = env.radio.retained_signalling(env.key).unwrap();
    let control = retained.iter().find(|rb| rb.id == 2).unwrap();
    config.rel99.physical.planner.signalling.um_uplink_bits = control.uplink_pdu_bits;
    config.rel99.physical.planner.signalling.um_downlink_bits = control.downlink_pdu_bits;
    let am = config.rel99.physical.planner.services.packet[0]
        .am
        .as_mut()
        .unwrap();
    am.ue.uplink_window = 32;
    am.ue.downlink_window = 32;
    let buffers = &mut am.buffers;
    buffers.maximum_sdu_bytes = 2048;
    buffers.maximum_buffer_bytes = 100_000;
    if modes.is_some() {
        config.hspa.downlink.enabled = true;
        config.hspa.uplink.enabled = true;
        config.hspa.downlink.qam64_enabled = true;
        config.hspa.downlink.dual_cell = false;
        config.cell.secondary_carrier.enabled = false;
        config.cell.packet_modes = modes;
    }
    ConfiguredPolicy::new(&config).unwrap()
}

#[test]
fn active_real_cs_lease_prepares_and_cancels_packet_addition_without_replacing_owners() {
    let mut peer = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let mut env = environment_for(
        Domain::Cs,
        vec![request_for(FIRST_CIRCUIT, peer.local_address().unwrap())],
        2,
    );
    let mut traffic = voices(2);
    env.admission
        .prepare_circuits(env.identity, env.core.gateway(), &mut traffic, |_, _| {
            Ok(settings())
        })
        .unwrap();
    start(&mut env);
    let now = Instant::now();
    negotiate(&mut env, &mut traffic, &mut peer, now);
    let serviced = complete_radio(&mut env);
    let clock = time(&env, now);
    env.admission
        .service_circuits(
            env.identity,
            env.core.gateway(),
            &mut traffic,
            &mut env.receive,
            clock,
            count(1),
        )
        .unwrap();
    let complete = env
        .admission
        .complete_circuits(
            env.identity,
            &mut env.core,
            &mut traffic,
            &mut env.receive,
            ControlTime(7),
        )
        .unwrap();
    assert_eq!(
        (complete.active, complete.ready, complete.recorded),
        (1, 1, 1)
    );
    env.core
        .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let cs_endpoint = env
        .admission
        .circuit_endpoint(env.identity, FIRST_CIRCUIT)
        .unwrap();
    assert_eq!(env.radio.dedicated_route(env.key), Some(env.identity));
    assert_eq!(env.resources.outstanding(), 1);
    for _ in 0..32 {
        if env.admission.active_radio(env.identity).is_ok() {
            break;
        }
        step(&mut env, serviced);
    }
    let active = env.admission.active_radio(env.identity).unwrap();
    assert!(Arc::ptr_eq(
        active.plan(),
        env.transmit.bank().session(env.identity).unwrap().plan()
    ));
    assert_eq!(active.lease().receive_identity(), env.identity);
    let original_plan = active.plan().clone();
    let retained = vec![
        env.admission
            .circuit_metadata(env.identity, FIRST_CIRCUIT)
            .unwrap()
            .clone(),
    ];

    let mut requested = item(INITIAL_RAB);
    requested.first.parameters.as_mut().unwrap().maximum =
        qos::MaximumRates::new(&[INITIAL_RATE]).unwrap();
    env.peer.rua(rua::Message::Connect {
        domain: Domain::Ps,
        context: env.context,
        cause: rua::EstablishmentCause::Normal,
        selector: None,
        membership: None,
        ranap: Assignment {
            setup: vec![requested],
            release: vec![],
        }
        .encode(WireLimits::default())
        .unwrap(),
    });
    let mut handoff = RabHandoff::new(WireLimits::default());
    env.core
        .drive(ControlTime(9), &mut env.radio, |_, event| {
            handoff.offer(event).map(|_| ())
        })
        .unwrap();
    let pending = handoff.take().unwrap().unwrap();
    let authority = pending.authorize_items(env.core.gateway()).unwrap();
    assert_eq!(authority.len(), 1);
    assert_eq!(authority[0].operation(), RabOperation::SetupOrModify);
    assert_eq!(authority[0].rab(), INITIAL_RAB);
    let config = policy(&env);
    let selected = config
        .services()
        .select(&[(CnDomain::Ps, &pending.assignment().setup[0])])
        .unwrap();
    let planned = config
        .plan_existing(
            &env.admission.active_radio(env.identity).unwrap(),
            &env.radio,
            &retained,
            selected.rabs(),
        )
        .unwrap();
    assert!(Arc::ptr_eq(planned.previous(), &original_plan));
    let active = env.admission.active_radio(env.identity).unwrap();
    assert!(Arc::ptr_eq(active.plan(), &original_plan));
    let activation = ScheduledActivation::new(at(0), 0, NonZeroU8::new(16).unwrap()).unwrap();
    assert!(activation.time().index > env.source.next_chip().index);
    let wrong = ReceiveIdentity {
        configuration: NonZeroU64::new(env.identity.configuration.get() + 1).unwrap(),
        ..env.identity
    };
    assert!(matches!(
        env.admission.active_radio(wrong),
        Err(AdmissionError::Identity)
    ));
    let duplicated_plan = Arc::new(
        crate::rel99_plan::CompiledRel99::new(
            original_plan.configuration().clone(),
            count(500_000),
            TransportPlanLimits {
                maximum_coded_bits: count(500_000),
                maximum_mapping_entries: count(1_000_000),
                decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    );
    assert!(Arc::ptr_eq(
        duplicated_plan.configuration(),
        original_plan.configuration()
    ));
    assert!(!Arc::ptr_eq(&duplicated_plan, &original_plan));
    let capabilities = umts_rrc::capabilities::rel99::Rel99Limits::from_report(
        env.radio.capabilities(env.key).unwrap(),
    )
    .unwrap();
    let unbound = crate::rel99_planner::plan_existing(
        duplicated_plan,
        &retained,
        selected.rabs(),
        &capabilities,
        &config.policy(),
    )
    .unwrap();
    let rejected = match unbound.allocate(active, &env.radio, &mut env.resources, activation) {
        Err(rejected) => rejected,
        Ok(_) => panic!("distinct unbound compiled plan accepted an authentic active-radio proof"),
    };
    assert!(matches!(
        rejected.error,
        crate::rel99_planner::Error::Identity
    ));
    assert!(rejected.change.is_none());
    assert_eq!(
        env.admission
            .active_radio(env.identity)
            .unwrap()
            .lease()
            .receive_identity(),
        env.identity
    );
    assert_eq!(env.resources.outstanding(), 1);

    let prepared = planned
        .allocate(
            env.admission.active_radio(env.identity).unwrap(),
            &env.radio,
            &mut env.resources,
            activation,
        )
        .unwrap();
    assert_eq!(env.resources.outstanding(), 1);
    assert_eq!(prepared.change.key(), env.key);
    assert!(prepared.change.receive_identity().configuration > env.identity.configuration);
    assert!(Arc::ptr_eq(
        prepared.setup.configuration(),
        prepared.change.plan().configuration()
    ));
    assert!(Arc::ptr_eq(
        prepared.profiles.plan(),
        prepared.change.plan()
    ));
    for old in original_plan.configuration().bearers() {
        assert_eq!(prepared.setup.configuration().bearer(old.id), Some(old));
    }
    let message = prepared
        .setup
        .message(TransactionId::new(3).unwrap())
        .unwrap();
    let decoded = decode_downlink_dedicated(&message.bytes, message.bit_length).unwrap();
    assert!(matches!(
        decoded.value.message,
        DLDCCHMessageType::radioBearerSetup(_)
    ));
    assert_eq!(prepared.setup.rabs().len(), 1);
    assert_eq!(prepared.setup.rabs()[0].identity, INITIAL_RAB);
    let charges = env.radio.resources();
    let mut invalid = owner_settings(&env, activation, serviced);
    invalid.samples.first_receive = umts_dsp::fractional_sampling::SamplePosition::new(
        umts_types::time::SampleInstant {
            epoch: umts_types::RadioEpoch::new(NonZeroU64::new(env.key.epoch.get() + 1).unwrap()),
            index: invalid.samples.first_receive.sample().index,
        },
        0.0,
    )
    .unwrap();
    assert!(matches!(
        env.admission
            .build_existing_owners(env.identity, &prepared, &mut env.radio, invalid),
        Err(OwnerBuildError::Receive(_))
    ));
    assert_eq!(env.radio.resources(), charges);
    for _ in 0..4 {
        let settings = owner_settings(&env, activation, serviced);
        let built = env
            .admission
            .build_existing_owners(env.identity, &prepared, &mut env.radio, settings)
            .unwrap();
        assert_eq!(built.previous(), env.identity);
        assert_eq!(built.identity(), prepared.change.receive_identity());
        assert!(Arc::ptr_eq(built.plan(), prepared.change.plan()));
        drop(built);
        assert_eq!(env.radio.resources(), charges);
        assert_eq!(env.resources.outstanding(), 1);
        assert_eq!(env.radio.dedicated_route(env.key), Some(env.identity));
    }
    let PreparedExisting { change, .. } = prepared;
    env.resources.cancel_reconfiguration(change).unwrap();
    assert_eq!(env.resources.outstanding(), 1);
    assert_eq!(
        env.admission
            .active_radio(env.identity)
            .unwrap()
            .lease()
            .receive_identity(),
        env.identity
    );
    assert_eq!(env.radio.dedicated_route(env.key), Some(env.identity));
    assert_eq!(
        env.admission.circuit_endpoint(env.identity, FIRST_CIRCUIT),
        Some(cs_endpoint)
    );
    let still_working = env
        .admission
        .circuit_progress(env.identity, &traffic)
        .unwrap();
    assert_eq!(
        (
            still_working.active,
            still_working.ready,
            still_working.recorded
        ),
        (1, 1, 1)
    );
    assert!(pending.authorize_items(env.core.gateway()).is_ok());
    let active = env.admission.active_radio(env.identity).unwrap();
    assert!(Arc::ptr_eq(active.plan(), &original_plan));
    let retry = planned
        .allocate(active, &env.radio, &mut env.resources, activation)
        .unwrap();
    env.resources.cancel_reconfiguration(retry.change).unwrap();
    assert_eq!(env.resources.outstanding(), 1);
}
