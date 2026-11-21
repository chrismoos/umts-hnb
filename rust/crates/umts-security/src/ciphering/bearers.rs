// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroUsize;

use super::{
    Binding, CipherBudget, CipherTimeline, CipherUse, Configuration, CountAnchor, Error, Position,
    ResetConfiguration, Scope, TransparentDomain, UpdateError,
};
use crate::{
    contexts::{CipherContext, KeyOrigin},
    counters::CipherMode,
};
use umts_types::{CnDomain, Direction, UeKey};

const MAX_BEARERS: usize = 32;
const DOMAINS: usize = 2;
const TRANSMIT_FLAG: u8 = 1;
const RECEIVE_FLAG: u8 = 2;

pub struct BearerCipherConfiguration {
    pub bearer: u8,
    pub scope: Scope,
    pub transmit: CountAnchor,
    pub receive: CountAnchor,
    pub initial: Configuration,
}
pub struct TransparentCounterActivation {
    pub domain: CnDomain,
    pub origin: Option<KeyOrigin>,
    pub anchor: CountAnchor,
    pub first_frame: u64,
}

pub struct ExistingTransparentBearer {
    pub bearer: u8,
    pub domain: CnDomain,
    pub origin: Option<KeyOrigin>,
    pub first_frame: u64,
}

pub enum AddedBearerCipherConfiguration {
    Initialized(BearerCipherConfiguration),
    ExistingTransparent(ExistingTransparentBearer),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CipherActivation {
    Sequenced {
        bearer: u8,
        direction: Direction,
        anchor: CountAnchor,
        first_new: Position,
    },
    Transparent {
        domain: CnDomain,
        anchor: CountAnchor,
        first_frame: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CipherReset {
    pub bearer: u8,
    pub origin: Option<KeyOrigin>,
    pub transmit: CountAnchor,
    pub receive: CountAnchor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CipherRetirement {
    Sequenced {
        bearer: u8,
        direction: Direction,
        floor: Position,
    },
    Transparent {
        domain: CnDomain,
        first_frame: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BearerCipherError {
    Cipher(Error),
    Layout,
    Duplicate,
    Capacity,
}
impl From<Error> for BearerCipherError {
    fn from(error: Error) -> Self {
        Self::Cipher(error)
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum BearerCipherUpdateError<E> {
    Cipher(BearerCipherError),
    Publication(E),
}

#[derive(Clone)]
enum Channel {
    Sequenced(Box<SequenceCiphers>),
    Transparent { domain: CnDomain, first_frame: u64 },
}
#[derive(Clone)]
struct SequenceCiphers {
    transmit: CipherTimeline,
    receive: CipherTimeline,
}
struct Tables {
    index: [Option<usize>; MAX_BEARERS],
    channels: Vec<Channel>,
    transparent: [Option<TransparentDomain>; DOMAINS],
}

pub struct BearerCiphers {
    ue: UeKey,
    capacity: NonZeroUsize,
    tables: Tables,
}

impl BearerCiphers {
    pub fn new(ue: UeKey, capacity: NonZeroUsize) -> Result<Self, BearerCipherError> {
        Self::maximum_storage_bytes(capacity)?;
        Ok(Self {
            ue,
            capacity,
            tables: Tables {
                index: [None; MAX_BEARERS],
                channels: Vec::with_capacity(capacity.get()),
                transparent: std::array::from_fn(|_| None),
            },
        })
    }
    /// Includes a private control-plane staging copy, but not the separately owned immutable key schedules.
    pub fn maximum_storage_bytes(capacity: NonZeroUsize) -> Result<usize, BearerCipherError> {
        if capacity.get() > MAX_BEARERS {
            return Err(BearerCipherError::Capacity);
        }
        std::mem::size_of::<Self>()
            .checked_add(
                capacity
                    .get()
                    .checked_mul(
                        std::mem::size_of::<Channel>() + std::mem::size_of::<SequenceCiphers>(),
                    )
                    .ok_or(BearerCipherError::Capacity)?,
            )
            .and_then(|bytes| bytes.checked_mul(2))
            .ok_or(BearerCipherError::Capacity)
    }
    pub fn len(&self) -> usize {
        self.tables.channels.len()
    }
    pub fn layout(&self, key: UeKey, bearer: u8) -> Result<(Scope, CipherMode), BearerCipherError> {
        self.owner(key)?;
        match channel(&self.tables, bearer)? {
            Channel::Sequenced(ciphers) => {
                let binding = ciphers.receive.binding();
                Ok((binding.scope(), binding.mode()))
            }
            Channel::Transparent { domain, .. } => {
                Ok((Scope::User(*domain), CipherMode::Transparent))
            }
        }
    }
    pub fn pending_sequence_activation(
        &self,
        key: UeKey,
        bearer: u8,
        direction: Direction,
        next: Position,
    ) -> Result<Option<Position>, BearerCipherError> {
        self.owner(key)?;
        match channel(&self.tables, bearer)? {
            Channel::Sequenced(ciphers) => match direction {
                Direction::Downlink => &ciphers.transmit,
                Direction::Uplink => &ciphers.receive,
            }
            .pending_activation(key, next)
            .map_err(Into::into),
            Channel::Transparent { .. } => Err(BearerCipherError::Layout),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.tables.channels.is_empty()
    }
    pub fn has_user_bearers(&self, key: UeKey) -> Result<bool, BearerCipherError> {
        self.owner(key)?;
        Ok(self.tables.channels.iter().any(|channel| match channel {
            Channel::Transparent { .. } => true,
            Channel::Sequenced(value) => matches!(value.receive.binding().scope(), Scope::User(_)),
        }))
    }

    pub fn has_transparent_domain(
        &self,
        key: UeKey,
        domain: CnDomain,
    ) -> Result<bool, BearerCipherError> {
        self.owner(key)?;
        Ok(has_transparent_domain(&self.tables, domain))
    }

    pub fn reset_configuration(
        &self,
        key: UeKey,
        bearer: u8,
        direction: Direction,
        next: Position,
    ) -> Result<ResetConfiguration, BearerCipherError> {
        self.owner(key)?;
        match channel(&self.tables, bearer)? {
            Channel::Sequenced(ciphers) => match direction {
                Direction::Downlink => &ciphers.transmit,
                Direction::Uplink => &ciphers.receive,
            }
            .reset_configuration(key, next)
            .map_err(Into::into),
            Channel::Transparent { .. } => Err(BearerCipherError::Layout),
        }
    }

    pub fn register_with<T, E>(
        &mut self,
        configuration: BearerCipherConfiguration,
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, BearerCipherUpdateError<E>> {
        self.register_batch_with(std::iter::once(configuration), publish)
    }

    pub fn register_batch_with<T, E>(
        &mut self,
        configurations: impl IntoIterator<Item = BearerCipherConfiguration>,
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, BearerCipherUpdateError<E>> {
        self.register_initial_with(configurations, &[], publish)
    }

    /// Drops the transparent-mode timelines of both domains once no transparent bearer uses
    /// them. Their frame positions belong to one dedicated channel, so a UE that left CELL_DCH
    /// starts new ones on its next channel (TS 25.331 8.5.15.1 restarts CFN counting).
    pub fn forget_transparent_domains(&mut self, key: UeKey) -> Result<(), BearerCipherError> {
        self.owner(key)?;
        if self
            .tables
            .channels
            .iter()
            .any(|channel| matches!(channel, Channel::Transparent { .. }))
        {
            return Err(BearerCipherError::Layout);
        }
        self.tables.transparent = Default::default();
        Ok(())
    }

    /// Commit only after the removed RBs' actual RX/TX owners retire and their pending output is settled.
    pub fn unregister_batch_with<T, E>(
        &mut self,
        key: UeKey,
        bearers: &[u8],
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, BearerCipherUpdateError<E>> {
        let fail = BearerCipherUpdateError::Cipher;
        self.owner(key).map_err(fail)?;
        if bearers.is_empty() || bearers.len() > MAX_BEARERS {
            return Err(fail(BearerCipherError::Layout));
        }
        let mut seen = [false; MAX_BEARERS];
        let mut removed = [false; MAX_BEARERS];
        for bearer in bearers {
            let slot = bearer_slot(*bearer).map_err(fail)?;
            if std::mem::replace(&mut seen[slot], true) {
                return Err(fail(BearerCipherError::Duplicate));
            }
            let channel = channel(&self.tables, *bearer).map_err(fail)?;
            if matches!(channel, Channel::Sequenced(value) if value.receive.binding().scope() == Scope::Signalling)
            {
                return Err(fail(BearerCipherError::Layout));
            }
            let index = self.tables.index[slot].ok_or(fail(BearerCipherError::Layout))?;
            removed[index] = true;
        }
        let mut remap = [None; MAX_BEARERS];
        let mut remaining = 0;
        for (old, index) in remap
            .iter_mut()
            .enumerate()
            .take(self.tables.channels.len())
        {
            if !removed[old] {
                *index = Some(remaining);
                remaining += 1;
            }
        }
        let mut index = [None; MAX_BEARERS];
        for (slot, old) in self.tables.index.iter().enumerate() {
            if let Some(old) = old {
                index[slot] = remap[*old];
            }
        }
        let result = publish().map_err(BearerCipherUpdateError::Publication)?;
        let mut old = 0;
        self.tables.channels.retain(|_| {
            let keep = !removed[old];
            old += 1;
            keep
        });
        self.tables.index = index;
        Ok(result)
    }

    pub fn register_initial_with<T, E>(
        &mut self,
        configurations: impl IntoIterator<Item = BearerCipherConfiguration>,
        counters: &[TransparentCounterActivation],
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, BearerCipherUpdateError<E>> {
        let fail = BearerCipherUpdateError::Cipher;
        if counters.len() > DOMAINS {
            return Err(fail(BearerCipherError::Capacity));
        }
        let mut candidate = self.staged();
        let original_length = candidate.channels.len();
        for configuration in configurations {
            self.register_candidate(&mut candidate, configuration)
                .map_err(fail)?;
        }
        if candidate.channels.len() == original_length {
            return Err(fail(BearerCipherError::Layout));
        }
        let mut seen = [false; DOMAINS];
        for counter in counters {
            let slot = domain_index(counter.domain);
            if std::mem::replace(&mut seen[slot], true) {
                return Err(fail(BearerCipherError::Duplicate));
            }
            candidate.transparent[slot]
                .as_mut()
                .ok_or(fail(BearerCipherError::Layout))?
                .activate_counter_with(
                    self.ue,
                    counter.origin,
                    counter.anchor,
                    counter.first_frame,
                    || Ok::<_, std::convert::Infallible>(()),
                )
                .map_err(|error| fail(update_error(error)))?;
        }
        self.budget(&candidate).map_err(fail)?;
        let result = publish().map_err(BearerCipherUpdateError::Publication)?;
        self.tables = candidate;
        Ok(result)
    }

    pub fn register_added_with<T, E>(
        &mut self,
        key: UeKey,
        configurations: impl IntoIterator<Item = AddedBearerCipherConfiguration>,
        counters: &[TransparentCounterActivation],
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, BearerCipherUpdateError<E>> {
        let fail = BearerCipherUpdateError::Cipher;
        self.owner(key).map_err(fail)?;
        if counters.len() > DOMAINS {
            return Err(fail(BearerCipherError::Capacity));
        }
        let mut candidate = self.staged();
        let original_length = candidate.channels.len();
        let existing_domains = [
            has_transparent_domain(&self.tables, CnDomain::Cs),
            has_transparent_domain(&self.tables, CnDomain::Ps),
        ];
        let mut new_domains = [false; DOMAINS];
        for configuration in configurations {
            match configuration {
                AddedBearerCipherConfiguration::Initialized(configuration) => {
                    let Scope::User(domain) = configuration.scope else {
                        return Err(fail(BearerCipherError::Layout));
                    };
                    let slot = domain_index(domain);
                    match configuration.transmit.mode() {
                        CipherMode::Transparent => {
                            if existing_domains[slot]
                                || configuration.transmit.running()
                                || (configuration.initial.origin().is_none()
                                    != configuration.transmit.stopped())
                            {
                                return Err(fail(BearerCipherError::Layout));
                            }
                            if !new_domains[slot] {
                                candidate.transparent[slot] = None;
                                new_domains[slot] = true;
                            }
                        }
                        _ => {
                            // TS 25.331 8.6.4.3 initializes only the added AM/UM RLC instance from START.
                            for anchor in [configuration.transmit, configuration.receive] {
                                let Position::Sequence(position) = anchor.position() else {
                                    return Err(fail(BearerCipherError::Layout));
                                };
                                let count = anchor
                                    .count_at(anchor.position())
                                    .map_err(|error| fail(error.into()))?;
                                if count & super::count::AM_SEQUENCE_MASK != 0
                                    || (position != 0 && position != u64::from(count))
                                {
                                    return Err(fail(BearerCipherError::Layout));
                                }
                            }
                            if configuration.transmit != configuration.receive {
                                return Err(fail(BearerCipherError::Layout));
                            }
                        }
                    }
                    self.register_candidate(&mut candidate, configuration)
                        .map_err(fail)?;
                }
                AddedBearerCipherConfiguration::ExistingTransparent(configuration) => {
                    let slot = bearer_slot(configuration.bearer).map_err(fail)?;
                    if candidate.index[slot].is_some() {
                        return Err(fail(BearerCipherError::Duplicate));
                    }
                    if candidate.channels.len() == self.capacity.get() {
                        return Err(fail(BearerCipherError::Capacity));
                    }
                    if !existing_domains[domain_index(configuration.domain)] {
                        return Err(fail(BearerCipherError::Layout));
                    }
                    super::timeline::validate_bearer(
                        Scope::User(configuration.domain),
                        configuration.bearer,
                    )
                    .map_err(|error| fail(error.into()))?;
                    let existing = candidate.transparent[domain_index(configuration.domain)]
                        .as_ref()
                        .ok_or(fail(BearerCipherError::Layout))?;
                    let selected = existing
                        .selected(key, configuration.first_frame)
                        .map_err(|error| fail(error.into()))?;
                    if !matches!(selected, CipherUse::Data { origin, .. } if origin == configuration.origin)
                    {
                        return Err(fail(Error::Owner.into()));
                    }
                    // TS 33.102 6.6.4.1 keeps the original domain timeline shared by both directions and all TM RBs.
                    candidate.index[slot] = Some(candidate.channels.len());
                    candidate.channels.push(Channel::Transparent {
                        domain: configuration.domain,
                        first_frame: configuration.first_frame,
                    });
                }
            }
        }
        if candidate.channels.len() == original_length {
            return Err(fail(BearerCipherError::Layout));
        }
        let mut seen = [false; DOMAINS];
        for counter in counters {
            let slot = domain_index(counter.domain);
            if std::mem::replace(&mut seen[slot], true) {
                return Err(fail(BearerCipherError::Duplicate));
            }
            if !new_domains[slot] {
                return Err(fail(BearerCipherError::Layout));
            }
            candidate.transparent[slot]
                .as_mut()
                .ok_or(fail(BearerCipherError::Layout))?
                .activate_counter_with(
                    key,
                    counter.origin,
                    counter.anchor,
                    counter.first_frame,
                    || Ok::<_, std::convert::Infallible>(()),
                )
                .map_err(|error| fail(update_error(error)))?;
        }
        if new_domains != seen {
            return Err(fail(BearerCipherError::Layout));
        }
        self.budget(&candidate).map_err(fail)?;
        let result = publish().map_err(BearerCipherUpdateError::Publication)?;
        self.tables = candidate;
        Ok(result)
    }

    fn register_candidate(
        &self,
        candidate: &mut Tables,
        configuration: BearerCipherConfiguration,
    ) -> Result<(), BearerCipherError> {
        let slot = bearer_slot(configuration.bearer)?;
        if candidate.index[slot].is_some() {
            return Err(BearerCipherError::Duplicate);
        }
        if candidate.channels.len() == self.capacity.get() {
            return Err(BearerCipherError::Capacity);
        }
        if configuration.transmit.mode() != configuration.receive.mode() {
            return Err(BearerCipherError::Layout);
        }
        let channel = match configuration.transmit.mode() {
            CipherMode::Transparent => {
                let Scope::User(domain) = configuration.scope else {
                    return Err(BearerCipherError::Layout);
                };
                super::timeline::validate_bearer(configuration.scope, configuration.bearer)?;
                if configuration.transmit != configuration.receive {
                    return Err(BearerCipherError::Layout);
                }
                let frame = configuration
                    .transmit
                    .position()
                    .get(CipherMode::Transparent)?;
                let target = &mut candidate.transparent[domain_index(domain)];
                if let Some(existing) = target {
                    if !existing.matches_anchor(
                        self.ue,
                        configuration.transmit,
                        configuration.initial.origin(),
                    )? {
                        return Err(BearerCipherError::Layout);
                    }
                } else {
                    *target = Some(TransparentDomain::new(
                        self.ue,
                        domain,
                        configuration.transmit,
                        configuration.initial,
                    )?);
                }
                Channel::Transparent {
                    domain,
                    first_frame: frame,
                }
            }
            mode => Channel::Sequenced(Box::new(SequenceCiphers {
                transmit: CipherTimeline::new(
                    Binding::new(
                        self.ue,
                        configuration.scope,
                        configuration.bearer,
                        mode,
                        Direction::Downlink,
                    )?,
                    configuration.transmit,
                    configuration.initial.clone(),
                )?,
                receive: CipherTimeline::new(
                    Binding::new(
                        self.ue,
                        configuration.scope,
                        configuration.bearer,
                        mode,
                        Direction::Uplink,
                    )?,
                    configuration.receive,
                    configuration.initial,
                )?,
            })),
        };
        candidate.index[slot] = Some(candidate.channels.len());
        candidate.channels.push(channel);
        Ok(())
    }

    pub fn activate_transparent_counter_with<T, E>(
        &mut self,
        key: UeKey,
        domain: CnDomain,
        origin: Option<KeyOrigin>,
        anchor: CountAnchor,
        first_frame: u64,
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, BearerCipherUpdateError<E>> {
        let fail = BearerCipherUpdateError::Cipher;
        self.owner(key).map_err(fail)?;
        self.tables.transparent[domain_index(domain)]
            .as_mut()
            .ok_or(fail(BearerCipherError::Layout))?
            .activate_counter_with(key, origin, anchor, first_frame, publish)
            .map_err(|error| match error {
                UpdateError::Cipher(error) => fail(error.into()),
                UpdateError::Publication(error) => BearerCipherUpdateError::Publication(error),
            })
    }
    pub fn update_with<T, E>(
        &mut self,
        context: CipherContext,
        activations: &[CipherActivation],
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, BearerCipherUpdateError<E>> {
        let fail = BearerCipherUpdateError::Cipher;
        if context.origin().ue != self.ue {
            return Err(fail(Error::Owner.into()));
        }
        if activations.is_empty() || activations.len() > 2 * MAX_BEARERS + DOMAINS {
            return Err(fail(BearerCipherError::Layout));
        }
        let mut candidate = self.staged();
        let mut seen = [0; MAX_BEARERS];
        let mut domains = [false; DOMAINS];
        for activation in activations {
            match *activation {
                CipherActivation::Sequenced {
                    bearer,
                    direction,
                    anchor,
                    first_new,
                } => {
                    admit_direction(&mut seen, bearer, direction).map_err(fail)?;
                    let channel = channel_mut(&mut candidate, bearer).map_err(fail)?;
                    let timeline = sequenced_mut(channel, direction).map_err(fail)?;
                    timeline
                        .schedule_with(context.clone(), anchor, first_new, || {
                            Ok::<_, std::convert::Infallible>(())
                        })
                        .map_err(|error| fail(update_error(error)))?;
                }
                CipherActivation::Transparent {
                    domain,
                    anchor,
                    first_frame,
                } => {
                    let slot = domain_index(domain);
                    if std::mem::replace(&mut domains[slot], true) {
                        return Err(fail(BearerCipherError::Duplicate));
                    }
                    candidate.transparent[slot]
                        .as_mut()
                        .ok_or(fail(BearerCipherError::Layout))?
                        .schedule_with(context.clone(), anchor, first_frame, || {
                            Ok::<_, std::convert::Infallible>(())
                        })
                        .map_err(|error| fail(update_error(error)))?;
                }
            }
        }
        self.budget(&candidate).map_err(fail)?;
        let result = publish().map_err(BearerCipherUpdateError::Publication)?;
        self.tables = candidate;
        Ok(result)
    }
    pub fn reset_with<T, E>(
        &mut self,
        key: UeKey,
        reset: CipherReset,
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, BearerCipherUpdateError<E>> {
        let fail = BearerCipherUpdateError::Cipher;
        self.owner(key).map_err(fail)?;
        let mut candidate = self.staged();
        let channel = channel_mut(&mut candidate, reset.bearer).map_err(fail)?;
        for (direction, anchor) in [
            (Direction::Downlink, reset.transmit),
            (Direction::Uplink, reset.receive),
        ] {
            sequenced_mut(channel, direction)
                .map_err(fail)?
                .reset_with(key, reset.origin, anchor, || {
                    Ok::<_, std::convert::Infallible>(())
                })
                .map_err(|error| fail(update_error(error)))?;
        }
        self.budget(&candidate).map_err(fail)?;
        let result = publish().map_err(BearerCipherUpdateError::Publication)?;
        self.tables = candidate;
        Ok(result)
    }
    /// Floors must come from settled RLC windows or shared TM frame retention, never from the latest received packet.
    pub fn retire(
        &mut self,
        key: UeKey,
        floors: &[CipherRetirement],
    ) -> Result<(), BearerCipherError> {
        self.owner(key)?;
        if floors.len() > 2 * MAX_BEARERS + DOMAINS {
            return Err(BearerCipherError::Layout);
        }
        let mut candidate = self.staged();
        let mut seen = [0; MAX_BEARERS];
        let mut domains = [false; DOMAINS];
        for floor in floors {
            match *floor {
                CipherRetirement::Sequenced {
                    bearer,
                    direction,
                    floor,
                } => {
                    admit_direction(&mut seen, bearer, direction)?;
                    sequenced_mut(channel_mut(&mut candidate, bearer)?, direction)?
                        .retire_before(key, floor)?;
                }
                CipherRetirement::Transparent {
                    domain,
                    first_frame,
                } => {
                    let slot = domain_index(domain);
                    if std::mem::replace(&mut domains[slot], true) {
                        return Err(BearerCipherError::Duplicate);
                    }
                    candidate.transparent[slot]
                        .as_mut()
                        .ok_or(BearerCipherError::Layout)?
                        .retire_before(key, first_frame)?;
                }
            }
        }
        self.tables = candidate;
        Ok(())
    }
    pub fn selected(
        &self,
        key: UeKey,
        bearer: u8,
        direction: Direction,
        position: Position,
    ) -> Result<CipherUse, BearerCipherError> {
        self.owner(key)?;
        match channel(&self.tables, bearer)? {
            Channel::Sequenced(ciphers) => match direction {
                Direction::Downlink => &ciphers.transmit,
                Direction::Uplink => &ciphers.receive,
            }
            .selected(key, position)
            .map_err(Into::into),
            Channel::Transparent {
                domain,
                first_frame,
            } => self.tables.transparent[domain_index(*domain)]
                .as_ref()
                .ok_or(BearerCipherError::Layout)?
                .selected(key, transparent_frame(position, *first_frame)?)
                .map_err(Into::into),
        }
    }
    pub fn apply(
        &self,
        key: UeKey,
        bearer: u8,
        direction: Direction,
        position: Position,
        bytes: &mut [u8],
        bits: usize,
    ) -> Result<CipherUse, BearerCipherError> {
        self.owner(key)?;
        match channel(&self.tables, bearer)? {
            Channel::Sequenced(ciphers) => match direction {
                Direction::Downlink => &ciphers.transmit,
                Direction::Uplink => &ciphers.receive,
            }
            .apply(key, position, bytes, bits)
            .map_err(Into::into),
            Channel::Transparent {
                domain,
                first_frame,
            } => self.tables.transparent[domain_index(*domain)]
                .as_ref()
                .ok_or(BearerCipherError::Layout)?
                .apply(
                    key,
                    bearer,
                    direction,
                    transparent_frame(position, *first_frame)?,
                    bytes,
                    bits,
                )
                .map_err(Into::into),
        }
    }
    fn owner(&self, key: UeKey) -> Result<(), BearerCipherError> {
        if key != self.ue {
            Err(Error::Owner.into())
        } else {
            Ok(())
        }
    }
    fn staged(&self) -> Tables {
        let mut channels = Vec::with_capacity(self.capacity.get());
        channels.extend(self.tables.channels.iter().cloned());
        Tables {
            index: self.tables.index,
            channels,
            transparent: self.tables.transparent.clone(),
        }
    }
    fn budget(&self, tables: &Tables) -> Result<(), BearerCipherError> {
        let mut budget = CipherBudget::new(self.ue);
        for channel in &tables.channels {
            if let Channel::Sequenced(ciphers) = channel {
                for timeline in [&ciphers.transmit, &ciphers.receive] {
                    for origin in timeline.origins() {
                        budget.admit(timeline.binding().scope(), origin)?;
                    }
                }
            }
        }
        for (slot, domain) in tables.transparent.iter().enumerate() {
            if let Some(domain) = domain {
                for origin in domain.origins() {
                    budget.admit(
                        Scope::User(if slot == 0 {
                            CnDomain::Cs
                        } else {
                            CnDomain::Ps
                        }),
                        origin,
                    )?;
                }
            }
        }
        Ok(())
    }
}

fn bearer_slot(bearer: u8) -> Result<usize, BearerCipherError> {
    bearer
        .checked_sub(1)
        .map(usize::from)
        .filter(|slot| *slot < MAX_BEARERS)
        .ok_or(Error::Bearer.into())
}
fn has_transparent_domain(tables: &Tables, domain: CnDomain) -> bool {
    tables.channels.iter().any(|channel| matches!(channel, Channel::Transparent { domain: existing, .. } if *existing == domain))
}
fn channel(tables: &Tables, bearer: u8) -> Result<&Channel, BearerCipherError> {
    tables.index[bearer_slot(bearer)?]
        .and_then(|index| tables.channels.get(index))
        .ok_or(Error::Bearer.into())
}
fn channel_mut(tables: &mut Tables, bearer: u8) -> Result<&mut Channel, BearerCipherError> {
    let index =
        tables.index[bearer_slot(bearer)?].ok_or(BearerCipherError::Cipher(Error::Bearer))?;
    tables.channels.get_mut(index).ok_or(Error::Bearer.into())
}
fn sequenced_mut(
    channel: &mut Channel,
    direction: Direction,
) -> Result<&mut CipherTimeline, BearerCipherError> {
    match channel {
        Channel::Sequenced(ciphers) => Ok(match direction {
            Direction::Downlink => &mut ciphers.transmit,
            Direction::Uplink => &mut ciphers.receive,
        }),
        _ => Err(BearerCipherError::Layout),
    }
}
fn admit_direction(
    seen: &mut [u8; MAX_BEARERS],
    bearer: u8,
    direction: Direction,
) -> Result<(), BearerCipherError> {
    let entry = &mut seen[bearer_slot(bearer)?];
    let flag = match direction {
        Direction::Downlink => TRANSMIT_FLAG,
        Direction::Uplink => RECEIVE_FLAG,
    };
    if *entry & flag != 0 {
        return Err(BearerCipherError::Duplicate);
    }
    *entry |= flag;
    Ok(())
}
fn update_error(error: UpdateError<std::convert::Infallible>) -> BearerCipherError {
    match error {
        UpdateError::Cipher(error) => error.into(),
        UpdateError::Publication(never) => match never {},
    }
}
fn domain_index(domain: CnDomain) -> usize {
    match domain {
        CnDomain::Cs => 0,
        CnDomain::Ps => 1,
    }
}
fn transparent_frame(position: Position, first: u64) -> Result<u64, BearerCipherError> {
    let frame = position.get(CipherMode::Transparent)?;
    if frame < first {
        Err(Error::Position.into())
    } else {
        Ok(frame)
    }
}

#[cfg(test)]
mod tests;
