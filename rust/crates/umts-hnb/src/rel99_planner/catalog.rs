// SPDX-License-Identifier: AGPL-3.0-or-later

mod packet;

use super::{
    CircuitProfile, CircuitSubflow, Contract, EmptyTransport, HighSpeedDownlink, PacketProfile,
    Rab, Reliability, RequestError, Requirements, Service, configured, service,
};
use anyhow::{Context, Result, ensure};
use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use umts_config::{
    R99CircuitService, R99ErrorDelivery, R99ServiceContract, R99ServicesConfig, R99TrafficClass,
};
use umts_iuh::ranap::rab::{SetupOrModify, qos};
use umts_rrc::transport::CrcSize;
use umts_types::CnDomain;

const MAX_PROFILES: usize = 32;
const MAX_SUBFLOWS: usize = 7;
const MAX_FORMATS: usize = 64;
const MAX_NAME_BYTES: usize = 64;
const MAX_RABS: usize = 16;

struct Named<T> {
    name: String,
    profile: T,
}
pub struct ConfiguredServices {
    circuit: Vec<Named<CircuitProfile>>,
    packet: Vec<Named<PacketProfile>>,
}
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct Selection<'a> {
    rabs: Vec<Rab<'a>>,
    names: Vec<&'a str>,
}
#[derive(Debug)]
pub enum SelectionError {
    Capacity,
    Request(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        RequestError,
    ),
    NoService {
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        domain: CnDomain,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        rab: u8,
    },
}
impl<'a> Selection<'a> {
    pub fn rabs(&self) -> &[Rab<'a>] {
        &self.rabs
    }
    #[cfg(test)]
    pub fn names(&self) -> &[&'a str] {
        &self.names
    }
}
impl ConfiguredServices {
    #[cfg(test)]
    pub fn new(config: &R99ServicesConfig, packet_capacity: NonZeroUsize) -> Result<Self> {
        Self::with_high_speed_downlink(config, packet_capacity, None)
    }

    /// `high_speed` resizes the downlink RLC of AM packet services for HSDPA-capable UEs.
    pub fn with_high_speed_downlink(
        config: &R99ServicesConfig,
        packet_capacity: NonZeroUsize,
        high_speed: Option<HighSpeedDownlink>,
    ) -> Result<Self> {
        config.validate_schema()?;
        ensure!(
            config
                .circuit
                .len()
                .checked_add(config.packet.len())
                .is_some_and(|count| count <= MAX_PROFILES),
            "rel99.physical.planner.services may contain at most {MAX_PROFILES} profiles"
        );
        let mut names = BTreeSet::new();
        for name in config
            .circuit
            .iter()
            .map(|profile| &profile.name)
            .chain(config.packet.iter().map(|profile| &profile.name))
        {
            ensure!(
                !name.is_empty()
                    && name.len() <= MAX_NAME_BYTES
                    && !name.chars().any(char::is_control)
                    && names.insert(name),
                "service names must be unique, nonempty, printable and at most {MAX_NAME_BYTES} bytes"
            );
        }
        let circuit = config
            .circuit
            .iter()
            .map(|input| {
                Ok(Named {
                    name: input.name.clone(),
                    profile: circuit(input)
                        .with_context(|| format!("circuit service {}", input.name))?,
                })
            })
            .collect::<Result<_>>()?;
        let packet = config
            .packet
            .iter()
            .map(|input| {
                Ok(Named {
                    name: input.name.clone(),
                    profile: packet::compile(input, packet_capacity, high_speed)
                        .with_context(|| format!("packet service {}", input.name))?,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self { circuit, packet })
    }

    /// Profile order is explicit operator policy. A later radio-admission failure never silently reduces QoS.
    pub fn select<'a>(
        &'a self,
        requests: &[(CnDomain, &'a SetupOrModify)],
    ) -> Result<Selection<'a>, SelectionError> {
        if requests.is_empty() || requests.len() > MAX_RABS {
            return Err(SelectionError::Capacity);
        }
        let mut rabs = Vec::with_capacity(requests.len());
        let mut names = Vec::with_capacity(requests.len());
        for &(domain, request) in requests {
            let required = Requirements::new(domain, request).map_err(SelectionError::Request)?;
            let selected = match domain {
                CnDomain::Cs => self
                    .circuit
                    .iter()
                    .find(|entry| service::validate_circuit(&required, &entry.profile).is_ok())
                    .map(|entry| (entry.name.as_str(), Service::Circuit(&entry.profile))),
                CnDomain::Ps => self
                    .packet
                    .iter()
                    .find(|entry| service::validate_packet(&required, &entry.profile).is_ok())
                    .map(|entry| (entry.name.as_str(), Service::Packet(&entry.profile))),
            };
            let (name, service) = selected.ok_or(SelectionError::NoService {
                domain,
                rab: required.rab,
            })?;
            rabs.push(Rab {
                domain,
                request,
                service,
            });
            names.push(name);
        }
        Ok(Selection { rabs, names })
    }
}

fn contract(input: &R99ServiceContract) -> Result<Contract> {
    ensure!(
        !input.subflows.is_empty() && input.subflows.len() <= MAX_SUBFLOWS,
        "service contract requires 1..={MAX_SUBFLOWS} subflows"
    );
    ensure!(
        input.maximum_transfer_delay_ms != Some(0),
        "maximum_transfer_delay_ms must be nonzero when present"
    );
    Ok(Contract {
        traffic_class: match input.traffic_class {
            R99TrafficClass::Conversational => qos::TrafficClass::Conversational,
            R99TrafficClass::Streaming => qos::TrafficClass::Streaming,
            R99TrafficClass::Interactive => qos::TrafficClass::Interactive,
            R99TrafficClass::Background => qos::TrafficClass::Background,
        },
        delivery_order: if input.delivery_order {
            qos::DeliveryOrder::Requested
        } else {
            qos::DeliveryOrder::NotRequested
        },
        maximum_transfer_delay_ms: input.maximum_transfer_delay_ms,
        subflows: input
            .subflows
            .iter()
            .map(|flow| {
                let residual =
                    qos::ResidualError::new(flow.residual.mantissa, flow.residual.exponent)
                        .map_err(|error| anyhow::anyhow!("residual error ratio: {error:?}"))?;
                let sdu = flow
                    .sdu
                    .as_ref()
                    .map(|ratio| {
                        qos::SduError::new(ratio.mantissa, ratio.exponent)
                            .map_err(|error| anyhow::anyhow!("SDU error ratio: {error:?}"))
                    })
                    .transpose()?;
                let delivery = match flow.delivery {
                    R99ErrorDelivery::Indicate => qos::Delivery::Yes,
                    R99ErrorDelivery::Discard => qos::Delivery::No,
                    R99ErrorDelivery::NoDetection => qos::Delivery::NoDetection,
                };
                qos::Subflow::new(residual.clone(), delivery, sdu.clone(), None)
                    .map_err(|error| anyhow::anyhow!("service subflow: {error:?}"))?;
                Ok(Reliability {
                    residual,
                    sdu,
                    delivery,
                })
            })
            .collect::<Result<_>>()?,
    })
}

fn circuit(input: &R99CircuitService) -> Result<CircuitProfile> {
    let contract = contract(&input.contract)?;
    let interval = configured::interval(input.interval_ms)?;
    ensure!(
        input.versions != 0,
        "versions must include at least one Iu-UP version"
    );
    ensure!(
        input.subflows.len() == contract.subflows.len(),
        "coding and contract subflow counts differ"
    );
    ensure!(
        !input.formats.is_empty() && input.formats.len() <= MAX_FORMATS,
        "circuit service requires 1..={MAX_FORMATS} indexed formats"
    );
    ensure!(
        input
            .contract
            .maximum_transfer_delay_ms
            .is_none_or(|delay| delay >= u16::from(input.interval_ms)),
        "service period exceeds transfer delay"
    );
    let subflows = input
        .subflows
        .iter()
        .zip(&contract.subflows)
        .map(|(flow, required)| {
            let uplink = configured::coding(&flow.uplink)?;
            let downlink = configured::coding(&flow.downlink)?;
            ensure!(
                required.delivery == qos::Delivery::NoDetection
                    || (!matches!(uplink.crc, CrcSize::None)
                        && !matches!(downlink.crc, CrcSize::None)),
                "error detection requires CRC in both directions"
            );
            Ok(CircuitSubflow {
                uplink,
                downlink,
                empty: if flow.empty_block {
                    EmptyTransport::ZeroSizeBlock
                } else {
                    EmptyTransport::NoBlock
                },
            })
        })
        .collect::<Result<_>>()?;
    let mut seen = BTreeSet::new();
    for format in &input.formats {
        ensure!(
            format.subflow_bits.len() == input.subflows.len() && seen.insert(&format.subflow_bits),
            "circuit formats must be distinct indexed rows covering every subflow"
        );
        ensure!(
            format
                .subflow_bits
                .iter()
                .map(|bits| usize::from(*bits))
                .sum::<usize>()
                <= usize::from(u16::MAX),
            "circuit SDU exceeds RANAP size range"
        );
    }
    Ok(CircuitProfile {
        contract,
        interval,
        subflows,
        supported_formats: input
            .formats
            .iter()
            .map(|format| format.subflow_bits.clone())
            .collect(),
        versions: input.versions,
    })
}

#[cfg(test)]
mod tests;
