// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    controller::tests::duplex::{Environment, environment},
    rel99_admission::AdmissionPhase,
    rel99_planner::tests::ps_request,
    rel99_traffic::Rel99Traffic,
    signalling::{
        CapabilityReadiness, SignallingError,
        tests::{COMPLETE, INITIAL, peer, request, send, send_capability_report_with_transaction},
    },
    test_support::{at, count},
};
use std::{
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use umts_iuh::{
    ContextId, Domain, hnbap, ranap::rab::Assignment, rua, session::ControlTime, wire::Limits,
};
use umts_rrc::{dedicated::SetupPhase, nas::SignallingBearer};
use umts_types::{UeKey, time::CHIPS_PER_SLOT};
use umts_user_plane::packet::{BoundDatagrams, Datagrams};

const PACKET_RATE: u32 = 12_800;
mod inactivity;
mod iu;
mod releases;

struct Memory {
    local: SocketAddr,
    polls: Arc<AtomicUsize>,
}

impl Datagrams for Memory {
    fn receive(&mut self, _: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.polls.fetch_add(1, Ordering::Relaxed);
        Ok(None)
    }

    fn send(&mut self, _: &[u8], _: SocketAddr) -> io::Result<bool> {
        panic!("initial dispatch cannot send packet network output")
    }
}

impl BoundDatagrams for Memory {
    fn local_address(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }
}

fn config() -> Config {
    let mut config = Config::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../rust/test-support/fixtures/config/serving.yaml"),
    )
    .unwrap();
    let profile: Config = serde_yaml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-support/fixtures/config/rel99.yaml"
    )))
    .unwrap();
    config.cell.packet_modes = Some(vec![umts_config::PacketMode::Rel99]);
    config.radio = profile.radio;
    config.cell.physical = profile.cell.physical;
    config.cell.access = profile.cell.access;
    config.cell.common_channels = profile.cell.common_channels;
    config.rel99 = profile.rel99;
    config.hspa.downlink = umts_config::HspaDownlinkSettings {
        resources: config.hspa.downlink.resources,
        ..profile.hspa.downlink
    };
    config.hspa.uplink = umts_config::HspaUplinkSettings {
        resources: config.hspa.uplink.resources,
        ..profile.hspa.uplink
    };
    config.receiver.turbo_iterations = profile.receiver.turbo_iterations;
    config.power_control.uplink = profile.power_control.uplink;
    config.power_control.downlink.pich_power_offset_db =
        profile.power_control.downlink.pich_power_offset_db;
    config.hspa.downlink.qam64_enabled = false;
    config.hspa.downlink.compact_common_channels = false;
    config.cell.physical.downlink_scrambling_code = 469;
    config.cell.access.prach.scrambling_code_word_number = 0;
    config.cell.access.prach.signature = 13;
    config.cell.access.prach.subchannel = 7;
    config.runtime.maximum_ues = 2;
    config.power_control.downlink.cpich_amplitude = crate::test_support::amplitudes().primary.cpich;
    config.runtime.maximum_sdu_bytes = 2048;
    config.receiver.dedicated.output_slots = 1;
    config.receiver.dedicated.work_per_cycle = 2;
    let buffers = config.rel99.physical.planner.services.packet[0]
        .am
        .as_mut()
        .unwrap();
    buffers.buffers.maximum_sdu_bytes = 2048;
    buffers.buffers.maximum_buffer_bytes = 100_000;
    buffers.ue.uplink_window = 32;
    buffers.ue.downlink_window = 32;
    config.validate().unwrap();
    config
}

fn traffic(config: &Config) -> Rel99Traffic<Memory> {
    traffic_with_polls(config, Arc::default())
}

fn traffic_with_polls(config: &Config, polls: Arc<AtomicUsize>) -> Rel99Traffic<Memory> {
    let plan = crate::serving::media::MediaPlan::new(config).unwrap();
    let local = plan.bind_address();
    plan.construct(Memory { local, polls }).unwrap()
}

fn register(env: &mut Environment) -> (UeKey, ContextId) {
    env.with_parts(|core, radio, port, source, gateway, _| {
        let key = radio.request(port, at(0), &request(0)).unwrap();
        send(
            radio,
            key,
            SignallingBearer::ControlAm,
            &mut peer(source),
            &COMPLETE,
        );
        assert_eq!(radio.process_one(), Some(Ok(key)));
        send(
            radio,
            key,
            SignallingBearer::NasAm,
            &mut peer(source),
            &INITIAL,
        );
        assert_eq!(
            radio.process_one(),
            Some(Err(SignallingError::RegistrationPending))
        );
        assert!(
            core.drive(ControlTime(3), radio, |_, _| Err::<(), _>(
                "unexpected core event"
            ))
            .unwrap()
            .uplink_published
        );
        let hnbap::Message::UeRegister { identity, .. } = gateway.last_hnbap() else {
            panic!("missing actual UE registration")
        };
        let context = ContextId::new(100 + key.slot).unwrap();
        gateway.hnbap(hnbap::Message::UeAccepted {
            identity,
            context,
            membership: None,
        });
        core.drive(ControlTime(4), radio, |_, _| {
            Err::<(), _>("unexpected core event")
        })
        .unwrap();
        assert_eq!(radio.process_one(), Some(Ok(key)));
        core.drive(ControlTime(5), radio, |_, _| {
            Err::<(), _>("unexpected core event")
        })
        .unwrap();
        (key, context)
    })
}

fn retain_packet(env: &mut Environment, assignments: &mut RabHandoff, context: ContextId) {
    env.with_parts(|core, radio, _, _, gateway, _| {
        gateway.rua(rua::Message::Connect {
            domain: Domain::Ps,
            context,
            cause: rua::EstablishmentCause::Normal,
            selector: None,
            membership: None,
            ranap: Assignment {
                setup: vec![ps_request(PACKET_RATE)],
                release: vec![],
            }
            .encode(Limits::default())
            .unwrap(),
        });
        core.drive(ControlTime(6), radio, |_, event| {
            assignments.offer(event).map(|_| ())
        })
        .unwrap();
        assert_eq!(assignments.confirm(core.gateway()), 1);
    });
}

fn adapt_signalling(config: &mut Config, env: &mut Environment, key: UeKey) {
    env.with_parts(|_, radio, _, _, _, _| {
        let retained = radio.retained_signalling(key).unwrap();
        let control = retained.iter().find(|bearer| bearer.id == 2).unwrap();
        config.rel99.physical.planner.signalling.um_uplink_bits = control.uplink_pdu_bits;
        config.rel99.physical.planner.signalling.um_downlink_bits = control.downlink_pdu_bits;
    });
}

fn service(
    plan: &mut BearerCoordinator,
    env: &mut Environment,
    traffic: &mut Rel99Traffic<Memory>,
) -> BearerStep {
    let control = Instant::now();
    plan.service(
        &mut env.controller,
        traffic,
        ControllerTime::new(at(0), ControlTime(10), control),
        control,
        2 * CHIPS_PER_SLOT,
    )
    .unwrap()
}

#[test]
fn automatic_dispatch_empty_cycle_keeps_original_paired_owners_idle() {
    let mut env = environment();
    let config = config();
    let mut plan = BearerCoordinator::new(
        &config,
        at(0).epoch,
        env._preparation.source().information(),
        None,
    )
    .unwrap();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut traffic = traffic_with_polls(&config, polls.clone());
    plan.assignments = RabHandoff::new(Limits::default());
    let report = service(&mut plan, &mut env, &mut traffic);
    assert_eq!(polls.load(Ordering::Relaxed), 1);
    assert_eq!(plan.network_failures, 0);
    assert_eq!((report.accepted, report.rejected), (0, 0));
    assert!(report.failure.is_none());
    assert_eq!((report.media.visited, report.media.failures), (0, 0));
    assert_eq!(
        (
            report.cleanup.visited,
            report.cleanup.completed,
            report.cleanup.failures
        ),
        (0, 0, 0)
    );
    assert_eq!(plan.outstanding(), 0);
    assert_eq!(plan.pending_cleanups(), 0);
    assert!(!plan.assignments.is_pending());
}

#[test]
fn automatic_dispatch_retains_capability_pending_request_then_prepares_canonical_packet_owner() {
    let mut env = environment();
    let (key, context) = register(&mut env);
    let mut config = config();
    adapt_signalling(&mut config, &mut env, key);
    let mut plan = BearerCoordinator::new(
        &config,
        key.epoch,
        env._preparation.source().information(),
        None,
    )
    .unwrap();
    let mut traffic = traffic(&config);
    plan.assignments = RabHandoff::new(Limits::default());
    retain_packet(&mut env, &mut plan.assignments, context);

    for _ in 0..2 {
        let report = service(&mut plan, &mut env, &mut traffic);
        assert_eq!((report.accepted, report.rejected), (0, 0));
        assert!(report.failure.is_none());
        assert_eq!((report.media.visited, report.media.failures), (0, 0));
        assert!(plan.assignments.is_pending());
        assert_eq!(plan.outstanding(), 0);
        assert_eq!(plan.pending_cleanups(), 0);
    }

    env.with_parts(|_, radio, _, source, _, _| {
        let CapabilityReadiness::Pending(transaction) =
            radio.ensure_capabilities(key, at(0)).unwrap()
        else {
            panic!("automatic dispatch did not retain its actual capability transaction")
        };
        let mut uplink = peer(source);
        uplink.enqueue(&COMPLETE).unwrap();
        uplink
            .publish_next(at(0), |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        send_capability_report_with_transaction(radio, key, &mut uplink, transaction);
        assert_eq!(radio.process_one(), Some(Ok(key)));
    });

    let report = service(&mut plan, &mut env, &mut traffic);
    assert_eq!((report.accepted, report.rejected), (1, 0));
    assert!(report.failure.is_none(), "{:?}", report.failure);
    assert_eq!((report.media.visited, report.media.failures), (1, 0));
    assert!(report.media.first_failure.is_none());
    assert!(!plan.assignments.is_pending());
    assert_eq!(plan.outstanding(), 1);
    assert_eq!(plan.pending_cleanups(), 0);
    env.with_parts(|_, radio, _, _, _, admission| {
        let visited = admission.visit_rab_assignments(count(2), |owner, identity| {
            assert_eq!(identity.ue, key);
            assert_eq!(owner.phase(identity), Some(AdmissionPhase::Preparing));
            let progress = owner.packet_progress(identity).unwrap();
            assert_eq!(
                (
                    progress.items,
                    progress.prepared,
                    progress.bound,
                    progress.recorded
                ),
                (1, 1, 0, 0)
            );
            assert_eq!(
                owner.procedure(identity).unwrap().phase(),
                SetupPhase::Prepared
            );
            assert!(owner.rab_assignment(identity).unwrap().is_committed());
            assert!(radio.dedicated_route(key).is_none());
        });
        assert_eq!(visited, 1);
    });
}
