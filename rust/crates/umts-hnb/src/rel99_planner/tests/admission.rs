// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_tx::CommonTx,
    dedicated_bearers::{
        BearerLimits, EmptyTransparentTraffic, FramePoolLimits, MappingActivation,
    },
    dedicated_controller::{DedicatedController, DedicatedControllerLimits},
    dedicated_rx::{
        DedicatedSampleRuntime, FrameAnchor, ReceiveLimits, ReceiveResources,
        SampleReceiveSettings, SampleRuntimeLimits, ShardLimits, TfciAcceptance,
    },
    dedicated_tx::{
        DedicatedTransmitPorts, DedicatedTransmitRuntime, TransmitLimits, TransmitRuntimeLimits,
    },
    rel99_admission::{
        AdmissionError, AdmissionLimits, AdmissionPhase, Rel99Admission, TrafficFramePolicy,
    },
    rel99_planner::{InitialOwnerSettings, OwnerBuildError},
};
use umts_dsp::{
    carrier::CarrierOffsetHz,
    dedicated::{CarrierTrackingSettings, DpchAmplitudes, LikelihoodSettings},
    fractional_sampling::SamplePosition,
    pulse_shaping::SamplesPerChip,
};
use umts_types::{
    RadioEpoch,
    time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, SampleInstant},
};

pub(crate) mod rabs;

pub(crate) fn prepared() -> (CommonTx, CommonSignalling, RadioResources, PreparedInitial) {
    prepared_for(false, true)
}

pub(crate) fn prepared_for(
    cs_enabled: bool,
    ps_enabled: bool,
) -> (CommonTx, CommonSignalling, RadioResources, PreparedInitial) {
    prepared_for_services(cs_enabled, ps_enabled, None)
}

pub(crate) fn prepared_packet_catalog(
    services: &umts_config::R99ServicesConfig,
) -> (CommonTx, CommonSignalling, RadioResources, PreparedInitial) {
    prepared_for_services(false, true, Some(services))
}

fn prepared_for_services(
    cs_enabled: bool,
    ps_enabled: bool,
    services: Option<&umts_config::R99ServicesConfig>,
) -> (CommonTx, CommonSignalling, RadioResources, PreparedInitial) {
    assert!(cs_enabled || ps_enabled);
    assert!(
        cs_enabled ^ ps_enabled,
        "prepared_for builds one initial CN-domain request; use prepared_multi_packet for PS+PS"
    );
    let (source, radio, key) = signalling();
    let retained = radio.retained_signalling(key).unwrap();
    let cs = cs_request();
    let cs_service = circuit(&cs);
    let ps = ps_request(PACKET_RATE);
    let ps_service = packet(&ps, false);
    let catalog = services.map(|services| {
        super::super::ConfiguredServices::new(services, count(USER_BYTES)).unwrap()
    });
    let requests = [(CnDomain::Ps, &ps)];
    let selected = catalog
        .as_ref()
        .map(|catalog| catalog.select(&requests).unwrap());
    let mut rabs = Vec::new();
    if cs_enabled {
        rabs.push(rab(CnDomain::Cs, &cs, Service::Circuit(&cs_service)));
    } else if ps_enabled {
        match &selected {
            Some(selected) => rabs.extend(selected.rabs().iter().map(|rab| Rab {
                domain: rab.domain,
                request: rab.request,
                service: rab.service,
            })),
            None => rabs.push(rab(CnDomain::Ps, &ps, Service::Packet(&ps_service))),
        }
    }
    let physical = [fixture::physical()];
    let mut configured = policy(&physical);
    let control = retained.iter().find(|rb| rb.id == 2).unwrap();
    configured.signalling.um_uplink_bits = control.uplink_pdu_bits;
    configured.signalling.um_downlink_bits = control.downlink_pdu_bits;
    let plan = plan_initial_for_ue(key, &radio, &rabs, &configured).unwrap();
    let cell = test_support::plan(false);
    let mut resources =
        RadioResources::new(key.epoch, cell.information(), count(8), 17..=24).unwrap();
    let prepared = plan.allocate(key, &radio, &mut resources).unwrap();
    (source, radio, resources, prepared)
}

pub(crate) fn prepared_multi_packet()
-> (CommonTx, CommonSignalling, RadioResources, PreparedInitial) {
    let (source, radio, key) = signalling_with_multi_trch_capability();
    let retained = radio.retained_signalling(key).unwrap();
    let ps = ps_request(PACKET_RATE);
    let ps_service = packet(&ps, false);
    let ps_second = super::request(
        CnDomain::Ps,
        super::PS_RAB + 1,
        ps.first.parameters.clone().expect("packet parameters"),
    );
    let ps_second_service = packet(&ps_second, false);
    let rabs = vec![
        rab(CnDomain::Ps, &ps, Service::Packet(&ps_service)),
        rab(
            CnDomain::Ps,
            &ps_second,
            Service::Packet(&ps_second_service),
        ),
    ];
    let physical = [fixture::physical()];
    let mut configured = policy(&physical);
    let control = retained.iter().find(|rb| rb.id == 2).unwrap();
    configured.signalling.um_uplink_bits = control.uplink_pdu_bits;
    configured.signalling.um_downlink_bits = control.downlink_pdu_bits;
    let plan = plan_initial_for_ue(key, &radio, &rabs, &configured).unwrap();
    let cell = test_support::plan(false);
    let mut resources =
        RadioResources::new(key.epoch, cell.information(), count(8), 17..=24).unwrap();
    let prepared = plan.allocate(key, &radio, &mut resources).unwrap();
    (source, radio, resources, prepared)
}

pub(crate) fn sequential_cs_then_ps_plans() -> (PlannedInitial, PlannedExisting) {
    let cs = cs_request();
    let cs_service = circuit(&cs);
    let ps = ps_request(PACKET_RATE);
    let ps_service = packet(&ps, false);
    let physical = [fixture::physical()];
    let configured = policy(&physical);
    let initial = plan_initial(
        &fixture::retained(),
        &[rab(CnDomain::Cs, &cs, Service::Circuit(&cs_service))],
        &limits(),
        &configured,
    )
    .unwrap();
    let addition = plan_existing(
        initial.compiled().clone(),
        initial.rabs(),
        &[rab(CnDomain::Ps, &ps, Service::Packet(&ps_service))],
        &limits(),
        &configured,
    )
    .unwrap();
    (initial, addition)
}

fn signalling_with_multi_trch_capability() -> (CommonTx, CommonSignalling, UeKey) {
    use crate::signalling::tests as signalling_fixture;
    use crate::test_support::capabilities as capability_fixture;
    use umts_rrc_schema::information_elements as ie;

    let source = test_support::source(false);
    let mut radio = signalling_fixture::controller(&source);
    let key = signalling_fixture::establish(&mut radio, &source, 0);
    radio
        .request_capabilities(key, at(0), TransactionId::new(2).unwrap())
        .unwrap();
    let mut peer = signalling_fixture::peer(&source);
    peer.enqueue(&signalling_fixture::COMPLETE).unwrap();
    peer.publish_next(at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    let mut information = capability_fixture::information();
    let radio_access = information
        .ue_radio_access_capability
        .as_mut()
        .expect("radio capability fixture");
    let transport = &mut radio_access.transport_channel_capability;
    let uplink = &mut transport.ul_trans_ch_capability;
    uplink.max_simultaneous_trans_chs = ie::MaxSimultaneousTransChsUL::e16;
    uplink.max_transmitted_blocks = ie::MaxTransportBlocksUL::tb32;
    uplink.max_number_of_tfc = ie::MaxNumberOfTFCUL::tfc1024;
    uplink.max_number_of_tf = ie::MaxNumberOfTF::tf1024;
    let downlink = &mut transport.dl_trans_ch_capability;
    downlink.max_simultaneous_trans_chs = ie::MaxSimultaneousTransChsDL::e16;
    downlink.max_received_transport_blocks = ie::MaxTransportBlocksDL::tb32;
    downlink.max_number_of_tfc = ie::MaxNumberOfTFCDL::tfc1024;
    downlink.max_number_of_tf = ie::MaxNumberOfTF::tf1024;
    information.rrc_transaction_identifier = Some(ie::RRCTransactionIdentifier(
        TransactionId::new(2).unwrap().get(),
    ));
    signalling_fixture::send_capability_report_information(&mut radio, key, &mut peer, information);
    assert_eq!(radio.process_one(), Some(Ok(key)));
    (source, radio, key)
}

pub(crate) fn settings(epoch: RadioEpoch) -> InitialOwnerSettings {
    let anchor = FrameAnchor {
        time: at(0),
        cfn: 0,
    };
    InitialOwnerSettings {
        power_control: None,
        timing: MappingActivation {
            uplink: anchor,
            uplink_frame: 0,
            downlink: anchor,
            downlink_frame: 0,
        },
        samples: SampleReceiveSettings {
            radio_link: Default::default(),
            rate: SamplesPerChip::Two,
            first_receive: SamplePosition::new(SampleInstant { epoch, index: 32 }, 0.0).unwrap(),
            search_radius_samples: 8,
            minimum_coherent_fraction: 0.05,
        },
        carrier: CarrierTrackingSettings {
            initial: CarrierOffsetHz::new(0.0).unwrap(),
            maximum_absolute_hz: 2000.0,
            update_fraction: 0.5,
            minimum_coherence: 0.8,
        },
        likelihood: LikelihoodSettings::new(128.0).unwrap(),
        tfci: TfciAcceptance {
            minimum_score: 0.5,
            minimum_margin: 0.1,
        },
        maximum_physical_bits: count(57_600),
        receive: ReceiveLimits {
            output_slots: count(8),
            maximum_output_bits: count(10_000),
            maximum_output_bytes: 100_000,
            maximum_coded_bits: count(500_000),
            maximum_frame_bits: count(100_000),
            maximum_decodes_per_step: count(8),
        },
        bearers: BearerLimits {
            maximum_transport_bits: count(10_000),
            output_slots: count(8),
            maximum_sdu_bytes: count(2048),
            maximum_buffer_bytes: 2_000_000,
        },
        transmit: TransmitLimits {
            frame_slots: count(8),
            maximum_frame_bits: count(100_000),
            maximum_retained_bits: count(1_000_000),
        },
        amplitudes: DpchAmplitudes {
            data: 0.2,
            tpc: 0.2,
            tfci: 0.2,
            pilot: 0.2,
        },
        downlink_power: None,
        pool: FramePoolLimits {
            payload_slots: count(1),
            receipt_slots: count(32),
            maximum_buffer_bytes: 2_000_000,
        },
        now: at(0),
        publication_deadline: at(100 * CHIPS_PER_FRAME),
        response_timeout: NonZeroU64::new(100 * CHIPS_PER_FRAME).unwrap(),
    }
}

pub(crate) fn transmit(source: &CommonTx) -> (DedicatedTransmitRuntime, DedicatedTransmitPorts) {
    DedicatedTransmitRuntime::new(
        source,
        count(8),
        count(32),
        count(8_000_000),
        TransmitRuntimeLimits {
            command_capacity: count(8),
            commands_per_step: count(8),
            feedback_capacity: count(8),
            feedback_per_step: count(8),
            maximum_frame_payload_bits: count(100_000),
            maximum_inflight_retained_bits: count(8_000_000),
        },
    )
    .unwrap()
}

pub(crate) fn receive(
    epoch: RadioEpoch,
    capacity: usize,
) -> (DedicatedSampleRuntime, DedicatedController) {
    let (runtime, port) = DedicatedSampleRuntime::new(
        epoch,
        SamplesPerChip::Two,
        SampleRuntimeLimits {
            shard: ShardLimits {
                maximum_ues: count(capacity),
                maximum_sessions_per_step: count(2),
                resources: ReceiveResources {
                    physical_sample_bytes: 8_000_000,
                    coded_bits: 1_000_000,
                    output_bytes: 1_000_000,
                    demultiplexed_bits: 1_000_000,
                    channels: 32,
                },
            },
            history_samples: count(CHIPS_PER_FRAME as usize * 4),
            maximum_shared_bytes: 4_000_000,
            command_capacity: count(2),
            commands_per_step: count(2),
            output_slots_per_ue: count(8),
            maximum_output_bits: count(10_000),
            maximum_output_bytes: 200_000,
        },
    )
    .unwrap();
    let controller = DedicatedController::new(
        port,
        DedicatedControllerLimits {
            receipt_visits: count(2),
            service_visits: count(2),
            maximum_bearer_bytes: 4_000_000,
        },
    );
    (runtime, controller)
}

pub(crate) fn admission_limits() -> AdmissionLimits {
    AdmissionLimits {
        visits: count(2),
        command_results: count(2),
        prefill_frames: count(8),
        maximum_lead_frames: count(8),
        health_interval_chips: NonZeroU64::new(1_000_000).unwrap(),
    }
}

#[test]
fn initial_owner_power_policy_is_validated_before_charging_the_frame_pool() {
    use crate::dedicated_rx::{PilotSirTarget, PilotTpcSettings, ReceiveError};
    use crate::dedicated_tx::TpcInitialization;
    use crate::rel99_planner::InitialPowerControl;
    let (source, mut radio, mut resources, prepared) = prepared();
    let (_runtime, ports) = transmit(&source);
    let identity = prepared.allocation.receive_identity();
    for case in 0..4 {
        let mut settings = settings(identity.ue.epoch);
        let mut pilot = PilotTpcSettings {
            target: PilotSirTarget::from_db(18.0).unwrap(),
            minimum_coherent_fraction: 0.05,
            uplink_anchor: settings.timing.uplink.time,
            downlink_anchor: settings.timing.downlink.time,
            rate: settings.samples.rate,
            lead_chips: NonZeroU64::new(CHIPS_PER_SLOT * 2).unwrap(),
            maximum_observation_age_chips: NonZeroU64::new(CHIPS_PER_SLOT * 4).unwrap(),
        };
        match case {
            1 => pilot.uplink_anchor.index += CHIPS_PER_SLOT,
            2 => pilot.downlink_anchor.index += CHIPS_PER_SLOT,
            3 => pilot.rate = SamplesPerChip::Four,
            _ => {}
        }
        settings.power_control = Some(InitialPowerControl {
            outer_loop: None,
            algorithm2_hold_groups: 2,
            pilot,
            initialization: TpcInitialization {
                first_radio_link_set: true,
                pattern_01_count: 1,
                maximum_slots: std::num::NonZeroU32::new(1500).unwrap(),
            },
        });
        let result = prepared.build_owners(&mut radio, &ports.control, settings);
        if case == 0 {
            assert_eq!(result.as_ref().unwrap().identity(), identity);
        } else if case == 2 {
            assert!(matches!(result, Err(OwnerBuildError::Identity)));
        } else {
            assert!(matches!(
                result,
                Err(OwnerBuildError::Receive(ReceiveError::Configuration))
            ));
        }
        drop(result);
        assert!(radio.dedicated_route(identity.ue).is_none());
        assert_eq!(resources.outstanding(), 1);
    }
    resources.cancel(prepared.allocation).unwrap();
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn dropped_owner_guards_return_pool_credits_for_the_same_allocation() {
    let (source, mut radio, mut resources, prepared) = prepared();
    let (_runtime, ports) = transmit(&source);
    let identity = prepared.allocation.receive_identity();
    for _ in 0..8 {
        let owners = prepared
            .build_owners(&mut radio, &ports.control, settings(identity.ue.epoch))
            .unwrap();
        assert_eq!(owners.identity(), identity);
        drop(owners);
        assert!(radio.dedicated_route(identity.ue).is_none());
        assert_eq!(resources.outstanding(), 1);
    }
    resources.cancel(prepared.allocation).unwrap();
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn bad_sample_epoch_fails_before_charging_a_frame_pool() {
    let (source, mut radio, mut resources, prepared) = prepared();
    let (_runtime, ports) = transmit(&source);
    let identity = prepared.allocation.receive_identity();
    let foreign = RadioEpoch::new(NonZeroU64::new(identity.ue.epoch.get() + 1).unwrap());
    for _ in 0..8 {
        assert!(matches!(
            prepared.build_owners(&mut radio, &ports.control, settings(foreign)),
            Err(OwnerBuildError::Receive(_))
        ));
    }
    drop(
        prepared
            .build_owners(&mut radio, &ports.control, settings(identity.ue.epoch))
            .unwrap(),
    );
    resources.cancel(prepared.allocation).unwrap();
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn rejected_begin_preserves_allocation_and_releases_pool_before_valid_retry() {
    let (mut source, mut radio, mut resources, mut prepared) = prepared();
    let identity = prepared.allocation.receive_identity();
    let (_rx, mut controller) = receive(identity.ue.epoch, 8);
    let foreign = RadioEpoch::new(NonZeroU64::new(identity.ue.epoch.get() + 1).unwrap());
    let (_foreign_rx, foreign_controller) = receive(foreign, 8);
    let (_small_rx, small_controller) = receive(identity.ue.epoch, 1);
    for case in 0..3 {
        let (_runtime, ports) = transmit(&source);
        let (_other_runtime, other_ports) = transmit(&source);
        let owners = prepared
            .build_owners(&mut radio, &ports.control, settings(identity.ue.epoch))
            .unwrap();
        let (port, receiver) = match case {
            0 => (other_ports.control, &controller),
            1 => (ports.control, &foreign_controller),
            _ => (ports.control, &small_controller),
        };
        let mut admission = Rel99Admission::new(port, count(8), admission_limits())
            .ok()
            .unwrap();
        let rejected = owners
            .begin(prepared.allocation, &mut admission, receiver)
            .unwrap_err();
        assert_eq!(rejected.error, AdmissionError::Identity);
        assert_eq!(rejected.allocation.receive_identity(), identity);
        assert!(admission.phase(identity).is_none());
        assert!(radio.dedicated_route(identity.ue).is_none());
        assert_eq!(resources.outstanding(), 1);
        prepared.allocation = rejected.allocation;
    }
    let (_runtime, ports) = transmit(&source);
    let owners = prepared
        .build_owners(&mut radio, &ports.control, settings(identity.ue.epoch))
        .unwrap();
    let mut admission = Rel99Admission::new(ports.control, count(8), admission_limits())
        .ok()
        .unwrap();
    assert_eq!(
        owners
            .begin(prepared.allocation, &mut admission, &controller)
            .unwrap(),
        identity
    );
    assert_eq!(admission.phase(identity), Some(AdmissionPhase::Preparing));
    admission.cancel(identity).unwrap();
    let _writer = source.submission_writer().unwrap();
    let mut common = source.control_port(count(8), count(8)).unwrap();
    for _ in 0..16 {
        admission
            .step_with_policy(
                at(0),
                &mut radio,
                &mut controller,
                &mut common,
                &mut TrafficFramePolicy::new(EmptyTransparentTraffic, count(1024)),
            )
            .unwrap();
        if admission.phase(identity) == Some(AdmissionPhase::Retired) {
            break;
        }
    }
    assert_eq!(admission.phase(identity), Some(AdmissionPhase::Retired));
    let retired = admission.take_retired(identity).unwrap();
    assert!(retired.unadmitted_receive.is_some());
    resources.release(retired.resources.unwrap()).unwrap();
    assert_eq!(resources.outstanding(), 0);
}
