// SPDX-License-Identifier: AGPL-3.0-or-later

use super::super::mapping::retirement::release::{accepted, retire_workers, subset};
use super::*;
use crate::{
    dedicated_bearers::{MappingReleaseError, ReleaseOutputPolicy, ResetPermission},
    dedicated_rx::{DecodedTransport, TransportDelivery},
    voice_bearers::{IngressSettings, InitializationStatus, VoiceTransport},
};
use umts_rrc::dedicated::{
    Combination as RadioCombination, DchDirection, DchTfcs, Rel99Configuration,
};
use umts_user_plane::{iu_cs::RtpEndpoint, uplink_media::Progress};

fn next(f: &Fixture) -> ReceiveIdentity {
    ReceiveIdentity {
        configuration: NonZeroU64::new(2).unwrap(),
        ..f.bank.identity()
    }
}

fn ready(f: &Fixture) -> (VoiceTraffic, VoiceHandle) {
    let (mut source, handle) = prepare(f);
    source.activate(handle, &f.bank).unwrap();
    let now = Instant::now();
    let mut peer = sender(0, &[81, 103, 60]);
    request(&mut source, handle, &mut peer, now);
    let mut ack = Vec::new();
    source
        .publish_initialization_rtp_at(handle, now, 9000, &mut [0; 256], &mut [0; 256], |wire| {
            ack.extend_from_slice(wire);
            Ok::<_, ()>(true)
        })
        .unwrap();
    assert_eq!(
        peer.acknowledgment(
            iuup::decode(rtp::decode(&ack).unwrap().payload).unwrap(),
            now
        )
        .unwrap(),
        AckEffect::Complete
    );
    (source, handle)
}

fn attach(
    source: &mut VoiceTraffic,
    handle: VoiceHandle,
) -> (VoiceTransport<RtpEndpoint>, RtpEndpoint) {
    let peer = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let local = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let transport = source
        .attach_transport(
            handle,
            local,
            peer.local_address().unwrap(),
            IngressSettings {
                datagram_bytes: count(256),
                payload_type: 96,
                ssrc: Some(7),
            },
        )
        .unwrap();
    (transport, peer)
}

#[test]
fn successor_voice_outputs_survive_until_the_media_handoff_commits() {
    use crate::dedicated_bearers::DeliveryMapping;
    let mut f = fixture(true);
    let old = f.bank.identity();
    let successor = next(&f);
    let (mut source, handle) = ready(&f);
    let charged = source.reserved_bytes();
    let plan = subset(&f, false, true);
    let token = accepted(&mut f, successor, &plan);
    let (rx, tx, _writer) = retire_workers(&mut f, successor, &plan);
    for (bearer, width) in [(10, 81), (11, 103), (12, 60)] {
        let transport = TransportId::new(
            plan.configuration().bearer(bearer).unwrap().mapping.uplink[0].transport,
        )
        .unwrap();
        let definition = plan.uplink_configured().definition(transport).unwrap();
        let format = definition
            .formats
            .iter()
            .position(|format| format.blocks == 1 && format.block_bits == width)
            .unwrap();
        let payload = vec![Bit::One; width];
        f.bank
            .enqueue_transport(TransportDelivery {
                decoded: DecodedTransport {
                    identity: successor,
                    transport,
                    format,
                    start: at(4 * CHIPS_PER_FRAME),
                    block_bits: width,
                    blocks: 1,
                    crc_checked: definition.formats[format].crc.width() != 0,
                },
                payload: &payload,
            })
            .unwrap();
        f.radio
            .with_bearer_access(f.key, |ciphers, _, _| {
                for _ in 0..8 {
                    f.bank
                        .service(at(10 * CHIPS_PER_FRAME), ciphers, ResetPermission::Defer)
                        .unwrap();
                }
            })
            .unwrap();
        assert!(f.bank.process_one(|delivery| {
            assert_eq!(delivery.metadata.identity, successor);
            assert_eq!(delivery.mapping, DeliveryMapping::Pending { previous: old });
            source.uplink(delivery).unwrap()
        }));
    }
    let mut before_commit = 0;
    source
        .publish_uplink(handle, at(10 * CHIPS_PER_FRAME), |_| {
            before_commit += 1;
            Ok::<_, ()>(true)
        })
        .unwrap();
    assert_eq!(before_commit, 0);
    f.radio
        .commit_bearer_release_with(&token, |ciphers| {
            f.bank.retire_release_mapping_with_view(
                &token,
                &rx,
                &tx,
                ReleaseOutputPolicy::Deliver,
                ciphers,
                |_, bank| {
                    source
                        .prepare_release_handoff::<RtpEndpoint>(
                            &token,
                            plan.clone(),
                            bank,
                            std::iter::empty(),
                        )
                        .map(|guard| guard.commit())
                },
            )
        })
        .unwrap();
    let mut published = Vec::new();
    assert_eq!(
        source
            .publish_uplink(handle, at(10 * CHIPS_PER_FRAME), |wire| {
                published.extend_from_slice(wire);
                Ok::<_, ()>(true)
            })
            .unwrap(),
        Progress::Sent
    );
    let packet = rtp::decode(&published).unwrap();
    assert_eq!((packet.sequence, packet.ssrc), (21, 77));
    let Frame::Data {
        quality,
        payload_crc,
        payload,
        ..
    } = iuup::decode(packet.payload).unwrap()
    else {
        panic!("voice data");
    };
    assert_eq!(
        (quality, payload_crc),
        (FrameQuality::Good, PayloadCheck::Valid)
    );
    let bits = Bits::new(payload, 244).unwrap();
    assert!((0..bits.len()).all(|index| bits.bit(index) == Some(true)));
    assert_eq!(source.reserved_bytes(), charged);
    source.release(handle).unwrap();
    assert_eq!(source.reserved_bytes(), 0);
}

#[test]
fn retained_handoff_preserves_original_media_endpoint_clock_and_shared_rtp_sequence() {
    let mut f = fixture(true);
    let old = f.bank.identity();
    let successor = next(&f);
    let (mut source, handle) = ready(&f);
    let (mut transport, _peer) = attach(&mut source, handle);
    let local = transport.local_address();
    let remote = transport.peer();
    source
        .downlink(
            handle,
            &packet(0),
            RtpAnchor::new(0, at(4 * CHIPS_PER_FRAME), NonZeroU32::new(3200).unwrap()).unwrap(),
        )
        .unwrap();
    for (bearer, width) in [(10, 81), (11, 103), (12, 60)] {
        receive_voice_part(&mut f, &mut source, bearer, width, 0);
    }
    let mut retained = Vec::new();
    assert_eq!(
        source
            .publish_uplink(handle, at(10 * CHIPS_PER_FRAME), |wire| {
                retained.extend_from_slice(wire);
                Ok::<_, ()>(false)
            })
            .unwrap(),
        Progress::Backpressure
    );
    let charged = source.reserved_bytes();
    let plan = subset(&f, false, true);
    let token = accepted(&mut f, successor, &plan);
    let (rx, tx, _writer) = retire_workers(&mut f, successor, &plan);
    f.radio
        .with_bearer_access(f.key, |ciphers, _, _| {
            assert!(matches!(
                f.bank.retire_release_mapping_with_view(
                    &token,
                    &rx,
                    &tx,
                    ReleaseOutputPolicy::Deliver,
                    ciphers,
                    |_, bank| {
                        let guard = source
                            .prepare_release_handoff::<RtpEndpoint>(
                                &token,
                                plan.clone(),
                                bank,
                                std::iter::empty(),
                            )
                            .unwrap();
                        drop(guard);
                        Err::<usize, _>("busy")
                    }
                ),
                Err(MappingReleaseError::Cleanup("busy"))
            ));
        })
        .unwrap();
    assert_eq!(source.queued(handle), Some(1));
    assert_eq!(source.reserved_bytes(), charged);
    assert_eq!(f.bank.identity(), old);
    assert!(
        source
            .offer(old, at(4 * CHIPS_PER_FRAME), BearerId::new(10).unwrap())
            .is_some()
    );
    f.radio
        .commit_bearer_release_with(&token, |ciphers| {
            f.bank.retire_release_mapping_with_view(
                &token,
                &rx,
                &tx,
                ReleaseOutputPolicy::Deliver,
                ciphers,
                |_, bank| {
                    source
                        .prepare_release_handoff::<RtpEndpoint>(
                            &token,
                            plan.clone(),
                            bank,
                            std::iter::empty(),
                        )
                        .map(|guard| guard.commit())
                },
            )
        })
        .unwrap();
    assert_eq!(source.reserved_bytes(), charged);
    assert_eq!(source.queued(handle), Some(1));
    assert!(matches!(
        source.prepare_release_handoff::<RtpEndpoint>(
            &token,
            plan.clone(),
            &f.bank,
            std::iter::empty()
        ),
        Err(Error::Mapping)
    ));
    assert!(
        source
            .offer(old, at(4 * CHIPS_PER_FRAME), BearerId::new(10).unwrap())
            .is_none()
    );
    let offer = source
        .offer(
            successor,
            at(4 * CHIPS_PER_FRAME),
            BearerId::new(10).unwrap(),
        )
        .unwrap();
    assert_eq!((offer.payload_bits_per_block, offer.blocks), (81, 1));
    assert_eq!(
        source
            .publish_uplink(handle, at(10 * CHIPS_PER_FRAME), |wire| {
                assert_eq!(wire, retained);
                let packet = rtp::decode(wire).unwrap();
                assert_eq!(
                    (packet.sequence, packet.timestamp, packet.ssrc),
                    (21, 9000, 77)
                );
                Ok::<_, ()>(true)
            })
            .unwrap(),
        Progress::Sent
    );
    assert_eq!(
        (
            transport.local_address(),
            transport.peer(),
            transport.source()
        ),
        (local, remote, Some(7))
    );
    assert!(transport.retire(&mut source).unwrap());
    assert_eq!(source.reserved_bytes(), 0);
}

#[test]
fn removed_attached_route_requires_exact_transport_and_drop_preserves_it() {
    let mut f = fixture(true);
    let successor = next(&f);
    let (mut source, handle) = ready(&f);
    let (mut transport, _peer) = attach(&mut source, handle);
    let local = transport.local_address();
    let charged = source.reserved_bytes();
    let plan = subset(&f, true, false);
    let token = accepted(&mut f, successor, &plan);
    let (rx, tx, _writer) = retire_workers(&mut f, successor, &plan);
    assert!(matches!(
        source.prepare_release_handoff::<RtpEndpoint>(
            &token,
            plan.clone(),
            &f.bank,
            std::iter::empty()
        ),
        Err(Error::Attached)
    ));
    let (mut other, other_handle) = ready_for_original(&f, &token);
    let (mut foreign, _foreign_peer) = attach(&mut other, other_handle);
    assert!(matches!(
        source.prepare_release_handoff(
            &token,
            plan.clone(),
            &f.bank,
            std::iter::once(&mut foreign)
        ),
        Err(Error::Mapping)
    ));
    drop(
        source
            .prepare_release_handoff(
                &token,
                plan.clone(),
                &f.bank,
                std::iter::once(&mut transport),
            )
            .unwrap(),
    );
    assert_eq!(source.reserved_bytes(), charged);
    assert_eq!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Ready
    );
    assert!(RtpEndpoint::bind(local).is_err());
    f.radio
        .commit_bearer_release_with(&token, |ciphers| {
            f.bank.retire_release_mapping_with_view(
                &token,
                &rx,
                &tx,
                ReleaseOutputPolicy::Deliver,
                ciphers,
                |_, bank| {
                    source
                        .prepare_release_handoff(
                            &token,
                            plan.clone(),
                            bank,
                            std::iter::once(&mut transport),
                        )
                        .map(|guard| guard.commit())
                },
            )
        })
        .unwrap();
    assert_eq!(source.reserved_bytes(), 0);
    assert!(source.initialization_status(handle).is_err());
    assert!(!transport.retire(&mut source).unwrap());
    let _rebound = RtpEndpoint::bind(local).unwrap();
    assert!(foreign.retire(&mut other).unwrap());
}

fn ready_for_original(
    f: &Fixture,
    token: &crate::signalling::AcceptedBearerRelease,
) -> (VoiceTraffic, VoiceHandle) {
    let original = fixture(true);
    assert_eq!(original.bank.identity(), token.previous_identity());
    let result = ready(&original);
    assert_eq!(f.key, original.key);
    result
}

#[test]
fn pending_receiver_media_and_ack_sequence_survive_a_retained_handoff() {
    let mut f = fixture(true);
    let successor = next(&f);
    let (mut source, handle) = ready(&f);
    let now = Instant::now();
    let mut peer = sender(1, &[81, 103, 60]);
    request(&mut source, handle, &mut peer, now);
    assert_eq!(source.queued(handle), None);
    let plan = subset(&f, false, true);
    let token = accepted(&mut f, successor, &plan);
    let (rx, tx, _writer) = retire_workers(&mut f, successor, &plan);
    let wrong = subset(&f, false, true);
    assert!(matches!(
        source.prepare_release_handoff::<RtpEndpoint>(&token, wrong, &f.bank, std::iter::empty()),
        Err(Error::Mapping)
    ));
    f.radio
        .commit_bearer_release_with(&token, |ciphers| {
            f.bank.retire_release_mapping_with_view(
                &token,
                &rx,
                &tx,
                ReleaseOutputPolicy::Deliver,
                ciphers,
                |_, bank| {
                    source
                        .prepare_release_handoff::<RtpEndpoint>(
                            &token,
                            plan.clone(),
                            bank,
                            std::iter::empty(),
                        )
                        .map(|guard| guard.commit())
                },
            )
        })
        .unwrap();
    assert_eq!(source.queued(handle), None);
    let mut ack = Vec::new();
    source
        .publish_initialization_rtp_at(handle, now, 9000, &mut [0; 256], &mut [0; 256], |wire| {
            assert_eq!(rtp::decode(wire).unwrap().sequence, 21);
            ack.extend_from_slice(wire);
            Ok::<_, ()>(true)
        })
        .unwrap();
    assert_eq!(
        peer.acknowledgment(
            iuup::decode(rtp::decode(&ack).unwrap().payload).unwrap(),
            now
        )
        .unwrap(),
        AckEffect::Complete
    );
    source
        .downlink(
            handle,
            &packet(1),
            RtpAnchor::new(0, at(4 * CHIPS_PER_FRAME), NonZeroU32::new(3200).unwrap()).unwrap(),
        )
        .unwrap();
    assert!(
        source
            .offer(
                successor,
                at(4 * CHIPS_PER_FRAME),
                BearerId::new(11).unwrap()
            )
            .is_some()
    );
}

#[test]
fn old_retained_cs_output_must_be_consumed_before_layout_interpretation_changes() {
    let mut f = fixture(true);
    let successor = next(&f);
    let (mut source, handle) = ready(&f);
    let transport =
        TransportId::new(f.plan.configuration().bearer(10).unwrap().mapping.uplink[0].transport)
            .unwrap();
    let definition = f.plan.uplink_configured().definition(transport).unwrap();
    let format = definition
        .formats
        .iter()
        .position(|format| format.blocks == 1 && format.block_bits == 81)
        .unwrap();
    let crc_checked = definition.formats[format].crc.width() != 0;
    f.bank
        .enqueue_transport(TransportDelivery {
            decoded: DecodedTransport {
                identity: f.bank.identity(),
                transport,
                format,
                start: at(0),
                block_bits: 81,
                blocks: 1,
                crc_checked,
            },
            payload: &[Bit::Zero; 81],
        })
        .unwrap();
    f.radio
        .with_bearer_access(f.key, |ciphers, _, _| {
            for _ in 0..8 {
                f.bank
                    .service(at(10 * CHIPS_PER_FRAME), ciphers, ResetPermission::Defer)
                    .unwrap();
            }
        })
        .unwrap();
    let plan = subset(&f, false, true);
    let token = accepted(&mut f, successor, &plan);
    let (_rx, _tx, _writer) = retire_workers(&mut f, successor, &plan);
    assert!(matches!(
        source.prepare_release_handoff::<RtpEndpoint>(
            &token,
            plan.clone(),
            &f.bank,
            std::iter::empty()
        ),
        Err(Error::Radio)
    ));
    assert!(
        f.bank
            .process_one(|delivery| source.uplink(delivery).unwrap())
    );
    drop(
        source
            .prepare_release_handoff::<RtpEndpoint>(&token, plan, &f.bank, std::iter::empty())
            .unwrap(),
    );
    assert_eq!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Ready
    );
}

fn unsupported_successor(f: &Fixture, partial: bool) -> Arc<CompiledRel99> {
    let old = f.plan.configuration();
    let removed = if partial { 12 } else { 20 };
    let bearers: Vec<_> = old
        .bearers()
        .iter()
        .filter(|rb| rb.id != removed)
        .cloned()
        .collect();
    let direction = |old: &DchTfcs, direction: DchDirection| {
        let indices: Vec<_> = old
            .transports()
            .iter()
            .enumerate()
            .filter(|(_, transport)| {
                bearers.iter().any(|rb| {
                    rb.mappings(direction)
                        .any(|mapping| mapping.transport == transport.id)
                })
            })
            .map(|(index, _)| index)
            .collect();
        let mut transports: Vec<_> = indices
            .iter()
            .map(|index| old.transports()[*index].clone())
            .collect();
        if !partial {
            let transport = bearers
                .iter()
                .find(|rb| rb.id == 10)
                .unwrap()
                .mappings(direction)
                .next()
                .unwrap()
                .transport;
            let definition = transports
                .iter_mut()
                .find(|definition| definition.id == transport)
                .unwrap();
            for group in &mut definition.groups {
                if group.rlc_bits == 81 {
                    group.rlc_bits = 82;
                }
            }
        }
        let mut combinations: Vec<RadioCombination> = Vec::new();
        for old in old.combinations() {
            let formats: Vec<_> = indices.iter().map(|index| old.formats[*index]).collect();
            if !combinations.iter().any(|old| old.formats == formats) {
                combinations.push(RadioCombination {
                    formats,
                    gain: old.gain,
                });
            }
        }
        DchTfcs::new(direction, transports, combinations).unwrap()
    };
    let configuration = Arc::new(
        Rel99Configuration::new(
            direction(old.uplink(), DchDirection::Uplink),
            direction(old.downlink(), DchDirection::Downlink),
            bearers,
            old.physical().clone(),
        )
        .unwrap(),
    );
    Arc::new(
        CompiledRel99::new(
            configuration,
            count(200_000),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(300_000),
                decode: DecodeBudget::new(std::num::NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    )
}

#[test]
fn handoff_rejects_an_unreconciled_same_ue_generation_without_mutation() {
    let mut f = fixture(true);
    let successor = next(&f);
    let (mut source, original) = ready(&f);
    let mut stale_key = key(f.bank.identity());
    stale_key.rab = 2;
    let stale = source
        .prepare(
            stale_key,
            successor,
            f.plan.clone(),
            &[10, 11, 12].map(|id| BearerId::new(id).unwrap()),
            Versions::new(3).unwrap(),
            uplink_policy(),
        )
        .unwrap();
    let charged = source.reserved_bytes();
    let plan = subset(&f, false, true);
    let token = accepted(&mut f, successor, &plan);
    let (_rx, _tx, _writer) = retire_workers(&mut f, successor, &plan);
    assert!(matches!(
        source.prepare_release_handoff::<RtpEndpoint>(&token, plan, &f.bank, std::iter::empty()),
        Err(Error::Mapping)
    ));
    assert_eq!(source.reserved_bytes(), charged);
    assert_eq!(
        source.initialization_status(original).unwrap(),
        InitializationStatus::Ready
    );
    assert_eq!(
        source.initialization_status(stale).unwrap(),
        InitializationStatus::Pending
    );
    assert_eq!(source.queued(stale), None);
}

#[test]
fn partially_retained_route_and_incompatible_joint_voice_formats_fail_without_mutation() {
    for partial in [true, false] {
        let mut f = fixture(true);
        let successor = next(&f);
        let (mut source, handle) = ready(&f);
        let charged = source.reserved_bytes();
        let plan = unsupported_successor(&f, partial);
        let token = accepted(&mut f, successor, &plan);
        let anchor = FrameAnchor {
            time: at(4 * CHIPS_PER_FRAME),
            cfn: 4,
        };
        let timing = MappingActivation {
            uplink: anchor,
            uplink_frame: 4,
            downlink: anchor,
            downlink_frame: 4,
        };
        let staged = f
            .bank
            .prepare_release_mapping(successor, timing, plan.clone())
            .unwrap();
        f.bank.stage_mapping(staged).unwrap();
        f.bank.commit_mapping(successor).unwrap();
        assert!(matches!(
            source.prepare_release_handoff::<RtpEndpoint>(
                &token,
                plan,
                &f.bank,
                std::iter::empty()
            ),
            Err(Error::Mapping)
        ));
        assert_eq!(source.reserved_bytes(), charged);
        assert_eq!(
            source.initialization_status(handle).unwrap(),
            InitializationStatus::Ready
        );
        assert_eq!(source.queued(handle), Some(0));
    }
}
