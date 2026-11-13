// SPDX-License-Identifier: AGPL-3.0-or-later

#[path = "../../../test-support/rrc_capabilities.rs"]
pub(crate) mod capabilities;
pub(crate) mod integrity;
#[path = "../../../test-support/rel99_dedicated.rs"]
pub(crate) mod rel99;

use crate::{
    cell::{CellPlan, ReferencePowers},
    common_tx::{CommonAmplitudes, CommonTx},
};
use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
};
use umts_config::Config;
use umts_dsp::{common_downlink::CommonChannelAmplitudes, pulse_shaping::SamplesPerChip};
use umts_rrc::random_access::{
    AccessServiceClass, Aich, AichTiming, MinimumSpreadingFactor, PrachSettings, PuncturingLimit,
    Signatures, Subchannels,
};
use umts_types::{RadioEpoch, time::ChipInstant};

pub(crate) fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}

pub(crate) fn failed_cell_update(
    assigned: umts_rrc::establishment::AssignedRntis,
) -> umts_rrc_schema::class_definitions::ULCCCHMessage {
    use rasn::types::FixedBitString;
    use umts_rrc_schema::{
        class_definitions::{ULCCCHMessage, ULCCCHMessageType},
        information_elements as ie, pdu_definitions as pdu,
    };
    fn fixed<const N: usize>(value: u32) -> FixedBitString<N> {
        let mut result = FixedBitString::default();
        for index in 0..N {
            result.set(index, value & (1 << (N - 1 - index)) != 0);
        }
        result
    }
    ULCCCHMessage::new(
        None,
        ULCCCHMessageType::cellUpdate(pdu::CellUpdate::new(
            ie::URNTI::new(
                ie::SRNCIdentity(fixed::<12>(u32::from(assigned.rnc()))),
                ie::SRNTI(fixed::<20>(assigned.srnti())),
            ),
            ie::STARTList(vec![ie::STARTSingle::new(
                ie::CNDomainIdentity::cs_domain,
                ie::STARTValue(fixed::<20>(1)),
            )]),
            true,
            false,
            ie::CellUpdateCause::rlc_unrecoverableError,
            None,
            ie::RbTimerIndicator::new(false, false),
            None,
            None,
        )),
    )
}
pub(crate) fn count(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

pub(crate) fn plan(compact: bool) -> CellPlan {
    let mut config = Config::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../rust/test-support/fixtures/config/serving.yaml"),
    )
    .unwrap();
    config.hspa.downlink.compact_common_channels = compact;
    config.cell.physical.downlink_scrambling_code = 469;
    config.cell.access.prach.scrambling_code_word_number = 0;
    config.cell.access.prach.signature = 13;
    config.cell.access.prach.subchannel = 7;
    CellPlan::compile(
        &config,
        at(0).epoch,
        1,
        ReferencePowers {
            cpich_tx_dbm: 10,
            uplink_interference_dbm: -100,
        },
    )
    .unwrap()
}

pub(crate) fn amplitudes() -> CommonAmplitudes {
    CommonAmplitudes {
        primary: CommonChannelAmplitudes {
            cpich: 0.12,
            pccpch: 0.12,
            primary_sch: 0.04,
            secondary_sch: 0.04,
        },
        fach: 0.16,
        pch: 0.16,
    }
}

pub(crate) fn source(compact: bool) -> CommonTx {
    CommonTx::new(
        plan(compact),
        at(0),
        SamplesPerChip::Two,
        32,
        amplitudes(),
        count(8),
        100,
    )
    .unwrap()
}

pub(crate) fn access_settings(
    timing: AichTiming,
    signatures: &[u8],
    subchannels: &[u8],
) -> PrachSettings {
    PrachSettings {
        signatures: Signatures::new(signatures).unwrap(),
        subchannels: Subchannels::new(subchannels).unwrap(),
        minimum_spreading_factor: MinimumSpreadingFactor::Sf32,
        preamble_scrambling_code: 0,
        puncturing_limit: PuncturingLimit::from_percent(100).unwrap(),
        transport_identity: 1,
        access_classes: vec![AccessServiceClass {
            first_signature_index: 0,
            last_signature_index: signatures.len() as u8 - 1,
            assigned_subchannels: 15,
        }],
        ac_to_asc: [0; 7],
        persistence_factors: None,
        primary_cpich_dbm: 10,
        initial_power_constant_db: -10,
        ramp_step_db: 1,
        preamble_retransmissions: 64,
        maximum_ramp_cycles: 32,
        backoff_min: 0,
        backoff_max: 50,
        aich: Aich {
            code256: 2,
            transmit_diversity: false,
            timing,
        },
    }
}
