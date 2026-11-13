// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroU32;

/// TS 25.214 4.3.3.2 radio link set supervision parameters, in radio frames.
/// Defaults follow the UE side of TS 25.331 (T313 3 s) so both ends give up together.
pub const DEFAULT_OUT_OF_SYNC_FRAMES: u32 = 50;
pub const DEFAULT_IN_SYNC_FRAMES: u32 = 10;
pub const DEFAULT_FAILURE_FRAMES: u32 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RadioLinkSyncSettings {
    /// N_OUTSYNC_IND: consecutive out-of-sync frames that start T_RLFAILURE.
    pub out_of_sync_frames: NonZeroU32,
    /// N_INSYNC_IND: consecutive in-sync frames that stop T_RLFAILURE or restore the link.
    pub in_sync_frames: NonZeroU32,
    /// T_RLFAILURE in frames. Zero fails the link as soon as N_OUTSYNC_IND is reached.
    pub failure_frames: u32,
}

/// TS 25.214 4.3.2.2 Node B radio link set states.
impl Default for RadioLinkSyncSettings {
    fn default() -> Self {
        Self {
            out_of_sync_frames: NonZeroU32::new(DEFAULT_OUT_OF_SYNC_FRAMES).expect("nonzero"),
            in_sync_frames: NonZeroU32::new(DEFAULT_IN_SYNC_FRAMES).expect("nonzero"),
            failure_frames: DEFAULT_FAILURE_FRAMES,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RadioLinkSyncState {
    #[default]
    Initial,
    InSync,
    OutOfSync,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RadioLinkEvent {
    #[default]
    None,
    Failure,
    Restore,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RadioLinkSyncStatus {
    pub state: RadioLinkSyncState,
    pub failures: u32,
    pub restores: u32,
    /// Frames since the last in-sync indication, saturating.
    pub out_of_sync_frames: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct RadioLinkMonitor {
    settings: RadioLinkSyncSettings,
    status: RadioLinkSyncStatus,
    out_of_sync_run: u32,
    in_sync_run: u32,
    failure_timer: Option<u32>,
}

impl RadioLinkMonitor {
    pub fn new(settings: RadioLinkSyncSettings) -> Self {
        Self {
            settings,
            status: RadioLinkSyncStatus::default(),
            out_of_sync_run: 0,
            in_sync_run: 0,
            failure_timer: None,
        }
    }
    pub fn status(&self) -> RadioLinkSyncStatus {
        self.status
    }
    /// One synchronization status indication per radio frame (TS 25.214 4.3.1.3).
    pub fn observe_frame(&mut self, in_sync: bool) -> RadioLinkEvent {
        if in_sync {
            self.out_of_sync_run = 0;
            self.in_sync_run = self.in_sync_run.saturating_add(1);
            self.status.out_of_sync_frames = 0;
        } else {
            self.in_sync_run = 0;
            self.out_of_sync_run = self.out_of_sync_run.saturating_add(1);
            self.status.out_of_sync_frames = self.status.out_of_sync_frames.saturating_add(1);
        }
        match self.status.state {
            // TS 25.214 4.3.2.3: the set leaves the initial state once synchronization is
            // achieved. Failure supervision only applies from the in-sync state.
            RadioLinkSyncState::Initial => {
                if in_sync {
                    self.status.state = RadioLinkSyncState::InSync;
                }
                RadioLinkEvent::None
            }
            RadioLinkSyncState::InSync => {
                if self.in_sync_run >= self.settings.in_sync_frames.get() {
                    self.failure_timer = None;
                }
                if self.failure_timer.is_none()
                    && self.out_of_sync_run >= self.settings.out_of_sync_frames.get()
                {
                    self.failure_timer = Some(0);
                    self.out_of_sync_run = 0;
                }
                match self.failure_timer {
                    Some(elapsed) if elapsed >= self.settings.failure_frames => {
                        self.failure_timer = None;
                        self.status.state = RadioLinkSyncState::OutOfSync;
                        self.status.failures = self.status.failures.saturating_add(1);
                        RadioLinkEvent::Failure
                    }
                    Some(elapsed) => {
                        self.failure_timer = Some(elapsed.saturating_add(1));
                        RadioLinkEvent::None
                    }
                    None => RadioLinkEvent::None,
                }
            }
            RadioLinkSyncState::OutOfSync => {
                if self.in_sync_run >= self.settings.in_sync_frames.get() {
                    self.status.state = RadioLinkSyncState::InSync;
                    self.status.restores = self.status.restores.saturating_add(1);
                    RadioLinkEvent::Restore
                } else {
                    RadioLinkEvent::None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(out: u32, r#in: u32, timer: u32) -> RadioLinkSyncSettings {
        RadioLinkSyncSettings {
            out_of_sync_frames: NonZeroU32::new(out).unwrap(),
            in_sync_frames: NonZeroU32::new(r#in).unwrap(),
            failure_frames: timer,
        }
    }

    #[test]
    fn failure_needs_in_sync_first_then_n_outsync_and_the_timer() {
        let mut monitor = RadioLinkMonitor::new(settings(3, 2, 4));
        for _ in 0..10 {
            assert_eq!(monitor.observe_frame(false), RadioLinkEvent::None);
        }
        assert_eq!(monitor.status().state, RadioLinkSyncState::Initial);
        assert_eq!(monitor.observe_frame(true), RadioLinkEvent::None);
        assert_eq!(monitor.status().state, RadioLinkSyncState::InSync);
        let mut events = Vec::new();
        for _ in 0..7 {
            events.push(monitor.observe_frame(false));
        }
        // Three out-of-sync frames start the timer, four more expire it on the seventh frame.
        assert_eq!(
            events
                .iter()
                .filter(|e| **e == RadioLinkEvent::Failure)
                .count(),
            1
        );
        assert_eq!(events[6], RadioLinkEvent::Failure);
        assert_eq!(monitor.status().state, RadioLinkSyncState::OutOfSync);
        assert_eq!(monitor.status().failures, 1);
        assert_eq!(monitor.status().out_of_sync_frames, 7);
    }

    #[test]
    fn n_insync_stops_the_timer_and_restores_a_failed_link() {
        let mut monitor = RadioLinkMonitor::new(settings(2, 2, 3));
        monitor.observe_frame(true);
        monitor.observe_frame(false);
        monitor.observe_frame(false);
        assert_eq!(monitor.observe_frame(true), RadioLinkEvent::None);
        assert_eq!(monitor.observe_frame(true), RadioLinkEvent::None);
        for _ in 0..3 {
            assert_eq!(monitor.observe_frame(false), RadioLinkEvent::None);
        }
        assert_eq!(monitor.status().state, RadioLinkSyncState::InSync);
        for _ in 0..3 {
            monitor.observe_frame(false);
        }
        assert_eq!(monitor.status().state, RadioLinkSyncState::OutOfSync);
        assert_eq!(monitor.observe_frame(true), RadioLinkEvent::None);
        assert_eq!(monitor.observe_frame(true), RadioLinkEvent::Restore);
        assert_eq!(monitor.status().restores, 1);
    }

    #[test]
    fn zero_timer_fails_on_the_n_outsync_frame() {
        let mut monitor = RadioLinkMonitor::new(settings(5, 1, 0));
        monitor.observe_frame(true);
        for _ in 0..4 {
            assert_eq!(monitor.observe_frame(false), RadioLinkEvent::None);
        }
        assert_eq!(monitor.observe_frame(false), RadioLinkEvent::Failure);
    }
}
