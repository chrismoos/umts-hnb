// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_rx::ReceiveIdentity;
use std::num::NonZeroU8;
use umts_rlc::acknowledged::ReceiveProvenance;
use umts_rrc::dedicated::{
    ExistingDchRelease, ExistingDchSetup, RadioBearerReleaseProcedure, ReconfigurationResponse,
    ScheduledActivation,
};
use umts_types::time::CHIPS_PER_FRAME;

const COMMAND_VISITS: usize = 128;
const STATUS_VISITS: usize = 64;
const ADDITION_FRAME: u8 = 32;
const RELEASE_FRAME: u8 = 64;
const RESPONSE_FRAMES: u64 = 8;

fn response_guard(frame: u8, now: u64) -> NonZeroU64 {
    NonZeroU64::new((u64::from(frame) + RESPONSE_FRAMES) * CHIPS_PER_FRAME - now).unwrap()
}

struct Connected {
    radio: CommonSignalling,
    key: UeKey,
    sender: AmEntity,
    identity: ReceiveIdentity,
    current: Arc<Rel99Configuration>,
    now: u64,
}

impl Connected {
    fn establish() -> Self {
        let (_source, radio, key, sender) = ready(false);
        let retained = radio.retained_signalling(key).unwrap();
        let current = configuration(&retained);
        let identity = ReceiveIdentity {
            ue: key,
            configuration: NonZeroU64::MIN,
        };
        let mut session = Self {
            radio,
            key,
            sender,
            identity,
            current,
            now: 0,
        };
        let mut procedure = RadioBearerSetupProcedure::initial(
            key,
            session.radio.next_bearer_setup_transaction(key).unwrap(),
            InitialDchSetup::new(
                session.current.clone(),
                retained,
                fixture::rabs(true, false),
            )
            .unwrap(),
            at(0),
            at(CHIPS_PER_FRAME),
            NonZeroU64::new(RESPONSE_FRAMES * CHIPS_PER_FRAME).unwrap(),
        )
        .unwrap();
        let receipt = session
            .radio
            .publish_bearer_setup_on(&mut procedure, identity, at(0))
            .unwrap();
        session.deliver(receipt);
        session.now += 100;
        let mut response = user_setup::response(procedure.transaction(), 3);
        let ULDCCHMessageType::radioBearerSetupComplete(body) = &mut response else {
            unreachable!()
        };
        body.count_c_activation_time = Some(ie::ActivationTime(208));
        session.receive(response, identity);
        assert_eq!(
            session.radio.process_one_at_with_control(
                at(session.now + 20),
                security_time(),
                |received, mut security| {
                    assert_eq!(received.protection(), ControlProtection::Unprotected);
                    received
                        .bearer_setup_response(&mut procedure, &mut security)
                        .map(|_| ())
                }
            ),
            Some(Ok(key))
        );
        session.radio.select_dedicated_route(identity).unwrap();
        session.acknowledge(receipt);
        session
            .radio
            .release_bearer_setup_receipt(key, receipt)
            .unwrap();
        session
    }

    fn receive(&mut self, message: ULDCCHMessageType, identity: ReceiveIdentity) {
        uplink_on_air(
            &mut self.radio,
            self.key,
            &mut self.sender,
            &clear(message),
            self.now,
            ReceiveProvenance::Path(identity.configuration),
            Some(at(self.now)),
        );
    }

    fn deliver(&mut self, receipt: SduReceipt) {
        let first = SampleInstant {
            epoch: self.key.epoch,
            index: self.now * 2,
        };
        let (reader, _writer) = tx_evidence::channel(first).unwrap();
        let transport = reader
            .interval_receipt(at(self.now), at(self.now + CHIPS_PER_FRAME), 2, 0)
            .unwrap();
        let bytes = self
            .radio
            .registry
            .get(self.key)
            .unwrap()
            .control
            .configuration()
            .transmit_pdu_bytes;
        for _ in 0..COMMAND_VISITS {
            if matches!(
                self.radio
                    .bearer_setup_transmission(self.key, receipt)
                    .unwrap(),
                SduTransmission::InFlight {
                    delimited: true,
                    ..
                } | SduTransmission::Acknowledged
            ) {
                self.check_clear_delivery();
                return;
            }
            assert!(
                self.radio
                    .publish_srb_payload(
                        self.key,
                        SignallingBearer::ControlAm,
                        at(self.now),
                        bytes,
                        |bytes| {
                            self.sender.receive(at(self.now), bytes).unwrap();
                            Ok(transport.clone())
                        }
                    )
                    .unwrap()
            );
        }
        panic!("clear command did not delimit within its bounded AM publication");
    }

    fn check_clear_delivery(&mut self) {
        let mut sdus = 0;
        for _ in 0..COMMAND_VISITS {
            if self
                .sender
                .deliver_with(|delivery| {
                    for bytes in delivery.sdus() {
                        let decoded =
                            decode_downlink_dedicated(bytes, bytes.len() * BITS_PER_BYTE).unwrap();
                        assert!(decoded.value.integrity_check_info.is_none());
                        assert!(matches!(
                            decoded.value.message,
                            DLDCCHMessageType::radioBearerSetup(_)
                                | DLDCCHMessageType::radioBearerRelease(_)
                        ));
                        sdus += 1;
                    }
                    Ok::<_, ()>(())
                })
                .unwrap()
                .is_none()
            {
                assert_eq!(sdus, 1);
                return;
            }
        }
        panic!("clear AM command reassembly exceeded its bounded receive window");
    }

    fn acknowledge(&mut self, receipt: SduReceipt) {
        self.now += 100;
        let interval = self
            .sender
            .configuration()
            .status_prohibit_chips
            .map_or(1, |value| value.get());
        for _ in 0..STATUS_VISITS {
            if self
                .radio
                .bearer_setup_transmission(self.key, receipt)
                .unwrap()
                == SduTransmission::Acknowledged
            {
                return;
            }
            if self
                .sender
                .publish_next(at(self.now), |publication| {
                    self.radio.receive_srb_payload(
                        self.key,
                        SignallingBearer::ControlAm,
                        ReceiveTime::new(at(self.now), at(self.now + 10)).unwrap(),
                        Bits::new(publication.bytes, publication.bytes.len() * BITS_PER_BYTE)
                            .unwrap(),
                    )
                })
                .unwrap()
                .is_none()
            {
                self.now += interval;
            }
        }
        panic!("clear command did not receive its actual peer AM acknowledgment");
    }

    fn addition(&self) -> RadioBearerSetupProcedure {
        let retained = self.radio.retained_signalling(self.key).unwrap();
        RadioBearerSetupProcedure::existing(
            self.key,
            self.radio.next_bearer_setup_transaction(self.key).unwrap(),
            ExistingDchSetup::new(
                self.current.clone(),
                configuration_for(&retained, true),
                fixture::rabs(false, true),
                ScheduledActivation::new(at(0), 0, NonZeroU8::new(ADDITION_FRAME).unwrap())
                    .unwrap(),
            )
            .unwrap(),
            at(self.now),
            at(self.now + CHIPS_PER_FRAME),
            response_guard(ADDITION_FRAME, self.now),
        )
        .unwrap()
    }
}

#[test]
fn clear_additional_then_release_preserves_protection_mode_and_exact_route() {
    let mut session = Connected::establish();
    let old = session.identity;
    let next = ReceiveIdentity {
        configuration: NonZeroU64::new(2).unwrap(),
        ..old
    };
    let mut setup = session.addition();
    let forecast = session
        .radio
        .additional_setup_forecast(old, setup.encoded().bit_length)
        .unwrap();
    assert_eq!(forecast.wire_bits, setup.encoded().bit_length);
    let receipt = session
        .radio
        .publish_additional_setup(&mut setup, old, next, at(session.now))
        .unwrap();
    assert_eq!(
        session
            .radio
            .registry
            .get(session.key)
            .unwrap()
            .bearer_change
            .as_ref()
            .unwrap()
            .protection,
        PublicationProtection::Clear
    );
    session.deliver(receipt);
    session.now = u64::from(ADDITION_FRAME) * CHIPS_PER_FRAME;
    session.receive(user_setup::response(setup.transaction(), 3), next);
    let mut accepted = None;
    assert_eq!(
        session.radio.process_one_at_with_control(
            at(session.now + 20),
            security_time(),
            |received, mut security| {
                assert_eq!(received.protection(), ControlProtection::Unprotected);
                let (_, result) = received.additional_setup_response_with(
                    &mut setup,
                    &mut security,
                    |_, _| Ok(()),
                )?;
                accepted = result.map(|(accepted, ())| accepted);
                Ok(())
            }
        ),
        Some(Ok(session.key))
    );
    let accepted = accepted.unwrap();
    assert_eq!(session.radio.dedicated_route(session.key), Some(old));
    session
        .radio
        .commit_additional_setup_with(&accepted, |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(session.radio.dedicated_route(session.key), Some(next));
    session.acknowledge(receipt);
    session
        .radio
        .release_additional_setup_receipt(&accepted, receipt)
        .unwrap();
    let mut release = RadioBearerReleaseProcedure::new(
        session.key,
        session
            .radio
            .next_bearer_release_transaction(session.key)
            .unwrap(),
        ExistingDchRelease::new(
            setup.configuration().clone(),
            session.current.clone(),
            ScheduledActivation::new(at(0), 0, NonZeroU8::new(RELEASE_FRAME).unwrap()).unwrap(),
        )
        .unwrap(),
        at(session.now),
        at(session.now + CHIPS_PER_FRAME),
        response_guard(RELEASE_FRAME, session.now),
    )
    .unwrap();
    let final_identity = ReceiveIdentity {
        configuration: NonZeroU64::new(3).unwrap(),
        ..old
    };
    let forecast = session
        .radio
        .control_am_forecast(next, release.encoded().bit_length)
        .unwrap();
    assert_eq!(forecast.wire_bits, release.encoded().bit_length);
    let receipt = session
        .radio
        .publish_bearer_release_on(&mut release, next, final_identity, at(session.now))
        .unwrap();
    session.deliver(receipt);
    session.now = u64::from(RELEASE_FRAME) * CHIPS_PER_FRAME;
    session.receive(
        release::release_complete(release.transaction()),
        final_identity,
    );
    let mut accepted = None;
    assert_eq!(
        session.radio.process_one_at_with_control(
            at(session.now + 20),
            security_time(),
            |received, mut security| {
                assert_eq!(received.protection(), ControlProtection::Unprotected);
                let (response, result) = received.bearer_release_response_with(
                    &mut release,
                    &mut security,
                    |_, _| Ok(()),
                )?;
                assert_eq!(response, ReconfigurationResponse::Complete);
                accepted = result.map(|(accepted, ())| accepted);
                Ok(())
            }
        ),
        Some(Ok(session.key))
    );
    let accepted = accepted.unwrap();
    session
        .radio
        .commit_bearer_release_with(&accepted, |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(
        session.radio.dedicated_route(session.key),
        Some(final_identity)
    );
    session.acknowledge(receipt);
    session
        .radio
        .release_bearer_release_receipt(&accepted, receipt)
        .unwrap();
    assert!(
        session
            .radio
            .registry
            .get(session.key)
            .unwrap()
            .bearer_change
            .is_none()
    );
}

#[test]
fn clear_additional_publication_backpressure_preserves_transaction_and_protection() {
    let mut session = Connected::establish();
    let mut setup = session.addition();
    let transaction = setup.transaction();
    let next = ReceiveIdentity {
        configuration: NonZeroU64::new(2).unwrap(),
        ..session.identity
    };
    for _ in 0..4 {
        session
            .radio
            .registry
            .get_mut(session.key)
            .unwrap()
            .control
            .enqueue(session.key, Bits::new(&[0; 8], 64).unwrap())
            .unwrap();
    }
    assert_eq!(
        session
            .radio
            .publish_additional_setup(&mut setup, session.identity, next, at(session.now)),
        Err(SignallingError::Capacity)
    );
    assert_eq!(setup.phase(), SetupPhase::Prepared);
    assert_eq!(setup.receipt(), None);
    assert_eq!(
        session
            .radio
            .next_bearer_setup_transaction(session.key)
            .unwrap(),
        transaction
    );
    assert!(
        session
            .radio
            .registry
            .get(session.key)
            .unwrap()
            .bearer_change
            .is_none()
    );
    assert_eq!(
        session
            .radio
            .registry
            .get(session.key)
            .unwrap()
            .protection
            .integrity
            .downlink_origin(session.key, SignallingBearer::ControlAm.identity())
            .unwrap(),
        None
    );
}

#[test]
fn different_integrity_state_cannot_replace_a_clear_procedure_snapshot() {
    let mut session = Connected::establish();
    let previous = session.identity;
    let next = ReceiveIdentity {
        configuration: NonZeroU64::new(2).unwrap(),
        ..previous
    };
    let mut setup = session.addition();
    let receipt = session
        .radio
        .publish_additional_setup(&mut setup, previous, next, at(session.now))
        .unwrap();
    session.deliver(receipt);
    let (integrity, fresh) = install_integrity(&mut session.radio, session.key);
    session.now = u64::from(ADDITION_FRAME) * CHIPS_PER_FRAME;
    let bytes = integrity::protected(
        &integrity,
        fresh,
        SignallingBearer::ControlAm.identity(),
        2,
        user_setup::response(setup.transaction(), 3),
    );
    uplink_on_air(
        &mut session.radio,
        session.key,
        &mut session.sender,
        &bytes,
        session.now,
        ReceiveProvenance::Path(next.configuration),
        Some(at(session.now)),
    );
    let mut called = false;
    assert_eq!(
        session.radio.process_one_at_with_control(
            at(session.now + 20),
            security_time(),
            |received, mut security| {
                received
                    .additional_setup_response_with(&mut setup, &mut security, |_, _| {
                        called = true;
                        Ok(())
                    })
                    .map(|_| ())
            }
        ),
        Some(Err(SignallingError::Mapping))
    );
    assert!(!called);
    assert_eq!(setup.phase(), SetupPhase::AwaitingComplete);
    assert_eq!(session.radio.dedicated_route(session.key), Some(previous));
    assert_eq!(
        session
            .radio
            .registry
            .get(session.key)
            .unwrap()
            .bearer_change
            .as_ref()
            .unwrap()
            .protection,
        PublicationProtection::Clear
    );
}
