// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    packet_rabs::{ModificationError, PacketModification, PacketRabs},
    rab_handoff::PendingRabAssignment,
    rel99_admission::{AdmissionError, InitialPacketError, RabAdmissionError},
};
use umts_iuh::{
    ranap::rab::{Cause, Failed},
    session::{RabOutcome, RabRequest},
};
use umts_user_plane::packet::BoundDatagrams;

const REQUEST_TYPE_NOT_SUPPORTED: u16 = 38;
const RADIO_INTERFACE_FAILURE: u16 = 14;

#[derive(Default)]
pub(crate) struct PacketModificationStep {
    pub modified: usize,
    pub rejected: usize,
}

impl<T: Transport> CommonController<T> {
    pub(crate) fn packet_modification_matches(&self, request: RabRequest, rabs: &[u8]) -> bool {
        self.admission
            .as_ref()
            .is_some_and(|admission| admission.packet_modification_matches(request, rabs))
    }

    pub(crate) fn packet_modification_ready(
        &self,
        identity: crate::dedicated_rx::ReceiveIdentity,
    ) -> bool {
        self.admission
            .as_ref()
            .is_some_and(|admission| admission.committed_radio(identity).is_ok())
    }

    pub(crate) fn modify_packet_rabs<D: BoundDatagrams>(
        &mut self,
        pending: &PendingRabAssignment,
        packets: &mut PacketRabs<D>,
        now: ControlTime,
    ) -> Result<PacketModificationStep, RabAdmissionError> {
        let request = pending.request();
        let identity = self
            .radio
            .dedicated_route(request.key())
            .ok_or(RabAdmissionError::Admission(AdmissionError::Identity))?;
        let admission = self
            .admission
            .as_ref()
            .ok_or(RabAdmissionError::Admission(AdmissionError::Phase))?;
        let plan = admission
            .committed_radio(identity)
            .map_err(RabAdmissionError::Admission)?
            .plan()
            .clone();
        let receive = self
            .dedicated
            .as_mut()
            .ok_or(RabAdmissionError::Admission(AdmissionError::Phase))?;
        let mut report = PacketModificationStep::default();
        for item in &pending.assignment().setup {
            let rab = item.first.id.get();
            match packets.modify_existing(
                &mut self.core,
                receive,
                PacketModification {
                    request,
                    item,
                    identity,
                    plan: &plan,
                    now,
                },
            ) {
                Ok(()) => report.modified += 1,
                Err(ModificationError::Core(error)) => {
                    return Err(RabAdmissionError::Authority(error));
                }
                Err(ModificationError::Cancellation(error)) => {
                    return Err(RabAdmissionError::Packet(InitialPacketError::Packet {
                        rab,
                        error,
                    }));
                }
                Err(ModificationError::Packet(error)) => {
                    let code = if matches!(
                        error,
                        crate::packet_rabs::Error::Mapping | crate::packet_rabs::Error::Unsupported
                    ) {
                        REQUEST_TYPE_NOT_SUPPORTED
                    } else {
                        RADIO_INTERFACE_FAILURE
                    };
                    self.core
                        .gateway_mut()
                        .record_rab_outcome(
                            now,
                            request,
                            RabOutcome::SetupFailed(Failed::new(
                                rab,
                                Cause::from_code(code).expect("standard RANAP cause"),
                            )),
                        )
                        .map_err(RabAdmissionError::Authority)?;
                    tracing::warn!(
                        "Packet RAB modification rejected: request={request:?} rab={rab} error={error:?}"
                    );
                    report.rejected += 1;
                }
            }
        }
        tracing::debug!(
            "Packet RAB modification: request={request:?} identity={identity:?} modified={} rejected={}",
            report.modified,
            report.rejected
        );
        Ok(report)
    }
}
