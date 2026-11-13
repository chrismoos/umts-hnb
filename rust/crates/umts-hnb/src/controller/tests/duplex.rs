// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::{DedicatedBearers, DownlinkFrame, FrameBuildError, SduDelivery},
    dedicated_controller::{DedicatedController, DedicatedControllerLimits},
    dedicated_tx::{DedicatedTpcPort, TransmitRuntimeLimits},
    rel99_admission::{
        AdmissionLimits, DuplexFramePolicy, FrameOpportunity, FramePolicy, Rel99Admission,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeliveryFailure {
    Rejected,
}

#[derive(Default)]
struct Policy {
    selected: usize,
    filled: usize,
    received: usize,
}

impl FramePolicy for Policy {
    fn select(
        &mut self,
        opportunity: FrameOpportunity,
        _: ChipInstant,
        _: &mut DedicatedBearers,
        _: &mut CommonSignalling,
    ) -> Result<Tfci, FrameBuildError> {
        self.selected += 1;
        Ok(opportunity.idle)
    }

    fn fill(
        &mut self,
        _: &mut DownlinkFrame<'_>,
        _: ChipInstant,
        _: &mut CommonSignalling,
    ) -> Result<(), FrameBuildError> {
        self.filled += 1;
        Ok(())
    }
}

impl DuplexFramePolicy for Policy {
    type UplinkError = DeliveryFailure;

    fn uplink(&mut self, _: SduDelivery<'_>) -> Result<bool, Self::UplinkError> {
        self.received += 1;
        Err(DeliveryFailure::Rejected)
    }
}

pub(crate) struct Environment {
    pub(crate) controller: CommonController<Mock>,
    pub(crate) _preparation: TxPreparation,
    _pump: TxPump,
    _receive: CommonRx,
    _hardware: MemoryRadioHandle,
    pub(crate) _peer: Peer,
    _feedback: DedicatedTpcPort,
}

impl Environment {
    pub(crate) fn with_parts<R>(
        &mut self,
        apply: impl FnOnce(
            &mut crate::core_link::CoreLink<Mock>,
            &mut CommonSignalling,
            &crate::common_tx::CommonTxPort,
            &crate::common_tx::CommonTx,
            &Peer,
            &mut Rel99Admission,
        ) -> R,
    ) -> R {
        apply(
            &mut self.controller.core,
            &mut self.controller.radio,
            &self.controller.port,
            self._preparation.source(),
            &self._peer,
            self.controller
                .admission
                .as_mut()
                .expect("paired admission"),
        )
    }
}

pub(crate) fn environment() -> Environment {
    let (mut preparation, pump, mut controller, mut receive, hardware, peer) = setup();
    let port = receive.configure_dedicated(dedicated_limits()).unwrap();
    controller
        .attach_dedicated(DedicatedController::new(
            port,
            DedicatedControllerLimits {
                receipt_visits: count(2),
                service_visits: count(2),
                maximum_bearer_bytes: 2_000_000,
            },
        ))
        .ok()
        .unwrap();
    let ports = preparation
        .dedicated_ports(
            count(2),
            count(4),
            count(2_000_000),
            TransmitRuntimeLimits {
                command_capacity: count(8),
                commands_per_step: count(2),
                feedback_capacity: count(2),
                feedback_per_step: count(2),
                maximum_frame_payload_bits: count(10_000),
                maximum_inflight_retained_bits: count(2_000_000),
            },
        )
        .unwrap();
    let admission = Rel99Admission::new(
        ports.control,
        count(2),
        AdmissionLimits {
            visits: count(2),
            command_results: count(2),
            prefill_frames: count(4),
            maximum_lead_frames: count(8),
            health_interval_chips: NonZeroU64::new(1_000_000).unwrap(),
        },
    )
    .ok()
    .unwrap();
    controller.attach_admission(admission).ok().unwrap();
    assert!(controller.paired_with(&preparation, &receive));
    Environment {
        controller,
        _preparation: preparation,
        _pump: pump,
        _receive: receive,
        _hardware: hardware,
        _peer: peer,
        _feedback: ports.feedback,
    }
}

#[test]
fn duplex_empty_cycles_keep_the_typed_policy_owner_and_service_real_paired_workers() {
    let mut env = environment();
    let mut policy = Policy::default();
    let base = Instant::now();
    for chip in [0, 1, 2] {
        let report: ControllerStep<(), (), DeliveryFailure> = env
            .controller
            .step_with_duplex_policy(
                time(base, chip),
                |_, _| Ok(()),
                |_, _| Err(SignallingError::Message),
                &mut policy,
            )
            .unwrap();
        assert!(report.uplink_failure.is_none());
        assert!(report.core.is_ok());
        assert!(report.security.unwrap().is_ok());
        assert!(report.fach.is_ok());
        assert!(report.timers.is_ok());
        let dedicated = report.dedicated_bearers.unwrap().unwrap();
        assert_eq!(dedicated.serviced, 0);
        assert_eq!(dedicated.delivered, 0);
        assert!(dedicated.failure.is_none());
        let admission = report.admission.unwrap().unwrap();
        assert_eq!(admission.visits, 0);
        assert_eq!(admission.command_results, 0);
    }
    assert_eq!((policy.selected, policy.filled, policy.received), (0, 0, 0));
}

#[test]
fn duplex_epoch_and_time_rejection_precede_policy_callbacks_and_leave_next_cycle_usable() {
    let mut env = environment();
    let mut policy = Policy::default();
    let base = Instant::now();
    env.controller
        .step_with_duplex_policy(
            time(base, 100),
            |_, _| Ok::<(), ()>(()),
            |_, _| Err(SignallingError::Message),
            &mut policy,
        )
        .unwrap();
    assert!(matches!(
        env.controller.step_with_duplex_policy(
            time(base, 99),
            |_, _| Ok::<(), ()>(()),
            |_, _| Err(SignallingError::Message),
            &mut policy
        ),
        Err(ControllerError::Time)
    ));
    let mut foreign = time(base, 101);
    foreign.radio.epoch = umts_types::RadioEpoch::new(NonZeroU64::new(2).unwrap());
    assert!(matches!(
        env.controller.step_with_duplex_policy(
            foreign,
            |_, _| Ok::<(), ()>(()),
            |_, _| Err(SignallingError::Message),
            &mut policy
        ),
        Err(ControllerError::Epoch)
    ));
    assert_eq!((policy.selected, policy.filled, policy.received), (0, 0, 0));
    let report = env
        .controller
        .step_with_duplex_policy(
            time(base, 101),
            |_, _| Ok::<(), ()>(()),
            |_, _| Err(SignallingError::Message),
            &mut policy,
        )
        .unwrap();
    assert!(report.core.is_ok());
    assert!(report.uplink_failure.is_none());
    assert!(report.admission.unwrap().is_ok());
}

#[test]
fn infallible_upper_callback_result_is_kept_through_the_duplex_adapter() {
    let mut env = environment();
    let mut policy = Policy::default();
    let mut callbacks = 0;
    let report: ControllerStep<(), ()> = env
        .controller
        .step_with_frame_policy(
            time(Instant::now(), 0),
            |_, _| Ok(()),
            |_| {
                callbacks += 1;
                false
            },
            |_, _| Err(SignallingError::Message),
            &mut policy,
        )
        .unwrap();
    assert!(report.uplink_failure.is_none());
    assert!(report.core.is_ok());
    assert!(report.admission.unwrap().is_ok());
    assert_eq!(callbacks, 0);
    assert_eq!((policy.selected, policy.filled, policy.received), (0, 0, 0));
}
