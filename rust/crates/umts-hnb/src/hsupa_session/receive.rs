// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_rx::{FrameAnchor, HsupaAttachment, ReceiveError, ReceiveIdentity},
    hsupa_rx::{
        ControlAdmission, FeedbackAssociation, Ingress, IngressInputs, IngressSettings,
        PacketDelivery, TrackedFormat, TrackedReceiver, TrackedSettings,
    },
};
use anyhow::{Context, Result, anyhow, ensure};
use std::{
    num::{NonZeroU8, NonZeroU64, NonZeroUsize},
    sync::Arc,
};
use umts_dsp::{
    DspError,
    dedicated::{DpchTiming, LikelihoodSettings, UplinkDpchCode},
    hsupa::{SingleReferencePower, TransportPlan, control_gain_ratio},
};
use umts_mac::hsupa::payload::Mapping;
use umts_rrc::hsupa::UplinkActivation;
use umts_types::time::ChipInstant;

const MAX_FORMATS: usize = 128;

#[cfg(test)]
mod tests;

pub struct ReceiveInputs {
    pub identity: ReceiveIdentity,
    pub code: Arc<UplinkDpchCode>,
    pub uplink: FrameAnchor,
    pub common_frame: ChipInstant,
    pub control_search: crate::dedicated_rx::ControlSearchGeometry,
}

#[derive(Clone, Copy)]
pub struct ReceiveSettings {
    pub likelihood: LikelihoodSettings,
    pub control_admission: ControlAdmission,
    pub iterations: NonZeroU8,
    pub maximum_format_bits: NonZeroUsize,
    pub maximum_catalog_bytes: NonZeroUsize,
    pub retained_sample_bytes: NonZeroUsize,
    pub soft_buffer_bytes: NonZeroUsize,
    pub scratch_bytes: NonZeroUsize,
    pub maximum_receive_bytes: NonZeroUsize,
    pub report_capacity: NonZeroUsize,
    pub report_bytes: NonZeroUsize,
    pub packet_capacity: NonZeroUsize,
    pub packet_bytes: NonZeroUsize,
    pub reorder_hold_ttis: NonZeroU64,
    pub feedback_capacity: NonZeroUsize,
    pub feedback_amplitude: f32,
}

pub struct ReceiveResources {
    configuration: Arc<UplinkActivation>,
    receiver: HsupaAttachment,
    ingress: Ingress,
    maximum_receive_bytes: NonZeroUsize,
    format_count: usize,
    catalog_bytes: usize,
}

impl ReceiveSettings {
    fn reorder_hold_chips(&self, interval: umts_types::hsupa::Tti) -> anyhow::Result<NonZeroU64> {
        self.reorder_hold_ttis
            .get()
            .checked_mul(interval.chips())
            .and_then(NonZeroU64::new)
            .context("HSUPA reorder hold exceeds chip timeline")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionAssemblyError {
    Configuration,
    Grant(crate::hsupa_tx::Error),
    Preparation,
}

pub struct RejectedReceiveSession {
    pub error: SessionAssemblyError,
    pub resources: ReceiveResources,
    pub lease: crate::radio_resources::HsupaUserLease,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Owns the rejected operation until it is retried or dropped."
        )
    )]
    pub procedure: Box<umts_rrc::hsupa::ActivationProcedure>,
}

impl ReceiveResources {
    pub fn reschedule(&mut self, configuration: Arc<UplinkActivation>) -> Result<()> {
        ensure!(
            self.configuration.same_definition(&configuration),
            "HSUPA prepared receiver cannot adopt another configuration definition"
        );
        ensure!(
            !self.receiver.receiver().is_started(),
            "HSUPA receiver has already started"
        );
        self.ingress
            .reschedule_before_service(
                super::timing::receive_activation(
                    configuration.activation(),
                    self.receiver.receiver().anchor(),
                )
                .map_err(|error| anyhow!("HSUPA receive activation: {error:?}"))?,
            )
            .map_err(|error| anyhow!("HSUPA prepared ingress reschedule: {error:?}"))?;
        self.configuration = configuration;
        Ok(())
    }

    pub fn identity(&self) -> ReceiveIdentity {
        self.receiver.identity()
    }
    pub fn configuration(&self) -> &Arc<UplinkActivation> {
        &self.configuration
    }
    #[cfg(test)]
    pub fn format_count(&self) -> usize {
        self.format_count
    }

    pub fn into_session(
        self,
        lease: crate::radio_resources::HsupaUserLease,
        procedure: Box<umts_rrc::hsupa::ActivationProcedure>,
        grant_value: u8,
        grant_ceiling: u8,
        grant_amplitude: f32,
        grant_refresh: Option<NonZeroU64>,
    ) -> Result<Box<super::Session>, Box<RejectedReceiveSession>> {
        if procedure.key() != self.identity().ue
            || !Arc::ptr_eq(procedure.construction(), &self.configuration)
            || procedure.phase() != umts_rrc::dedicated::ReconfigurationPhase::Prepared
        {
            return Err(Box::new(RejectedReceiveSession {
                error: SessionAssemblyError::Configuration,
                resources: self,
                lease,
                procedure,
            }));
        }
        let grants = match super::Grants::new(
            self.identity(),
            self.configuration.parameters().transport.interval(),
            self.configuration.activation().time(),
            grant_amplitude,
            grant_value,
        )
        .and_then(|grants| grants.with_refresh_period(grant_refresh))
        .and_then(|grants| grants.with_bounds(grant_value, grant_ceiling))
        {
            Ok(grants) => grants,
            Err(error) => {
                return Err(Box::new(RejectedReceiveSession {
                    error: SessionAssemblyError::Grant(error),
                    resources: self,
                    lease,
                    procedure,
                }));
            }
        };
        let Self {
            configuration,
            receiver,
            ingress,
            maximum_receive_bytes,
            format_count,
            catalog_bytes,
        } = self;
        let preparation = match super::Preparation::new(super::PreparationInputs {
            configuration,
            receiver,
            lease,
            maximum_receive_bytes,
        }) {
            Ok(preparation) => preparation,
            Err(rejected) => {
                let super::PreparationInputs {
                    configuration,
                    receiver,
                    lease,
                    maximum_receive_bytes,
                } = rejected.inputs;
                return Err(Box::new(RejectedReceiveSession {
                    error: SessionAssemblyError::Preparation,
                    resources: Self {
                        configuration,
                        receiver,
                        ingress,
                        maximum_receive_bytes,
                        format_count,
                        catalog_bytes,
                    },
                    lease,
                    procedure,
                }));
            }
        };
        Ok(Box::new(super::Session {
            preparation,
            procedure,
            admission: super::Admission::Pending,
            ingress: Some(ingress),
            grants: Some(grants),
            retirement: None,
            restoration: None,
        }))
    }

    // Build off the radio worker and reserve the cell's aggregate budget before publication.
    pub fn build(
        configuration: Arc<UplinkActivation>,
        inputs: ReceiveInputs,
        settings: ReceiveSettings,
    ) -> Result<Self> {
        let parameters = configuration.parameters();
        let physical = configuration.dedicated().physical();
        ensure!(
            inputs.code.code().number() == physical.uplink_scrambling,
            "HSUPA receive code differs from the signaled DPCH"
        );
        let transport = &parameters.transport;
        let interval = transport.interval();
        let activation =
            super::timing::receive_activation(configuration.activation(), inputs.uplink)
                .map_err(|error| anyhow!("HSUPA receive activation: {error:?}"))?;
        ensure!(
            inputs.identity.ue.epoch == activation.epoch
                && inputs.uplink.time.epoch == activation.epoch
                && inputs.common_frame.epoch == activation.epoch,
            "HSUPA receive coordinates belong to different epochs"
        );
        let offset = parameters
            .flows
            .first()
            .context("HSUPA has no MAC-d flow")?
            .power_offset();
        // TS 25.321 11.8.1.4 selects the profile from data priority or control-only SI.
        ensure!(
            parameters
                .flows
                .iter()
                .all(|flow| flow.power_offset() == offset)
                && parameters.scheduling.power_offset == offset,
            "HSUPA receive configuration needs distinct data/control HARQ power profiles"
        );
        let powers = parameters
            .references
            .as_slice()
            .iter()
            .map(|reference| {
                let (plan, layout) = TransportPlan::from_table(
                    transport.table(),
                    reference.format,
                    transport.category(),
                    transport.layouts(),
                    settings.maximum_format_bits,
                )
                .map_err(|error| anyhow!("HSUPA reference transport: {error:?}"))?;
                SingleReferencePower::new(
                    NonZeroUsize::new(plan.payload_bits()).context("empty HSUPA reference")?,
                    layout,
                    reference.power_index,
                )
                .map_err(|error| anyhow!("HSUPA reference power: {error:?}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut definitions = Vec::with_capacity(MAX_FORMATS);
        let mut catalog_bytes = definitions.capacity() * std::mem::size_of::<TrackedFormat>();
        ensure!(
            catalog_bytes <= settings.maximum_catalog_bytes.get(),
            "HSUPA catalog exceeds budget"
        );
        let mut maximum_payload = 0usize;
        for (identity, _) in transport.table().formats(interval, transport.category()) {
            let definition = match TrackedFormat::from_table(
                transport.table(),
                identity,
                transport.category(),
                transport.layouts(),
                powers[parameters.references.index_for(identity)],
                offset,
                settings.maximum_format_bits,
            ) {
                Ok(definition) => definition,
                // An E-TFC beyond the signaled code/puncturing capacity is not configured.
                Err(crate::hsupa_rx::ReceiveError::Dsp(DspError::InvalidRateMatching)) => continue,
                Err(error) => return Err(anyhow!("HSUPA E-TFC {}: {error:?}", identity.value())),
            };
            catalog_bytes = catalog_bytes
                .checked_add(
                    definition
                        .plan
                        .retained_bytes()
                        .context("HSUPA plan size overflow")?,
                )
                .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<usize>()))
                .and_then(|bytes| {
                    bytes
                        .checked_add(definition.gain_ratios.capacity() * std::mem::size_of::<f64>())
                })
                .context("HSUPA catalog size overflow")?;
            ensure!(
                catalog_bytes <= settings.maximum_catalog_bytes.get(),
                "HSUPA catalog exceeds budget"
            );
            maximum_payload = maximum_payload.max(definition.plan.payload_bits());
            definitions.push(definition);
        }
        ensure!(
            parameters
                .references
                .as_slice()
                .iter()
                .all(|reference| definitions
                    .iter()
                    .any(|format| format.identity == reference.format))
                && definitions
                    .iter()
                    .any(|format| format.identity == parameters.minimum),
            "HSUPA reference or minimum E-TFC cannot use the signaled physical layout"
        );
        let format_count = definitions.len();
        let maximum_payload = NonZeroUsize::new(maximum_payload).context("empty HSUPA catalog")?;
        let control = if configuration.maximum_dpdch() == 0 {
            Some(Box::new(
                inputs
                    .control_search
                    .prepare_edch_only(
                        inputs.code.clone(),
                        inputs.uplink.time,
                        configuration.hsdpa_active(),
                    )
                    .map_err(|error| anyhow!("HSUPA successor control search: {error:?}"))?,
            ))
        } else {
            None
        };
        let mut receiver = Box::new(
            TrackedReceiver::new(
                inputs.code,
                TrackedSettings {
                    identity: inputs.identity,
                    anchor: inputs.uplink,
                    interval,
                    maximum_dpdch: configuration.maximum_dpdch(),
                    hsdpa_configured: configuration.hsdpa_active(),
                    control_gain_ratio: control_gain_ratio(parameters.control.power_index())
                        .map_err(|error| anyhow!("HSUPA control gain: {error:?}"))?,
                    likelihood: settings.likelihood,
                    admission: settings.control_admission,
                    rv_zero_only: parameters.rv_zero_only,
                    iterations: settings.iterations,
                    retained_sample_bytes: settings.retained_sample_bytes,
                    soft_buffer_bytes: settings.soft_buffer_bytes,
                    scratch_bytes: settings.scratch_bytes,
                },
                definitions,
            )
            .map_err(|error| anyhow!("HSUPA tracked receiver: {error:?}"))?,
        );
        let receive = receiver
            .connect(settings.report_capacity, settings.report_bytes)
            .map_err(|error| anyhow!("HSUPA receive report pool: {error:?}"))?;
        let mut receiver = match control {
            Some(control) => HsupaAttachment::edch_only(receiver, control)
                .map_err(|_| anyhow!("HSUPA successor control ownership"))?,
            None => match configuration.as_ref() {
                UplinkActivation::Coexistence(value) => HsupaAttachment::coexistence(
                    receiver,
                    Box::new(
                        crate::dedicated_rx::PreparedDchSelection::new(value)
                            .map_err(|error| anyhow!("HSUPA DCH selection: {error:?}"))?,
                    ),
                ),
                UplinkActivation::EdchOnly(_) => unreachable!("E-DCH-only has prepared control"),
            },
        };
        if configuration.maximum_dpdch() == 0 && interval == umts_types::hsupa::Tti::TwoMilliseconds
        {
            match receiver.prepare_initial_suffixes() {
                Ok(()) | Err(ReceiveError::Capacity | ReceiveError::Dsp(DspError::Capacity)) => {}
                Err(error) => return Err(anyhow!("HSUPA initial suffix resources: {error:?}")),
            }
            if receiver
                .retained_bytes()
                .map_err(|error| anyhow!("HSUPA receive storage: {error:?}"))?
                > settings.maximum_receive_bytes.get()
            {
                receiver.discard_initial_suffixes();
            }
        }
        ensure!(
            receiver
                .retained_bytes()
                .map_err(|error| anyhow!("HSUPA receive storage: {error:?}"))?
                <= settings.maximum_receive_bytes.get(),
            "HSUPA receive owner exceeds budget"
        );
        let mappings: Result<Vec<_>> = configuration
            .mappings()
            .iter()
            .map(|mapping| {
                Ok((
                    mapping.uplink.ddi(),
                    Mapping {
                        logical_channel: mapping.uplink.logical(),
                        flow: mapping.uplink.flow(),
                        pdu_bits: NonZeroUsize::new(usize::from(mapping.uplink.pdu_bits()))
                            .context("empty HSUPA mapped PDU")?,
                    },
                ))
            })
            .collect();
        let mappings = mappings?;
        let required = PacketDelivery::required_capacity(&mappings)
            .map_err(|error| anyhow!("HSUPA packet capacity: {error:?}"))?;
        ensure!(
            settings.packet_capacity.get() >= required,
            "HSUPA packet_capacity must be at least {required} for the configured logical channels"
        );
        let packets = Box::new(
            PacketDelivery::new(
                inputs.identity,
                &mappings,
                maximum_payload,
                settings.packet_capacity,
                settings.reorder_hold_chips(interval)?,
                0,
                settings.packet_bytes,
            )
            .map_err(|error| anyhow!("HSUPA packet delivery: {error:?}"))?,
        );
        let dpch = DpchTiming::new(physical.frame_offset_256_chips)
            .map_err(|error| anyhow!("HSUPA DPCH timing: {error:?}"))?;
        let association = FeedbackAssociation::new(
            inputs.identity,
            inputs.uplink,
            inputs.common_frame,
            interval,
            dpch,
        )
        .map_err(|error| anyhow!("HSUPA feedback timing: {error:?}"))?;
        let ingress = Ingress::new(
            IngressSettings {
                identity: inputs.identity,
                activation,
                feedback_amplitude: settings.feedback_amplitude,
                feedback_capacity: settings.feedback_capacity,
            },
            IngressInputs {
                receive,
                packets,
                association,
            },
        )
        .map_err(|rejected| anyhow!("HSUPA ingress: {:?}", rejected.error))?;
        Ok(Self {
            configuration,
            receiver,
            ingress,
            maximum_receive_bytes: settings.maximum_receive_bytes,
            format_count,
            catalog_bytes,
        })
    }
}
