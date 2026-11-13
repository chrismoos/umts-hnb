// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::rel99 as fixture;
use crate::test_support::{at, count};
use std::num::NonZeroU8;
use umts_dsp::{
    bits::Bit,
    coding::{CodedBlockDecoder, CodedBlockEncoder, DecodeBudget},
    transport_blocks::BlockOutcome,
};
use umts_rrc::{
    dedicated::{DchDirection, ExistingDchSetup, InitialDchSetup, ScheduledActivation},
    establishment::TransactionId,
};

fn limits() -> TransportPlanLimits {
    TransportPlanLimits {
        maximum_coded_bits: count(200_000),
        maximum_mapping_entries: count(300_000),
        decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
    }
}
#[test]
fn requirements_match_independent_transport_physical_and_receive_storage_geometry() {
    use crate::dedicated_rx::{
        DedicatedReceiveSession, FrameAnchor, ReceiveIdentity, ReceiveLimits,
    };
    use umts_types::{
        UeKey,
        time::{CHIPS_PER_SLOT, SLOTS_PER_FRAME},
    };
    for (voice, packet) in [(true, false), (false, true), (true, true)] {
        for flexible in [false, true] {
            let (ul, dl, bearers, mut physical) = fixture::components(voice, packet, 10);
            physical.flexible_position = flexible;
            let configuration =
                Arc::new(Rel99Configuration::new(ul, dl, bearers, physical).unwrap());
            let plan = CompiledRel99::new(configuration.clone(), count(200_000), limits()).unwrap();
            let requirements = plan.requirements().unwrap();
            let maximum_payload = |direction, tfcs: &DchTfcs| {
                tfcs.transports()
                    .iter()
                    .flat_map(|transport| {
                        let multiplexed =
                            configuration.multiplexed(direction, transport.id).unwrap();
                        transport
                            .formats()
                            .map(move |format| {
                                format.transport_bits(multiplexed) * usize::from(format.blocks)
                            })
                            .collect::<Vec<_>>()
                    })
                    .max()
                    .unwrap()
            };
            assert_eq!(
                requirements.maximum_uplink_transport_output_bits,
                maximum_payload(DchDirection::Uplink, configuration.uplink())
            );
            assert_eq!(
                requirements.maximum_downlink_transport_output_bits,
                maximum_payload(DchDirection::Downlink, configuration.downlink())
            );
            assert_eq!(
                requirements.maximum_uplink_transport_output_bits,
                if packet { 1280 } else { 148 }
            );
            assert_eq!(
                requirements.maximum_downlink_transport_output_bits,
                if packet { 1280 } else { 340 }
            );
            let expected_coded = |configured: &ConfiguredTfcs| {
                configured
                    .transport_ids()
                    .map(|id| {
                        configured
                            .definition(id)
                            .unwrap()
                            .formats
                            .iter()
                            .enumerate()
                            .map(|(index, _)| {
                                configured.block_plan(id, index).unwrap().coded_bits()
                            })
                            .sum::<usize>()
                    })
                    .sum::<usize>()
            };
            assert_eq!(
                requirements.uplink_coded_bits,
                expected_coded(plan.uplink_configured())
            );
            assert_eq!(
                requirements.downlink_coded_bits,
                expected_coded(plan.downlink_configured())
            );
            let mut maximum_demultiplexed = 0;
            for combination in plan.uplink_configured().combinations() {
                let layout = plan.uplink().allocation(combination.tfci).unwrap().layout();
                let frame_bits = (0..layout.len())
                    .map(|channel| {
                        let channel = layout.channel(channel).unwrap();
                        assert_eq!(
                            channel.bits_per_slot(),
                            CHIPS_PER_SLOT as usize / channel.code().spreading_factor() as usize
                        );
                        channel.bits_per_slot() * SLOTS_PER_FRAME as usize
                    })
                    .sum::<usize>();
                maximum_demultiplexed = maximum_demultiplexed.max(frame_bits);
            }
            assert_eq!(
                requirements.maximum_uplink_demultiplexed_bits,
                maximum_demultiplexed
            );
            let physical = configuration.physical();
            let slot = (0..=16)
                .filter_map(|number| DownlinkFormat::new(number, physical.downlink_tfci).ok())
                .find(|slot| {
                    slot.spreading_factor() == physical.downlink_sf
                        && slot.pilots().bits() == usize::from(physical.downlink_pilot_bits)
                })
                .unwrap();
            assert_eq!(
                requirements.downlink_channels,
                physical.downlink_codes.len()
            );
            assert_eq!(
                requirements.downlink_frame_payload_bits,
                slot.data_bits() * SLOTS_PER_FRAME as usize * physical.downlink_codes.len()
            );
            let mut matched_maximum = 0;
            for combination in plan.downlink_configured().combinations() {
                let mut total = 0;
                for (id, &format) in plan
                    .downlink_configured()
                    .transport_ids()
                    .zip(&combination.formats)
                {
                    total += match plan.downlink() {
                        DownlinkPlans::Fixed(plan) => plan.matched_tti_bits(id, format),
                        DownlinkPlans::Flexible(plan) => plan.matched_tti_bits(id, format),
                    }
                    .unwrap();
                }
                matched_maximum = matched_maximum.max(total);
            }
            assert_eq!(
                requirements.maximum_downlink_matched_tti_bits,
                matched_maximum
            );
            let slots = count(3);
            let bytes = DedicatedReceiveSession::output_storage_bytes(
                slots,
                requirements.maximum_uplink_transport_output_bits,
            )
            .unwrap()
                + plan
                    .uplink_configured()
                    .transport_ids()
                    .map(|id| {
                        plan.uplink_configured()
                            .definition(id)
                            .unwrap()
                            .formats
                            .iter()
                            .map(|format| format.block_bits * format.blocks)
                            .max()
                            .unwrap()
                            * std::mem::size_of::<umts_dsp::bits::Bit>()
                            + std::mem::size_of::<usize>()
                    })
                    .sum::<usize>();
            assert_eq!(requirements.receive_output_storage_bytes(slots), Ok(bytes));
            let session = DedicatedReceiveSession::new(
                ReceiveIdentity {
                    ue: UeKey {
                        epoch: crate::test_support::at(0).epoch,
                        slot: 0,
                        generation: std::num::NonZeroU64::MIN,
                    },
                    configuration: std::num::NonZeroU64::MIN,
                },
                FrameAnchor {
                    time: crate::test_support::at(0),
                    cfn: 0,
                },
                plan.uplink().clone(),
                ReceiveLimits {
                    output_slots: slots,
                    maximum_output_bits: count(requirements.maximum_uplink_transport_output_bits),
                    maximum_output_bytes: bytes,
                    maximum_coded_bits: count(requirements.uplink_coded_bits),
                    maximum_frame_bits: count(requirements.maximum_uplink_demultiplexed_bits),
                    maximum_decodes_per_step: count(8),
                },
            )
            .unwrap();
            assert_eq!(session.resources().output_bytes, bytes);
            for frames in [1, 8, 16] {
                assert_eq!(
                    requirements.transmit_frame_window_bits(count(frames)),
                    Ok(requirements.downlink_frame_payload_bits * (frames + 1))
                );
            }
        }
    }
}

#[test]
fn requirement_payloads_do_not_include_crc_a_second_time() {
    let plan = CompiledRel99::new(
        fixture::configuration(false, true, 10),
        count(200_000),
        limits(),
    )
    .unwrap();
    let requirements = plan.requirements().unwrap();
    for (configured, packet_id) in [
        (plan.uplink_configured(), 4),
        (plan.downlink_configured(), 9),
    ] {
        let packet = configured
            .definition(TransportId::new(packet_id).unwrap())
            .unwrap();
        let full = packet
            .formats
            .iter()
            .find(|format| format.blocks == 4)
            .unwrap();
        assert_eq!(
            (full.block_bits, full.crc.width(), full.blocks),
            (320, 16, 4)
        );
        assert_eq!(full.block_bits * full.blocks, 1280);
        assert_ne!(
            requirements.maximum_uplink_transport_output_bits,
            (full.block_bits + full.crc.width()) * full.blocks
        );
    }
    assert_eq!(requirements.maximum_uplink_transport_output_bits, 1280);
    assert_eq!(requirements.maximum_downlink_transport_output_bits, 1280);
}

#[test]
fn requirement_storage_and_frame_window_arithmetic_reject_overflow() {
    let plan = CompiledRel99::new(
        fixture::configuration(true, true, 10),
        count(200_000),
        limits(),
    )
    .unwrap();
    let requirements = plan.requirements().unwrap();
    assert_eq!(
        requirements.transmit_frame_window_bits(count(usize::MAX)),
        Err(Rel99PlanError::Capacity)
    );
    let single = PlanRequirements {
        downlink_frame_payload_bits: 1,
        ..requirements
    };
    assert_eq!(
        single.transmit_frame_window_bits(count(usize::MAX)),
        Err(Rel99PlanError::Capacity)
    );
    let huge = PlanRequirements {
        downlink_frame_payload_bits: usize::MAX,
        ..requirements
    };
    assert_eq!(
        huge.transmit_frame_window_bits(count(2)),
        Err(Rel99PlanError::Capacity)
    );
    assert_eq!(
        requirements.receive_output_storage_bytes(count(usize::MAX)),
        Err(Rel99PlanError::Capacity)
    );
    let huge = PlanRequirements {
        maximum_uplink_transport_output_bits: usize::MAX,
        ..requirements
    };
    assert_eq!(
        huge.receive_output_storage_bytes(count(1)),
        Err(Rel99PlanError::Capacity)
    );
}

#[test]
fn canonical_wire_plan_compiles_asymmetric_phy_and_preserves_one_exact_snapshot() {
    for (voice, packet) in [(true, false), (false, true), (true, true)] {
        let configuration = fixture::configuration(voice, packet, 10);
        let encoded = if voice && packet {
            let previous = fixture::configuration(true, false, 10);
            let initial = InitialDchSetup::new(
                previous.clone(),
                fixture::retained(),
                fixture::rabs(true, false),
            )
            .unwrap();
            assert!(
                !initial
                    .message(TransactionId::new(0).unwrap())
                    .unwrap()
                    .bytes
                    .is_empty()
            );
            ExistingDchSetup::new(
                previous,
                configuration.clone(),
                fixture::rabs(false, true),
                ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap(),
            )
            .unwrap()
            .message(TransactionId::new(0).unwrap())
            .unwrap()
        } else {
            InitialDchSetup::new(
                configuration.clone(),
                fixture::retained(),
                fixture::rabs(voice, packet),
            )
            .unwrap()
            .message(TransactionId::new(0).unwrap())
            .unwrap()
        };
        assert!(!encoded.bytes.is_empty());
        let plan = CompiledRel99::new(configuration.clone(), count(200_000), limits()).unwrap();
        assert!(Arc::ptr_eq(plan.configuration(), &configuration));
        assert!(Arc::ptr_eq(
            plan.uplink().configured(),
            plan.uplink_configured()
        ));
        for (configured, id, bits) in [
            (plan.uplink_configured(), 5, 148),
            (plan.downlink_configured(), 10, 340),
        ] {
            let definition = configured
                .definition(TransportId::new(id).unwrap())
                .unwrap();
            assert!(
                definition
                    .formats
                    .iter()
                    .all(|format| format.block_bits == bits)
            );
        }
        assert_eq!(plan.uplink_control().tfci_bits(), 2);
        for index in 0..configuration.uplink().combinations().len() {
            let tfci = Tfci::new(index as u16).unwrap();
            assert!(plan.uplink().allocation(tfci).is_some());
            assert_eq!(plan.uplink_gains(tfci).unwrap().ratio(), 1.0);
        }
        if voice {
            let empty = plan
                .downlink_configured()
                .definition(TransportId::new(6).unwrap())
                .unwrap()
                .formats[0];
            assert_eq!(
                (empty.block_bits, empty.blocks, empty.crc.width()),
                (0, 1, 12)
            );
        }
        assert_eq!(configuration.multiplexed(DchDirection::Uplink, 5), Ok(true));
    }
}
#[test]
fn compiled_canonical_transport_recovers_every_payload_and_crc_in_both_directions() {
    let configuration = fixture::configuration(true, true, 10);
    let plan = CompiledRel99::new(configuration, count(200_000), limits()).unwrap();
    for configured in [plan.uplink_configured(), plan.downlink_configured()] {
        for id in configured.transport_ids() {
            let definition = configured.definition(id).unwrap();
            for (index, format) in definition.formats.iter().enumerate() {
                let block_plan = configured.block_plan(id, index).unwrap();
                let payload: Vec<_> = (0..format.block_bits * format.blocks)
                    .map(|index| if index % 3 == 0 { Bit::One } else { Bit::Zero })
                    .collect();
                let mut coded = vec![Bit::Zero; block_plan.coded_bits()];
                CodedBlockEncoder::new(block_plan)
                    .unwrap()
                    .encode(&payload, &mut coded)
                    .unwrap();
                let soft: Vec<_> = coded
                    .iter()
                    .map(|bit| if *bit == Bit::Zero { 8.0 } else { -8.0 })
                    .collect();
                let mut output = vec![Bit::Zero; payload.len()];
                let outcome = CodedBlockDecoder::new(block_plan, limits().decode)
                    .unwrap()
                    .decode(&soft, &mut output)
                    .unwrap();
                assert_eq!(
                    outcome,
                    if format.blocks == 0 {
                        BlockOutcome::Absent
                    } else {
                        BlockOutcome::Decoded
                    }
                );
                assert_eq!(output, payload);
            }
        }
    }
}
#[test]
fn fixed_and_flexible_downlink_are_distinct_plans_with_the_same_canonical_formats() {
    for flexible in [false, true] {
        let (ul, dl, bearers, mut physical) = fixture::components(true, true, 10);
        physical.flexible_position = flexible;
        let configuration = Arc::new(Rel99Configuration::new(ul, dl, bearers, physical).unwrap());
        let plan = CompiledRel99::new(configuration, count(200_000), limits()).unwrap();
        assert_eq!(
            matches!(plan.downlink(), DownlinkPlans::Flexible(_)),
            flexible
        );
    }
}
#[test]
fn capacity_or_physical_limit_failure_does_not_silently_rewrite_wire_rate_or_tfcs() {
    let configuration = fixture::configuration(true, true, 10);
    assert!(CompiledRel99::new(configuration.clone(), count(1), limits()).is_err());
    let (ul, dl, bearers, mut physical) = fixture::components(true, true, 10);
    physical.uplink_minimum_sf = 256;
    let limited = Arc::new(Rel99Configuration::new(ul, dl, bearers, physical).unwrap());
    assert!(CompiledRel99::new(limited.clone(), count(200_000), limits()).is_err());
    assert_eq!(
        limited.uplink().combinations().len(),
        configuration.uplink().combinations().len()
    );
    assert_eq!(limited.physical().uplink_minimum_sf, 256);
}
