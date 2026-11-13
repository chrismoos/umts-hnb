// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    num::NonZeroU64,
    time::{Duration, Instant},
};

use umts_iuh::{
    Domain, ranap,
    session::{AssociationId, ControlTime, SecurityRequest},
};
use umts_rrc::{
    establishment::TransactionId,
    integrity::{AcceptedUplink, IntegritySchedule, ScheduleError},
    security::{
        ActivationSequences, BearerLayout, Capabilities, Command, CommandParameters, Completion,
        ExchangeError, ExchangePhase, IntegrityChange, SecurityExchange,
    },
};
#[cfg(test)]
use umts_rrc::{integrity::DownlinkIntegrity, security::AbortReason};
use umts_rrc_codec::EncodedRrc;
use umts_security::{
    Key,
    contexts::{CipherContext, CipherMaterial, IntegrityContext, KeyOrigin, KeySet},
    counters::{CounterInitialization, Start},
    negotiation::{self, Algorithms, CipherAlternative, Constraints, Support},
};
use umts_types::UeKey;

const DOMAIN_COUNT: usize = 2;
const ALGORITHM_COUNT: usize = 16;
// TS 33.102 6.5.5/6.6.5 starts the network budget at reception, not queue admission.
const PROCEDURE_GUARD_MS: u64 = 5000;

#[derive(Clone, Copy)]
pub struct SecurityTime {
    pub control: ControlTime,
    pub monotonic: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    AwaitingPreparation,
    Prepared,
    AwaitingRadio,
    Reporting,
    Retired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Accepted,
    Duplicate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preparation {
    Prepared(KeyOrigin),
    Rejected(ranap::SecurityReject),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error<E> {
    Owner,
    Association,
    Time,
    Phase,
    Capacity,
    Conflict,
    RadioPlan,
    MissingStart,
    Command(umts_rrc::security::Error),
    Exchange(ExchangeError<E>),
    Scheduled(ScheduleError<ExchangeError<E>>),
    Downstream(E),
}

pub struct RadioPlan {
    pub transaction: TransactionId,
    pub layout: BearerLayout,
    pub integrity: IntegrityChange,
    pub cipher: Option<ActivationSequences>,
    pub dpch_cfn: Option<u8>,
}

pub struct KeyContexts {
    cipher: CipherContext,
    integrity: IntegrityContext,
}
impl KeyContexts {
    pub fn origin(&self) -> KeyOrigin {
        self.cipher.origin()
    }
    #[cfg(test)]
    pub fn integrity(&self) -> &IntegrityContext {
        &self.integrity
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct Activation<'a> {
    pub completion: Completion,
    pub keys: &'a KeyContexts,
    pub new_keys: bool,
}

struct Request {
    token: SecurityRequest,
    received_at: ControlTime,
    deadline: ControlTime,
    command: ranap::SecurityCommand,
}
struct Radio {
    request: Request,
    keys: KeyContexts,
    exchange: SecurityExchange,
    published_counters: Option<CounterInitialization>,
}
pub struct CipherPlan {
    pub context: CipherContext,
    pub counters: CounterInitialization,
    pub downlink: ActivationSequences,
    pub dpch_cfn: Option<u8>,
}
struct Report {
    token: SecurityRequest,
    response: ranap::SecurityResponse,
}
enum Procedure {
    Received(Request),
    Radio(Box<Radio>),
    Reporting(Report),
}
struct Active {
    algorithms: Algorithms,
    keys: KeyContexts,
}
struct EstablishedCipher {
    context: CipherContext,
    counters: Option<CounterInitialization>,
}
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct UserCipher {
    pub context: CipherContext,
    // This is the submitted security command's initialization, not a later RB setup's START.
    pub command_counters: Option<CounterInitialization>,
}
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
struct CompletionHold {
    origin: KeyOrigin,
    previous: Option<Active>,
}
#[derive(Default)]
struct DomainState {
    highest_request: u64,
    signalling_active: bool,
    active: Option<Active>,
    cipher: Option<EstablishedCipher>,
    procedure: Option<Procedure>,
}

pub struct ConnectionSecurity {
    key: UeKey,
    allowed: Support,
    fresh: u32,
    association: Option<AssociationId>,
    domains: [DomainState; DOMAIN_COUNT],
    next_generation: Option<NonZeroU64>,
    changed_at: Option<SecurityTime>,
    retired: bool,
    completion: Option<CompletionHold>,
}

impl ConnectionSecurity {
    #[cfg(test)]
    pub fn new(key: UeKey, fresh: u32) -> Self {
        Self::with_algorithms(key, fresh, Support::IMPLEMENTED)
    }
    pub(crate) fn with_algorithms(key: UeKey, fresh: u32, allowed: Support) -> Self {
        Self {
            key,
            allowed,
            fresh,
            association: None,
            domains: std::array::from_fn(|_| DomainState::default()),
            next_generation: Some(NonZeroU64::MIN),
            changed_at: None,
            retired: false,
            completion: None,
        }
    }
    pub fn fresh(&self) -> u32 {
        self.fresh
    }
    pub fn cipher_plan(
        &self,
        domain: Domain,
        start: Option<Start>,
    ) -> Result<Option<CipherPlan>, Error<()>> {
        if self.retired {
            return Err(Error::Phase);
        }
        let Some(Procedure::Radio(radio)) = self.domains[index(domain)].procedure.as_ref() else {
            return Err(Error::Phase);
        };
        let Some(downlink) = radio.exchange.cipher_activation() else {
            return Ok(None);
        };
        Ok(Some(CipherPlan {
            context: radio.keys.cipher.clone(),
            counters: initialization(radio, start)?,
            downlink,
            dpch_cfn: radio.exchange.cipher_frame(),
        }))
    }
    pub fn buffer_bytes() -> usize {
        const KEYSETS: usize = 2 * DOMAIN_COUNT + 2;
        std::mem::size_of::<Self>()
            + std::mem::size_of::<Radio>()
            + DOMAIN_COUNT * 2 * ALGORITHM_COUNT * std::mem::size_of::<ranap::Algorithm>()
            + KEYSETS
                * (umts_security::contexts::CIPHER_KEY_BYTES
                    + umts_security::contexts::INTEGRITY_KEY_BYTES
                    + 4 * std::mem::size_of::<usize>())
    }
    pub fn phase(&self, domain: Domain) -> Phase {
        if self.retired {
            return Phase::Retired;
        }
        match self.domains[index(domain)].procedure.as_ref() {
            None => Phase::Idle,
            Some(Procedure::Received(_)) => Phase::AwaitingPreparation,
            Some(Procedure::Reporting(_)) => Phase::Reporting,
            Some(Procedure::Radio(radio)) => match radio.exchange.phase() {
                ExchangePhase::Prepared => Phase::Prepared,
                ExchangePhase::AwaitingResponse => Phase::AwaitingRadio,
                _ => unreachable!("terminal radio exchanges are replaced by a reserved report"),
            },
        }
    }
    pub fn active_origin(&self, domain: Domain) -> Option<KeyOrigin> {
        self.domains[index(domain)]
            .active
            .as_ref()
            .map(|active| active.keys.origin())
    }
    pub(crate) fn release_signalling(&mut self, domain: Domain) -> Result<(), Error<()>> {
        if self.phase(domain) != Phase::Idle {
            return Err(Error::Phase);
        }
        // TS 25.413 8.18.4 constrains an existing Iu connection, not retained radio keys.
        self.domains[index(domain)].signalling_active = false;
        Ok(())
    }
    // An integrity-only command does not replace the domain's user-plane cipher key.
    pub fn user_cipher(&self, domain: Domain) -> Result<Option<UserCipher>, Error<()>> {
        if self.retired
            || self
                .domains
                .iter()
                .any(|state| matches!(state.procedure, Some(Procedure::Radio(_))))
        {
            return Err(Error::Phase);
        }
        Ok(self.domains[index(domain)]
            .cipher
            .as_ref()
            .map(|cipher| UserCipher {
                context: cipher.context.clone(),
                command_counters: cipher.counters,
            }))
    }
    pub fn unconfirmed_origin(&self) -> Option<KeyOrigin> {
        self.completion.as_ref().map(|hold| hold.origin)
    }
    #[cfg(test)]
    pub fn previous_completion_origin(&self) -> Option<KeyOrigin> {
        self.completion
            .as_ref()?
            .previous
            .as_ref()
            .map(|active| active.keys.origin())
    }
    pub fn confirm_completion_with<T, E>(
        &mut self,
        origin: KeyOrigin,
        confirm: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, Error<E>> {
        if origin.ue != self.key {
            return Err(Error::Owner);
        }
        if self.retired || self.unconfirmed_origin() != Some(origin) {
            return Err(Error::Phase);
        }
        let result = confirm().map_err(Error::Downstream)?;
        self.completion = None;
        Ok(result)
    }
    pub fn pending_origin(&self, domain: Domain) -> Option<KeyOrigin> {
        match self.domains[index(domain)].procedure.as_ref()? {
            Procedure::Radio(radio) => Some(radio.keys.origin()),
            _ => None,
        }
    }
    pub fn transaction(&self, domain: Domain) -> Option<TransactionId> {
        match self.domains[index(domain)].procedure.as_ref()? {
            Procedure::Radio(radio) => Some(radio.exchange.transaction()),
            _ => None,
        }
    }
    pub fn can_prepare(&self, domain: Domain) -> bool {
        self.phase(domain) == Phase::AwaitingPreparation
            && self.completion.is_none()
            && self.domains.iter().enumerate().all(|(other, state)| {
                other == index(domain) || !matches!(state.procedure, Some(Procedure::Radio(_)))
            })
    }

    // Only an unsubmitted command can be discarded. The original core request and deadline remain unchanged.
    pub(crate) fn discard_prepared(
        &mut self,
        domain: Domain,
        origin: KeyOrigin,
    ) -> Result<(), Error<()>> {
        if self.phase(domain) != Phase::Prepared || self.pending_origin(domain) != Some(origin) {
            return Err(Error::Phase);
        }
        let Some(Procedure::Radio(radio)) = self.domains[index(domain)].procedure.take() else {
            unreachable!("validated prepared command")
        };
        self.domains[index(domain)].procedure = Some(Procedure::Received(radio.request));
        Ok(())
    }
    #[cfg(test)]
    pub fn keys(&self, origin: KeyOrigin) -> Option<&KeyContexts> {
        if origin.ue != self.key {
            return None;
        }
        let domain = &self.domains[index(origin.domain.into())];
        if let Some(active) = &domain.active
            && active.keys.origin() == origin
        {
            return Some(&active.keys);
        }
        if let Some(previous) = self
            .completion
            .as_ref()
            .and_then(|hold| hold.previous.as_ref())
            && previous.keys.origin() == origin
        {
            return Some(&previous.keys);
        }
        match domain.procedure.as_ref()? {
            Procedure::Radio(radio) if radio.keys.origin() == origin => Some(&radio.keys),
            _ => None,
        }
    }
    pub fn receive(
        &mut self,
        now: SecurityTime,
        received_at: ControlTime,
        token: SecurityRequest,
        command: &ranap::SecurityCommand,
    ) -> Result<Admission, Error<()>> {
        self.time(now)?;
        if token.key() != self.key {
            return Err(Error::Owner);
        }
        if self
            .association
            .is_some_and(|association| association != token.association())
        {
            return Err(Error::Association);
        }
        if received_at > now.control {
            return Err(Error::Time);
        }
        let deadline = ControlTime(
            received_at
                .0
                .checked_add(PROCEDURE_GUARD_MS)
                .ok_or(Error::Time)?,
        );
        let domain = &mut self.domains[index(token.domain())];
        if let Some(procedure) = &domain.procedure {
            return match procedure {
                Procedure::Received(request)
                    if request.token == token
                        && request.received_at == received_at
                        && request.command == *command =>
                {
                    Ok(Admission::Duplicate)
                }
                Procedure::Radio(radio)
                    if radio.request.token == token
                        && radio.request.received_at == received_at
                        && radio.request.command == *command =>
                {
                    Ok(Admission::Duplicate)
                }
                _ => Err(Error::Conflict),
            };
        }
        if token.serial().get() <= domain.highest_request {
            return Err(Error::Owner);
        }
        domain.procedure = Some(if now.control >= deadline {
            Procedure::Reporting(Report {
                token,
                response: ranap::SecurityResponse::Reject(
                    ranap::SecurityReject::RadioProcedureFailure,
                ),
            })
        } else {
            Procedure::Received(Request {
                token,
                received_at,
                deadline,
                command: command.clone(),
            })
        });
        domain.highest_request = token.serial().get();
        self.association = Some(token.association());
        self.changed_at = Some(now);
        Ok(Admission::Accepted)
    }
    #[cfg(test)]
    pub fn prepare(
        &mut self,
        domain: Domain,
        now: SecurityTime,
        capabilities: &Capabilities,
        plan: RadioPlan,
    ) -> Result<Preparation, Error<()>> {
        self.prepare_using(domain, now, capabilities, |_, _| Ok(plan))
    }
    pub fn prepare_using<E>(
        &mut self,
        domain: Domain,
        now: SecurityTime,
        capabilities: &Capabilities,
        build: impl FnOnce(KeyOrigin, negotiation::Selection) -> Result<RadioPlan, E>,
    ) -> Result<Preparation, Error<E>> {
        self.time(now)?;
        let slot = index(domain);
        if !self.can_prepare(domain) {
            return Err(Error::Phase);
        }
        let Some(Procedure::Received(request)) = self.domains[slot].procedure.as_ref() else {
            return Err(Error::Phase);
        };
        if now.control >= request.deadline {
            tracing::warn!(
                "Security mode command for {domain:?} expired before preparation (deadline {:?}, now {:?})",
                request.deadline,
                now.control
            );
            return self.reject_received(domain, now, ranap::SecurityReject::RadioProcedureFailure);
        }
        let mut integrity = [0u8; ALGORITHM_COUNT];
        let mut cipher = [0u8; ALGORITHM_COUNT];
        let command = &request.command;
        if command.integrity.algorithms.len() > ALGORITHM_COUNT
            || command
                .encryption
                .as_ref()
                .is_some_and(|info| info.algorithms.len() > ALGORITHM_COUNT)
        {
            return self.reject_received(domain, now, ranap::SecurityReject::AlgorithmsUnsupported);
        }
        for (target, algorithm) in integrity.iter_mut().zip(&command.integrity.algorithms) {
            *target = algorithm.0;
        }
        let cipher = command.encryption.as_ref().map(|info| {
            for (target, algorithm) in cipher.iter_mut().zip(&info.algorithms) {
                *target = algorithm.0;
            }
            &cipher[..info.algorithms.len()]
        });
        let selection = match negotiation::select(
            negotiation::Request {
                integrity: &integrity[..command.integrity.algorithms.len()],
                cipher,
                keys: match command.key_status {
                    ranap::KeyStatus::Old => negotiation::KeyStatus::Old,
                    ranap::KeyStatus::New => negotiation::KeyStatus::New,
                    _ => return Err(Error::Conflict),
                },
            },
            Some(capabilities.support()),
            self.allowed,
            Constraints {
                same_domain: self.domains[slot]
                    .active
                    .as_ref()
                    .filter(|_| self.domains[slot].signalling_active)
                    .map(|active| active.algorithms),
                other_domain: self.domains[1 - slot]
                    .active
                    .as_ref()
                    .filter(|_| self.domains[1 - slot].signalling_active)
                    .map(|active| active.algorithms),
            },
        ) {
            Ok(selection) => selection,
            Err(error) => {
                tracing::warn!(
                    "Security negotiation rejected: slot={} generation={} domain={domain:?} error={error:?} core_integrity={:?} core_cipher={cipher:?} ue={:?} allowed={:?}",
                    self.key.slot,
                    self.key.generation,
                    &integrity[..command.integrity.algorithms.len()],
                    capabilities.support(),
                    self.allowed,
                );
                return self.reject_received(
                    domain,
                    now,
                    match error {
                        negotiation::Error::Conflict => ranap::SecurityReject::Conflict,
                        _ => ranap::SecurityReject::AlgorithmsUnsupported,
                    },
                );
            }
        };
        let generation = self.next_generation.ok_or(Error::Capacity)?;
        let origin = KeyOrigin {
            ue: self.key,
            domain: domain.into(),
            generation,
        };
        let plan = build(origin, selection).map_err(Error::Downstream)?;
        let already_started = self.domains.iter().any(|state| state.active.is_some());
        match (already_started, plan.integrity) {
            (false, IntegrityChange::Start { fresh }) if fresh == self.fresh => (),
            (true, IntegrityChange::Modify(_)) => (),
            _ => return Err(Error::RadioPlan),
        }
        let cipher = match selection.algorithms.cipher {
            CipherAlternative::Clear => CipherMaterial::Clear,
            CipherAlternative::Uea1 => CipherMaterial::Uea1(Key::new(
                *command
                    .encryption
                    .as_ref()
                    .ok_or(Error::RadioPlan)?
                    .key
                    .bytes(),
            )),
            CipherAlternative::Uea2 => CipherMaterial::Uea2(Key::new(
                *command
                    .encryption
                    .as_ref()
                    .ok_or(Error::RadioPlan)?
                    .key
                    .bytes(),
            )),
        };
        let (cipher, integrity) = KeySet::new(
            origin,
            cipher,
            Key::new(*command.integrity.key.bytes()),
            selection.algorithms.integrity,
        )
        .split();
        let command = Command::new(
            CommandParameters {
                origin,
                transaction: plan.transaction,
                layout: plan.layout,
                selection,
                integrity: plan.integrity,
                cipher: plan.cipher,
                dpch_cfn: plan.dpch_cfn,
            },
            capabilities,
        )
        .map_err(Error::Command)?;
        let remaining = Duration::from_millis(request.deadline.0 - now.control.0);
        let exchange = SecurityExchange::prepare(command, now.monotonic, remaining)
            .map_err(|_| Error::Exchange(ExchangeError::Time))?;
        let Some(Procedure::Received(request)) = self.domains[slot].procedure.take() else {
            unreachable!("validated received request");
        };
        self.domains[slot].procedure = Some(Procedure::Radio(Box::new(Radio {
            request,
            keys: KeyContexts { cipher, integrity },
            exchange,
            published_counters: None,
        })));
        self.next_generation = generation.get().checked_add(1).and_then(NonZeroU64::new);
        self.changed_at = Some(now);
        Ok(Preparation::Prepared(origin))
    }
    fn reject_received<E>(
        &mut self,
        domain: Domain,
        now: SecurityTime,
        cause: ranap::SecurityReject,
    ) -> Result<Preparation, Error<E>> {
        let state = &mut self.domains[index(domain)];
        let Some(Procedure::Received(request)) = state.procedure.as_ref() else {
            return Err(Error::Phase);
        };
        state.procedure = Some(Procedure::Reporting(Report {
            token: request.token,
            response: ranap::SecurityResponse::Reject(cause),
        }));
        self.changed_at = Some(now);
        Ok(Preparation::Rejected(cause))
    }
    #[cfg(test)]
    pub fn publish_with<T, E>(
        &mut self,
        domain: Domain,
        now: SecurityTime,
        integrity: &mut DownlinkIntegrity,
        publish_old_cipher: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<T, Error<E>> {
        self.time(now)?;
        let Some(Procedure::Radio(radio)) = self.domains[index(domain)].procedure.as_mut() else {
            return Err(Error::Phase);
        };
        if now.control >= radio.request.deadline {
            return Err(Error::Time);
        }
        let result = radio
            .exchange
            .publish_with(now.monotonic, integrity, publish_old_cipher)
            .map_err(Error::Exchange)?;
        radio.published_counters = initialization::<()>(radio, None).ok();
        self.changed_at = Some(now);
        Ok(result)
    }
    pub fn publish_scheduled_with<T, E>(
        &mut self,
        domain: Domain,
        now: SecurityTime,
        start: Option<Start>,
        schedule: &mut IntegritySchedule,
        publish_old_cipher: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<T, Error<E>> {
        self.time(now)?;
        let Some(Procedure::Radio(radio)) = self.domains[index(domain)].procedure.as_mut() else {
            return Err(Error::Phase);
        };
        if now.control >= radio.request.deadline {
            return Err(Error::Time);
        }
        let counters = initialization(radio, start)?;
        let result = radio
            .exchange
            .publish_scheduled_with(
                now.monotonic,
                radio.keys.integrity.clone(),
                counters,
                schedule,
                publish_old_cipher,
            )
            .map_err(Error::Scheduled)?;
        radio.published_counters = Some(counters);
        self.changed_at = Some(now);
        Ok(result)
    }
    pub fn complete_with<T, E>(
        &mut self,
        domain: Domain,
        now: SecurityTime,
        accepted: AcceptedUplink<'_>,
        activate: impl FnOnce(Activation<'_>) -> Result<T, E>,
    ) -> Result<T, Error<E>> {
        self.time(now)?;
        let state = &mut self.domains[index(domain)];
        let Some(Procedure::Radio(radio)) = state.procedure.as_mut() else {
            return Err(Error::Phase);
        };
        if now.control >= radio.request.deadline {
            return Err(Error::Time);
        }
        let keys = &radio.keys;
        let new_keys = radio.request.command.key_status == ranap::KeyStatus::New;
        let result = radio
            .exchange
            .complete_with(now.monotonic, accepted, |completion| {
                activate(Activation {
                    completion: *completion,
                    keys,
                    new_keys,
                })
            })
            .map_err(Error::Exchange)?;
        let umts_rrc::security::CompletionResult::Activated(result) = result else {
            return Err(Error::Phase);
        };
        let Some(Procedure::Radio(radio)) = state.procedure.take() else {
            unreachable!("validated radio procedure");
        };
        let selection = radio.exchange.selection();
        tracing::info!(
            "Security mode complete: slot={} generation={} domain={domain:?} integrity={:?} cipher={:?}",
            self.key.slot,
            self.key.generation,
            selection.algorithms.integrity,
            selection.algorithms.cipher
        );
        let response = ranap::SecurityResponse::Complete {
            integrity: ranap::Algorithm(selection.algorithms.integrity.ranap()),
            encryption: selection
                .chosen_encryption()
                .map(|algorithm| ranap::Algorithm(algorithm.ranap())),
        };
        let origin = radio.keys.origin();
        let mut algorithms = selection.algorithms;
        if selection.chosen_encryption().is_some() {
            state.cipher = Some(EstablishedCipher {
                context: radio.keys.cipher.clone(),
                counters: radio.published_counters,
            });
        } else if let Some(previous) = &state.active {
            algorithms.cipher = previous.algorithms.cipher;
        }
        let previous = state.active.replace(Active {
            algorithms,
            keys: radio.keys,
        });
        state.signalling_active = true;
        self.completion = Some(CompletionHold { origin, previous });
        state.procedure = Some(Procedure::Reporting(Report {
            token: radio.request.token,
            response,
        }));
        self.changed_at = Some(now);
        Ok(result)
    }
    #[cfg(test)]
    pub fn abort_with<T, E>(
        &mut self,
        domain: Domain,
        now: SecurityTime,
        reason: AbortReason,
        restore: impl FnOnce(KeyOrigin, AbortReason) -> Result<T, E>,
    ) -> Result<T, Error<E>> {
        self.time(now)?;
        let state = &mut self.domains[index(domain)];
        let Some(Procedure::Radio(radio)) = state.procedure.as_mut() else {
            return Err(Error::Phase);
        };
        let result = radio
            .exchange
            .abort_with(now.monotonic, reason, restore)
            .map_err(Error::Exchange)?;
        let token = radio.request.token;
        state.procedure = Some(Procedure::Reporting(Report {
            token,
            response: ranap::SecurityResponse::Reject(ranap::SecurityReject::RadioProcedureFailure),
        }));
        self.changed_at = Some(now);
        Ok(result)
    }
    pub fn timeout_action(
        &mut self,
        domain: Domain,
        now: SecurityTime,
    ) -> Result<Option<KeyOrigin>, Error<()>> {
        self.time(now)?;
        match self.domains[index(domain)].procedure.as_ref() {
            Some(Procedure::Received(request)) if now.control >= request.deadline => {
                tracing::warn!(
                    "Security mode command for {domain:?} expired while awaiting preparation (deadline {:?}, now {:?})",
                    request.deadline,
                    now.control
                );
                self.reject_received(domain, now, ranap::SecurityReject::RadioProcedureFailure)?;
                Ok(None)
            }
            Some(Procedure::Radio(radio))
                if now.control >= radio.request.deadline
                    || now.monotonic >= radio.exchange.deadline() =>
            {
                Ok(Some(radio.keys.origin()))
            }
            _ => Ok(None),
        }
    }
    pub fn publish_report_with<T, E>(
        &mut self,
        domain: Domain,
        now: SecurityTime,
        publish: impl FnOnce(SecurityRequest, ranap::SecurityResponse) -> Result<T, E>,
    ) -> Result<Option<T>, Error<E>> {
        self.time(now)?;
        let state = &mut self.domains[index(domain)];
        let Some(Procedure::Reporting(report)) = state.procedure.as_ref() else {
            return Ok(None);
        };
        let result = publish(report.token, report.response).map_err(Error::Downstream)?;
        state.procedure = None;
        self.changed_at = Some(now);
        Ok(Some(result))
    }
    pub fn retire(&mut self) {
        self.retired = true;
    }
    fn time<E>(&self, now: SecurityTime) -> Result<(), Error<E>> {
        if self.retired {
            return Err(Error::Phase);
        }
        if self.changed_at.is_some_and(|previous| {
            now.control < previous.control || now.monotonic < previous.monotonic
        }) {
            return Err(Error::Time);
        }
        Ok(())
    }
}

fn initialization<E>(
    radio: &Radio,
    start: Option<Start>,
) -> Result<CounterInitialization, Error<E>> {
    match radio.request.command.key_status {
        ranap::KeyStatus::New => Ok(CounterInitialization::NewKeys),
        ranap::KeyStatus::Old => Ok(CounterInitialization::ReusedKeys(
            start.ok_or(Error::MissingStart)?,
        )),
        _ => Err(Error::Conflict),
    }
}
fn index(domain: Domain) -> usize {
    match domain {
        Domain::Cs => 0,
        Domain::Ps => 1,
    }
}

#[cfg(test)]
mod tests;
