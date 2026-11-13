// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;
use umts_rrc::{
    dedicated::*,
    transport::{ChannelCoding, ConvolutionalRate, CrcSize, TransportInterval},
};
use umts_rrc_schema::information_elements as ie;
use umts_types::CnDomain;

pub fn am() -> Rel99Rlc {
    Rel99Rlc::Acknowledged {
        uplink: ie::ULAMRLCMode::new(
            ie::TransmissionRLCDiscard::noDiscard(ie::MaxDAT::dat10),
            ie::TransmissionWindowSize::tw32,
            ie::TimerRST::tr400,
            ie::MaxRST::rst4,
            Some(ie::PollingInfo::new(
                None,
                Some(ie::TimerPoll::tp200),
                Some(ie::PollPDU::pdu1),
                None,
                true,
                true,
                None,
                None,
            )),
        ),
        downlink: ie::DLAMRLCMode::new(
            true,
            ie::ReceivingWindowSize::rw32,
            ie::DLRLCStatusInfo::new(Some(ie::TimerStatusProhibit::tsp100), None, true, None),
        ),
    }
}
pub fn physical() -> NormalDpch {
    NormalDpch {
        primary_scrambling: 469,
        secondary_scrambling: None,
        uplink_scrambling: 12345,
        uplink_minimum_sf: 4,
        uplink_dpdch_count: 1,
        uplink_tfci: true,
        puncturing_percent: 100,
        dpcch_offset_db: -60,
        pc_preamble_frames: 0,
        srb_delay_frames: 0,
        power_control: UplinkPowerControl::Algorithm1 { step_db: 1 },
        maximum_uplink_dbm: 24,
        downlink_sf: 16,
        downlink_pilot_bits: 16,
        downlink_tfci: true,
        downlink_codes: vec![3],
        flexible_position: true,
        pilot_offset_quarter_db: 0,
        default_offset_512_chips: 0,
        frame_offset_256_chips: 0,
    }
}
pub fn mapping(ul: u8, dl: u8, logical: Option<u8>) -> BearerMapping {
    BearerMapping {
        uplink: vec![UplinkLogicalMapping {
            transport: ul,
            logical,
            priority: 1,
        }],
        downlink: vec![LogicalMapping {
            transport: dl,
            logical,
        }],
    }
}
pub fn components(
    voice: bool,
    packet: bool,
    dl_signal_id: u8,
) -> (DchTfcs, DchTfcs, Vec<Rel99Bearer>, NormalDpch) {
    let mut ul = Vec::new();
    let mut dl = Vec::new();
    let make = |id, interval, coding, crc, groups| DchTransport {
        id,
        interval,
        coding,
        crc,
        rate_matching_attribute: 200,
        groups,
    };
    let group = |rlc_bits, blocks| DchFormatGroup { rlc_bits, blocks };
    let third = ChannelCoding::Convolutional(ConvolutionalRate::Third);
    if voice {
        // TS 34.108 6.10.2.4.1.4a supplies AMR A/B/C subflows and the four speech/SID sizes.
        for (offset, (coding, crc, attribute, groups)) in [
            (
                third,
                CrcSize::Bits12,
                200,
                vec![
                    group(0, vec![1]),
                    group(39, vec![1]),
                    group(42, vec![1]),
                    group(55, vec![1]),
                    group(75, vec![1]),
                    group(81, vec![1]),
                ],
            ),
            (
                third,
                CrcSize::None,
                190,
                vec![
                    group(103, vec![0, 1]),
                    group(53, vec![1]),
                    group(63, vec![1]),
                    group(84, vec![1]),
                ],
            ),
            (
                ChannelCoding::Convolutional(ConvolutionalRate::Half),
                CrcSize::None,
                235,
                vec![group(60, vec![0, 1])],
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let mut uplink = make(
                offset as u8 + 1,
                TransportInterval::Ms20,
                coding,
                crc,
                groups.clone(),
            );
            uplink.rate_matching_attribute = attribute;
            let downlink = DchTransport {
                id: offset as u8 + 6,
                ..uplink.clone()
            };
            ul.push(uplink);
            dl.push(downlink);
        }
    }
    if packet {
        ul.push(make(
            4,
            TransportInterval::Ms20,
            ChannelCoding::Turbo,
            CrcSize::Bits16,
            vec![group(320, vec![0, 4])],
        ));
        dl.push(make(
            9,
            TransportInterval::Ms20,
            ChannelCoding::Turbo,
            CrcSize::Bits16,
            vec![group(320, vec![0, 4])],
        ));
    }
    ul.push(make(
        5,
        TransportInterval::Ms40,
        third,
        CrcSize::Bits16,
        vec![group(144, vec![0, 1])],
    ));
    dl.push(make(
        dl_signal_id,
        TransportInterval::Ms40,
        third,
        CrcSize::Bits16,
        vec![group(336, vec![0, 1])],
    ));
    let combinations = |direction| {
        let speech = if voice {
            vec![
                vec![0, 0, 0],
                vec![1, 0, 0],
                vec![2, 2, 0],
                vec![3, 3, 0],
                vec![4, 4, 0],
                vec![5, 1, 1],
            ]
        } else {
            vec![vec![]]
        };
        let mut rows = Vec::new();
        for signalling in 0..2 {
            for packet_format in 0..=usize::from(packet) {
                for speech in &speech {
                    let mut formats = speech.clone();
                    if packet {
                        formats.push(packet_format);
                    }
                    formats.push(signalling);
                    rows.push(Combination {
                        formats,
                        gain: (direction == DchDirection::Uplink).then_some(GainFactors {
                            control: 15,
                            data: 15,
                        }),
                    });
                }
            }
        }
        rows
    };
    let mut bearers = vec![
        Rel99Bearer {
            id: 1,
            domain: None,
            rlc: Rel99Rlc::Unacknowledged,
            mapping: mapping(5, dl_signal_id, Some(1)),
        },
        Rel99Bearer {
            id: 2,
            domain: None,
            rlc: am(),
            mapping: mapping(5, dl_signal_id, Some(2)),
        },
        Rel99Bearer {
            id: 3,
            domain: None,
            rlc: am(),
            mapping: mapping(5, dl_signal_id, Some(3)),
        },
    ];
    if voice {
        for subflow in 0..3 {
            bearers.push(Rel99Bearer {
                id: 10 + subflow,
                domain: Some(CnDomain::Cs),
                rlc: Rel99Rlc::Transparent {
                    uplink_segmentation: false,
                    downlink_segmentation: false,
                },
                mapping: mapping(1 + subflow, 6 + subflow, None),
            });
        }
    }
    if packet {
        bearers.push(Rel99Bearer {
            id: 20,
            domain: Some(CnDomain::Ps),
            rlc: am(),
            mapping: mapping(4, 9, None),
        });
    }
    (
        DchTfcs::new(DchDirection::Uplink, ul, combinations(DchDirection::Uplink)).unwrap(),
        DchTfcs::new(
            DchDirection::Downlink,
            dl,
            combinations(DchDirection::Downlink),
        )
        .unwrap(),
        bearers,
        physical(),
    )
}
pub fn configuration(voice: bool, packet: bool, dl_signal_id: u8) -> Arc<Rel99Configuration> {
    let (ul, dl, bearers, physical) = components(voice, packet, dl_signal_id);
    Arc::new(Rel99Configuration::new(ul, dl, bearers, physical).unwrap())
}
pub fn retained() -> Vec<RetainedBearer> {
    (1..=3)
        .map(|id| RetainedBearer {
            id,
            domain: None,
            rlc: if id == 1 {
                Rel99Rlc::Unacknowledged
            } else {
                am()
            },
            uplink_pdu_bits: 144,
            downlink_pdu_bits: 336,
        })
        .collect()
}
pub fn rabs(voice: bool, packet: bool) -> Vec<NewRab> {
    let mut result = Vec::new();
    if voice {
        result.push(NewRab {
            identity: 7,
            domain: CnDomain::Cs,
            nas_synchronization: Some(7),
            bearers: vec![10, 11, 12],
        });
    }
    if packet {
        result.push(NewRab {
            identity: 9,
            domain: CnDomain::Ps,
            nas_synchronization: None,
            bearers: vec![20],
        });
    }
    result
}
