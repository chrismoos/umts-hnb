// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::dedicated_bearers::CommittedHsdpa;
use anyhow::{Context, Result, ensure};
use std::sync::Arc;
use umts_rrc::{
    dedicated::{Rel99Bearer, Rel99Configuration, Rel99Rlc, ScheduledActivation},
    hsupa::{
        ActivationParameters, EdchOnlyActivation, EdchOnlyBearerPlan, MappedBearer,
        RetainedDownlink, UplinkActivation, UplinkMapping,
    },
};
use umts_types::CnDomain;

const MAX_PACKET_BEARERS: usize = 8;
// TS 25.331 E-DCH MAC-d flow information reserves FDD identity 7 for CCCH.
pub(crate) const MAX_DEDICATED_FLOWS: usize = 7;
const FIRST_LOGICAL: u8 = 1;

/// Flow order corresponds to packet-bearer order in the retained configuration.
pub fn build_coexistence_activation(
    dedicated: Arc<Rel99Configuration>,
    parameters: ActivationParameters,
    downlink: Option<&CommittedHsdpa>,
    activation: ScheduledActivation,
) -> Result<Arc<UplinkActivation>> {
    if let Some(downlink) = downlink {
        ensure!(
            Arc::ptr_eq(downlink.dedicated(), &dedicated),
            "HSUPA must retain the current committed HSDPA bearer mapping"
        );
    }
    let packets: Vec<_> = dedicated
        .bearers()
        .iter()
        .filter(|bearer| bearer.domain == Some(CnDomain::Ps))
        .collect();
    ensure!(
        !packets.is_empty()
            && packets.len() <= MAX_PACKET_BEARERS
            && valid_packet_flows(&parameters, packets.len()),
        "HSUPA requires scheduled dedicated flows for one through eight packet bearers"
    );
    let mut mappings = Vec::with_capacity(packets.len());
    for (index, bearer) in packets.into_iter().enumerate() {
        mappings.push(map_bearer(
            &dedicated,
            bearer,
            index,
            parameters.flows[index.min(parameters.flows.len() - 1)].identity(),
            downlink,
        )?);
    }
    UplinkActivation::coexistence(
        dedicated,
        parameters,
        mappings,
        activation,
        downlink.is_some(),
    )
    .and_then(|activation| {
        activation.with_secondary_cell(downlink.is_some_and(|value| {
            value
                .activation()
                .configuration()
                .secondary_cell()
                .is_some()
        }))
    })
    .map(Arc::new)
    .map_err(|error| anyhow::anyhow!("HSUPA retained-DCH activation: {error:?}"))
}

pub fn build_edch_only_activation(
    dedicated: Arc<Rel99Configuration>,
    parameters: ActivationParameters,
    downlink: Option<&CommittedHsdpa>,
    activation: ScheduledActivation,
) -> Result<Arc<UplinkActivation>> {
    ensure!(
        downlink.is_none_or(|downlink| Arc::ptr_eq(downlink.dedicated(), &dedicated)),
        "HSUPA must retain the current committed HSDPA bearer mapping"
    );
    let packet_count = dedicated
        .bearers()
        .iter()
        .filter(|bearer| bearer.domain == Some(CnDomain::Ps))
        .count();
    ensure!(
        (1..=MAX_PACKET_BEARERS).contains(&packet_count)
            && valid_packet_flows(&parameters, packet_count),
        "HSUPA requires scheduled dedicated flows for one through eight packet bearers"
    );
    ensure!(
        dedicated
            .bearers()
            .iter()
            .all(|bearer| bearer.domain != Some(CnDomain::Cs)),
        "CS bearers require the DPDCH coexistence construction"
    );
    let signalling_flow = parameters.flows[0].identity();
    let mut packet = 0;
    let mut mappings = Vec::with_capacity(dedicated.bearers().len());
    for (index, bearer) in dedicated.bearers().iter().enumerate() {
        let flow = if bearer.domain == Some(CnDomain::Ps) {
            let flow = parameters.flows[packet.min(parameters.flows.len() - 1)].identity();
            packet += 1;
            flow
        } else {
            // TS 25.321 11.8.1.4 preserves logical-channel priority within a MAC-d flow.
            signalling_flow
        };
        mappings.push(map_bearer(&dedicated, bearer, index, flow, downlink)?);
    }
    let bearers = EdchOnlyBearerPlan::new(dedicated, mappings)
        .map_err(|error| anyhow::anyhow!("HSUPA complete E-DCH mappings: {error:?}"))?;
    EdchOnlyActivation::new(bearers, parameters, activation, downlink.is_some())
        .and_then(|activation| {
            UplinkActivation::EdchOnly(activation).with_secondary_cell(downlink.is_some_and(
                |value| {
                    value
                        .activation()
                        .configuration()
                        .secondary_cell()
                        .is_some()
                },
            ))
        })
        .map(Arc::new)
        .map_err(|error| anyhow::anyhow!("HSUPA no-DPDCH activation: {error:?}"))
}

fn valid_packet_flows(parameters: &ActivationParameters, packets: usize) -> bool {
    (1..=packets.min(MAX_DEDICATED_FLOWS)).contains(&parameters.flows.len())
        && parameters
            .flows
            .iter()
            .all(|flow| usize::from(flow.identity()) < MAX_DEDICATED_FLOWS)
}

fn map_bearer(
    dedicated: &Rel99Configuration,
    bearer: &Rel99Bearer,
    index: usize,
    flow: u8,
    downlink: Option<&CommittedHsdpa>,
) -> Result<MappedBearer> {
    ensure!(
        !matches!(bearer.rlc, Rel99Rlc::Transparent { .. }),
        "HSUPA packet bearer {} cannot use transparent RLC",
        bearer.id
    );
    let mut size = None;
    let mut priority = None;
    for mapping in &bearer.mapping.uplink {
        if let Some(previous) = priority {
            ensure!(
                previous == mapping.priority,
                "HSUPA cannot collapse unequal logical priorities"
            );
        }
        priority = Some(mapping.priority);
        let transport = dedicated
            .uplink()
            .transport(mapping.transport)
            .context("HSUPA packet bearer refers to an absent uplink transport")?;
        for format in transport
            .formats()
            .filter(|format| format.blocks != 0 && format.rlc_bits != 0)
        {
            if let Some(previous) = size {
                ensure!(
                    previous == format.rlc_bits,
                    "HSUPA packet bearer requires multiple DDI sizes"
                );
            }
            size = Some(format.rlc_bits);
        }
    }
    let logical = FIRST_LOGICAL + u8::try_from(index)?;
    let uplink = UplinkMapping::new(
        logical,
        flow,
        u8::try_from(index)?,
        size.context("HSUPA packet bearer has no nonempty uplink format")?,
        priority.context("HSUPA packet bearer has no uplink mapping")?,
        true,
    )
    .map_err(|error| anyhow::anyhow!("HSUPA bearer {} mapping: {error:?}", bearer.id))?;
    let retained = match downlink.and_then(|downlink| downlink.mapping(bearer.id)) {
        Some(mapping)
            if downlink
                .is_some_and(|downlink| downlink.activation().configuration().uses_mac_ehs()) =>
        {
            let configuration = downlink.expect("matched retained HSDPA").activation();
            let queue = configuration
                .parameters()
                .queues
                .queues()
                .iter()
                .find(|queue| queue.flow() == mapping.flow)
                .context("retained MAC-ehs queue missing")?;
            RetainedDownlink::Ehs {
                queue: queue.id(),
                logical: mapping
                    .logical
                    .context("retained MAC-ehs logical identity missing")?,
            }
        }
        Some(mapping) => RetainedDownlink::Hsdpa {
            flow: mapping.flow,
            logical: mapping.logical,
        },
        None => RetainedDownlink::Dedicated,
    };
    Ok(MappedBearer {
        bearer: bearer.id,
        uplink,
        downlink: retained,
    })
}
