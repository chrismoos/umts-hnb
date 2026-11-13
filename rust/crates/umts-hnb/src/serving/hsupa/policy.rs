// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    controller::HsupaCandidate,
    dedicated_rx::HsupaReceiveSource,
    hsupa_session::{
        MAX_DEDICATED_FLOWS, ReceiveInputs, build_coexistence_activation,
        build_edch_only_activation, receive_preparation::Request,
    },
};
use anyhow::{Context, Result, anyhow, ensure};
use std::num::{NonZeroU8, NonZeroU64};
use umts_config::Config;
use umts_rrc::{
    dedicated::ScheduledActivation,
    hsupa::{
        ActivationParameters, ControlConfiguration, ReferencePowers, ReportingPeriod,
        ScheduledFlow, SchedulingInformation, TransportConfiguration,
    },
};
use umts_types::{
    CnDomain,
    hsupa::{CodeLayout, Etfci, PuncturingLimit, Rel6Category, TransportBlockTable, Tti},
};

const HAPPY_DELAY_MS: u16 = 20;
const PUNCTURING_PERCENT: u8 = 44;
const HARQ_POWER_OFFSET_DB: u8 = 0;
const MAX_RETRANSMISSIONS: u8 = 3;
const MAX_PACKET_FLOWS: usize = 8;
const MAX_COEXISTING_CODES: u8 = 2;
const FOUR_CODE_CHANNELS: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UplinkMode {
    Coexistence,
    EdchOnly,
}

pub(super) struct Policy {
    four_code: bool,
    rv_zero_only: bool,
    maximum: Rel6Category,
    references: ReferencePowers,
    control: ControlConfiguration,
    receive: crate::hsupa_session::ReceiveSettings,
    pub(super) grant_value: u8,
    pub(super) grant_ceiling: u8,
    pub(super) grant_amplitude: f32,
    pub(super) grant_refresh: Option<NonZeroU64>,
}

impl Policy {
    pub(super) fn new(
        config: &Config,
        power: Option<&super::super::downlink_power::Plan>,
    ) -> Result<Self> {
        ensure!(
            config.hspa.uplink.scheduled,
            "non-scheduled HSUPA serving construction is not integrated"
        );
        let settings = config
            .hspa
            .uplink
            .resources
            .as_ref()
            .context("HSUPA budgets missing")?;
        let grant_value = config
            .hspa
            .uplink
            .initial_serving_grant
            .unwrap_or(config.hspa.uplink.serving_grant);
        ensure!(
            grant_value <= config.hspa.uplink.serving_grant,
            "initial HSUPA grant exceeds its configured ceiling"
        );
        let mut receive = super::receive_settings(&settings.receive)?;
        receive.feedback_amplitude = power
            .and_then(|power| power.e_hich_amplitude)
            .unwrap_or(receive.feedback_amplitude);
        Ok(Self {
            four_code: config.hspa.uplink.four_code_uplink,
            rv_zero_only: config.hspa.uplink.rv_zero_only,
            maximum: Rel6Category::new(config.hspa.uplink.max_category)
                .map_err(|error| anyhow!("HSUPA category limit: {error:?}"))?,
            references: super::reference_powers(config)?,
            control: ControlConfiguration::new(
                config.hspa.uplink.edpcch_power_offset,
                HAPPY_DELAY_MS,
            )
            .map_err(|error| anyhow!("HSUPA control parameters: {error:?}"))?,
            receive,
            grant_value,
            grant_ceiling: config.hspa.uplink.serving_grant,
            grant_amplitude: power
                .and_then(|power| power.e_agch_amplitude)
                .unwrap_or(settings.grant_amplitude),
            grant_refresh: settings.primary_grant_refresh_ms.map(|ms| {
                NonZeroU64::new(u64::from(ms) * (umts_types::time::CHIPS_PER_SECOND / 1000))
                    .expect("validated nonzero grant refresh interval")
            }),
        })
    }

    fn mode(&self, category: Rel6Category, cs: bool) -> UplinkMode {
        if self.four_code
            && !cs
            && self.maximum.limits().maximum_codes == FOUR_CODE_CHANNELS
            && category.limits().maximum_codes == FOUR_CODE_CHANNELS
        {
            UplinkMode::EdchOnly
        } else {
            UplinkMode::Coexistence
        }
    }

    fn transport(
        &self,
        category: Rel6Category,
        mode: UplinkMode,
    ) -> Result<TransportConfiguration> {
        let configured = self.maximum.limits();
        let reported = category.limits();
        let interval =
            if configured.maximum_two_ms_bits.is_some() && reported.maximum_two_ms_bits.is_some() {
                Tti::TwoMilliseconds
            } else {
                Tti::TenMilliseconds
            };
        let codes = configured
            .maximum_codes
            .min(reported.maximum_codes)
            .min(match mode {
                UplinkMode::Coexistence => MAX_COEXISTING_CODES,
                UplinkMode::EdchOnly => FOUR_CODE_CHANNELS,
            });
        let sf = configured
            .minimum_spreading_factor
            .max(reported.minimum_spreading_factor);
        let layout = CodeLayout::new(match (codes, sf) {
            (1, 4) => &[4],
            (2, 4) => &[4, 4],
            (2, 2) => &[2, 2],
            (4, 2) => &[2, 2, 4, 4],
            _ => anyhow::bail!("unsupported HSUPA capability intersection"),
        })
        .map_err(|error| anyhow!("HSUPA code layout: {error:?}"))?;
        TransportConfiguration::new(
            interval,
            category,
            TransportBlockTable::Zero,
            layout,
            PuncturingLimit::new(PUNCTURING_PERCENT).expect("legal puncturing limit"),
        )
        .map_err(|error| anyhow!("HSUPA transport configuration: {error:?}"))
    }

    pub(super) fn request(
        &self,
        candidate: &HsupaCandidate,
        source: HsupaReceiveSource,
    ) -> Result<Request> {
        ensure!(
            candidate
                .downlink
                .as_ref()
                .is_none_or(|downlink| downlink.identity() == candidate.identity),
            "HSUPA downlink snapshot belongs to a different committed configuration"
        );
        let dpch = umts_dsp::dedicated::DpchTiming::new(
            candidate.dedicated.physical().frame_offset_256_chips,
        )
        .map_err(|error| anyhow!("HSUPA dedicated timing: {error:?}"))?;
        let common_frame =
            crate::serving::initial_timing::associated_common_frame(source.anchor.time, dpch)
                .map_err(|error| anyhow!("HSUPA associated common frame: {error:?}"))?;
        let packet_count = candidate
            .dedicated
            .bearers()
            .iter()
            .filter(|bearer| bearer.domain == Some(CnDomain::Ps))
            .count();
        ensure!(
            (1..=MAX_PACKET_FLOWS).contains(&packet_count),
            "HSUPA requires one through eight packet flows"
        );
        let flows = (0..packet_count.min(MAX_DEDICATED_FLOWS))
            .map(|index| {
                ScheduledFlow::new(index as u8, HARQ_POWER_OFFSET_DB, MAX_RETRANSMISSIONS)
                    .map_err(|error| anyhow!("HSUPA scheduled flow: {error:?}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let period = [candidate.dedicated.uplink(), candidate.dedicated.downlink()]
            .into_iter()
            .flat_map(|tfcs| tfcs.transports())
            .map(|transport| transport.frames())
            .max()
            .context("HSUPA retained transport set is empty")?;
        let period = u8::try_from(period)?;
        ensure!(period != 0, "HSUPA retained transport period is zero");
        let ahead = NonZeroU8::new(period - source.anchor.cfn % period)
            .context("HSUPA provisional activation period")?;
        // This timestamp is private to catalog preparation and is rescheduled before publication.
        let activation = ScheduledActivation::new(source.anchor.time, source.anchor.cfn, ahead)
            .map_err(|error| anyhow!("HSUPA provisional activation: {error:?}"))?;
        let mode = self.mode(
            candidate.category,
            candidate
                .dedicated
                .bearers()
                .iter()
                .any(|bearer| bearer.domain == Some(CnDomain::Cs)),
        );
        let parameters = ActivationParameters {
            e_rnti: candidate.lease.e_rnti(),
            transport: self.transport(candidate.category, mode)?,
            control: self.control,
            downlink: candidate.lease.controls(),
            references: self.references.clone(),
            minimum: Etfci::new(0).expect("minimum E-TFCI"),
            scheduling: SchedulingInformation {
                without_grant: Some(ReportingPeriod::EveryTti),
                with_grant: None,
                power_offset: HARQ_POWER_OFFSET_DB,
            },
            rv_zero_only: self.rv_zero_only,
            flows,
        };
        let construct = match mode {
            UplinkMode::Coexistence => build_coexistence_activation,
            UplinkMode::EdchOnly => build_edch_only_activation,
        };
        let configuration = construct(
            candidate.dedicated.clone(),
            parameters,
            candidate.downlink.as_ref(),
            activation,
        )?;
        Ok(Request {
            configuration,
            inputs: ReceiveInputs {
                identity: candidate.identity,
                code: source.code,
                uplink: source.anchor,
                common_frame,
                control_search: source.control_search,
            },
            settings: self.receive,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_code_selection_is_per_ue_and_retains_voice_coexistence() {
        let mut config = super::super::tests::config();
        config.hspa.uplink.four_code_uplink = true;
        config.hspa.uplink.max_category = 6;
        let policy = Policy::new(&config, None).unwrap();
        for value in 1..=6 {
            let category = Rel6Category::new(value).unwrap();
            for cs in [false, true] {
                let mode = policy.mode(category, cs);
                assert_eq!(mode == UplinkMode::EdchOnly, value == 6 && !cs);
                let transport = policy.transport(category, mode).unwrap();
                if mode == UplinkMode::EdchOnly {
                    assert_eq!(
                        transport.maximum_layout().spreading_factors(),
                        &[2, 2, 4, 4]
                    );
                } else {
                    assert!(
                        transport.maximum_layout().spreading_factors().len()
                            <= usize::from(MAX_COEXISTING_CODES)
                    );
                }
            }
        }
        config.hspa.uplink.four_code_uplink = false;
        assert_eq!(
            Policy::new(&config, None)
                .unwrap()
                .mode(Rel6Category::new(6).unwrap(), false),
            UplinkMode::Coexistence
        );
    }

    #[test]
    fn transport_intersects_reported_and_configured_capabilities() {
        for maximum in 1..=6 {
            let mut config = super::super::tests::config();
            config.hspa.uplink.max_category = maximum;
            let policy = Policy::new(&config, None).unwrap();
            for reported in 1..=6 {
                let category = Rel6Category::new(reported).unwrap();
                let transport = policy.transport(category, UplinkMode::Coexistence).unwrap();
                assert_eq!(transport.category(), category);
                let limits = category.limits();
                let configured = policy.maximum.limits();
                let factors = transport.maximum_layout().spreading_factors();
                assert!(
                    factors.len()
                        <= usize::from(limits.maximum_codes.min(configured.maximum_codes))
                );
                assert!(factors.len() <= usize::from(MAX_COEXISTING_CODES));
                assert!(factors.iter().all(|factor| {
                    *factor
                        >= limits
                            .minimum_spreading_factor
                            .max(configured.minimum_spreading_factor)
                }));
                assert_eq!(
                    transport.interval() == Tti::TwoMilliseconds,
                    limits.maximum_two_ms_bits.is_some()
                        && configured.maximum_two_ms_bits.is_some()
                );
            }
        }
    }
}
