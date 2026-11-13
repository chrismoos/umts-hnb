// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_rrc_schema::class_definitions::*;
use umts_rrc_schema::{information_elements::*, pdu_definitions::*};

fn status() -> ULDCCHMessageType {
    ULDCCHMessageType::rrcStatus(RRCStatus::new(
        ProtocolErrorMoreInformation::new(ProtocolErrorMoreInformationDiagnosticsType::type1(
            ProtocolErrorMoreInformationDiagnosticsTypeType1::messageNotCompatibleWithReceiverState(
                IdentificationOfReceivedMessage::new(
                    RRCTransactionIdentifier(1),
                    ReceivedMessageType::radioBearerSetup,
                ),
            ),
        )),
        None,
    ))
}

fn measurement() -> ULDCCHMessageType {
    ULDCCHMessageType::measurementReport(MeasurementReport::new(
        MeasurementIdentity(1),
        None,
        None,
        None,
        None,
        None,
    ))
}

fn counter_response() -> ULDCCHMessageType {
    ULDCCHMessageType::counterCheckResponse(CounterCheckResponse::new(
        RRCTransactionIdentifier(0),
        None,
        None,
    ))
}

#[test]
fn unsupported_authenticated_control_does_not_block_later_status_or_change_bearers() {
    for protected in [false, true] {
        let (_source, mut radio, key, mut sender) = ready(false);
        let integrity = protected.then(|| install_integrity(&mut radio, key));
        let mut procedure = procedure(&radio, key);
        radio.publish_bearer_setup(&mut procedure, at(0)).unwrap();
        for (index, message) in [counter_response(), status(), measurement()]
            .into_iter()
            .enumerate()
        {
            let bytes = if let Some((context, fresh)) = &integrity {
                integrity::protected(context, *fresh, 2, 2 + index as u32, message)
            } else {
                clear(message)
            };
            uplink(
                &mut radio,
                key,
                &mut sender,
                &bytes,
                100 + index as u64 * 100,
            );
            assert_eq!(
                radio.process_one_at_with_control(at(450), security_time(), |received, _| {
                    Err(received.unsupported())
                }),
                Some(Ok(key))
            );
        }
        let context = radio.registry.get(key).unwrap();
        assert_eq!(context.reports.unsupported_control_count, 1);
        assert_eq!(context.reports.status_count, 1);
        assert_eq!(context.reports.measurement_count, 1);
        assert!(!context.protection.blocks_status());
        assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
        assert!(context.bearer_change.is_some());
    }
}

#[test]
fn unsupported_control_with_invalid_mac_is_not_consumed() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let (context, fresh) = install_integrity(&mut radio, key);
    let mut bytes = integrity::protected(&context, fresh, 2, 2, counter_response());
    bytes[1] ^= 1;
    uplink(&mut radio, key, &mut sender, &bytes, 100);
    assert!(
        radio
            .process_one_at_with_control(at(150), security_time(), |_, _| {
                panic!("invalid MAC must not reach unsupported-message handling")
            })
            .unwrap()
            .is_err()
    );
    assert_eq!(
        radio
            .registry
            .get(key)
            .unwrap()
            .reports
            .unsupported_control_count,
        0
    );
}

#[test]
fn reports_are_consumed_without_completing_a_pending_bearer_procedure() {
    for protected in [false, true] {
        let (_source, mut radio, key, mut sender) = ready(false);
        let integrity = protected.then(|| install_integrity(&mut radio, key));
        let mut procedure = procedure(&radio, key);
        radio.publish_bearer_setup(&mut procedure, at(0)).unwrap();
        for (index, message) in [status(), measurement()].into_iter().enumerate() {
            let bytes = if let Some((context, fresh)) = &integrity {
                integrity::protected(context, *fresh, 2, 2 + index as u32, message)
            } else {
                clear(message)
            };
            uplink(
                &mut radio,
                key,
                &mut sender,
                &bytes,
                100 + index as u64 * 100,
            );
            assert_eq!(
                radio.process_one_at_with_control(at(350), security_time(), |_, _| {
                    panic!("report delegated to bearer completion")
                }),
                Some(Ok(key))
            );
        }
        let context = radio.registry.get(key).unwrap();
        assert_eq!(context.reports.status_count, 1);
        assert_eq!(context.reports.measurement_count, 1);
        assert_eq!(
            context
                .reports
                .last_status
                .unwrap()
                .rejected
                .unwrap()
                .transaction,
            1
        );
        assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
        assert!(context.bearer_change.is_some());
    }
}

#[test]
fn invalid_integrity_cannot_publish_reports() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let (context, fresh) = install_integrity(&mut radio, key);
    let mut bytes = integrity::protected(&context, fresh, 2, 2, status());
    bytes[1] ^= 1;
    uplink(&mut radio, key, &mut sender, &bytes, 100);
    assert!(
        radio
            .process_one_at_with_control(at(150), security_time(), |_, _| {
                panic!("unauthenticated report dispatched")
            })
            .unwrap()
            .is_err()
    );
    assert_eq!(radio.registry.get(key).unwrap().reports.status_count, 0);
}
