// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn shared_transport_usage_keeps_uncoded_units_and_all_four_budgets() {
    let limits = DirectionLimits {
        transport_bits: 6400,
        convolutional_bits: 1000,
        turbo_bits: Some(6400),
        transport_blocks: 32,
        transport_channels: 8,
        combinations: 64,
        formats: 64,
    };
    let signal = TransportUsage::transport(
        ChannelCoding::Convolutional(crate::transport::ConvolutionalRate::Third),
        376,
        2,
    );
    let packet = TransportUsage::transport(ChannelCoding::Turbo, 19 * 320, 19);
    assert!(limits.validate_usage(DchDirection::Uplink, packet).is_ok());
    assert!(matches!(
        limits.validate_usage(DchDirection::Uplink, signal.checked_add(packet).unwrap()),
        Err(Error::Exceeded {
            limit: Limit::TransportBits,
            required: 6456,
            supported: 6400,
            ..
        })
    ));
    let mut one = limits;
    one.convolutional_bits = 375;
    assert!(matches!(
        one.validate_usage(DchDirection::Uplink, signal),
        Err(Error::Exceeded {
            limit: Limit::ConvolutionalBits,
            ..
        })
    ));
    one = limits;
    one.turbo_bits = None;
    assert!(matches!(
        one.validate_usage(DchDirection::Uplink, packet),
        Err(Error::Exceeded {
            limit: Limit::TurboBits,
            ..
        })
    ));
    one = limits;
    one.transport_blocks = 18;
    assert!(matches!(
        one.validate_usage(DchDirection::Uplink, packet),
        Err(Error::Exceeded {
            limit: Limit::TransportBlocks,
            ..
        })
    ));
    assert!(
        TransportUsage {
            bits: usize::MAX,
            ..TransportUsage::default()
        }
        .checked_add(packet)
        .is_err()
    );
}

use crate::test_support::{capabilities as capability_fixture, dedicated as dedicated_fixture};
use std::num::{NonZeroU8, NonZeroU64};
use umts_types::{RadioEpoch, time::ChipInstant};

fn time(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}

fn supported() -> Rel99Limits {
    let mut information = capability_fixture::information();
    let radio = information.ue_radio_access_capability.as_mut().unwrap();
    let transport = &mut radio.transport_channel_capability;
    transport.ul_trans_ch_capability.max_simultaneous_trans_chs = ie::MaxSimultaneousTransChsUL::e8;
    transport.dl_trans_ch_capability.max_simultaneous_trans_chs = ie::MaxSimultaneousTransChsDL::e8;
    transport.ul_trans_ch_capability.max_transmitted_blocks = ie::MaxTransportBlocksUL::tb8;
    transport
        .dl_trans_ch_capability
        .max_received_transport_blocks = ie::MaxTransportBlocksDL::tb8;
    transport.ul_trans_ch_capability.max_number_of_tfc = ie::MaxNumberOfTFCUL::tfc32;
    transport.dl_trans_ch_capability.max_number_of_tfc = ie::MaxNumberOfTFCDL::tfc32;
    Rel99Limits::from_information(&information).unwrap()
}

#[test]
fn before_deratematching_limit_uses_exact_rational_bound_separate_from_tb_bits() {
    let mut limits = supported();
    limits.downlink.transport_bits = 641;
    assert_eq!(limits.validate_downlink_matched_bits(4230), Ok(()));
    exceeded(
        limits.validate_downlink_matched_bits(4231),
        Some(DchDirection::Downlink),
        Limit::DownlinkBeforeDeratematchingBits,
        4231,
        4230,
    );
    limits.downlink.transport_bits = usize::MAX;
    assert_eq!(
        limits.validate_downlink_matched_bits(0),
        Err(Error::InvalidValue("de-rate-matching bound"))
    );
}

fn configuration(
    voice: bool,
    packet: bool,
    edit: impl FnOnce(&mut DchTfcs, &mut DchTfcs, &mut Vec<crate::dedicated::Rel99Bearer>),
) -> Rel99Configuration {
    let (mut ul, mut dl, mut bearers, physical) = dedicated_fixture::components(voice, packet, 10);
    edit(&mut ul, &mut dl, &mut bearers);
    Rel99Configuration::new(ul, dl, bearers, physical).unwrap()
}

fn exceeded(
    result: Result<(), Error>,
    direction: Option<DchDirection>,
    limit: Limit,
    required: usize,
    supported: usize,
) {
    assert_eq!(
        result,
        Err(Error::Exceeded {
            direction,
            limit,
            required,
            supported
        })
    );
}

#[test]
fn actual_asn1_enum_values_normalize_to_units_not_discriminants() {
    let mut information = capability_fixture::information();
    let radio = information.ue_radio_access_capability.as_mut().unwrap();
    radio
        .transport_channel_capability
        .ul_trans_ch_capability
        .max_no_bits_transmitted = ie::MaxNoBits::b163840;
    radio
        .transport_channel_capability
        .dl_trans_ch_capability
        .max_no_bits_received = ie::MaxNoBits::b640;
    radio
        .transport_channel_capability
        .ul_trans_ch_capability
        .max_number_of_tfc = ie::MaxNumberOfTFCUL::tfc96;
    radio
        .transport_channel_capability
        .dl_trans_ch_capability
        .max_number_of_tfc = ie::MaxNumberOfTFCDL::tfc48;
    radio
        .transport_channel_capability
        .ul_trans_ch_capability
        .max_number_of_tf = ie::MaxNumberOfTF::tf1024;
    let physical = radio
        .physical_channel_capability
        .fdd_phys_ch_capability
        .as_mut()
        .unwrap();
    physical
        .downlink_phys_ch_capability
        .max_no_phys_ch_bits_received = ie::MaxNoPhysChBitsReceived::b76800;
    physical
        .uplink_phys_ch_capability
        .max_no_dpdch_bits_transmitted = ie::MaxNoDPDCHBitsTransmitted::b57600;
    radio.rlc_capability.total_rlc_am_buffer_size = ie::TotalRLCAMBufferSize::kb10;
    radio.rlc_capability.maximum_rlc_window_size = ie::MaximumRLCWindowSize::mws4095;
    let limits = Rel99Limits::from_information(&information).unwrap();
    assert_eq!(limits.uplink.transport_bits, 163_840);
    assert_eq!(limits.downlink.transport_bits, 640);
    assert_eq!(limits.uplink.combinations, 96);
    assert_eq!(limits.downlink.combinations, 48);
    assert_eq!(limits.uplink.formats, 1024);
    assert_eq!(limits.downlink.formats, 32);
    assert_eq!(limits.uplink.transport_blocks, 4);
    assert_eq!(limits.downlink.transport_channels, 4);
    assert_eq!(limits.uplink.turbo_bits, Some(10_240));
    assert_eq!(limits.downlink_physical_bits, 76_800);
    assert_eq!(limits.uplink_physical_bits, 57_600);
    assert_eq!(limits.am_entities, 4);
    assert_eq!(limits.am_window, 4095);
    assert_eq!(limits.am_buffer_bytes, 10 * RLC_KBYTE_OCTETS);
}

#[test]
fn dummy_and_spare_values_never_become_guessed_capability_defaults() {
    for value in [ie::MaxNumberOfTFCUL::dummy1, ie::MaxNumberOfTFCUL::dummy2] {
        let mut information = capability_fixture::information();
        information
            .ue_radio_access_capability
            .as_mut()
            .unwrap()
            .transport_channel_capability
            .ul_trans_ch_capability
            .max_number_of_tfc = value;
        assert!(matches!(
            Rel99Limits::from_information(&information),
            Err(Error::InvalidValue(_))
        ));
    }
    for value in [
        ie::TotalRLCAMBufferSize::dummy,
        ie::TotalRLCAMBufferSize::spare,
    ] {
        let mut information = capability_fixture::information();
        information
            .ue_radio_access_capability
            .as_mut()
            .unwrap()
            .rlc_capability
            .total_rlc_am_buffer_size = value;
        assert!(matches!(
            Rel99Limits::from_information(&information),
            Err(Error::InvalidValue(_))
        ));
    }
    let mut information = capability_fixture::information();
    information
        .ue_radio_access_capability
        .as_mut()
        .unwrap()
        .physical_channel_capability
        .fdd_phys_ch_capability
        .as_mut()
        .unwrap()
        .downlink_phys_ch_capability
        .max_no_phys_ch_bits_received = ie::MaxNoPhysChBitsReceived::dummy;
    assert!(matches!(
        Rel99Limits::from_information(&information),
        Err(Error::InvalidValue(_))
    ));
    for field in 0..3 {
        let mut information = capability_fixture::information();
        let radio = information.ue_radio_access_capability.as_mut().unwrap();
        match field {
            0 => {
                radio
                    .transport_channel_capability
                    .ul_trans_ch_capability
                    .max_simultaneous_trans_chs = ie::MaxSimultaneousTransChsUL::dummy
            }
            1 => {
                radio
                    .transport_channel_capability
                    .ul_trans_ch_capability
                    .max_transmitted_blocks = ie::MaxTransportBlocksUL::dummy
            }
            _ => {
                radio.rlc_capability.maximum_am_entity_number =
                    ie::MaximumAMEntityNumberRLCCap::dummy
            }
        }
        assert!(matches!(
            Rel99Limits::from_information(&information),
            Err(Error::InvalidValue(_))
        ));
    }
}

#[test]
fn actual_initial_cs_ps_and_combined_wire_configurations_fit_a_reported_ue() {
    let limits = supported();
    for (voice, packet) in [(true, false), (false, true), (true, true)] {
        let configuration = dedicated_fixture::configuration(voice, packet, 10);
        if voice && packet {
            let previous = dedicated_fixture::configuration(true, false, 10);
            let initial = crate::dedicated::InitialDchSetup::new(
                previous.clone(),
                dedicated_fixture::retained(),
                dedicated_fixture::rabs(true, false),
            )
            .unwrap();
            assert!(
                !initial
                    .message(crate::establishment::TransactionId::new(0).unwrap())
                    .unwrap()
                    .bytes
                    .is_empty()
            );
            let request = crate::dedicated::ExistingDchSetup::new(
                previous,
                configuration.clone(),
                dedicated_fixture::rabs(false, true),
                crate::dedicated::ScheduledActivation::new(time(0), 0, NonZeroU8::new(4).unwrap())
                    .unwrap(),
            )
            .unwrap();
            assert!(
                !request
                    .message(crate::establishment::TransactionId::new(0).unwrap())
                    .unwrap()
                    .bytes
                    .is_empty()
            );
        } else {
            let request = crate::dedicated::InitialDchSetup::new(
                configuration.clone(),
                dedicated_fixture::retained(),
                dedicated_fixture::rabs(voice, packet),
            )
            .unwrap();
            assert!(
                !request
                    .message(crate::establishment::TransactionId::new(0).unwrap())
                    .unwrap()
                    .bytes
                    .is_empty()
            );
        }
        assert_eq!(limits.validate_dch(&configuration), Ok(()));
    }
}

#[test]
fn directional_coding_limits_count_transport_block_bits_not_fec_output() {
    let configuration = configuration(false, true, |_, _, _| ());
    let mut limits = supported();
    limits.uplink.transport_bits = 1428;
    limits.downlink.transport_bits = 1620;
    limits.uplink.convolutional_bits = 148;
    limits.downlink.convolutional_bits = 340;
    limits.uplink.turbo_bits = Some(1280);
    limits.downlink.turbo_bits = Some(1280);
    limits.uplink.transport_blocks = 5;
    limits.downlink.transport_blocks = 5;
    assert_eq!(limits.validate_dch(&configuration), Ok(()));
    let mut smaller = limits;
    smaller.uplink.transport_bits -= 1;
    exceeded(
        smaller.validate_dch(&configuration),
        Some(DchDirection::Uplink),
        Limit::TransportBits,
        1428,
        1427,
    );
    let mut smaller = limits;
    smaller.downlink.convolutional_bits -= 1;
    exceeded(
        smaller.validate_dch(&configuration),
        Some(DchDirection::Downlink),
        Limit::ConvolutionalBits,
        340,
        339,
    );
    let mut smaller = limits;
    smaller.uplink.turbo_bits = Some(1279);
    exceeded(
        smaller.validate_dch(&configuration),
        Some(DchDirection::Uplink),
        Limit::TurboBits,
        1280,
        1279,
    );
    let mut unsupported = limits;
    unsupported.downlink.turbo_bits = None;
    exceeded(
        unsupported.validate_dch(&configuration),
        Some(DchDirection::Downlink),
        Limit::TurboBits,
        1280,
        0,
    );
}

#[test]
fn only_explicit_simultaneous_tfcs_rows_are_checked_not_a_cartesian_product() {
    let configuration = configuration(false, true, |ul, dl, _| {
        for tfcs in [ul, dl] {
            let gain = tfcs.combinations()[0].gain;
            *tfcs = DchTfcs::new(
                tfcs.direction(),
                tfcs.transports().to_vec(),
                vec![
                    crate::dedicated::Combination {
                        formats: vec![0, 0],
                        gain,
                    },
                    crate::dedicated::Combination {
                        formats: vec![1, 0],
                        gain,
                    },
                    crate::dedicated::Combination {
                        formats: vec![0, 1],
                        gain,
                    },
                ],
            )
            .unwrap();
        }
    });
    let mut limits = supported();
    limits.uplink.transport_bits = 1280;
    limits.downlink.transport_bits = 1280;
    limits.uplink.transport_blocks = 4;
    limits.downlink.transport_blocks = 4;
    assert_eq!(limits.validate_dch(&configuration), Ok(()));
    limits.downlink.transport_bits = 1279;
    exceeded(
        limits.validate_dch(&configuration),
        Some(DchDirection::Downlink),
        Limit::TransportBits,
        1280,
        1279,
    );
}

#[test]
fn channel_format_combination_and_physical_limits_are_independent() {
    let configuration = configuration(true, true, |_, _, _| ());
    let limits = supported();
    assert_eq!(limits.validate_dch(&configuration), Ok(()));
    let mut smaller = limits;
    smaller.uplink.transport_channels = 4;
    exceeded(
        smaller.validate_dch(&configuration),
        Some(DchDirection::Uplink),
        Limit::TransportChannels,
        5,
        4,
    );
    let mut smaller = limits;
    smaller.downlink.combinations = 23;
    exceeded(
        smaller.validate_dch(&configuration),
        Some(DchDirection::Downlink),
        Limit::Combinations,
        24,
        23,
    );
    let mut smaller = limits;
    smaller.uplink.formats = 16;
    exceeded(
        smaller.validate_dch(&configuration),
        Some(DchDirection::Uplink),
        Limit::Formats,
        17,
        16,
    );
    let mut smaller = limits;
    smaller.downlink_codes = 0;
    exceeded(
        smaller.validate_dch(&configuration),
        None,
        Limit::DownlinkCodes,
        1,
        0,
    );
    let mut smaller = limits;
    smaller.downlink_physical_bits = 4799;
    exceeded(
        smaller.validate_dch(&configuration),
        None,
        Limit::DownlinkPhysicalBits,
        4800,
        4799,
    );
    let mut smaller = limits;
    smaller.uplink_physical_bits = 9599;
    exceeded(
        smaller.validate_dch(&configuration),
        None,
        Limit::UplinkPhysicalBits,
        9600,
        9599,
    );
}

#[test]
fn am_caps_apply_per_entity_and_window_without_summing_every_maximum_window() {
    let configuration = configuration(false, true, |_, _, bearers| {
        for bearer in bearers {
            if let Rel99Rlc::Acknowledged { uplink, downlink } = &mut bearer.rlc {
                uplink.transmission_window_size = ie::TransmissionWindowSize::tw2047;
                downlink.receiving_window_size = ie::ReceivingWindowSize::rw2047;
            }
        }
    });
    let mut limits = supported();
    limits.am_buffer_bytes = 10_000;
    assert_eq!(limits.validate_dch(&configuration), Ok(()));
    let mut smaller = limits;
    smaller.am_entities = 2;
    exceeded(
        smaller.validate_dch(&configuration),
        None,
        Limit::AmEntities,
        3,
        2,
    );
    let mut smaller = limits;
    smaller.am_window = 2046;
    exceeded(
        smaller.validate_dch(&configuration),
        None,
        Limit::AmWindow,
        2047,
        2046,
    );
}

#[test]
fn absent_radio_fdd_or_incompatible_uplink_mode_is_not_inferred() {
    let mut information = capability_fixture::information();
    information.ue_radio_access_capability = None;
    assert_eq!(
        Rel99Limits::from_information(&information),
        Err(Error::MissingRadio)
    );
    let mut information = capability_fixture::information();
    information
        .ue_radio_access_capability
        .as_mut()
        .unwrap()
        .physical_channel_capability
        .fdd_phys_ch_capability = None;
    assert_eq!(
        Rel99Limits::from_information(&information),
        Err(Error::MissingFdd)
    );
    let mut information = capability_fixture::information();
    information
        .ue_radio_access_capability
        .as_mut()
        .unwrap()
        .transport_channel_capability
        .ul_trans_ch_capability
        .mode_specific_info = ie::ULTransChCapabilityModeSpecificInfo::tdd(
        ie::ULTransChCapabilityModeSpecificInfoTdd::new(ie::MaxSimultaneousCCTrCHCount(1)),
    );
    assert_eq!(
        Rel99Limits::from_information(&information),
        Err(Error::MissingFdd)
    );
}
