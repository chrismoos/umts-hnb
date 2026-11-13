// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use umts_dsp::{
    DspError,
    bits::{Bit, unpack_msb},
    broadcast::PAYLOAD_BITS,
    waveform::{BroadcastWaveform, WaveformWindow},
};
use umts_rrc::broadcast::{BroadcastSchedule, ScheduleError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BroadcastError {
    Schedule(ScheduleError),
    Waveform(DspError),
}

pub struct BroadcastSource {
    schedule: BroadcastSchedule,
    waveform: BroadcastWaveform,
}

impl BroadcastSource {
    pub fn new(
        schedule: BroadcastSchedule,
        waveform: BroadcastWaveform,
    ) -> Result<Self, BroadcastError> {
        schedule
            .message_at(waveform.next_chip())
            .map_err(BroadcastError::Schedule)?;
        Ok(Self { schedule, waveform })
    }

    pub fn samples_per_tti(&self) -> usize {
        self.waveform.samples_per_tti()
    }

    pub fn generate(&mut self, output: &mut [Complex32]) -> Result<WaveformWindow, BroadcastError> {
        let time = self.waveform.next_chip();
        let encoded = self
            .schedule
            .message_at(time)
            .map_err(BroadcastError::Schedule)?;
        let mut bits = [Bit::Zero; PAYLOAD_BITS];
        unpack_msb(encoded, &mut bits).map_err(BroadcastError::Waveform)?;
        self.waveform
            .generate(time, &bits, output)
            .map_err(BroadcastError::Waveform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use umts_dsp::{
        common_downlink::CommonChannelAmplitudes, pulse_shaping::SamplesPerChip,
        scrambling::PrimaryScramblingCode,
    };
    use umts_rrc::broadcast::{BroadcastBlock, BroadcastTiming};
    use umts_rrc::master_information::{ReferenceTag, compile_broadcast};
    use umts_rrc::system_information::{
        CsDomainInformation, NetworkMode, PsDomainInformation, Sib1, Sib7,
    };
    use umts_rrc::ue_timers::UeTimerProfile;
    use umts_rrc_codec::SibType;
    use umts_types::{
        RadioEpoch,
        time::{CHIPS_PER_FRAME, ChipInstant, SYSTEM_FRAME_MODULUS},
    };

    fn epoch(value: u64) -> RadioEpoch {
        RadioEpoch::new(NonZeroU64::new(value).unwrap())
    }
    fn schedule() -> BroadcastSchedule {
        let sib1 = Sib1 {
            location_area_code: 1,
            cs: Some(CsDomainInformation {
                periodic_update_decihours: 10,
                attach_detach: true,
                drx_coefficient: 6,
            }),
            ps: Some(PsDomainInformation {
                routing_area_code: 1,
                network_mode: NetworkMode::II,
                drx_coefficient: 6,
            }),
        }
        .encode_with_timers(&UeTimerProfile::release_reference())
        .unwrap();
        let sib7 = Sib7 {
            uplink_interference_dbm: -100,
            sib5_persistence: vec![1],
            sib6_persistence: None,
            expiration_time_factor: Some(1),
        }
        .encode()
        .unwrap();
        let block1 = BroadcastBlock::new(
            SibType::Type1,
            BroadcastTiming::new(8, vec![2]).unwrap(),
            &sib1.bytes,
            sib1.bit_length,
        )
        .unwrap();
        let block7 = BroadcastBlock::new(
            SibType::Type7,
            BroadcastTiming::new(8, vec![4]).unwrap(),
            &sib7.bytes,
            sib7.bit_length,
        )
        .unwrap();
        compile_broadcast(
            epoch(1),
            1,
            [0, 0, 1],
            vec![0, 1],
            vec![
                (block1, ReferenceTag::Value(1)),
                (block7, ReferenceTag::None),
            ],
        )
        .unwrap()
    }
    fn waveform(epoch: RadioEpoch) -> BroadcastWaveform {
        BroadcastWaveform::new(
            PrimaryScramblingCode::new(0).unwrap(),
            CommonChannelAmplitudes {
                cpich: 0.15,
                pccpch: 0.2,
                primary_sch: 0.1,
                secondary_sch: 0.1,
            },
            SamplesPerChip::Two,
            32,
            ChipInstant {
                epoch,
                index: (SYSTEM_FRAME_MODULUS - 2) * CHIPS_PER_FRAME,
            },
        )
        .unwrap()
    }

    #[test]
    fn scheduled_bch_messages_feed_continuous_samples_across_sfn_wrap() {
        let mut source = BroadcastSource::new(schedule(), waveform(epoch(1))).unwrap();
        let mut output = vec![Complex32::default(); source.samples_per_tti()];
        let first = source.generate(&mut output).unwrap();
        let next = source.generate(&mut output).unwrap();
        assert_eq!(first.next_sample, next.first_sample);
        assert_eq!(output.len(), 2 * CHIPS_PER_FRAME as usize * 2);
        assert!(
            output
                .iter()
                .all(|sample| sample.re.is_finite() && sample.im.is_finite())
        );
        assert!(output.iter().any(|sample| sample.norm_sqr() > 0.01));
    }

    #[test]
    fn incompatible_schedule_is_rejected_before_waveform_generation() {
        assert!(matches!(
            BroadcastSource::new(schedule(), waveform(epoch(2))),
            Err(BroadcastError::Schedule(ScheduleError::WrongEpoch))
        ));
        let mut source = BroadcastSource::new(schedule(), waveform(epoch(1))).unwrap();
        assert_eq!(
            source.generate(&mut []),
            Err(BroadcastError::Waveform(DspError::InvalidLength))
        );
    }
}
