// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::NonZeroU8;
use umts_dsp::{
    channel_codes::OvsfCode,
    hsupa::{ControlDecision, ControlInformation},
    transport_blocks::BlockOutcome,
};
use umts_rrc::{
    dedicated::{DchDirection, DchTfcs, Rel99Configuration, ScheduledActivation},
    hsupa::*,
};
use umts_types::hsupa::{
    CodeLayout, Etfci, PuncturingLimit, Rel6Category, Rsn, TransportBlockTable, Tti,
};

fn configuration() -> UplinkActivation {
    let (ul, dl, bearers, physical) = crate::test_support::rel99::components(true, true, 10);
    let rebuild = |tfcs: DchTfcs, direction| {
        let mut transports = tfcs.transports().to_vec();
        for transport in &mut transports {
            for group in &mut transport.groups {
                if group.rlc_bits == 320 {
                    group.rlc_bits = 128;
                }
            }
        }
        DchTfcs::new(direction, transports, tfcs.combinations().to_vec()).unwrap()
    };
    let dedicated = Arc::new(
        Rel99Configuration::new(
            rebuild(ul, DchDirection::Uplink),
            rebuild(dl, DchDirection::Downlink),
            bearers,
            physical,
        )
        .unwrap(),
    );
    UplinkActivation::coexistence(
        dedicated,
        ActivationParameters {
            e_rnti: 0x8421,
            transport: TransportConfiguration::new(
                Tti::TwoMilliseconds,
                Rel6Category::new(6).unwrap(),
                TransportBlockTable::Zero,
                CodeLayout::new(&[2, 2]).unwrap(),
                PuncturingLimit::new(44).unwrap(),
            )
            .unwrap(),
            control: ControlConfiguration::new(8, 20).unwrap(),
            downlink: DownlinkControl::new(
                OvsfCode::new(256, 10).unwrap(),
                OvsfCode::new(128, 6).unwrap(),
                2,
                None,
                &[],
            )
            .unwrap(),
            references: umts_rrc::hsupa::ReferencePowers::single(Etfci::new(50).unwrap(), 20)
                .unwrap(),
            minimum: Etfci::new(0).unwrap(),
            scheduling: SchedulingInformation {
                without_grant: Some(ReportingPeriod::EveryTti),
                with_grant: None,
                power_offset: 0,
            },
            rv_zero_only: false,
            flows: vec![ScheduledFlow::new(0, 0, 3).unwrap()],
        },
        vec![MappedBearer {
            bearer: 20,
            uplink: UplinkMapping::new(1, 0, 1, 128, 1, true).unwrap(),
            downlink: RetainedDownlink::Dedicated,
        }],
        ScheduledActivation::new(
            ChipInstant { index: 0, ..at(0) },
            0,
            NonZeroU8::new(4).unwrap(),
        )
        .unwrap(),
        false,
    )
    .unwrap()
}

fn packet(bytes: &[u8], frame: u64) -> crate::hsupa_rx::PacketPdu<'_> {
    crate::hsupa_rx::PacketPdu {
        report: crate::hsupa_rx::Report {
            timing: None,
            identity: identity(),
            start: at(frame),
            absolute_tti: frame * 5,
            control: ControlDecision {
                information: ControlInformation {
                    format: Etfci::new(1).unwrap(),
                    sequence: Rsn::new(0).unwrap(),
                    happy: true,
                },
                normalized_score: 1.0,
                normalized_margin: Some(0.5),
            },
            feedback: umts_mac::hsupa::Reception::Ack { deliver: true },
            outcome: BlockOutcome::Decoded,
            combined: false,
            reset_reason: None,
            payload_bits: bytes.len() * 8,
        },
        logical_channel: 1,
        flow: 0,
        bits: Bits::new(bytes, bytes.len() * 8).unwrap(),
    }
}

fn encrypted(peer: &mut AmEntity, sequence: u32, contents: &[u8]) -> Vec<u8> {
    peer.enqueue(contents).unwrap();
    let (_, mut pdu) = peer
        .publish_next(at(0), |publication| Ok::<_, ()>(publication.bytes.to_vec()))
        .unwrap()
        .unwrap();
    let length = pdu.len();
    Uea1Key::new(&Key::new([0x22; 16]))
        .apply(
            sequence,
            19,
            Direction::Uplink,
            &mut pdu[2..],
            (length - 2) * 8,
        )
        .unwrap();
    pdu
}

#[test]
fn edch_and_rel99_share_ps_cipher_sequence_and_preserve_cs_context() {
    let configuration = configuration();
    let mut bank = bank(8, Segmentation::Disabled);
    let mut ciphers = ciphers();
    let mut peer = am();
    let first = encrypted(&mut peer, 0, b"enhanced");
    let mut packets = crate::hsupa_rx::PacketDelivery::new(
        identity(),
        &[(
            1,
            umts_mac::hsupa::payload::Mapping {
                logical_channel: 1,
                flow: 0,
                pdu_bits: n(128),
            },
        )],
        n(256),
        n(2),
        NonZeroU64::new(10).unwrap(),
        0,
        n(1_000_000),
    )
    .unwrap();
    let length = 18 + first.len() * 8;
    let mut packed = vec![0; length.div_ceil(8)];
    umts_types::bits::write_u32(&mut packed, 0, 6, 1).unwrap();
    umts_types::bits::write_u32(&mut packed, 6, 6, 1).unwrap();
    Bits::new(&first, first.len() * 8)
        .unwrap()
        .copy_into(&mut packed, 18)
        .unwrap();
    let mut decoded = vec![Bit::Zero; length];
    umts_dsp::bits::unpack_msb(&packed, &mut decoded).unwrap();
    let mut report = packet(&first, 4).report;
    report.payload_bits = length;
    packets
        .admit(
            crate::hsupa_rx::ReportRef {
                report,
                payload: &decoded,
            },
            at(5).index,
        )
        .unwrap();
    assert_eq!(
        bank.service_hsupa(
            &configuration,
            &mut packets,
            at(5),
            &mut ciphers,
            ResetPermission::Defer
        ),
        Ok(BearerProgress::Pdu)
    );
    assert!(packets.is_empty());
    let second = encrypted(&mut peer, 1, b"second");
    enqueue(&mut bank, 2, 6, bits(&second, second.len() * 8)).unwrap();
    run(&mut bank, &mut ciphers);
    let outputs = collect(&mut bank);
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0].1, b"enhanced");
    assert_eq!(outputs[1].1, b"second");
    assert_eq!(
        outputs[0].0.received.index,
        at(4).index + Tti::TwoMilliseconds.chips()
    );
    assert_eq!(outputs[1].0.received, at(7));
    assert_eq!(
        ciphers.layout(key(), 10).unwrap(),
        (Scope::User(CnDomain::Cs), CipherMode::Transparent)
    );
}

#[test]
fn edch_rejects_wrong_route_generation_and_activation_without_consuming_sequence() {
    let configuration = configuration();
    let mut bank = bank(8, Segmentation::Disabled);
    let mut ciphers = ciphers();
    let first = encrypted(&mut am(), 0, b"retained");
    let mut wrong = packet(&first, 4);
    wrong.report.identity.configuration = NonZeroU64::new(2).unwrap();
    assert!(
        bank.receive_hsupa(
            &configuration,
            wrong,
            at(5),
            &mut ciphers,
            ResetPermission::Defer
        )
        .is_err()
    );
    let mut wrong = packet(&first, 4);
    wrong.flow = 1;
    assert!(
        bank.receive_hsupa(
            &configuration,
            wrong,
            at(5),
            &mut ciphers,
            ResetPermission::Defer
        )
        .is_err()
    );
    assert!(
        bank.receive_hsupa(
            &configuration,
            packet(&first, 0),
            at(1),
            &mut ciphers,
            ResetPermission::Defer
        )
        .is_err()
    );
    assert_eq!(
        bank.receive_hsupa(
            &configuration,
            packet(&first, 4),
            at(5),
            &mut ciphers,
            ResetPermission::Defer
        ),
        Ok(BearerProgress::Pdu)
    );
    run(&mut bank, &mut ciphers);
    assert_eq!(collect(&mut bank)[0].1, b"retained");
}
