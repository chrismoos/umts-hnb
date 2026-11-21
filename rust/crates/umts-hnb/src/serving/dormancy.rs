// SPDX-License-Identifier: AGPL-3.0-or-later

//! Moves an inactive packet-only UE from CELL_DCH to a paging state and sets its bearers up
//! again when it returns.

use crate::{
    controller::CommonController,
    dedicated_rx::ReceiveIdentity,
    rab_handoff::{PendingRabAssignment, RabHandoff},
    rel99_traffic::Rel99Traffic,
    signalling::ParkProgress,
};
use umts_core::Transport;
use umts_rrc::mobility::ParkedState;
use umts_types::{
    UeKey,
    time::{CHIPS_PER_SECOND, ChipInstant},
};
use umts_user_plane::packet::BoundDatagrams;

const PARK_ANSWER_SECONDS: u64 = 3;
const RETRY_SECONDS: u64 = 30;
const DEFERRED_CONFIRM_LIMIT_SECONDS: u64 = 3;
// TS 25.331 8.3.1: a parked UE sends a periodical update every T305 and repeats an unanswered
// one N302 times at T302 intervals. Past that plus a margin it is treated as lost.
const UPDATE_MARGIN_SECONDS: u64 = 60;

#[derive(Clone, Copy, Debug)]
pub(super) struct Settings {
    /// T305 plus (N302 + 1) x T302, in chips. Zero when T305 is infinity.
    pub update_period_chips: u64,
    pub after_chips: u64,
    pub state: ParkedState,
    pub drx_coefficient: u8,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Counters {
    pub requested: u64,
    pub parked: u64,
    pub failed: u64,
    pub resumed: u64,
    pub paged: u64,
    pub lost: u64,
    pub released: u64,
}

#[derive(Clone, Copy)]
struct Watch {
    key: UeKey,
    signalling: u64,
    packets: u64,
    since: ChipInstant,
    parking: Option<(Option<ReceiveIdentity>, ChipInstant)>,
    retry_at: u64,
    session_end: bool,
    updates: u64,
    updated: ChipInstant,
}

pub(super) struct Dormancy {
    settings: Option<Settings>,
    watches: Vec<Option<Watch>>,
    stored: Vec<Option<PendingRabAssignment>>,
    pub counters: Counters,
}

impl Dormancy {
    pub(super) fn new(settings: Option<Settings>, capacity: usize) -> Self {
        let mut watches = Vec::new();
        watches.resize_with(capacity, || None);
        let mut stored = Vec::new();
        stored.resize_with(capacity, || None);
        Self {
            settings,
            watches,
            stored,
            counters: Counters::default(),
        }
    }

    pub(super) fn store(&mut self, key: UeKey, pending: PendingRabAssignment) {
        if let Some(slot) = self.stored.get_mut(key.slot as usize) {
            *slot = Some(pending);
            self.counters.parked = self.counters.parked.saturating_add(1);
        }
    }

    pub(super) fn service<T: Transport, D: BoundDatagrams>(
        &mut self,
        controller: &mut CommonController<T>,
        traffic: &mut Rel99Traffic<D>,
        assignments: &mut RabHandoff,
        now: ChipInstant,
    ) {
        let Some(settings) = self.settings else {
            return;
        };
        while let Some(key) = controller.radio_mut().retire_released_parked() {
            self.counters.released = self.counters.released.saturating_add(1);
            tracing::info!("Parked UE released by paging: key={key:?}");
        }
        while let Some(key) = controller.radio_mut().take_unanswered_page() {
            self.counters.lost = self.counters.lost.saturating_add(1);
            tracing::warn!("Parked UE did not answer its pages and is released: key={key:?}");
            if let Err(error) = controller.radio_mut().release_lost(key) {
                tracing::warn!("Parked UE release failed: key={key:?} error={error:?}");
            }
        }
        for slot in 0..self.watches.len() {
            let Some(candidate) = controller.radio().idle_candidate(slot as u32, now) else {
                self.watches[slot] = None;
                self.stored[slot] = None;
                continue;
            };
            let key = candidate.key;
            if self.stored[slot]
                .as_ref()
                .is_some_and(|pending| pending.request().key() != key)
            {
                self.stored[slot] = None;
            }
            if controller.radio().wake_ready(key)
                && (self.stored[slot].is_none()
                    || self.stored[slot]
                        .as_ref()
                        .is_some_and(|pending| !controller.resumable_parked(pending)))
            {
                // The bearers went with a core release while the UE was parked. It returns to
                // CELL_FACH with signalling only.
                self.stored[slot] = None;
                if let Err(error) = controller.radio_mut().abandon_resume(key) {
                    tracing::warn!("Returning UE not answered: key={key:?} error={error:?}");
                }
            }
            if controller.radio().wake_ready(key)
                && let Some(pending) = self.stored[slot].as_ref()
                && !assignments.has_ue(key)
                && controller.resume_parked(pending)
            {
                match assignments.reoffer(pending) {
                    Ok(()) => {
                        self.counters.resumed = self.counters.resumed.saturating_add(1);
                        tracing::info!(
                            "Parked bearers resuming: slot={} generation={}",
                            key.slot,
                            key.generation
                        );
                        self.stored[slot] = None;
                    }
                    Err(error) => {
                        tracing::warn!("Parked bearers not queued: key={key:?} error={error:?}");
                    }
                }
            }
            // TS 25.331 8.3.1.3: the UE repeats its cell update after T302, 4 s by default.
            if controller
                .radio()
                .deferred_confirm_age(key, now)
                .is_some_and(|age| age >= DEFERRED_CONFIRM_LIMIT_SECONDS * CHIPS_PER_SECOND)
                && let Err(error) = controller.radio_mut().send_deferred_confirm(key)
            {
                tracing::warn!("Deferred cell update confirm failed: key={key:?} error={error:?}");
            }
            let packets = traffic.packets_mut().activity(key);
            let watch = self.watches[slot].get_or_insert(Watch {
                key,
                signalling: candidate.activity,
                packets,
                since: now,
                parking: None,
                retry_at: 0,
                session_end: false,
                updates: 0,
                updated: now,
            });
            if watch.key != key {
                *watch = Watch {
                    key,
                    signalling: candidate.activity,
                    packets,
                    since: now,
                    parking: None,
                    retry_at: 0,
                    session_end: false,
                    updates: 0,
                    updated: now,
                };
            }
            if controller.radio().parked(key).is_some() {
                // A packet that cannot be delivered counts as activity on its tunnel, which
                // for a parked UE means downlink data is waiting.
                // The count falls back when the tunnel is released, which is not data.
                if packets > watch.packets && controller.radio_mut().page_parked(key) {
                    self.counters.paged = self.counters.paged.saturating_add(1);
                }
                watch.packets = packets;
                watch.since = now;
                let updates = controller.radio().parked_updates(key);
                if updates != watch.updates {
                    watch.updates = updates;
                    watch.updated = now;
                } else if settings.update_period_chips != 0
                    && now.index.saturating_sub(watch.updated.index)
                        > settings.update_period_chips + UPDATE_MARGIN_SECONDS * CHIPS_PER_SECOND
                {
                    self.counters.lost = self.counters.lost.saturating_add(1);
                    watch.updated = now;
                    tracing::warn!(
                        "Parked UE sent no periodical update and is released: key={key:?}"
                    );
                    if let Err(error) = controller.radio_mut().release_lost(key) {
                        tracing::warn!("Parked UE release failed: key={key:?} error={error:?}");
                    }
                }
                continue;
            }
            watch.updated = now;
            if let Some((identity, started)) = watch.parking {
                match controller.radio().park_progress(key, now) {
                    Some(ParkProgress::Acknowledged) => {
                        watch.parking = None;
                        let parked =
                            match identity {
                                Some(identity) => controller
                                    .parkable_bearers(identity)
                                    .is_some_and(|bearers| {
                                        controller.park_dedicated(identity, &bearers)
                                    }),
                                None => controller.radio_mut().enter_parked(key, &[]).is_ok(),
                            };
                        if parked && controller.radio().resumable(key) {
                            self.stored[slot] = None;
                        }
                        if !parked {
                            tracing::warn!(
                                "Park answered but the dedicated session could not be stopped: identity={identity:?} route={:?}",
                                controller.radio().dedicated_route(key)
                            );
                            controller.radio_mut().abandon_park(key);
                            self.counters.failed = self.counters.failed.saturating_add(1);
                            watch.retry_at =
                                now.index.saturating_add(RETRY_SECONDS * CHIPS_PER_SECOND);
                        }
                    }
                    Some(ParkProgress::Requested)
                        if now.index.saturating_sub(started.index)
                            < PARK_ANSWER_SECONDS * CHIPS_PER_SECOND => {}
                    _ => {
                        watch.parking = None;
                        controller.radio_mut().abandon_park(key);
                        self.counters.failed = self.counters.failed.saturating_add(1);
                        watch.retry_at = now.index.saturating_add(RETRY_SECONDS * CHIPS_PER_SECOND);
                        tracing::warn!("Park request unanswered or refused: key={key:?}");
                    }
                }
                continue;
            }
            if watch.signalling != candidate.activity || watch.packets != packets {
                watch.signalling = candidate.activity;
                watch.packets = packets;
                watch.since = now;
                watch.session_end = false;
                continue;
            }
            let identity = candidate.route;
            watch.session_end |= controller.radio_mut().take_session_end(key);
            if candidate.blocker.is_some()
                || assignments.has_ue(key)
                || now.index < watch.retry_at
                || (!watch.session_end
                    && now.index.saturating_sub(watch.since.index) < settings.after_chips)
                || candidate.connections[0].is_some()
                // A UE in CELL_FACH is parked while a core connection keeps it. Without one
                // its RRC connection is released instead.
                || (identity.is_none() && candidate.connections[1].is_none())
                || identity.is_some_and(|identity| {
                    controller.parkable_bearers(identity).is_none()
                        || controller.dedicated_mut().is_none_or(|dedicated| {
                            dedicated
                                .with_active_bank(identity, |bank| bank.user_data_pending())
                                .unwrap_or(true)
                        })
                })
            {
                continue;
            }
            match controller
                .radio_mut()
                .request_park(key, settings.state, settings.drx_coefficient)
            {
                Ok(true) => {
                    watch.session_end = false;
                    watch.parking = Some((identity, now));
                    self.counters.requested = self.counters.requested.saturating_add(1);
                }
                Ok(false) => (),
                Err(error) => {
                    watch.retry_at = now.index.saturating_add(RETRY_SECONDS * CHIPS_PER_SECOND);
                    tracing::warn!("Park request failed: key={key:?} error={error:?}");
                }
            }
        }
    }
}
