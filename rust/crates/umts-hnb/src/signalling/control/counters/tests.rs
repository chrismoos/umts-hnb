// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_rx::ReceiveIdentity;
use crate::signalling::control::tests::{
    additional, configuration, install_integrity, publish, ready, release, security_time,
    uplink_on, uplink_on_air, user_setup,
};
use crate::test_support::{at, integrity, rel99 as fixture};
use rasn::types::FixedBitString;
use std::num::NonZeroU64;
use umts_rlc::acknowledged::ReceiveProvenance;
use umts_rrc::dedicated::{InitialDchSetup, SetupPhase};
use umts_rrc_schema::class_definitions::ULDCCHMessage;
use umts_security::{
    Key,
    ciphering::{BearerCipherConfiguration, Configuration, CountAnchor, Position, Scope},
    contexts::{CipherMaterial, KeySet},
    counters::{CipherMode, Start},
};
use umts_types::Direction;

fn retain_cipher(radio: &mut CommonSignalling, key: UeKey) {
    let origin = KeyOrigin {
        ue: key,
        domain: CnDomain::Ps,
        generation: NonZeroU64::MIN,
    };
    let context = KeySet::uia1(
        origin,
        CipherMaterial::Uea1(Key::new([0x37; 16])),
        Key::new([0x19; 16]),
    )
    .split()
    .0;
    let anchor = CountAnchor::from_start(
        CipherMode::Acknowledged,
        Position::Sequence(0),
        Start::new(7).unwrap(),
    )
    .unwrap();
    radio
        .registry
        .get_mut(key)
        .unwrap()
        .protection
        .ciphers
        .register_with(
            BearerCipherConfiguration {
                bearer: 20,
                scope: Scope::User(CnDomain::Ps),
                initial: Configuration::Configured(context),
                transmit: anchor,
                receive: anchor,
            },
            || Ok::<_, ()>(()),
        )
        .unwrap();
}

fn cipher(radio: &CommonSignalling, key: UeKey) -> (umts_security::ciphering::CipherUse, [u8; 4]) {
    let bank = &radio.registry.get(key).unwrap().protection.ciphers;
    let position = Position::Sequence(4097);
    let selected = bank
        .selected(key, 20, Direction::Downlink, position)
        .unwrap();
    let mut bytes = [0x80, 0x08, 0x5a, 0xa5];
    bank.apply(key, 20, Direction::Downlink, position, &mut bytes, 32)
        .unwrap();
    (selected, bytes)
}

fn value(start: u32) -> ie::STARTValue {
    let mut bits = FixedBitString::default();
    for bit in 0..20 {
        bits.set(bit, start & (1 << (19 - bit)) != 0);
    }
    ie::STARTValue(bits)
}

fn response(transaction: TransactionId, starts: DomainStarts) -> ULDCCHMessageType {
    let mut message = user_setup::response(transaction, 0x12345);
    let ULDCCHMessageType::radioBearerSetupComplete(body) = &mut message else {
        unreachable!()
    };
    let mut values = Vec::new();
    if let Some(start) = starts.cs {
        values.push(ie::STARTSingle::new(
            ie::CNDomainIdentity::cs_domain,
            value(start),
        ));
    }
    if let Some(start) = starts.ps {
        values.push(ie::STARTSingle::new(
            ie::CNDomainIdentity::ps_domain,
            value(start),
        ));
    }
    body.ul_counter_synchronisation_info = (!values.is_empty())
        .then(|| ie::ULCounterSynchronisationInfo::new(None, ie::STARTList(values)));
    message
}

fn starts(radio: &CommonSignalling, key: UeKey) -> DomainStarts {
    radio
        .registry
        .get(key)
        .unwrap()
        .router
        .as_ref()
        .unwrap()
        .starts()
}

#[test]
fn authenticated_initial_response_merges_domains_only_after_admission_and_keeps_publication_snapshot()
 {
    for via_bearer_setup_response in [false, true] {
        for updates in [
            DomainStarts::default(),
            DomainStarts {
                cs: None,
                ps: Some(0x65432),
            },
            DomainStarts {
                cs: Some(0x54321),
                ps: Some(0x65432),
            },
        ] {
            let (_source, mut radio, key, mut sender) = ready(false);
            let (integrity, fresh) = install_integrity(&mut radio, key);
            retain_cipher(&mut radio, key);
            let cipher_before = cipher(&radio, key);
            let before = starts(&radio, key);
            let retained = radio.retained_signalling(key).unwrap();
            let configuration = configuration(&retained);
            let mut procedure = RadioBearerSetupProcedure::initial(
                key,
                radio.next_bearer_setup_transaction(key).unwrap(),
                InitialDchSetup::new(configuration, retained, fixture::rabs(true, false)).unwrap(),
                at(0),
                at(1000),
                NonZeroU64::new(1_000_000).unwrap(),
            )
            .unwrap();
            let identity = ReceiveIdentity {
                ue: key,
                configuration: NonZeroU64::new(2).unwrap(),
            };
            let receipt = radio
                .publish_bearer_setup_on(&mut procedure, identity, at(0))
                .unwrap();
            publish(&mut radio, key, receipt, &mut sender);
            let expected = DomainStarts {
                cs: Some(updates.cs.unwrap_or(0x12345)),
                ps: updates.ps.or(before.ps),
            };
            let bytes = integrity::protected(
                &integrity,
                fresh,
                2,
                2,
                response(procedure.transaction(), updates),
            );
            uplink_on(
                &mut radio,
                key,
                &mut sender,
                &bytes,
                100,
                ReceiveProvenance::Path(identity.configuration),
            );
            if !via_bearer_setup_response {
                assert_eq!(
                    radio.process_one_at_with_control(
                        at(130),
                        security_time(),
                        |received, mut security| {
                            assert_eq!(security.starts(), before);
                            received
                                .bearer_setup_response_with(
                                    &mut procedure,
                                    &mut security,
                                    |accepted, _| {
                                        assert_eq!(accepted.prior_starts(), before);
                                        assert_eq!(
                                            accepted.counters().new_bearer_start().unwrap().get(),
                                            0x12345
                                        );
                                        Err::<(), _>(SignallingError::Capacity)
                                    },
                                )
                                .map(|_| ())
                        }
                    ),
                    Some(Err(SignallingError::Capacity))
                );
                assert_eq!(starts(&radio, key), before);
                assert_eq!(cipher(&radio, key), cipher_before);
                assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
            }
            assert_eq!(
                radio.process_one_at_with_control(
                    at(140),
                    security_time(),
                    |received, mut security| {
                        let accepted = if via_bearer_setup_response {
                            received.bearer_setup_response(&mut procedure, &mut security)?;
                            received.accepted_user_setup(&procedure, &security)?
                        } else {
                            received
                                .bearer_setup_response_with(
                                    &mut procedure,
                                    &mut security,
                                    |accepted, _| Ok(accepted),
                                )?
                                .1
                                .unwrap()
                        };
                        assert_eq!(accepted.prior_starts(), before);
                        assert_eq!(
                            accepted.counters().new_bearer_start().unwrap().get(),
                            0x12345
                        );
                        assert_eq!(security.starts(), expected);
                        Ok(())
                    }
                ),
                Some(Ok(key))
            );
            assert_eq!(starts(&radio, key), expected);
            assert_eq!(cipher(&radio, key), cipher_before);
        }
    }
}

#[test]
fn clear_start_response_requires_original_unprotected_publication_and_admission_success() {
    for protected in [false, true] {
        let (_source, mut radio, key, mut sender) = ready(false);
        if protected {
            install_integrity(&mut radio, key);
        }
        let before = starts(&radio, key);
        let retained = radio.retained_signalling(key).unwrap();
        let mut procedure = RadioBearerSetupProcedure::initial(
            key,
            radio.next_bearer_setup_transaction(key).unwrap(),
            InitialDchSetup::new(
                configuration(&retained),
                retained,
                fixture::rabs(true, false),
            )
            .unwrap(),
            at(0),
            at(1000),
            NonZeroU64::new(1_000_000).unwrap(),
        )
        .unwrap();
        let identity = ReceiveIdentity {
            ue: key,
            configuration: NonZeroU64::new(2).unwrap(),
        };
        let receipt = radio
            .publish_bearer_setup_on(&mut procedure, identity, at(0))
            .unwrap();
        publish(&mut radio, key, receipt, &mut sender);
        let bytes = rasn::uper::encode(&ULDCCHMessage::new(
            None,
            response(
                procedure.transaction(),
                DomainStarts {
                    cs: Some(10),
                    ps: None,
                },
            ),
        ))
        .unwrap();
        uplink_on(
            &mut radio,
            key,
            &mut sender,
            &bytes,
            100,
            ReceiveProvenance::Path(identity.configuration),
        );
        let mut calls = 0;
        let result = radio.process_one_at_with_control(
            at(130),
            security_time(),
            |received, mut security| {
                received
                    .bearer_setup_response_with(&mut procedure, &mut security, |_, _| {
                        assert!(
                            !protected,
                            "protected publication accepted a clear downgrade"
                        );
                        calls += 1;
                        Err::<(), _>(SignallingError::Capacity)
                    })
                    .map(|_| ())
            },
        );
        assert_eq!(
            result,
            Some(Err(if protected {
                SignallingError::Nas(NasError::SecurityRequired)
            } else {
                SignallingError::Capacity
            }))
        );
        assert_eq!(calls, usize::from(!protected));
        assert_eq!(starts(&radio, key), before);
        assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
        if !protected {
            assert_eq!(
                radio.process_one_at_with_control(
                    at(140),
                    security_time(),
                    |received, mut security| {
                        received.bearer_setup_response_with(
                            &mut procedure,
                            &mut security,
                            |accepted, _| {
                                assert_eq!(accepted.prior_starts(), before);
                                assert_eq!(
                                    accepted.counters().new_bearer_start().unwrap().get(),
                                    0x12345
                                );
                                Ok(())
                            },
                        )?;
                        assert_eq!(
                            security.starts(),
                            DomainStarts {
                                cs: Some(10),
                                ps: before.ps
                            }
                        );
                        Ok(())
                    }
                ),
                Some(Ok(key))
            );
            assert_eq!(procedure.phase(), SetupPhase::Complete);
        }
    }
}

#[test]
fn authenticated_response_cannot_replace_the_publication_integrity_generation() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let (_, fresh) = install_integrity(&mut radio, key);
    let before = starts(&radio, key);
    let retained = radio.retained_signalling(key).unwrap();
    let mut procedure = RadioBearerSetupProcedure::initial(
        key,
        radio.next_bearer_setup_transaction(key).unwrap(),
        InitialDchSetup::new(
            configuration(&retained),
            retained,
            fixture::rabs(true, false),
        )
        .unwrap(),
        at(0),
        at(1000),
        NonZeroU64::new(1_000_000).unwrap(),
    )
    .unwrap();
    let identity = ReceiveIdentity {
        ue: key,
        configuration: NonZeroU64::new(2).unwrap(),
    };
    let receipt = radio
        .publish_bearer_setup_on(&mut procedure, identity, at(0))
        .unwrap();
    publish(&mut radio, key, receipt, &mut sender);
    let replacement = KeySet::uia1(
        KeyOrigin {
            ue: key,
            domain: CnDomain::Cs,
            generation: NonZeroU64::new(2).unwrap(),
        },
        CipherMaterial::Clear,
        Key::new([0x31; 16]),
    )
    .split()
    .1;
    radio.registry.get_mut(key).unwrap().protection.integrity =
        integrity::initial_schedule(&replacement, fresh);
    let bytes = integrity::protected(
        &replacement,
        fresh,
        2,
        2,
        response(procedure.transaction(), DomainStarts::default()),
    );
    uplink_on(
        &mut radio,
        key,
        &mut sender,
        &bytes,
        100,
        ReceiveProvenance::Path(identity.configuration),
    );
    assert_eq!(radio.process_one_at_with_control(at(130),security_time(),|received,mut security| {
        assert!(matches!(received.protection(),ControlProtection::Integrity {origin,..} if origin==replacement.origin()));
        received.bearer_setup_response_with(&mut procedure,&mut security,|_,_| -> Result<(),SignallingError> {panic!("foreign publication integrity owner")}).map(|_|())
    }),Some(Err(SignallingError::Mapping)));
    assert_eq!(starts(&radio, key), before);
    assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
}

#[test]
fn authenticated_scheduled_response_preserves_peer_domain_and_retries_cache_atomically() {
    for updates in [
        DomainStarts::default(),
        DomainStarts {
            cs: Some(0x56789),
            ps: None,
        },
        DomainStarts {
            cs: None,
            ps: Some(0x45678),
        },
    ] {
        let mut c = additional::connected();
        let before = starts(&c.radio, c.key);
        let expected = DomainStarts {
            cs: updates.cs.or(before.cs),
            ps: Some(updates.ps.unwrap_or(0x12345)),
        };
        let receipt = additional::publish(&mut c);
        let boundary = c.procedure.activation().unwrap().time().index;
        let bytes = integrity::protected(
            &c.integrity,
            c.fresh,
            2,
            3,
            response(c.procedure.transaction(), updates),
        );
        uplink_on_air(
            &mut c.radio,
            c.key,
            &mut c.sender,
            &bytes,
            boundary,
            ReceiveProvenance::Path(c.new.configuration),
            Some(at(boundary)),
        );
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
                                assert_eq!(accepted.prior_starts(), before);
                                Err::<(), _>(SignallingError::Capacity)
                            },
                        )
                        .map(|_| ())
                }
            ),
            Some(Err(SignallingError::Capacity))
        );
        assert_eq!(starts(&c.radio, c.key), before);
        let mut evidence = None;
        assert_eq!(
            c.radio.process_one_at_with_control(
                at(boundary + 30),
                security_time(),
                |received, mut security| {
                    let (_, accepted) = received.additional_setup_response_with(
                        &mut c.procedure,
                        &mut security,
                        |_, _| Ok(()),
                    )?;
                    let accepted = accepted.unwrap().0;
                    assert_eq!(accepted.prior_starts(), before);
                    evidence = Some(accepted);
                    assert_eq!(security.starts(), expected);
                    Ok(())
                }
            ),
            Some(Ok(c.key))
        );
        assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
        let accepted = evidence.unwrap();
        c.radio
            .commit_additional_setup_with(&accepted, |_| Ok::<_, ()>(()))
            .unwrap();
        let acknowledged_at = additional::acknowledge(&mut c, receipt, boundary + 40);
        let duplicate_at = acknowledged_at.checked_add(10).unwrap();
        c.radio
            .release_additional_setup_receipt(&accepted, receipt)
            .unwrap();
        let bytes = integrity::protected(
            &c.integrity,
            c.fresh,
            2,
            4,
            response(c.procedure.transaction(), updates),
        );
        uplink_on_air(
            &mut c.radio,
            c.key,
            &mut c.sender,
            &bytes,
            duplicate_at,
            ReceiveProvenance::Path(c.new.configuration),
            Some(at(duplicate_at)),
        );
        assert_eq!(
            c.radio.process_one_at_with_control(
                at(duplicate_at + 20),
                security_time(),
                |received, security| {
                    received.additional_setup_duplicate(&mut c.procedure, &accepted)?;
                    assert_eq!(security.starts(), expected);
                    Ok(())
                }
            ),
            Some(Ok(c.key))
        );
    }
}

#[test]
fn authenticated_release_domain_start_is_cached_after_callback_without_cipher_reset() {
    let mut c = release::connected();
    retain_cipher(&mut c.radio, c.key);
    let cipher_before = cipher(&c.radio, c.key);
    let before = starts(&c.radio, c.key);
    let receipt = c
        .radio
        .publish_bearer_release_on(
            &mut c.procedure,
            c.old,
            c.new,
            at(release::RELEASE_PUBLICATION_FRAME * umts_types::time::CHIPS_PER_FRAME),
        )
        .unwrap();
    release::publish_release(&mut c, receipt);
    let mut response = release::release_complete(c.procedure.transaction());
    let ULDCCHMessageType::radioBearerReleaseComplete(body) = &mut response else {
        unreachable!()
    };
    body.ul_counter_synchronisation_info = Some(ie::ULCounterSynchronisationInfo::new(
        None,
        ie::STARTList(vec![ie::STARTSingle::new(
            ie::CNDomainIdentity::cs_domain,
            value(0x32123),
        )]),
    ));
    let path = ReceiveProvenance::Path(c.new.configuration);
    release::receive(&mut c, response, path);
    let boundary = c.procedure.activation().time().index;
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(boundary + 20),
            security_time(),
            |received, mut security| {
                received
                    .bearer_release_response_with(&mut c.procedure, &mut security, |counters, _| {
                        assert_eq!(
                            counters.domain_starts(),
                            DomainStarts {
                                cs: Some(0x32123),
                                ps: None
                            }
                        );
                        Err::<(), _>(SignallingError::Capacity)
                    })
                    .map(|_| ())
            }
        ),
        Some(Err(SignallingError::Capacity))
    );
    assert_eq!(starts(&c.radio, c.key), before);
    assert_eq!(cipher(&c.radio, c.key), cipher_before);
    assert_eq!(
        c.radio.process_one_at_with_control(
            at(boundary + 30),
            security_time(),
            |received, mut security| {
                received.bearer_release_response_with(
                    &mut c.procedure,
                    &mut security,
                    |_, _| Ok(()),
                )?;
                assert_eq!(
                    security.starts(),
                    DomainStarts {
                        cs: Some(0x32123),
                        ps: before.ps
                    }
                );
                Ok(())
            }
        ),
        Some(Ok(c.key))
    );
    assert_eq!(cipher(&c.radio, c.key), cipher_before);
    assert_eq!(c.radio.dedicated_route(c.key), Some(c.old));
}
