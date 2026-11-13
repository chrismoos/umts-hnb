// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    channel_codes::OvsfCode,
    dedicated::{UplinkControlFormat, UplinkDpchCode, UplinkDpchReceiver},
    tfci::{Tfci, TfciDecoder},
    uplink_scrambling::{UplinkLongSequence, UplinkScramblingCode},
};
use num_complex::Complex32;
use std::sync::Arc;

fn waveform(data: &[Bit], tfci: Tfci) -> Vec<Complex32> {
    let mut scrambling = vec![Complex32::default(); 38400];
    UplinkLongSequence::new(UplinkScramblingCode::new(0x123456).unwrap(), 0)
        .unwrap()
        .write(&mut scrambling);
    let mut code = vec![0.0; 64];
    OvsfCode::new(64, 16).unwrap().write(&mut code).unwrap();
    let pilots = [
        62_u8, 38, 45, 36, 53, 62, 60, 52, 46, 63, 45, 55, 52, 39, 39,
    ];
    let word = tfci.encode();
    (0..38400)
        .map(|chip| {
            let slot = chip / 2560;
            let symbol = chip % 2560 / 256;
            let control = if symbol < 6 {
                Bit::from(pilots[slot] & (1 << (5 - symbol)) != 0)
            } else if symbol < 8 {
                word[slot * 2 + symbol - 6]
            } else {
                Bit::One
            };
            let i = if data[chip / 64] == Bit::Zero {
                code[chip % 64]
            } else {
                -code[chip % 64]
            };
            let q = if control == Bit::Zero {
                8.0 / 15.0
            } else {
                -8.0 / 15.0
            };
            Complex32::new(i, q) * scrambling[chip] * Complex32::new(0.7, 0.4)
        })
        .collect()
}

#[test]
fn two_transport_coding_tfci_phy_demultiplexing_and_variable_tti_allocation_are_connected() {
    let first_format = BlockFormat {
        block_bits: 140,
        blocks: 1,
        crc: TransportCrc::Crc12,
        rate: CodeRate::Half,
    };
    let second_format = BlockFormat {
        block_bits: 60,
        blocks: 1,
        crc: TransportCrc::Crc16,
        rate: CodeRate::Third,
    };
    let first_payload = payload(140);
    let second_payload = payload(60);
    let first = coded_frames(first_format, Rel99Tti::Ms20, &first_payload);
    let second = coded_frames(second_format, Rel99Tti::Ms10, &second_payload);
    assert_eq!((first.len() / 2, second.len()), (160, 252));
    let allocations = [
        allocate(&[rate(1, 160, 16), rate(5, 252, 8)], 100),
        allocate(&[rate(1, 160, 16), rate(5, 0, 8)], 100),
    ];
    let variants: Vec<_> = allocations
        .iter()
        .map(|allocation| allocation.channels()[0].output_bits)
        .collect();
    assert_eq!(variants, [335, 600]);
    let mut first_decoder =
        DedicatedUplink::new(id(1), first_format, Rel99Tti::Ms20, &variants, limits()).unwrap();
    first_decoder.begin(at(1024), 254).unwrap();
    let mut second_decoder =
        DedicatedUplink::new(id(5), second_format, Rel99Tti::Ms10, &[265], limits()).unwrap();
    second_decoder.begin(at(1024), 254).unwrap();
    let code = Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(0x123456).unwrap()).unwrap());
    let mut radio = UplinkDpchReceiver::new(code, UplinkControlFormat::new(0).unwrap()).unwrap();
    let tfcis = [Tfci::new(511).unwrap(), Tfci::new(17).unwrap()];
    let tfci_decoder = TfciDecoder::new(&tfcis).unwrap();
    let mut output = vec![Bit::One; 140];
    for (frame, allocation) in allocations.into_iter().enumerate() {
        let layout = allocation.layout();
        assert_eq!(layout, UplinkLayout::single(64).unwrap());
        let mut joined = Vec::new();
        let mut matched = vec![Bit::Zero; variants[frame]];
        RateMatching::uplink_convolutional(160, matched.len(), Rel99Tti::Ms20, frame)
            .unwrap()
            .apply(&first[frame * 160..(frame + 1) * 160], &mut matched)
            .unwrap();
        joined.extend(matched);
        if frame == 0 {
            let mut matched = vec![Bit::Zero; 265];
            RateMatching::uplink_convolutional(252, 265, Rel99Tti::Ms10, 0)
                .unwrap()
                .apply(&second, &mut matched)
                .unwrap();
            joined.extend(matched);
        }
        let physical = reference_second(&joined);
        let chips = waveform(&physical, tfcis[frame]);
        let time = 1024 + frame as u64 * CHIPS_PER_FRAME;
        radio.begin_frame(at(time)).unwrap();
        for (slot, chips) in chips.chunks_exact(2560).enumerate() {
            radio.receive(at(time + slot as u64 * 2560), chips).unwrap();
        }
        let decoded = radio.frame().unwrap();
        assert_eq!(
            decoded.decode_tfci(&tfci_decoder).unwrap().unwrap().tfci,
            tfcis[frame]
        );
        let mut soft = vec![0.0; 600];
        decoded.despread_data(layout, 0, &mut soft).unwrap();
        let matched = allocation.channels()[0];
        let second_matched = allocation.channels()[1];
        let mut composite = UplinkComposite::new(allocation).unwrap();
        let transports = composite.receive(&[&soft]).unwrap();
        first_decoder
            .receive(at(time), matched, transports.get(id(1)).unwrap())
            .unwrap();
        if frame == 0 {
            assert_eq!(first_decoder.decode(&mut output), Ok(None));
            second_decoder
                .receive(at(time), second_matched, transports.get(id(5)).unwrap())
                .unwrap();
            let mut output = vec![Bit::One; 60];
            assert_eq!(
                second_decoder.decode(&mut output),
                Ok(Some(BlockOutcome::Decoded))
            );
            assert_eq!(output, second_payload);
        } else {
            assert!(transports.get(id(5)).unwrap().is_empty());
        }
    }
    assert_eq!(
        first_decoder.decode(&mut output),
        Ok(Some(BlockOutcome::Decoded))
    );
    assert_eq!(output, first_payload);
}
