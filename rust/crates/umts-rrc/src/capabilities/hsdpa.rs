// SPDX-License-Identifier: AGPL-3.0-or-later

use rasn::Decode;
use umts_rrc_schema::{information_elements as ie, pdu_definitions as pdu};
use umts_types::hsdpa::{Category, DualCellCategory};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DualCellHsdpa {
    pub category: DualCellCategory,
    pub cell_specific_transmit_diversity: bool,
}

// TS 25.331 10.3.3 compressed-mode capability: presence of this Rel-8
// flag explicitly means adjacent-frequency measurements need no gaps.
pub fn adjacent_measurements_without_gaps(information: &pdu::UECapabilityInformation) -> bool {
    super::hsupa::container(information).is_some_and(adjacent_from_container)
}

fn adjacent_from_container(bits: &rasn::types::BitStr) -> bool {
    let decode = || {
        let mut decoder =
            rasn::uper::de::Decoder::<0, 0>::new(bits, rasn::uper::de::DecoderOptions::unaligned());
        let container = ie::UECapabilityContainerIEs::decode(&mut decoder).ok()?;
        let release = container
            .v6b0_non_critical_extensions?
            .v6e0_non_critical_extensions?
            .v770_non_critical_extensions?
            .v790_non_critical_extensions?
            .v860_non_critical_extensions?;
        release
            .ue_radio_access_capability_v860ext
            .measurement_capability?
            .downlink_compressed_mode
            .adjacent_frequency_measurements
    };
    decode().is_some()
}

pub fn dual_cell_from_information(
    information: &pdu::UECapabilityInformation,
) -> Option<DualCellHsdpa> {
    dual_cell_with_legacy(
        from_information(information),
        super::hsupa::container(information)?,
    )
}

fn dual_cell_with_legacy(legacy: FddHsdpa, bits: &rasn::types::BitStr) -> Option<DualCellHsdpa> {
    let FddHsdpa::Supported(legacy) = legacy else {
        return None;
    };
    let (dual, single) = dual_cell_capabilities(bits)?;
    // TS 25.331 8.1.6.2 explicitly constrains the legacy category for
    // extensions 13..20. It imposes no such pairing for extensions 9/10,
    // which describe MAC-ehs independently of the legacy MAC-hs category.
    let compatible = match single {
        13 | 15 | 17 | 19 => legacy.value() == 9,
        14 | 16 | 18 | 20 => legacy.value() == 10,
        9 | 10 => true,
        _ => false,
    };
    compatible.then_some(dual)
}

#[cfg(test)]
fn dual_cell_from_container(bits: &rasn::types::BitStr) -> Option<DualCellHsdpa> {
    dual_cell_capabilities(bits).map(|(dual, _)| dual)
}

fn dual_cell_capabilities(bits: &rasn::types::BitStr) -> Option<(DualCellHsdpa, u8)> {
    let mut decoder =
        rasn::uper::de::Decoder::<0, 0>::new(bits, rasn::uper::de::DecoderOptions::unaligned());
    let container = ie::UECapabilityContainerIEs::decode(&mut decoder).ok()?;
    let release = container
        .v6b0_non_critical_extensions?
        .v6e0_non_critical_extensions?
        .v770_non_critical_extensions?;
    release.ue_radio_access_capability_v770ext.mac_ehs_support?;
    let single = release
        .ue_radio_access_capability_v770ext
        .physical_channel_capability
        .fdd_phys_ch_capability
        .as_ref()?
        .downlink_phys_ch_capability
        .hsdsch_physical_layer_category_ext
        .as_ref()?
        .0;
    let release = release
        .v790_non_critical_extensions?
        .v860_non_critical_extensions?;
    let category = release
        .ue_radio_access_capability_v860ext
        .physical_channel_capability
        .fdd_phys_ch_capability?
        .downlink_phys_ch_capability
        .hsdsch_physical_layer_category_ext2?
        .0;
    // TS 25.331 8.1.6.2: mandatory ext1 and exact ext2/ext1 combinations.
    let compatible = match category {
        21 => matches!(single, 9 | 10 | 13 | 14 | 15 | 16 | 17 | 18),
        22 => matches!(single, 10 | 14 | 16 | 18),
        23 => matches!(single, 13 | 14 | 17 | 18 | 19 | 20),
        24 => matches!(single, 14 | 18 | 20),
        _ => false,
    };
    if !compatible {
        return None;
    }
    let cell_specific_transmit_diversity = release
        .v880_non_critical_extensions
        .and_then(|release| release.v890_non_critical_extensions)
        .is_some_and(|release| {
            release
                .ue_radio_access_capability_v890ext
                .support_cell_specific_tx_diversityin_dc_operation
                .is_some()
        });
    Some((
        DualCellHsdpa {
            category: DualCellCategory::new(category).ok()?,
            cell_specific_transmit_diversity,
        },
        single,
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FddHsdpa {
    NotReported,
    Unsupported,
    Supported(Category),
    UnrecognizedCategory(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimultaneousDch {
    Kbps32,
    Kbps64,
    Kbps128,
    Kbps384,
}

impl SimultaneousDch {
    // TS 25.306 4.11 Table 4.11-1 replaces, rather than intersects, the ordinary DL limits.
    pub fn apply(self, mut limits: super::rel99::Rel99Limits) -> super::rel99::Rel99Limits {
        let (bits, turbo, blocks, combinations, formats, codes, physical) = match self {
            Self::Kbps32 => (640, None, 8, 32, 32, 1, 1200),
            Self::Kbps64 => (3840, Some(3840), 8, 48, 64, 1, 2400),
            Self::Kbps128 => (3840, Some(3840), 16, 96, 64, 1, 4800),
            Self::Kbps384 => (6400, Some(6400), 32, 128, 64, 3, 19200),
        };
        limits.downlink = super::rel99::DirectionLimits {
            transport_bits: bits,
            convolutional_bits: 640,
            turbo_bits: turbo,
            transport_blocks: blocks,
            transport_channels: 8,
            combinations,
            formats,
        };
        limits.downlink_codes = codes;
        limits.downlink_physical_bits = physical;
        limits
    }
}

pub fn simultaneous_dch(information: &pdu::UECapabilityInformation) -> Option<SimultaneousDch> {
    use ie::DLCapabilityWithSimultaneousHSDSCHConfig as Wire;
    Some(
        match extension(information)?.dl_capability_with_simultaneous_hs_dschconfig? {
            Wire::kbps32 => SimultaneousDch::Kbps32,
            Wire::kbps64 => SimultaneousDch::Kbps64,
            Wire::kbps128 => SimultaneousDch::Kbps128,
            Wire::kbps384 => SimultaneousDch::Kbps384,
        },
    )
}

pub fn from_information(information: &pdu::UECapabilityInformation) -> FddHsdpa {
    let Some(physical) = physical(information) else {
        return FddHsdpa::NotReported;
    };
    match &physical.fdd_hspdsch {
        ie::PhysicalChannelCapabilityHspdschR5FddHspdsch::unsupported(()) => FddHsdpa::Unsupported,
        ie::PhysicalChannelCapabilityHspdschR5FddHspdsch::supported(value) => {
            let category = value.hsdsch_physical_layer_category.0;
            match Category::new(category) {
                Ok(category) if category.value() <= 12 => FddHsdpa::Supported(category),
                _ => FddHsdpa::UnrecognizedCategory(category),
            }
        }
    }
}

pub fn qam64_from_information(information: &pdu::UECapabilityInformation) -> Option<Category> {
    let bits = super::hsupa::container(information)?;
    qam64_from_container(bits)
}

fn qam64_from_container(bits: &rasn::types::BitStr) -> Option<Category> {
    let mut decoder =
        rasn::uper::de::Decoder::<0, 0>::new(bits, rasn::uper::de::DecoderOptions::unaligned());
    let container = ie::UECapabilityContainerIEs::decode(&mut decoder).ok()?;
    let release = container
        .v6b0_non_critical_extensions?
        .v6e0_non_critical_extensions?
        .v770_non_critical_extensions?
        .ue_radio_access_capability_v770ext;
    release.mac_ehs_support?;
    let category = release
        .physical_channel_capability
        .fdd_phys_ch_capability?
        .downlink_phys_ch_capability
        .hsdsch_physical_layer_category_ext?
        .0;
    single_cell_category(category)
}

fn single_cell_category(category: u8) -> Option<Category> {
    // TS 25.306 Table 5.1a preserves the larger single-cell buffers of categories 19/20.
    Category::new(match category {
        13 | 17 => 13,
        14 | 18 => 14,
        19 | 20 => category,
        _ => return None,
    })
    .ok()
}

fn physical(
    information: &pdu::UECapabilityInformation,
) -> Option<&ie::PhysicalChannelCapabilityHspdschR5> {
    Some(&extension(information)?.physical_channel_capability)
}

fn extension(
    information: &pdu::UECapabilityInformation,
) -> Option<&ie::UERadioAccessCapabilityV590ext> {
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
        .as_ref()?;
    release
        .ue_capability_information_v590ext
        .ue_radio_access_capability_v590ext
        .as_ref()
}

#[cfg(test)]
mod evolved_tests {
    use super::*;

    #[test]
    fn malformed_container_does_not_admit_dual_cell() {
        for bytes in [&[][..], &[0xff][..], &[0x00][..]] {
            assert_eq!(
                dual_cell_from_container(rasn::types::BitStr::from_slice(bytes)),
                None
            );
        }
    }

    #[test]
    fn ext2_requires_mac_ehs_but_not_cell_specific_diversity() {
        use ie::UECapabilityContainerIEsV6b0NonCriticalExtensionsV6e0NonCriticalExtensionsV770NonCriticalExtensionsV790NonCriticalExtensions as V790;
        use ie::UECapabilityContainerIEsV6b0NonCriticalExtensionsV6e0NonCriticalExtensionsV770NonCriticalExtensionsV790NonCriticalExtensionsV860NonCriticalExtensions as V860;

        let bytes = [
            69, 249, 34, 205, 100, 128, 235, 247, 0, 224, 48, 161, 196, 141, 46, 101, 11, 22, 110,
            93, 208, 208,
        ];
        for category in 21..=24 {
            let mut container: ie::UECapabilityContainerIEs = rasn::uper::decode(&bytes).unwrap();
            let release = container
                .v6b0_non_critical_extensions
                .as_mut()
                .unwrap()
                .v6e0_non_critical_extensions
                .as_mut()
                .unwrap()
                .v770_non_critical_extensions
                .as_mut()
                .unwrap();
            // All absent optional fields encode as zero in these extension sequences.
            let mut capability: ie::UERadioAccessCapabilityV860extIEs =
                rasn::uper::decode(&[0; 3]).unwrap();
            assert_eq!(rasn::uper::encode(&capability).unwrap(), [0; 3]);
            // Category support does not imply the optional no-gap measurement flag.
            capability.measurement_capability = Some(ie::MeasurementCapabilityV860ext::new(
                ie::CompressedModeMeasCapabilityV860ext::new(None),
            ));
            capability
                .physical_channel_capability
                .fdd_phys_ch_capability = Some(
                ie::PhysicalChannelCapabilityV860extFddPhysChCapability::new(
                    ie::DLPhysChCapabilityFDDV860ext::new(
                        Some(ie::HSDSCHPhysicalLayerCategoryExt2(category)),
                        None,
                        None,
                    ),
                ),
            );
            release.v790_non_critical_extensions = Some(V790::new(
                ie::UERadioAccessCapabilityV790extIEs::new(None),
                Some(V860::new(capability, None, None)),
            ));
            check_category_relations(category, &container);
            let encoded = rasn::uper::encode(&container).unwrap();
            assert!(!adjacent_from_container(rasn::types::BitStr::from_slice(
                &encoded
            )));
            let flag = &mut container
                .v6b0_non_critical_extensions
                .as_mut()
                .unwrap()
                .v6e0_non_critical_extensions
                .as_mut()
                .unwrap()
                .v770_non_critical_extensions
                .as_mut()
                .unwrap()
                .v790_non_critical_extensions
                .as_mut()
                .unwrap()
                .v860_non_critical_extensions
                .as_mut()
                .unwrap()
                .ue_radio_access_capability_v860ext
                .measurement_capability
                .as_mut()
                .unwrap()
                .downlink_compressed_mode
                .adjacent_frequency_measurements;
            *flag =
                Some(ie::CompressedModeMeasCapabilityV860extAdjacentFrequencyMeasurements::R_true);
            let encoded = rasn::uper::encode(&container).unwrap();
            assert!(adjacent_from_container(rasn::types::BitStr::from_slice(
                &encoded
            )));
            assert_eq!(
                dual_cell_from_container(rasn::types::BitStr::from_slice(&encoded)),
                Some(DualCellHsdpa {
                    category: DualCellCategory::new(category).unwrap(),
                    cell_specific_transmit_diversity: false,
                }),
            );
            let bits = rasn::types::BitStr::from_slice(&encoded);
            assert_eq!(
                dual_cell_with_legacy(FddHsdpa::Supported(Category::new(10).unwrap()), bits),
                dual_cell_from_container(bits),
            );
            for legacy in [
                FddHsdpa::NotReported,
                FddHsdpa::Unsupported,
                FddHsdpa::UnrecognizedCategory(0),
                FddHsdpa::UnrecognizedCategory(13),
            ] {
                assert_eq!(
                    dual_cell_with_legacy(legacy, bits),
                    None,
                    "legacy {legacy:?} must not admit ext2 category {category}",
                );
            }
            container
                .v6b0_non_critical_extensions
                .as_mut()
                .unwrap()
                .v6e0_non_critical_extensions
                .as_mut()
                .unwrap()
                .v770_non_critical_extensions
                .as_mut()
                .unwrap()
                .ue_radio_access_capability_v770ext
                .mac_ehs_support = None;
            let encoded = rasn::uper::encode(&container).unwrap();
            assert_eq!(
                dual_cell_from_container(rasn::types::BitStr::from_slice(&encoded)),
                None,
            );
        }
    }

    fn check_category_relations(dual: u8, original: &ie::UECapabilityContainerIEs) {
        // Independent explicit table from TS 25.331 8.1.6.2, including every
        // permitted ext1. Exercise the encoded-container path, not just a predicate.
        let allowed: &[u8] = match dual {
            21 => &[9, 10, 13, 14, 15, 16, 17, 18],
            22 => &[10, 14, 16, 18],
            23 => &[13, 14, 17, 18, 19, 20],
            24 => &[14, 18, 20],
            _ => panic!("test category"),
        };
        for single in std::iter::once(None).chain((1..=20).map(Some)) {
            let mut container = original.clone();
            let physical = &mut container
                .v6b0_non_critical_extensions
                .as_mut()
                .unwrap()
                .v6e0_non_critical_extensions
                .as_mut()
                .unwrap()
                .v770_non_critical_extensions
                .as_mut()
                .unwrap()
                .ue_radio_access_capability_v770ext
                .physical_channel_capability
                .fdd_phys_ch_capability
                .as_mut()
                .unwrap()
                .downlink_phys_ch_capability;
            if let Some(single) = single {
                physical
                    .hsdsch_physical_layer_category_ext
                    .as_mut()
                    .unwrap()
                    .0 = single;
            } else {
                physical.hsdsch_physical_layer_category_ext = None;
            }
            let encoded = rasn::uper::encode(&container).unwrap();
            for legacy in 1..=12 {
                let legacy_ok = match single {
                    Some(13 | 15 | 17 | 19) => legacy == 9,
                    Some(14 | 16 | 18 | 20) => legacy == 10,
                    Some(9 | 10) => true,
                    _ => false,
                };
                let expected = single.is_some_and(|single| allowed.contains(&single)) && legacy_ok;
                let admitted = dual_cell_with_legacy(
                    FddHsdpa::Supported(Category::new(legacy).unwrap()),
                    rasn::types::BitStr::from_slice(&encoded),
                );
                assert_eq!(
                    admitted.is_some(),
                    expected,
                    "legacy {legacy}, ext1 {single:?}, ext2 {dual}"
                );
                if let Some(admitted) = admitted {
                    assert_eq!(admitted.category.value(), dual);
                }
            }
        }
        let mut missing_fdd = original.clone();
        missing_fdd
            .v6b0_non_critical_extensions
            .as_mut()
            .unwrap()
            .v6e0_non_critical_extensions
            .as_mut()
            .unwrap()
            .v770_non_critical_extensions
            .as_mut()
            .unwrap()
            .ue_radio_access_capability_v770ext
            .physical_channel_capability
            .fdd_phys_ch_capability = None;
        let encoded = rasn::uper::encode(&missing_fdd).unwrap();
        assert_eq!(
            dual_cell_with_legacy(
                FddHsdpa::Supported(Category::new(10).unwrap()),
                rasn::types::BitStr::from_slice(&encoded)
            ),
            None
        );
    }

    #[test]
    fn single_cell_category_preserves_reported_memory_and_non_mimo_limits() {
        for (reported, effective, memory) in [
            (13, 13, 259200),
            (14, 14, 259200),
            (17, 13, 259200),
            (18, 14, 259200),
            (19, 19, 518400),
            (20, 20, 518400),
        ] {
            let category = single_cell_category(reported).unwrap();
            assert_eq!(category.value(), effective);
            assert_eq!(category.limits().total_soft_bits, memory);
        }
        assert_eq!(single_cell_category(24), None);
    }

    #[test]
    fn independent_extension_reports_category_fourteen_and_mac_ehs() {
        let bytes = [
            69, 249, 34, 205, 100, 128, 235, 247, 0, 224, 48, 161, 196, 141, 46, 101, 11, 22, 110,
            93, 208, 208,
        ];
        assert_eq!(
            qam64_from_container(rasn::types::BitStr::from_slice(&bytes)),
            Some(Category::new(14).unwrap())
        );
        assert_eq!(
            qam64_from_container(rasn::types::BitStr::from_slice(&[])),
            None
        );
    }
}
