// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use anyhow::{Context, ensure};
use std::{num::NonZeroU64, sync::Arc};
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SECOND};

const RESTORATION_GUARD_SECONDS: u64 = 5;

impl<T: Transport> CommonController<T> {
    pub(crate) fn ensure_hsupa_dch(
        &mut self,
        identity: crate::dedicated_rx::ReceiveIdentity,
        launch: crate::rel99_admission::AdditionalLaunch,
        resources: &mut crate::radio_resources::RadioResources,
    ) -> anyhow::Result<bool> {
        let Some(session) = self
            .hsupa
            .as_mut()
            .and_then(|sessions| sessions.entries.get_mut(identity.ue.slot as usize))
            .and_then(Option::as_deref_mut)
        else {
            return Ok(true);
        };
        ensure!(
            session.identity() == identity,
            "restoration refers to another dedicated generation"
        );
        if session.phase() == Phase::Restored {
            if !self.radio.additional_setup_ready(identity)
                || self
                    .admission
                    .as_ref()
                    .is_none_or(|admission| !admission.additional_request_ready(identity))
            {
                return Ok(false);
            }
            if !session
                .release_retired_resources(resources)
                .map_err(|error| anyhow::anyhow!("restored HSUPA resources: {error:?}"))?
            {
                return Ok(false);
            }
            self.hsupa.as_mut().expect("retained registry").entries[identity.ue.slot as usize] =
                None;
            return Ok(true);
        }
        match session.phase() {
            Phase::Active => (),
            Phase::RecoveryRequired => anyhow::bail!("HSUPA restoration requires recovery"),
            _ => return Ok(false),
        }
        let previous = Arc::clone(session.procedure().construction());
        let uplink = session.uplink_anchor();
        if !self.radio.additional_setup_ready(identity) {
            return Ok(false);
        }
        let admission = self
            .admission
            .as_ref()
            .context("restoration requires radio admission")?;
        if !admission.additional_request_ready(identity) {
            return Ok(false);
        }
        let receive = self
            .dedicated
            .as_mut()
            .context("restoration requires dedicated RX")?;
        let guard = NonZeroU64::new(RESTORATION_GUARD_SECONDS * CHIPS_PER_SECOND)
            .expect("positive restoration guard");
        let procedure = admission.prepare_hspa_activation(
            identity,
            launch,
            guard,
            &self.radio,
            receive,
            |dedicated, activation| {
                ensure!(
                    Arc::ptr_eq(&dedicated, previous.dedicated()),
                    "restoration predecessor changed"
                );
                umts_rrc::hsupa::DchRestoration::new(Arc::clone(&previous), activation)
                    .map(Arc::new)
                    .map_err(|error| anyhow::anyhow!("DCH restoration construction: {error:?}"))
            },
        )?;
        let activation = procedure.construction().activation().time();
        let deadline = ChipInstant {
            index: activation
                .index
                .checked_add(CHIPS_PER_FRAME)
                .and_then(|index| index.checked_add(guard.get()))
                .context("restoration deadline overflow")?,
            ..activation
        };
        let restoration =
            crate::hsupa_session::Restoration::new(identity, procedure, uplink, deadline)
                .map_err(|_| anyhow::anyhow!("invalid restoration procedure"))?;
        self.hsupa.as_mut().expect("retained registry").entries[identity.ue.slot as usize]
            .as_deref_mut()
            .expect("retained session")
            .begin_restoration(restoration)
            .map_err(|_| anyhow::anyhow!("restoration admission or grant boundary changed"))?;
        Ok(false)
    }
}
