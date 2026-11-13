// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{AdditionalLaunch, Rel99Admission};
use crate::{
    dedicated_controller::DedicatedController, dedicated_rx::ReceiveIdentity,
    signalling::CommonSignalling,
};
use anyhow::{Context, Result, ensure};
use std::{num::NonZeroU64, sync::Arc};
use umts_rrc::{
    capabilities::hsupa::FddHsupa,
    dedicated::{Rel99Configuration, ScheduledActivation},
    hsupa::{ActivationProcedure, UplinkActivation},
};
use umts_types::hsupa::Rel6Category;

impl Rel99Admission {
    pub fn reschedule_prepared_hsupa_activation(
        &self,
        prepared: &crate::hsupa_session::ReceiveResources,
        launch: AdditionalLaunch,
        response_guard: NonZeroU64,
        radio: &CommonSignalling,
        receive: &mut DedicatedController,
    ) -> Result<Box<ActivationProcedure>> {
        self.prepare_hsupa_activation(
            prepared.identity(),
            launch,
            response_guard,
            radio,
            receive,
            |category, dedicated, activation| {
                let configuration = prepared.configuration();
                ensure!(
                    configuration.parameters().transport.category() == category
                        && Arc::ptr_eq(configuration.dedicated(), &dedicated),
                    "HSUPA prepared catalog no longer matches the active UE configuration"
                );
                configuration
                    .rescheduled(activation)
                    .map(Arc::new)
                    .map_err(|error| anyhow::anyhow!("HSUPA prepared activation: {error:?}"))
            },
        )
    }

    pub fn prepare_hsupa_activation(
        &self,
        identity: ReceiveIdentity,
        launch: AdditionalLaunch,
        response_guard: NonZeroU64,
        radio: &CommonSignalling,
        receive: &mut DedicatedController,
        mut construct: impl FnMut(
            Rel6Category,
            Arc<Rel99Configuration>,
            ScheduledActivation,
        ) -> Result<Arc<UplinkActivation>>,
    ) -> Result<Box<ActivationProcedure>> {
        let capability = radio
            .capabilities(identity.ue)
            .context("HSUPA capability information has not been received")?;
        let FddHsupa::Supported(category) = capability.hsupa() else {
            anyhow::bail!("UE has not reported a supported Release-6 HSUPA category");
        };
        self.prepare_hspa_activation(
            identity,
            launch,
            response_guard,
            radio,
            receive,
            |dedicated, activation| {
                let construction = construct(category, dedicated, activation)?;
                ensure!(
                    construction.parameters().transport.category() == category,
                    "HSUPA construction must use the UE's reported capability"
                );
                Ok(construction)
            },
        )
    }
}
