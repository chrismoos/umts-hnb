// SPDX-License-Identifier: AGPL-3.0-or-later

use super::PendingRabAssignment;
use crate::core_link::CoreLink;
use std::num::NonZeroUsize;
use umts_core::{Gateway, Transport};
use umts_iuh::{
    ranap::rab::{Cause, Failed},
    session::{ControlTime, RabOperation, RabOutcome, RabRequest},
};

#[derive(Debug)]
pub enum FailureDispositionError {
    Capacity,
    Protocol(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        umts_iuh::Error,
    ),
    Authority,
}

#[must_use]
pub struct RejectedFailureDisposition {
    pub error: FailureDispositionError,
    pub pending: PendingRabAssignment,
}

impl std::fmt::Debug for RejectedFailureDisposition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RejectedFailureDisposition")
            .field("error", &self.error)
            .field("request", &self.pending.request())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureDisposition {
    Pending,
    Recorded,
    Superseded,
}

struct Item {
    rab: u8,
    operation: RabOperation,
    disposition: FailureDisposition,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RejectionStep {
    pub visited: usize,
    pub recorded: usize,
    pub superseded: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg(test)]
pub struct RejectionProgress {
    pub items: usize,
    pub recorded: usize,
    pub superseded: usize,
}

#[must_use]
pub struct FailedRabAssignment {
    pending: PendingRabAssignment,
    cause: Cause,
    items: Vec<Item>,
    cursor: usize,
}

impl FailedRabAssignment {
    pub fn new<T: Transport>(
        mut pending: PendingRabAssignment,
        cause: Cause,
        gateway: &Gateway<T>,
    ) -> Result<Self, Box<RejectedFailureDisposition>> {
        let prepared = (|| {
            pending
                .assignment()
                .validate()
                .map_err(FailureDispositionError::Protocol)?;
            Cause::from_code(cause.code()).map_err(FailureDispositionError::Protocol)?;
            let request = pending.request();
            if !gateway.same_rab_owner(request)
                || (!pending.is_committed() && !gateway.has_pending_rab_request(request))
            {
                return Err(FailureDispositionError::Authority);
            }
            let assignment = pending.assignment();
            let count = assignment
                .setup
                .len()
                .checked_add(assignment.release.len())
                .filter(|count| *count != 0)
                .ok_or(FailureDispositionError::Capacity)?;
            let mut items = Vec::new();
            items
                .try_reserve_exact(count)
                .map_err(|_| FailureDispositionError::Capacity)?;
            items.extend(assignment.setup.iter().map(|item| Item {
                rab: item.first.id.get(),
                operation: RabOperation::SetupOrModify,
                disposition: FailureDisposition::Pending,
            }));
            items.extend(assignment.release.iter().map(|item| Item {
                rab: item.id.get(),
                operation: RabOperation::Release,
                disposition: FailureDisposition::Pending,
            }));
            Ok(items)
        })();
        match prepared {
            Ok(items) => {
                pending.confirm(gateway);
                Ok(Self {
                    pending,
                    cause,
                    items,
                    cursor: 0,
                })
            }
            Err(error) => Err(Box::new(RejectedFailureDisposition { error, pending })),
        }
    }

    pub fn request(&self) -> RabRequest {
        self.pending.request()
    }

    #[cfg(test)]
    pub fn into_pending(self) -> PendingRabAssignment {
        self.pending
    }

    #[cfg(test)]
    pub fn disposition(&self, rab: u8) -> Option<FailureDisposition> {
        self.items
            .iter()
            .find(|item| item.rab == rab)
            .map(|item| item.disposition)
    }

    #[cfg(test)]
    pub fn progress(&self) -> RejectionProgress {
        let mut result = RejectionProgress {
            items: self.items.len(),
            ..RejectionProgress::default()
        };
        for item in &self.items {
            match item.disposition {
                FailureDisposition::Pending => (),
                FailureDisposition::Recorded => result.recorded += 1,
                FailureDisposition::Superseded => result.superseded += 1,
            }
        }
        result
    }

    pub fn is_complete(&self) -> bool {
        self.items
            .iter()
            .all(|item| item.disposition != FailureDisposition::Pending)
    }

    pub fn record<T: Transport>(
        &mut self,
        core: &mut CoreLink<T>,
        now: ControlTime,
        budget: NonZeroUsize,
    ) -> Result<RejectionStep, umts_core::Error> {
        let request = self.request();
        if !core.gateway().same_rab_owner(request) {
            return Err(umts_core::Error::Session(umts_iuh::Error::Origin));
        }
        let mut report = RejectionStep::default();
        for _ in 0..budget.get().min(self.items.len()) {
            let index = self.cursor;
            self.cursor = (self.cursor + 1) % self.items.len();
            report.visited += 1;
            let item = &mut self.items[index];
            if item.disposition != FailureDisposition::Pending {
                continue;
            }
            let gateway = core.gateway();
            let current =
                if !gateway.owns_rab_request(request) || !gateway.rab_connection_current(request) {
                    false
                } else {
                    match gateway.authorize_rab(request, item.rab, item.operation) {
                        Ok(_) => true,
                        Err(umts_core::Error::Session(
                            umts_iuh::Error::Identity
                            | umts_iuh::Error::Origin
                            | umts_iuh::Error::State,
                        )) => false,
                        Err(error) => return Err(error),
                    }
                };
            if !current {
                item.disposition = FailureDisposition::Superseded;
                report.superseded += 1;
                continue;
            }
            let failure = Failed::new(item.rab, self.cause.clone());
            // TS 25.413 8.2.2 defines distinct failed-to-establish/modify and failed-to-release lists.
            let outcome = match item.operation {
                RabOperation::SetupOrModify => RabOutcome::SetupFailed(failure),
                RabOperation::Release => RabOutcome::ReleaseFailed(failure),
            };
            core.gateway_mut()
                .record_rab_outcome(now, request, outcome)?;
            item.disposition = FailureDisposition::Recorded;
            report.recorded += 1;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core_link::tests::{Mock, Peer, ready, register},
        rab_handoff::RabHandoff,
        signalling::{CommonSignalling, tests::controller},
        test_support::{count, source},
    };
    use umts_iuh::{
        ContextId, Domain,
        ranap::rab::{Assignment, AssignmentResponse, First, Release, Second, SetupOrModify},
        rua,
        wire::Limits,
    };

    const FAILURE_CAUSE: u16 = 32;

    struct Environment {
        core: CoreLink<Mock>,
        radio: CommonSignalling,
        peer: Peer,
        context: ContextId,
    }

    fn environment() -> Environment {
        let source = source(false);
        let mut radio = controller(&source);
        let (mut core, peer) = ready(&mut radio);
        let (_, context, _) = register(&mut core, &peer, &mut radio, &source, 0, 3);
        Environment {
            core,
            radio,
            peer,
            context,
        }
    }

    fn setup(id: u8) -> SetupOrModify {
        SetupOrModify {
            first: First::modification(id),
            second: Second::default(),
        }
    }

    fn cause() -> Cause {
        Cause::from_code(FAILURE_CAUSE).unwrap()
    }

    fn two_setup() -> Assignment {
        Assignment {
            setup: vec![setup(5), setup(6)],
            release: vec![],
        }
    }

    fn send(env: &Environment, assignment: &Assignment) {
        env.peer.rua(rua::Message::Direct {
            domain: Domain::Cs,
            context: env.context,
            ranap: assignment.encode(Limits::default()).unwrap(),
        });
    }

    fn drive(env: &mut Environment, handoff: &mut RabHandoff, now: u64) -> crate::core_link::Step {
        env.core
            .drive(ControlTime(now), &mut env.radio, |_, event| {
                handoff.offer(event).map(|_| ())
            })
            .unwrap()
    }

    fn offer(env: &mut Environment, assignment: &Assignment, now: u64) -> PendingRabAssignment {
        send(env, assignment);
        let mut handoff = RabHandoff::new(Limits::default());
        drive(env, &mut handoff, now);
        handoff.take().unwrap().unwrap()
    }

    fn disconnect(env: &mut Environment, now: u64) {
        env.peer.rua(rua::Message::Disconnect {
            domain: Domain::Cs,
            context: env.context,
            cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
            ranap: None,
        });
        crate::core_link::tests::finish_peer_disconnect(&mut env.core, &mut env.radio, now);
    }

    #[test]
    fn constructor_requires_actual_commit_and_returns_original_uncommitted_request() {
        let mut env = environment();
        send(&env, &two_setup());
        let mut handoff = RabHandoff::new(Limits::default());
        assert!(
            env.core
                .drive(ControlTime(6), &mut env.radio, |_, event| {
                    handoff.offer(event).unwrap();
                    Err::<(), _>("hold before core commit")
                })
                .is_err()
        );
        let pending = handoff.take().unwrap().unwrap();
        let request = pending.request();
        assert!(env.core.gateway().same_rab_owner(request));
        assert!(env.core.gateway().rab_connection_current(request));
        assert!(!env.core.gateway().has_pending_rab_request(request));
        let rejected = match FailedRabAssignment::new(pending, cause(), env.core.gateway()) {
            Ok(_) => panic!("uncommitted request cannot authorize rejection"),
            Err(rejected) => rejected,
        };
        assert!(matches!(rejected.error, FailureDispositionError::Authority));
        assert_eq!(rejected.pending.request(), request);
        assert_eq!(rejected.pending.assignment(), &two_setup());
        drive(&mut env, &mut handoff, 7);
        let committed = handoff.take().unwrap().unwrap();
        assert_eq!(committed.request(), request);
        assert!(env.core.gateway().has_pending_rab_request(request));
        let failure = FailedRabAssignment::new(committed, cause(), env.core.gateway()).unwrap();
        assert_eq!(failure.request(), request);
        assert_eq!(failure.into_pending().assignment(), &two_setup());
    }

    #[test]
    fn uncommitted_connect_is_not_mistaken_for_a_lost_domain_generation() {
        let mut env = environment();
        env.peer.rua(rua::Message::Connect {
            domain: Domain::Ps,
            context: env.context,
            cause: rua::EstablishmentCause::Normal,
            selector: None,
            membership: None,
            ranap: two_setup().encode(Limits::default()).unwrap(),
        });
        let mut handoff = RabHandoff::new(Limits::default());
        assert!(
            env.core
                .drive(ControlTime(6), &mut env.radio, |_, event| {
                    handoff.offer(event).unwrap();
                    Err::<(), _>("hold new domain before core commit")
                })
                .is_err()
        );
        let pending = handoff.take().unwrap().unwrap();
        let request = pending.request();
        assert!(!env.core.gateway().rab_connection_current(request));
        assert!(!env.core.gateway().has_pending_rab_request(request));
        let rejected = match FailedRabAssignment::new(pending, cause(), env.core.gateway()) {
            Ok(_) => panic!("an unopened domain does not prove request revocation"),
            Err(rejected) => rejected,
        };
        assert_eq!(rejected.pending.request(), request);
        assert!(matches!(rejected.error, FailureDispositionError::Authority));
        drive(&mut env, &mut handoff, 7);
        let pending = handoff.take().unwrap().unwrap();
        assert_eq!(pending.request(), request);
        let mut failure = FailedRabAssignment::new(pending, cause(), env.core.gateway()).unwrap();
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(8), count(2))
                .unwrap()
                .recorded,
            2
        );
    }

    #[test]
    fn bounded_records_survive_blocked_publication_and_emit_exact_mixed_failure_lists_once() {
        let mut env = environment();
        let assignment = Assignment {
            setup: vec![setup(5)],
            release: vec![Release::new(6, cause())],
        };
        let pending = offer(&mut env, &assignment, 6);
        let request = pending.request();
        let mut failure = FailedRabAssignment::new(pending, cause(), env.core.gateway()).unwrap();
        let mut handoff = RabHandoff::new(Limits::default());
        env.peer.set_blocked(true);
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(7), count(1))
                .unwrap(),
            RejectionStep {
                visited: 1,
                recorded: 1,
                superseded: 0
            }
        );
        assert!(!failure.is_complete());
        assert!(!drive(&mut env, &mut handoff, 7).rab_published);
        assert_eq!(failure.disposition(5), Some(FailureDisposition::Recorded));
        assert_eq!(failure.disposition(6), Some(FailureDisposition::Pending));
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(8), count(1))
                .unwrap()
                .recorded,
            1
        );
        assert!(failure.is_complete());
        assert!(drive(&mut env, &mut handoff, 8).rab_published);
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(8), count(16))
                .unwrap(),
            RejectionStep {
                visited: 2,
                recorded: 0,
                superseded: 0
            }
        );
        env.peer.set_blocked(false);
        for now in 9..13 {
            assert!(!drive(&mut env, &mut handoff, now).rab_published);
        }
        let rua::Message::Direct {
            domain,
            context,
            ranap,
        } = env.peer.last_rua()
        else {
            panic!("missing canonical RAB response")
        };
        assert_eq!((domain, context), (Domain::Cs, env.context));
        let response = AssignmentResponse::decode(&ranap, Limits::default())
            .unwrap()
            .0;
        assert_eq!(
            response,
            AssignmentResponse {
                failed: vec![Failed::new(5, cause())],
                release_failed: vec![Failed::new(6, cause())],
                ..AssignmentResponse::default()
            }
        );
        assert!(!env.core.gateway().has_pending_rab_request(request));
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(13), count(1))
                .unwrap()
                .recorded,
            0
        );
        assert_eq!(
            failure.progress(),
            RejectionProgress {
                items: 2,
                recorded: 2,
                superseded: 0
            }
        );
    }

    #[test]
    fn command_failure_retains_pending_item_and_round_robin_retry_never_replays_recorded_item() {
        let mut env = environment();
        let pending = offer(&mut env, &two_setup(), 6);
        let mut failure = FailedRabAssignment::new(pending, cause(), env.core.gateway()).unwrap();
        assert_eq!(
            failure.record(&mut env.core, ControlTime(5), count(2)),
            Err(umts_core::Error::Time)
        );
        assert_eq!(
            failure.progress(),
            RejectionProgress {
                items: 2,
                recorded: 0,
                superseded: 0
            }
        );
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(7), count(1))
                .unwrap()
                .recorded,
            1
        );
        assert_eq!(failure.disposition(5), Some(FailureDisposition::Pending));
        assert_eq!(failure.disposition(6), Some(FailureDisposition::Recorded));
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(7), count(1))
                .unwrap()
                .recorded,
            1
        );
        assert!(failure.is_complete());
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(7), count(16))
                .unwrap()
                .recorded,
            0
        );
    }

    #[test]
    fn partial_supersession_before_construction_preserves_unrelated_committed_item() {
        let mut env = environment();
        let original = offer(&mut env, &two_setup(), 6);
        let release = offer(
            &mut env,
            &Assignment {
                setup: vec![],
                release: vec![Release::new(5, cause())],
            },
            7,
        );
        assert!(
            env.core
                .gateway()
                .has_pending_rab_request(original.request())
        );
        let mut failure = FailedRabAssignment::new(original, cause(), env.core.gateway()).unwrap();
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(8), count(1))
                .unwrap(),
            RejectionStep {
                visited: 1,
                recorded: 0,
                superseded: 1
            }
        );
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(8), count(1))
                .unwrap()
                .recorded,
            1
        );
        assert_eq!(
            failure.progress(),
            RejectionProgress {
                items: 2,
                recorded: 1,
                superseded: 1
            }
        );
        assert!(failure.is_complete());
        release.authorize_items(env.core.gateway()).unwrap();
    }

    #[test]
    fn witnessed_request_fully_superseded_before_construction_finishes_without_old_outcomes() {
        let mut env = environment();
        let mut original = offer(&mut env, &two_setup(), 6);
        let request = original.request();
        assert!(original.confirm(env.core.gateway()));
        let release = offer(
            &mut env,
            &Assignment {
                setup: vec![],
                release: vec![Release::new(5, cause()), Release::new(6, cause())],
            },
            7,
        );
        assert!(!env.core.gateway().has_pending_rab_request(request));
        assert!(env.core.gateway().rab_connection_current(request));
        let mut failure = FailedRabAssignment::new(original, cause(), env.core.gateway()).unwrap();
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(8), count(1))
                .unwrap(),
            RejectionStep {
                visited: 1,
                recorded: 0,
                superseded: 1
            }
        );
        assert!(!failure.is_complete());
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(8), count(1))
                .unwrap(),
            RejectionStep {
                visited: 1,
                recorded: 0,
                superseded: 1
            }
        );
        assert!(failure.is_complete());
        assert_eq!(failure.progress().recorded, 0);
        release.authorize_items(env.core.gateway()).unwrap();
        assert!(!drive(&mut env, &mut RabHandoff::new(Limits::default()), 9).rab_published);
    }

    #[test]
    fn witnessed_request_disconnected_before_construction_has_terminal_disposition() {
        let mut env = environment();
        let mut pending = offer(&mut env, &two_setup(), 6);
        assert!(pending.confirm(env.core.gateway()));
        disconnect(&mut env, 7);
        assert!(
            !env.core
                .gateway()
                .has_pending_rab_request(pending.request())
        );
        let mut failure = FailedRabAssignment::new(pending, cause(), env.core.gateway()).unwrap();
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(8), count(2))
                .unwrap(),
            RejectionStep {
                visited: 2,
                recorded: 0,
                superseded: 2
            }
        );
        assert!(failure.is_complete());
    }

    #[test]
    fn disconnected_generation_is_terminal_without_recording_against_replacement() {
        let mut env = environment();
        let pending = offer(&mut env, &two_setup(), 6);
        let request = pending.request();
        let mut failure = FailedRabAssignment::new(pending, cause(), env.core.gateway()).unwrap();
        disconnect(&mut env, 7);
        assert!(!env.core.gateway().rab_connection_current(request));
        env.peer.rua(rua::Message::Connect {
            domain: Domain::Cs,
            context: env.context,
            cause: rua::EstablishmentCause::Normal,
            selector: None,
            membership: None,
            ranap: two_setup().encode(Limits::default()).unwrap(),
        });
        let mut handoff = RabHandoff::new(Limits::default());
        drive(&mut env, &mut handoff, 8);
        let replacement = handoff.take().unwrap().unwrap();
        assert!(!request.same_connection(replacement.request()));
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(9), count(1))
                .unwrap()
                .superseded,
            1
        );
        assert!(!failure.is_complete());
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(9), count(1))
                .unwrap()
                .superseded,
            1
        );
        assert!(failure.is_complete());
        replacement.authorize_items(env.core.gateway()).unwrap();
    }

    #[test]
    fn foreign_core_cannot_construct_or_terminate_the_original_failure_owner() {
        let mut env = environment();
        let mut foreign = environment();
        let mut pending = offer(&mut env, &two_setup(), 6);
        assert!(!pending.confirm(foreign.core.gateway()));
        assert!(pending.confirm(env.core.gateway()));
        let request = pending.request();
        let rejected = match FailedRabAssignment::new(pending, cause(), foreign.core.gateway()) {
            Ok(_) => panic!("foreign core cannot claim pending request"),
            Err(rejected) => rejected,
        };
        assert!(matches!(rejected.error, FailureDispositionError::Authority));
        assert_eq!(rejected.pending.request(), request);
        let mut failure =
            FailedRabAssignment::new(rejected.pending, cause(), env.core.gateway()).unwrap();
        assert_eq!(
            failure.record(&mut foreign.core, ControlTime(7), count(2)),
            Err(umts_core::Error::Session(umts_iuh::Error::Origin))
        );
        assert_eq!(
            failure.progress(),
            RejectionProgress {
                items: 2,
                recorded: 0,
                superseded: 0
            }
        );
        assert_eq!(
            failure
                .record(&mut env.core, ControlTime(7), count(2))
                .unwrap()
                .recorded,
            2
        );
    }
}
