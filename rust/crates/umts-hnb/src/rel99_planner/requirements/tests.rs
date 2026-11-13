// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use rasn::{AsnType, Encode, types::FixedBitString};
use umts_iuh::{
    ranap::rab::{Address, Assignment, Association, First, Mode, Second, Transport, UserPlane},
    wire::{InformationElement, Limits},
};

const VOICE_RATE: u32 = 12_200;
const PACKET_RATE: u32 = 384_000;
const UNKNOWN_EXTENSION: u16 = 50_000;
const PARAMETERS_IE: u16 = 149;

fn voice(bitrate: Option<u32>) -> qos::Parameters {
    let flows = [vec![0, 39, 81], vec![0, 0, 103]]
        .into_iter()
        .map(|lengths| {
            qos::Subflow::new(
                qos::ResidualError::new(1, 6).unwrap(),
                qos::Delivery::Yes,
                Some(qos::SduError::new(1, 3).unwrap()),
                Some(
                    qos::Formats::new(
                        lengths
                            .into_iter()
                            .map(|size| qos::Format::new(Some(size), bitrate).unwrap())
                            .collect(),
                    )
                    .unwrap(),
                ),
            )
            .unwrap()
        })
        .collect();
    let mut parameters = qos::Parameters::new(
        qos::TrafficClass::Conversational,
        qos::Asymmetry::SymmetricBidirectional,
        qos::MaximumRates::new(&[VOICE_RATE]).unwrap(),
        qos::DeliveryOrder::Requested,
        qos::MaxSduBits::new(184).unwrap(),
        qos::Subflows::new(flows).unwrap(),
    );
    parameters.guaranteed = Some(qos::GuaranteedRates::new(&[VOICE_RATE]).unwrap());
    parameters.transfer_delay = Some(80);
    parameters.source_statistics = Some(qos::SourceStatistics::Speech);
    parameters
}

fn packet() -> qos::Parameters {
    qos::Parameters::new(
        qos::TrafficClass::Background,
        qos::Asymmetry::AsymmetricBidirectional,
        qos::MaximumRates::new(&[PACKET_RATE, PACKET_RATE / 3]).unwrap(),
        qos::DeliveryOrder::Requested,
        qos::MaxSduBits::new(12_000).unwrap(),
        qos::Subflows::new(vec![
            qos::Subflow::new(
                qos::ResidualError::new(1, 6).unwrap(),
                qos::Delivery::No,
                Some(qos::SduError::new(1, 3).unwrap()),
                None,
            )
            .unwrap(),
        ])
        .unwrap(),
    )
}

fn request(domain: CnDomain, parameters: qos::Parameters, mode: Mode) -> SetupOrModify {
    let domain = match domain {
        CnDomain::Cs => Domain::Cs,
        CnDomain::Ps => Domain::Ps,
    };
    let mut first = First::modification(7);
    first.parameters = Some(parameters);
    first.user_plane = Some(UserPlane::new(mode, 3).unwrap());
    first.transport = Some(Transport::new(
        Address::from_ip(domain, "127.0.0.2".parse().unwrap()).unwrap(),
        if domain == Domain::Cs {
            Association::binding([0x0f, 0xa0, 0, 0])
        } else {
            Association::gtp(0)
        },
    ));
    SetupOrModify {
        first,
        second: Second::default(),
    }
}

fn through_codec(request: SetupOrModify) -> SetupOrModify {
    let encoded = Assignment {
        setup: vec![request],
        release: vec![],
    }
    .encode(Limits::default())
    .unwrap();
    Assignment::decode(&encoded, Limits::default())
        .unwrap()
        .assignment
        .setup
        .remove(0)
}

#[derive(AsnType, Encode)]
#[rasn(delegate, size("1..=65535"))]
struct ExtensionList(Vec<InformationElement>);

#[derive(AsnType, Encode)]
#[rasn(automatic_tags)]
#[non_exhaustive]
struct ExtendedParameters {
    traffic_class: qos::TrafficClass,
    asymmetry: qos::Asymmetry,
    maximum: qos::MaximumRates,
    guaranteed: Option<qos::GuaranteedRates>,
    delivery_order: qos::DeliveryOrder,
    maximum_sdu_bits: qos::MaxSduBits,
    subflows: qos::Subflows,
    #[rasn(value("0..=65535"))]
    transfer_delay: Option<u16>,
    traffic_priority: Option<qos::Priority>,
    allocation: Option<qos::Allocation>,
    source_statistics: Option<qos::SourceStatistics>,
    relocation: Option<qos::Relocation>,
    extensions: Option<ExtensionList>,
}

fn extended(value: qos::Parameters, extensions: Vec<InformationElement>) -> qos::Parameters {
    InformationElement::new(
        PARAMETERS_IE,
        Criticality::Reject,
        &ExtendedParameters {
            traffic_class: value.traffic_class,
            asymmetry: value.asymmetry,
            maximum: value.maximum,
            guaranteed: value.guaranteed,
            delivery_order: value.delivery_order,
            maximum_sdu_bits: value.maximum_sdu_bits,
            subflows: value.subflows,
            transfer_delay: value.transfer_delay,
            traffic_priority: value.traffic_priority,
            allocation: value.allocation,
            source_statistics: value.source_statistics,
            relocation: value.relocation,
            extensions: Some(ExtensionList(extensions)),
        },
    )
    .unwrap()
    .decode()
    .unwrap()
}

#[test]
fn actual_cs_ps_and_ps_predefined_requests_borrow_canonical_values() {
    for (domain, parameters, mode) in [
        (CnDomain::Cs, voice(None), Mode::PredefinedSdu),
        (CnDomain::Ps, packet(), Mode::Transparent),
        (CnDomain::Ps, voice(None), Mode::PredefinedSdu),
    ] {
        let request = through_codec(request(domain, parameters, mode));
        let normalized = Requirements::new(domain, &request).unwrap();
        assert_eq!(normalized.domain, domain);
        assert_eq!(normalized.rab, 7);
        assert_eq!(normalized.nas, None);
        assert!(std::ptr::eq(
            normalized.parameters,
            request.first.parameters.as_ref().unwrap()
        ));
        assert!(std::ptr::eq(
            normalized.user_plane,
            request.first.user_plane.as_ref().unwrap()
        ));
        assert_eq!(normalized.user_plane.mode, mode);
        assert_eq!(normalized.user_plane.version_mask(), 3);
    }
}

#[test]
fn all_four_nas_bits_are_read_in_significance_order() {
    for value in 0u8..16 {
        let mut request = request(CnDomain::Ps, packet(), Mode::Transparent);
        let mut bits = FixedBitString::<4>::default();
        for index in 0..4 {
            bits.set(index, value & (1 << (3 - index)) != 0);
        }
        request.first.nas_synchronization = Some(bits);
        let request = through_codec(request);
        assert_eq!(
            Requirements::new(CnDomain::Ps, &request).unwrap().nas,
            Some(value)
        );
    }
}

#[test]
fn directional_rates_and_maximum_sdu_do_not_become_pdu_sizes_or_guessed_intervals() {
    let asymmetric = request(CnDomain::Ps, packet(), Mode::Transparent);
    let normalized = Requirements::new(CnDomain::Ps, &asymmetric).unwrap();
    assert_eq!(
        normalized.maximum,
        qos::Rates::Asymmetric {
            downlink: u64::from(PACKET_RATE),
            uplink: u64::from(PACKET_RATE / 3)
        }
    );
    assert_eq!(normalized.guaranteed, None);
    assert_eq!(normalized.combinations[0].subflow_bits.as_ref(), &[12_000]);
    assert_eq!(normalized.combinations[0].bitrate, None);
    for (asymmetry, expected) in [
        (
            qos::Asymmetry::Downlink,
            qos::Rates::Downlink(u64::from(PACKET_RATE)),
        ),
        (
            qos::Asymmetry::Uplink,
            qos::Rates::Uplink(u64::from(PACKET_RATE)),
        ),
    ] {
        let mut parameters = packet();
        parameters.asymmetry = asymmetry;
        parameters.maximum = qos::MaximumRates::new(&[PACKET_RATE]).unwrap();
        let request = request(CnDomain::Ps, parameters, Mode::Transparent);
        assert_eq!(
            Requirements::new(CnDomain::Ps, &request).unwrap().maximum,
            expected
        );
    }
}

#[test]
fn indexed_subflow_rows_preserve_zero_lengths_and_absent_versus_zero_bitrate() {
    for bitrate in [None, Some(0)] {
        let request = through_codec(request(CnDomain::Cs, voice(bitrate), Mode::PredefinedSdu));
        let normalized = Requirements::new(CnDomain::Cs, &request).unwrap();
        assert_eq!(
            normalized.maximum,
            qos::Rates::Symmetric {
                both: u64::from(VOICE_RATE)
            }
        );
        assert_eq!(normalized.guaranteed, Some(normalized.maximum));
        assert_eq!(normalized.combinations.len(), 3);
        for (row, bits) in normalized
            .combinations
            .iter()
            .zip([[0, 0], [39, 0], [81, 103]])
        {
            assert_eq!(row.subflow_bits.as_ref(), bits);
            assert_eq!(row.bitrate, bitrate);
        }
    }
}

#[test]
fn new_bearer_missing_fields_and_domain_mismatches_are_not_defaulted() {
    for missing in 0..3 {
        let mut request = request(CnDomain::Ps, packet(), Mode::Transparent);
        match missing {
            0 => request.first.parameters = None,
            1 => request.first.user_plane = None,
            _ => request.first.transport = None,
        }
        assert!(matches!(
            Requirements::new(CnDomain::Ps, &request),
            Err(Error::Protocol(umts_iuh::Error::MissingIe(_)))
        ));
    }
    let wrong_domain = request(CnDomain::Ps, packet(), Mode::Transparent);
    assert!(matches!(
        Requirements::new(CnDomain::Cs, &wrong_domain),
        Err(Error::Protocol(_))
    ));
    let mut invalid = request(CnDomain::Cs, voice(None), Mode::PredefinedSdu);
    invalid.first.parameters.as_mut().unwrap().guaranteed = None;
    assert!(matches!(
        Requirements::new(CnDomain::Cs, &invalid),
        Err(Error::Protocol(_))
    ));
}

#[test]
fn supported_rate_override_takes_precedence_without_losing_extended_direction_order() {
    let mut parameters = packet();
    parameters.maximum = qos::MaximumRates::new(&[16_000_000, 16_000_000]).unwrap();
    let extended_maximum = InformationElement::new(
        qos::EXTENDED_MAXIMUM,
        Criticality::Reject,
        &qos::ExtendedRates::new(&[20_000_000, 17_000_000]).unwrap(),
    )
    .unwrap();
    let extended_parameters = extended(parameters.clone(), vec![extended_maximum.clone()]);
    let request_extended = through_codec(request(
        CnDomain::Ps,
        extended_parameters,
        Mode::Transparent,
    ));
    assert_eq!(
        Requirements::new(CnDomain::Ps, &request_extended)
            .unwrap()
            .maximum,
        qos::Rates::Asymmetric {
            downlink: 20_000_000,
            uplink: 17_000_000
        }
    );
    let supported = InformationElement::new(
        qos::SUPPORTED_MAXIMUM,
        Criticality::Reject,
        &qos::SupportedRates::new(&[900_000_000, 500_000_000]).unwrap(),
    )
    .unwrap();
    let parameters = extended(parameters, vec![extended_maximum, supported]);
    let request_supported = through_codec(request(CnDomain::Ps, parameters, Mode::Transparent));
    assert_eq!(
        Requirements::new(CnDomain::Ps, &request_supported)
            .unwrap()
            .maximum,
        qos::Rates::Asymmetric {
            downlink: 900_000_000,
            uplink: 500_000_000
        }
    );
}

#[test]
fn unknown_extensions_preserve_canonical_criticality_disposition() {
    for criticality in [
        Criticality::Ignore,
        Criticality::Notify,
        Criticality::Reject,
    ] {
        let parameters = extended(
            packet(),
            vec![InformationElement::new(UNKNOWN_EXTENSION, criticality, &false).unwrap()],
        );
        let request = request(CnDomain::Ps, parameters, Mode::Transparent);
        let normalized = Requirements::new(CnDomain::Ps, &request);
        if criticality == Criticality::Reject {
            assert!(matches!(
                normalized,
                Err(Error::Protocol(umts_iuh::Error::UnknownRejectIe(
                    UNKNOWN_EXTENSION
                )))
            ));
        } else {
            assert!(normalized.is_ok());
            let assignment = Assignment {
                setup: vec![request],
                release: vec![],
            };
            let decoded = Assignment::decode(
                &assignment.encode(Limits::default()).unwrap(),
                Limits::default(),
            )
            .unwrap();
            if criticality == Criticality::Notify {
                assert!(decoded.notify_ies.contains(&UNKNOWN_EXTENSION));
            } else {
                assert!(decoded.notify_ies.is_empty());
            }
            assert!(Requirements::new(CnDomain::Ps, &decoded.assignment.setup[0]).is_ok());
        }
    }
}
