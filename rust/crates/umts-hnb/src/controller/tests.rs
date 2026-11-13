// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
pub(crate) mod duplex;
mod media;
use crate::{
    common_rx::{
        CommonRx,
        tests::{advance_tx, pipeline, sampled_requests},
    },
    core_link::tests::{Mock, Peer, ready},
    radio_io::{TxPreparation, TxPump},
    signalling::{
        CommonIngressWork, FachTarget,
        tests::{COMPLETE, INITIAL, limits, peer, request},
    },
    test_support::{at, count},
};
use sdr_hal::memory::{IqBlock, MemoryRadioHandle};
use umts_dsp::{
    prach::{PrachCode, PreambleSignature},
    tfci::{Tfci, TfciDecision},
};
use umts_iuh::{ContextId, Domain, hnbap, rua};
use umts_mac::{CommonIdentity, FddCommonChannel, LogicalChannelId, encode_common};
use umts_rlc::{acknowledged::AmEntity, transparent::TmLimits};
use umts_types::{CnDomain, bits::Bits};

fn settings() -> ControllerSettings {
    ControllerSettings {
        maximum_ingress_age_chips: NonZeroU64::new(3_840_000).unwrap(),
        preparation_guard_chips: NonZeroU64::new(5120).unwrap(),
        maximum_future_frames: count(2),
        fach_scan_budget: count(25),
        retirement_scan_budget: count(4),
    }
}

fn dedicated_limits() -> crate::dedicated_rx::SampleRuntimeLimits {
    use crate::dedicated_rx::{ReceiveResources, SampleRuntimeLimits, ShardLimits};
    SampleRuntimeLimits {
        shard: ShardLimits {
            maximum_ues: count(2),
            maximum_sessions_per_step: count(2),
            resources: ReceiveResources {
                physical_sample_bytes: 8_000_000,
                coded_bits: 1_000_000,
                output_bytes: 1_000_000,
                demultiplexed_bits: 1_000_000,
                channels: 8,
            },
        },
        history_samples: count(100_000),
        maximum_shared_bytes: 4_000_000,
        command_capacity: count(2),
        commands_per_step: count(1),
        output_slots_per_ue: count(1),
        maximum_output_bits: count(5000),
        maximum_output_bytes: 10_000,
    }
}

#[test]
fn admission_binds_the_actual_preparation_worker_and_requires_explicit_frame_policy() {
    use crate::{
        dedicated_controller::{DedicatedController, DedicatedControllerLimits},
        dedicated_tx::{DedicatedTransmitRuntime, TransmitRuntimeLimits},
        rel99_admission::{AdmissionLimits, Rel99Admission},
    };
    for foreign in [false, true] {
        let (mut preparation, _pump, mut controller, mut receive, _hardware, _peer) = setup();
        let port = receive.configure_dedicated(dedicated_limits()).unwrap();
        controller
            .attach_dedicated(DedicatedController::new(
                port,
                DedicatedControllerLimits {
                    receipt_visits: count(2),
                    service_visits: count(2),
                    maximum_bearer_bytes: 2_000_000,
                },
            ))
            .ok()
            .unwrap();
        let runtime_limits = TransmitRuntimeLimits {
            command_capacity: count(8),
            commands_per_step: count(2),
            feedback_capacity: count(2),
            feedback_per_step: count(2),
            maximum_frame_payload_bits: count(10_000),
            maximum_inflight_retained_bits: count(2_000_000),
        };
        let ports = preparation
            .dedicated_ports(count(2), count(4), count(2_000_000), runtime_limits)
            .unwrap();
        let (_foreign_runtime, other_ports) = DedicatedTransmitRuntime::new(
            preparation.source(),
            count(2),
            count(4),
            count(2_000_000),
            runtime_limits,
        )
        .unwrap();
        let ports = if foreign { other_ports } else { ports };
        let _feedback = ports.feedback;
        let admission = Rel99Admission::new(
            ports.control,
            count(2),
            AdmissionLimits {
                visits: count(2),
                command_results: count(2),
                prefill_frames: count(4),
                maximum_lead_frames: count(8),
                health_interval_chips: NonZeroU64::new(1_000_000).unwrap(),
            },
        )
        .ok()
        .unwrap();
        controller.attach_admission(admission).ok().unwrap();
        assert_eq!(controller.paired_with(&preparation, &receive), !foreign);
        let base = Instant::now();
        assert!(matches!(
            controller.step(time(base, 0), |_, _| Ok::<_, ()>(())),
            Err(ControllerError::Configuration)
        ));
        if !foreign {
            let step = controller
                .step_with_admission(
                    time(base, 0),
                    |_, _| Ok::<_, ()>(()),
                    |_| false,
                    |_, _| Err(SignallingError::Message),
                    |opportunity| opportunity.idle,
                    |_, _| Ok(()),
                )
                .unwrap();
            let admission = step.admission.unwrap().unwrap();
            assert_eq!(admission.visits, 0);
            assert_eq!(admission.command_results, 0);
        }
    }
}

#[test]
fn dedicated_controller_requires_the_same_receive_pipeline_not_just_the_same_epoch() {
    use crate::dedicated_controller::{DedicatedController, DedicatedControllerLimits};
    let (preparation, _pump, mut controller, mut receive, _hardware, _peer) = setup();
    let port = receive.configure_dedicated(dedicated_limits()).unwrap();
    let dedicated = DedicatedController::new(
        port,
        DedicatedControllerLimits {
            receipt_visits: count(2),
            service_visits: count(2),
            maximum_bearer_bytes: 1_000_000,
        },
    );
    assert!(controller.attach_dedicated(dedicated).is_ok());
    assert!(controller.paired_with(&preparation, &receive));
    let (_preparation, _pump, mut other, other_receive, _hardware, _peer) = setup();
    let (_runtime, port) = crate::dedicated_rx::DedicatedSampleRuntime::new(
        other.epoch(),
        umts_dsp::pulse_shaping::SamplesPerChip::Two,
        dedicated_limits(),
    )
    .unwrap();
    let dedicated = DedicatedController::new(
        port,
        DedicatedControllerLimits {
            receipt_visits: count(2),
            service_visits: count(2),
            maximum_bearer_bytes: 1_000_000,
        },
    );
    assert!(other.attach_dedicated(dedicated).is_ok());
    assert!(!other.paired_with(&_preparation, &other_receive));
}

#[test]
fn common_controller_services_attached_dedicated_lifecycle_without_an_upper_consumer() {
    use crate::dedicated_controller::{DedicatedController, DedicatedControllerLimits};
    let (_preparation, _pump, mut controller, mut receive, _hardware, _peer) = setup();
    let port = receive.configure_dedicated(dedicated_limits()).unwrap();
    let dedicated = DedicatedController::new(
        port,
        DedicatedControllerLimits {
            receipt_visits: count(2),
            service_visits: count(2),
            maximum_bearer_bytes: 1_000_000,
        },
    );
    assert!(controller.attach_dedicated(dedicated).is_ok());
    let report = step(&mut controller, Instant::now(), 0);
    let dedicated = report.dedicated_bearers.unwrap().unwrap();
    assert_eq!(dedicated.receipts, 0);
    assert_eq!(dedicated.serviced, 0);
    assert_eq!(dedicated.delivered, 0);
    assert!(dedicated.failure.is_none());
}

fn time(base: Instant, chip: u64) -> ControllerTime {
    ControllerTime::new(
        at(chip),
        ControlTime(10 + chip / 3840),
        base + std::time::Duration::from_nanos(chip * 1_000_000_000 / 3_840_000),
    )
}

fn setup() -> (
    TxPreparation,
    TxPump,
    CommonController<Mock>,
    CommonRx,
    MemoryRadioHandle,
    Peer,
) {
    let (preparation, pump, port, rx, rach, hardware) = pipeline(4);
    let mut radio = CommonSignalling::new(&port, 1, 1, limits()).unwrap();
    let (core, peer) = ready(&mut radio);
    let common = ControlIngress::new(
        at(0).epoch,
        TmLimits {
            maximum_pdus: count(2),
            maximum_pdu_bits: count(254),
            maximum_sdu_bits: count(254),
        },
        count(1),
    )
    .unwrap();
    let controller = CommonController::new(radio, port, rach, common, core, settings()).unwrap();
    (preparation, pump, controller, rx, hardware, peer)
}

fn step(
    controller: &mut CommonController<Mock>,
    base: Instant,
    chip: u64,
) -> ControllerStep<(), ()> {
    let step = controller.step(time(base, chip), |_, _| Err(())).unwrap();
    assert!(step.core.is_ok());
    assert!(step.security.as_ref().is_some_and(Result::is_ok));
    assert!(step.retired.is_ok());
    assert!(step.timers.is_ok());
    assert!(step.collected.is_ok());
    assert!(step.fach.is_ok());
    if let Some(event) = step.transmit {
        assert!(!matches!(
            event.stage,
            crate::common_tx::PlanStage::Rejected(_)
        ));
    }
    step
}

fn metadata(start: u64, blocks: usize) -> DecodedPrachMessage {
    DecodedPrachMessage {
        start: at(start),
        code: PrachCode::new(7504).unwrap(),
        signature: PreambleSignature::new(13).unwrap(),
        tfci: TfciDecision {
            tfci: Tfci::new((blocks - 1) as u16).unwrap(),
            normalized_score: 1.0,
            normalized_margin: None,
        },
        payload_bits: blocks * 256,
    }
}

fn common_block(variant: u8) -> [u8; 32] {
    let request = request(variant);
    let bytes = rasn::uper::encode(request.value.as_ref()).unwrap();
    let mut output = [0; 32];
    encode_common(
        FddCommonChannel::Rach,
        CommonHeader::CommonControl,
        Bits::new(&bytes, request.consumed_bits).unwrap(),
        &mut output,
    )
    .unwrap();
    output
}

fn send_dedicated(
    runtime: &mut CommonRx,
    entity: &mut AmEntity,
    identity: u16,
    logical: u8,
    received: u64,
    bytes: &[u8],
) {
    entity.enqueue(bytes).unwrap();
    entity
        .publish_next(at(received), |publication| {
            let mut mac = [0; 32];
            encode_common(
                FddCommonChannel::Rach,
                CommonHeader::Dedicated {
                    identity: CommonIdentity::Cell(identity),
                    channel: LogicalChannelId::new(logical).unwrap(),
                },
                Bits::new(publication.bytes, publication.bytes.len() * 8).unwrap(),
                &mut mac,
            )
            .unwrap();
            runtime.inject_transport(metadata(received, 1), 256, 1, &mac);
            Ok::<_, ()>(())
        })
        .unwrap()
        .unwrap();
}

#[test]
fn sampled_automatic_requests_drive_owned_controller_and_periodic_fach_without_manual_dispatch() {
    let (mut preparation, mut pump, mut controller, mut rx, hardware, _peer) = setup();
    let samples = sampled_requests();
    let expected = samples.len() as u64;
    hardware
        .input
        .send(IqBlock {
            samples,
            time_ticks: Some(1_000_000_000),
            discontinuity: false,
        })
        .unwrap();
    rx.start(None).unwrap();
    let base = Instant::now();
    let mut keys = Vec::new();
    let mut published = 0;
    let mut count = 0;
    while rx.statistics().radio.input_samples < expected {
        count += 1;
        assert!(count < 1000);
        rx.cycle().unwrap();
        let now = rx.statistics().radio.input_samples / 2;
        advance_tx(&mut preparation, &mut pump, &hardware, now);
        let current = step(&mut controller, base, now);
        if let Some(common) = current.common {
            let CommonIngressWork::ConnectionRequested(key) = common.work else {
                panic!("request not dispatched")
            };
            keys.push(key);
        }
        published += usize::from(matches!(
            current.fach,
            Ok(FachWork::Published(FachTarget::Common))
        ));
    }
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1]);
    assert_eq!(controller.radio().counters().created, 2);
    assert!(published > 0);
    for key in keys {
        assert_eq!(
            controller.radio().phase(key),
            Some(EstablishmentPhase::AwaitingComplete)
        );
    }
    assert_eq!(rx.statistics().access.admitted, 2);
    assert_eq!(rx.statistics().access.decoded, 2);
    rx.stop().unwrap();
}

#[test]
fn controller_actuates_a_received_core_security_request_without_manual_procedure_calls() {
    let (mut preparation, mut pump, mut controller, mut rx, hardware, core) = setup();
    let base = Instant::now();
    let key = controller
        .radio
        .request(&controller.port, at(0), &request(0))
        .unwrap();
    let identity = controller.radio.assigned(key).unwrap().crnti();
    let mut control = peer(preparation.source());
    let mut nas = peer(preparation.source());
    advance_tx(&mut preparation, &mut pump, &hardware, 100000);
    let mut now = 100000;
    send_dedicated(&mut rx, &mut control, identity, 2, 10, &COMPLETE);
    for _ in 0..32 {
        step(&mut controller, base, now);
        now += 1;
        if controller.radio.phase(key) == Some(EstablishmentPhase::Established) {
            break;
        }
    }
    assert_eq!(
        controller.radio.phase(key),
        Some(EstablishmentPhase::Established)
    );
    send_dedicated(&mut rx, &mut nas, identity, 3, 20, &INITIAL);
    let mut registered = false;
    for _ in 0..64 {
        let work = step(&mut controller, base, now);
        now += 1;
        if work.core.unwrap().uplink_published {
            registered = true;
            break;
        }
    }
    assert!(registered);
    let hnbap::Message::UeRegister { identity, .. } = core.last_hnbap() else {
        panic!("missing UE registration");
    };
    let context = ContextId::new(100 + key.slot).unwrap();
    core.hnbap(hnbap::Message::UeAccepted {
        identity,
        context,
        membership: None,
    });
    let mut connected = false;
    for _ in 0..64 {
        let work = step(&mut controller, base, now);
        now += 1;
        if work.core.unwrap().uplink_published {
            connected = true;
            break;
        }
    }
    assert!(connected);
    assert!(
        matches!(core.last_rua(), rua::Message::Connect { domain: Domain::Cs, context: actual, .. } if actual == context)
    );
    let command = umts_iuh::ranap::SecurityCommand {
        integrity: umts_iuh::ranap::Protection {
            algorithms: vec![umts_iuh::ranap::Algorithm(0)],
            key: umts_iuh::ranap::Key::new([0x11; 16]),
        },
        encryption: None,
        key_status: umts_iuh::ranap::KeyStatus::New,
    };
    core.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: umts_iuh::ranap::encode_security(&command, umts_iuh::wire::Limits::default())
            .unwrap(),
    });
    let mut requested = false;
    for _ in 0..64 {
        let work = step(&mut controller, base, now);
        now += 1;
        if let Some(Ok(SecurityAction::CapabilitiesRequested {
            key: owner,
            domain,
            transaction,
        })) = work.security
        {
            assert_eq!(owner, key);
            assert_eq!(domain, Domain::Cs);
            assert_eq!(transaction.get(), 0);
            requested = true;
            break;
        }
    }
    assert!(requested);
    assert_eq!(
        controller.radio.security(key).unwrap().phase(Domain::Cs),
        crate::connection_security::Phase::AwaitingPreparation
    );
}

#[test]
fn per_pdu_retry_keeps_accepted_blocks_committed_and_retains_only_the_rejected_block() {
    let (_preparation, _pump, mut controller, mut rx, _hardware, _peer) = setup();
    let mut bytes = Vec::from(common_block(0));
    bytes.extend_from_slice(&common_block(1));
    rx.inject_transport(metadata(10, 2), 256, 2, &bytes);
    let mut accepted = Vec::new();
    assert!(matches!(
        controller
            .rach
            .process_pdu_one_with(|item| {
                accepted.push(item.index);
                assert_eq!(item.transport.start, at(10));
                ControlDecision::Consume(())
            })
            .unwrap(),
        Some(ControlDecision::Consume(()))
    ));
    for _ in 0..3 {
        assert!(matches!(
            controller
                .rach
                .process_pdu_one_with(|item| {
                    assert_eq!(item.index, 1);
                    assert_eq!(item.blocks, 2);
                    assert_eq!(item.pdu.unwrap().header, CommonHeader::CommonControl);
                    ControlDecision::Retry(())
                })
                .unwrap(),
            Some(ControlDecision::Retry(()))
        ));
    }
    assert_eq!(
        controller
            .rach
            .process_one_with(|_| ControlDecision::Consume(()))
            .err(),
        Some(IngressError::PartialTransport)
    );
    controller
        .rach
        .process_pdu_one_with(|item| {
            accepted.push(item.index);
            ControlDecision::Consume(())
        })
        .unwrap();
    assert_eq!(accepted, [0, 1]);
    assert!(
        controller
            .rach
            .process_pdu_one_with(|_| ControlDecision::Consume(()))
            .unwrap()
            .is_none()
    );
}

#[test]
fn common_backpressure_retries_original_block_and_malformed_peer_blocks_do_not_stall_it() {
    let (_preparation, _pump, mut controller, mut rx, _hardware, _peer) = setup();
    let base = Instant::now();
    let first = request(0);
    let bytes = rasn::uper::encode(first.value.as_ref()).unwrap();
    crate::common_control::tests::enqueue_sdu(
        &mut controller.common,
        &bytes,
        first.consumed_bits,
        at(0),
    );
    rx.inject_transport(metadata(10, 1), 256, 1, &common_block(1));
    assert_eq!(
        step(&mut controller, base, 20).rach.unwrap().work,
        RachWork::Deferred
    );
    let retry = step(&mut controller, base, 21).rach.unwrap();
    assert_eq!(retry.transport.start, at(10));
    assert_eq!(retry.block, 0);
    assert_eq!(retry.work, RachWork::CommonQueued);
    assert_eq!(controller.radio().counters().created, 2);
    let mut mixed = vec![0xff; 32];
    mixed.extend_from_slice(&common_block(2));
    rx.inject_transport(metadata(30, 2), 256, 2, &mixed);
    assert!(matches!(
        step(&mut controller, base, 31).rach.unwrap().work,
        RachWork::Rejected(RachRejection::Mac(_))
    ));
    assert_eq!(
        step(&mut controller, base, 32).rach.unwrap().work,
        RachWork::CommonQueued
    );
    assert_eq!(controller.radio().counters().created, 3);
}

#[test]
fn two_ue_cs_ps_nas_reaches_core_with_independent_contexts_after_delayed_radio_delivery() {
    let (mut preparation, mut pump, mut controller, mut rx, hardware, core) = setup();
    let base = Instant::now();
    let mut keys = Vec::new();
    for variant in 0..2 {
        keys.push(
            controller
                .radio
                .request(&controller.port, at(0), &request(variant))
                .unwrap(),
        );
    }
    let mut peers = Vec::new();
    for key in &keys {
        let control_peer = peer(preparation.source());
        let nas_peer = peer(preparation.source());
        peers.push((
            controller.radio.assigned(*key).unwrap().crnti(),
            control_peer,
            nas_peer,
        ));
    }
    advance_tx(&mut preparation, &mut pump, &hardware, 100000);
    let mut now = 100000;
    for (index, key) in keys.iter().enumerate() {
        let (identity, control, _) = &mut peers[index];
        send_dedicated(&mut rx, control, *identity, 2, 10 + index as u64, &COMPLETE);
        for _ in 0..32 {
            step(&mut controller, base, now);
            now += 1;
            if controller.radio.phase(*key) == Some(EstablishmentPhase::Established) {
                break;
            }
        }
        assert_eq!(
            controller.radio.phase(*key),
            Some(EstablishmentPhase::Established)
        );
    }
    for (index, key) in keys.iter().enumerate() {
        let (identity, _, nas) = &mut peers[index];
        send_dedicated(&mut rx, nas, *identity, 3, 20 + index as u64, &INITIAL);
        let mut registered = false;
        for _ in 0..64 {
            let work = step(&mut controller, base, now);
            now += 1;
            if work.core.unwrap().uplink_published {
                registered = true;
                break;
            }
        }
        assert!(registered);
        let hnbap::Message::UeRegister { identity, .. } = core.last_hnbap() else {
            panic!("missing UE registration")
        };
        let context = ContextId::new(100 + key.slot).unwrap();
        core.hnbap(hnbap::Message::UeAccepted {
            identity,
            context,
            membership: None,
        });
        let mut connected = false;
        for _ in 0..64 {
            let work = step(&mut controller, base, now);
            now += 1;
            if work.core.unwrap().uplink_published {
                connected = true;
                break;
            }
        }
        assert!(connected);
        assert!(
            matches!(core.last_rua(), rua::Message::Connect { domain: Domain::Cs, context: actual, .. } if actual == context)
        );
        let mut ps = INITIAL;
        ps[1] |= 0x80;
        let (identity, _, nas) = &mut peers[index];
        send_dedicated(&mut rx, nas, *identity, 3, 30 + index as u64, &ps);
        let mut published = false;
        for _ in 0..64 {
            let work = step(&mut controller, base, now);
            now += 1;
            if work.core.unwrap().uplink_published {
                published = true;
                break;
            }
        }
        assert!(published);
        assert!(
            matches!(core.last_rua(), rua::Message::Connect { domain: Domain::Ps, context: actual, .. } if actual == context)
        );
        assert_eq!(
            controller
                .radio
                .connection(*key, CnDomain::Cs)
                .unwrap()
                .context
                .get(),
            context.get()
        );
        assert_eq!(
            controller
                .radio
                .connection(*key, CnDomain::Ps)
                .unwrap()
                .context
                .get(),
            context.get()
        );
    }
    assert_eq!(controller.core.gateway().registered_ues(), 2);
    assert_ne!(
        controller
            .radio
            .connection(keys[0], CnDomain::Cs)
            .unwrap()
            .context,
        controller
            .radio
            .connection(keys[1], CnDomain::Cs)
            .unwrap()
            .context
    );
}

#[test]
fn controller_time_validation_is_before_mutation_and_never_schedules_outside_its_horizon() {
    let (_preparation, _pump, mut controller, _rx, _hardware, _peer) = setup();
    let base = Instant::now();
    step(&mut controller, base, 100);
    assert!(matches!(
        controller.step(time(base, 99), |_, _| Ok::<_, ()>(())),
        Err(ControllerError::Time)
    ));
    let mut foreign = time(base, 101);
    foreign.radio.epoch = umts_types::RadioEpoch::new(NonZeroU64::new(2).unwrap());
    assert!(matches!(
        controller.step(foreign, |_, _| Ok::<_, ()>(())),
        Err(ControllerError::Epoch)
    ));
    controller.last_frame = Some(2 * CHIPS_PER_FRAME);
    assert_eq!(controller.frame(at(100)).unwrap(), None);
}

#[test]
fn automatic_retirement_cancellation_does_not_cancel_another_ue_or_reused_generation() {
    let (_preparation, _pump, mut controller, _rx, _hardware, _peer) = setup();
    let first = controller
        .radio
        .request(&controller.port, at(0), &request(0))
        .unwrap();
    let second = controller
        .radio
        .request(&controller.port, at(0), &request(1))
        .unwrap();
    let bits = controller.port.information().sib5().secondary_channels()[1].transports()[0]
        .formats
        .groups[0]
        .transport_block_bits as usize;
    let payload = vec![umts_dsp::bits::Bit::One; bits];
    let old = controller
        .port
        .publish_transport(at(CHIPS_PER_FRAME), 1, &payload, &[], Some(first))
        .unwrap();
    let other = controller
        .port
        .publish_transport(at(2 * CHIPS_PER_FRAME), 1, &payload, &[], Some(second))
        .unwrap();
    controller.radio.retire(first).unwrap();
    let replacement = controller
        .radio
        .request(&controller.port, at(0), &request(2))
        .unwrap();
    assert_eq!(replacement.slot, first.slot);
    assert_ne!(replacement.generation, first.generation);
    let work = step(&mut controller, Instant::now(), 1);
    assert_eq!(work.cancelled, 1);
    assert_eq!(
        old.plan_stage(),
        Some(crate::common_tx::PlanStage::Cancelled)
    );
    assert_eq!(
        other.plan_stage(),
        Some(crate::common_tx::PlanStage::Queued)
    );
}

impl<T: Transport> CommonController<T> {
    pub fn step<U>(
        &mut self,
        now: ControllerTime,
        other_core: impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
    ) -> Result<ControllerStep<T::Error, U>, ControllerError> {
        self.step_with_bearers(now, other_core, |_| false)
    }
    pub fn step_with_bearers<U>(
        &mut self,
        now: ControllerTime,
        other_core: impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
        upper: impl FnMut(crate::dedicated_bearers::SduDelivery<'_>) -> bool,
    ) -> Result<ControllerStep<T::Error, U>, ControllerError> {
        self.step_with_control(now, other_core, upper, |_, _| Err(SignallingError::Message))
    }
    pub fn step_with_control<U>(
        &mut self,
        now: ControllerTime,
        other_core: impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
        upper: impl FnMut(crate::dedicated_bearers::SduDelivery<'_>) -> bool,
        control: impl FnMut(
            crate::signalling::ReceivedControl<'_>,
            crate::signalling::ControlSecurity<'_>,
        ) -> Result<(), SignallingError>,
    ) -> Result<ControllerStep<T::Error, U>, ControllerError> {
        if self.admission.is_some() {
            return Err(ControllerError::Configuration);
        }
        self.step_with_admission(
            now,
            other_core,
            upper,
            control,
            |opportunity| opportunity.idle,
            |_, _| Ok(()),
        )
    }
    pub fn step_with_admission<U>(
        &mut self,
        now: ControllerTime,
        other_core: impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
        upper: impl FnMut(crate::dedicated_bearers::SduDelivery<'_>) -> bool,
        control: impl FnMut(
            crate::signalling::ReceivedControl<'_>,
            crate::signalling::ControlSecurity<'_>,
        ) -> Result<(), SignallingError>,
        select: impl FnMut(crate::rel99_admission::FrameOpportunity) -> umts_dsp::tfci::Tfci,
        fill: impl FnMut(
            &mut crate::dedicated_bearers::DownlinkFrame<'_>,
            &mut CommonSignalling,
        ) -> Result<(), crate::dedicated_bearers::FrameBuildError>,
    ) -> Result<ControllerStep<T::Error, U>, ControllerError> {
        self.step_with_frame_policy(
            now,
            other_core,
            upper,
            control,
            &mut crate::rel99_admission::ClosurePolicy { select, fill },
        )
    }
    pub fn step_with_frame_policy<U>(
        &mut self,
        now: ControllerTime,
        other_core: impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
        upper: impl FnMut(crate::dedicated_bearers::SduDelivery<'_>) -> bool,
        control: impl FnMut(
            crate::signalling::ReceivedControl<'_>,
            crate::signalling::ControlSecurity<'_>,
        ) -> Result<(), SignallingError>,
        policy: &mut impl crate::rel99_admission::FramePolicy,
    ) -> Result<ControllerStep<T::Error, U>, ControllerError> {
        self.step_with_duplex_policy(
            now,
            other_core,
            control,
            &mut crate::rel99_admission::CallbackDuplexPolicy { policy, upper },
        )
    }
}
