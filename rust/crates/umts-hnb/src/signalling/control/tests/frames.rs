// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::{
        BearerEndpoint, BearerError, BearerId, BearerLimits, DedicatedBearers, DownlinkGrant,
        FrameBuildError, MappingActivation, TransparentSettings,
    },
    dedicated_rx::{FrameAnchor, ReceiveIdentity},
    dedicated_tx::{
        DedicatedTransmitPorts, DedicatedTransmitRuntime, DedicatedTransmitSession, TpcPlan,
        TransmitCommandOutcome, TransmitError, TransmitLimits, TransmitRuntimeLimits,
    },
    rel99_plan::CompiledRel99,
    test_support::count,
};
use umts_dsp::{
    bits::Bit,
    coding::DecodeBudget,
    dedicated_transport::{TransportId, TransportPlanLimits},
    tfci::Tfci,
    waveform::WaveformWindow,
};
use umts_rlc::{
    acknowledged::AmLimits,
    transparent::{ErrorDelivery, Segmentation, TmLimits},
};
use umts_security::{
    ciphering::{BearerCipherConfiguration, Configuration, CountAnchor, Position, Scope},
    counters::CipherMode,
};
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT};

mod admission;
mod mapping;
mod packet;
mod pool;
mod traffic;
mod voice;

struct Fixture {
    source: CommonTx,
    radio: CommonSignalling,
    key: UeKey,
    plan: Arc<CompiledRel99>,
    bank: DedicatedBearers,
    runtime: DedicatedTransmitRuntime,
    ports: DedicatedTransmitPorts,
}
fn packet_only_configuration(retained: &[RetainedBearer]) -> Arc<Rel99Configuration> {
    let (ul, dl, mut bearers, physical) = fixture::components(false, true, 10);
    for bearer in &mut bearers {
        if let Some(old) = retained.iter().find(|old| old.id == bearer.id) {
            bearer.rlc = old.rlc.clone();
        }
    }
    let rebuild = |tfcs: &DchTfcs, direction, size| {
        let mut transports = tfcs.transports().to_vec();
        for group in &mut transports.last_mut().unwrap().groups {
            group.rlc_bits = size;
        }
        DchTfcs::new(direction, transports, tfcs.combinations().to_vec()).unwrap()
    };
    Arc::new(
        Rel99Configuration::new(
            rebuild(&ul, DchDirection::Uplink, retained[1].uplink_pdu_bits),
            rebuild(&dl, DchDirection::Downlink, retained[1].downlink_pdu_bits),
            bearers,
            physical,
        )
        .unwrap(),
    )
}
fn fixture(packet: bool) -> Fixture {
    fixture_with_activation(packet, true)
}
fn fixture_with_activation(packet: bool, activate: bool) -> Fixture {
    fixture_parts(packet, activate, true, false)
}
fn fixture_parts(
    packet: bool,
    activate: bool,
    prepare_transmit: bool,
    pending_users: bool,
) -> Fixture {
    fixture_parts_with_plan(
        packet,
        activate,
        prepare_transmit,
        pending_users,
        |radio, key| {
            Arc::new(
                CompiledRel99::new(
                    configuration_for(&radio.retained_signalling(key).unwrap(), packet),
                    count(200_000),
                    TransportPlanLimits {
                        maximum_coded_bits: count(200_000),
                        maximum_mapping_entries: count(300_000),
                        decode: DecodeBudget::new(std::num::NonZeroU8::new(8).unwrap()).unwrap(),
                    },
                )
                .unwrap(),
            )
        },
    )
}
fn fixture_parts_with_plan(
    packet: bool,
    activate: bool,
    prepare_transmit: bool,
    pending_users: bool,
    build: impl FnOnce(&CommonSignalling, UeKey) -> Arc<CompiledRel99>,
) -> Fixture {
    let (source, mut radio, key, _) = ready(false);
    let plan = build(&radio, key);
    let identity = ReceiveIdentity {
        ue: key,
        configuration: NonZeroU64::MIN,
    };
    let anchor = FrameAnchor {
        time: at(0),
        cfn: 0,
    };
    let mut endpoints = radio.signalling_endpoints(key).unwrap();
    if pending_users {
        endpoints.extend(user_profiles(plan.clone()).placeholders(key));
    } else {
        for id in 10..=12 {
            endpoints.push(
                BearerEndpoint::transparent(
                    key,
                    BearerId::new(id).unwrap(),
                    Scope::User(CnDomain::Cs),
                    TransparentSettings {
                        uplink_segmentation: Segmentation::Disabled,
                        downlink_segmentation: Segmentation::Disabled,
                        errors: ErrorDelivery::Indicate,
                        limits: TmLimits {
                            maximum_pdus: count(1),
                            maximum_pdu_bits: count(104),
                            maximum_sdu_bits: count(104),
                        },
                    },
                )
                .unwrap(),
            );
        }
        if packet {
            endpoints.push(
                BearerEndpoint::acknowledged(
                    key,
                    BearerId::new(20).unwrap(),
                    Scope::User(CnDomain::Ps),
                    AmEntity::new(
                        SignallingProfile.network_am_configuration(count(40), count(40), 0, 0),
                        AmLimits {
                            maximum_sdu_bytes: count(512),
                            queued_sdus: count(4),
                            maximum_buffer_bytes: 1_000_000,
                        },
                        at(0),
                    )
                    .unwrap(),
                )
                .unwrap(),
            );
        }
        radio
            .with_bearer_access(key, |ciphers, _, _| {
                for (id, domain, mode) in [
                    (10, CnDomain::Cs, CipherMode::Transparent),
                    (11, CnDomain::Cs, CipherMode::Transparent),
                    (12, CnDomain::Cs, CipherMode::Transparent),
                    (20, CnDomain::Ps, CipherMode::Acknowledged),
                ] {
                    if id == 20 && !packet {
                        continue;
                    }
                    let anchor = CountAnchor::new(
                        mode,
                        if mode == CipherMode::Transparent {
                            Position::Frame(0)
                        } else {
                            Position::Sequence(0)
                        },
                        0,
                    )
                    .unwrap();
                    ciphers
                        .register_with(
                            BearerCipherConfiguration {
                                bearer: id,
                                scope: Scope::User(domain),
                                transmit: anchor,
                                receive: anchor,
                                initial: Configuration::Unprotected,
                            },
                            || Ok::<_, ()>(()),
                        )
                        .unwrap();
                }
            })
            .unwrap();
    }
    let mut bank = DedicatedBearers::from_compiled(
        identity,
        MappingActivation {
            uplink: anchor,
            uplink_frame: 0,
            downlink: anchor,
            downlink_frame: 0,
        },
        plan.clone(),
        endpoints,
        BearerLimits {
            maximum_transport_bits: count(10_000),
            output_slots: count(8),
            maximum_sdu_bytes: radio.limits.maximum_sdu_bytes,
            maximum_buffer_bytes: 2_000_000,
        },
    )
    .unwrap();
    if activate {
        bank.activate(identity).unwrap();
    }
    let (mut runtime, mut ports) = DedicatedTransmitRuntime::new(
        &source,
        count(2),
        count(4),
        count(2_000_000),
        TransmitRuntimeLimits {
            command_capacity: count(8),
            commands_per_step: count(8),
            feedback_capacity: count(2),
            feedback_per_step: count(2),
            maximum_frame_payload_bits: count(10_000),
            maximum_inflight_retained_bits: count(2_000_000),
        },
    )
    .unwrap();
    if prepare_transmit {
        ports
            .control
            .prepare(Box::new(
                DedicatedTransmitSession::new(
                    identity,
                    anchor,
                    plan.clone(),
                    umts_dsp::dedicated::DpchAmplitudes {
                        data: 0.2,
                        tpc: 0.2,
                        tfci: 0.2,
                        pilot: 0.2,
                    },
                    TransmitLimits {
                        frame_slots: count(8),
                        maximum_frame_bits: count(100_000),
                        maximum_retained_bits: count(1_000_000),
                    },
                )
                .unwrap(),
            ))
            .ok()
            .unwrap();
        runtime.service().unwrap();
        assert!(
            matches!(ports.control.try_event().unwrap().outcome, TransmitCommandOutcome::Prepared(id) if id == identity)
        );
    }
    Fixture {
        source,
        radio,
        key,
        plan,
        bank,
        runtime,
        ports,
    }
}
pub(super) fn user_profiles(
    plan: Arc<CompiledRel99>,
) -> Arc<crate::dedicated_bearers::UserRlcProfiles> {
    use crate::dedicated_bearers::{NetworkAmPolicy, UserRlcPolicy, UserRlcProfiles};
    let mut policies = Vec::new();
    for bearer in plan
        .configuration()
        .bearers()
        .iter()
        .filter(|rb| rb.domain.is_some())
    {
        let policy = match bearer.rlc {
            umts_rrc::dedicated::Rel99Rlc::Transparent { .. } => UserRlcPolicy::Transparent {
                errors: ErrorDelivery::Indicate,
                maximum_sdu_bits: count(208),
            },
            _ => {
                let config = SignallingProfile.network_am_configuration(count(40), count(40), 0, 0);
                UserRlcPolicy::Acknowledged {
                    network: NetworkAmPolicy {
                        maximum_data_attempts: config.maximum_data_attempts,
                        maximum_reset_attempts: config.maximum_reset_attempts,
                        reset_timeout_chips: config.reset_timeout_chips,
                        polling: config.polling,
                        status_prohibit_chips: config.status_prohibit_chips,
                        missing_pdu_indicator: config.missing_pdu_indicator,
                    },
                    limits: AmLimits {
                        maximum_sdu_bytes: count(512),
                        queued_sdus: count(4),
                        maximum_buffer_bytes: 1_000_000,
                    },
                }
            }
        };
        policies.push((BearerId::new(bearer.id).unwrap(), policy));
    }
    Arc::new(UserRlcProfiles::new(plan, &policies, count(2_000_000)).unwrap())
}
fn grant(
    identity: ReceiveIdentity,
    transport: u8,
    bearer: u8,
    format: usize,
    frame: u64,
) -> DownlinkGrant {
    DownlinkGrant {
        identity,
        transport: TransportId::new(transport).unwrap(),
        bearer: BearerId::new(bearer).unwrap(),
        format,
        start: at(frame * CHIPS_PER_FRAME),
    }
}
fn frame_ready(f: &mut Fixture, frame: u64) -> crate::tx_evidence::TransportSubmission {
    let mut draft = f
        .bank
        .begin_frame(
            f.plan.clone(),
            &f.ports.control,
            at(frame * CHIPS_PER_FRAME),
            Tfci::new(6).unwrap(),
        )
        .unwrap();
    if frame == 0 {
        assert!(
            draft
                .publish_signalling(
                    grant(draft.identity(), 10, 2, 1, frame),
                    at(0),
                    &mut f.radio
                )
                .unwrap()
        );
    }
    assert!(draft.complete());
    let receipt = draft.submission();
    let input = draft.finish().ok().unwrap();
    if frame == 0 {
        let transport = input
            .transports
            .iter()
            .find(|transport| transport.transport == TransportId::new(10).unwrap())
            .unwrap();
        let mut packed = vec![0; transport.bits.len().div_ceil(BITS_PER_BYTE)];
        umts_dsp::bits::pack_msb(&transport.bits, &mut packed).unwrap();
        let pdu = umts_mac::decode_dedicated(
            umts_mac::DedicatedMapping::Multiplexed,
            Bits::new(&packed, transport.bits.len()).unwrap(),
        )
        .unwrap();
        assert_eq!(pdu.channel.unwrap().get(), 2);
        let mut raw = vec![0; pdu.payload.len() / BITS_PER_BYTE];
        pdu.payload.copy_to(&mut raw).unwrap();
        assert_eq!(
            umts_rlc::acknowledged::parse_status(&raw)
                .unwrap()
                .acknowledge,
            Some(1)
        );
    }
    f.ports.control.frame(input).ok().unwrap();
    f.runtime.service().unwrap();
    assert!(matches!(
        f.ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Frame { result: Ok(_), .. }
    ));
    receipt
}
fn feedback(f: &mut Fixture, first: u64, slots: usize) {
    f.ports
        .feedback
        .publish(TpcPlan {
            identity: f.bank.identity(),
            first_slot: at(first * CHIPS_PER_SLOT),
            commands: vec![Bit::Zero; slots],
        })
        .ok()
        .unwrap();
    f.runtime.service().unwrap();
    assert!(matches!(
        f.ports.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Feedback { result: Ok(()), .. }
    ));
}

#[test]
fn status_submission_requires_full_dedicated_tti_and_driver_pulse_tail() {
    let mut f = fixture(false);
    let mut writer = f.source.submission_writer().unwrap();
    let mut receipt = None;
    for frame in 0..4 {
        let current = frame_ready(&mut f, frame);
        if frame == 0 {
            receipt = Some(current);
        }
    }
    let receipt = receipt.unwrap();
    let mut status = f
        .radio
        .registry
        .get(f.key)
        .unwrap()
        .control
        .status_submission(f.key)
        .unwrap()
        .unwrap()
        .transport
        .clone();
    assert!(!status.submitted());
    assert!(!status.detach_submitted_emission());
    f.ports.control.arm(f.bank.identity()).unwrap();
    f.runtime.service().unwrap();
    f.ports.control.try_event().unwrap();
    feedback(&mut f, 0, 60);
    let mut samples = vec![num_complex::Complex32::default(); f.source.samples_per_slot()];
    for slot in 0..60 {
        let window = f
            .runtime
            .generate_slot(&mut f.source, &mut samples)
            .unwrap();
        assert!(writer.accepted(window));
        assert!(
            !receipt.submitted(),
            "slot {slot} must still include pulse tail"
        );
    }
    assert!(receipt.viable());
    assert!(!status.submitted());
    let window = f.source.generate_slot(&mut samples).unwrap();
    assert!(writer.accepted(window));
    assert!(receipt.submitted());
    assert!(status.submitted());
    assert!(status.detach_submitted_emission());
    assert!(!status.detach_submitted_emission());
    assert!(status.submitted());
    writer.fail();
    assert!(!status.viable());
    assert!(!status.submitted());
}

#[test]
fn common_driver_progress_cannot_confirm_an_unprepared_dedicated_status() {
    let mut f = fixture(false);
    let mut writer = f.source.submission_writer().unwrap();
    let mut draft = f
        .bank
        .begin_frame(
            f.plan.clone(),
            &f.ports.control,
            at(0),
            Tfci::new(6).unwrap(),
        )
        .unwrap();
    assert!(
        draft
            .publish_signalling(grant(draft.identity(), 10, 2, 1, 0), at(0), &mut f.radio)
            .unwrap()
    );
    let receipt = draft.submission();
    let input = draft.finish().ok().unwrap();
    assert!(writer.accepted(WaveformWindow {
        first_sample: SampleInstant {
            epoch: f.key.epoch,
            index: 0
        },
        next_sample: SampleInstant {
            epoch: f.key.epoch,
            index: receipt.air_end().index * 2 + 1000
        },
        chip_origin_sample: SampleInstant {
            epoch: f.key.epoch,
            index: 0
        }
    }));
    assert!(receipt.viable());
    assert!(!receipt.submitted());
    drop(input);
    assert!(!receipt.viable());
    assert!(!receipt.submitted());
}

#[test]
fn incomplete_frame_retains_storage_and_abandoned_admitted_data_requires_recovery() {
    let mut f = fixture(false);
    let draft = f
        .bank
        .begin_frame(
            f.plan.clone(),
            &f.ports.control,
            at(0),
            Tfci::new(6).unwrap(),
        )
        .unwrap();
    assert!(!draft.complete());
    let mut draft = draft.finish().err().unwrap();
    assert!(
        draft
            .publish_signalling(grant(draft.identity(), 10, 2, 1, 0), at(0), &mut f.radio)
            .unwrap()
    );
    let receipt = draft.submission();
    drop(draft);
    assert!(!receipt.viable());
    assert!(matches!(
        f.bank.begin_frame(
            f.plan.clone(),
            &f.ports.control,
            at(0),
            Tfci::new(6).unwrap()
        ),
        Err(FrameBuildError::Bearer(BearerError::RecoveryRequired))
    ));
}

#[test]
fn empty_draft_drop_and_invalid_grants_do_not_consume_srb_or_poison_bank() {
    let mut f = fixture(false);
    let before = f
        .radio
        .registry
        .get(f.key)
        .unwrap()
        .control
        .window_positions();
    let mut draft = f
        .bank
        .begin_frame(
            f.plan.clone(),
            &f.ports.control,
            at(0),
            Tfci::new(6).unwrap(),
        )
        .unwrap();
    assert!(
        draft
            .publish_signalling(grant(draft.identity(), 10, 2, 0, 0), at(0), &mut f.radio)
            .is_err()
    );
    assert!(
        draft
            .publish_signalling(grant(draft.identity(), 10, 2, 1, 1), at(0), &mut f.radio)
            .is_err()
    );
    drop(draft);
    assert_eq!(
        f.radio
            .registry
            .get(f.key)
            .unwrap()
            .control
            .window_positions(),
        before
    );
    assert!(
        f.bank
            .begin_frame(
                f.plan.clone(),
                &f.ports.control,
                at(0),
                Tfci::new(6).unwrap()
            )
            .is_ok()
    );
}

#[test]
fn middle_of_tti_format_change_is_rejected_before_rlc_consumption() {
    let mut f = fixture(false);
    frame_ready(&mut f, 0);
    let before = f
        .radio
        .registry
        .get(f.key)
        .unwrap()
        .control
        .window_positions();
    assert!(
        f.bank
            .begin_frame(
                f.plan.clone(),
                &f.ports.control,
                at(CHIPS_PER_FRAME),
                Tfci::new(0).unwrap()
            )
            .is_err()
    );
    assert_eq!(
        f.radio
            .registry
            .get(f.key)
            .unwrap()
            .control
            .window_positions(),
        before
    );
    frame_ready(&mut f, 1);
}

#[test]
fn foreign_submission_pipeline_is_rejected_before_encoder_mutation() {
    let mut f = fixture(false);
    let other = source(false);
    let (_other_runtime, other_ports) = DedicatedTransmitRuntime::new(
        &other,
        count(1),
        count(4),
        count(2_000_000),
        TransmitRuntimeLimits {
            command_capacity: count(1),
            commands_per_step: count(1),
            feedback_capacity: count(1),
            feedback_per_step: count(1),
            maximum_frame_payload_bits: count(10_000),
            maximum_inflight_retained_bits: count(2_000_000),
        },
    )
    .unwrap();
    let mut draft = f
        .bank
        .begin_frame(
            f.plan.clone(),
            &other_ports.control,
            at(0),
            Tfci::new(6).unwrap(),
        )
        .unwrap();
    assert!(
        draft
            .publish_signalling(grant(draft.identity(), 10, 2, 1, 0), at(0), &mut f.radio)
            .unwrap()
    );
    let receipt = draft.submission();
    f.ports
        .control
        .frame(draft.finish().ok().unwrap())
        .ok()
        .unwrap();
    f.runtime.service().unwrap();
    assert!(matches!(
        f.ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Frame {
            result: Err(TransmitError::Identity),
            ..
        }
    ));
    assert!(!receipt.viable());
}

#[test]
fn combined_voice_packet_and_signalling_fill_one_complete_frame() {
    let mut f = fixture(true);
    f.bank
        .enqueue_sdu(BearerId::new(20).unwrap(), &[0x51; 200])
        .unwrap();
    let mut draft = f
        .bank
        .begin_frame(
            f.plan.clone(),
            &f.ports.control,
            at(0),
            Tfci::new(23).unwrap(),
        )
        .unwrap();
    f.radio
        .with_bearer_access(f.key, |ciphers, _, _| {
            for (transport, bearer, format, width) in
                [(6, 10, 5, 81), (7, 11, 1, 103), (8, 12, 1, 60)]
            {
                draft
                    .publish_transparent(
                        grant(draft.identity(), transport, bearer, format, 0),
                        &[Bits::new(&[0x55; 13], width).unwrap()],
                        ciphers,
                    )
                    .unwrap();
            }
            for _ in 0..4 {
                assert!(
                    draft
                        .publish_sequenced(grant(draft.identity(), 9, 20, 1, 0), at(0), ciphers)
                        .unwrap()
                );
            }
        })
        .unwrap();
    assert!(!draft.complete());
    assert!(
        draft
            .publish_signalling(grant(draft.identity(), 10, 2, 1, 0), at(0), &mut f.radio)
            .unwrap()
    );
    assert!(draft.complete());
    f.ports
        .control
        .frame(draft.finish().ok().unwrap())
        .ok()
        .unwrap();
    f.runtime.service().unwrap();
    assert!(matches!(
        f.ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Frame { result: Ok(_), .. }
    ));
}

#[test]
fn full_command_queue_returns_the_complete_frame_for_retry_without_false_submission() {
    let mut f = fixture(false);
    let mut draft = f
        .bank
        .begin_frame(
            f.plan.clone(),
            &f.ports.control,
            at(0),
            Tfci::new(6).unwrap(),
        )
        .unwrap();
    assert!(
        draft
            .publish_signalling(grant(draft.identity(), 10, 2, 1, 0), at(0), &mut f.radio)
            .unwrap()
    );
    let receipt = draft.submission();
    let input = draft.finish().ok().unwrap();
    let pointer = input.transports.last().unwrap().bits.as_ptr();
    for _ in 0..8 {
        f.ports.control.observe(f.bank.identity()).unwrap();
    }
    let input = f.ports.control.frame(input).err().unwrap();
    assert_eq!(input.transports.last().unwrap().bits.as_ptr(), pointer);
    assert!(receipt.viable());
    assert!(!receipt.submitted());
    f.runtime.service().unwrap();
    for _ in 0..8 {
        f.ports.control.try_event().unwrap();
    }
    f.ports.control.frame(input).ok().unwrap();
    f.runtime.service().unwrap();
    assert!(matches!(
        f.ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Frame { result: Ok(_), .. }
    ));
    assert!(!receipt.submitted());
    f.runtime.close();
    assert!(!receipt.viable());
}

#[test]
fn failed_tpc_synthesis_invalidates_pending_dedicated_receipts() {
    for good_slots in [0, 15] {
        let mut f = fixture(false);
        let receipt = frame_ready(&mut f, 0);
        for frame in 1..4 {
            frame_ready(&mut f, frame);
        }
        f.ports.control.arm(f.bank.identity()).unwrap();
        f.runtime.service().unwrap();
        f.ports.control.try_event().unwrap();
        if good_slots != 0 {
            feedback(&mut f, 0, good_slots);
        }
        let mut samples = vec![num_complex::Complex32::default(); f.source.samples_per_slot()];
        for _ in 0..good_slots {
            f.runtime
                .generate_slot(&mut f.source, &mut samples)
                .unwrap();
        }
        assert!(receipt.viable());
        assert!(!receipt.submitted());
        f.runtime
            .generate_slot(&mut f.source, &mut samples)
            .unwrap();
        assert!(!receipt.viable());
        assert!(!receipt.submitted());
        assert_eq!(
            f.runtime
                .bank()
                .session(f.bank.identity())
                .unwrap()
                .failure(),
            Some(&TransmitError::TpcUnavailable)
        );
    }
}

#[test]
fn closure_invalidates_queued_frame_evidence_without_waiting_for_destruction() {
    for producer in [false, true] {
        let mut f = fixture(false);
        let mut draft = f
            .bank
            .begin_frame(
                f.plan.clone(),
                &f.ports.control,
                at(0),
                Tfci::new(6).unwrap(),
            )
            .unwrap();
        assert!(
            draft
                .publish_signalling(grant(draft.identity(), 10, 2, 1, 0), at(0), &mut f.radio)
                .unwrap()
        );
        let receipt = draft.submission();
        f.ports
            .control
            .frame(draft.finish().ok().unwrap())
            .ok()
            .unwrap();
        assert!(receipt.viable());
        if producer {
            drop(f.ports.control);
        } else {
            f.runtime.close();
        }
        assert!(!receipt.viable());
        assert!(!receipt.submitted());
    }
}

#[test]
fn closed_runtime_cannot_consume_signalling_from_an_already_reserved_draft() {
    let mut f = fixture(false);
    let before = f
        .radio
        .registry
        .get(f.key)
        .unwrap()
        .control
        .published_status(f.key)
        .unwrap();
    let mut draft = f
        .bank
        .begin_frame(
            f.plan.clone(),
            &f.ports.control,
            at(0),
            Tfci::new(6).unwrap(),
        )
        .unwrap();
    f.runtime.close();
    assert_eq!(
        draft.publish_signalling(grant(draft.identity(), 10, 2, 1, 0), at(0), &mut f.radio),
        Err(FrameBuildError::Bearer(BearerError::RecoveryRequired))
    );
    drop(draft);
    assert_eq!(
        f.radio
            .registry
            .get(f.key)
            .unwrap()
            .control
            .published_status(f.key)
            .unwrap(),
        before
    );
}

#[test]
fn actual_port_payload_budget_and_canonical_plan_are_checked_before_any_publication() {
    let mut f = fixture(false);
    let before = f
        .radio
        .registry
        .get(f.key)
        .unwrap()
        .control
        .published_status(f.key)
        .unwrap();
    let (_small_runtime, small_ports) = DedicatedTransmitRuntime::new(
        &f.source,
        count(1),
        count(4),
        count(2_000_000),
        TransmitRuntimeLimits {
            command_capacity: count(1),
            commands_per_step: count(1),
            feedback_capacity: count(1),
            feedback_per_step: count(1),
            maximum_frame_payload_bits: count(1),
            maximum_inflight_retained_bits: count(2_000_000),
        },
    )
    .unwrap();
    assert!(matches!(
        f.bank.begin_frame(
            f.plan.clone(),
            &small_ports.control,
            at(0),
            Tfci::new(6).unwrap()
        ),
        Err(FrameBuildError::Bearer(BearerError::Capacity))
    ));
    let foreign_plan = Arc::new(
        CompiledRel99::new(
            f.plan.configuration().clone(),
            count(200_000),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(300_000),
                decode: DecodeBudget::new(std::num::NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    );
    assert!(matches!(
        f.bank
            .begin_frame(foreign_plan, &f.ports.control, at(0), Tfci::new(6).unwrap()),
        Err(FrameBuildError::Bearer(BearerError::Mapping))
    ));
    assert_eq!(
        f.radio
            .registry
            .get(f.key)
            .unwrap()
            .control
            .published_status(f.key)
            .unwrap(),
        before
    );
    assert!(
        f.bank
            .begin_frame(
                f.plan.clone(),
                &f.ports.control,
                at(0),
                Tfci::new(6).unwrap()
            )
            .is_ok()
    );
}
