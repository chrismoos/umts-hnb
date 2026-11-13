// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_rx::ReceiveIdentity;
use std::num::NonZeroU8;
use umts_rlc::acknowledged::ReceiveProvenance;
use umts_rrc::dedicated::{
    ExistingDchRelease, RadioBearerReleaseProcedure, ReconfigurationResponse, ScheduledActivation,
};
use umts_types::time::CHIPS_PER_FRAME;
mod hsdpa;

pub(in crate::signalling::control) const RELEASE_PUBLICATION_FRAME: u64 = 16;
const RELEASE_ACTIVATION_FRAME: u64 = 20;

fn release_configuration(retained: &[RetainedBearer], voice: bool) -> Arc<Rel99Configuration> {
    if voice {
        return configuration_for(retained, true);
    }
    let (ul, dl, mut bearers, physical) = fixture::components(false, true, 10);
    for bearer in &mut bearers {
        if let Some(old) = retained.iter().find(|old| old.id == bearer.id) {
            bearer.rlc = old.rlc.clone();
        }
    }
    let rebuild = |tfcs: &DchTfcs, direction, size| {
        let mut transports = tfcs.transports().to_vec();
        for group in &mut transports.last_mut().unwrap().groups {
            group.rlc_bits = size;
        }
        DchTfcs::new(direction, transports, tfcs.combinations().to_vec()).unwrap()
    };
    Arc::new(
        Rel99Configuration::new(
            rebuild(&ul, DchDirection::Uplink, retained[1].uplink_pdu_bits),
            rebuild(&dl, DchDirection::Downlink, retained[1].downlink_pdu_bits),
            bearers,
            physical,
        )
        .unwrap(),
    )
}
fn acknowledge(
    radio: &mut CommonSignalling,
    key: UeKey,
    sender: &mut AmEntity,
    receipt: SduReceipt,
    mut tick: u64,
) {
    let status_interval = sender
        .configuration()
        .status_prohibit_chips
        .map_or(1, |interval| interval.get());
    for _ in 0..64 {
        if radio.bearer_setup_transmission(key, receipt).unwrap() == SduTransmission::Acknowledged {
            return;
        }
        let published = sender
            .publish_next(at(tick), |publication| {
                radio.receive_srb_payload(
                    key,
                    SignallingBearer::ControlAm,
                    ReceiveTime::new(at(tick), at(tick + 10)).unwrap(),
                    Bits::new(publication.bytes, publication.bytes.len() * BITS_PER_BYTE).unwrap(),
                )
            })
            .unwrap();
        if published.is_none() {
            tick = tick.checked_add(status_interval).unwrap();
        }
    }
    panic!("missing bounded RLC acknowledgment");
}
pub(in crate::signalling::control) fn publish_release(c: &mut Connected, receipt: SduReceipt) {
    let tick = RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME + 300;
    let first = SampleInstant {
        epoch: c.key.epoch,
        index: tick,
    };
    let (reader, _writer) = tx_evidence::channel(first).unwrap();
    let transport = reader
        .interval_receipt(at(tick), at(tick + CHIPS_PER_FRAME), 2, 0)
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
            return;
        }
        assert!(
            c.radio
                .publish_srb_payload(
                    c.key,
                    SignallingBearer::ControlAm,
                    at(tick),
                    bytes,
                    |bytes| {
                        c.sender.receive(at(tick), bytes).unwrap();
                        Ok(transport.clone())
                    }
                )
                .unwrap()
        );
    }
    panic!("release SDU did not delimit within bounded publications");
}
pub(in crate::signalling::control) struct Connected {
    pub(in crate::signalling::control) radio: CommonSignalling,
    pub(in crate::signalling::control) key: UeKey,
    sender: AmEntity,
    integrity: IntegrityContext,
    fresh: u32,
    pub(in crate::signalling::control) old: ReceiveIdentity,
    pub(in crate::signalling::control) new: ReceiveIdentity,
    pub(in crate::signalling::control) procedure: RadioBearerReleaseProcedure,
}
pub(in crate::signalling::control) fn connected() -> Connected {
    let mut c = additional::connected();
    let receipt = additional::publish(&mut c);
    let boundary = c.procedure.activation().unwrap().time().index;
    let response = integrity::protected(
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
        &response,
        boundary,
        ReceiveProvenance::Path(c.new.configuration),
        Some(at(boundary)),
    );
    let mut accepted = None;
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(boundary + 20),
            security_time(),
            |message, mut security| {
                let (_, result) = message.additional_setup_response_with(
                    &mut c.procedure,
                    &mut security,
                    |_, _| Ok(()),
                )?;
                accepted = Some(result.unwrap().0);
                Ok(())
            }
        ),
        Some(Ok(c.key))
    );
    let accepted = accepted.unwrap();
    c.radio
        .commit_additional_setup_with(&accepted, |_| Ok::<_, ()>(()))
        .unwrap();
    let acknowledged = additional::acknowledge(&mut c, receipt, boundary + 50);
    c.radio
        .release_additional_setup_receipt(&accepted, receipt)
        .unwrap();
    assert!(acknowledged < RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME);
    let retained = c.radio.retained_signalling(c.key).unwrap();
    let previous = c.procedure.configuration().clone();
    let next = release_configuration(&retained, false);
    let old = c.new;
    let new = ReceiveIdentity {
        configuration: NonZeroU64::new(old.configuration.get() + 1).unwrap(),
        ..old
    };
    let activation = ScheduledActivation::new(
        at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
        RELEASE_PUBLICATION_FRAME as u8,
        NonZeroU8::new((RELEASE_ACTIVATION_FRAME - RELEASE_PUBLICATION_FRAME) as u8).unwrap(),
    )
    .unwrap();
    let procedure = RadioBearerReleaseProcedure::new(
        c.key,
        c.radio.next_bearer_release_transaction(c.key).unwrap(),
        ExistingDchRelease::new(previous, next, activation).unwrap(),
        at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
        at((RELEASE_PUBLICATION_FRAME + 1) * CHIPS_PER_FRAME),
        NonZeroU64::new(CHIPS_PER_FRAME * 8).unwrap(),
    )
    .unwrap();
    Connected {
        radio: c.radio,
        key: c.key,
        sender: c.sender,
        integrity: c.integrity,
        fresh: c.fresh,
        old,
        new,
        procedure,
    }
}
pub(in crate::signalling::control) fn release_complete(
    transaction: TransactionId,
) -> ULDCCHMessageType {
    ULDCCHMessageType::radioBearerReleaseComplete(pdu::RadioBearerReleaseComplete::new(
        ie::RRCTransactionIdentifier(transaction.get()),
        None,
        None,
        None,
        None,
        None,
        None,
    ))
}
pub(in crate::signalling::control) fn receive(
    c: &mut Connected,
    message: ULDCCHMessageType,
    provenance: ReceiveProvenance,
) {
    uplink_on(
        &mut c.radio,
        c.key,
        &mut c.sender,
        &integrity::protected(&c.integrity, c.fresh, 2, 4, message),
        CHIPS_PER_FRAME * RELEASE_ACTIVATION_FRAME,
        provenance,
    );
}

#[test]
fn actual_protected_release_requires_delimited_publication_and_new_response_then_mints_exact_evidence()
 {
    let mut c = connected();
    let receipt = c
        .radio
        .publish_bearer_release_on(
            &mut c.procedure,
            c.old,
            c.new,
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
        )
        .unwrap();
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
    publish_release(&mut c, receipt);
    let response = release_complete(c.procedure.transaction());
    let provenance = ReceiveProvenance::Path(c.new.configuration);
    receive(&mut c, response, provenance);
    let mut accepted = None;
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(CHIPS_PER_FRAME * RELEASE_ACTIVATION_FRAME + 20),
            security_time(),
            |message, mut security| {
                message
                    .bearer_release_response_with(&mut c.procedure, &mut security, |_, _| {
                        Err::<(), _>(SignallingError::Capacity)
                    })
                    .map(|_| ())
            }
        ),
        Some(Err(SignallingError::Capacity))
    );
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(CHIPS_PER_FRAME * RELEASE_ACTIVATION_FRAME + 30),
            security_time(),
            |message, mut security| {
                let (response, token) = message.bearer_release_response_with(
                    &mut c.procedure,
                    &mut security,
                    |counters, _| {
                        assert_eq!(counters.domain_starts(), Default::default());
                        Ok(7)
                    },
                )?;
                assert_eq!(response, ReconfigurationResponse::Complete);
                accepted = token;
                Ok(())
            }
        ),
        Some(Ok(c.key))
    );
    let (token, result) = accepted.unwrap();
    assert_eq!(result, 7);
    assert_eq!(token.key(), c.key);
    assert_eq!(token.previous_identity(), c.old);
    assert_eq!(token.identity(), c.new);
    assert!(Arc::ptr_eq(token.previous(), c.procedure.previous()));
    assert!(Arc::ptr_eq(
        token.configuration(),
        c.procedure.configuration()
    ));
    assert_eq!(token.activation(), c.procedure.activation());
    assert_eq!(
        token.time().received(),
        at(CHIPS_PER_FRAME * RELEASE_ACTIVATION_FRAME)
    );
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
    uplink_on(
        &mut c.radio,
        c.key,
        &mut c.sender,
        &integrity::protected(
            &c.integrity,
            c.fresh,
            2,
            5,
            release_complete(c.procedure.transaction()),
        ),
        CHIPS_PER_FRAME * RELEASE_ACTIVATION_FRAME + 100,
        ReceiveProvenance::Path(c.new.configuration),
    );
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(CHIPS_PER_FRAME * RELEASE_ACTIVATION_FRAME + 110),
            security_time(),
            |message, mut security| {
                let (response, duplicate) = message.bearer_release_response_with(
                    &mut c.procedure,
                    &mut security,
                    |_, _| -> Result<(), SignallingError> {
                        panic!("duplicate cannot mint another admission")
                    },
                )?;
                assert_eq!(response, ReconfigurationResponse::Duplicate);
                assert!(duplicate.is_none());
                Ok(())
            }
        ),
        Some(Ok(c.key))
    );
    assert_eq!(
        c.radio.release_bearer_release_receipt(&token, receipt),
        Err(SignallingError::Mapping)
    );
    assert_eq!(
        c.radio
            .commit_bearer_release_with(&token, |_| Err::<(), _>(7)),
        Err(BearerReleaseCommitError::Commit(7))
    );
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
    assert_eq!(
        c.radio
            .commit_bearer_release_with(&token, |_| Ok::<_, ()>(11)),
        Ok(11)
    );
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.new));
    assert_eq!(
        c.radio
            .commit_bearer_release_with(&token, |_| -> Result<(), ()> {
                panic!("duplicate commit cannot retire twice")
            }),
        Err(BearerReleaseCommitError::Signalling(
            SignallingError::Mapping
        ))
    );
    assert_eq!(
        c.radio.release_bearer_setup_receipt(c.key, receipt),
        Err(SignallingError::Phase)
    );
    acknowledge(
        &mut c.radio,
        c.key,
        &mut c.sender,
        receipt,
        CHIPS_PER_FRAME * (RELEASE_ACTIVATION_FRAME + 1),
    );
    c.radio
        .release_bearer_release_receipt(&token, receipt)
        .unwrap();
    assert!(c.radio.registry.get(c.key).unwrap().bearer_change.is_none());
    uplink_on(
        &mut c.radio,
        c.key,
        &mut c.sender,
        &integrity::protected(
            &c.integrity,
            c.fresh,
            2,
            6,
            release_complete(c.procedure.transaction()),
        ),
        CHIPS_PER_FRAME * (RELEASE_ACTIVATION_FRAME + 16),
        ReceiveProvenance::Path(c.new.configuration),
    );
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(CHIPS_PER_FRAME * (RELEASE_ACTIVATION_FRAME + 16) + 10),
            security_time(),
            |message, _| message.bearer_release_duplicate(&mut c.procedure, &token)
        ),
        Some(Ok(c.key))
    );
    assert!(c.radio.registry.get(c.key).unwrap().bearer_change.is_none());
    assert_eq!(
        c.radio
            .next_bearer_release_transaction(c.key)
            .unwrap()
            .get(),
        1
    );
}

#[test]
fn queued_release_cannot_accept_completion_before_actual_am_delimitation() {
    let mut c = connected();
    c.radio
        .publish_bearer_release_on(
            &mut c.procedure,
            c.old,
            c.new,
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
        )
        .unwrap();
    let response = release_complete(c.procedure.transaction());
    let provenance = ReceiveProvenance::Path(c.new.configuration);
    receive(&mut c, response, provenance);
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(CHIPS_PER_FRAME * RELEASE_ACTIVATION_FRAME + 10),
            security_time(),
            |message, mut security| {
                message
                    .bearer_release_response_with(
                        &mut c.procedure,
                        &mut security,
                        |_, _| -> Result<(), SignallingError> {
                            panic!("queued release has not reached UE")
                        },
                    )
                    .map(|_| ())
            }
        ),
        Some(Err(SignallingError::Phase))
    );
    assert_eq!(
        c.procedure.phase(),
        umts_rrc::dedicated::ReconfigurationPhase::AwaitingComplete
    );
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
}

#[test]
fn release_publication_rejects_foreign_route_non_successor_and_overlapping_setup() {
    let mut c = connected();
    let transaction = c.procedure.transaction();
    assert_eq!(
        c.radio.publish_bearer_release_on(
            &mut c.procedure,
            c.old,
            c.old,
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME)
        ),
        Err(SignallingError::Origin)
    );
    assert_eq!(
        c.radio.publish_bearer_release_on(
            &mut c.procedure,
            c.new,
            ReceiveIdentity {
                configuration: NonZeroU64::new(c.new.configuration.get() + 1).unwrap(),
                ..c.new
            },
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME)
        ),
        Err(SignallingError::Mapping)
    );
    assert_eq!(
        c.radio.next_bearer_release_transaction(c.key).unwrap(),
        transaction
    );
    c.radio
        .publish_bearer_release_on(
            &mut c.procedure,
            c.old,
            c.new,
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
        )
        .unwrap();
    let mut setup = procedure(&c.radio, c.key);
    assert_eq!(
        c.radio
            .publish_bearer_setup(&mut setup, at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME)),
        Err(SignallingError::Capacity)
    );
    assert_eq!(setup.phase(), SetupPhase::Prepared);
}

#[test]
fn release_publication_waits_for_actual_capability_exchange_without_consuming_transaction() {
    let mut c = connected();
    let transaction = c.procedure.transaction();
    c.radio
        .request_capabilities(
            c.key,
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
            TransactionId::new(0).unwrap(),
        )
        .unwrap();
    assert_eq!(
        c.radio.publish_bearer_release_on(
            &mut c.procedure,
            c.old,
            c.new,
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME)
        ),
        Err(SignallingError::Capacity)
    );
    assert_eq!(c.procedure.receipt(), None);
    assert_eq!(
        c.radio.next_bearer_release_transaction(c.key).unwrap(),
        transaction
    );
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
}

#[test]
fn release_cannot_accept_setup_wrong_transaction_or_old_completion_provenance() {
    for case in 0..3 {
        let mut c = connected();
        let receipt = c
            .radio
            .publish_bearer_release_on(
                &mut c.procedure,
                c.old,
                c.new,
                at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
            )
            .unwrap();
        publish_release(&mut c, receipt);
        let body = match case {
            0 => complete(c.procedure.transaction()),
            1 => release_complete(TransactionId::new(1).unwrap()),
            _ => release_complete(c.procedure.transaction()),
        };
        let provenance = if case == 2 {
            c.old.configuration
        } else {
            c.new.configuration
        };
        receive(&mut c, body, ReceiveProvenance::Path(provenance));
        let result = c.radio.process_one_at_with_control(
            at(CHIPS_PER_FRAME * RELEASE_ACTIVATION_FRAME + 10),
            security_time(),
            |message, mut security| {
                message
                    .bearer_release_response_with(
                        &mut c.procedure,
                        &mut security,
                        |_, _| -> Result<(), SignallingError> {
                            panic!("unmatched response admission")
                        },
                    )
                    .map(|_| ())
            },
        );
        assert!(matches!(
            result,
            Some(Err(
                SignallingError::Mapping | SignallingError::Dedicated(DedicatedError::Transaction)
            ))
        ));
        assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
    }
}

#[test]
fn authenticated_release_failure_before_activation_preserves_original_route_and_requires_recovery()
{
    let mut c = connected();
    let receipt = c
        .radio
        .publish_bearer_release_on(
            &mut c.procedure,
            c.old,
            c.new,
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME),
        )
        .unwrap();
    publish_release(&mut c, receipt);
    let failure = pdu::RadioBearerReleaseFailure::new(
        ie::RRCTransactionIdentifier(c.procedure.transaction().get()),
        ie::FailureCauseWithProtErr::configurationUnsupported(()),
        Some(ie::RBIdentityList(vec![ie::RBIdentity(10)])),
        None,
    );
    uplink_on(
        &mut c.radio,
        c.key,
        &mut c.sender,
        &integrity::protected(
            &c.integrity,
            c.fresh,
            2,
            4,
            ULDCCHMessageType::radioBearerReleaseFailure(failure.clone()),
        ),
        RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME + 1000,
        ReceiveProvenance::Path(c.old.configuration),
    );
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(RELEASE_PUBLICATION_FRAME * CHIPS_PER_FRAME + 1100),
            security_time(),
            |message, mut security| {
                let (response, token) = message.bearer_release_response_with(
                    &mut c.procedure,
                    &mut security,
                    |_, _| -> Result<(), SignallingError> {
                        panic!("failed release must not admit completion")
                    },
                )?;
                assert_eq!(response, ReconfigurationResponse::Failure);
                assert!(token.is_none());
                Ok(())
            }
        ),
        Some(Ok(c.key))
    );
    assert_eq!(c.procedure.failure(), Some(&failure));
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
    assert_eq!(
        c.radio.release_bearer_setup_receipt(c.key, receipt),
        Err(SignallingError::Phase)
    );
    c.radio
        .require_bearer_release_recovery(&c.procedure)
        .unwrap();
    assert!(
        c.radio
            .registry
            .get(c.key)
            .unwrap()
            .protection
            .bearer_reconfiguration_block()
            .is_some()
    );
}
