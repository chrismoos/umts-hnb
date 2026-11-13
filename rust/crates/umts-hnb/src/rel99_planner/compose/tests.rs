// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    rel99_planner::tests::{cs_request, limits, ps_request},
    test_support::{count, rel99 as fixture},
};
use umts_rrc::transport::ConvolutionalRate;

const POLICY_GAIN: GainFactors = GainFactors {
    control: 15,
    data: 12,
};

fn configured() -> ConfiguredPolicy {
    let config = serde_yaml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-support/fixtures/config/rel99.yaml"
    )))
    .unwrap();
    ConfiguredPolicy::new(&config).unwrap()
}

fn packet<'a>(configured: &'a ConfiguredPolicy, request: &'a SetupOrModify) -> &'a PacketProfile {
    let selected = configured
        .services()
        .select(&[(CnDomain::Ps, request)])
        .unwrap();
    let Service::Packet(profile) = selected.rabs()[0].service else {
        panic!("packet profile")
    };
    profile
}

fn circuit<'a>(configured: &'a ConfiguredPolicy, request: &'a SetupOrModify) -> &'a CircuitProfile {
    let selected = configured
        .services()
        .select(&[(CnDomain::Cs, request)])
        .unwrap();
    let Service::Circuit(profile) = selected.rabs()[0].service else {
        panic!("circuit profile")
    };
    profile
}

fn channel(id: u8, bits: u16) -> DchTransport {
    DchTransport {
        id,
        interval: TransportInterval::Ms40,
        coding: ChannelCoding::Convolutional(ConvolutionalRate::Third),
        crc: CrcSize::Bits16,
        rate_matching_attribute: 211,
        groups: vec![DchFormatGroup {
            rlc_bits: bits,
            blocks: vec![0, 1],
        }],
    }
}

fn base() -> Rel99Configuration {
    base_with_idle_gain(0)
}

fn base_with_idle_gain(idle_gain: u8) -> Rel99Configuration {
    let uplink = DchTfcs::new(
        DchDirection::Uplink,
        vec![channel(2, 144), channel(32, 320)],
        vec![
            Combination {
                formats: vec![0, 0],
                gain: Some(GainFactors {
                    control: 15,
                    data: idle_gain,
                }),
            },
            Combination {
                formats: vec![1, 0],
                gain: Some(GainFactors {
                    control: 15,
                    data: 7,
                }),
            },
            Combination {
                formats: vec![0, 1],
                gain: Some(GainFactors {
                    control: 15,
                    data: 9,
                }),
            },
            Combination {
                formats: vec![1, 1],
                gain: Some(GainFactors {
                    control: 15,
                    data: 11,
                }),
            },
        ],
    )
    .unwrap();
    let downlink = DchTfcs::new(
        DchDirection::Downlink,
        vec![channel(1, 336), channel(31, 320)],
        vec![
            Combination {
                formats: vec![0, 0],
                gain: None,
            },
            Combination {
                formats: vec![0, 1],
                gain: None,
            },
            Combination {
                formats: vec![1, 1],
                gain: None,
            },
        ],
    )
    .unwrap();
    Rel99Configuration::new(
        uplink,
        downlink,
        vec![
            Rel99Bearer {
                id: 1,
                domain: None,
                rlc: Rel99Rlc::Unacknowledged,
                mapping: fixture::mapping(2, 1, Some(1)),
            },
            Rel99Bearer {
                id: 32,
                domain: Some(CnDomain::Ps),
                rlc: fixture::am(),
                mapping: fixture::mapping(32, 31, None),
            },
        ],
        fixture::physical(),
    )
    .unwrap()
}

fn finish(
    composition: &Composition,
    old: &Rel99Configuration,
    maximum: usize,
) -> Result<Rel99Configuration, Error> {
    let configured = configured();
    let mut policy = configured.policy();
    policy.uplink_gains = POLICY_GAIN;
    policy.maximum_combinations = count(maximum);
    composition.configuration(old.physical().clone(), &limits(), &policy)
}

fn projected(row: &Combination, current: &DchTfcs, old: &DchTfcs) -> Vec<usize> {
    old.transports()
        .iter()
        .map(|channel| {
            let index = current
                .transports()
                .iter()
                .position(|candidate| candidate.id == channel.id)
                .unwrap();
            row.formats[index]
        })
        .collect()
}

fn assert_preserved(old: &Rel99Configuration, current: &Rel99Configuration) {
    for bearer in old.bearers() {
        assert_eq!(current.bearer(bearer.id), Some(bearer));
    }
    for (old, current) in [
        (old.uplink(), current.uplink()),
        (old.downlink(), current.downlink()),
    ] {
        for channel in old.transports() {
            let retained = current.transport(channel.id).unwrap();
            assert_eq!(
                retained.wire_uplink().unwrap(),
                channel.wire_uplink().unwrap()
            );
            assert_eq!(
                retained.wire_downlink().unwrap(),
                channel.wire_downlink().unwrap()
            );
            assert_eq!(
                retained.formats().collect::<Vec<_>>(),
                channel.formats().collect::<Vec<_>>()
            );
        }
        for (index, row) in current.combinations().iter().enumerate() {
            let expected = &old.combinations()[index % old.combinations().len()];
            assert_eq!(projected(row, current, old), expected.formats);
            let added_active =
                current
                    .transports()
                    .iter()
                    .zip(&row.formats)
                    .any(|(channel, index)| {
                        old.transport(channel.id).is_none()
                            && channel.formats().nth(*index).is_some_and(|format| {
                                format.blocks != 0
                                    && (format.rlc_bits != 0
                                        || !matches!(channel.crc, CrcSize::None))
                            })
                    });
            if current.direction() == DchDirection::Uplink && added_active {
                assert_eq!(row.gain, Some(POLICY_GAIN));
            } else {
                assert_eq!(row.gain, expected.gain);
            }
        }
    }
}

#[test]
fn existing_configuration_without_additions_keeps_exact_rows_bearers_and_gains() {
    let old = base();
    let composition = Composition::from_existing(&old).unwrap();
    let current = finish(&composition, &old, 4).unwrap();
    assert_preserved(&old, &current);
    assert_eq!(current.bearers(), old.bearers());
    assert_eq!(current.uplink().ctfc_values(), old.uplink().ctfc_values());
    assert_eq!(
        current.downlink().ctfc_values(),
        old.downlink().ctfc_values()
    );
}

#[test]
fn appended_packet_fills_directional_id_holes_and_preserves_zero_gain_only_for_idle_addition() {
    let old = base();
    let configured = configured();
    let request = ps_request(12_800);
    let mut composition = Composition::from_existing(&old).unwrap();
    let id = composition
        .packet(packet(&configured, &request), [1, 2])
        .unwrap();
    assert_eq!(id, FIRST_USER_BEARER);
    let current = finish(&composition, &old, 9).unwrap();
    let added = current.bearer(id).unwrap();
    assert_eq!(added.mapping.uplink[0].transport, 1);
    assert_eq!(added.mapping.downlink[0].transport, 2);
    assert_eq!(
        current
            .uplink()
            .transports()
            .iter()
            .map(|channel| channel.id)
            .collect::<Vec<_>>(),
        [1, 2, 32]
    );
    assert_eq!(
        current
            .downlink()
            .transports()
            .iter()
            .map(|channel| channel.id)
            .collect::<Vec<_>>(),
        [1, 2, 31]
    );
    assert_eq!(current.uplink().combinations().len(), 8);
    assert_eq!(current.downlink().combinations().len(), 9);
    assert_preserved(&old, &current);
    assert_eq!(current.uplink().combinations()[0].gain.unwrap().data, 0);
    assert_eq!(current.uplink().combinations()[4].gain, Some(POLICY_GAIN));
    for row in current.uplink().combinations() {
        let retained = projected(row, current.uplink(), old.uplink());
        assert!(
            old.uplink()
                .combinations()
                .iter()
                .any(|old| old.formats == retained)
        );
    }
}

#[test]
fn appended_circuit_preserves_asymmetric_old_packet_rows_and_keeps_new_tm_subflows_correlated() {
    let old = base_with_idle_gain(3);
    let configured = configured();
    let request = cs_request();
    let requirements = Requirements::new(CnDomain::Cs, &request).unwrap();
    let mut composition = Composition::from_existing(&old).unwrap();
    let added = composition
        .circuit(&requirements, circuit(&configured, &request))
        .unwrap();
    assert_eq!(added, [5, 6, 7]);
    let current = finish(&composition, &old, 1024).unwrap();
    assert_preserved(&old, &current);
    for (old, current, direction) in [
        (old.uplink(), current.uplink(), DchDirection::Uplink),
        (old.downlink(), current.downlink(), DchDirection::Downlink),
    ] {
        let mut allowed: Vec<Vec<usize>> = vec![vec![0; added.len()]];
        for request in &requirements.combinations {
            let row = added
                .iter()
                .enumerate()
                .map(|(index, id)| {
                    let bearer = composition
                        .bearers
                        .iter()
                        .find(|bearer| bearer.id == *id)
                        .unwrap();
                    let mapping = bearer.mappings(direction).next().unwrap();
                    let channel = current.transport(mapping.transport).unwrap();
                    let bits = request.subflow_bits[index];
                    if bits == 0 {
                        0
                    } else {
                        channel
                            .formats()
                            .position(|format| format.rlc_bits == bits && format.blocks == 1)
                            .unwrap()
                    }
                })
                .collect::<Vec<_>>();
            if !allowed.contains(&row) {
                allowed.push(row);
            }
        }
        assert_eq!(
            current.combinations().len(),
            old.combinations().len() * allowed.len()
        );
        for row in current.combinations() {
            let selected: Vec<_> = added
                .iter()
                .map(|id| {
                    let bearer = composition
                        .bearers
                        .iter()
                        .find(|bearer| bearer.id == *id)
                        .unwrap();
                    let mapping = bearer.mappings(direction).next().unwrap();
                    let index = current
                        .transports()
                        .iter()
                        .position(|channel| channel.id == mapping.transport)
                        .unwrap();
                    row.formats[index]
                })
                .collect();
            assert!(allowed.contains(&selected));
        }
    }
}

#[test]
fn added_circuit_idle_crc_parity_uses_explicit_gain_even_when_old_empty_tfc_has_zero_data_gain() {
    let old = base();
    let configured = configured();
    let request = cs_request();
    let mut composition = Composition::from_existing(&old).unwrap();
    let added = composition
        .circuit(
            &Requirements::new(CnDomain::Cs, &request).unwrap(),
            circuit(&configured, &request),
        )
        .unwrap();
    let current = finish(&composition, &old, 1024).unwrap();
    assert_preserved(&old, &current);
    assert_eq!(old.uplink().combinations()[0].gain.unwrap().data, 0);
    let first = &current.uplink().combinations()[0];
    assert_eq!(first.gain, Some(POLICY_GAIN));
    assert_eq!(
        projected(first, current.uplink(), old.uplink()),
        old.uplink().combinations()[0].formats
    );
    let mapping = current.bearer(added[0]).unwrap().mapping.uplink[0];
    let index = current
        .uplink()
        .transports()
        .iter()
        .position(|channel| channel.id == mapping.transport)
        .unwrap();
    let channel = current.uplink().transport(mapping.transport).unwrap();
    let format = channel.formats().nth(first.formats[index]).unwrap();
    assert_eq!((format.rlc_bits, format.blocks), (0, 1));
    assert!(matches!(channel.crc, CrcSize::Bits12));
}

#[test]
fn existing_combined_configuration_accepts_packet_and_circuit_without_any_id_collision() {
    let old = fixture::configuration(true, true, 10);
    let configured = configured();
    let packet_request = ps_request(12_800);
    let circuit_request = cs_request();
    let mut composition = Composition::from_existing(&old).unwrap();
    let packet = composition
        .packet(packet(&configured, &packet_request), [1, 1])
        .unwrap();
    let circuit = composition
        .circuit(
            &Requirements::new(CnDomain::Cs, &circuit_request).unwrap(),
            circuit(&configured, &circuit_request),
        )
        .unwrap();
    assert_eq!(packet, 5);
    assert_eq!(circuit, [6, 7, 8]);
    let current = finish(&composition, &old, 1024).unwrap();
    assert_preserved(&old, &current);
    for (old, current) in [
        (old.uplink(), current.uplink()),
        (old.downlink(), current.downlink()),
    ] {
        let ids = current
            .transports()
            .iter()
            .map(|channel| channel.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), current.transports().len());
        assert_eq!(current.transports().len(), old.transports().len() + 4);
    }
}

#[test]
fn correlated_product_is_bounded_by_each_direction_before_configuration_is_built() {
    let old = base();
    let configured = configured();
    let request = ps_request(12_800);
    let mut composition = Composition::from_existing(&old).unwrap();
    composition
        .packet(packet(&configured, &request), [1, 2])
        .unwrap();
    assert!(matches!(
        finish(&composition, &old, 6),
        Err(Error::CombinationBudget { maximum: 6, .. })
    ));
    assert!(finish(&composition, &old, 8).is_ok());
    assert!(finish(&composition, &old, 9).is_ok());
    assert!(matches!(
        composition.direction(DchDirection::Uplink, 7, POLICY_GAIN),
        Err(Error::CombinationBudget { maximum: 7, .. })
    ));
    assert_eq!(
        composition
            .direction(DchDirection::Uplink, 8, POLICY_GAIN)
            .unwrap()
            .combinations()
            .len(),
        8
    );
}

fn selectable_group(id: u8, bits: u16, interval: TransportInterval, maximum: u16) -> Group {
    let mut transport = channel(id, bits);
    transport.interval = interval;
    transport.groups[0].blocks = (0..=maximum).collect();
    if maximum > 1 {
        transport.coding = ChannelCoding::Turbo;
    }
    let choices: Vec<_> = (0..=maximum)
        .map(|index| Combination {
            formats: vec![usize::from(index)],
            gain: None,
        })
        .collect();
    Group {
        uplink: vec![transport.clone()],
        downlink: vec![transport],
        ul_rows: choices.clone(),
        dl_rows: choices,
        policy: if maximum > 1 {
            GroupPolicy::Packet { minimum: [0, 0] }
        } else {
            GroupPolicy::Signalling
        },
    }
}

fn initial_packet_composition() -> Composition {
    Composition {
        groups: vec![
            selectable_group(1, 144, TransportInterval::Ms40, 1),
            selectable_group(2, 232, TransportInterval::Ms40, 1),
            selectable_group(3, 320, TransportInterval::Ms20, 31),
        ],
        bearers: Vec::new(),
        used_rb: BTreeSet::new(),
        used_ul_transport: BTreeSet::new(),
        used_dl_transport: BTreeSet::new(),
        existing: false,
    }
}

fn assert_shorter_tti_closure(tfcs: &DchTfcs) {
    let empty: Vec<_> = tfcs
        .transports()
        .iter()
        .map(|channel| {
            channel
                .formats()
                .position(|format| format.blocks == 0 || format.rlc_bits == 0)
                .unwrap()
        })
        .collect();
    for row in tfcs.combinations() {
        for interval in [
            TransportInterval::Ms10,
            TransportInterval::Ms20,
            TransportInterval::Ms40,
            TransportInterval::Ms80,
        ] {
            if !tfcs
                .transports()
                .iter()
                .any(|channel| channel.interval == interval)
            {
                continue;
            }
            assert!(
                tfcs.combinations().iter().any(|candidate| {
                    tfcs.transports()
                        .iter()
                        .enumerate()
                        .all(|(index, channel)| {
                            match (channel.interval as u8).cmp(&(interval as u8)) {
                                std::cmp::Ordering::Less => {
                                    candidate.formats[index] == empty[index]
                                }
                                std::cmp::Ordering::Equal => {
                                    candidate.formats[index] == row.formats[index]
                                }
                                std::cmp::Ordering::Greater => true,
                            }
                        })
                }),
                "missing closure for {:?} at {:?}",
                row.formats,
                interval
            );
        }
    }
}

#[test]
fn reference_sized_tfcs_keeps_minimum_rows_all_packet_rates_and_joint_peak() {
    let composition = initial_packet_composition();
    let uplink = composition
        .direction(DchDirection::Uplink, 64, POLICY_GAIN)
        .unwrap();
    let downlink = composition
        .direction(DchDirection::Downlink, 128, POLICY_GAIN)
        .unwrap();
    assert_eq!(uplink.combinations().len(), 64);
    assert_eq!(downlink.combinations().len(), 128);
    for row in [
        vec![0, 0, 0],
        vec![1, 0, 0],
        vec![0, 1, 0],
        vec![0, 0, 1],
        vec![1, 1, 31],
    ] {
        assert!(
            uplink
                .combinations()
                .iter()
                .any(|candidate| candidate.formats == row)
        );
    }
    for packet in 0..=31 {
        assert!(
            uplink
                .combinations()
                .iter()
                .any(|row| row.formats[2] == packet)
        );
    }
    assert_shorter_tti_closure(&uplink);
    let repeated = composition
        .direction(DchDirection::Uplink, 64, POLICY_GAIN)
        .unwrap();
    assert_eq!(
        uplink
            .combinations()
            .iter()
            .map(|row| (&row.formats, row.gain))
            .collect::<Vec<_>>(),
        repeated
            .combinations()
            .iter()
            .map(|row| (&row.formats, row.gain))
            .collect::<Vec<_>>()
    );
}

#[test]
fn bounded_combined_voice_preserves_every_synchronous_mode_at_peak_packet_rate() {
    let mut composition = initial_packet_composition();
    let channels: Vec<_> = [(4, 81), (5, 103), (6, 60)]
        .into_iter()
        .map(|(id, bits)| {
            let mut transport = channel(id, bits);
            transport.interval = TransportInterval::Ms20;
            transport.groups = vec![
                DchFormatGroup {
                    rlc_bits: 0,
                    blocks: vec![1],
                },
                DchFormatGroup {
                    rlc_bits: bits,
                    blocks: vec![1],
                },
            ];
            transport
        })
        .collect();
    let modes = [vec![0, 0, 0], vec![1, 0, 0], vec![1, 1, 1]];
    let choices: Vec<_> = modes
        .iter()
        .map(|formats| Combination {
            formats: formats.clone(),
            gain: None,
        })
        .collect();
    composition.groups.push(Group {
        uplink: channels.clone(),
        downlink: channels,
        ul_rows: choices.clone(),
        dl_rows: choices,
        policy: GroupPolicy::Retain,
    });
    let uplink = composition
        .direction(DchDirection::Uplink, 64, POLICY_GAIN)
        .unwrap();
    for row in uplink.combinations() {
        assert!(
            modes
                .iter()
                .any(|mode| mode.as_slice() == &row.formats[3..])
        );
    }
    for mode in modes {
        assert!(
            uplink
                .combinations()
                .iter()
                .any(|row| row.formats[2] == 31 && row.formats[3..] == mode)
        );
    }
    assert_shorter_tti_closure(&uplink);
}

#[test]
fn count_limited_existing_tfcs_keeps_old_indices_gains_and_every_old_mode_at_packet_peak() {
    let old = base();
    let configured = configured();
    let request = ps_request(12_800);
    let mut composition = Composition::from_existing(&old).unwrap();
    composition
        .packet(packet(&configured, &request), [31, 31])
        .unwrap();
    let mut bearers = composition.bearers.clone();
    bearers.sort_by_key(|bearer| bearer.id);
    let current = Rel99Configuration::new(
        composition
            .direction(DchDirection::Uplink, 64, POLICY_GAIN)
            .unwrap(),
        composition
            .direction(DchDirection::Downlink, 64, POLICY_GAIN)
            .unwrap(),
        bearers,
        old.physical().clone(),
    )
    .unwrap();
    for (old, current) in [
        (old.uplink(), current.uplink()),
        (old.downlink(), current.downlink()),
    ] {
        for (index, row) in old.combinations().iter().enumerate() {
            assert_eq!(
                projected(&current.combinations()[index], current, old),
                row.formats
            );
            assert_eq!(current.combinations()[index].gain, row.gain);
            assert!(current.combinations().iter().any(|candidate| {
                projected(candidate, current, old) == row.formats
                    && current
                        .transports()
                        .iter()
                        .zip(&candidate.formats)
                        .filter(|(channel, _)| old.transport(channel.id).is_none())
                        .all(|(_, index)| *index == 31)
            }));
        }
    }
    assert_shorter_tti_closure(current.uplink());
}

#[test]
fn irreducible_minimum_and_peak_budget_is_rejected_without_truncation() {
    let composition = initial_packet_composition();
    assert!(
        matches!(composition.direction(DchDirection::Uplink, 4, POLICY_GAIN),
        Err(Error::CombinationBudget { required, maximum: 4, .. }) if required > 4)
    );
    let complete = composition
        .direction(DchDirection::Uplink, 64, POLICY_GAIN)
        .unwrap();
    assert!(
        complete
            .combinations()
            .iter()
            .any(|row| row.formats == [1, 1, 31])
    );
}

#[test]
fn large_cartesian_space_is_not_materialized_to_build_a_bounded_tfcs() {
    let mut composition = initial_packet_composition();
    composition.groups = (1..=4)
        .map(|id| selectable_group(id, 320, TransportInterval::Ms20, 31))
        .collect();
    let tfcs = composition
        .direction(DchDirection::Uplink, 64, POLICY_GAIN)
        .unwrap();
    assert_eq!(tfcs.combinations().len(), 64);
    assert!(tfcs.combinations().iter().any(|row| {
        tfcs.transports()
            .iter()
            .zip(&row.formats)
            .all(|(channel, index)| channel.formats().nth(*index).unwrap().blocks == 31)
    }));
    assert_shorter_tti_closure(&tfcs);
}

fn transport_limits() -> DirectionLimits {
    DirectionLimits {
        transport_bits: 6400,
        convolutional_bits: 6400,
        turbo_bits: Some(6400),
        transport_blocks: 32,
        transport_channels: 8,
        combinations: 64,
        formats: 64,
    }
}

#[test]
fn packet_peak_tracks_simultaneous_ue_bits_and_tb_budget_not_a_fixed_block_count() {
    let composition = initial_packet_composition();
    let limits = transport_limits();
    let uplink = composition
        .direction_budgeted(DchDirection::Uplink, 64, POLICY_GAIN, Some(limits))
        .unwrap();
    let counts = |row: &Combination| {
        uplink
            .transports()
            .iter()
            .zip(&row.formats)
            .map(|(channel, index)| channel.formats().nth(*index).unwrap().blocks)
            .collect::<Vec<_>>()
    };
    assert!(
        uplink
            .combinations()
            .iter()
            .any(|row| counts(row) == [0, 0, 20])
    );
    assert!(
        uplink
            .combinations()
            .iter()
            .any(|row| counts(row) == [1, 1, 18])
    );
    for row in uplink.combinations() {
        let mut usage = TransportUsage::default();
        for (channel, index) in uplink.transports().iter().zip(&row.formats) {
            let format = channel.formats().nth(*index).unwrap();
            usage = usage
                .checked_add(TransportUsage::transport(
                    channel.coding,
                    usize::from(format.rlc_bits) * usize::from(format.blocks),
                    usize::from(format.blocks),
                ))
                .unwrap();
        }
        limits.validate_usage(DchDirection::Uplink, usage).unwrap();
    }
    assert!(
        uplink
            .transports()
            .iter()
            .map(|channel| channel.formats().count())
            .sum::<usize>()
            <= limits.formats
    );
    assert!(
        uplink
            .transport(3)
            .unwrap()
            .formats()
            .all(|format| format.blocks <= 20)
    );
    assert_shorter_tti_closure(&uplink);
    let mut smaller = limits;
    smaller.transport_blocks = 4;
    let reduced = composition
        .direction_budgeted(DchDirection::Uplink, 64, POLICY_GAIN, Some(smaller))
        .unwrap();
    assert!(
        reduced
            .transport(3)
            .unwrap()
            .formats()
            .all(|format| format.blocks <= 4)
    );
    assert!(reduced.combinations().iter().any(|row| {
        reduced
            .transports()
            .iter()
            .zip(&row.formats)
            .map(|(channel, index)| channel.formats().nth(*index).unwrap().blocks)
            .collect::<Vec<_>>()
            == [1, 1, 2]
    }));
}

#[test]
fn stored_format_limit_is_applied_before_new_tfs_publication() {
    let composition = initial_packet_composition();
    let mut limits = transport_limits();
    limits.formats = 8;
    let tfcs = composition
        .direction_budgeted(DchDirection::Uplink, 64, POLICY_GAIN, Some(limits))
        .unwrap();
    assert!(
        tfcs.transports()
            .iter()
            .map(|channel| channel.formats().count())
            .sum::<usize>()
            <= 8
    );
    assert!(
        tfcs.transport(3)
            .unwrap()
            .formats()
            .any(|format| format.blocks == 20)
    );
    assert_shorter_tti_closure(&tfcs);
    limits.turbo_bits = None;
    assert!(matches!(
        composition.direction_budgeted(DchDirection::Uplink, 64, POLICY_GAIN, Some(limits)),
        Err(Error::Capability(
            umts_rrc::capabilities::rel99::Error::Exceeded {
                limit: umts_rrc::capabilities::rel99::Limit::TurboBits,
                ..
            }
        ))
    ));
}

#[test]
fn independent_directional_transport_spaces_can_extend_even_when_their_union_is_full() {
    let make = |direction, first| {
        let transports = (0..16)
            .map(|offset: u8| channel(first + offset * 2, 320))
            .collect::<Vec<_>>();
        DchTfcs::new(
            direction,
            transports,
            vec![Combination {
                formats: vec![0; 16],
                gain: (direction == DchDirection::Uplink).then_some(POLICY_GAIN),
            }],
        )
        .unwrap()
    };
    let old = Rel99Configuration::new(
        make(DchDirection::Uplink, 1),
        make(DchDirection::Downlink, 2),
        (0..16)
            .map(|offset| Rel99Bearer {
                id: FIRST_USER_BEARER + offset,
                domain: Some(CnDomain::Ps),
                rlc: fixture::am(),
                mapping: fixture::mapping(1 + offset * 2, 2 + offset * 2, None),
            })
            .collect(),
        fixture::physical(),
    )
    .unwrap();
    let configured = configured();
    let request = ps_request(12_800);
    let mut composition = Composition::from_existing(&old).unwrap();
    let id = composition
        .packet(packet(&configured, &request), [1, 1])
        .unwrap();
    let mut capabilities = limits();
    capabilities.uplink.transport_channels = 32;
    capabilities.downlink.transport_channels = 32;
    let physical = [old.physical().clone()];
    let mut policy = super::super::tests::policy(&physical);
    policy.maximum_combinations = count(2);
    policy.uplink_gains = POLICY_GAIN;
    let current = composition
        .configuration(old.physical().clone(), &capabilities, &policy)
        .unwrap();
    let added = current.bearer(id).unwrap();
    assert_eq!(added.mapping.uplink[0].transport, 2);
    assert_eq!(added.mapping.downlink[0].transport, 1);
    assert_preserved(&old, &current);
}

#[test]
fn physical_uplink_allocation_limits_peak_without_widening_puncturing() {
    let composition = initial_packet_composition();
    let physical = fixture::physical();
    assert_eq!(physical.puncturing_percent, 100);
    let budget = UplinkBudget::new(&physical, count(500_000)).unwrap();
    let tfcs = composition
        .direction_planned(
            DchDirection::Uplink,
            64,
            POLICY_GAIN,
            Some(transport_limits()),
            Some(&budget),
            None,
        )
        .unwrap();
    assert!(
        tfcs.transport(3)
            .unwrap()
            .formats()
            .any(|format| format.blocks == 19)
    );
    assert!(
        tfcs.transport(3)
            .unwrap()
            .formats()
            .all(|format| format.blocks <= 19)
    );
    assert_shorter_tti_closure(&tfcs);
}
