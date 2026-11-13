// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::NonZeroU16;
use umts_dsp::{
    coding::{ChannelCoding, CodedBlockEncoder, CodedBlockFormat, DecodeBudget},
    convolutional::CodeRate,
    crc::TransportCrc,
    dedicated::UplinkLayout,
    dedicated_transport::{
        CombinationDefinition, ConfiguredTfcs, PuncturingLimit, TransportDefinition,
        TransportPlanLimits,
    },
    interleaving::{Interleaver, Rel99Tti},
};
use umts_types::RadioEpoch;

mod chips;
mod incremental;
mod samples;

fn count(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}
fn epoch() -> RadioEpoch {
    RadioEpoch::new(NonZeroU64::MIN)
}
fn at(frame: u64) -> ChipInstant {
    ChipInstant {
        epoch: epoch(),
        index: 1024 + frame * CHIPS_PER_FRAME,
    }
}
fn id(value: u8) -> TransportId {
    TransportId::new(value).unwrap()
}
fn identity(slot: u32, configuration: u64) -> ReceiveIdentity {
    ReceiveIdentity {
        ue: UeKey {
            epoch: epoch(),
            slot,
            generation: NonZeroU64::MIN,
        },
        configuration: NonZeroU64::new(configuration).unwrap(),
    }
}
fn tfci(voice: usize, packet: usize) -> Tfci {
    Tfci::new((voice * 3 + packet) as u16).unwrap()
}
fn configuration() -> Arc<UplinkTfcs> {
    configuration_with_interval(Rel99Tti::Ms20)
}
fn configuration_with_interval(interval: Rel99Tti) -> Arc<UplinkTfcs> {
    let voice = CodedBlockFormat {
        block_bits: 40,
        blocks: 1,
        crc: TransportCrc::Crc16,
        coding: ChannelCoding::Convolutional(CodeRate::Half),
    };
    let packet = CodedBlockFormat {
        block_bits: 400,
        blocks: 1,
        crc: TransportCrc::Crc24,
        coding: ChannelCoding::Turbo,
    };
    let definitions = vec![
        TransportDefinition {
            id: id(1),
            interval,
            attribute: NonZeroU16::new(8).unwrap(),
            formats: vec![
                CodedBlockFormat { blocks: 0, ..voice },
                voice,
                CodedBlockFormat {
                    block_bits: 80,
                    ..voice
                },
            ],
        },
        TransportDefinition {
            id: id(5),
            interval: Rel99Tti::Ms10,
            attribute: NonZeroU16::new(8).unwrap(),
            formats: vec![
                CodedBlockFormat {
                    blocks: 0,
                    ..packet
                },
                packet,
                CodedBlockFormat {
                    block_bits: 5000,
                    ..packet
                },
            ],
        },
    ];
    let combinations = (0..3)
        .flat_map(|voice| {
            (0..3).map(move |packet| CombinationDefinition {
                tfci: tfci(voice, packet),
                formats: vec![voice, packet],
            })
        })
        .collect();
    let configured =
        Arc::new(ConfiguredTfcs::new(definitions, combinations, count(200000)).unwrap());
    let mut layouts: Vec<_> = [256, 128, 64, 32, 16, 8, 4]
        .into_iter()
        .map(|sf| UplinkLayout::single(sf).unwrap())
        .collect();
    layouts.extend((2..=6).map(|channels| UplinkLayout::multicode(channels).unwrap()));
    Arc::new(
        UplinkTfcs::new(
            configured,
            &layouts,
            PuncturingLimit::new(100).unwrap(),
            TransportPlanLimits {
                maximum_coded_bits: count(200000),
                maximum_mapping_entries: count(5000000),
                decode: DecodeBudget::new(std::num::NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    )
}
fn limits(slots: usize) -> ReceiveLimits {
    ReceiveLimits {
        output_slots: count(slots),
        maximum_output_bits: count(5000),
        maximum_output_bytes: 100000,
        maximum_coded_bits: count(200000),
        maximum_frame_bits: count(57600),
        maximum_decodes_per_step: count(1),
    }
}
fn session(slot: u32, slots: usize, config: Arc<UplinkTfcs>) -> DedicatedReceiveSession {
    DedicatedReceiveSession::new(
        identity(slot, 1),
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        config,
        limits(slots),
    )
    .unwrap()
}
fn payload(config: &UplinkTfcs, id: TransportId, format: usize, seed: usize) -> Vec<Bit> {
    let plan = config.configured().block_plan(id, format).unwrap();
    (0..plan.payload_bits())
        .map(|index| Bit::from((index * 17 + seed + usize::from(id.get())) % 7 < 3))
        .collect()
}
fn physical(
    config: &UplinkTfcs,
    voice: usize,
    packet: usize,
    cfn: u8,
    seed: usize,
) -> Vec<Vec<f64>> {
    let allocation = config.allocation(tfci(voice, packet)).unwrap();
    let mut combined = Vec::new();
    for (id, format) in [(id(1), voice), (id(5), packet)] {
        let definition = config.configured().definition(id).unwrap();
        let plan = config.configured().block_plan(id, format).unwrap();
        let frames = definition.interval.frames();
        let width = plan.coded_bits().div_ceil(frames);
        let mut coded = vec![Bit::Zero; width * frames];
        CodedBlockEncoder::new(plan)
            .unwrap()
            .encode(
                &payload(config, id, format, seed),
                &mut coded[..plan.coded_bits()],
            )
            .unwrap();
        let mut first = coded.clone();
        Interleaver::first(definition.interval, coded.len())
            .unwrap()
            .interleave(&coded, &mut first)
            .unwrap();
        let matched = allocation
            .channels()
            .iter()
            .find(|matched| matched.id == id)
            .unwrap();
        let phase = usize::from(cfn) % frames;
        let mut output = vec![Bit::Zero; matched.output_bits];
        plan.matching_uplink(definition.interval, output.len(), phase, count(5000000))
            .unwrap()
            .apply(&first[phase * width..(phase + 1) * width], &mut output)
            .unwrap();
        combined.extend(
            output
                .iter()
                .map(|bit| if *bit == Bit::Zero { 1.0 } else { -1.0 }),
        );
    }
    if allocation.layout().is_empty() {
        return Vec::new();
    }
    let width = combined.len() / allocation.layout().len();
    combined
        .chunks_exact(width)
        .map(|input| {
            let mut output = vec![0.0; width];
            Interleaver::second(width)
                .unwrap()
                .interleave(input, &mut output)
                .unwrap();
            output
        })
        .collect()
}
fn receive(
    session: &mut DedicatedReceiveSession,
    frame: u64,
    cfn: u8,
    voice: usize,
    packet: usize,
    seed: usize,
) -> Result<(), ReceiveError> {
    let input = physical(&session.tfcs, voice, packet, cfn, seed);
    session.receive(
        session.identity(),
        at(frame),
        cfn,
        tfci(voice, packet),
        &input.iter().map(Vec::as_slice).collect::<Vec<_>>(),
    )
}
fn consume(session: &mut DedicatedReceiveSession) -> Vec<(DecodedTransport, Vec<Bit>)> {
    let mut result = Vec::new();
    while session.process_one(|delivery| {
        result.push((delivery.decoded, delivery.payload.to_vec()));
        true
    }) {}
    result
}

#[test]
fn mixed_tti_session_reuses_format_workspaces_and_retains_ordered_results() {
    let config = configuration();
    let mut session = session(0, 4, Arc::clone(&config));
    receive(&mut session, 0, 0, 1, 1, 7).unwrap();
    assert_eq!(session.service(), Ok(1));
    receive(&mut session, 1, 1, 1, 2, 7).unwrap();
    assert_eq!(session.service(), Ok(1));
    assert_eq!(session.service(), Ok(1));
    let received = consume(&mut session);
    assert_eq!(received.len(), 3);
    assert_eq!(
        (received[0].0.transport, received[0].0.start),
        (id(5), at(0))
    );
    assert_eq!(
        (received[1].0.transport, received[1].0.start),
        (id(1), at(0))
    );
    assert_eq!(received[0].1, payload(&config, id(5), 1, 7));
    assert_eq!(received[1].1, payload(&config, id(1), 1, 7));
    assert_eq!(received[2].1, payload(&config, id(5), 2, 7));
    for (frame, voice) in [(2, 2), (3, 2), (4, 0), (5, 0), (6, 1), (7, 1)] {
        receive(&mut session, frame, frame as u8, voice, 0, 3).unwrap();
        session.service().unwrap();
        session.service().unwrap();
    }
    let received = consume(&mut session);
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].1, payload(&config, id(1), 2, 3));
    assert_eq!(received[1].1, payload(&config, id(1), 1, 3));
    assert!(received.iter().all(|result| result.0.crc_checked));
}

#[test]
fn full_output_retains_completed_tti_and_drops_only_new_ttis_with_counters() {
    let config = configuration();
    let mut session = session(0, 1, Arc::clone(&config));
    receive(&mut session, 0, 0, 0, 1, 0).unwrap();
    session.service().unwrap();
    session.service().unwrap();
    assert_eq!(session.queued(), 1);
    assert!(!session.process_one(|_| false));
    receive(&mut session, 1, 1, 0, 1, 1).unwrap();
    session.service().unwrap();
    receive(&mut session, 2, 2, 0, 1, 2).unwrap();
    assert_eq!(session.counters().skipped_ttis, 1);
    let first = consume(&mut session);
    assert_eq!(first[0].1, payload(&config, id(5), 1, 0));
    session.service().unwrap();
    session.service().unwrap();
    let second = consume(&mut session);
    assert_eq!(second[0].0.start, at(1));
    assert_eq!(second[0].1, payload(&config, id(5), 1, 1));
    receive(&mut session, 3, 3, 0, 1, 3).unwrap();
    session.service().unwrap();
    session.service().unwrap();
    let third = consume(&mut session);
    assert_eq!(third[0].0.start, at(3));
}

#[test]
fn rejected_identity_time_and_mid_tti_format_changes_leave_frame_retryable() {
    let config = configuration();
    let mut session = session(0, 4, config);
    receive(&mut session, 0, 0, 1, 1, 0).unwrap();
    assert_eq!(
        receive(&mut session, 1, 1, 2, 1, 0),
        Err(ReceiveError::FormatChangedWithinTti)
    );
    assert_eq!(
        receive(&mut session, 1, 9, 1, 1, 0),
        Err(ReceiveError::Time)
    );
    assert_eq!(
        session.receive(identity(0, 2), at(1), 1, tfci(1, 1), &[]),
        Err(ReceiveError::Identity)
    );
    assert!(
        session
            .receive(identity(0, 1), at(1), 1, tfci(1, 1), &[&[f64::NAN]])
            .is_err()
    );
    assert_eq!(session.counters().frames, 1);
    receive(&mut session, 1, 1, 1, 1, 0).unwrap();
    assert_eq!(session.counters().frames, 2);
    assert_eq!(
        receive(&mut session, 1, 1, 1, 1, 0),
        Err(ReceiveError::Time)
    );
}

#[test]
fn gaps_discard_incomplete_ttis_and_resume_only_at_each_transports_boundary() {
    let config = configuration();
    let mut session = session(0, 8, Arc::clone(&config));
    receive(&mut session, 0, 0, 1, 1, 1).unwrap();
    session.service().unwrap();
    receive(&mut session, 3, 3, 2, 1, 4).unwrap();
    session.service().unwrap();
    receive(&mut session, 4, 4, 2, 1, 7).unwrap();
    session.service().unwrap();
    receive(&mut session, 5, 5, 2, 1, 7).unwrap();
    session.service().unwrap();
    session.service().unwrap();
    let received = consume(&mut session);
    let voice: Vec<_> = received
        .iter()
        .filter(|received| received.0.transport == id(1))
        .collect();
    assert_eq!(voice.len(), 1);
    assert_eq!(voice[0].0.start, at(4));
    assert_eq!(voice[0].1, payload(&config, id(1), 2, 7));
    assert_eq!(session.counters().missing_frames, 2);
    assert_eq!(session.counters().interrupted_ttis, 1);
}

#[test]
fn anchor_checks_activation_epoch_tti_alignment_and_cfn_wrap_without_wall_clock() {
    let config = configuration();
    assert!(
        DedicatedReceiveSession::new(
            identity(0, 1),
            FrameAnchor {
                time: at(0),
                cfn: 255
            },
            Arc::clone(&config),
            limits(4)
        )
        .is_err()
    );
    let mut session = DedicatedReceiveSession::new(
        identity(0, 1),
        FrameAnchor {
            time: at(0),
            cfn: 254,
        },
        config,
        limits(4),
    )
    .unwrap();
    receive(&mut session, 0, 254, 1, 0, 7).unwrap();
    receive(&mut session, 1, 255, 1, 0, 7).unwrap();
    session.service().unwrap();
    session.service().unwrap();
    receive(&mut session, 2, 0, 2, 0, 9).unwrap();
    assert_eq!(session.counters().frames, 3);
    session.retire();
    assert_eq!(session.queued(), 0);
    assert_eq!(session.counters().retired_results, 1);
    session.retire();
    assert_eq!(session.counters().retired_results, 1);
    assert_eq!(session.service(), Err(ReceiveError::Retired));
    assert_eq!(
        receive(&mut session, 3, 1, 2, 0, 9),
        Err(ReceiveError::Retired)
    );
}

#[test]
fn shard_admission_is_atomic_and_recycled_identity_cannot_select_old_session() {
    let config = configuration();
    let first = Box::new(session(0, 2, Arc::clone(&config)));
    let resources = first.resources();
    let mut shard = DedicatedRxShard::new(
        epoch(),
        ShardLimits {
            maximum_ues: count(2),
            maximum_sessions_per_step: count(1),
            resources,
        },
    )
    .unwrap();
    assert!(shard.install(first).is_ok());
    let rejected = shard.install(Box::new(session(1, 2, Arc::clone(&config))));
    assert!(
        matches!(rejected, Err((ReceiveError::ResourceBudget { required, maximum, .. }, _))
        if required == 2 * maximum && maximum > 0)
    );
    assert_eq!(shard.used(), resources);
    let old = shard.remove(identity(0, 1)).unwrap();
    assert_eq!(shard.used(), ReceiveResources::default());
    assert!(matches!(
        shard.install(old),
        Err((ReceiveError::Identity, _))
    ));
    let new = DedicatedReceiveSession::new(
        identity(0, 2),
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        config,
        limits(2),
    )
    .unwrap();
    assert!(shard.install(Box::new(new)).is_ok());
    assert!(shard.session_mut(identity(0, 1)).is_none());
    assert!(shard.session_mut(identity(0, 2)).is_some());
}

#[test]
fn bounded_shard_service_and_backpressured_peer_do_not_stop_other_ues() {
    let config = configuration();
    let first = Box::new(session(0, 1, Arc::clone(&config)));
    let second = Box::new(session(1, 1, config));
    let a = first.resources();
    let b = second.resources();
    let resources = ReceiveResources {
        physical_sample_bytes: 0,
        coded_bits: a.coded_bits + b.coded_bits,
        output_bytes: a.output_bytes + b.output_bytes,
        demultiplexed_bits: a.demultiplexed_bits + b.demultiplexed_bits,
        channels: a.channels + b.channels,
    };
    let mut shard = DedicatedRxShard::new(
        epoch(),
        ShardLimits {
            maximum_ues: count(2),
            maximum_sessions_per_step: count(1),
            resources,
        },
    )
    .unwrap();
    assert!(shard.install(first).is_ok());
    assert!(shard.install(second).is_ok());
    for slot in 0..2 {
        receive(
            shard.session_mut(identity(slot, 1)).unwrap(),
            0,
            0,
            0,
            1,
            slot as usize,
        )
        .unwrap();
    }
    let mut serviced = Vec::new();
    for _ in 0..4 {
        assert_eq!(
            shard.service(|identity, result| {
                assert!(result.is_ok());
                serviced.push(identity.ue.slot);
            }),
            1
        );
    }
    assert_eq!(serviced, [0, 1, 0, 1]);
    assert_eq!(shard.session_mut(identity(0, 1)).unwrap().queued(), 1);
    assert_eq!(shard.session_mut(identity(1, 1)).unwrap().queued(), 1);
    consume(shard.session_mut(identity(1, 1)).unwrap());
    for slot in 0..2 {
        receive(shard.session_mut(identity(slot, 1)).unwrap(), 1, 1, 0, 1, 3).unwrap();
    }
    for _ in 0..4 {
        shard.service(|_, result| {
            result.unwrap();
        });
    }
    let received = consume(shard.session_mut(identity(1, 1)).unwrap());
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].0.start, at(1));
    assert_eq!(shard.session_mut(identity(0, 1)).unwrap().queued(), 1);
}

#[test]
fn replacement_preserves_cfn_continuity_and_includes_removed_longer_ttis() {
    let old = Box::new(session(0, 2, configuration_with_interval(Rel99Tti::Ms40)));
    let resources = old.resources();
    let mut shard = DedicatedRxShard::new(
        epoch(),
        ShardLimits {
            maximum_ues: count(1),
            maximum_sessions_per_step: count(1),
            resources: ReceiveResources {
                physical_sample_bytes: 0,
                coded_bits: resources.coded_bits * 2,
                output_bytes: resources.output_bytes * 2,
                demultiplexed_bits: resources.demultiplexed_bits * 2,
                channels: resources.channels * 2,
            },
        },
    )
    .unwrap();
    assert!(shard.install(old).is_ok());
    shard.remove(identity(0, 1)).unwrap().retire();
    let config = configuration();
    let early = DedicatedReceiveSession::new(
        identity(0, 2),
        FrameAnchor {
            time: at(2),
            cfn: 2,
        },
        Arc::clone(&config),
        limits(2),
    )
    .unwrap();
    assert!(matches!(
        shard.install(Box::new(early)),
        Err((ReceiveError::Time, _))
    ));
    let wrong_cfn = DedicatedReceiveSession::new(
        identity(0, 2),
        FrameAnchor {
            time: at(4),
            cfn: 8,
        },
        Arc::clone(&config),
        limits(2),
    )
    .unwrap();
    assert!(matches!(
        shard.install(Box::new(wrong_cfn)),
        Err((ReceiveError::Time, _))
    ));
    let next = DedicatedReceiveSession::new(
        identity(0, 2),
        FrameAnchor {
            time: at(4),
            cfn: 4,
        },
        config,
        limits(2),
    )
    .unwrap();
    assert!(shard.install(Box::new(next)).is_ok());
}
