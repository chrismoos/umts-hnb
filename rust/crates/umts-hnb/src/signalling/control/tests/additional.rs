// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_rx::ReceiveIdentity;
use std::num::NonZeroU8;
use umts_rlc::acknowledged::ReceiveProvenance;
use umts_rrc::dedicated::{ExistingDchSetup, ScheduledActivation};
use umts_types::time::CHIPS_PER_FRAME;

mod install;

const ADDITIONAL_PUBLICATION_CHIPS: u64 = 300;

#[test]
fn additional_setup_waits_for_queued_and_unacknowledged_control_data() {
    let mut c = connected();
    assert!(c.radio.additional_setup_ready(c.old));
    let receipt = c
        .radio
        .registry
        .get_mut(c.key)
        .unwrap()
        .control
        .enqueue_tracked(c.key, Bits::new(&[0x55], BITS_PER_BYTE).unwrap())
        .unwrap();
    assert!(!c.radio.additional_setup_ready(c.old));
    let now = at(ADDITIONAL_PUBLICATION_CHIPS);
    let (reader, _writer) = tx_evidence::channel(SampleInstant {
        epoch: c.key.epoch,
        index: now.index * 2,
    })
    .unwrap();
    let transport = reader
        .interval_receipt(now, at(now.index + CHIPS_PER_FRAME), 2, 0)
        .unwrap();
    let bytes = c
        .radio
        .registry
        .get(c.key)
        .unwrap()
        .control
        .configuration()
        .transmit_pdu_bytes;
    assert!(
        c.radio
            .publish_srb_payload(c.key, SignallingBearer::ControlAm, now, bytes, |pdu| {
                c.sender.receive(now, pdu).unwrap();
                Ok(transport.clone())
            })
            .unwrap()
    );
    assert!(
        !c.radio
            .registry
            .get(c.key)
            .unwrap()
            .control
            .pending_new_data()
    );
    assert!(!c.radio.additional_setup_ready(c.old));
    acknowledge(&mut c, receipt, ADDITIONAL_PUBLICATION_CHIPS + 1);
    assert!(c.radio.additional_setup_ready(c.old));
}

pub(in crate::signalling::control) struct Connected {
    pub(in crate::signalling::control) radio: CommonSignalling,
    pub(in crate::signalling::control) key: UeKey,
    pub(in crate::signalling::control) sender: AmEntity,
    pub(in crate::signalling::control) integrity: IntegrityContext,
    pub(in crate::signalling::control) fresh: u32,
    pub(in crate::signalling::control) old: ReceiveIdentity,
    pub(in crate::signalling::control) new: ReceiveIdentity,
    previous: Arc<Rel99Configuration>,
    configuration: Arc<Rel99Configuration>,
    initial: AcceptedUserSetup,
    pub(in crate::signalling::control) procedure: RadioBearerSetupProcedure,
}

pub(in crate::signalling::control) fn acknowledge(
    c: &mut Connected,
    receipt: SduReceipt,
    mut tick: u64,
) -> u64 {
    let status_interval = c
        .sender
        .configuration()
        .status_prohibit_chips
        .map_or(1, |interval| interval.get());
    for _ in 0..64 {
        if c.radio.bearer_setup_transmission(c.key, receipt).unwrap()
            == SduTransmission::Acknowledged
        {
            return tick;
        }
        if c.sender
            .publish_next(at(tick), |publication| {
                c.radio.receive_srb_payload(
                    c.key,
                    SignallingBearer::ControlAm,
                    ReceiveTime::new(at(tick), at(tick + 10)).unwrap(),
                    Bits::new(publication.bytes, publication.bytes.len() * BITS_PER_BYTE).unwrap(),
                )
            })
            .unwrap()
            .is_none()
        {
            tick = tick.checked_add(status_interval).unwrap();
        }
    }
    panic!("actual peer RLC status did not acknowledge the bounded setup publication");
}

pub(in crate::signalling::control) fn connected() -> Connected {
    let (_source, mut radio, key, mut sender) = ready(false);
    let (integrity, fresh) = install_integrity(&mut radio, key);
    let retained = radio.retained_signalling(key).unwrap();
    let previous = configuration(&retained);
    let configuration = configuration_for(&retained, true);
    let old = ReceiveIdentity {
        ue: key,
        configuration: NonZeroU64::MIN,
    };
    let new = ReceiveIdentity {
        ue: key,
        configuration: NonZeroU64::new(2).unwrap(),
    };
    let mut initial = RadioBearerSetupProcedure::initial(
        key,
        radio.next_bearer_setup_transaction(key).unwrap(),
        InitialDchSetup::new(previous.clone(), retained, fixture::rabs(true, false)).unwrap(),
        at(0),
        at(1000),
        NonZeroU64::new(1_000_000).unwrap(),
    )
    .unwrap();
    let receipt = radio
        .publish_bearer_setup_on(&mut initial, old, at(0))
        .unwrap();
    super::publish(&mut radio, key, receipt, &mut sender);
    let mut initial_response = user_setup::response(initial.transaction(), 3);
    let ULDCCHMessageType::radioBearerSetupComplete(body) = &mut initial_response else {
        unreachable!()
    };
    body.count_c_activation_time = Some(ie::ActivationTime(208));
    uplink_on_air(
        &mut radio,
        key,
        &mut sender,
        &integrity::protected(&integrity, fresh, 2, 2, initial_response),
        100,
        ReceiveProvenance::Path(old.configuration),
        Some(at(100)),
    );
    let mut accepted_initial = None;
    assert_eq!(
        radio.process_one_at_with_control(at(130), security_time(), |received, mut security| {
            let (_, accepted) = received.bearer_setup_response_with(
                &mut initial,
                &mut security,
                |accepted, _| Ok(accepted),
            )?;
            accepted_initial = accepted;
            Ok(())
        }),
        Some(Ok(key))
    );
    radio.select_dedicated_route(old).unwrap();
    let activation = ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap();
    let transaction = radio.next_bearer_setup_transaction(key).unwrap();
    let mut c = Connected {
        radio,
        key,
        sender,
        integrity,
        fresh,
        old,
        new,
        previous: previous.clone(),
        configuration: configuration.clone(),
        initial: accepted_initial.unwrap(),
        procedure: RadioBearerSetupProcedure::existing(
            key,
            transaction,
            ExistingDchSetup::new(
                previous,
                configuration,
                fixture::rabs(false, true),
                activation,
            )
            .unwrap(),
            at(0),
            at(1000),
            NonZeroU64::new(CHIPS_PER_FRAME * 8).unwrap(),
        )
        .unwrap(),
    };
    acknowledge(&mut c, receipt, 200);
    c.radio.release_bearer_setup_receipt(key, receipt).unwrap();
    assert_eq!(
        c.radio.next_bearer_setup_transaction(key).unwrap(),
        c.procedure.transaction()
    );
    c
}

pub(in crate::signalling::control) fn publish(c: &mut Connected) -> SduReceipt {
    let receipt = c
        .radio
        .publish_additional_setup(
            &mut c.procedure,
            c.old,
            c.new,
            at(ADDITIONAL_PUBLICATION_CHIPS),
        )
        .unwrap();
    let now = at(ADDITIONAL_PUBLICATION_CHIPS);
    let (reader, _writer) = tx_evidence::channel(SampleInstant {
        epoch: c.key.epoch,
        index: now.index * 2,
    })
    .unwrap();
    let transport = reader
        .interval_receipt(now, at(now.index + CHIPS_PER_FRAME), 2, 0)
        .unwrap();
    let bytes = c
        .radio
        .registry
        .get(c.key)
        .unwrap()
        .control
        .configuration()
        .transmit_pdu_bytes;
    for _ in 0..128 {
        if matches!(
            c.radio.bearer_setup_transmission(c.key, receipt).unwrap(),
            SduTransmission::InFlight {
                delimited: true,
                ..
            } | SduTransmission::Acknowledged
        ) {
            return receipt;
        }
        assert!(
            c.radio
                .publish_srb_payload(c.key, SignallingBearer::ControlAm, now, bytes, |bytes| {
                    c.sender.receive(now, bytes).unwrap();
                    Ok(transport.clone())
                })
                .unwrap()
        );
    }
    panic!("additional setup did not delimit within bounded original RLC publications");
}

fn receive(c: &mut Connected, received: u64, provenance: ReceiveProvenance) {
    let message = integrity::protected(
        &c.integrity,
        c.fresh,
        2,
        3,
        user_setup::response(c.procedure.transaction(), 3),
    );
    uplink_on_air(
        &mut c.radio,
        c.key,
        &mut c.sender,
        &message,
        received,
        provenance,
        Some(at(received)),
    );
}

#[test]
fn authenticated_scheduled_completion_retries_admission_then_promotes_route_separately() {
    let mut c = connected();
    assert!(Arc::ptr_eq(c.initial.configuration(), &c.previous));
    let receipt = publish(&mut c);
    let boundary = c.procedure.activation().unwrap().time().index;
    let path = ReceiveProvenance::Path(c.new.configuration);
    receive(&mut c, boundary, path);
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(boundary + 20),
            security_time(),
            |received, mut security| {
                received
                    .additional_setup_response_with(
                        &mut c.procedure,
                        &mut security,
                        |accepted, _| {
                            assert_eq!(accepted.previous_identity(), c.old);
                            assert_eq!(accepted.identity(), c.new);
                            Err::<(), _>(SignallingError::Capacity)
                        },
                    )
                    .map(|_| ())
            }
        ),
        Some(Err(SignallingError::Capacity))
    );
    assert_eq!(c.procedure.phase(), SetupPhase::AwaitingComplete);
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
    let mut accepted = None;
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(boundary + 30),
            security_time(),
            |received, mut security| {
                let (response, result) = received.additional_setup_response_with(
                    &mut c.procedure,
                    &mut security,
                    |_, _| Ok(()),
                )?;
                assert_eq!(response, SetupResponse::Complete);
                accepted = Some(result.unwrap().0);
                Ok(())
            }
        ),
        Some(Ok(c.key))
    );
    let accepted = accepted.unwrap();
    assert!(Arc::ptr_eq(accepted.previous(), &c.previous));
    assert!(Arc::ptr_eq(accepted.configuration(), &c.configuration));
    assert_eq!(accepted.time().received(), at(boundary));
    assert_eq!(accepted.activation(), c.procedure.activation().unwrap());
    assert_eq!(accepted.rabs()[0].domain, CnDomain::Ps);
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
    assert!(matches!(
        c.radio
            .commit_additional_setup_with(&accepted, |_| Err::<(), _>("retirement busy")),
        Err(AdditionalSetupCommitError::Commit("retirement busy"))
    ));
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
    c.radio
        .commit_additional_setup_with(&accepted, |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.new));
    acknowledge(&mut c, receipt, boundary + 40);
    c.radio
        .release_additional_setup_receipt(&accepted, receipt)
        .unwrap();
    assert!(c.radio.registry.get(c.key).unwrap().bearer_change.is_none());
}

#[test]
fn additional_setup_requires_new_path_and_original_air_activation_not_late_dispatch() {
    for (received, provenance) in [
        (1000, ReceiveProvenance::Path(NonZeroU64::new(2).unwrap())),
        (
            4 * CHIPS_PER_FRAME,
            ReceiveProvenance::Path(NonZeroU64::MIN),
        ),
        (4 * CHIPS_PER_FRAME, ReceiveProvenance::Unspecified),
    ] {
        let mut c = connected();
        publish(&mut c);
        receive(&mut c, received, provenance);
        let mut called = false;
        let result = c.radio.process_one_at_with_control(
            at(5 * CHIPS_PER_FRAME),
            security_time(),
            |received, mut security| {
                received
                    .additional_setup_response_with(&mut c.procedure, &mut security, |_, _| {
                        called = true;
                        Ok(())
                    })
                    .map(|_| ())
            },
        );
        assert!(matches!(
            result,
            Some(Err(
                SignallingError::Mapping | SignallingError::Dedicated(DedicatedError::Time)
            ))
        ));
        assert!(!called);
        assert_eq!(c.procedure.phase(), SetupPhase::AwaitingComplete);
        assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
    }
}

#[test]
fn publication_rejects_initial_stale_and_cross_ue_inputs_without_consuming_transaction() {
    let mut c = connected();
    let mut initial = procedure(&c.radio, c.key);
    assert_eq!(
        c.radio.publish_additional_setup(
            &mut initial,
            c.old,
            c.new,
            at(ADDITIONAL_PUBLICATION_CHIPS)
        ),
        Err(SignallingError::Phase)
    );
    let foreign = ReceiveIdentity {
        ue: UeKey {
            slot: c.key.slot + 1,
            ..c.key
        },
        ..c.old
    };
    assert_eq!(
        c.radio.publish_additional_setup(
            &mut c.procedure,
            foreign,
            c.new,
            at(ADDITIONAL_PUBLICATION_CHIPS)
        ),
        Err(SignallingError::Origin)
    );
    assert_eq!(
        c.radio.publish_additional_setup(
            &mut c.procedure,
            c.new,
            c.old,
            at(ADDITIONAL_PUBLICATION_CHIPS)
        ),
        Err(SignallingError::Origin)
    );
    let stale = ReceiveIdentity {
        configuration: NonZeroU64::new(3).unwrap(),
        ..c.old
    };
    let target = ReceiveIdentity {
        configuration: NonZeroU64::new(4).unwrap(),
        ..c.new
    };
    assert_eq!(
        c.radio.publish_additional_setup(
            &mut c.procedure,
            stale,
            target,
            at(ADDITIONAL_PUBLICATION_CHIPS)
        ),
        Err(SignallingError::Mapping)
    );
    assert_eq!(c.procedure.phase(), SetupPhase::Prepared);
    assert_eq!(c.procedure.receipt(), None);
    assert_eq!(
        c.radio.next_bearer_setup_transaction(c.key).unwrap(),
        c.procedure.transaction()
    );
    publish(&mut c);
}

#[test]
fn initial_now_admission_cannot_accept_additional_setup_publication() {
    let mut c = connected();
    publish(&mut c);
    let boundary = c.procedure.activation().unwrap().time().index;
    let path = ReceiveProvenance::Path(c.new.configuration);
    receive(&mut c, boundary, path);
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(boundary + 20),
            security_time(),
            |received, mut security| {
                received
                    .bearer_setup_response_with(&mut c.procedure, &mut security, |_, _| Ok(()))
                    .map(|_| ())
            }
        ),
        Some(Err(SignallingError::Phase))
    );
    assert_eq!(c.procedure.phase(), SetupPhase::AwaitingComplete);
}

#[test]
fn protected_additional_publication_pressure_preserves_integrity_and_transaction_counters() {
    let mut c = connected();
    let before = c
        .radio
        .registry
        .get(c.key)
        .unwrap()
        .protection
        .integrity
        .next_downlink_sequence(c.key, 2)
        .unwrap();
    let transaction = c.radio.next_bearer_setup_transaction(c.key).unwrap();
    for _ in 0..4 {
        c.radio
            .registry
            .get_mut(c.key)
            .unwrap()
            .control
            .enqueue(c.key, Bits::new(&[0; 8], 64).unwrap())
            .unwrap();
    }
    assert_eq!(
        c.radio.publish_additional_setup(
            &mut c.procedure,
            c.old,
            c.new,
            at(ADDITIONAL_PUBLICATION_CHIPS)
        ),
        Err(SignallingError::Capacity)
    );
    assert_eq!(c.procedure.phase(), SetupPhase::Prepared);
    assert_eq!(c.procedure.receipt(), None);
    assert_eq!(
        c.radio.next_bearer_setup_transaction(c.key).unwrap(),
        transaction
    );
    assert_eq!(
        c.radio
            .registry
            .get(c.key)
            .unwrap()
            .protection
            .integrity
            .next_downlink_sequence(c.key, 2)
            .unwrap(),
        before
    );
    assert!(c.radio.registry.get(c.key).unwrap().bearer_change.is_none());
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
}

#[test]
fn even_authenticated_target_completion_requires_delimited_original_rlc_publication() {
    let mut c = connected();
    c.radio
        .publish_additional_setup(
            &mut c.procedure,
            c.old,
            c.new,
            at(ADDITIONAL_PUBLICATION_CHIPS),
        )
        .unwrap();
    let boundary = c.procedure.activation().unwrap().time().index;
    let path = ReceiveProvenance::Path(c.new.configuration);
    receive(&mut c, boundary, path);
    let mut called = false;
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(boundary + 20),
            security_time(),
            |received, mut security| {
                received
                    .additional_setup_response_with(&mut c.procedure, &mut security, |_, _| {
                        called = true;
                        Ok(())
                    })
                    .map(|_| ())
            }
        ),
        Some(Err(SignallingError::Phase))
    );
    assert!(!called);
    assert_eq!(c.procedure.phase(), SetupPhase::AwaitingComplete);
}
