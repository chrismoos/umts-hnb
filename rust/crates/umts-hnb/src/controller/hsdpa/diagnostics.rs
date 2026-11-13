// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

impl<T: Transport> CommonController<T> {
    pub fn visit_hsdpa_carrier_progress(
        &self,
        mut visit: impl FnMut(
            ReceiveIdentity,
            umts_types::hsdpa::CarrierId,
            crate::hsdpa_session::SchedulingProgress,
        ),
    ) {
        if let Some(sessions) = &self.hsdpa {
            for session in sessions.entries.iter().flatten() {
                for carrier in session.carriers() {
                    visit(
                        session.identity(),
                        carrier,
                        session.scheduling_progress_on(carrier),
                    );
                }
            }
        }
    }

    pub fn visit_hsdpa_carrier_diagnostics(
        &self,
        mut visit: impl FnMut(
            ReceiveIdentity,
            Phase,
            umts_types::hsdpa::CarrierId,
            crate::hsdpa_session::FeedbackSnapshot,
            Option<crate::hsdpa_session::SecondaryState>,
        ),
    ) {
        if let Some(sessions) = &self.hsdpa {
            for session in sessions.entries.iter().flatten() {
                for carrier in session.carriers() {
                    visit(
                        session.identity(),
                        session.phase(),
                        carrier,
                        session.feedback_snapshot_on(carrier),
                        session.secondary_state(),
                    );
                }
            }
        }
    }
}
