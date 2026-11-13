// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::wire::{Criticality, decode_value, encode_value};

const AMR_BITS: u16 = 244;
const AMR_RATE: u32 = 12_200;
const PACKET_RATE: u32 = 384_000;
const OSMO_EXTENDED_RATE: u32 = 42_000_000;
const UNKNOWN_EXTENSION: u16 = 50_000;
const VOICE_OPTIONAL_FIELDS: u8 = 0x64;

fn voice() -> Parameters {
    let flow = Subflow::new(
        ResidualError::new(1, 6).unwrap(),
        Delivery::Yes,
        Some(SduError::new(1, 3).unwrap()),
        Some(Formats::new(vec![Format::new(Some(AMR_BITS), None).unwrap()]).unwrap()),
    )
    .unwrap();
    let mut parameters = Parameters::new(
        TrafficClass::Conversational,
        Asymmetry::SymmetricBidirectional,
        MaximumRates::new(&[AMR_RATE]).unwrap(),
        DeliveryOrder::Requested,
        MaxSduBits::new(AMR_BITS).unwrap(),
        Subflows::new(vec![flow]).unwrap(),
    );
    parameters.guaranteed = Some(GuaranteedRates::new(&[AMR_RATE]).unwrap());
    parameters.transfer_delay = Some(80);
    parameters.source_statistics = Some(SourceStatistics::Speech);
    parameters
}

fn packet() -> Parameters {
    Parameters::new(
        TrafficClass::Background,
        Asymmetry::AsymmetricBidirectional,
        MaximumRates::new(&[PACKET_RATE, PACKET_RATE / 3]).unwrap(),
        DeliveryOrder::Requested,
        MaxSduBits::new(12_000).unwrap(),
        Subflows::new(vec![
            Subflow::new(
                ResidualError::new(1, 6).unwrap(),
                Delivery::No,
                Some(SduError::new(1, 3).unwrap()),
                None,
            )
            .unwrap(),
        ])
        .unwrap(),
    )
}

fn extension<T: Encode>(id: u16, value: &T) -> InformationElement {
    InformationElement::new(id, Criticality::Reject, value).unwrap()
}

#[test]
fn spec_enumerations_and_nonextensible_error_ratios_match_literal_aper() {
    for (delivery, literal) in [
        (Delivery::Yes, [0]),
        (Delivery::No, [0x40]),
        (Delivery::NoDetection, [0x80]),
    ] {
        assert_eq!(encode_value(&delivery).unwrap(), literal);
        assert_eq!(decode_value::<Delivery>(&literal).unwrap(), delivery);
    }
    assert_eq!(
        decode_value::<TrafficClass>(&[0x40]).unwrap(),
        TrafficClass::Interactive
    );
    assert_eq!(
        decode_value::<Asymmetry>(&[0x60]).unwrap(),
        Asymmetry::AsymmetricBidirectional
    );
    assert_eq!(
        encode_value(&ResidualError::new(9, 8).unwrap()).unwrap(),
        [0x47]
    );
    assert_eq!(encode_value(&SduError::new(9, 6).unwrap()).unwrap(), [0x45]);
    assert_eq!(
        decode_value::<ResidualError>(&[0x47]).unwrap(),
        ResidualError::new(9, 8).unwrap()
    );
    assert!(decode_value::<Delivery>(&[0xc0]).is_err());
}

#[test]
fn complete_cs_and_ps_root_fields_roundtrip_without_changing_direction_order() {
    for parameters in [voice(), packet()] {
        parameters.validate().unwrap();
        let encoded = encode_value(&parameters).unwrap();
        assert_eq!(
            encoded[0],
            if parameters.traffic_class == TrafficClass::Conversational {
                VOICE_OPTIONAL_FIELDS
            } else {
                0
            }
        );
        let decoded = decode_value::<Parameters>(&encoded).unwrap();
        assert_eq!(decoded, parameters);
        decoded.validate().unwrap();
        for length in 0..encoded.len() {
            assert!(decode_value::<Parameters>(&encoded[..length]).is_err());
        }
    }
    let rates = packet().effective_maximum().unwrap();
    assert_eq!(rates.downlink(), Some(u64::from(PACKET_RATE)));
    assert_eq!(rates.uplink(), Some(u64::from(PACKET_RATE / 3)));
    assert_eq!(
        voice().combination(0).unwrap().subflow_bits.as_ref(),
        &[AMR_BITS]
    );
}

#[test]
fn traffic_class_conditions_are_not_filled_with_defaults() {
    let mut missing = voice();
    missing.guaranteed = None;
    assert_eq!(missing.validate(), Err(Error::Value(PARAMETERS)));
    let mut missing = voice();
    missing.transfer_delay = None;
    assert_eq!(missing.validate(), Err(Error::Value(PARAMETERS)));
    let mut missing = voice();
    missing.source_statistics = None;
    assert_eq!(missing.validate(), Err(Error::Value(PARAMETERS)));
    let mut interactive = packet();
    interactive.traffic_class = TrafficClass::Interactive;
    assert_eq!(interactive.validate(), Err(Error::Value(PARAMETERS)));
    interactive.traffic_priority = Some(Priority::new(1).unwrap());
    interactive.validate().unwrap();
    interactive.traffic_priority = Some(Priority(0));
    assert_eq!(interactive.validate(), Err(Error::Value(PARAMETERS)));
    let mut background = packet();
    background.transfer_delay = Some(80);
    assert_eq!(background.validate(), Err(Error::Value(PARAMETERS)));
}

#[test]
fn asymmetry_requires_the_effective_number_of_directional_values() {
    let mut parameters = packet();
    parameters.maximum = MaximumRates::new(&[PACKET_RATE]).unwrap();
    assert_eq!(parameters.validate(), Err(Error::Value(PARAMETERS)));
    parameters.asymmetry = Asymmetry::Downlink;
    parameters.validate().unwrap();
    assert_eq!(parameters.effective_maximum().unwrap().uplink(), None);
    parameters.asymmetry = Asymmetry::Uplink;
    parameters.validate().unwrap();
    assert_eq!(parameters.effective_maximum().unwrap().downlink(), None);
    parameters.maximum = MaximumRates::new(&[PACKET_RATE, PACKET_RATE]).unwrap();
    assert_eq!(parameters.validate(), Err(Error::Value(PARAMETERS)));
}

#[test]
fn supported_rates_override_extended_and_root_rates_without_silent_fallback() {
    let mut parameters = packet();
    parameters.extensions = Some(ExtensionList(vec![
        extension(
            EXTENDED_MAXIMUM,
            &ExtendedRates::new(&[20_000_000, 17_000_000]).unwrap(),
        ),
        extension(
            SUPPORTED_MAXIMUM,
            &SupportedRates::new(&[900_000_000, 500_000_000]).unwrap(),
        ),
    ]));
    parameters.validate().unwrap();
    assert_eq!(
        parameters.effective_maximum().unwrap(),
        Rates::Asymmetric {
            downlink: 900_000_000,
            uplink: 500_000_000
        }
    );
    parameters.extensions.as_mut().unwrap().0.pop();
    assert_eq!(parameters.validate(), Err(Error::Value(EXTENDED_MAXIMUM)));
    parameters.maximum = MaximumRates::new(&[ROOT_RATE_MAX, ROOT_RATE_MAX]).unwrap();
    parameters.validate().unwrap();
    assert_eq!(
        parameters.effective_maximum().unwrap().downlink(),
        Some(20_000_000)
    );
    parameters.extensions = Some(ExtensionList(vec![extension(SUPPORTED_MAXIMUM, &false)]));
    assert_eq!(
        parameters.effective_maximum(),
        Err(Error::Value(SUPPORTED_MAXIMUM))
    );
}

#[test]
fn erroneous_ignore_rate_extensions_fall_back_to_root_rates() {
    let root = Rates::Asymmetric {
        downlink: u64::from(PACKET_RATE),
        uplink: u64::from(PACKET_RATE / 3),
    };
    let mut parameters = packet();
    let single_extended = ExtendedRates::new(&[OSMO_EXTENDED_RATE]).unwrap();
    parameters.extensions = Some(ExtensionList(vec![
        InformationElement::new(EXTENDED_MAXIMUM, Criticality::Ignore, &single_extended).unwrap(),
    ]));
    parameters.validate().unwrap();
    assert_eq!(parameters.effective_maximum(), Ok(root));
    assert_eq!(
        parameters.ignored_rate_extensions().collect::<Vec<_>>(),
        [(EXTENDED_MAXIMUM, Error::Value(EXTENDED_MAXIMUM))]
    );

    parameters.extensions = Some(ExtensionList(vec![
        InformationElement::new(SUPPORTED_MAXIMUM, Criticality::Ignore, &false).unwrap(),
    ]));
    parameters.validate().unwrap();
    assert_eq!(parameters.effective_maximum(), Ok(root));

    for criticality in [Criticality::Reject, Criticality::Notify] {
        parameters.extensions = Some(ExtensionList(vec![
            InformationElement::new(EXTENDED_MAXIMUM, criticality, &single_extended).unwrap(),
        ]));
        assert_eq!(parameters.validate(), Err(Error::Value(EXTENDED_MAXIMUM)));
        assert_eq!(
            parameters.effective_maximum(),
            Err(Error::Value(EXTENDED_MAXIMUM))
        );
        assert_eq!(parameters.ignored_rate_extensions().count(), 0);
    }
}

#[test]
fn guaranteed_rate_overrides_preserve_direction_and_cannot_exceed_maximum() {
    let mut parameters = voice();
    parameters.maximum = MaximumRates::new(&[ROOT_RATE_MAX]).unwrap();
    parameters.guaranteed = Some(GuaranteedRates::new(&[ROOT_RATE_MAX]).unwrap());
    parameters.extensions = Some(ExtensionList(vec![
        extension(
            EXTENDED_MAXIMUM,
            &ExtendedRates::new(&[32_000_000]).unwrap(),
        ),
        extension(
            EXTENDED_GUARANTEED,
            &ExtendedRates::new(&[20_000_000]).unwrap(),
        ),
    ]));
    parameters.validate().unwrap();
    assert_eq!(
        parameters
            .effective_guaranteed()
            .unwrap()
            .unwrap()
            .downlink(),
        Some(20_000_000)
    );
    parameters.extensions.as_mut().unwrap().0.push(extension(
        SUPPORTED_GUARANTEED,
        &SupportedRates::new(&[40_000_000]).unwrap(),
    ));
    assert_eq!(parameters.validate(), Err(Error::Value(PARAMETERS)));
}

#[test]
fn subflow_combinations_are_zipped_by_index_instead_of_cross_product() {
    let mut parameters = voice();
    let flow = |sizes: &[u16]| {
        Subflow::new(
            ResidualError::new(1, 6).unwrap(),
            Delivery::Yes,
            Some(SduError::new(1, 3).unwrap()),
            Some(
                Formats::new(
                    sizes
                        .iter()
                        .map(|size| Format::new(Some(*size), None).unwrap())
                        .collect(),
                )
                .unwrap(),
            ),
        )
        .unwrap()
    };
    parameters.subflows =
        Subflows::new(vec![flow(&[81, 39]), flow(&[103, 0]), flow(&[60, 0])]).unwrap();
    parameters.validate().unwrap();
    assert_eq!(parameters.combination_count(), 2);
    assert_eq!(
        parameters.combination(0).unwrap().subflow_bits.as_ref(),
        &[81, 103, 60]
    );
    assert_eq!(
        parameters.combination(1).unwrap().subflow_bits.as_ref(),
        &[39, 0, 0]
    );
    parameters.subflows.0[1] = flow(&[103]);
    assert_eq!(parameters.validate(), Err(Error::Value(PARAMETERS)));
}

#[test]
fn erroneous_delivery_requires_error_ratio_and_preserves_all_three_policies() {
    for delivery in [Delivery::Yes, Delivery::No, Delivery::NoDetection] {
        let error = (delivery != Delivery::NoDetection).then(|| SduError::new(1, 3).unwrap());
        let flow = Subflow::new(ResidualError::new(1, 6).unwrap(), delivery, error, None).unwrap();
        assert_eq!(
            decode_value::<Subflow>(&encode_value(&flow).unwrap()).unwrap(),
            flow
        );
        let mut invalid = flow;
        invalid.sdu_error = if delivery == Delivery::NoDetection {
            Some(SduError::new(1, 3).unwrap())
        } else {
            None
        };
        assert_eq!(invalid.validate(), Err(Error::Value(PARAMETERS)));
    }
}

#[test]
fn format_omissions_have_only_the_specified_single_subflow_fallback() {
    let mut parameters = voice();
    parameters.subflows.0[0].formats =
        Some(Formats::new(vec![Format::new(None, Some(AMR_RATE)).unwrap()]).unwrap());
    parameters.validate().unwrap();
    assert_eq!(
        parameters.combination(0).unwrap(),
        Combination {
            subflow_bits: vec![AMR_BITS].into_boxed_slice(),
            bitrate: Some(AMR_RATE)
        }
    );
    parameters.subflows.0.push(parameters.subflows.0[0].clone());
    assert_eq!(parameters.combination(0), Err(Error::Unsupported));
    assert_eq!(parameters.validate(), Err(Error::Unsupported));
    assert!(Format::new(None, None).is_err());
}

#[test]
fn unknown_nested_extensions_remain_visible_and_duplicate_ids_are_rejected() {
    let mut parameters = packet();
    let unknown = InformationElement::new(UNKNOWN_EXTENSION, Criticality::Notify, &false).unwrap();
    parameters.extensions = Some(ExtensionList(vec![unknown.clone()]));
    parameters.subflows.0[0].extensions = Some(ExtensionList(vec![unknown.clone()]));
    parameters.validate().unwrap();
    assert_eq!(parameters.unknown_extension_ies().count(), 2);
    let decoded = decode_value::<Parameters>(&encode_value(&parameters).unwrap()).unwrap();
    assert_eq!(decoded.extension_ies().count(), 2);
    parameters.extensions.as_mut().unwrap().0.push(unknown);
    assert_eq!(
        parameters.validate(),
        Err(Error::DuplicateIe(UNKNOWN_EXTENSION))
    );
}

#[test]
fn signalling_indication_is_only_valid_on_interactive_traffic() {
    let mut parameters = packet();
    parameters.extensions = Some(ExtensionList(vec![
        InformationElement::new(
            SIGNALLING_INDICATION,
            Criticality::Ignore,
            &SignallingIndication::Signalling,
        )
        .unwrap(),
    ]));
    assert_eq!(parameters.validate(), Err(Error::Value(PARAMETERS)));
    parameters.traffic_class = TrafficClass::Interactive;
    parameters.traffic_priority = Some(Priority::new(1).unwrap());
    parameters.validate().unwrap();
    assert_eq!(parameters.signalling(), Ok(true));
}

#[test]
fn bounds_and_spare_priorities_do_not_become_valid_semantic_values() {
    assert!(MaximumRates::new(&[]).is_err());
    assert!(MaximumRates::new(&[1, 1, 1]).is_err());
    assert!(MaximumRate::new(0).is_err());
    assert!(ExtendedRate::new(ROOT_RATE_MAX).is_err());
    assert!(SupportedRate::new(0).is_err());
    assert!(SubflowBits::new(4096).is_err());
    assert!(MaxSduBits::new(32769).is_err());
    assert!(ResidualError::new(0, 1).is_err());
    assert!(SduError::new(1, 7).is_err());
    assert!(Priority::new(0).is_err());
    assert!(Subflows::new(vec![]).is_err());
    let mut parameters = voice();
    parameters.maximum_sdu_bits = MaxSduBits::new(AMR_BITS - 1).unwrap();
    assert_eq!(parameters.validate(), Err(Error::Value(PARAMETERS)));
}
