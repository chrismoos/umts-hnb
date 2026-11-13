// SPDX-License-Identifier: AGPL-3.0-or-later

use super::ConfigurationError;
use std::num::NonZeroU8;
use umts_rrc_schema::information_elements as ie;
use umts_types::channel_codes::OvsfCode;

const CONTROL_SF: u16 = 128;
const MAX_MONITORED_CHANNELS: usize = 4;
const MAX_CQI_REPETITION: u8 = 4;
const MAX_CQI_POWER_INDEX: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedbackCycle {
    Disabled,
    Ms2,
    Ms4,
    Ms8,
    Ms10,
    Ms20,
    Ms40,
    Ms80,
    Ms160,
}

impl FeedbackCycle {
    pub fn period_ttis(self) -> Option<NonZeroU8> {
        let ttis = match self {
            Self::Disabled => return None,
            Self::Ms2 => 1,
            Self::Ms4 => 2,
            Self::Ms8 => 4,
            Self::Ms10 => 5,
            Self::Ms20 => 10,
            Self::Ms40 => 20,
            Self::Ms80 => 40,
            Self::Ms160 => 80,
        };
        NonZeroU8::new(ttis)
    }

    fn wire(self) -> ie::FeedbackCycle {
        match self {
            Self::Disabled => ie::FeedbackCycle::fc0,
            Self::Ms2 => ie::FeedbackCycle::fc2,
            Self::Ms4 => ie::FeedbackCycle::fc4,
            Self::Ms8 => ie::FeedbackCycle::fc8,
            Self::Ms10 => ie::FeedbackCycle::fc10,
            Self::Ms20 => ie::FeedbackCycle::fc20,
            Self::Ms40 => ie::FeedbackCycle::fc40,
            Self::Ms80 => ie::FeedbackCycle::fc80,
            Self::Ms160 => ie::FeedbackCycle::fc160,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MeasurementFeedback {
    power_offset_half_db: i8,
    cycle: FeedbackCycle,
    repetition: NonZeroU8,
    cqi_power_index: u8,
}

impl MeasurementFeedback {
    pub fn new(
        power_offset_half_db: i8,
        cycle: FeedbackCycle,
        repetition: NonZeroU8,
        cqi_power_index: u8,
    ) -> Result<Self, ConfigurationError> {
        if !(-12..=26).contains(&power_offset_half_db)
            || repetition.get() > MAX_CQI_REPETITION
            || cqi_power_index > MAX_CQI_POWER_INDEX
            || cycle
                .period_ttis()
                .is_some_and(|period| period < repetition)
        {
            return Err(ConfigurationError::Feedback);
        }
        Ok(Self {
            power_offset_half_db,
            cycle,
            repetition,
            cqi_power_index,
        })
    }

    pub fn cycle(self) -> FeedbackCycle {
        self.cycle
    }

    pub fn repetition(self) -> NonZeroU8 {
        self.repetition
    }

    pub fn power_offset_half_db(self) -> i8 {
        self.power_offset_half_db
    }

    pub fn cqi_power_index(self) -> u8 {
        self.cqi_power_index
    }

    pub fn wire(self) -> ie::MeasurementFeedbackInfo {
        ie::MeasurementFeedbackInfo::new(ie::MeasurementFeedbackInfoModeSpecificInfo::fdd(
            ie::MeasurementFeedbackInfoModeSpecificInfoFdd::new(
                ie::MeasurementPowerOffset(self.power_offset_half_db),
                self.cycle.wire(),
                ie::CQIRepetitionFactor(self.repetition.get()),
                ie::DeltaCQI(self.cqi_power_index),
            ),
        ))
    }
}

#[derive(Clone)]
pub struct ControlChannels {
    codes: Box<[OvsfCode]>,
}

impl ControlChannels {
    pub fn new(codes: &[OvsfCode]) -> Result<Self, ConfigurationError> {
        if codes.is_empty() || codes.len() > MAX_MONITORED_CHANNELS {
            return Err(ConfigurationError::ControlCodes);
        }
        for (index, code) in codes.iter().enumerate() {
            if code.spreading_factor() != CONTROL_SF || codes[..index].contains(code) {
                return Err(ConfigurationError::ControlCodes);
            }
        }
        Ok(Self {
            codes: codes.into(),
        })
    }

    pub fn codes(&self) -> &[OvsfCode] {
        &self.codes
    }

    pub fn downlink_r5(&self, feedback: MeasurementFeedback) -> ie::DLHSPDSCHInformation {
        // TS 25.331 10.3.6.36a defaults omitted scrambling to the primary CPICH code.
        let control = ie::HSSCCHInfo::new(ie::HSSCCHInfoModeSpecificInfo::fdd(
            ie::HSSCCHInfoModeSpecificInfoFdd::new(self.wire_codes(), None),
        ));
        ie::DLHSPDSCHInformation::new(
            Some(control),
            Some(feedback.wire()),
            ie::DLHSPDSCHInformationModeSpecificInfo::fdd(()),
        )
    }

    pub fn downlink_r6(&self, feedback: MeasurementFeedback) -> ie::DLHSPDSCHInformationR6 {
        let control = ie::HSSCCHInfoR6::new(ie::HSSCCHInfoR6ModeSpecificInfo::fdd(
            ie::HSSCCHInfoR6ModeSpecificInfoFdd::new(self.wire_codes(), None),
        ));
        ie::DLHSPDSCHInformationR6::new(
            Some(control),
            Some(feedback.wire()),
            ie::DLHSPDSCHInformationR6ModeSpecificInfo::fdd(()),
        )
    }

    pub fn downlink_evolved(
        &self,
        feedback: MeasurementFeedback,
        qam64: bool,
    ) -> ie::DLHSPDSCHInformationR7 {
        let mut value = self.downlink_r7(feedback);
        if !qam64
            && let ie::DLHSPDSCHInformationR7ModeSpecificInfo::fdd(fdd) =
                &mut value.mode_specific_info
        {
            fdd.dl_64_qam_configured = None;
        }
        value
    }

    pub fn downlink_r7(&self, feedback: MeasurementFeedback) -> ie::DLHSPDSCHInformationR7 {
        let cycle = match feedback.cycle {
            FeedbackCycle::Disabled => ie::FeedbackCycleR7::fc0,
            FeedbackCycle::Ms2 => ie::FeedbackCycleR7::fc2,
            FeedbackCycle::Ms4 => ie::FeedbackCycleR7::fc4,
            FeedbackCycle::Ms8 => ie::FeedbackCycleR7::fc8,
            FeedbackCycle::Ms10 => ie::FeedbackCycleR7::fc10,
            FeedbackCycle::Ms20 => ie::FeedbackCycleR7::fc20,
            FeedbackCycle::Ms40 => ie::FeedbackCycleR7::fc40,
            FeedbackCycle::Ms80 => ie::FeedbackCycleR7::fc80,
            FeedbackCycle::Ms160 => ie::FeedbackCycleR7::fc160,
        };
        ie::DLHSPDSCHInformationR7::new(
            Some(ie::HSSCCHInfoR7::new(
                ie::HSSCCHInfoR7ModeSpecificInfo::fdd(ie::HSSCCHInfoR7ModeSpecificInfoFdd::new(
                    self.wire_codes(),
                    None,
                )),
            )),
            Some(ie::MeasurementFeedbackInfoR7::new(
                ie::MeasurementFeedbackInfoR7ModeSpecificInfo::fdd(
                    ie::MeasurementFeedbackInfoR7ModeSpecificInfoFdd::new(
                        ie::MeasurementPowerOffset(feedback.power_offset_half_db),
                        cycle,
                        ie::CQIRepetitionFactor(feedback.repetition.get()),
                        ie::DeltaCQI(feedback.cqi_power_index),
                    ),
                ),
            )),
            ie::DLHSPDSCHInformationR7ModeSpecificInfo::fdd(
                ie::DLHSPDSCHInformationR7ModeSpecificInfoFdd::new(Some(
                    ie::DLHSPDSCHInformationR7ModeSpecificInfoFddDl64QAMConfigured::R_true,
                )),
            ),
        )
    }

    fn wire_codes(&self) -> Vec<ie::HSSCCHCodes> {
        self.codes
            .iter()
            .map(|code| ie::HSSCCHCodes(code.index() as u8))
            .collect()
    }
}
