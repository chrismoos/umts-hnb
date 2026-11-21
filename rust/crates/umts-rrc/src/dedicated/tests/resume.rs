// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    establishment::DomainStarts,
    mobility::{CellUpdate, UpdateCause, UpdateFailure, UpdateFailureCause},
};
use rasn::types::FixedBitString;

const PS_START: u32 = 0x54321;

fn procedure() -> RadioBearerSetupProcedure {
    RadioBearerSetupProcedure::resume(
        key(),
        transaction(),
        initial(false, true),
        time(0),
        time(100),
        NonZeroU64::new(1000).unwrap(),
    )
    .unwrap()
}

fn uplink(value: class::ULDCCHMessageType) -> DecodedUplinkDedicated {
    let bytes = rasn::uper::encode(&class::ULDCCHMessage::new(None, value)).unwrap();
    decode_uplink_dedicated(&bytes, bytes.len() * 8).unwrap()
}

fn reconfiguration_complete(id: u8, start: Option<u32>) -> DecodedUplinkDedicated {
    let starts = start.map(|value| {
        let mut bits = FixedBitString::<20>::default();
        for index in 0..20 {
            bits.set(index, value & (1 << (19 - index)) != 0);
        }
        ie::ULCounterSynchronisationInfo::new(
            None,
            ie::STARTList(vec![ie::STARTSingle::new(
                ie::CNDomainIdentity::ps_domain,
                ie::STARTValue(bits),
            )]),
        )
    });
    uplink(
        class::ULDCCHMessageType::radioBearerReconfigurationComplete(
            pdu::RadioBearerReconfigurationComplete::new(
                ie::RRCTransactionIdentifier(id),
                None,
                None,
                None,
                None,
                starts,
                None,
            ),
        ),
    )
}

fn reconfiguration_failure(id: u8) -> DecodedUplinkDedicated {
    uplink(class::ULDCCHMessageType::radioBearerReconfigurationFailure(
        pdu::RadioBearerReconfigurationFailure::new(
            ie::RRCTransactionIdentifier(id),
            ie::FailureCauseWithProtErr::physicalChannelFailure(()),
            None,
            None,
        ),
    ))
}

fn cell_update(failure: Option<UpdateFailure>) -> CellUpdate {
    CellUpdate {
        urnti: 1,
        starts: DomainStarts {
            cs: None,
            ps: Some(PS_START),
        },
        cause: UpdateCause::RadioLinkFailure,
        signalling_rlc_error: false,
        user_rlc_error: false,
        failure,
    }
}

#[test]
fn a_configuration_with_a_circuit_bearer_cannot_be_resumed() {
    let setup = initial(true, false);
    assert_eq!(
        setup.resume_reconfiguration(transaction()),
        Err(DedicatedError::Bearer)
    );
    assert!(matches!(
        RadioBearerSetupProcedure::resume(
            key(),
            transaction(),
            setup,
            time(0),
            time(100),
            NonZeroU64::new(1000).unwrap(),
        ),
        Err(DedicatedError::Bearer)
    ));
}

#[test]
fn resume_publishes_the_reconfiguration_and_completes_on_its_complete_with_starts() {
    let mut procedure = procedure();
    assert_eq!(procedure.mode(), SetupMode::Resume);
    assert_eq!(
        *procedure.encoded(),
        initial(false, true)
            .resume_reconfiguration(transaction())
            .unwrap()
    );
    assert!(procedure.initial_hsdpa().is_none());
    assert!(procedure.previous().is_none() && procedure.activation().is_none());
    assert_eq!(procedure.rabs(), fixture::rabs(false, true));
    procedure
        .publish_with(time(1), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
    assert_eq!(
        procedure.response(key(), ReceiveTime::simultaneous(time(2)), &complete(2)),
        Err(DedicatedError::Transaction)
    );
    assert_eq!(
        procedure.response(
            key(),
            ReceiveTime::simultaneous(time(2)),
            &reconfiguration_complete(1, None)
        ),
        Err(DedicatedError::Transaction)
    );
    assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
    let response = reconfiguration_complete(2, Some(PS_START));
    assert_eq!(
        procedure.response(key(), ReceiveTime::simultaneous(time(3)), &response),
        Ok(SetupResponse::Complete)
    );
    assert_eq!(procedure.phase(), SetupPhase::Complete);
    assert!(procedure.completion().is_none());
    assert_eq!(
        procedure
            .resume_completion()
            .unwrap()
            .rrc_transaction_identifier
            .0,
        2
    );
    let counters = procedure.completion_counters().unwrap();
    assert!(counters.new_bearer_start().is_none());
    assert_eq!(
        counters.domain_starts(),
        DomainStarts {
            cs: None,
            ps: Some(PS_START)
        }
    );
    assert_eq!(
        procedure.response(key(), ReceiveTime::simultaneous(time(4)), &response),
        Ok(SetupResponse::Duplicate)
    );
    assert_eq!(
        procedure.cell_update_failure(
            key(),
            time(5),
            &cell_update(Some(UpdateFailure {
                transaction: 2,
                cause: UpdateFailureCause::PhysicalChannelFailure
            }))
        ),
        Err(DedicatedError::Phase)
    );
}

#[test]
fn resume_completion_without_a_start_list_reports_no_domain_starts() {
    let mut procedure = procedure();
    procedure
        .publish_with(time(1), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    assert_eq!(
        procedure.response(
            key(),
            ReceiveTime::simultaneous(time(2)),
            &reconfiguration_complete(2, None)
        ),
        Ok(SetupResponse::Complete)
    );
    assert_eq!(
        procedure.completion_counters().unwrap().domain_starts(),
        DomainStarts::default()
    );
}

#[test]
fn resume_fails_on_reconfiguration_failure_with_the_cause_kept() {
    let mut procedure = procedure();
    procedure
        .publish_with(time(1), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    let failure = reconfiguration_failure(2);
    assert_eq!(
        procedure.response(key(), ReceiveTime::simultaneous(time(2)), &failure),
        Ok(SetupResponse::Failure)
    );
    assert_eq!(procedure.phase(), SetupPhase::FailureReported);
    assert!(procedure.failure().is_none());
    assert_eq!(
        procedure.resume_failure().unwrap().failure_cause,
        ie::FailureCauseWithProtErr::physicalChannelFailure(())
    );
    assert!(procedure.reported_failure().is_none());
    assert_eq!(
        procedure.response(key(), ReceiveTime::simultaneous(time(3)), &failure),
        Ok(SetupResponse::Duplicate)
    );
}

#[test]
fn resume_fails_on_a_cell_update_that_reports_the_confirm_transaction() {
    let failure = |transaction| {
        cell_update(Some(UpdateFailure {
            transaction,
            cause: UpdateFailureCause::PhysicalChannelFailure,
        }))
    };
    let mut procedure = procedure();
    assert_eq!(
        procedure.cell_update_failure(key(), time(1), &failure(2)),
        Err(DedicatedError::Phase)
    );
    procedure
        .publish_with(time(1), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    assert_eq!(
        procedure.cell_update_failure(key(), time(2), &cell_update(None)),
        Ok(false)
    );
    assert_eq!(
        procedure.cell_update_failure(key(), time(2), &failure(1)),
        Ok(false)
    );
    let stale = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..key()
    };
    assert_eq!(
        procedure.cell_update_failure(stale, time(2), &failure(2)),
        Err(DedicatedError::Identity)
    );
    assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
    assert_eq!(
        procedure.cell_update_failure(key(), time(3), &failure(2)),
        Ok(true)
    );
    assert_eq!(procedure.phase(), SetupPhase::FailureReported);
    assert_eq!(
        procedure.reported_failure(),
        Some(UpdateFailureCause::PhysicalChannelFailure)
    );
    assert!(procedure.resume_failure().is_none());
    assert_eq!(
        procedure.cell_update_failure(key(), time(4), &failure(2)),
        Ok(true)
    );
    assert_eq!(
        procedure.response(
            key(),
            ReceiveTime::simultaneous(time(5)),
            &reconfiguration_complete(2, None)
        ),
        Err(DedicatedError::Phase)
    );
}

#[test]
fn a_cell_update_failure_never_fails_a_radio_bearer_setup() {
    let mut procedure = RadioBearerSetupProcedure::initial(
        key(),
        transaction(),
        initial(false, true),
        time(0),
        time(100),
        NonZeroU64::new(1000).unwrap(),
    )
    .unwrap();
    assert_eq!(procedure.mode(), SetupMode::Setup);
    procedure
        .publish_with(time(1), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    assert_eq!(
        procedure.cell_update_failure(
            key(),
            time(2),
            &cell_update(Some(UpdateFailure {
                transaction: 2,
                cause: UpdateFailureCause::InvalidConfiguration
            }))
        ),
        Ok(false)
    );
    assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
    assert_eq!(
        procedure.response(
            key(),
            ReceiveTime::simultaneous(time(3)),
            &reconfiguration_complete(2, None)
        ),
        Err(DedicatedError::Transaction)
    );
    assert!(procedure.resume_completion().is_none() && procedure.resume_failure().is_none());
}

#[test]
fn resume_reconfiguration_moves_existing_bearers_to_cell_dch_without_deleting_channels() {
    use umts_rrc_codec::decode_downlink_dedicated;
    use umts_rrc_schema::class_definitions::DLDCCHMessageType;
    let encoded = initial(false, true)
        .resume_reconfiguration(transaction())
        .unwrap();
    let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
    let DLDCCHMessageType::radioBearerReconfiguration(pdu::RadioBearerReconfiguration::r3(message)) =
        decoded.value.message
    else {
        panic!("not an R3 reconfiguration")
    };
    let ies = message.radio_bearer_reconfiguration_r3;
    assert_eq!(ies.rrc_state_indicator, ie::RRCStateIndicator::cell_DCH);
    assert!(ies.activation_time.is_none() && ies.new_c_rnti.is_none());
    assert!(ies.ul_deleted_trans_ch_info_list.is_none());
    assert!(ies.dl_deleted_trans_ch_info_list.is_none());
    assert!(ies.rb_information_reconfig_list.0.iter().all(|bearer| {
        bearer.rlc_info.is_none()
            && bearer
                .rb_mapping_info
                .as_ref()
                .is_some_and(|mapping| mapping.0.len() == 2)
    }));
    assert!(ies.dl_common_information.is_some());
    assert!(!ies.dl_information_per_rl_list.0.is_empty());
}
