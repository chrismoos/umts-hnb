// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_rlc::acknowledged::SduReceipt;

fn construction(voice: bool, packet: bool) -> ExistingDchRelease {
    ExistingDchRelease::new(
        fixture::configuration(true, true, 10),
        fixture::configuration(voice, packet, 10),
        ScheduledActivation::new(time(0), 252, NonZeroU8::new(4).unwrap()).unwrap(),
    )
    .unwrap()
}
fn procedure() -> RadioBearerReleaseProcedure {
    RadioBearerReleaseProcedure::new(
        key(),
        transaction(),
        construction(false, true),
        time(0),
        time(CHIPS_PER_FRAME),
        NonZeroU64::new(CHIPS_PER_FRAME * 8).unwrap(),
    )
    .unwrap()
}
fn uplink(value: class::ULDCCHMessageType) -> DecodedUplinkDedicated {
    let bytes = rasn::uper::encode(&class::ULDCCHMessage::new(None, value)).unwrap();
    decode_uplink_dedicated(&bytes, bytes.len() * 8).unwrap()
}
fn complete(id: u8) -> DecodedUplinkDedicated {
    uplink(class::ULDCCHMessageType::radioBearerReleaseComplete(
        pdu::RadioBearerReleaseComplete::new(
            ie::RRCTransactionIdentifier(id),
            None,
            None,
            None,
            None,
            None,
            None,
        ),
    ))
}
fn response_time(index: u64) -> ReceiveTime {
    ReceiveTime::new(time(index), time(index)).unwrap()
}

#[test]
fn release_removes_only_selected_user_bearers_and_keeps_both_domain_connections() {
    for (voice, packet, expected, retained, deleted_ul, deleted_dl) in [
        (
            false,
            true,
            vec![10, 11, 12],
            vec![1, 2, 3, 20],
            vec![1, 2, 3],
            vec![6, 7, 8],
        ),
        (
            true,
            false,
            vec![20],
            vec![1, 2, 3, 10, 11, 12],
            vec![4],
            vec![9],
        ),
        (
            false,
            false,
            vec![10, 11, 12, 20],
            vec![1, 2, 3],
            vec![1, 2, 3, 4],
            vec![6, 7, 8, 9],
        ),
    ] {
        let release = construction(voice, packet);
        assert_eq!(release.released(), expected);
        let encoded = release.message(transaction()).unwrap();
        let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
        let class::DLDCCHMessageType::radioBearerRelease(pdu::RadioBearerRelease::r3(value)) =
            decoded.value.message
        else {
            panic!("Rel99 Release")
        };
        let body = value.radio_bearer_release_r3;
        assert_eq!(
            body.rb_information_release_list
                .0
                .iter()
                .map(|id| id.0)
                .collect::<Vec<_>>(),
            expected
        );
        assert!(body.rb_information_affected_list.is_none());
        assert_eq!(
            release
                .configuration()
                .bearers()
                .iter()
                .map(|rb| rb.id)
                .collect::<Vec<_>>(),
            retained
        );
        assert_eq!(
            body.ul_deleted_trans_ch_info_list
                .unwrap()
                .0
                .iter()
                .map(|trch| trch.ul_transport_channel_identity.0)
                .collect::<Vec<_>>(),
            deleted_ul
        );
        assert_eq!(
            body.dl_deleted_trans_ch_info_list
                .unwrap()
                .0
                .iter()
                .map(|trch| trch.dl_transport_channel_identity.0)
                .collect::<Vec<_>>(),
            deleted_dl
        );
        for transport in body.ul_add_reconf_trans_ch_info_list.unwrap().0 {
            assert_directional_tfs(
                &transport.transport_format_set,
                ie::LogicalChannelList::configured(()),
            );
        }
        for transport in body.dl_add_reconf_trans_ch_info_list.unwrap().0 {
            let ie::DLAddReconfTransChInformation2TfsSignallingMode::explicit_config(tfs) =
                transport.tfs_signalling_mode
            else {
                panic!("missing DL TFS")
            };
            assert_directional_tfs(&tfs, ie::LogicalChannelList::allSizes(()));
        }
        assert_eq!(body.activation_time.unwrap().0, 0);
        assert_eq!(body.rrc_state_indicator, ie::RRCStateIndicator::cell_DCH);
        assert!(body.signalling_connection_rel_indication.is_none());
        assert!(body.integrity_protection_mode_info.is_none());
        assert!(body.ciphering_mode_info.is_none());
        assert!(body.rab_information_reconfig_list.is_none());
        assert!(body.ul_channel_requirement.is_none());
        assert!(body.dl_common_information.is_none());
        assert!(body.dl_information_per_rl_list.is_none());
    }
}

#[test]
fn release_rejects_additions_srb_removal_changed_rlc_and_misaligned_old_tti() {
    let activation = ScheduledActivation::new(time(0), 0, NonZeroU8::new(4).unwrap()).unwrap();
    let previous = fixture::configuration(true, true, 10);
    assert!(matches!(
        ExistingDchRelease::new(previous.clone(), previous.clone(), activation),
        Err(DedicatedError::Bearer)
    ));
    assert!(matches!(
        ExistingDchRelease::new(
            fixture::configuration(true, false, 10),
            fixture::configuration(false, true, 10),
            activation
        ),
        Err(DedicatedError::Bearer)
    ));
    let (ul, dl, mut bearers, physical) = fixture::components(true, false, 10);
    bearers.retain(|bearer| bearer.id != 1);
    let target = Arc::new(Rel99Configuration::new(ul, dl, bearers, physical).unwrap());
    assert!(matches!(
        ExistingDchRelease::new(previous.clone(), target, activation),
        Err(DedicatedError::Bearer)
    ));
    let (ul, dl, mut bearers, physical) = fixture::components(false, true, 10);
    if let Rel99Rlc::Acknowledged { downlink, .. } =
        &mut bearers.iter_mut().find(|rb| rb.id == 20).unwrap().rlc
    {
        downlink.in_sequence_delivery = !downlink.in_sequence_delivery;
    }
    let target = Arc::new(Rel99Configuration::new(ul, dl, bearers, physical).unwrap());
    assert!(matches!(
        ExistingDchRelease::new(previous.clone(), target, activation),
        Err(DedicatedError::Bearer)
    ));
    let misaligned = ScheduledActivation::new(time(0), 0, NonZeroU8::new(1).unwrap()).unwrap();
    assert!(matches!(
        ExistingDchRelease::new(
            previous,
            fixture::configuration(false, true, 10),
            misaligned
        ),
        Err(DedicatedError::Activation)
    ));
}

#[test]
fn release_publication_and_completion_are_retryable_and_exact_duplicates_are_idempotent() {
    let mut procedure = procedure();
    assert_eq!(
        procedure.publish_with(time(1), |_| Err::<SduReceipt, _>("full")),
        Err(PublicationError::Publication("full"))
    );
    assert_eq!(procedure.phase(), ReconfigurationPhase::Prepared);
    assert_eq!(procedure.receipt(), None);
    let tracked = receipt();
    procedure
        .publish_with(time(2), |_| Ok::<_, ()>(tracked))
        .unwrap();
    assert_eq!(procedure.receipt(), Some(tracked));
    let complete = complete(transaction().get());
    let instant = response_time(CHIPS_PER_FRAME * 4);
    assert_eq!(
        procedure.response_with(key(), instant, &complete, |_| Err::<(), _>("pending")),
        Err(ResponseError::Admission("pending"))
    );
    assert_eq!(procedure.phase(), ReconfigurationPhase::AwaitingComplete);
    assert!(procedure.completion().is_none());
    assert!(procedure.completion_counters().is_none());
    assert_eq!(
        procedure.response_with(key(), instant, &complete, |counters| {
            assert_eq!(
                counters.domain_starts(),
                crate::establishment::DomainStarts::default()
            );
            assert_eq!(counters.transparent_cfn(), None);
            Ok::<_, ()>(17)
        }),
        Ok((ReconfigurationResponse::Complete, Some(17)))
    );
    assert_eq!(
        procedure.response_with(key(), instant, &complete, |_| -> Result<(), ()> {
            panic!("duplicate admission")
        }),
        Ok((ReconfigurationResponse::Duplicate, None))
    );
    assert_eq!(procedure.phase(), ReconfigurationPhase::Complete);
}

#[test]
fn release_binds_original_ue_transaction_message_and_activation_time() {
    let mut procedure = procedure();
    procedure
        .publish_with(time(1), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    let valid = complete(transaction().get());
    let early = response_time(CHIPS_PER_FRAME * 4 - 1);
    assert_eq!(
        procedure.response_with(key(), early, &valid, |_| Ok::<_, ()>(())),
        Err(ResponseError::Procedure(DedicatedError::Time))
    );
    let instant = response_time(CHIPS_PER_FRAME * 4);
    let mut foreign = key();
    foreign.generation = NonZeroU64::new(2).unwrap();
    assert_eq!(
        procedure.response_with(foreign, instant, &valid, |_| Ok::<_, ()>(())),
        Err(ResponseError::Procedure(DedicatedError::Identity))
    );
    assert_eq!(
        procedure.response_with(key(), instant, &complete(1), |_| Ok::<_, ()>(())),
        Err(ResponseError::Procedure(DedicatedError::Transaction))
    );
    assert_eq!(
        procedure.response_with(
            key(),
            instant,
            &super::complete(transaction().get()),
            |_| Ok::<_, ()>(())
        ),
        Err(ResponseError::Procedure(DedicatedError::Transaction))
    );
    assert_eq!(procedure.phase(), ReconfigurationPhase::AwaitingComplete);
    assert_eq!(
        procedure.response_with(key(), instant, &valid, |_| Ok::<_, ()>(())),
        Ok((ReconfigurationResponse::Complete, Some(())))
    );
}

#[test]
fn release_failure_does_not_call_admission_and_post_publication_timeout_requires_recovery() {
    let mut failed = procedure();
    failed
        .publish_with(time(1), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    let failure = uplink(class::ULDCCHMessageType::radioBearerReleaseFailure(
        pdu::RadioBearerReleaseFailure::new(
            ie::RRCTransactionIdentifier(transaction().get()),
            ie::FailureCauseWithProtErr::configurationUnsupported(()),
            None,
            None,
        ),
    ));
    let instant = response_time(2);
    assert_eq!(
        failed.response_with(key(), instant, &failure, |_| -> Result<(), ()> {
            panic!("failure admission")
        }),
        Ok((ReconfigurationResponse::Failure, None))
    );
    assert_eq!(failed.phase(), ReconfigurationPhase::FailureReported);
    assert!(failed.failure().is_some());
    assert_eq!(
        failed.response_with(key(), instant, &failure, |_| -> Result<(), ()> {
            panic!("duplicate failure")
        }),
        Ok((ReconfigurationResponse::Duplicate, None))
    );
    let mut expired = procedure();
    assert!(expired.expire(time(CHIPS_PER_FRAME)).unwrap());
    assert_eq!(
        expired.phase(),
        ReconfigurationPhase::AbortedBeforePublication
    );
    let mut expired = procedure();
    expired
        .publish_with(time(1), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    assert!(expired.expire(time(CHIPS_PER_FRAME * 8 + 1)).unwrap());
    assert_eq!(expired.phase(), ReconfigurationPhase::RecoveryRequired);
}

#[test]
fn scheduled_setup_failure_is_valid_on_the_old_configuration_before_activation() {
    let activation = ScheduledActivation::new(time(0), 0, NonZeroU8::new(4).unwrap()).unwrap();
    let setup = ExistingDchSetup::new(
        fixture::configuration(false, true, 10),
        fixture::configuration(true, true, 10),
        fixture::rabs(true, false),
        activation,
    )
    .unwrap();
    let mut procedure = RadioBearerSetupProcedure::existing(
        key(),
        transaction(),
        setup,
        time(0),
        time(CHIPS_PER_FRAME),
        NonZeroU64::new(CHIPS_PER_FRAME * 8).unwrap(),
    )
    .unwrap();
    procedure
        .publish_with(time(1), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    let failure = uplink(class::ULDCCHMessageType::radioBearerSetupFailure(
        pdu::RadioBearerSetupFailure::new(
            ie::RRCTransactionIdentifier(transaction().get()),
            ie::FailureCauseWithProtErr::configurationUnsupported(()),
            None,
            None,
        ),
    ));
    assert_eq!(
        procedure.response(key(), response_time(2), &failure),
        Ok(SetupResponse::Failure)
    );
    assert_eq!(procedure.phase(), SetupPhase::FailureReported);
}

#[test]
fn scheduled_release_rejects_impossible_response_window_and_waits_for_a_usable_publication_time() {
    assert!(matches!(
        RadioBearerReleaseProcedure::new(
            key(),
            transaction(),
            construction(false, true),
            time(0),
            time(CHIPS_PER_FRAME),
            NonZeroU64::new(CHIPS_PER_FRAME).unwrap()
        ),
        Err(DedicatedError::Time)
    ));
    let mut procedure = RadioBearerReleaseProcedure::new(
        key(),
        transaction(),
        construction(false, true),
        time(0),
        time(CHIPS_PER_FRAME * 3),
        NonZeroU64::new(CHIPS_PER_FRAME * 2).unwrap(),
    )
    .unwrap();
    assert_eq!(
        procedure.publish_with(time(1), |_| -> Result<SduReceipt, ()> {
            panic!("impossible response deadline")
        }),
        Err(PublicationError::Procedure(DedicatedError::Time))
    );
    assert_eq!(procedure.phase(), ReconfigurationPhase::Prepared);
    assert!(procedure.receipt().is_none());
    procedure
        .publish_with(time(CHIPS_PER_FRAME * 2 + 1), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    assert_eq!(
        procedure.response_with(
            key(),
            response_time(CHIPS_PER_FRAME * 4),
            &complete(transaction().get()),
            |_| Ok::<_, ()>(())
        ),
        Ok((ReconfigurationResponse::Complete, Some(())))
    );
}

#[test]
fn release_rejects_a_response_received_before_its_actual_publication() {
    let mut procedure = procedure();
    procedure
        .publish_with(time(10), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    let failure = uplink(class::ULDCCHMessageType::radioBearerReleaseFailure(
        pdu::RadioBearerReleaseFailure::new(
            ie::RRCTransactionIdentifier(transaction().get()),
            ie::FailureCauseWithProtErr::configurationUnsupported(()),
            None,
            None,
        ),
    ));
    assert_eq!(
        procedure.response_with(
            key(),
            ReceiveTime::new(time(9), time(10)).unwrap(),
            &failure,
            |_| Ok::<_, ()>(())
        ),
        Err(ResponseError::Procedure(DedicatedError::Time))
    );
    assert_eq!(procedure.phase(), ReconfigurationPhase::AwaitingComplete);
}
