// SPDX-License-Identifier: AGPL-3.0-or-later

use super::super::super::super::release as release_fixture;
use super::*;
use crate::rel99_admission::{
    AdditionalLaunch, ReleaseLaunchResult, ReleaseRabHost, ScheduledReleasePhase,
    TestReleaseServices,
};
use crate::{
    dedicated_bearers::{DedicatedBearers, ResetPermission},
    dedicated_rx::{DecodedTransport, TransportDelivery},
    packet_bearers::{Error as PacketError, NetworkBinding, PacketBearers},
};
use std::{io, net::SocketAddr};
use umts_rlc::acknowledged::{AmEntity, AmLimits};
use umts_rrc::dedicated::DchDirection;
use umts_rrc_schema::{information_elements as ie, pdu_definitions as pdu};
use umts_user_plane::{
    gtpu::{self, PeerTeid},
    packet::{Datagrams, Limits, PacketPlane, RouteKey, SequenceState, Update},
};

const CROSS_MAPPING_PAYLOAD_BYTES: usize = 435;

#[derive(Default)]
struct Memory {
    sent: Vec<Vec<u8>>,
}
impl Datagrams for Memory {
    fn receive(&mut self, _: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        Ok(None)
    }
    fn send(&mut self, bytes: &[u8], _: SocketAddr) -> io::Result<bool> {
        self.sent.push(bytes.to_vec());
        Ok(true)
    }
}
fn packets() -> PacketBearers<Memory> {
    PacketBearers::new(
        PacketPlane::new(
            Memory::default(),
            Limits {
                routes: count(4),
                packets_per_route: count(4),
                maximum_payload_bytes: count(1500),
                maximum_storage_bytes: 1_000_000,
                receive_budget: count(4),
                route_visits: count(4),
            },
        )
        .unwrap(),
    )
}
fn sender(octets: usize, now: ChipInstant) -> AmEntity {
    AmEntity::new(
        SignallingProfile.network_am_configuration(count(octets), count(octets), 0, 0),
        AmLimits {
            maximum_sdu_bytes: count(512),
            queued_sdus: count(8),
            maximum_buffer_bytes: 1_000_000,
        },
        now,
    )
    .unwrap()
}
fn enqueue(
    bank: &mut DedicatedBearers,
    plan: &CompiledRel99,
    identity: ReceiveIdentity,
    bearer: BearerId,
    frame: u64,
    sender: &mut AmEntity,
) {
    let mapping = plan
        .configuration()
        .bearer(bearer.get())
        .unwrap()
        .mappings(DchDirection::Uplink)
        .next()
        .unwrap();
    assert!(mapping.logical.is_none());
    let transport = TransportId::new(mapping.transport).unwrap();
    let (index, format) = plan
        .uplink_configured()
        .definition(transport)
        .unwrap()
        .formats
        .iter()
        .enumerate()
        .find(|(_, format)| format.blocks == 1 && format.block_bits != 0)
        .unwrap();
    let mut payload = Vec::new();
    for _ in 0..format.blocks {
        sender
            .publish_next(at(frame * CHIPS_PER_FRAME), |pdu| {
                assert_eq!(pdu.bytes.len() * BITS_PER_BYTE, format.block_bits);
                let data = Bits::new(pdu.bytes, format.block_bits).unwrap();
                payload.extend((0..format.block_bits).map(|bit| Bit::from(data.bit(bit).unwrap())));
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
    }
    bank.enqueue_transport(TransportDelivery {
        decoded: DecodedTransport {
            identity,
            transport,
            format: index,
            start: at(frame * CHIPS_PER_FRAME),
            block_bits: format.block_bits,
            blocks: format.blocks,
            crc_checked: format.crc.width() != 0,
        },
        payload: &payload,
    })
    .unwrap();
}
fn service(env: &mut InitialEnvironment, identity: ReceiveIdentity, now: ChipInstant) {
    env.radio
        .with_bearer_access(env.key, |ciphers, _, _| {
            env.receive
                .with_active_bank(identity, |bank| {
                    for _ in 0..64 {
                        bank.service(now, ciphers, ResetPermission::Defer).unwrap();
                    }
                })
                .unwrap();
        })
        .unwrap();
}

#[test]
fn production_packet_step_retires_only_drained_alias_after_actual_radio_commit() {
    with_combined(
        |Combined {
             env,
             voices,
             identity: previous,
             now,
             integrity,
             fresh,
         }| {
            let original = env.admission.active_radio(previous).unwrap().plan().clone();
            let bearer = BearerId::new(
                original
                    .configuration()
                    .bearers()
                    .iter()
                    .find(|rb| rb.domain == Some(CnDomain::Ps))
                    .unwrap()
                    .id,
            )
            .unwrap();
            let circuit_bearers = original
                .configuration()
                .bearers()
                .iter()
                .filter(|rb| rb.domain == Some(CnDomain::Cs))
                .count();
            assert!(circuit_bearers > 0);
            let key = RouteKey {
                connection: umts_types::ConnectionKey {
                    ue: env.key,
                    domain: CnDomain::Ps,
                },
                rab: 9,
            };
            assert!(env.packets.requested(key).is_none());
            let mut packets = packets();
            let route = packets
                .prepare_bound(
                    RouteKey {
                        connection: umts_types::ConnectionKey {
                            ue: env.key,
                            domain: CnDomain::Ps,
                        },
                        rab: 9,
                    },
                    previous,
                    bearer,
                    original.clone(),
                    NetworkBinding {
                        peer: SocketAddr::from(([127, 0, 0, 1], 2152)),
                        peer_teid: PeerTeid::new(99),
                        sequence: SequenceState {
                            next_uplink: 17,
                            next_downlink: Some(23),
                        },
                    },
                )
                .unwrap();
            packets.activate(route, &mut env.receive).unwrap();
            assert_eq!(packets.step(&mut env.receive).unwrap().aliases_retired, 0);
            let other_ue = umts_types::UeKey {
                slot: env.key.slot + 1,
                ..env.key
            };
            let other = packets
                .prepare_bound(
                    RouteKey {
                        connection: umts_types::ConnectionKey {
                            ue: other_ue,
                            domain: CnDomain::Ps,
                        },
                        rab: 9,
                    },
                    ReceiveIdentity {
                        ue: other_ue,
                        configuration: NonZeroU64::MIN,
                    },
                    bearer,
                    original.clone(),
                    NetworkBinding {
                        peer: SocketAddr::from(([127, 0, 0, 1], 2152)),
                        peer_teid: PeerTeid::new(100),
                        sequence: SequenceState::default(),
                    },
                )
                .unwrap();
            let (successor, plan, activation, committed_at) =
                ready_release(env, previous, now, integrity, fresh);
            let mapping = original
                .configuration()
                .bearer(bearer.get())
                .unwrap()
                .mappings(DchDirection::Uplink)
                .next()
                .unwrap();
            let transport = TransportId::new(mapping.transport).unwrap();
            let geometry = original.uplink_configured().definition(transport).unwrap();
            let octets = geometry
                .formats
                .iter()
                .find(|format| format.blocks == 1 && format.block_bits != 0)
                .unwrap()
                .block_bits
                / BITS_PER_BYTE;
            let interval = geometry.interval.frames() as u64;
            let old_frame = activation.time().index / CHIPS_PER_FRAME - interval;
            assert!(old_frame.is_multiple_of(interval));
            let mut sender = sender(octets, at(old_frame * CHIPS_PER_FRAME));
            sender.enqueue(b"old output").unwrap();
            sender
                .enqueue(&[0x63; CROSS_MAPPING_PAYLOAD_BYTES])
                .unwrap();
            env.receive
                .with_active_bank(previous, |bank| {
                    enqueue(bank, &original, previous, bearer, old_frame, &mut sender)
                })
                .unwrap();
            service(env, previous, committed_at);
            assert!(sender.pending_new_data());
            let committed = env
                .admission
                .commit_core_release_packet_plane(
                    previous,
                    crate::dedicated_bearers::ReleaseOutputPolicy::DiscardReleased,
                    TestReleaseServices {
                        gateway: env.core.gateway(),
                        resources: &mut env.resources,
                        radio: &mut env.radio,
                        receive: &mut env.receive,
                        packets: &mut packets,
                        voices,
                    },
                )
                .unwrap();
            assert_eq!(committed.cleanup.retained_voices, 0);
            assert_eq!(committed.released_bearers, circuit_bearers);
            assert_eq!(committed.previous, previous);
            assert_eq!(committed.next, successor);
            assert_eq!(voices.reserved_bytes(), 0);
            assert_eq!(committed.cleanup.packets.retained, 1);
            assert_eq!(packets.step(&mut env.receive).unwrap().aliases_retired, 0);
            assert!(matches!(
                packets.rebind(
                    route,
                    successor,
                    bearer,
                    plan.clone(),
                    Update::default(),
                    &mut env.receive
                ),
                Err(PacketError::Mapping)
            ));
            let mut delivered = false;
            env.receive
                .with_active_bank(successor, |bank| {
                    assert!(bank.process_one(|delivery| {
                        assert_eq!(delivery.metadata.identity, previous);
                        assert_eq!(
                            delivery.metadata.coverage.unwrap().provenance(),
                            ReceiveProvenance::Path(previous.configuration)
                        );
                        let mut bytes = vec![0; delivery.data.len() / BITS_PER_BYTE];
                        delivery.data.copy_to(&mut bytes).unwrap();
                        assert_eq!(bytes, b"old output");
                        assert!(packets.uplink(delivery).unwrap());
                        delivered = true;
                        true
                    }));
                })
                .unwrap();
            assert!(delivered);
            assert_eq!(packets.step(&mut env.receive).unwrap().aliases_retired, 0);
            let mut frame = activation.time().index / CHIPS_PER_FRAME;
            for visit in 0..32 {
                if !sender.pending_new_data() {
                    break;
                }
                env.receive
                    .with_active_bank(successor, |bank| {
                        enqueue(bank, &plan, successor, bearer, frame, &mut sender)
                    })
                    .unwrap();
                let serviced = at(committed_at.index.max((frame + interval) * CHIPS_PER_FRAME));
                service(env, successor, serviced);
                if visit == 0 {
                    assert_eq!(packets.step(&mut env.receive).unwrap().aliases_retired, 0);
                    packets.release(other).unwrap();
                }
                frame += interval;
            }
            assert!(!sender.pending_new_data());
            assert_eq!(packets.step(&mut env.receive).unwrap().aliases_retired, 1);
            assert_eq!(packets.step(&mut env.receive).unwrap().aliases_retired, 0);
            let before = packets.sequence_state(route).unwrap();
            packets
                .rebind(
                    route,
                    successor,
                    bearer,
                    plan.clone(),
                    Update::default(),
                    &mut env.receive,
                )
                .unwrap();
            assert_eq!(packets.sequence_state(route).unwrap(), before);
            let mut complete = false;
            env.receive
                .with_active_bank(successor, |bank| {
                    assert!(bank.process_one(|delivery| {
                        assert_eq!(delivery.metadata.identity, successor);
                        assert_eq!(
                            delivery.metadata.coverage.unwrap().provenance(),
                            ReceiveProvenance::Mixed
                        );
                        let mut bytes = vec![0; delivery.data.len() / BITS_PER_BYTE];
                        delivery.data.copy_to(&mut bytes).unwrap();
                        assert_eq!(bytes, vec![0x63; CROSS_MAPPING_PAYLOAD_BYTES]);
                        assert!(packets.uplink(delivery).unwrap());
                        complete = true;
                        true
                    }));
                })
                .unwrap();
            assert!(complete);
            for _ in 0..4 {
                packets.step(&mut env.receive).unwrap();
            }
            let sent: Vec<_> = packets
                .plane()
                .io()
                .sent
                .iter()
                .map(|bytes| gtpu::decode(bytes).unwrap())
                .collect();
            assert_eq!(sent.len(), 2);
            assert_eq!(sent[0].sequence, Some(17));
            assert_eq!(sent[1].sequence, Some(18));
            assert_eq!(sent[0].payload, b"old output");
            assert_eq!(sent[1].payload, vec![0x63; CROSS_MAPPING_PAYLOAD_BYTES]);
        },
    );
}

fn ready_release(
    env: &mut InitialEnvironment,
    previous: ReceiveIdentity,
    serviced: ChipInstant,
    integrity: &umts_security::contexts::IntegrityContext,
    fresh: u32,
) -> (
    ReceiveIdentity,
    Arc<CompiledRel99>,
    ScheduledActivation,
    ChipInstant,
) {
    env.peer.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context: env.context,
        ranap: Assignment {
            setup: vec![],
            release: vec![umts_iuh::ranap::rab::Release::new(
                FIRST_CIRCUIT,
                umts_iuh::ranap::rab::Cause::from_code(NORMAL_RELEASE).unwrap(),
            )],
        }
        .encode(WireLimits::default())
        .unwrap(),
    });
    let mut handoff = RabHandoff::new(WireLimits::default());
    env.core
        .drive(ControlTime(10), &mut env.radio, |_, event| {
            handoff.offer(event).map(|_| ())
        })
        .unwrap();
    let pending = handoff.take().unwrap().unwrap();
    let now = at(serviced.index.max(env.source.next_chip().index) + 120);
    let configured = policy(env);
    let old = env.admission.active_radio(previous).unwrap().plan().clone();
    let mut activation = None;
    let mut target = None;
    let launched = ReleaseRabHost {
        gateway: env.core.gateway(),
        policy: &configured,
        radio: &mut env.radio,
        resources: &mut env.resources,
        receive: &mut env.receive,
        admission: &mut env.admission,
    }
    .begin_with_delivery(
        previous,
        pending,
        AdditionalLaunch {
            now,
            threshold: at(now.index + CHIPS_PER_FRAME),
            maximum_combinations: count(1024),
        },
        |prepared, delivery| {
            activation = Some(delivery.activation);
            target = Some(prepared.plan().clone());
            Ok(release_fixture::release_settings(
                now.epoch,
                now,
                delivery.activation,
                delivery.publication_deadline,
            ))
        },
    )
    .unwrap();
    let ReleaseLaunchResult::Scheduled(successor) = launched else {
        panic!("active CS requires radio release")
    };
    let activation = activation.unwrap();
    let target = target.unwrap();
    let transaction = env
        .admission
        .release_procedure(previous)
        .unwrap()
        .transaction();
    let mut air = release_fixture::AirPeer {
        previous,
        successor,
        activation: activation.time(),
        old,
        target: target.clone(),
        pending: Vec::new(),
        statuses: 0,
    };
    for _ in 0..COMMAND_VISITS {
        release_fixture::release_step(env, previous, now, &mut air);
        if env.admission.release_phase(previous) == Some(ScheduledReleasePhase::AwaitingComplete) {
            break;
        }
    }
    let mut now = now;
    for _ in 0..(activation.time().index - now.index) / CHIPS_PER_SLOT {
        if env.admission.release_phase(previous) != Some(ScheduledReleasePhase::Switching) {
            break;
        }
        now = release_fixture::generate(env, previous, successor, &mut air);
    }
    assert_eq!(
        env.admission.release_phase(previous),
        Some(ScheduledReleasePhase::AwaitingComplete)
    );
    let slot_limit =
        (activation.time().index - env.source.next_chip().index) / CHIPS_PER_SLOT + 180;
    for _ in 0..slot_limit {
        now = release_fixture::generate(env, previous, successor, &mut air);
        if now.index > activation.time().index {
            break;
        }
    }
    assert!(now.index > activation.time().index);
    let first = umts_types::time::SampleInstant {
        epoch: env.key.epoch,
        index: activation.time().index * 2 + 100,
    };
    env.rx
        .feed(crate::radio_io::MatchedSamples {
            window: umts_dsp::receive_filter::FilteredWindow {
                first_center: Some(first),
                sample_stride: 1,
                next_input: umts_types::time::SampleInstant {
                    index: first.index + 256,
                    ..first
                },
            },
            generation: 3,
            discontinuity: true,
            samples: &[num_complex::Complex32::default(); 256],
        })
        .unwrap();
    let response =
        ULDCCHMessageType::radioBearerReleaseComplete(pdu::RadioBearerReleaseComplete::new(
            ie::RRCTransactionIdentifier(transaction.get()),
            None,
            None,
            None,
            None,
            None,
            None,
        ));
    let received = now.index + 100;
    control::uplink_on_air(
        &mut env.radio,
        env.key,
        &mut env.sender,
        &crate::test_support::integrity::protected(
            integrity,
            fresh,
            SignallingBearer::ControlAm.identity(),
            3,
            response,
        ),
        received,
        ReceiveProvenance::Path(successor.configuration),
        Some(at(received)),
    );
    now = at(received + 20);
    assert_eq!(
        env.radio.process_one_at_with_control(
            now,
            control::security_time(),
            |received, mut security| {
                env.admission
                    .response(&received, &mut security, &mut env.receive)
                    .unwrap()
            }
        ),
        Some(Ok(env.key))
    );
    for _ in 0..COMMAND_VISITS {
        release_fixture::release_step(env, previous, now, &mut air);
        if env.admission.release_phase(previous) == Some(ScheduledReleasePhase::ReadyToCommit) {
            break;
        }
    }
    assert_eq!(
        env.admission.release_phase(previous),
        Some(ScheduledReleasePhase::ReadyToCommit)
    );
    (successor, target, activation, now)
}
