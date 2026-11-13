// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::signalling::AcceptedAdditionalSetup;
use umts_security::ciphering::{
    AddedBearerCipherConfiguration, CipherUse, ExistingTransparentBearer,
};
use umts_types::Direction;

impl DedicatedBearers {
    pub fn install_additional_users(
        &mut self,
        accepted: &AcceptedAdditionalSetup,
        contexts: [Option<UserCipher>; DOMAINS],
        ciphers: &mut BearerCiphers,
        now: ChipInstant,
    ) -> Result<(), BearerError> {
        if self.phase != BearerPhase::Active
            || accepted.key() != self.owner
            || now.epoch != self.owner.epoch
            || now.index < accepted.time().serviced().index
        {
            return Err(BearerError::Phase);
        }
        let next = self.mapping_index(accepted.identity())?;
        let previous = self.mapping_index(accepted.previous_identity())?;
        let map = self.maps[next].as_ref().ok_or(BearerError::Mapping)?;
        let old = self.maps[previous].as_ref().ok_or(BearerError::Mapping)?;
        if !map.additional
            || map.added_bearers.is_empty()
            || !matches!(map.phase, MappingPhase::Scheduled { previous } if previous == accepted.previous_identity())
            || !map
                .plan
                .as_ref()
                .is_some_and(|plan| Arc::ptr_eq(plan.configuration(), accepted.configuration()))
            || !old
                .plan
                .as_ref()
                .is_some_and(|plan| Arc::ptr_eq(plan.configuration(), accepted.previous()))
            || map.anchor.cfn != accepted.activation().cfn()
            || map
                .downlink
                .as_ref()
                .is_none_or(|downlink| downlink.anchor.time != accepted.activation().time())
        {
            return Err(BearerError::Mapping);
        }
        let mut profiles: Option<Arc<UserRlcProfiles>> = None;
        for bearer in &map.added_bearers {
            let index = self.lookup[bearer.index()].ok_or(BearerError::Mapping)?;
            let Entity::AwaitingUser {
                profiles: candidate,
                ..
            } = &self.endpoints[index].entity
            else {
                return Err(BearerError::Phase);
            };
            if profiles
                .as_ref()
                .is_some_and(|selected| !Arc::ptr_eq(selected, candidate))
            {
                return Err(BearerError::Mapping);
            }
            profiles = Some(candidate.clone());
        }
        let profiles = profiles.ok_or(BearerError::Phase)?;
        if !map
            .plan
            .as_ref()
            .is_some_and(|plan| Arc::ptr_eq(plan, profiles.plan()))
        {
            return Err(BearerError::Mapping);
        }
        let start = accepted.counters().new_bearer_start();
        let mut registrations = Vec::with_capacity(map.added_bearers.len());
        let mut updates = Vec::with_capacity(DOMAINS);
        let mut transparent = [false; DOMAINS];
        let mut activation = None;
        for bearer in &map.added_bearers {
            let endpoint =
                &self.endpoints[self.lookup[bearer.index()].ok_or(BearerError::Mapping)?];
            let Scope::User(domain) = endpoint.scope else {
                return Err(BearerError::Mapping);
            };
            let slot = match domain {
                CnDomain::Cs => 0,
                CnDomain::Ps => 1,
            };
            let origin = contexts[slot]
                .as_ref()
                .map(|context| context.context.origin());
            if endpoint.mode() == CipherMode::Transparent
                && ciphers.has_transparent_domain(self.owner, domain)?
            {
                let retained = self
                    .endpoints
                    .iter()
                    .find(|endpoint| {
                        endpoint.scope == Scope::User(domain)
                            && matches!(endpoint.entity, Entity::Transparent { .. })
                    })
                    .ok_or(BearerError::Mapping)?;
                let CipherUse::Data { origin, .. } = ciphers.selected(
                    self.owner,
                    retained.id.get(),
                    Direction::Uplink,
                    Position::Frame(map.first_frame),
                )?
                else {
                    return Err(BearerError::Mapping);
                };
                if !matches!(ciphers.selected(self.owner, retained.id.get(), Direction::Downlink,
                    Position::Frame(map.first_frame))?, CipherUse::Data { origin: downlink, .. } if downlink == origin)
                {
                    return Err(BearerError::Mapping);
                }
                registrations.push(AddedBearerCipherConfiguration::ExistingTransparent(
                    ExistingTransparentBearer {
                        bearer: bearer.get(),
                        domain,
                        origin,
                        first_frame: map.first_frame,
                    },
                ));
                continue;
            }
            let start = start.ok_or(BearerError::MissingStart(domain))?;
            let anchor = match endpoint.mode() {
                CipherMode::Acknowledged | CipherMode::Unacknowledged => CountAnchor::from_start(
                    endpoint.mode(),
                    Position::Sequence(u64::from(start.initial_count())),
                    start,
                ),
                CipherMode::Transparent => {
                    if map
                        .downlink
                        .as_ref()
                        .ok_or(BearerError::Mapping)?
                        .first_frame
                        != map.first_frame
                    {
                        return Err(BearerError::Time);
                    }
                    let boundary = if let Some(boundary) = activation {
                        boundary
                    } else {
                        let air = accepted.time().air_start().ok_or(BearerError::Time)?;
                        let delta = air
                            .index
                            .checked_sub(map.anchor.time.index)
                            .filter(|_| air.epoch == self.owner.epoch)
                            .ok_or(BearerError::Time)?;
                        let observed = map
                            .first_frame
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
                        CountAnchor::transparent_frozen_hfn(
                            map.first_frame,
                            Start::new(
                                accepted
                                    .prior_starts()
                                    .get(domain)
                                    .ok_or(BearerError::MissingStart(domain))?,
                            )
                            .map_err(|_| BearerError::Mapping)?,
                        )
                    } else {
                        CountAnchor::transparent_stopped(map.first_frame, start)
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
                        updates.push(TransparentCounterActivation {
                            domain,
                            origin,
                            anchor: next,
                            first_frame: map.first_frame,
                        });
                    }
                    anchor
                }
            }
            .map_err(|error| BearerError::Cipher(error.into()))?;
            registrations.push(AddedBearerCipherConfiguration::Initialized(
                BearerCipherConfiguration {
                    bearer: bearer.get(),
                    scope: endpoint.scope,
                    transmit: anchor,
                    receive: anchor,
                    initial: contexts[slot]
                        .as_ref()
                        .map_or(Configuration::Unprotected, |context| {
                            Configuration::Configured(context.context.clone())
                        }),
                },
            ));
        }
        let mut replacements = profiles.instantiate(self.owner, [start; DOMAINS], now)?;
        if replacements.len() != map.added_bearers.len()
            || replacements
                .iter()
                .any(|endpoint| !map.added_bearers.contains(&endpoint.id))
        {
            return Err(BearerError::Mapping);
        }
        for replacement in &mut replacements {
            let index = self.lookup[replacement.id.index()].ok_or(BearerError::Mapping)?;
            preserve_reservation(&self.endpoints[index], replacement)?;
        }
        ciphers
            .register_added_with(self.owner, registrations, &updates, || {
                for replacement in replacements {
                    let index =
                        self.lookup[replacement.id.index()].expect("validated additional slot");
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
