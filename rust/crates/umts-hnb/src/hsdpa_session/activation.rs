// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::radio_resources::HsdpaCodeSet;
use anyhow::{Context, Result, anyhow, ensure};
use std::{
    num::{NonZeroU8, NonZeroUsize},
    sync::Arc,
};
use umts_config::{HspaDownlinkSettings, PacketMode};
use umts_rrc::{
    dedicated::{Rel99Configuration, Rel99Rlc, ScheduledActivation},
    hsdpa::{
        AckFeedback, ActivationParameters, ControlChannels, FeedbackCycle, HarqConfiguration,
        InitialActivation, InitialConfiguration, MappedBearer, MeasurementFeedback,
        QueueConfiguration, QueueSet,
    },
};
use umts_types::{CnDomain, channel_codes::OvsfCode, hsdpa::Category};

const MAX_QUEUES: usize = 8;
const MAX_PROCESSES: u8 = 8;
const PDU_SIZE_INDICES: usize = 8;
const MONITORED_CONTROL_CODES: usize = 4;
const REORDERING_TIMER_MS: u16 = 100;
const REORDERING_WINDOW: u8 = 32;

pub struct RetainedDpchPolicy {
    packet_modes: Option<Box<[PacketMode]>>,
    qam64_enabled: bool,
    dual_cell_enabled: bool,
    processes: NonZeroU8,
    memory: Box<[NonZeroUsize]>,
    measurement: MeasurementFeedback,
    acknowledgement: AckFeedback,
}

impl RetainedDpchPolicy {
    pub fn new(config: &HspaDownlinkSettings, measurement_power_offset_db: f64) -> Result<Self> {
        let processes =
            NonZeroU8::new(config.harq_processes).context("HSDPA needs HARQ processes")?;
        ensure!(
            processes.get() <= MAX_PROCESSES,
            "HSDPA supports at most eight HARQ processes"
        );
        let offset = measurement_power_offset_db * 2.0;
        ensure!(
            offset.is_finite() && offset.fract() == 0.0 && (-12.0..=26.0).contains(&offset),
            "HSDPA measurement power offset must be -6..13 dB in half-dB steps"
        );
        let cycle = match config.feedback.cqi_interval_ms {
            2 => FeedbackCycle::Ms2,
            4 => FeedbackCycle::Ms4,
            8 => FeedbackCycle::Ms8,
            10 => FeedbackCycle::Ms10,
            20 => FeedbackCycle::Ms20,
            40 => FeedbackCycle::Ms40,
            80 => FeedbackCycle::Ms80,
            160 => FeedbackCycle::Ms160,
            value => return Err(anyhow!("unsupported CQI feedback cycle {value} ms")),
        };
        let measurement = MeasurementFeedback::new(
            offset as i8,
            cycle,
            NonZeroU8::MIN,
            config.feedback.cqi_power_index,
        )
        .map_err(|error| anyhow!("HSDPA measurement feedback: {error:?}"))?;
        let acknowledgement = AckFeedback::new(
            config.feedback.ack_nack_power_index,
            config.feedback.ack_nack_power_index,
            NonZeroU8::MIN,
        )
        .map_err(|error| anyhow!("HSDPA acknowledgement feedback: {error:?}"))?;
        let memory = config
            .harq_memory_bits
            .iter()
            .map(|&bits| {
                usize::try_from(bits)
                    .ok()
                    .and_then(NonZeroUsize::new)
                    .context("HSDPA HARQ memory partition must be nonzero")
            })
            .collect::<Result<Box<[_]>>>()?;
        ensure!(
            memory.is_empty() || memory.len() == usize::from(processes.get()),
            "explicit HSDPA HARQ memory must contain one entry per configured process"
        );
        Ok(Self {
            packet_modes: None,
            qam64_enabled: config.qam64_enabled,
            dual_cell_enabled: config.dual_cell,
            processes,
            memory,
            measurement,
            acknowledgement,
        })
    }

    pub fn with_packet_modes(mut self, modes: Option<&[PacketMode]>) -> Self {
        self.packet_modes = modes.map(|modes| modes.to_vec().into_boxed_slice());
        self
    }

    pub fn allows_packet_mode(&self, mode: PacketMode) -> bool {
        self.packet_modes
            .as_ref()
            .is_none_or(|modes| modes.contains(&mode))
    }

    pub fn validate_rel99_packet_fallback(&self, dedicated: &Rel99Configuration) -> Result<()> {
        ensure!(
            self.allows_packet_mode(PacketMode::Rel99)
                || !dedicated
                    .bearers()
                    .iter()
                    .any(|bearer| bearer.domain == Some(CnDomain::Ps)),
            "cell.packet_modes does not permit Rel99 packet fallback"
        );
        Ok(())
    }

    pub fn selected_category(
        &self,
        report: &umts_rrc::capabilities::CapabilityReport,
    ) -> Option<Category> {
        self.select_single_category(report.hsdpa(), report.hsdpa_qam64())
    }

    fn select_single_category(
        &self,
        legacy: umts_rrc::capabilities::hsdpa::FddHsdpa,
        qam64: Option<Category>,
    ) -> Option<Category> {
        let umts_rrc::capabilities::hsdpa::FddHsdpa::Supported(category) = legacy else {
            return None;
        };
        if self.qam64_enabled
            && self.allows_packet_mode(PacketMode::HspaPlus)
            && let Some(category) = qam64
        {
            return Some(category);
        }
        self.allows_packet_mode(PacketMode::Hspa)
            .then_some(category)
    }

    pub fn selected_dual_cell(
        &self,
        report: &umts_rrc::capabilities::CapabilityReport,
    ) -> Option<umts_types::hsdpa::DualCellCategory> {
        self.select_dual_category(report.hsdpa_dual_cell().map(|report| report.category))
    }

    fn select_dual_category(
        &self,
        reported: Option<umts_types::hsdpa::DualCellCategory>,
    ) -> Option<umts_types::hsdpa::DualCellCategory> {
        let category = reported?;
        (self.dual_cell_enabled && self.allows_packet_mode(PacketMode::DcHspa))
            .then(|| {
                category
                    .with_qam64(self.qam64_enabled && category.per_cell_limits().qam64)
                    .ok()
            })
            .flatten()
    }

    pub fn construct_configuration_dual(
        &self,
        dedicated: Arc<Rel99Configuration>,
        category: umts_types::hsdpa::DualCellCategory,
        codes: HsdpaCodeSet,
        h_rnti: u16,
        secondary: crate::controller::SecondaryHsdpaAdmission,
    ) -> Result<Arc<InitialConfiguration>> {
        ensure!(self.dual_cell_enabled, "dual-cell HSDPA is disabled");
        let offset = secondary.measurement_power_offset_db * 2.0;
        ensure!(
            offset.is_finite() && offset.fract() == 0.0 && (-12.0..=26.0).contains(&offset),
            "secondary measurement power offset must be -6..13 dB in half-dB steps"
        );
        let control = ControlChannels::new(
            &secondary
                .codes
                .control_codes()
                .take(MONITORED_CONTROL_CODES)
                .collect::<Vec<_>>(),
        )
        .map_err(|error| anyhow!("secondary HSDPA control allocation: {error:?}"))?;
        let secondary = umts_rrc::hsdpa::SecondaryCellConfiguration::new(
            h_rnti,
            secondary.uarfcn_dl,
            secondary.primary.index(),
            control,
            offset as i8,
            category.per_cell_limits().qam64,
        )
        .map_err(|error| anyhow!("secondary HSDPA RRC parameters: {error:?}"))?;
        let configuration = self.construct_configuration_owned(
            dedicated,
            category.per_cell_category(),
            codes,
            h_rnti,
            true,
        )?;
        configuration
            .with_secondary_cell(secondary)
            .map(Arc::new)
            .map_err(|error| anyhow!("dual-cell HSDPA configuration: {error:?}"))
    }

    pub fn construct(
        &self,
        dedicated: Arc<Rel99Configuration>,
        category: Category,
        codes: HsdpaCodeSet,
        h_rnti: u16,
        activation: ScheduledActivation,
    ) -> Result<Arc<InitialActivation>> {
        InitialActivation::schedule(
            self.construct_configuration(dedicated, category, codes, h_rnti)?,
            activation,
        )
        .map(Arc::new)
        .map_err(|error| anyhow!("retained-DPCH HSDPA activation: {error:?}"))
    }

    pub fn construct_configuration(
        &self,
        dedicated: Arc<Rel99Configuration>,
        category: Category,
        codes: HsdpaCodeSet,
        h_rnti: u16,
    ) -> Result<Arc<InitialConfiguration>> {
        self.construct_configuration_owned(
            dedicated,
            category,
            codes,
            h_rnti,
            category.limits().qam64,
        )
        .map(Arc::new)
    }

    fn construct_configuration_owned(
        &self,
        dedicated: Arc<Rel99Configuration>,
        category: Category,
        codes: HsdpaCodeSet,
        h_rnti: u16,
        mac_ehs: bool,
    ) -> Result<InitialConfiguration> {
        // The HS-DSCH scheduler skips a lent data code for every UE.
        let lent = crate::radio_resources::lent_data_codes();
        for &index in &dedicated.physical().downlink_codes {
            let retained = OvsfCode::new(dedicated.physical().downlink_sf, index)
                .map_err(|error| anyhow!("retained DPCH code: {error:?}"))?;
            ensure!(
                dedicated.physical().secondary_scrambling.is_some()
                    || !codes
                        .data_codes()
                        .any(|code| { code.overlaps(retained) && lent & (1 << code.index()) == 0 }),
                "HSDPA data codes overlap the retained DPCH"
            );
        }
        let harq = if self.memory.is_empty() {
            HarqConfiguration::implicit(category, self.processes)
        } else {
            HarqConfiguration::explicit(category, &self.memory)
        }
        .map_err(|error| anyhow!("HSDPA HARQ capability/memory: {error:?}"))?;
        let control = ControlChannels::new(
            &codes
                .control_codes()
                .take(MONITORED_CONTROL_CODES)
                .collect::<Vec<_>>(),
        )
        .map_err(|error| anyhow!("HSDPA control allocation: {error:?}"))?;
        let mut mappings = Vec::new();
        let mut queues = Vec::new();
        for bearer in dedicated
            .bearers()
            .iter()
            .filter(|bearer| bearer.domain == Some(CnDomain::Ps))
        {
            ensure!(
                !matches!(bearer.rlc, Rel99Rlc::Transparent { .. }),
                "HSDPA bearer {} cannot use transparent RLC",
                bearer.id
            );
            ensure!(
                queues.len() < MAX_QUEUES,
                "HSDPA activation exceeds the MAC-hs queue capacity"
            );
            let flow = u8::try_from(queues.len()).context("HSDPA flow index overflow")?;
            let mut sizes = [None; PDU_SIZE_INDICES];
            for mapping in &bearer.mapping.downlink {
                let transport = dedicated
                    .downlink()
                    .transport(mapping.transport)
                    .context("HSDPA bearer refers to an absent downlink transport")?;
                for format in transport.formats() {
                    let Some(bits) = NonZeroUsize::new(usize::from(format.rlc_bits)) else {
                        continue;
                    };
                    if sizes.contains(&Some(bits)) {
                        continue;
                    }
                    let slot = sizes
                        .iter_mut()
                        .find(|size| size.is_none())
                        .context("HSDPA bearer exceeds eight distinct MAC-d PDU sizes")?;
                    *slot = Some(bits);
                }
            }
            queues.push(
                QueueConfiguration::new(flow, flow, sizes, REORDERING_TIMER_MS, REORDERING_WINDOW)
                    .map_err(|error| anyhow!("HSDPA bearer {} queue: {error:?}", bearer.id))?,
            );
            mappings.push(MappedBearer {
                bearer: bearer.id,
                flow,
                logical: mac_ehs.then_some(flow + 1),
            });
        }
        let queues = QueueSet::new(queues)
            .map_err(|error| anyhow!("HSDPA packet bearer queues: {error:?}"))?;
        let construct = if mac_ehs {
            InitialConfiguration::new_mac_ehs
        } else {
            InitialConfiguration::new
        };
        construct(
            dedicated,
            ActivationParameters {
                h_rnti,
                harq,
                queues,
                control,
                measurement: self.measurement,
                acknowledgement: self.acknowledgement,
            },
            mappings,
        )
        .map_err(|error| anyhow!("retained-DPCH HSDPA construction: {error:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        radio_resources::RadioResources,
        test_support::{at, count, rel99, source},
    };
    #[cfg(not(test))]
    use umts_config::Config;

    fn config() -> umts_config::Config {
        serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-support/fixtures/config/rel99.yaml"
        )))
        .unwrap()
    }

    #[test]
    fn packet_mode_allow_list_preserves_legacy_and_restricts_each_fallback() {
        use umts_rrc::capabilities::hsdpa::FddHsdpa;
        use umts_types::hsdpa::DualCellCategory;
        let mut config = config();
        config.hspa.downlink.qam64_enabled = true;
        config.hspa.downlink.dual_cell = true;
        let legacy = Category::new(10).unwrap();
        let plus = Category::new(14).unwrap();
        let dual = DualCellCategory::new(24).unwrap();
        for (modes, expected_single, expected_dual, rel99) in [
            (None, Some(plus), Some(dual), true),
            (Some(vec![PacketMode::Rel99]), None, None, true),
            (Some(vec![PacketMode::Hspa]), Some(legacy), None, false),
            (Some(vec![PacketMode::HspaPlus]), Some(plus), None, false),
            (Some(vec![PacketMode::DcHspa]), None, Some(dual), false),
            (
                Some(vec![
                    PacketMode::Rel99,
                    PacketMode::Hspa,
                    PacketMode::HspaPlus,
                ]),
                Some(plus),
                None,
                true,
            ),
            (
                Some(vec![PacketMode::Hspa, PacketMode::DcHspa]),
                Some(legacy),
                Some(dual),
                false,
            ),
        ] {
            let policy = RetainedDpchPolicy::new(&config.hspa.downlink, 6.0)
                .unwrap()
                .with_packet_modes(modes.as_deref());
            assert_eq!(
                policy.select_single_category(FddHsdpa::Supported(legacy), Some(plus)),
                expected_single
            );
            assert_eq!(policy.select_dual_category(Some(dual)), expected_dual);
            assert_eq!(policy.allows_packet_mode(PacketMode::Rel99), rel99);
            assert!(
                policy
                    .select_single_category(FddHsdpa::Unsupported, Some(plus))
                    .is_none()
            );
            assert!(
                policy
                    .select_single_category(FddHsdpa::NotReported, None)
                    .is_none()
            );
            assert!(policy.select_dual_category(None).is_none());
        }
        let plus_only = RetainedDpchPolicy::new(&config.hspa.downlink, 6.0)
            .unwrap()
            .with_packet_modes(Some(&[PacketMode::HspaPlus]));
        assert!(
            plus_only
                .select_single_category(FddHsdpa::Supported(legacy), None)
                .is_none()
        );
        let dc_only = RetainedDpchPolicy::new(&config.hspa.downlink, 6.0)
            .unwrap()
            .with_packet_modes(Some(&[PacketMode::DcHspa]));
        assert!(
            dc_only
                .select_single_category(FddHsdpa::Supported(legacy), Some(plus))
                .is_none()
        );
        assert_eq!(
            dc_only.select_dual_category(Some(DualCellCategory::new(21).unwrap())),
            Some(DualCellCategory::new(21).unwrap())
        );
    }

    #[test]
    fn rel99_packet_fallback_does_not_restrict_voice_or_signalling() {
        let config = config();
        for modes in [vec![PacketMode::HspaPlus], vec![PacketMode::DcHspa]] {
            let policy = RetainedDpchPolicy::new(&config.hspa.downlink, 6.0)
                .unwrap()
                .with_packet_modes(Some(&modes));
            assert!(
                policy
                    .validate_rel99_packet_fallback(&rel99::configuration(false, false, 10))
                    .is_ok()
            );
            assert!(
                policy
                    .validate_rel99_packet_fallback(&rel99::configuration(true, false, 10))
                    .is_ok()
            );
            assert!(
                policy
                    .validate_rel99_packet_fallback(&rel99::configuration(false, true, 10))
                    .is_err()
            );
            assert!(
                policy
                    .validate_rel99_packet_fallback(&rel99::configuration(true, true, 10))
                    .is_err()
            );
        }
        let policy = RetainedDpchPolicy::new(&config.hspa.downlink, 6.0)
            .unwrap()
            .with_packet_modes(Some(&[PacketMode::Rel99]));
        assert!(
            policy
                .validate_rel99_packet_fallback(&rel99::configuration(false, true, 10))
                .is_ok()
        );
    }

    #[test]
    fn packet_and_combined_activation_preserve_exact_rlc_sizes_and_reported_category() {
        let config = config();
        let policy = RetainedDpchPolicy::new(
            &config.hspa.downlink,
            config
                .hspa
                .downlink
                .measurement_power_offset_db
                .unwrap_or(6.0),
        )
        .unwrap();
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::new(5).unwrap(), NonZeroU8::MIN)
            .unwrap();
        for voice in [false, true] {
            let (ul, dl, bearers, physical) = rel99::components(voice, true, 10);
            let dedicated = Arc::new(Rel99Configuration::new(ul, dl, bearers, physical).unwrap());
            let category = Category::new(6).unwrap();
            let activation =
                ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap();
            let built = policy
                .construct(dedicated.clone(), category, *lease.codes(), 1, activation)
                .unwrap();
            assert!(Arc::ptr_eq(built.dedicated(), &dedicated));
            assert_eq!(built.parameters().harq.category(), category);
            assert_eq!(
                built.parameters().harq.process_memory().len(),
                usize::from(config.hspa.downlink.harq_processes)
            );
            for mapped in built.mappings() {
                let bearer = dedicated.bearer(mapped.bearer).unwrap();
                assert_eq!(bearer.domain, Some(CnDomain::Ps));
                assert!(mapped.logical.is_none());
                let queue = built
                    .parameters()
                    .queues
                    .queues()
                    .iter()
                    .find(|queue| queue.flow() == mapped.flow)
                    .unwrap();
                for mapping in &bearer.mapping.downlink {
                    for format in dedicated
                        .downlink()
                        .transport(mapping.transport)
                        .unwrap()
                        .formats()
                    {
                        if let Some(bits) = NonZeroUsize::new(usize::from(format.rlc_bits)) {
                            assert!(queue.sizes().contains(&Some(bits)));
                        }
                    }
                }
            }
            built
                .message(umts_rrc::establishment::TransactionId::new(0).unwrap())
                .unwrap();
        }
        resources.release_hsdpa(lease).unwrap();
    }

    #[test]
    fn retained_dpch_may_use_only_a_lent_data_code() {
        let config = config();
        let policy = RetainedDpchPolicy::new(&config.hspa.downlink, 6.0).unwrap();
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::new(5).unwrap(), NonZeroU8::MIN)
            .unwrap();
        let data = lease.codes().data_codes().next().unwrap();
        let (ul, dl, bearers, mut physical) = rel99::components(false, true, 10);
        let per_data_code = physical.downlink_sf / 16;
        physical.downlink_codes = vec![data.index() * per_data_code];
        physical.secondary_scrambling = None;
        let dedicated = Arc::new(Rel99Configuration::new(ul, dl, bearers, physical).unwrap());
        let category = Category::new(6).unwrap();
        let activation = ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap();
        crate::radio_resources::set_lent_data_codes(0);
        assert!(
            policy
                .construct(dedicated.clone(), category, *lease.codes(), 1, activation)
                .is_err()
        );
        crate::radio_resources::set_lent_data_codes(1 << data.index());
        assert!(
            policy
                .construct(dedicated, category, *lease.codes(), 1, activation)
                .is_ok()
        );
        crate::radio_resources::set_lent_data_codes(0);
        resources.release_hsdpa(lease).unwrap();
    }

    #[test]
    fn policy_rejects_fractional_power_steps_and_inconsistent_memory() {
        let mut config = config();
        config.hspa.downlink.measurement_power_offset_db = Some(6.25);
        assert!(
            RetainedDpchPolicy::new(
                &config.hspa.downlink,
                config
                    .hspa
                    .downlink
                    .measurement_power_offset_db
                    .unwrap_or(6.0)
            )
            .is_err()
        );
        config.hspa.downlink.measurement_power_offset_db = Some(6.5);
        assert!(
            RetainedDpchPolicy::new(
                &config.hspa.downlink,
                config
                    .hspa
                    .downlink
                    .measurement_power_offset_db
                    .unwrap_or(6.0)
            )
            .is_ok()
        );
        config.hspa.downlink.harq_memory_bits = vec![800];
        assert!(
            RetainedDpchPolicy::new(
                &config.hspa.downlink,
                config
                    .hspa
                    .downlink
                    .measurement_power_offset_db
                    .unwrap_or(6.0)
            )
            .is_err()
        );
        config.hspa.downlink.harq_memory_bits =
            vec![0; usize::from(config.hspa.downlink.harq_processes)];
        assert!(
            RetainedDpchPolicy::new(
                &config.hspa.downlink,
                config
                    .hspa
                    .downlink
                    .measurement_power_offset_db
                    .unwrap_or(6.0)
            )
            .is_err()
        );
    }

    #[test]
    fn cqi_overrides_preserve_independent_acknowledgement_power() {
        let mut config = config();
        let original = RetainedDpchPolicy::new(
            &config.hspa.downlink,
            config
                .hspa
                .downlink
                .measurement_power_offset_db
                .unwrap_or(6.0),
        )
        .unwrap();
        assert_eq!(original.measurement.cqi_power_index(), 8);
        assert_eq!(original.measurement.cycle(), FeedbackCycle::Ms2);
        config.hspa.downlink.feedback.cqi_power_index = 5;
        config.hspa.downlink.feedback.cqi_interval_ms = 10;
        let updated = RetainedDpchPolicy::new(
            &config.hspa.downlink,
            config
                .hspa
                .downlink
                .measurement_power_offset_db
                .unwrap_or(6.0),
        )
        .unwrap();
        assert_eq!(updated.measurement.cqi_power_index(), 5);
        assert_eq!(updated.measurement.cycle(), FeedbackCycle::Ms10);
        assert_eq!(updated.measurement.repetition(), NonZeroU8::MIN);
        assert_eq!(
            updated.acknowledgement.ack_power_index(),
            original.acknowledgement.ack_power_index()
        );
        assert_eq!(
            updated.acknowledgement.nack_power_index(),
            original.acknowledgement.nack_power_index()
        );
        assert_eq!(
            updated.measurement.wire(),
            MeasurementFeedback::new(
                original.measurement.power_offset_half_db(),
                FeedbackCycle::Ms10,
                NonZeroU8::MIN,
                5,
            )
            .unwrap()
            .wire()
        );
        config.hspa.downlink.feedback.cqi_interval_ms = 3;
        assert!(
            RetainedDpchPolicy::new(
                &config.hspa.downlink,
                config
                    .hspa
                    .downlink
                    .measurement_power_offset_db
                    .unwrap_or(6.0)
            )
            .is_err()
        );
        config.hspa.downlink.feedback.cqi_interval_ms = 2;
        config.hspa.downlink.feedback.cqi_power_index = 9;
        assert!(
            RetainedDpchPolicy::new(
                &config.hspa.downlink,
                config
                    .hspa
                    .downlink
                    .measurement_power_offset_db
                    .unwrap_or(6.0)
            )
            .is_err()
        );
    }
}
