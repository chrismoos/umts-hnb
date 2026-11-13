// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    CircuitProfile, Coding, Contract, Error, MAX_COMBINATIONS, PacketProfile, Rel99Rlc,
    Requirements, UserRlcPolicy,
};
use umts_iuh::ranap::rab::{Mode, qos};
use umts_rrc::transport::{CrcSize, TransportInterval};
use umts_types::CnDomain;

const MAX_SUBFLOWS: usize = 7;
pub(super) const MAX_PACKET_BLOCKS: u16 = 31;
const BITS_PER_OCTET: usize = u8::BITS as usize;
const AM_HEADER_BITS: u16 = 16;
const UM_HEADER_BITS: u16 = 8;
const MILLISECONDS_PER_SECOND: u128 = 1000;
const ERROR_RATIO_BASE: u64 = 10;
const FRAME_MILLISECONDS: u16 = 10;

pub(super) fn packet_policy(
    requirements: &Requirements<'_>,
    profile: &PacketProfile,
) -> Result<UserRlcPolicy, Error> {
    let maximum = std::num::NonZeroUsize::new(
        usize::from(requirements.parameters.maximum_sdu_bits.get()).div_ceil(BITS_PER_OCTET),
    )
    .ok_or(Error::Contract {
        rab: requirements.rab,
    })?;
    match profile.policy {
        UserRlcPolicy::Acknowledged {
            network,
            mut limits,
        } if maximum <= limits.maximum_sdu_bytes => {
            // TS 23.107 6.4.4.1 separates QoS conformance size from implementation-specific packet handling.
            limits.maximum_sdu_bytes =
                maximum.max(profile.packet_capacity.min(limits.maximum_sdu_bytes));
            Ok(UserRlcPolicy::Acknowledged { network, limits })
        }
        UserRlcPolicy::Unacknowledged {
            mut uplink,
            mut downlink,
        } if maximum <= uplink.maximum_sdu_bytes && maximum <= downlink.maximum_sdu_bytes => {
            uplink.maximum_sdu_bytes =
                maximum.max(profile.packet_capacity.min(uplink.maximum_sdu_bytes));
            downlink.maximum_sdu_bytes =
                maximum.max(profile.packet_capacity.min(downlink.maximum_sdu_bytes));
            Ok(UserRlcPolicy::Unacknowledged { uplink, downlink })
        }
        _ => Err(Error::Contract {
            rab: requirements.rab,
        }),
    }
}

fn interval_ms(interval: TransportInterval) -> u16 {
    FRAME_MILLISECONDS
        * match interval {
            TransportInterval::Ms10 => 1,
            TransportInterval::Ms20 => 2,
            TransportInterval::Ms40 => 4,
            TransportInterval::Ms80 => 8,
        }
}

fn contract(requirements: &Requirements<'_>, offered: &Contract) -> Result<(), Error> {
    let requested = requirements.parameters;
    let reject = || Error::Contract {
        rab: requirements.rab,
    };
    if offered.traffic_class != requested.traffic_class
        || offered.delivery_order != requested.delivery_order
        || offered.subflows.len() != requested.subflows.as_slice().len()
        || offered.subflows.is_empty()
        || offered.subflows.len() > MAX_SUBFLOWS
        || requested.transfer_delay.is_some_and(|delay| {
            offered
                .maximum_transfer_delay_ms
                .is_none_or(|maximum| maximum > delay)
        })
    {
        return Err(reject());
    }
    for (offer, request) in offered.subflows.iter().zip(requested.subflows.as_slice()) {
        // TS 25.413 SDU parameters map Yes to NoDetection for transparent Iu user plane, not RLC TM.
        let normalized = requirements.user_plane.mode == Mode::Transparent
            && request.delivery == qos::Delivery::Yes;
        let delivery = if normalized {
            qos::Delivery::NoDetection
        } else {
            request.delivery
        };
        let sdu_error = if normalized {
            None
        } else {
            request.sdu_error.as_ref()
        };
        if offer.delivery != delivery
            || !error_at_most(
                offer.residual.mantissa(),
                offer.residual.exponent(),
                request.residual_error.mantissa(),
                request.residual_error.exponent(),
            )
            || match (offer.sdu.as_ref(), sdu_error) {
                (Some(offer), Some(request)) => !error_at_most(
                    offer.mantissa(),
                    offer.exponent(),
                    request.mantissa(),
                    request.exponent(),
                ),
                (None, None) => false,
                _ => true,
            }
        {
            return Err(reject());
        }
    }
    Ok(())
}
fn error_at_most(
    mantissa: u8,
    exponent: u8,
    requested_mantissa: u8,
    requested_exponent: u8,
) -> bool {
    u64::from(mantissa) * ERROR_RATIO_BASE.pow(u32::from(requested_exponent))
        <= u64::from(requested_mantissa) * ERROR_RATIO_BASE.pow(u32::from(exponent))
}
fn detection(coding: Coding, delivery: qos::Delivery) -> bool {
    matches!(delivery, qos::Delivery::NoDetection) || !matches!(coding.crc, CrcSize::None)
}
pub(super) fn validate_circuit(
    requirements: &Requirements<'_>,
    profile: &CircuitProfile,
) -> Result<(), Error> {
    contract(requirements, &profile.contract)?;
    if requirements.domain != CnDomain::Cs
        || requirements.user_plane.mode != Mode::PredefinedSdu
        || requirements.user_plane.version_mask() & profile.versions == 0
        || profile.subflows.len() != profile.contract.subflows.len()
        || profile.supported_formats.is_empty()
        || profile.supported_formats.len() > MAX_COMBINATIONS
        || profile
            .supported_formats
            .iter()
            .any(|row| row.len() != profile.subflows.len())
        || profile
            .contract
            .maximum_transfer_delay_ms
            .is_some_and(|delay| delay < interval_ms(profile.interval))
    {
        return Err(Error::Unsupported);
    }
    for (coding, contract) in profile.subflows.iter().zip(&profile.contract.subflows) {
        if !detection(coding.uplink, contract.delivery)
            || !detection(coding.downlink, contract.delivery)
        {
            return Err(Error::Contract {
                rab: requirements.rab,
            });
        }
    }
    let mut maximum_payload = 0u128;
    for row in &requirements.combinations {
        if !profile
            .supported_formats
            .iter()
            .any(|allowed| allowed.as_slice() == row.subflow_bits.as_ref())
        {
            return Err(Error::Contract {
                rab: requirements.rab,
            });
        }
        if row.bitrate.is_some_and(|rate| {
            rate != 0
                && u128::from(rate) * u128::from(interval_ms(profile.interval))
                    != row
                        .subflow_bits
                        .iter()
                        .map(|bits| u128::from(*bits))
                        .sum::<u128>()
                        * MILLISECONDS_PER_SECOND
        }) {
            return Err(Error::Rate {
                rab: requirements.rab,
            });
        }
        let payload = row
            .subflow_bits
            .iter()
            .map(|bits| u128::from(*bits))
            .sum::<u128>()
            * MILLISECONDS_PER_SECOND;
        maximum_payload = maximum_payload.max(payload);
        if requirements.maximum.uplink().is_none_or(|rate| {
            u128::from(rate) * u128::from(interval_ms(profile.interval)) < payload
        }) || requirements.maximum.downlink().is_none_or(|rate| {
            u128::from(rate) * u128::from(interval_ms(profile.interval)) < payload
        }) {
            return Err(Error::Rate {
                rab: requirements.rab,
            });
        }
    }
    if requirements.guaranteed.is_some_and(|rates| {
        rates
            .uplink()
            .into_iter()
            .chain(rates.downlink())
            .any(|rate| {
                u128::from(rate) * u128::from(interval_ms(profile.interval)) > maximum_payload
            })
    }) {
        return Err(Error::Rate {
            rab: requirements.rab,
        });
    }
    Ok(())
}
pub(super) fn validate_packet(
    requirements: &Requirements<'_>,
    profile: &PacketProfile,
) -> Result<[u16; 2], Error> {
    contract(requirements, &profile.contract)?;
    if requirements.domain != CnDomain::Ps
        || requirements.user_plane.mode != Mode::Transparent
        || profile.contract.subflows.len() != 1
    {
        return Err(Error::Unsupported);
    }
    let (header, maximum_sdu) = match (&profile.rlc, profile.policy) {
        (Rel99Rlc::Acknowledged { downlink, .. }, UserRlcPolicy::Acknowledged { limits, .. }) => {
            if profile.contract.delivery_order == qos::DeliveryOrder::Requested
                && !downlink.in_sequence_delivery
            {
                return Err(Error::Contract {
                    rab: requirements.rab,
                });
            }
            (AM_HEADER_BITS, limits.maximum_sdu_bytes.get())
        }
        (Rel99Rlc::Unacknowledged, UserRlcPolicy::Unacknowledged { uplink, downlink }) => (
            UM_HEADER_BITS,
            uplink
                .maximum_sdu_bytes
                .get()
                .min(downlink.maximum_sdu_bytes.get()),
        ),
        _ => return Err(Error::Unsupported),
    };
    if usize::from(requirements.parameters.maximum_sdu_bits.get())
        > maximum_sdu.saturating_mul(BITS_PER_OCTET)
        || requirements.parameters.maximum_sdu_bits.get() == 0
        || !matches!(
            profile.contract.subflows[0].delivery,
            qos::Delivery::No | qos::Delivery::NoDetection
        )
    {
        return Err(Error::Contract {
            rab: requirements.rab,
        });
    }
    let mut counts = [0; 2];
    for (index, channel) in [profile.uplink, profile.downlink].into_iter().enumerate() {
        if !channel.rlc_bits.is_multiple_of(BITS_PER_OCTET as u16)
            || channel.payload_budget_bits == 0
            || channel
                .rlc_bits
                .checked_sub(header)
                .is_none_or(|bits| channel.payload_budget_bits > bits)
            || (channel.maximum_blocks == 0 && index == 0)
            || channel.maximum_blocks > MAX_PACKET_BLOCKS
            || !detection(channel.coding, profile.contract.subflows[0].delivery)
            || profile
                .contract
                .maximum_transfer_delay_ms
                .is_some_and(|delay| delay < interval_ms(channel.interval))
        {
            return Err(Error::Unsupported);
        }
        let maximum = match index {
            0 => requirements.maximum.uplink(),
            _ => requirements.maximum.downlink(),
        }
        .ok_or(Error::Unsupported)?;
        let guaranteed = requirements
            .guaranteed
            .and_then(|rates| match index {
                0 => rates.uplink(),
                _ => rates.downlink(),
            })
            .unwrap_or(0);
        let payload = u128::from(channel.payload_budget_bits) * MILLISECONDS_PER_SECOND;
        let needed =
            (u128::from(maximum) * u128::from(interval_ms(channel.interval))).div_ceil(payload);
        // A downlink channel with no blocks uses HS-DSCH for data. Its transport format
        // fixes the MAC-d PDU size.
        counts[index] = u16::try_from(needed.min(u128::from(channel.maximum_blocks)))
            .map_err(|_| Error::Capacity)?
            .max(u16::from(channel.maximum_blocks != 0));
        if u128::from(guaranteed) * u128::from(interval_ms(channel.interval))
            > u128::from(counts[index]) * payload
        {
            return Err(Error::Rate {
                rab: requirements.rab,
            });
        }
    }
    Ok(counts)
}

pub(super) fn packet_minimum(
    requirements: &Requirements<'_>,
    profile: &PacketProfile,
) -> Result<[u16; 2], Error> {
    let mut result = [0; 2];
    for (index, channel) in [profile.uplink, profile.downlink].into_iter().enumerate() {
        let guaranteed = requirements
            .guaranteed
            .and_then(|rates| {
                if index == 0 {
                    rates.uplink()
                } else {
                    rates.downlink()
                }
            })
            .unwrap_or(0);
        let numerator = u128::from(guaranteed) * u128::from(interval_ms(channel.interval));
        let denominator = u128::from(channel.payload_budget_bits) * MILLISECONDS_PER_SECOND;
        if denominator == 0 {
            return Err(Error::Unsupported);
        }
        result[index] =
            u16::try_from(numerator.div_ceil(denominator)).map_err(|_| Error::Rate {
                rab: requirements.rab,
            })?;
    }
    Ok(result)
}

pub(super) fn validate_guaranteed<'a>(
    configuration: &umts_rrc::dedicated::Rel99Configuration,
    rabs: impl IntoIterator<Item = &'a super::RabPlan>,
) -> Result<(), Error> {
    use std::collections::BTreeSet;
    use umts_rrc::dedicated::DchDirection;
    let rabs: Vec<_> = rabs.into_iter().collect();
    for (direction, index, tfcs) in [
        (DchDirection::Uplink, 0, configuration.uplink()),
        (DchDirection::Downlink, 1, configuration.downlink()),
    ] {
        let mut voice_columns = BTreeSet::new();
        let mut targets = Vec::new();
        for rab in &rabs {
            if rab.domain == CnDomain::Cs {
                for id in &rab.bearers {
                    let bearer = configuration
                        .bearer(*id)
                        .ok_or_else(|| crate::rel99_planner::trace_identity("service-1"))?;
                    for mapping in bearer.mappings(direction) {
                        voice_columns.insert(
                            tfcs.transports()
                                .iter()
                                .position(|channel| channel.id == mapping.transport)
                                .ok_or_else(|| crate::rel99_planner::trace_identity("service-2"))?,
                        );
                    }
                }
                continue;
            }
            let rate = rab
                .guaranteed
                .and_then(|rates| {
                    if index == 0 {
                        rates.uplink()
                    } else {
                        rates.downlink()
                    }
                })
                .unwrap_or(0);
            if rate == 0 {
                continue;
            }
            let [id] = rab.bearers.as_slice() else {
                return Err(Error::Unsupported);
            };
            let bearer = configuration
                .bearer(*id)
                .ok_or_else(|| crate::rel99_planner::trace_identity("service-3"))?;
            let mut mappings = bearer.mappings(direction);
            let mapping = mappings
                .next()
                .ok_or_else(|| crate::rel99_planner::trace_identity("service-4"))?;
            if mappings.next().is_some() {
                return Err(Error::Unsupported);
            }
            let column = tfcs
                .transports()
                .iter()
                .position(|channel| channel.id == mapping.transport)
                .ok_or_else(|| crate::rel99_planner::trace_identity("service-5"))?;
            let payload = rab
                .packet_payload_budget_bits
                .ok_or_else(|| crate::rel99_planner::trace_identity("service-6"))?[index];
            targets.push((column, payload, rate, rab.rab));
        }
        if targets.is_empty() {
            continue;
        }
        let mut required = BTreeSet::new();
        let mut fulfilled = BTreeSet::new();
        for row in tfcs.combinations() {
            let state: Vec<_> = voice_columns
                .iter()
                .map(|column| row.formats[*column])
                .collect();
            required.insert(state.clone());
            let mut valid = true;
            for (column, payload, rate, _) in &targets {
                let channel = &tfcs.transports()[*column];
                let format = channel
                    .formats()
                    .nth(row.formats[*column])
                    .ok_or_else(|| crate::rel99_planner::trace_identity("service-7"))?;
                valid &= u128::from(format.blocks) * u128::from(*payload) * MILLISECONDS_PER_SECOND
                    >= u128::from(*rate) * u128::from(interval_ms(channel.interval));
            }
            if valid {
                fulfilled.insert(state);
            }
        }
        if !required.is_subset(&fulfilled) {
            return Err(Error::Rate { rab: targets[0].3 });
        }
    }
    Ok(())
}
