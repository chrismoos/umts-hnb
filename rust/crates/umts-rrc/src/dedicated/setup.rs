// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{DchDirection, DedicatedError, Rel99Configuration, Rel99Rlc};
use crate::establishment::TransactionId;
use rasn::types::FixedBitString;
use std::{collections::BTreeSet, num::NonZeroU8, sync::Arc};
use umts_rrc_codec::{EncodedRrc, encode_downlink_dedicated};
use umts_rrc_schema::{
    class_definitions as class, information_elements as ie, pdu_definitions as pdu,
};
use umts_types::{
    CnDomain,
    time::{CHIPS_PER_FRAME, ChipInstant},
};

const FIRST_USER_BEARER: u8 = 5;
const MAX_RABS: usize = 16;
const MAX_RB_PER_RAB: usize = 8;

#[path = "setup_hsdpa.rs"]
mod hsdpa;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRab {
    pub identity: u8,
    pub domain: CnDomain,
    pub nas_synchronization: Option<u8>,
    pub bearers: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct RetainedBearer {
    pub id: u8,
    pub domain: Option<CnDomain>,
    pub rlc: Rel99Rlc,
    pub uplink_pdu_bits: u16,
    pub downlink_pdu_bits: u16,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScheduledActivation {
    time: ChipInstant,
    cfn: u8,
}
impl ScheduledActivation {
    pub fn new(
        current_frame: ChipInstant,
        current_cfn: u8,
        frames_ahead: NonZeroU8,
    ) -> Result<Self, DedicatedError> {
        // TS 25.331 8.6.3.1 permits the next CFN cycle without the Delay restriction flag.
        let index = current_frame
            .index
            .checked_add(u64::from(frames_ahead.get()) * CHIPS_PER_FRAME)
            .ok_or(DedicatedError::Time)?;
        Ok(Self {
            time: ChipInstant {
                epoch: current_frame.epoch,
                index,
            },
            cfn: current_cfn.wrapping_add(frames_ahead.get()),
        })
    }
    pub fn time(self) -> ChipInstant {
        self.time
    }
    pub fn cfn(self) -> u8 {
        self.cfn
    }
}

pub struct InitialDchSetup {
    configuration: Arc<Rel99Configuration>,
    retained: Vec<RetainedBearer>,
    rabs: Vec<NewRab>,
}
pub struct ExistingDchSetup {
    retained_hsdpa: Option<Arc<crate::hsdpa::InitialConfiguration>>,
    previous: Arc<Rel99Configuration>,
    configuration: Arc<Rel99Configuration>,
    retained: Vec<RetainedBearer>,
    rabs: Vec<NewRab>,
    activation: ScheduledActivation,
}
impl InitialDchSetup {
    pub fn new(
        configuration: Arc<Rel99Configuration>,
        retained: Vec<RetainedBearer>,
        rabs: Vec<NewRab>,
    ) -> Result<Self, DedicatedError> {
        if ![1, 2, 3]
            .iter()
            .all(|id| retained.iter().any(|bearer| bearer.id == *id))
        {
            return Err(DedicatedError::Bearer);
        }
        validate(&configuration, &retained, &rabs)?;
        Ok(Self {
            configuration,
            retained,
            rabs,
        })
    }
    pub fn configuration(&self) -> &Arc<Rel99Configuration> {
        &self.configuration
    }
    pub fn rabs(&self) -> &[NewRab] {
        &self.rabs
    }
    pub fn message(&self, transaction: TransactionId) -> Result<EncodedRrc, DedicatedError> {
        // TS 25.331 8.6.3.1: FDD CELL_FACH entry cannot use a non-Now activation time.
        build(
            &self.configuration,
            &self.retained,
            &self.rabs,
            transaction,
            None,
            None,
        )
    }

    pub fn message_with_secondary_cell(
        &self,
        transaction: TransactionId,
        hsdpa: &crate::hsdpa::InitialConfiguration,
        secondary: crate::hsdpa::SecondaryCellUpdate<'_>,
    ) -> Result<EncodedRrc, DedicatedError> {
        if let crate::hsdpa::SecondaryCellUpdate::Configure(configuration) = secondary {
            hsdpa.validate_secondary_configuration(configuration)?;
        }
        crate::hsdpa::dual_cell::setup(
            transaction,
            hsdpa::body_r7(self, transaction, hsdpa)?,
            secondary,
        )
    }

    /// Requires identical configuration ownership and fixed-size AM packet bearers.
    pub fn message_with_evolved_hsdpa(
        &self,
        transaction: TransactionId,
        hsdpa: &crate::hsdpa::InitialConfiguration,
    ) -> Result<EncodedRrc, DedicatedError> {
        match hsdpa.secondary_cell() {
            Some(secondary) => self.message_with_secondary_cell(
                transaction,
                hsdpa,
                crate::hsdpa::SecondaryCellUpdate::Configure(secondary),
            ),
            None => hsdpa::message(self, transaction, hsdpa),
        }
    }
}
impl ExistingDchSetup {
    pub fn new(
        previous: Arc<Rel99Configuration>,
        configuration: Arc<Rel99Configuration>,
        rabs: Vec<NewRab>,
        activation: ScheduledActivation,
    ) -> Result<Self, DedicatedError> {
        let retained = previous
            .bearers()
            .iter()
            .map(|bearer| {
                let size = |direction| {
                    let tfcs = match direction {
                        DchDirection::Uplink => previous.uplink(),
                        DchDirection::Downlink => previous.downlink(),
                    };
                    tfcs.transport(
                        bearer
                            .mappings(direction)
                            .next()
                            .ok_or(DedicatedError::Mapping)?
                            .transport,
                    )
                    .and_then(|transport| transport.formats().next())
                    .map(|format| format.rlc_bits)
                    .ok_or(DedicatedError::Mapping)
                };
                Ok(RetainedBearer {
                    id: bearer.id,
                    domain: bearer.domain,
                    rlc: bearer.rlc.clone(),
                    uplink_pdu_bits: size(DchDirection::Uplink)?,
                    downlink_pdu_bits: size(DchDirection::Downlink)?,
                })
            })
            .collect::<Result<Vec<_>, DedicatedError>>()?;
        validate(&configuration, &retained, &rabs)?;
        for tfcs in [
            previous.uplink(),
            previous.downlink(),
            configuration.uplink(),
            configuration.downlink(),
        ] {
            if tfcs
                .transports()
                .iter()
                .any(|transport| !usize::from(activation.cfn).is_multiple_of(transport.frames()))
            {
                return Err(DedicatedError::Activation);
            }
        }
        Ok(Self {
            retained_hsdpa: None,
            previous,
            configuration,
            retained,
            rabs,
            activation,
        })
    }
    pub fn configuration(&self) -> &Arc<Rel99Configuration> {
        &self.configuration
    }
    pub fn previous(&self) -> &Arc<Rel99Configuration> {
        &self.previous
    }
    pub fn rabs(&self) -> &[NewRab] {
        &self.rabs
    }
    pub fn activation(&self) -> ScheduledActivation {
        self.activation
    }
    pub fn with_retained_hsdpa(
        mut self,
        configuration: Option<Arc<crate::hsdpa::InitialConfiguration>>,
    ) -> Self {
        self.retained_hsdpa = configuration;
        self
    }

    pub fn retained_hsdpa(&self) -> Option<&Arc<crate::hsdpa::InitialConfiguration>> {
        self.retained_hsdpa.as_ref()
    }

    pub fn message(&self, transaction: TransactionId) -> Result<EncodedRrc, DedicatedError> {
        if let Some(hs) = &self.retained_hsdpa {
            hs.validate_retained_flows(&self.configuration)?;
        }
        if let Some(hs) = self
            .retained_hsdpa
            .as_ref()
            .filter(|hs| hs.secondary_cell().is_some())
        {
            let base = body_r3(
                &self.configuration,
                &self.retained,
                &self.rabs,
                transaction,
                Some(&self.previous),
                Some(self.activation),
            )?;
            let base = hsdpa::body_r7_from_base(&self.configuration, base, hs, false)?;
            return crate::hsdpa::dual_cell::setup(
                transaction,
                base,
                crate::hsdpa::SecondaryCellUpdate::Continue,
            );
        }
        build(
            &self.configuration,
            &self.retained,
            &self.rabs,
            transaction,
            Some(&self.previous),
            Some(self.activation),
        )
    }
}

fn validate(
    configuration: &Rel99Configuration,
    retained: &[RetainedBearer],
    rabs: &[NewRab],
) -> Result<(), DedicatedError> {
    if rabs.is_empty() || rabs.len() > MAX_RABS {
        return Err(DedicatedError::Bearer);
    }
    let domain = rabs[0].domain;
    if rabs.iter().any(|rab| rab.domain != domain) {
        return Err(DedicatedError::Bearer);
    }
    let mut accounted = BTreeSet::new();
    for old in retained {
        let target = configuration.bearer(old.id).ok_or(DedicatedError::Bearer)?;
        if !accounted.insert(old.id) || target.domain != old.domain || target.rlc != old.rlc {
            return Err(DedicatedError::Bearer);
        }
        if matches!(old.rlc, Rel99Rlc::Acknowledged { .. }) {
            for (tfcs, direction, bits) in [
                (
                    configuration.uplink(),
                    DchDirection::Uplink,
                    old.uplink_pdu_bits,
                ),
                (
                    configuration.downlink(),
                    DchDirection::Downlink,
                    old.downlink_pdu_bits,
                ),
            ] {
                if target.mappings(direction).any(|mapping| {
                    tfcs.transport(mapping.transport).is_none_or(|transport| {
                        transport.formats().any(|format| format.rlc_bits != bits)
                    })
                }) {
                    return Err(DedicatedError::Format);
                }
            }
        }
    }
    let mut identities = Vec::new();
    for rab in rabs {
        if identities.contains(&(rab.domain, rab.identity))
            || rab.bearers.is_empty()
            || rab.bearers.len() > MAX_RB_PER_RAB
            || rab.nas_synchronization.is_some_and(|value| value > 15)
        {
            return Err(DedicatedError::Bearer);
        }
        identities.push((rab.domain, rab.identity));
        for &id in &rab.bearers {
            let bearer = configuration.bearer(id).ok_or(DedicatedError::Bearer)?;
            if id < FIRST_USER_BEARER || !accounted.insert(id) || bearer.domain != Some(rab.domain)
            {
                return Err(DedicatedError::Bearer);
            }
        }
    }
    if accounted.len() != configuration.bearers().len() {
        return Err(DedicatedError::Bearer);
    }
    Ok(())
}
fn fixed<const N: usize>(value: u32) -> FixedBitString<N> {
    let mut output = FixedBitString::default();
    for index in 0..N {
        output.set(index, value & (1 << (N - 1 - index)) != 0);
    }
    output
}
fn rab_wire(
    configuration: &Rel99Configuration,
    rab: &NewRab,
) -> Result<ie::RABInformationSetup, DedicatedError> {
    let domain = match rab.domain {
        CnDomain::Cs => ie::CNDomainIdentity::cs_domain,
        CnDomain::Ps => ie::CNDomainIdentity::ps_domain,
    };
    let timer = match rab.domain {
        CnDomain::Cs => ie::ReEstablishmentTimer::useT314,
        CnDomain::Ps => ie::ReEstablishmentTimer::useT315,
    };
    let mut bearers = Vec::with_capacity(rab.bearers.len());
    for &id in &rab.bearers {
        let bearer = configuration.bearer(id).ok_or(DedicatedError::Bearer)?;
        let pdcp = (rab.domain == CnDomain::Ps).then(|| {
            ie::PDCPInfo::new(
                Some(ie::LosslessSRNSRelocSupport::notSupported(())),
                ie::PDCPPDUHeader::absent,
                None,
            )
        });
        bearers.push(ie::RBInformationSetup::new(
            ie::RBIdentity(id),
            pdcp,
            bearer.rlc.wire(),
            bearer.mapping.wire(),
        ));
    }
    Ok(ie::RABInformationSetup::new(
        ie::RABInfo::new(
            ie::RABIdentity::gsm_MAP_RAB_Identity(rasn::types::BitString::from_slice(&[
                rab.identity
            ])),
            domain,
            rab.nas_synchronization
                .map(|value| ie::NASSynchronisationIndicator(fixed::<4>(u32::from(value)))),
            timer,
        ),
        ie::RBInformationSetupList(bearers),
    ))
}

fn build(
    configuration: &Rel99Configuration,
    retained: &[RetainedBearer],
    rabs: &[NewRab],
    transaction: TransactionId,
    previous: Option<&Rel99Configuration>,
    activation: Option<ScheduledActivation>,
) -> Result<EncodedRrc, DedicatedError> {
    let body = body_r3(
        configuration,
        retained,
        rabs,
        transaction,
        previous,
        activation,
    )?;
    encode_downlink_dedicated(&class::DLDCCHMessage::new(
        None,
        class::DLDCCHMessageType::radioBearerSetup(pdu::RadioBearerSetup::r3(
            pdu::RadioBearerSetupR3::new(body, None),
        )),
    ))
    .map_err(DedicatedError::Codec)
}

fn body_r3(
    configuration: &Rel99Configuration,
    retained: &[RetainedBearer],
    rabs: &[NewRab],
    transaction: TransactionId,
    previous: Option<&Rel99Configuration>,
    activation: Option<ScheduledActivation>,
) -> Result<pdu::RadioBearerSetupR3IEs, DedicatedError> {
    let uplink = configuration
        .uplink()
        .transports()
        .iter()
        .map(|transport| {
            Ok(ie::ULAddReconfTransChInformation::new(
                ie::ULTrCHType::dch,
                ie::TransportChannelIdentity(transport.id),
                transport.wire_uplink()?,
            ))
        })
        .collect::<Result<_, DedicatedError>>()?;
    let downlink = configuration
        .downlink()
        .transports()
        .iter()
        .map(|transport| {
            Ok(ie::DLAddReconfTransChInformation::new(
                ie::DLTrCHType::dch,
                ie::TransportChannelIdentity(transport.id),
                ie::DLAddReconfTransChInformationTfsSignallingMode::explicit_config(
                    transport.wire_downlink()?,
                ),
                None,
                None,
            ))
        })
        .collect::<Result<_, DedicatedError>>()?;
    // The first CELL_DCH setup also deletes the CELL_FACH setup's placeholder DCH (TS 25.331
    // 8.6.5.7 and 8.6.5.8), so the UE holds no transport channel outside the new TFCS.
    let placeholder = crate::establishment::UNUSED_DCH_ID;
    let deleted_ul: Vec<_> = match previous {
        Some(old) => old
            .uplink()
            .transports()
            .iter()
            .map(|old| old.id)
            .filter(|id| configuration.uplink().transport(*id).is_none())
            .collect::<Vec<u8>>(),
        None => (configuration.uplink().transport(placeholder).is_none())
            .then_some(placeholder)
            .into_iter()
            .collect(),
    }
    .into_iter()
    .map(|id| {
        ie::ULTransportChannelIdentity::new(ie::ULTrCHType::dch, ie::TransportChannelIdentity(id))
    })
    .collect();
    let deleted_dl: Vec<_> = match previous {
        Some(old) => old
            .downlink()
            .transports()
            .iter()
            .map(|old| old.id)
            .filter(|id| configuration.downlink().transport(*id).is_none())
            .collect::<Vec<u8>>(),
        None => (configuration.downlink().transport(placeholder).is_none())
            .then_some(placeholder)
            .into_iter()
            .collect(),
    }
    .into_iter()
    .map(|id| {
        ie::DLTransportChannelIdentity::new(ie::DLTrCHType::dch, ie::TransportChannelIdentity(id))
    })
    .collect::<Vec<_>>();
    let physical_changed = previous.is_none_or(|old| old.physical() != configuration.physical());
    let (common, links) = if physical_changed {
        let (common, links) = configuration.physical().downlink_wire(previous.is_none())?;
        (Some(common), Some(links))
    } else {
        (None, None)
    };
    // TS 25.331 8.6.4.8 replaces all stored mappings, including a retained HS-DSCH route.
    let affected: Vec<_> = retained
        .iter()
        .filter_map(|old| {
            let bearer = configuration
                .bearer(old.id)
                .expect("validated retained bearer");
            if previous
                .and_then(|previous| previous.bearer(old.id))
                .is_some_and(|previous| previous.mapping == bearer.mapping)
            {
                return None;
            }
            Some(ie::RBInformationAffected::new(
                ie::RBIdentity(old.id),
                bearer.mapping.wire(),
            ))
        })
        .collect();
    let body = pdu::RadioBearerSetupR3IEs {
        rrc_transaction_identifier: ie::RRCTransactionIdentifier(transaction.get()),
        integrity_protection_mode_info: None,
        ciphering_mode_info: None,
        activation_time: activation.map(|activation| ie::ActivationTime(activation.cfn)),
        new_u_rnti: None,
        new_c_rnti: None,
        rrc_state_indicator: ie::RRCStateIndicator::cell_DCH,
        utran_drx_cycle_length_coeff: None,
        ura_identity: None,
        cn_information_info: None,
        srb_information_setup_list: None,
        rab_information_setup_list: Some(ie::RABInformationSetupList(
            rabs.iter()
                .map(|rab| rab_wire(configuration, rab))
                .collect::<Result<_, _>>()?,
        )),
        rb_information_affected_list: (!affected.is_empty())
            .then_some(ie::RBInformationAffectedList(affected)),
        dl_counter_synchronisation_info: None,
        ul_common_trans_ch_info: Some(ie::ULCommonTransChInfo::new(
            None,
            None,
            Some(ie::ULCommonTransChInfoModeSpecificInfo::fdd(
                ie::ULCommonTransChInfoModeSpecificInfoFdd::new(configuration.uplink().wire()?),
            )),
        )),
        ul_deleted_trans_ch_info_list: (!deleted_ul.is_empty())
            .then_some(ie::ULDeletedTransChInfoList(deleted_ul)),
        ul_add_reconf_trans_ch_info_list: Some(ie::ULAddReconfTransChInfoList(uplink)),
        dummy: None,
        dl_common_trans_ch_info: Some(ie::DLCommonTransChInfo::new(
            None,
            ie::DLCommonTransChInfoModeSpecificInfo::fdd(
                ie::DLCommonTransChInfoModeSpecificInfoFdd::new(Some(
                    ie::DLCommonTransChInfoModeSpecificInfoFddDlParameters::dl_DCH_TFCS(
                        configuration.downlink().wire()?,
                    ),
                )),
            ),
        )),
        dl_deleted_trans_ch_info_list: (!deleted_dl.is_empty())
            .then_some(ie::DLDeletedTransChInfoList(deleted_dl)),
        dl_add_reconf_trans_ch_info_list: Some(ie::DLAddReconfTransChInfoList(downlink)),
        frequency_info: None,
        max_allowed_ul_tx_power: physical_changed.then_some(ie::MaxAllowedULTXPower(
            configuration.physical().maximum_uplink_dbm,
        )),
        ul_channel_requirement: if physical_changed {
            Some(configuration.physical().uplink_wire()?)
        } else {
            None
        },
        mode_specific_phys_ch_info: pdu::RadioBearerSetupR3IEsModeSpecificPhysChInfo::fdd(
            pdu::RadioBearerSetupR3IEsModeSpecificPhysChInfoFdd::new(None),
        ),
        dl_common_information: common,
        dl_information_per_rl_list: links,
    };
    Ok(body)
}

pub(super) fn retained_release_r7(
    configuration: &Rel99Configuration,
    previous: &Rel99Configuration,
    activation: ScheduledActivation,
    transaction: TransactionId,
    affected: Option<ie::RBInformationAffectedList>,
    hs: &crate::hsdpa::InitialConfiguration,
) -> Result<pdu::RadioBearerSetupR7IEs, DedicatedError> {
    let mut base = body_r3(
        configuration,
        &[],
        &[],
        transaction,
        Some(previous),
        Some(activation),
    )?;
    base.rb_information_affected_list = affected;
    hsdpa::body_r7_from_base(configuration, base, hs, false)
}
