// SPDX-License-Identifier: AGPL-3.0-or-later

use super::ConfigurationError;
use crate::dedicated::{DedicatedError, NormalDpch};
use std::num::NonZeroU8;
use umts_rrc_schema::information_elements as ie;

const MAX_POWER_INDEX: u8 = 8;
const MAX_REPETITION: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarqPreamble {
    Mode0,
    Mode1,
}

#[derive(Clone, Copy, Debug)]
pub struct AckFeedback {
    ack_power_index: u8,
    nack_power_index: u8,
    repetition: NonZeroU8,
}

impl AckFeedback {
    pub fn new(
        ack_power_index: u8,
        nack_power_index: u8,
        repetition: NonZeroU8,
    ) -> Result<Self, ConfigurationError> {
        if ack_power_index > MAX_POWER_INDEX
            || nack_power_index > MAX_POWER_INDEX
            || repetition.get() > MAX_REPETITION
        {
            return Err(ConfigurationError::Feedback);
        }
        Ok(Self {
            ack_power_index,
            nack_power_index,
            repetition,
        })
    }

    pub fn ack_power_index(self) -> u8 {
        self.ack_power_index
    }

    pub fn nack_power_index(self) -> u8 {
        self.nack_power_index
    }

    pub fn repetition(self) -> NonZeroU8 {
        self.repetition
    }

    pub fn power_r5(
        self,
        dedicated: &NormalDpch,
    ) -> Result<ie::ULDPCHPowerControlInfoR5, DedicatedError> {
        let power = dedicated.power_wire()?;
        Ok(ie::ULDPCHPowerControlInfoR5::fdd(
            ie::ULDPCHPowerControlInfoR5Fdd::new(
                power.dpcch_power_offset,
                power.pc_preamble,
                power.s_rb_delay,
                power.power_control_algorithm,
                Some(ie::DeltaACK(self.ack_power_index)),
                Some(ie::DeltaNACK(self.nack_power_index)),
                Some(ie::ACKNACKRepetitionFactor(self.repetition.get())),
            ),
        ))
    }

    pub fn power_r6(
        self,
        dedicated: &NormalDpch,
        preamble: HarqPreamble,
    ) -> Result<ie::ULDPCHPowerControlInfoR6, DedicatedError> {
        let power = dedicated.power_wire()?;
        let mode = match preamble {
            HarqPreamble::Mode0 => 0,
            HarqPreamble::Mode1 => 1,
        };
        Ok(ie::ULDPCHPowerControlInfoR6::fdd(
            ie::ULDPCHPowerControlInfoR6Fdd::new(
                power.dpcch_power_offset,
                power.pc_preamble,
                power.s_rb_delay,
                power.power_control_algorithm,
                Some(ie::DeltaACK(self.ack_power_index)),
                Some(ie::DeltaNACK(self.nack_power_index)),
                Some(ie::ACKNACKRepetitionFactor(self.repetition.get())),
                ie::HARQPreambleMode(mode),
            ),
        ))
    }
}
