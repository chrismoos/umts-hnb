// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    connection_security::UserCipher, rel99_plan::CompiledRel99, signalling::AcceptedUserSetup,
};
use umts_security::ciphering::{
    BearerCipherConfiguration, BearerCipherUpdateError, BearerCiphers, Configuration, CountAnchor,
    Position, TransparentCounterActivation, next_occurrence,
};
use umts_security::counters::Start;
use umts_types::CnDomain;

const TM_RESPONSE_MARGIN_FRAMES: u64 = 200;
const DOMAINS: usize = 2;

mod additional;

#[cfg(test)]
mod tests;

pub(crate) struct VoiceTiming {
    pub origin: ChipInstant,
    pub first_frame: u64,
}
pub(crate) struct VoiceAnchors {
    pub uplink: VoiceTiming,
    pub downlink: VoiceTiming,
}

fn preserve_reservation(
    original: &BearerEndpoint,
    replacement: &mut BearerEndpoint,
) -> Result<(), BearerError> {
    if !matches!(original.entity, Entity::AwaitingUser { .. })
        || original.ue != replacement.ue
        || original.id != replacement.id
        || original.scope != replacement.scope
        || original.mode() != replacement.mode()
    {
        return Err(BearerError::Mapping);
    }
    if replacement.storage_bytes > original.storage_bytes {
        return Err(BearerError::Capacity);
    }
    // The bank retains its original reservation until the endpoint is retired, not merely initialized.
    replacement.storage_bytes = original.storage_bytes;
    Ok(())
}

impl DedicatedBearers {
    pub(crate) fn voice_binding_anchor(
        &self,
        identity: ReceiveIdentity,
        bearers: &[BearerId],
        plan: &Arc<CompiledRel99>,
    ) -> Option<VoiceAnchors> {
        if self.phase != BearerPhase::Active || self.identity() != identity {
            return None;
        }
        let map = self.current_map();
        if !map
            .plan
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, plan))
            || bearers.iter().any(|bearer| {
                self.lookup[bearer.index()].is_none_or(|index| {
                    let endpoint = &self.endpoints[index];
                    endpoint.scope != Scope::User(CnDomain::Cs)
                        || !matches!(endpoint.entity, Entity::Transparent { .. })
                })
            })
        {
            return None;
        }
        let downlink = map.downlink.as_ref()?;
        Some(VoiceAnchors {
            uplink: VoiceTiming {
                origin: map.anchor.time,
                first_frame: map.first_frame,
            },
            downlink: VoiceTiming {
                origin: downlink.anchor.time,
                first_frame: downlink.first_frame,
            },
        })
    }

    pub(crate) fn packet_binding_ready(
        &self,
        identity: ReceiveIdentity,
        bearer: BearerId,
        plan: &Arc<CompiledRel99>,
    ) -> bool {
        self.phase == BearerPhase::Active
            && self.identity() == identity
            && self
                .current_map()
                .plan
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, plan))
            && self.lookup[bearer.index()].is_some_and(|index| {
                let endpoint = &self.endpoints[index];
                endpoint.scope == Scope::User(CnDomain::Ps)
                    && matches!(
                        endpoint.entity,
                        Entity::Acknowledged(_) | Entity::Unacknowledged { .. }
                    )
            })
    }
    pub fn user_bearers_ready(&self) -> bool {
        !self
            .endpoints
            .iter()
            .any(|endpoint| matches!(endpoint.entity, Entity::AwaitingUser { .. }))
    }
    pub(crate) fn awaiting_users(&self) -> Result<Option<Arc<UserRlcProfiles>>, BearerError> {
        let mut result = None;
        for endpoint in &self.endpoints {
            if endpoint.scope == Scope::Signalling {
                continue;
            }
            match &endpoint.entity {
                Entity::AwaitingUser { profiles, .. } => {
                    if result
                        .as_ref()
                        .is_some_and(|old| !Arc::ptr_eq(old, profiles))
                    {
                        return Err(BearerError::Mapping);
                    }
                    result = Some(profiles.clone());
                }
                _ if !self.user_bearers_ready() => return Err(BearerError::Phase),
                _ => (),
            }
        }
        Ok(result)
    }
    pub(crate) fn install_initial_users(
        &mut self,
        accepted: &AcceptedUserSetup,
        contexts: [Option<UserCipher>; DOMAINS],
        ciphers: &mut BearerCiphers,
        now: ChipInstant,
    ) -> Result<(), BearerError> {
        if self.phase != BearerPhase::Active
            || accepted.key() != self.owner
            || self.maps.iter().flatten().count() != 1
            || ciphers.has_user_bearers(self.owner)?
        {
            return Err(BearerError::Phase);
        }
        let profiles = self.awaiting_users()?.ok_or(BearerError::Phase)?;
        let map = self.current_map();
        let plan = map.plan.as_ref().ok_or(BearerError::Mapping)?;
        if !Arc::ptr_eq(plan, profiles.plan())
            || accepted
                .initial_hsdpa()
                .is_some_and(|hs| !Arc::ptr_eq(hs.dedicated(), plan.configuration()))
        {
            return Err(BearerError::Mapping);
        }
        let start = accepted.counters().new_bearer_start();
        let first_frame = map.first_frame;
        let mut activation = None;
        let mut registrations = Vec::with_capacity(self.endpoints.len());
        let mut counter_updates = Vec::with_capacity(DOMAINS);
        let mut transparent = [false; DOMAINS];
        for endpoint in &self.endpoints {
            let Entity::AwaitingUser { .. } = &endpoint.entity else {
                continue;
            };
            let Scope::User(domain) = endpoint.scope else {
                return Err(BearerError::Mapping);
            };
            let slot = match domain {
                CnDomain::Cs => 0,
                CnDomain::Ps => 1,
            };
            let mode = endpoint.mode();
            let initial = contexts[slot]
                .as_ref()
                .map_or(Configuration::Unprotected, |context| {
                    Configuration::Configured(context.context.clone())
                });
            let anchor = match mode {
                CipherMode::Acknowledged | CipherMode::Unacknowledged => {
                    let start = start.ok_or(BearerError::MissingStart(domain))?;
                    CountAnchor::from_start(
                        mode,
                        Position::Sequence(u64::from(start.initial_count())),
                        start,
                    )
                }
                CipherMode::Transparent => {
                    if map
                        .downlink
                        .as_ref()
                        .ok_or(BearerError::Mapping)?
                        .first_frame
                        != first_frame
                    {
                        return Err(BearerError::Time);
                    }
                    let start = start.ok_or(BearerError::MissingStart(domain))?;
                    let boundary = if let Some(boundary) = activation {
                        boundary
                    } else {
                        let air = accepted.time().air_start().ok_or(BearerError::Time)?;
                        let delta = air
                            .index
                            .checked_sub(map.anchor.time.index)
                            .filter(|_| air.epoch == self.owner.epoch)
                            .ok_or(BearerError::Time)?;
                        let observed = first_frame
                            .checked_add(delta / CHIPS_PER_FRAME)
                            .ok_or(BearerError::Time)?;
                        let cfn = accepted
                            .counters()
                            .transparent_cfn()
                            .ok_or(BearerError::Time)?;
                        let Position::Frame(boundary) = next_occurrence(
                            CipherMode::Transparent,
                            u16::from(cfn),
                            Position::Frame(observed),
                        )
                        .map_err(|error| BearerError::Cipher(error.into()))?
                        else {
                            unreachable!("transparent frame")
                        };
                        if boundary
                            .checked_sub(observed)
                            .is_none_or(|distance| distance < TM_RESPONSE_MARGIN_FRAMES)
                        {
                            return Err(BearerError::Time);
                        }
                        activation = Some(boundary);
                        boundary
                    };
                    let anchor = if contexts[slot].is_some() {
                        let latest = Start::new(
                            accepted
                                .prior_starts()
                                .get(domain)
                                .ok_or(BearerError::MissingStart(domain))?,
                        )
                        .map_err(|_| BearerError::Mapping)?;
                        CountAnchor::transparent_frozen_hfn(first_frame, latest)
                    } else {
                        CountAnchor::transparent_stopped(first_frame, start)
                    };
                    if !std::mem::replace(&mut transparent[slot], true) {
                        let next = if contexts[slot].is_some() {
                            CountAnchor::from_start(
                                CipherMode::Transparent,
                                Position::Frame(boundary),
                                start,
                            )
                        } else {
                            CountAnchor::transparent_stopped(boundary, start)
                        }
                        .map_err(|error| BearerError::Cipher(error.into()))?;
                        counter_updates.push(TransparentCounterActivation {
                            domain,
                            origin: contexts[slot]
                                .as_ref()
                                .map(|context| context.context.origin()),
                            anchor: next,
                            first_frame,
                        });
                    }
                    anchor
                }
            }
            .map_err(|error| BearerError::Cipher(error.into()))?;
            registrations.push(BearerCipherConfiguration {
                bearer: endpoint.id.get(),
                scope: endpoint.scope,
                transmit: anchor,
                receive: anchor,
                initial,
            });
        }
        let mut replacements = profiles.instantiate_accepted(accepted, now)?;
        for replacement in &mut replacements {
            let index = self.lookup[replacement.id.index()].ok_or(BearerError::Mapping)?;
            let original = &self.endpoints[index];
            preserve_reservation(original, replacement)?;
        }
        ciphers
            .register_initial_with(registrations, &counter_updates, || {
                for replacement in replacements {
                    let index =
                        self.lookup[replacement.id.index()].expect("validated original slot");
                    self.endpoints[index] = replacement;
                }
                Ok::<_, std::convert::Infallible>(())
            })
            .map_err(|error| match error {
                BearerCipherUpdateError::Cipher(error) => BearerError::Cipher(error),
                BearerCipherUpdateError::Publication(never) => match never {},
            })?;
        Ok(())
    }
}
