// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{fmt::Write, fs};
use umts_user_plane::{gtpu, iuup, rtp};

fn hex(bytes: &[u8]) -> String {
    let mut text = String::from("000000");
    for byte in bytes {
        write!(&mut text, " {byte:02x}").expect("string write");
    }
    text.push_str("\n\n");
    text
}
fn main() {
    let directory = std::env::args().nth(1).expect("output directory");
    let mut output = [0; 256];
    let mut gtp = String::new();
    let ip = [
        0x45, 0, 0, 20, 0, 0, 0, 0, 64, 59, 0x7c, 0xac, 127, 0, 0, 1, 127, 0, 0, 2,
    ];
    let size = gtpu::encode(
        gtpu::Packet {
            kind: gtpu::MessageType::Data,
            teid: Some(gtpu::PeerTeid::new(0x10203040)),
            sequence: Some(0x1234),
            n_pdu: None,
            payload: &ip,
        },
        &mut output,
    )
    .unwrap();
    gtp.push_str(&hex(&output[..size]));
    let size = gtpu::echo_response(0x4321, &mut output).unwrap();
    gtp.push_str(&hex(&output[..size]));
    let size = gtpu::encode(
        gtpu::Packet {
            kind: gtpu::MessageType::Data,
            teid: Some(gtpu::PeerTeid::new(0)),
            sequence: Some(u16::MAX),
            n_pdu: None,
            payload: &ip,
        },
        &mut output,
    )
    .unwrap();
    gtp.push_str(&hex(&output[..size]));
    fs::write(format!("{directory}/gtpu.hex"), gtp).unwrap();
    let mut media = String::new();
    use iuup::initialization::{Combination, DataPduType, Initialization, Rfci, Versions};
    let initialization = Initialization::new(
        &[
            Combination::new(Rfci::new(0).unwrap(), &[81, 103, 60], Some(2)).unwrap(),
            Combination::new(Rfci::new(1).unwrap(), &[39, 0, 0], Some(3)).unwrap(),
        ],
        Versions::new(3).unwrap(),
        DataPduType::Crc,
        false,
    )
    .unwrap();
    let mut initialization_payload = [0; 64];
    let initialization_bytes = initialization.encode(&mut initialization_payload).unwrap();
    let frames = [
        iuup::Frame::Data {
            number: 3,
            quality: iuup::FrameQuality::Good,
            rfci: 5,
            payload_crc: iuup::PayloadCheck::Valid,
            payload: &[0x12, 0x34, 0x56],
        },
        iuup::Frame::Data {
            number: 4,
            quality: iuup::FrameQuality::BadRadio,
            rfci: 6,
            payload_crc: iuup::PayloadCheck::Absent,
            payload: &[0x78, 0x9a],
        },
        iuup::Frame::Control {
            number: 2,
            acknowledgment: iuup::Acknowledgment::Positive,
            mode: 0,
            procedure: 0,
            payload_crc: iuup::PayloadCheck::Absent,
            payload: &[],
        },
        iuup::Frame::Control {
            number: 0,
            acknowledgment: iuup::Acknowledgment::Procedure,
            mode: 0,
            procedure: 0,
            payload_crc: iuup::PayloadCheck::Valid,
            payload: &initialization_payload[..initialization_bytes],
        },
    ];
    let mut payload = [0; 128];
    for (index, frame) in frames.into_iter().enumerate() {
        let size = iuup::encode(frame, &mut payload).unwrap();
        let size = rtp::encode(
            rtp::Packet {
                marker: false,
                payload_type: 96,
                sequence: index as u16,
                timestamp: index as u32 * (rtp::IU_CLOCK_HZ / 50),
                ssrc: 0x10203040,
                payload: &payload[..size],
            },
            &mut output,
        )
        .unwrap();
        media.push_str(&hex(&output[..size]));
    }
    fs::write(format!("{directory}/iuup-rtp.hex"), media).unwrap();
}
