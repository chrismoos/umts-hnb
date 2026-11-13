// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{configured, contract};
use crate::{
    dedicated_bearers::{NetworkAmPolicy, UserRlcPolicy},
    rel99_planner::{HighSpeedDownlink, PacketChannel, PacketProfile},
};
use anyhow::{Context, Result, anyhow, ensure};
use rasn::types::Enumerated;
use std::num::{NonZeroU16, NonZeroU64, NonZeroUsize};
use umts_config::{
    R99AmPeer, R99AmPolicy, R99BufferLimits, R99NetworkAmPolicy, R99PacketChannel, R99PacketService,
};
use umts_rlc::{
    acknowledged::{AmConfig, AmLimits, Polling},
    unacknowledged::{EInterpretation, LiWidth, UmConfig, UmLimits},
};
use umts_rrc::dedicated::Rel99Rlc;
use umts_rrc_schema::information_elements as ie;

const OCTET_BITS: u16 = 8;
const AM_HEADER_BITS: u16 = 16;
const UM_HEADER_BITS: u16 = 8;
const MAX_BLOCKS: u16 = 31;
const MAX_WINDOW: u16 = 2047;
const MAX_SDU_BYTES: u32 = 65_536;
const MAX_BASE_UM_DOWNLINK_OCTETS: usize = 125;
const MIN_UM_DOWNLINK_OCTETS: usize = 3;
const CHIPS_PER_MILLISECOND: u64 = 3840;

pub(super) fn compile(
    input: &R99PacketService,
    packet_capacity: NonZeroUsize,
    high_speed: Option<HighSpeedDownlink>,
) -> Result<PacketProfile> {
    let mut profile = compile_one(input, packet_capacity)?;
    // UM downlink PDUs are bounded by the base length-indicator range, so only AM is resized.
    if let (Some(high_speed), Some(am)) = (high_speed, &input.am) {
        let window = am.ue.downlink_window.min(high_speed.receive_window);
        if input.downlink.rlc_bits == high_speed.rlc_bits && am.ue.downlink_window == window {
            return Ok(profile);
        }
        let mut resized = input.clone();
        resized.downlink.rlc_bits = high_speed.rlc_bits;
        if let Some(am) = &mut resized.am {
            am.ue.downlink_window = window;
        }
        profile.high_speed = Some(Box::new(
            compile_one(&resized, packet_capacity).context("HS-DSCH downlink RLC")?,
        ));
    }
    Ok(profile)
}

fn compile_one(input: &R99PacketService, packet_capacity: NonZeroUsize) -> Result<PacketProfile> {
    let contract = contract(&input.contract)?;
    ensure!(
        contract.subflows[0].delivery != umts_iuh::ranap::rab::qos::Delivery::Yes,
        "transparent Iu packet services must declare the effective NoDetection contract instead of error indication"
    );
    ensure!(
        contract.subflows.len() == 1,
        "packet services require exactly one subflow"
    );
    let am = match (&input.am, &input.um) {
        (Some(_), None) => true,
        (None, Some(_)) => false,
        _ => {
            return Err(anyhow!(
                "packet service requires exactly one AM or UM policy"
            ));
        }
    };
    let uplink = channel(&input.uplink, am)?;
    let downlink = channel(&input.downlink, am)?;
    ensure!(
        input
            .contract
            .maximum_transfer_delay_ms
            .is_none_or(|delay| delay
                >= u16::from(input.uplink.interval_ms.max(input.downlink.interval_ms))),
        "packet service interval exceeds transfer delay"
    );
    if contract.subflows[0].delivery != umts_iuh::ranap::rab::qos::Delivery::NoDetection {
        ensure!(
            !matches!(uplink.coding.crc, umts_rrc::transport::CrcSize::None)
                && !matches!(downlink.coding.crc, umts_rrc::transport::CrcSize::None),
            "packet error detection requires CRC in both directions"
        );
    }
    let (rlc, policy) = if let Some(config) = &input.am {
        // TS 25.413 Delivery Order and TS 25.331 In-sequence delivery must describe the same service.
        ensure!(
            contract.delivery_order != umts_iuh::ranap::rab::qos::DeliveryOrder::Requested
                || config.ue.in_sequence_delivery,
            "ordered AM service requires UE in-sequence delivery"
        );
        acknowledged(config, uplink, downlink)?
    } else {
        let config = input.um.as_ref().expect("one explicit RLC construction");
        let receive = um_limits(&config.uplink, uplink.rlc_bits)?;
        let transmit = um_limits(&config.downlink, downlink.rlc_bits)?;
        ensure!(
            transmit.maximum_pdu_bytes.get() >= MIN_UM_DOWNLINK_OCTETS,
            "UM downlink requires at least {MIN_UM_DOWNLINK_OCTETS} PDU octets"
        );
        ensure!(
            transmit.maximum_pdu_bytes.get() <= MAX_BASE_UM_DOWNLINK_OCTETS,
            "base-wire UM downlink requires at most {MAX_BASE_UM_DOWNLINK_OCTETS} PDU octets"
        );
        let receive_config = UmConfig {
            li_width: if receive.maximum_pdu_bytes.get() <= MAX_BASE_UM_DOWNLINK_OCTETS {
                LiWidth::Seven
            } else {
                LiWidth::Fifteen
            },
            interpretation: EInterpretation::Normal,
        };
        let transmit_config = UmConfig {
            li_width: LiWidth::Seven,
            interpretation: EInterpretation::Normal,
        };
        receive
            .validate(receive_config)
            .map_err(|error| anyhow!("UM uplink limits: {error:?}"))?;
        transmit
            .validate(transmit_config)
            .map_err(|error| anyhow!("UM downlink limits: {error:?}"))?;
        ensure!(
            receive
                .receive_buffer_bytes()
                .map_err(|error| anyhow!("UM uplink storage: {error:?}"))?
                <= receive.maximum_buffer_bytes,
            "UM uplink storage budget exceeded"
        );
        ensure!(
            transmit
                .transmit_buffer_bytes()
                .map_err(|error| anyhow!("UM downlink storage: {error:?}"))?
                <= transmit.maximum_buffer_bytes,
            "UM downlink storage budget exceeded"
        );
        (
            Rel99Rlc::Unacknowledged,
            UserRlcPolicy::Unacknowledged {
                uplink: receive,
                downlink: transmit,
            },
        )
    };
    Ok(PacketProfile {
        contract,
        rlc,
        policy,
        packet_capacity,
        uplink,
        downlink,
        high_speed: None,
    })
}

fn channel(input: &R99PacketChannel, am: bool) -> Result<PacketChannel> {
    let header = if am { AM_HEADER_BITS } else { UM_HEADER_BITS };
    ensure!(
        input.rlc_bits.is_multiple_of(OCTET_BITS)
            && input.rlc_bits > header
            && input.payload_budget_bits > 0
            && input.payload_budget_bits <= input.rlc_bits - header,
        "packet payload budget must fit its octet-aligned RLC PDU after the header"
    );
    // Zero blocks is a downlink channel whose data rides HS-DSCH only, the service
    // validation keeps the uplink at one block or more.
    ensure!(
        (0..=MAX_BLOCKS).contains(&input.maximum_blocks),
        "packet block budget must be 0..={MAX_BLOCKS}"
    );
    Ok(PacketChannel {
        interval: configured::interval(input.interval_ms)?,
        rlc_bits: input.rlc_bits,
        payload_budget_bits: input.payload_budget_bits,
        maximum_blocks: input.maximum_blocks,
        coding: configured::coding(&input.coding)?,
    })
}

fn acknowledged(
    input: &R99AmPolicy,
    uplink: PacketChannel,
    downlink: PacketChannel,
) -> Result<(Rel99Rlc, UserRlcPolicy)> {
    let peer = peer(&input.ue)?;
    let network = network(&input.network)?;
    let limits = am_limits(&input.buffers)?;
    let config = AmConfig {
        transmit_pdu_bytes: count(u32::from(downlink.rlc_bits / OCTET_BITS))?,
        receive_pdu_bytes: count(u32::from(uplink.rlc_bits / OCTET_BITS))?,
        // TS 25.331 10.3.4.23 pairs each network window with the opposite UE window.
        transmit_window: nonzero(input.ue.downlink_window)?,
        receive_window: nonzero(input.ue.uplink_window)?,
        maximum_data_attempts: network.maximum_data_attempts,
        maximum_reset_attempts: network.maximum_reset_attempts,
        reset_timeout_chips: network.reset_timeout_chips,
        polling: network.polling,
        status_prohibit_chips: network.status_prohibit_chips,
        missing_pdu_indicator: network.missing_pdu_indicator,
        accept_special_he: false,
        initial_tx_hfn: 0,
        initial_rx_hfn: 0,
    };
    let bytes = config
        .buffer_bytes(limits)
        .map_err(|error| anyhow!("AM limits: {error:?}"))?;
    ensure!(
        bytes <= limits.maximum_buffer_bytes,
        "AM storage budget exceeded"
    );
    Ok((peer, UserRlcPolicy::Acknowledged { network, limits }))
}

fn peer(input: &R99AmPeer) -> Result<Rel99Rlc> {
    ensure!(
        input.uplink_window <= MAX_WINDOW && input.downlink_window <= MAX_WINDOW,
        "AM windows above {MAX_WINDOW} are not implemented"
    );
    let polling = ie::PollingInfo::new(
        input
            .poll_prohibit_ms
            .map(|v| enumerated(v, "tpp"))
            .transpose()?,
        input
            .poll_timeout_ms
            .map(|v| enumerated(v, "tp"))
            .transpose()?,
        input
            .poll_every_pdus
            .map(|v| enumerated(v, "pdu"))
            .transpose()?,
        None,
        input.last_transmission,
        input.last_retransmission,
        None,
        None,
    );
    Ok(Rel99Rlc::Acknowledged {
        uplink: ie::ULAMRLCMode::new(
            ie::TransmissionRLCDiscard::noDiscard(enumerated(input.maximum_data_attempts, "dat")?),
            enumerated(input.uplink_window, "tw")?,
            enumerated(input.reset_timeout_ms, "tr")?,
            enumerated(input.maximum_reset_attempts, "rst")?,
            Some(polling),
        ),
        downlink: ie::DLAMRLCMode::new(
            input.in_sequence_delivery,
            enumerated(input.downlink_window, "rw")?,
            ie::DLRLCStatusInfo::new(
                input
                    .status_prohibit_ms
                    .map(|v| enumerated(v, "tsp"))
                    .transpose()?,
                None,
                input.missing_pdu_indicator,
                None,
            ),
        ),
    })
}

fn network(input: &R99NetworkAmPolicy) -> Result<NetworkAmPolicy> {
    Ok(NetworkAmPolicy {
        maximum_data_attempts: nonzero(input.maximum_data_attempts)?,
        maximum_reset_attempts: nonzero(input.maximum_reset_attempts)?,
        reset_timeout_chips: chips(input.reset_timeout_ms)?,
        polling: Polling {
            every_pdus: input.poll_every_pdus.map(nonzero).transpose()?,
            last_transmission: input.last_transmission,
            last_retransmission: input.last_retransmission,
            timeout_chips: input.poll_timeout_ms.map(chips).transpose()?,
            prohibit_chips: input.poll_prohibit_ms.map(chips).transpose()?,
        },
        status_prohibit_chips: input.status_prohibit_ms.map(chips).transpose()?,
        missing_pdu_indicator: input.missing_pdu_indicator,
    })
}

// TS 25.331 ASN.1 enumerator identifiers bind actual numeric values, not enum ordinal indices.
fn enumerated<T: Enumerated>(value: u16, prefix: &str) -> Result<T> {
    T::from_identifier(&format!("{prefix}{value}")).ok_or_else(|| {
        anyhow!(
            "{} value {value} is not enumerated",
            std::any::type_name::<T>()
        )
    })
}
fn chips(milliseconds: u16) -> Result<NonZeroU64> {
    NonZeroU64::new(u64::from(milliseconds) * CHIPS_PER_MILLISECOND)
        .ok_or_else(|| anyhow!("timer must be nonzero when present"))
}
fn nonzero(value: u16) -> Result<NonZeroU16> {
    NonZeroU16::new(value).ok_or_else(|| anyhow!("value must be nonzero"))
}
fn count(value: u32) -> Result<NonZeroUsize> {
    usize::try_from(value)
        .ok()
        .and_then(NonZeroUsize::new)
        .ok_or_else(|| anyhow!("size must be nonzero and fit the host address space"))
}
fn am_limits(input: &R99BufferLimits) -> Result<AmLimits> {
    ensure!(
        input.maximum_sdu_bytes <= MAX_SDU_BYTES,
        "maximum_sdu_bytes exceeds {MAX_SDU_BYTES}"
    );
    Ok(AmLimits {
        maximum_sdu_bytes: count(input.maximum_sdu_bytes)?,
        queued_sdus: count(u32::from(input.queued_sdus))?,
        maximum_buffer_bytes: count(input.maximum_buffer_bytes)?.get(),
    })
}
fn um_limits(input: &R99BufferLimits, bits: u16) -> Result<UmLimits> {
    let limits = am_limits(input)?;
    Ok(UmLimits {
        maximum_pdu_bytes: count(u32::from(bits / OCTET_BITS))?,
        maximum_sdu_bytes: limits.maximum_sdu_bytes,
        queued_sdus: limits.queued_sdus,
        maximum_buffer_bytes: limits.maximum_buffer_bytes,
    })
}
