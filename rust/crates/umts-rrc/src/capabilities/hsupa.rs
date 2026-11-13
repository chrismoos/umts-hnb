// SPDX-License-Identifier: AGPL-3.0-or-later

use rasn::{Decode, types::BitStr};
use umts_rrc_schema::{information_elements as ie, pdu_definitions as pdu};
use umts_types::hsupa::Rel6Category;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FddHsupa {
    NotReported,
    Unsupported,
    Supported(Rel6Category),
    UnrecognizedCategory(u8),
    MalformedContainer,
}

// TS 25.306 4.12 Table 4.12-1 replaces ordinary DCH limits when E-DCH coexists.
pub fn simultaneous_dch_limits(mut limits: super::rel99::Rel99Limits) -> super::rel99::Rel99Limits {
    limits.uplink = super::rel99::DirectionLimits {
        transport_bits: 3840,
        convolutional_bits: 640,
        turbo_bits: Some(3840),
        transport_blocks: 8,
        transport_channels: 8,
        combinations: 32,
        formats: 32,
    };
    limits.uplink_physical_bits = 2400;
    limits
}

pub fn from_information(information: &pdu::UECapabilityInformation) -> FddHsupa {
    container(information).map_or(FddHsupa::NotReported, decode_container)
}

fn decode_container(bits: &BitStr) -> FddHsupa {
    let mut decoder =
        rasn::uper::de::Decoder::<0, 0>::new(bits, rasn::uper::de::DecoderOptions::unaligned());
    let Ok(value) = ie::UECapabilityContainerIEs::decode(&mut decoder) else {
        return FddHsupa::MalformedContainer;
    };
    // TS 25.331 10.1.1 ignores unknown non-critical extensions beyond the known prefix.
    if value.v6b0_non_critical_extensions.is_none()
        && (decoder.input().len() >= u8::BITS as usize || decoder.input().any())
    {
        return FddHsupa::MalformedContainer;
    }
    match value
        .ue_radio_access_capability_v690ext
        .physicalchannelcapability_edch
        .fdd_edch
    {
        ie::PhysicalChannelCapabilityEdchR6FddEdch::unsupported(()) => FddHsupa::Unsupported,
        ie::PhysicalChannelCapabilityEdchR6FddEdch::supported(value) => {
            let category = value.edch_physical_layer_category;
            Rel6Category::new(category).map_or(
                FddHsupa::UnrecognizedCategory(category),
                FddHsupa::Supported,
            )
        }
    }
}

pub(super) fn container(information: &pdu::UECapabilityInformation) -> Option<&BitStr> {
    let release = information
        .v370_non_critical_extensions
        .as_ref()?
        .v380_non_critical_extensions
        .as_ref()?
        .v3a0_non_critical_extensions
        .as_ref()?
        .later_non_critical_extensions
        .as_ref()?
        .v4b0_non_critical_extensions
        .as_ref()?
        .v590_non_critical_extensions
        .as_ref()?
        .v5c0_non_critical_extensions
        .as_ref()?
        .v690_non_critical_extensions
        .as_ref()?;
    release
        .ue_capability_information_v690ext
        .ue_capability_container
        .as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_six_with_independently_decoded_later_noncritical_extensions() {
        let bytes = [
            69, 249, 34, 205, 100, 128, 235, 247, 0, 224, 48, 161, 196, 141, 46, 101, 11, 22, 110,
            93, 208, 208,
        ];
        assert_eq!(
            decode_container(BitStr::from_slice(&bytes)),
            FddHsupa::Supported(Rel6Category::new(6).unwrap())
        );
    }

    #[test]
    fn edch_container_preserves_supported_unsupported_and_unknown_categories() {
        for category in 1..=16 {
            let value = ie::UECapabilityContainerIEs::new(
                ie::UERadioAccessCapabilityV690ext::new(
                    ie::PhysicalChannelCapabilityEdchR6::new(
                        ie::PhysicalChannelCapabilityEdchR6FddEdch::supported(
                            ie::PhysicalChannelCapabilityEdchR6FddEdchSupported::new(category),
                        ),
                    ),
                    None,
                ),
                None,
                None,
            );
            let bytes = rasn::uper::encode(&value).unwrap();
            let expected = Rel6Category::new(category).map_or(
                FddHsupa::UnrecognizedCategory(category),
                FddHsupa::Supported,
            );
            assert_eq!(decode_container(BitStr::from_slice(&bytes)), expected);
        }
        let value = ie::UECapabilityContainerIEs::new(
            ie::UERadioAccessCapabilityV690ext::new(
                ie::PhysicalChannelCapabilityEdchR6::new(
                    ie::PhysicalChannelCapabilityEdchR6FddEdch::unsupported(()),
                ),
                None,
            ),
            None,
            None,
        );
        let bytes = rasn::uper::encode(&value).unwrap();
        assert_eq!(
            decode_container(BitStr::from_slice(&bytes)),
            FddHsupa::Unsupported
        );
        assert_eq!(
            decode_container(BitStr::from_slice(&[])),
            FddHsupa::MalformedContainer
        );
        let mut trailing = bytes;
        trailing.push(0);
        assert_eq!(
            decode_container(BitStr::from_slice(&trailing)),
            FddHsupa::MalformedContainer
        );
    }
}
