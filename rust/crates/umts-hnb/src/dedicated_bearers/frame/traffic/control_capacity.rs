// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_bearers::SignallingRoute;
use std::num::{NonZeroU16, NonZeroU64};
use umts_dsp::{
    coding::{ChannelCoding, CodedBlockFormat},
    convolutional::CodeRate,
    crc::TransportCrc,
    dedicated_transport::{CombinationDefinition, TransportDefinition},
    interleaving::Rel99Tti,
};
use umts_rlc::acknowledged::{AmEntity, AmLimits};
use umts_rrc::establishment::SignallingProfile;
use umts_types::{CnDomain, RadioEpoch};

const PDU_BYTES: usize = 18;
fn n(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}
fn identity() -> ReceiveIdentity {
    ReceiveIdentity {
        ue: UeKey {
            epoch: RadioEpoch::new(NonZeroU64::MIN),
            slot: 0,
            generation: NonZeroU64::MIN,
        },
        configuration: NonZeroU64::MIN,
    }
}
fn anchor() -> FrameAnchor {
    FrameAnchor {
        time: ChipInstant {
            epoch: identity().ue.epoch,
            index: 0,
        },
        cfn: 0,
    }
}
fn id(value: u8) -> BearerId {
    BearerId::new(value).unwrap()
}
fn bindings(multiplexed: bool) -> Vec<TransportBinding> {
    vec![
        TransportBinding {
            transport: TransportId::new(1).unwrap(),
            channels: TransportChannels::Single(id(5)),
        },
        TransportBinding {
            transport: TransportId::new(2).unwrap(),
            channels: if multiplexed {
                TransportChannels::Multiplexed(vec![(LogicalChannelId::new(1).unwrap(), id(2))])
            } else {
                TransportChannels::Single(id(2))
            },
        },
    ]
}
fn bank(
    rows: &[[usize; 2]],
    control_blocks: &[usize],
    ttis: [Rel99Tti; 2],
    bindings: Vec<TransportBinding>,
    route: SignallingRoute,
) -> DedicatedBearers {
    bank_mode(rows, control_blocks, ttis, bindings, route, false)
}
fn bank_mode(
    rows: &[[usize; 2]],
    control_blocks: &[usize],
    ttis: [Rel99Tti; 2],
    bindings: Vec<TransportBinding>,
    route: SignallingRoute,
    voice: bool,
) -> DedicatedBearers {
    let definitions = bindings
        .iter()
        .enumerate()
        .map(|(index, binding)| {
            let header = if matches!(binding.channels, TransportChannels::Multiplexed(_)) {
                C_T_BITS
            } else {
                0
            };
            let blocks = if index == 0 {
                &[0, 1][..]
            } else {
                control_blocks
            };
            TransportDefinition {
                id: binding.transport,
                interval: ttis[index],
                attribute: NonZeroU16::new(8).unwrap(),
                formats: blocks
                    .iter()
                    .map(|blocks| CodedBlockFormat {
                        block_bits: PDU_BYTES * BITS_PER_BYTE + header,
                        blocks: *blocks,
                        crc: TransportCrc::Crc16,
                        coding: ChannelCoding::Convolutional(CodeRate::Half),
                    })
                    .collect(),
            }
        })
        .collect();
    let formats = Arc::new(
        ConfiguredTfcs::new(
            definitions,
            rows.iter()
                .enumerate()
                .map(|(index, row)| CombinationDefinition {
                    tfci: Tfci::new(index as u16).unwrap(),
                    formats: row.to_vec(),
                })
                .collect(),
            n(100_000),
        )
        .unwrap(),
    );
    let configuration =
        SignallingProfile.network_am_configuration(n(PDU_BYTES), n(PDU_BYTES), 0, 0);
    let mut user_limits = AmLimits {
        maximum_sdu_bytes: n(128),
        queued_sdus: n(4),
        maximum_buffer_bytes: 1_000_000,
    };
    user_limits.maximum_buffer_bytes = configuration.buffer_bytes(user_limits).unwrap();
    let user = AmEntity::new(configuration, user_limits, anchor().time).unwrap();
    let user = if voice {
        BearerEndpoint::transparent(
            identity().ue,
            id(5),
            Scope::User(CnDomain::Cs),
            crate::dedicated_bearers::TransparentSettings {
                uplink_segmentation: Segmentation::Disabled,
                downlink_segmentation: Segmentation::Disabled,
                errors: ErrorDelivery::Indicate,
                limits: TmLimits {
                    maximum_pdus: n(1),
                    maximum_pdu_bits: n(PDU_BYTES * BITS_PER_BYTE),
                    maximum_sdu_bits: n(PDU_BYTES * BITS_PER_BYTE),
                },
            },
        )
        .unwrap()
    } else {
        BearerEndpoint::acknowledged(identity().ue, id(5), Scope::User(CnDomain::Ps), user).unwrap()
    };
    let endpoints = vec![
        BearerEndpoint::signalling(identity().ue, id(2), route, 128).unwrap(),
        user,
    ];
    let mut bank = DedicatedBearers::new(
        identity(),
        anchor(),
        0,
        formats.clone(),
        bindings.clone(),
        endpoints,
        BearerLimits {
            maximum_transport_bits: n(4096),
            output_slots: n(4),
            maximum_sdu_bytes: n(128),
            maximum_buffer_bytes: 2_000_000,
        },
    )
    .unwrap();
    bank.configure_downlink(anchor(), 0, formats, bindings)
        .unwrap();
    bank
}
fn route() -> SignallingRoute {
    SignallingRoute {
        mode: CipherMode::Acknowledged,
        receive_bytes: PDU_BYTES,
        transmit_minimum: PDU_BYTES,
        transmit_maximum: PDU_BYTES,
    }
}

#[test]
fn packet_grants_can_idle_only_after_the_longest_tti_boundary() {
    let mut bank = bank(
        &[[0, 0], [0, 1], [0, 2], [1, 0], [1, 1]],
        &[0, 1, 3],
        [Rel99Tti::Ms80, Rel99Tti::Ms10],
        bindings(true),
        route(),
    );
    let before = bank.storage_bytes();
    let expected = ControlAmServiceFloor {
        period_frames: n(8),
        pdus_per_period: n(3),
        pdu_bytes: n(PDU_BYTES),
    };
    assert_eq!(
        bank.control_am_service_floor(identity(), n(5)),
        Ok(expected)
    );
    assert_eq!(bank.storage_bytes(), before);
    assert_eq!(bank.phase(), BearerPhase::Prepared);
    bank.activate(identity()).unwrap();
    assert_eq!(
        bank.control_am_service_floor(identity(), n(5)),
        Ok(expected)
    );
}

#[test]
fn fixed_pdu_service_counts_blocks_not_header_or_crc_bits() {
    for multiplexed in [false, true] {
        let bank = bank(
            &[[0, 0], [0, 1], [1, 0], [1, 1]],
            &[0, 2],
            [Rel99Tti::Ms20, Rel99Tti::Ms40],
            bindings(multiplexed),
            route(),
        );
        assert_eq!(
            bank.control_am_service_floor(identity(), n(4)),
            Ok(ControlAmServiceFloor {
                period_frames: n(4),
                pdus_per_period: n(2),
                pdu_bytes: n(PDU_BYTES)
            })
        );
    }
}

#[test]
fn packet_only_peak_without_control_does_not_block_the_aligned_control_grant() {
    let bank = bank(
        &[[0, 0], [0, 1], [1, 0]],
        &[0, 1],
        [Rel99Tti::Ms20, Rel99Tti::Ms40],
        bindings(true),
        route(),
    );
    assert_eq!(
        bank.control_am_service_floor(identity(), n(3)),
        Ok(ControlAmServiceFloor {
            period_frames: n(4),
            pdus_per_period: n(1),
            pdu_bytes: n(PDU_BYTES)
        })
    );
}

#[test]
fn transparent_modes_cannot_be_idled_to_invent_control_service() {
    for supported in [false, true] {
        let mut rows = vec![[0, 0], [0, 1], [1, 0]];
        if supported {
            rows.push([1, 1]);
        }
        let bank = bank_mode(
            &rows,
            &[0, 1],
            [Rel99Tti::Ms20, Rel99Tti::Ms40],
            bindings(true),
            route(),
            true,
        );
        let result = bank.control_am_service_floor(identity(), n(rows.len()));
        if supported {
            assert_eq!(
                result,
                Ok(ControlAmServiceFloor {
                    period_frames: n(4),
                    pdus_per_period: n(1),
                    pdu_bytes: n(PDU_BYTES)
                })
            );
        } else {
            assert_eq!(result, Err(BearerError::UnsupportedBinding));
        }
    }
}

#[test]
fn identity_budget_and_retired_phase_cannot_claim_service() {
    let mut bank = bank(
        &[[0, 0], [0, 1], [1, 0], [1, 1]],
        &[0, 1],
        [Rel99Tti::Ms20, Rel99Tti::Ms40],
        bindings(true),
        route(),
    );
    let mut foreign = identity();
    foreign.ue.generation = NonZeroU64::new(2).unwrap();
    assert_eq!(
        bank.control_am_service_floor(foreign, n(4)),
        Err(BearerError::Owner)
    );
    assert_eq!(
        bank.control_am_service_floor(identity(), n(3)),
        Err(BearerError::Capacity)
    );
    bank.retire();
    assert_eq!(
        bank.control_am_service_floor(identity(), n(4)),
        Err(BearerError::Phase)
    );
}

#[test]
fn missing_and_multiple_control_transport_mappings_are_rejected() {
    for multiple in [false, true] {
        let mut mappings = bindings(false);
        mappings[0].channels = TransportChannels::Single(id(if multiple { 2 } else { 5 }));
        if !multiple {
            mappings[1].channels = TransportChannels::Single(id(5));
        }
        let bank = bank(
            &[[0, 0], [0, 1], [1, 0], [1, 1]],
            &[0, 1],
            [Rel99Tti::Ms20, Rel99Tti::Ms40],
            mappings,
            route(),
        );
        assert_eq!(
            bank.control_am_service_floor(identity(), n(4)),
            Err(if multiple {
                BearerError::UnsupportedBinding
            } else {
                BearerError::Mapping
            })
        );
    }
}

#[test]
fn only_fixed_size_acknowledged_control_am_has_a_floor() {
    for configuration in [
        SignallingRoute {
            transmit_maximum: PDU_BYTES + 1,
            ..route()
        },
        SignallingRoute {
            mode: CipherMode::Unacknowledged,
            ..route()
        },
    ] {
        let bank = bank(
            &[[0, 0], [0, 1], [1, 0], [1, 1]],
            &[0, 1],
            [Rel99Tti::Ms20, Rel99Tti::Ms40],
            bindings(true),
            configuration,
        );
        assert_eq!(
            bank.control_am_service_floor(identity(), n(4)),
            Err(BearerError::UnsupportedBinding)
        );
    }
}
