// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    controller::CommonController,
    dedicated_rx::ReceiveIdentity,
    rel99_traffic::Rel99Traffic,
    signalling::{IdleBlocker, SignallingError},
};
use umts_config::Config;
use umts_core::Transport;
use umts_rrc::nas::CoreConnection;
use umts_types::{
    CnDomain, UeKey,
    time::{CHIPS_PER_SECOND, ChipInstant},
};
use umts_user_plane::packet::BoundDatagrams;

const SCAN_INTERVAL_CHIPS: u64 = CHIPS_PER_SECOND / 100;

#[derive(Clone, Copy, Default, Debug)]
pub(super) struct Counters {
    pub iu_requests: u64,
    pub rrc_releases: u64,
    pub completed: u64,
    pub timed_out: u64,
    pub failed: u64,
}

#[derive(Clone, Copy)]
struct Activity {
    key: UeKey,
    route: Option<ReceiveIdentity>,
    signalling: u64,
    packets: u64,
    since: ChipInstant,
    observed: ChipInstant,
    blocker: Option<IdleBlocker>,
    requested: [Option<CoreConnection>; 2],
}

impl Activity {
    fn record_packets(&mut self, sequence: u64) -> bool {
        let changed = sequence > self.packets;
        // Removing a GTP route is not user activity. New packets have cell-wide monotonic serials.
        self.packets = self.packets.max(sequence);
        changed
    }

    fn observe_activity(
        &mut self,
        now: ChipInstant,
        changed: bool,
        user_bearers: bool,
        blocker: Option<IdleBlocker>,
    ) {
        let active_user = matches!(blocker, Some(IdleBlocker::Circuit))
            || (user_bearers && matches!(blocker, Some(IdleBlocker::UserData)));
        // Pending diagnostic signaling defers release without restarting user inactivity.
        if changed || (active_user && self.requested == [None; 2]) {
            self.since = now;
            if changed {
                self.requested = [None; 2];
            }
        }
    }
}

pub(super) struct Inactivity {
    entries: Vec<Option<Activity>>,
    cursor: u32,
    next: u64,
    release_chips: u64,
    delete_chips: u64,
    release_retries: umts_rrc::mobility::ReleaseCompleteRetries,
    pub counters: Counters,
}

impl Inactivity {
    pub fn visit(&self, mut visitor: impl FnMut(UeKey, u64, Option<IdleBlocker>)) {
        for entry in self.entries.iter().flatten() {
            visitor(
                entry.key,
                entry.observed.index.saturating_sub(entry.since.index),
                entry.blocker,
            );
        }
    }

    pub fn new(config: &Config) -> anyhow::Result<Self> {
        let release = config.rrc.inactivity_release_seconds;
        let delete = config.rrc.inactivity_delete_seconds;
        anyhow::ensure!(
            delete == 0 || release == 0 || delete >= release,
            "rrc.inactivity_delete_seconds must be zero or at least inactivity_release_seconds"
        );
        Ok(Self {
            entries: vec![None; config.runtime.maximum_ues as usize],
            cursor: 0,
            next: 0,
            release_chips: u64::from(release) * CHIPS_PER_SECOND,
            delete_chips: u64::from(delete) * CHIPS_PER_SECOND,
            release_retries: umts_rrc::mobility::ReleaseCompleteRetries::new(
                config.rrc.release_complete_retransmissions,
            )
            .map_err(|error| anyhow::anyhow!("invalid RRC release retransmissions: {error:?}"))?,
            counters: Counters::default(),
        })
    }

    pub fn service<T: Transport, D: BoundDatagrams>(
        &mut self,
        controller: &mut CommonController<T>,
        traffic: &mut Rel99Traffic<D>,
        assignments: &crate::rab_handoff::RabHandoff,
        now: ChipInstant,
    ) -> Result<(), SignallingError> {
        if now.index < self.next {
            return Ok(());
        }
        self.next = now.index.saturating_add(SCAN_INTERVAL_CHIPS);
        let slot = self.cursor;
        self.cursor = (slot + 1) % self.entries.len() as u32;
        let record = &mut self.entries[slot as usize];
        let Some(candidate) = controller.radio().idle_candidate(slot, now) else {
            *record = None;
            return Ok(());
        };
        let packets = traffic.packets_mut().activity(candidate.key);
        let mut blocker = candidate.blocker.or_else(|| {
            assignments
                .has_ue(candidate.key)
                .then_some(IdleBlocker::Assignment)
        });
        let mut user_bearers = false;
        if let Some(identity) = candidate.route {
            match controller
                .admission()
                .and_then(|admission| admission.committed_radio(identity).ok())
            {
                Some(active) => {
                    for bearer in active.plan().configuration().bearers() {
                        if bearer.domain == Some(CnDomain::Cs) {
                            blocker = blocker.or(Some(IdleBlocker::Circuit));
                        }
                        user_bearers |= bearer.domain.is_some();
                    }
                }
                None => blocker = blocker.or(Some(IdleBlocker::Radio)),
            }
            if controller.dedicated_mut().is_none_or(|dedicated| {
                dedicated
                    .with_active_bank(identity, |bank| bank.user_data_pending())
                    .unwrap_or(true)
            }) {
                blocker = blocker.or(Some(IdleBlocker::UserData));
            }
        }
        let busy = blocker.is_some();
        let previous = record.filter(|record| record.key == candidate.key);
        let changed = previous.is_none_or(|record| {
            record.signalling != candidate.activity
                || (record.route != candidate.route && record.requested == [None; 2])
        });
        let activity = record.get_or_insert(Activity {
            key: candidate.key,
            route: candidate.route,
            signalling: candidate.activity,
            packets,
            since: now,
            observed: now,
            blocker,
            requested: [None; 2],
        });
        if activity.key != candidate.key {
            *activity = Activity {
                key: candidate.key,
                route: candidate.route,
                signalling: candidate.activity,
                packets,
                since: now,
                observed: now,
                blocker,
                requested: [None; 2],
            };
        }
        let packets_changed = activity.record_packets(packets);
        activity.observed = now;
        activity.blocker = blocker;
        let changed = changed || packets_changed;
        if let Some(complete) = candidate.release_complete {
            let timed_out = self.delete_chips != 0
                && now.index.saturating_sub(activity.since.index) >= self.delete_chips;
            if complete || (timed_out && !candidate.release_queued) {
                controller.radio_mut().retire_idle(candidate.key)?;
                self.counters.completed += u64::from(complete);
                self.counters.timed_out += u64::from(!complete);
                *record = None;
            }
            return Ok(());
        }
        activity.observe_activity(now, changed, user_bearers, blocker);
        activity.route = candidate.route;
        activity.signalling = candidate.activity;
        let final_connection_closed = candidate.release_after_core_disconnect
            && candidate.connections.iter().all(Option::is_none)
            && !user_bearers;
        if busy
            || (!final_connection_closed
                && (self.release_chips == 0
                    || now.index.saturating_sub(activity.since.index) < self.release_chips))
        {
            return Ok(());
        }
        if candidate.connections.iter().any(Option::is_some) {
            for (index, connection) in candidate.connections.into_iter().enumerate() {
                if let Some(connection) = connection
                    && activity.requested[index] != Some(connection)
                {
                    controller
                        .radio_mut()
                        .request_inactivity_release(connection)?;
                    activity.requested[index] = Some(connection);
                    self.counters.iu_requests += 1;
                }
            }
        } else if !user_bearers {
            controller
                .radio_mut()
                .release_idle_rrc(candidate.key, self.release_retries)?;
            self.counters.rrc_releases += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::at;
    use std::num::NonZeroU64;

    fn activity() -> Activity {
        Activity {
            key: UeKey {
                epoch: at(0).epoch,
                slot: 0,
                generation: NonZeroU64::MIN,
            },
            route: None,
            signalling: 0,
            packets: 10,
            since: at(1),
            observed: at(1),
            blocker: None,
            requested: [None; 2],
        }
    }

    #[test]
    fn packet_route_retirement_does_not_reset_activity_or_hide_a_new_packet() {
        let mut record = activity();
        assert!(!record.record_packets(0));
        assert_eq!(record.packets, 10);
        assert!(record.record_packets(11));
        assert!(!record.record_packets(0));
        assert!(record.record_packets(12));
        assert_eq!(record.since, at(1));
    }

    #[test]
    fn transient_protocol_work_does_not_restart_signalling_only_inactivity() {
        for blocker in [
            IdleBlocker::Establishment,
            IdleBlocker::BearerChange,
            IdleBlocker::Protection,
            IdleBlocker::Capability,
            IdleBlocker::Security,
            IdleBlocker::Registration,
            IdleBlocker::DeferredNas,
            IdleBlocker::ControlTransmit,
            IdleBlocker::ControlReceive,
            IdleBlocker::NasTransmit,
            IdleBlocker::NasReceive,
            IdleBlocker::UmTransmit,
            IdleBlocker::CoreOutbox,
            IdleBlocker::Ingress,
            IdleBlocker::Assignment,
            IdleBlocker::Radio,
            IdleBlocker::UserData,
        ] {
            let mut record = activity();
            for second in 1..=180 {
                record.observe_activity(at(second * CHIPS_PER_SECOND), false, false, Some(blocker));
            }
            assert_eq!(record.since, at(1), "{blocker:?}");
        }
    }

    #[test]
    fn calls_user_data_and_new_signalling_restart_inactivity() {
        for (user_bearers, blocker, changed) in [
            (false, IdleBlocker::Circuit, false),
            (true, IdleBlocker::Circuit, false),
            (true, IdleBlocker::UserData, false),
            (false, IdleBlocker::NasReceive, true),
            (false, IdleBlocker::ControlTransmit, true),
        ] {
            let mut record = activity();
            record.observe_activity(at(CHIPS_PER_SECOND), changed, user_bearers, Some(blocker));
            assert_eq!(record.since, at(CHIPS_PER_SECOND));
        }
    }
}
