// SPDX-License-Identifier: AGPL-3.0-or-later

mod capability_request;
mod control;
mod delivery;
mod diagnostics;
mod ingress;
mod lifecycle;
mod reports;
pub(crate) use lifecycle::IdleBlocker;
mod mobility;
mod nas_delivery;
mod paging;
mod planning;
mod protection;
mod queues;
mod registration;
pub(crate) mod scheduling;
mod security_driver;
mod signalling_release;
mod transactions;
mod transport;
pub use capability_request::CapabilityReadiness;
pub use control::{
    AcceptedActivation, AcceptedAdditionalSetup, AcceptedBearerRelease, AcceptedHsdpaActivation,
    AcceptedHsupaActivation, AcceptedUserSetup, AdditionalSetupCommitError,
    BearerReleaseCommitError, ControlMessageKind, ControlProtection, ControlSecurity,
    HspaCommitError, HspaConfiguration, ReceivedControl,
};
pub use diagnostics::{SubscriberHint, UeOverview};
pub use mobility::{CommonIngressStep, CommonIngressWork, CommonMessageKind};
pub use paging::PagingCounters;
#[cfg(test)]
pub use protection::ControlAdmission;
pub use protection::{BearerProtectionBlock, SecurityDeliveryStatus};
pub use queues::{CoreEventKind, CoreOutbox};
pub use registration::{
    ConnectionReleaseProgress, RegistrationFailure, RegistrationIdentity, RegistrationRequest,
};
pub use reports::Reports;
pub(crate) use reports::{desire_process_allocation, retain_process_allocations};
pub use scheduling::{FachTarget, FachWork};
pub use security_driver::SecurityAction;
pub use transport::ResetBlockers;

#[cfg(test)]
use crate::common_tx::CommonTx;
use crate::{
    common_am::{CommonAmBearer, CommonAmError, CommonAmSettings},
    common_control_tx::{CommonControlDownlink, CommonControlTxError},
    common_transport::FachTransport,
    common_tx::CommonTxTarget,
    common_um::{CommonUmBearer, CommonUmError},
    connection_security::{self, ConnectionSecurity, SecurityTime},
};
use ingress::DedicatedIngress;
use std::{
    collections::HashMap,
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
};
use umts_mac::{CommonHeader, CommonIdentity, CommonPdu, LogicalChannelId};
use umts_rlc::{
    acknowledged::{AmLimits, AmState, PublishError as AmPublishError},
    unacknowledged::{EInterpretation, LiWidth, UmConfig, UmLimits},
};
use umts_rrc::{
    capabilities::{self, AccessRelease},
    establishment::{
        AssignedRntis, CellFachSetup, Establishment, EstablishmentPhase, ProcedureError,
        PublishError, RequestMatch, SignallingProfile, TransactionId,
    },
    nas::{
        CoreConnection, CoreContextId, DownlinkPriority, NasError, NasPublishError, NasRouter,
        SignallingBearer,
        deferred::DeferredNas,
        identification::{self, Identification},
    },
};
use umts_rrc_codec::{CodecError, DecodedUplinkCommon, MAX_DEDICATED_BITS};
use umts_rrc_schema::{
    class_definitions::{ULCCCHMessageType, ULDCCHMessageType},
    information_elements::InitialUEIdentity,
};
mod registry;
mod resources;
use registry::{RegistryInsertError, UeRegistry};
pub use resources::{AdmissionError, Lease, ResourceLedger, Resources};
use umts_types::{
    UeKey,
    bits::{BITS_PER_BYTE, Bits},
    identity::Imsi,
    time::ChipInstant,
};

const FIRST_RADIO_ID: u32 = 1;
const MAX_CELL_RADIO_ID: u32 = u16::MAX as u32;
const FRAME_POOL_CONFIGURATIONS: usize = 2;
const LEASES_PER_CONNECTION: usize = 1 + FRAME_POOL_CONFIGURATIONS;

#[derive(Clone, Copy)]
pub struct SignallingLimits {
    pub maximum_ues: NonZeroU32,
    pub maximum_sdu_bytes: NonZeroUsize,
    pub queued_sdus_per_bearer: NonZeroUsize,
    pub ingress_entries: NonZeroUsize,
    pub core_entries: NonZeroUsize,
    pub response_guard_chips: NonZeroU64,
    pub drx_coefficient: u8,
    pub per_ue_cpu_ns_per_frame: u64,
    /// The cell PLMN when UEs may be registered by TMSI/LAI or P-TMSI/RAI without an IMSI.
    pub tmsi_registration: Option<[u8; 3]>,
    pub cell_resources: Resources,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignallingError {
    Entropy,
    Capacity,
    RegistrationPending,
    Origin,
    Mapping,
    IdentityConflict,
    Message,
    UnsupportedControl(ControlMessageKind),
    Phase,
    ConfigurationFailure,
    CipherResetRequired,
    CipherResetBlocked {
        bearer: SignallingBearer,
        blockers: ResetBlockers,
    },
    Cipher(umts_security::ciphering::BearerCipherError),
    Duplicate,
    Integrity(umts_rrc::integrity::ScheduleFailure),
    SecurityProcedure(umts_rrc::security::Error),
    Admission(AdmissionError),
    Procedure(ProcedureError),
    Common(CommonControlTxError),
    Am(CommonAmError),
    Um(CommonUmError),
    Nas(NasError),
    Codec(CodecError),
    Identification(identification::Error),
    IdentityReleaseRequired(RegistrationFailure),
    Capabilities(capabilities::Error),
    Mobility(umts_rrc::mobility::MobilityError),
    Dedicated(umts_rrc::dedicated::DedicatedError),
    FramePool(crate::dedicated_bearers::FramePoolError),
    UserBearer(crate::dedicated_bearers::BearerError),
    DedicatedControl(crate::dedicated_controller::DedicatedControlError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetupPublicationReadiness {
    pub ingress_pending: bool,
    pub bearer_change_pending: bool,
    pub protection_blocks_reconfiguration: bool,
    pub protection_block: Option<BearerProtectionBlock>,
    pub integrity_pending: bool,
    pub cs_cipher_pending: bool,
    pub ps_cipher_pending: bool,
    pub capabilities_pending: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SignallingCounters {
    pub paging: paging::PagingCounters,
    pub created: u64,
    pub retransmissions: u64,
    pub retired: u64,
    pub processed: u64,
    pub rejected: u64,
    pub backpressure: u64,
    pub capability_timeouts: u64,
    pub confirmed_core_identities: u64,
    pub conflicting_core_identities: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SecurityWork {
    #[default]
    Idle,
    DeliveryRetired {
        key: UeKey,
        origin: umts_security::contexts::KeyOrigin,
    },
    RecoveryRequired {
        key: UeKey,
    },
    NeedCapabilities {
        key: UeKey,
        domain: umts_iuh::Domain,
    },
    ReadyToPrepare {
        key: UeKey,
        domain: umts_iuh::Domain,
    },
    CommandReady {
        key: UeKey,
        domain: umts_iuh::Domain,
    },
    AbortRequired {
        key: UeKey,
        domain: umts_iuh::Domain,
        origin: umts_security::contexts::KeyOrigin,
    },
    ReportQueued {
        key: UeKey,
        domain: umts_iuh::Domain,
    },
    ReportBlocked {
        key: UeKey,
        domain: umts_iuh::Domain,
    },
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IngressWork {
    #[default]
    Idle,
    Collected {
        key: UeKey,
        bearer: SignallingBearer,
    },
    Blocked {
        key: UeKey,
        bearer: SignallingBearer,
    },
}

struct Context {
    reports: Reports,
    transition_reservation: Option<delivery::Reservation>,
    activity: u64,
    idle_release: Option<lifecycle::IdleRelease>,
    release_after_core_disconnect: bool,
    transactions: transactions::Transactions,
    bearer_change: Option<control::BearerChangeAdmission>,
    frame_pools: [Option<NonZeroU64>; FRAME_POOL_CONFIGURATIONS],
    dedicated_route: Option<crate::dedicated_rx::ReceiveIdentity>,
    protection: protection::Protection,
    security: ConnectionSecurity,
    capabilities: capabilities::CapabilityExchange,
    identity: InitialUEIdentity,
    lease: Lease,
    setup: Establishment,
    um: CommonUmBearer,
    control: CommonAmBearer,
    nas_bearer: CommonAmBearer,
    nas_deliveries: nas_delivery::NasDeliveries,
    router: Option<NasRouter>,
    registration: Registration,
    registration_imsi: Option<Imsi>,
    core_imsi: Option<Imsi>,
    identification: Option<Identification>,
    deferred: DeferredNas,
    access_release: AccessRelease,
    supports_csg: bool,
    emergency: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Registration {
    Unregistered,
    Identifying,
    Pending,
    Active(CoreContextId),
    ReleaseRequired(RegistrationFailure),
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct CommonSignalling {
    security_algorithms: umts_security::negotiation::Support,
    pub(crate) request_cpich_measurements: bool,
    pub(crate) secondary_cpich_target: Option<umts_rrc::measurement::SecondaryCpichTarget>,
    pub(crate) measurement_reporting: umts_rrc::measurement::PeriodicalReporting,
    paging: paging::PagingQueue,
    limits: SignallingLimits,
    rnc: u16,
    channel: usize,
    registry: UeRegistry<Context>,
    pending_reset_acks: [Option<umts_iuh::Domain>; 2],
    identities: HashMap<InitialUEIdentity, UeKey>,
    cell_ids: HashMap<u16, UeKey>,
    urntis: HashMap<u32, UeKey>,
    ledger: ResourceLedger,
    shared_bytes: u64,
    rb0: CommonControlDownlink,
    ingress: DedicatedIngress,
    core: CoreOutbox,
    next_radio_id: u32,
    gateway_contexts: HashMap<CoreContextId, UeKey>,
    cursor: u32,
    registration_cursor: u32,
    security_cursor: u64,
    ingress_cursor: u64,
    transmit_cursor: u64,
    timer_cursor: u32,
    counters: SignallingCounters,
}

impl CommonSignalling {
    pub(crate) fn with_bearer_access<T>(
        &mut self,
        key: UeKey,
        apply: impl FnOnce(
            &mut umts_security::ciphering::BearerCiphers,
            crate::dedicated_bearers::ResetPermission,
            &mut transport::SignallingReceiver<'_>,
        ) -> T,
    ) -> Result<T, SignallingError> {
        use crate::dedicated_bearers::ResetPermission;
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        if context.setup.phase() == EstablishmentPhase::Closed {
            return Err(SignallingError::Origin);
        }
        // TS 25.322 11.4 resets one AM entity, not unrelated SRB receipts or partial SDUs.
        let reset = if context.protection.bearer_reconfiguration_block().is_some()
            || context.protection.integrity.pending_origin().is_some()
            || [umts_iuh::Domain::Cs, umts_iuh::Domain::Ps]
                .into_iter()
                .any(|domain| context.security.pending_origin(domain).is_some())
        {
            ResetPermission::Defer
        } else {
            ResetPermission::Settled {
                cs: context.security.active_origin(umts_iuh::Domain::Cs),
                ps: context.security.active_origin(umts_iuh::Domain::Ps),
            }
        };
        let (ciphers, mut receiver) = transport::split_receiver(context, &mut self.ingress, key);
        let result = apply(ciphers, reset, &mut receiver);
        self.refresh_control_status(key);
        Ok(result)
    }
    #[cfg(test)]
    pub fn new(
        source: &dyn CommonTxTarget,
        channel: usize,
        rnc: u16,
        limits: SignallingLimits,
    ) -> Result<Self, SignallingError> {
        Self::with_security_algorithms(
            source,
            channel,
            rnc,
            limits,
            umts_security::negotiation::Support::IMPLEMENTED,
        )
    }
    pub(crate) fn with_security_algorithms(
        source: &dyn CommonTxTarget,
        channel: usize,
        rnc: u16,
        limits: SignallingLimits,
        security_algorithms: umts_security::negotiation::Support,
    ) -> Result<Self, SignallingError> {
        AssignedRntis::new(rnc, 0, 0).map_err(|_| SignallingError::Mapping)?;
        if limits.maximum_sdu_bytes.get() > MAX_DEDICATED_BITS / BITS_PER_BYTE {
            return Err(SignallingError::Capacity);
        }
        let rb0_limits = UmLimits {
            maximum_pdu_bytes: CommonControlDownlink::maximum_pdu_bytes(source, channel)
                .map_err(SignallingError::Common)?,
            maximum_sdu_bytes: limits.maximum_sdu_bytes,
            queued_sdus: limits.queued_sdus_per_bearer,
            maximum_buffer_bytes: usize::MAX,
        };
        let control_capacity = NonZeroUsize::new(umts_rlc::acknowledged::MAX_DELIVERY_SDUS)
            .expect("nonzero AM delivery bound");
        let ingress_bytes = DedicatedIngress::buffer_bytes(
            limits.ingress_entries,
            control_capacity,
            limits.maximum_sdu_bytes,
        )?;
        let core_bytes = limits
            .core_entries
            .get()
            .checked_mul(umts_rrc::nas::MAX_NAS_BYTES)
            .ok_or(SignallingError::Capacity)?;
        let shared = rb0_limits
            .transmit_buffer_bytes()
            .map_err(|_| SignallingError::Capacity)?
            .checked_add(limits.maximum_sdu_bytes.get())
            .and_then(|n| n.checked_add(FachTransport::retained_bytes(source, channel).ok()?))
            .and_then(|n| n.checked_add(ingress_bytes))
            .and_then(|n| n.checked_add(core_bytes))
            .and_then(|n| {
                n.checked_add(paging::PagingQueue::retained_bytes(
                    source,
                    limits.maximum_ues.get() as usize,
                )?)
            })
            .ok_or(SignallingError::Capacity)?;
        let shared_bytes = u64::try_from(shared).map_err(|_| SignallingError::Capacity)?;
        let mut available = limits.cell_resources;
        available.queue_bytes = available
            .queue_bytes
            .checked_sub(shared_bytes)
            .ok_or(SignallingError::Capacity)?;
        let mut identities = HashMap::new();
        let mut cell_ids = HashMap::new();
        let mut urntis = HashMap::new();
        let mut gateway_contexts = HashMap::new();
        identities
            .try_reserve(limits.maximum_ues.get() as usize)
            .map_err(|_| SignallingError::Capacity)?;
        cell_ids
            .try_reserve(limits.maximum_ues.get() as usize)
            .map_err(|_| SignallingError::Capacity)?;
        urntis
            .try_reserve(limits.maximum_ues.get() as usize)
            .map_err(|_| SignallingError::Capacity)?;
        gateway_contexts
            .try_reserve(limits.maximum_ues.get() as usize)
            .map_err(|_| SignallingError::Capacity)?;
        Ok(Self {
            security_algorithms,
            request_cpich_measurements: false,
            secondary_cpich_target: None,
            measurement_reporting: umts_rrc::measurement::PeriodicalReporting::default(),
            paging: paging::PagingQueue::new(source, limits.maximum_ues.get() as usize)?,
            limits,
            rnc,
            channel,
            registry: UeRegistry::new(source.next_chip().epoch, limits.maximum_ues.get()),
            pending_reset_acks: [None; 2],
            identities,
            cell_ids,
            urntis,
            ledger: ResourceLedger::new(
                source.next_chip().epoch,
                available,
                (limits.maximum_ues.get() as usize)
                    .checked_mul(LEASES_PER_CONNECTION)
                    .ok_or(SignallingError::Capacity)?,
            ),
            shared_bytes,
            rb0: CommonControlDownlink::new(source, channel, rb0_limits)
                .map_err(SignallingError::Common)?,
            ingress: DedicatedIngress::new(
                limits.ingress_entries,
                control_capacity,
                limits.maximum_sdu_bytes,
            )?,
            core: CoreOutbox::new(limits.core_entries)?,
            next_radio_id: FIRST_RADIO_ID,
            gateway_contexts,
            cursor: 0,
            registration_cursor: 0,
            security_cursor: 0,
            ingress_cursor: 0,
            transmit_cursor: 0,
            timer_cursor: 0,
            counters: SignallingCounters::default(),
        })
    }
    pub fn counters(&self) -> SignallingCounters {
        self.counters
    }
    #[cfg(test)]
    pub fn rnc(&self) -> u16 {
        self.rnc
    }
    pub fn core(&mut self) -> &mut CoreOutbox {
        &mut self.core
    }
    pub fn capabilities(&self, key: UeKey) -> Option<&capabilities::CapabilityReport> {
        self.registry.get(key)?.capabilities.report()
    }
    #[cfg(test)]
    pub fn security(&self, key: UeKey) -> Option<&ConnectionSecurity> {
        Some(&self.registry.get(key)?.security)
    }
    pub fn security_delivery(
        &self,
        key: UeKey,
    ) -> Result<Option<SecurityDeliveryStatus>, SignallingError> {
        protection::Protection::status(self.registry.get(key).ok_or(SignallingError::Origin)?, key)
    }
    pub fn publish_security_command(
        &mut self,
        key: UeKey,
        domain: umts_iuh::Domain,
        now: SecurityTime,
    ) -> Result<(), SignallingError> {
        if self.ingress.pending(key, SignallingBearer::ControlAm)
            || self
                .registry
                .get(key)
                .is_some_and(|context| context.bearer_change.is_some())
        {
            return Err(SignallingError::Capacity);
        }
        if self.registry.get(key).is_some_and(|context| {
            context.capabilities.phase() == capabilities::ExchangePhase::AwaitingInformation
        }) {
            return Err(SignallingError::Capacity);
        }
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        let transaction = context.security.transaction(domain);
        let result = protection::Protection::publish(context, key, domain, now);
        if result.is_ok() {
            context.transactions.admitted(
                transactions::MessageType::SecurityModeCommand,
                transaction.expect("published security exchange"),
            );
        }
        self.refresh_control_status(key);
        result
    }
    pub fn receive_security_command(
        &mut self,
        now: SecurityTime,
        received_at: umts_iuh::session::ControlTime,
        request: umts_iuh::session::SecurityRequest,
        command: &umts_iuh::ranap::SecurityCommand,
    ) -> Result<(), SignallingError> {
        let context = self
            .registry
            .get_mut(request.key())
            .ok_or(SignallingError::Origin)?;
        if !matches!(context.registration, Registration::Active(id) if id.get() == request.context().get())
            || context
                .router
                .as_ref()
                .and_then(|router| router.connection(request.domain().into()))
                .is_none()
        {
            return Err(SignallingError::Phase);
        }
        context
            .security
            .receive(now, received_at, request, command)
            .map(|_| ())
            .map_err(security_error)
    }
    #[cfg(test)]
    pub fn prepare_security(
        &mut self,
        key: UeKey,
        domain: umts_iuh::Domain,
        now: SecurityTime,
        plan: connection_security::RadioPlan,
    ) -> Result<connection_security::Preparation, SignallingError> {
        let capabilities =
            self.security_capabilities(key)?
                .ok_or(SignallingError::Capabilities(
                    capabilities::Error::MissingRadioCapability,
                ))?;
        self.registry
            .get_mut(key)
            .ok_or(SignallingError::Origin)?
            .security
            .prepare(domain, now, &capabilities, plan)
            .map_err(security_error)
    }
    pub fn prepare_common_security(
        &mut self,
        key: UeKey,
        domain: umts_iuh::Domain,
        now: SecurityTime,
        transaction: TransactionId,
    ) -> Result<connection_security::Preparation, SignallingError> {
        let capabilities =
            self.security_capabilities(key)?
                .ok_or(SignallingError::Capabilities(
                    capabilities::Error::MissingRadioCapability,
                ))?;
        planning::prepare(
            self.registry.get_mut(key).ok_or(SignallingError::Origin)?,
            key,
            domain,
            now,
            transaction,
            &capabilities,
        )
    }
    pub fn publish_security_report(
        &mut self,
        key: UeKey,
        domain: umts_iuh::Domain,
        now: SecurityTime,
    ) -> Result<bool, SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        let core = &mut self.core;
        context
            .security
            .publish_report_with(domain, now, |request, response| {
                core.push_security_response(request, response)
            })
            .map(|value| value.is_some())
            .map_err(|error| match error {
                connection_security::Error::Downstream(error) => error,
                _ => SignallingError::Phase,
            })
    }
    pub fn service_security_one(
        &mut self,
        now: SecurityTime,
    ) -> Result<SecurityWork, SignallingError> {
        let slot = (self.security_cursor / 2) as u32;
        let domain = if self.security_cursor.is_multiple_of(2) {
            umts_iuh::Domain::Cs
        } else {
            umts_iuh::Domain::Ps
        };
        self.security_cursor =
            (self.security_cursor + 1) % (2 * u64::from(self.registry.capacity()));
        let Some((key, _)) = self.registry.at_slot(slot) else {
            return Ok(SecurityWork::Idle);
        };
        let context = self.registry.get_mut(key).expect("occupied security owner");
        if context.security.phase(domain) == connection_security::Phase::Retired {
            return Ok(SecurityWork::Idle);
        }
        if context.protection.blocks_status() {
            return Ok(SecurityWork::RecoveryRequired { key });
        }
        if let Some(origin) = protection::Protection::retire_delivery(context, key)? {
            return Ok(SecurityWork::DeliveryRetired { key, origin });
        }
        if let Some(origin) = context
            .security
            .timeout_action(domain, now)
            .map_err(security_error)?
        {
            return Ok(SecurityWork::AbortRequired {
                key,
                domain,
                origin,
            });
        }
        match context.security.phase(domain) {
            connection_security::Phase::AwaitingPreparation => {
                Ok(if self.security_capabilities(key)?.is_some() {
                    SecurityWork::ReadyToPrepare { key, domain }
                } else {
                    SecurityWork::NeedCapabilities { key, domain }
                })
            }
            connection_security::Phase::Reporting => match self
                .publish_security_report(key, domain, now)
            {
                Ok(true) => Ok(SecurityWork::ReportQueued { key, domain }),
                Err(SignallingError::Capacity) => Ok(SecurityWork::ReportBlocked { key, domain }),
                Ok(false) => Ok(SecurityWork::Idle),
                Err(error) => Err(error),
            },
            connection_security::Phase::Prepared => Ok(SecurityWork::CommandReady { key, domain }),
            _ => Ok(SecurityWork::Idle),
        }
    }
    pub fn security_capabilities(
        &self,
        key: UeKey,
    ) -> Result<Option<umts_rrc::security::Capabilities>, SignallingError> {
        let context = self.registry.get(key).ok_or(SignallingError::Origin)?;
        // Bearer admission needs the full capability report, and after security activation a
        // UE CAPABILITY ENQUIRY must be integrity protected. Security therefore waits for the
        // unprotected enquiry even when RRC CONNECTION SETUP COMPLETE already contained the
        // radio access capability that a capability update requirement asked for.
        Ok(context
            .capabilities
            .report()
            .map(|report| report.security().clone()))
    }
    pub fn request_capabilities(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        transaction: TransactionId,
    ) -> Result<(), SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        if context.bearer_change.is_some() {
            return Err(SignallingError::Capacity);
        }
        if context.setup.phase() != EstablishmentPhase::Established {
            return Err(SignallingError::Phase);
        }
        let control = &mut context.control;
        context
            .capabilities
            .publish_enquiry_with(
                key,
                now,
                transaction,
                self.limits.response_guard_chips,
                |encoded| {
                    control.enqueue(
                        key,
                        Bits::new(&encoded.bytes, encoded.bit_length)
                            .map_err(|_| CommonAmError::Payload)?,
                    )
                },
            )
            .map_err(|error| capability_error(error, am_admission_error))?;
        context
            .transactions
            .admitted(transactions::MessageType::CapabilityEnquiry, transaction);
        Ok(())
    }
    pub fn key_for_cell_id(&self, id: u16) -> Option<UeKey> {
        self.cell_ids.get(&id).copied()
    }
    pub fn key_for_urnti(&self, urnti: u32) -> Option<UeKey> {
        self.urntis.get(&urnti).copied()
    }
    /// Records that a CN RESET for `domain` has been applied and needs RESET ACKNOWLEDGE.
    pub fn request_reset_acknowledge(&mut self, domain: umts_iuh::Domain) {
        let slot = usize::from(domain == umts_iuh::Domain::Ps);
        self.pending_reset_acks[slot] = Some(domain);
    }
    pub fn take_reset_acknowledges(&mut self) -> [Option<umts_iuh::Domain>; 2] {
        std::mem::take(&mut self.pending_reset_acks)
    }
    /// The UE whose dedicated receive route is `identity`, scanned over the registry capacity.
    pub fn key_for_route(&self, identity: crate::dedicated_rx::ReceiveIdentity) -> Option<UeKey> {
        (0..self.registry.capacity()).find_map(|slot| {
            self.registry
                .at_slot(slot)
                .filter(|(_, context)| context.dedicated_route == Some(identity))
                .map(|(key, _)| key)
        })
    }
    pub fn phase(&self, key: UeKey) -> Option<EstablishmentPhase> {
        self.registry.get(key).map(|ctx| ctx.setup.phase())
    }
    pub fn assigned(&self, key: UeKey) -> Option<AssignedRntis> {
        self.registry.get(key).map(|ctx| ctx.setup.assigned())
    }
    pub fn connection(&self, key: UeKey, domain: umts_types::CnDomain) -> Option<CoreConnection> {
        self.registry.get(key)?.router.as_ref()?.connection(domain)
    }
    #[cfg(test)]
    pub fn resources(&self) -> Resources {
        let mut used = self.ledger.used();
        used.queue_bytes += self.shared_bytes;
        used
    }

    pub fn request(
        &mut self,
        source: &dyn CommonTxTarget,
        now: ChipInstant,
        message: &DecodedUplinkCommon,
    ) -> Result<UeKey, SignallingError> {
        self.rb0
            .validate_source(source, now)
            .map_err(SignallingError::Common)?;
        if now.epoch != source.next_chip().epoch {
            return Err(SignallingError::Origin);
        }
        if message.value.integrity_check_info.is_some() {
            return Err(SignallingError::Message);
        }
        let ULCCCHMessageType::rrcConnectionRequest(request) = &message.value.message else {
            return Err(SignallingError::Message);
        };
        let identity = request.initial_ue_identity.clone();
        let subscriber =
            identification::rrc_imsi(&identity).map_err(SignallingError::Identification)?;
        let access_release =
            capabilities::access_release(message).map_err(SignallingError::Capabilities)?;
        let emergency = request.establishment_cause
            == umts_rrc_schema::information_elements::EstablishmentCause::emergencyCall;
        if let Some(key) = self.identities.get(&identity).copied() {
            let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
            // A repeated request with the protocol error indicator reports that the UE could not use
            // the stored setup, so retransmitting or rejecting it only repeats the failure.
            // Retire the context and answer with a fresh setup instead.
            let failed =
                context.setup.classify_request(message) == RequestMatch::ConfigurationFailure;
            if !failed && context.setup.phase() == EstablishmentPhase::AwaitingComplete {
                let rb0 = &mut self.rb0;
                context
                    .setup
                    .retransmit_with(now, |response| {
                        rb0.enqueue(
                            Bits::new(&response.bytes, response.bit_length)
                                .map_err(|_| CommonControlTxError::Payload)?,
                        )
                    })
                    .map_err(setup_error)?;
                self.counters.retransmissions = self.counters.retransmissions.saturating_add(1);
                return Ok(key);
            }
            self.retire(key)?;
        }
        if self.next_radio_id > MAX_CELL_RADIO_ID {
            return Err(SignallingError::Capacity);
        }
        // C-RNTI is not generation-tagged on air. Never reuse it within a radio epoch.
        let assigned = AssignedRntis::new(self.rnc, self.next_radio_id, self.next_radio_id as u16)
            .map_err(|_| SignallingError::Mapping)?;
        let parameters = CellFachSetup {
            assigned,
            transaction: TransactionId::new(0).map_err(|_| SignallingError::Message)?,
            drx_coefficient: self.limits.drx_coefficient,
            signalling: SignallingProfile,
        };
        let (tx, rx) = CommonAmBearer::sizes(source, self.channel).map_err(SignallingError::Am)?;
        let am_config = SignallingProfile.network_am_configuration(tx, rx, 0, 0);
        let am_limits = AmLimits {
            maximum_sdu_bytes: self.limits.maximum_sdu_bytes,
            queued_sdus: self.limits.queued_sdus_per_bearer,
            maximum_buffer_bytes: usize::MAX,
        };
        let um_tx = UmLimits {
            maximum_pdu_bytes: tx,
            maximum_sdu_bytes: self.limits.maximum_sdu_bytes,
            queued_sdus: self.limits.queued_sdus_per_bearer,
            maximum_buffer_bytes: usize::MAX,
        };
        let um_rx = UmLimits {
            maximum_pdu_bytes: rx,
            ..um_tx
        };
        let fach = FachTransport::retained_bytes(source, self.channel)
            .map_err(|_| SignallingError::Capacity)?;
        let am = am_config
            .buffer_bytes(am_limits)
            .map_err(|_| SignallingError::Capacity)?;
        let preparation =
            CommonAmBearer::preparation_bytes(am_config).map_err(SignallingError::Am)?;
        let demand = [
            am,
            am,
            preparation,
            preparation,
            CommonUmBearer::preparation_bytes(um_tx, um_rx).map_err(SignallingError::Um)?,
            ConnectionSecurity::buffer_bytes(),
            nas_delivery::NasDeliveries::buffer_bytes(am_limits.queued_sdus)
                .ok_or(SignallingError::Capacity)?,
            protection::Protection::buffer_bytes(self.limits.maximum_sdu_bytes.get())
                .ok_or(SignallingError::Capacity)?,
            capabilities::CapabilityExchange::buffer_bytes(self.limits.maximum_sdu_bytes)
                .ok_or(SignallingError::Capacity)?,
            um_tx
                .transmit_buffer_bytes()
                .map_err(|_| SignallingError::Capacity)?,
            um_rx
                .receive_buffer_bytes()
                .map_err(|_| SignallingError::Capacity)?,
            fach,
            fach,
            fach,
            rx.get(),
            rx.get(),
            rx.get(),
            self.limits.maximum_sdu_bytes.get(),
            std::mem::size_of::<transactions::Transactions>(),
            self.limits
                .maximum_sdu_bytes
                .get()
                .checked_mul(self.limits.ingress_entries.get())
                .ok_or(SignallingError::Capacity)?,
            self.limits.maximum_sdu_bytes.get(),
            self.limits.maximum_sdu_bytes.get(),
        ]
        .into_iter()
        .try_fold(0usize, |sum, n| sum.checked_add(n))
        .and_then(|n| u64::try_from(n).ok())
        .ok_or(SignallingError::Capacity)?;
        let ledger = &mut self.ledger;
        let rb0 = &mut self.rb0;
        let channel = self.channel;
        let limits = self.limits;
        let key = self
            .registry
            .try_insert_with(|key| {
                let lease = ledger
                    .reserve(
                        key,
                        Resources {
                            queue_bytes: demand,
                            cpu_ns_per_frame: limits.per_ue_cpu_ns_per_frame,
                            ..Resources::default()
                        },
                    )
                    .map_err(SignallingError::Admission)?;
                let built = (|| {
                    let stored_identity = identity.clone();
                    let deferred = DeferredNas::new(
                        key,
                        limits.ingress_entries,
                        NonZeroUsize::new(
                            limits
                                .maximum_sdu_bytes
                                .get()
                                .min(umts_rrc::nas::MAX_NAS_BYTES),
                        )
                        .expect("nonzero NAS limit"),
                    )
                    .map_err(|_| SignallingError::Capacity)?;
                    let mut setup = Establishment::prepare(key, now, message, &parameters)
                        .map_err(SignallingError::Procedure)?;
                    let um = CommonUmBearer::new(
                        source,
                        channel,
                        key,
                        assigned.crnti(),
                        UmConfig {
                            li_width: LiWidth::Seven,
                            interpretation: EInterpretation::Normal,
                        },
                        um_tx,
                        um_rx,
                    )
                    .map_err(SignallingError::Um)?;
                    let mut control = CommonAmBearer::new(
                        source,
                        channel,
                        key,
                        assigned.crnti(),
                        logical(SignallingBearer::ControlAm)?,
                        CommonAmSettings {
                            config: am_config,
                            limits: am_limits,
                            started_at: now,
                        },
                    )
                    .map_err(SignallingError::Am)?;
                    control
                        .require_delivery_admission(key)
                        .map_err(SignallingError::Am)?;
                    let nas_bearer = CommonAmBearer::new(
                        source,
                        channel,
                        key,
                        assigned.crnti(),
                        logical(SignallingBearer::NasAm)?,
                        CommonAmSettings {
                            config: am_config,
                            limits: am_limits,
                            started_at: now,
                        },
                    )
                    .map_err(SignallingError::Am)?;
                    let capabilities = capabilities::CapabilityExchange::with_limit(
                        key,
                        now,
                        limits.maximum_sdu_bytes,
                    )
                    .map_err(|_| SignallingError::Origin)?;
                    let nas_deliveries = nas_delivery::NasDeliveries::new(am_limits.queued_sdus)?;
                    let security = ConnectionSecurity::with_algorithms(
                        key,
                        getrandom::u32().map_err(|_| SignallingError::Entropy)?,
                        self.security_algorithms,
                    );
                    let protection = protection::Protection::new(
                        key,
                        security.fresh(),
                        limits.maximum_sdu_bytes.get(),
                    )?;
                    setup
                        .publish_with(now, limits.response_guard_chips, |response| {
                            log_common(
                                "RRC connection setup",
                                key,
                                now,
                                &response.bytes,
                                response.bit_length,
                            );
                            rb0.enqueue(
                                Bits::new(&response.bytes, response.bit_length)
                                    .map_err(|_| CommonControlTxError::Payload)?,
                            )
                        })
                        .map_err(setup_error)?;
                    Ok((
                        stored_identity,
                        setup,
                        um,
                        control,
                        nas_bearer,
                        nas_deliveries,
                        deferred,
                        capabilities,
                        security,
                        protection,
                    ))
                })();
                match built {
                    Ok((
                        stored_identity,
                        setup,
                        um,
                        control,
                        nas_bearer,
                        nas_deliveries,
                        deferred,
                        capabilities,
                        security,
                        protection,
                    )) => Ok(Context {
                        reports: Reports::default(),
                        transition_reservation: None,
                        activity: 0,
                        idle_release: None,
                        release_after_core_disconnect: false,
                        transactions: transactions::Transactions::default(),
                        bearer_change: None,
                        frame_pools: [None; FRAME_POOL_CONFIGURATIONS],
                        dedicated_route: None,
                        protection,
                        security,
                        capabilities,
                        identity: stored_identity,
                        lease,
                        setup,
                        um,
                        control,
                        nas_bearer,
                        nas_deliveries,
                        router: None,
                        registration: Registration::Unregistered,
                        registration_imsi: subscriber,
                        core_imsi: None,
                        identification: None,
                        deferred,
                        access_release,
                        supports_csg: false,
                        emergency,
                    }),
                    Err(error) => {
                        ledger
                            .release(&lease)
                            .expect("lease reserved by this owner");
                        Err(error)
                    }
                }
            })
            .map_err(|error| match error {
                RegistryInsertError::Capacity => SignallingError::Capacity,
                RegistryInsertError::Construction(error) => error,
            })?;
        self.next_radio_id += 1;
        self.identities.insert(identity, key);
        self.cell_ids.insert(assigned.crnti(), key);
        self.urntis.insert(assigned.urnti(), key);
        self.counters.created = self.counters.created.saturating_add(1);
        Ok(key)
    }

    // Invoke only with CRC-valid transport on the control owner and retain the original PDU on Capacity.
    #[cfg(test)]
    pub fn receive(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        pdu: CommonPdu<'_>,
    ) -> Result<(), SignallingError> {
        self.receive_timed(
            key,
            umts_rlc::acknowledged::ReceiveTime::simultaneous(now),
            pdu,
        )
    }

    pub fn receive_timed(
        &mut self,
        key: UeKey,
        time: umts_rlc::acknowledged::ReceiveTime,
        pdu: CommonPdu<'_>,
    ) -> Result<(), SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        if time.serviced().epoch != key.epoch {
            return Err(SignallingError::Origin);
        }
        let CommonHeader::Dedicated {
            identity: CommonIdentity::Cell(identity),
            channel,
        } = pdu.header
        else {
            return Err(SignallingError::Mapping);
        };
        if identity != context.setup.assigned().crnti() {
            return Err(SignallingError::Mapping);
        }
        let bearer = match channel.get() {
            id if id == SignallingBearer::Unacknowledged.identity() => {
                SignallingBearer::Unacknowledged
            }
            id if id == SignallingBearer::ControlAm.identity() => SignallingBearer::ControlAm,
            id if id == SignallingBearer::NasAm.identity() => SignallingBearer::NasAm,
            _ => return Err(SignallingError::Mapping),
        };
        if bearer == SignallingBearer::Unacknowledged {
            context
                .um
                .validate_common(key, pdu)
                .map_err(SignallingError::Um)?;
        }
        transport::receive(context, &mut self.ingress, key, bearer, time, pdu.payload)
    }

    /// The caller supplies a CRC-valid MAC-demultiplexed PDU with the full registered UE generation.
    #[cfg(test)]
    pub fn receive_srb_payload(
        &mut self,
        key: UeKey,
        bearer: SignallingBearer,
        time: umts_rlc::acknowledged::ReceiveTime,
        payload: Bits<'_>,
    ) -> Result<(), SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        if time.serviced().epoch != key.epoch {
            return Err(SignallingError::Origin);
        }
        transport::receive(context, &mut self.ingress, key, bearer, time, payload)
    }

    /// The scheduler validates transport mapping and must reserve admission before committing its callback.
    pub fn publish_srb_payload(
        &mut self,
        key: UeKey,
        bearer: SignallingBearer,
        now: ChipInstant,
        pdu_bytes: NonZeroUsize,
        admit: impl FnOnce(&[u8]) -> Result<crate::tx_evidence::TransportSubmission, SignallingError>,
    ) -> Result<bool, SignallingError> {
        if bearer == SignallingBearer::ControlAm {
            self.refresh_control_status(key);
        }
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        if now.epoch != key.epoch {
            return Err(SignallingError::Origin);
        }
        transport::publish_payload(context, key, bearer, now, pdu_bytes, admit)
    }

    pub fn srb_publication_ready(
        &mut self,
        key: UeKey,
        bearer: SignallingBearer,
        now: ChipInstant,
    ) -> Result<bool, SignallingError> {
        if now.epoch != key.epoch {
            return Err(SignallingError::Origin);
        }
        if bearer == SignallingBearer::ControlAm {
            self.refresh_control_status(key);
        }
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        match bearer {
            SignallingBearer::Unacknowledged => Ok(context.um.publication_ready()),
            SignallingBearer::ControlAm => context
                .control
                .publication_ready(now)
                .map_err(SignallingError::Am),
            SignallingBearer::NasAm => context
                .nas_bearer
                .publication_ready(now)
                .map_err(SignallingError::Am),
            _ => Err(SignallingError::Mapping),
        }
    }

    #[cfg(test)]
    pub fn initiate_am_reset(
        &mut self,
        key: UeKey,
        bearer: SignallingBearer,
        now: ChipInstant,
    ) -> Result<(), SignallingError> {
        if bearer == SignallingBearer::ControlAm && self.ingress.pending(key, bearer) {
            return Err(SignallingError::Capacity);
        }
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        transport::initiate_reset(context, key, bearer, now)
    }

    pub fn collect_ingress_one(
        &mut self,
        now: ChipInstant,
    ) -> Result<IngressWork, SignallingError> {
        let slot = (self.ingress_cursor / 2) as u32;
        let bearer = if self.ingress_cursor.is_multiple_of(2) {
            SignallingBearer::ControlAm
        } else {
            SignallingBearer::NasAm
        };
        self.ingress_cursor = (self.ingress_cursor + 1) % (2 * u64::from(self.registry.capacity()));
        let Some((key, _)) = self.registry.at_slot(slot) else {
            return Ok(IngressWork::Idle);
        };
        match self.collect_one(key, bearer, now) {
            Ok(true) => Ok(IngressWork::Collected { key, bearer }),
            Ok(false) => Ok(IngressWork::Idle),
            Err(SignallingError::Capacity) => Ok(IngressWork::Blocked { key, bearer }),
            Err(error) => Err(error),
        }
    }
    pub fn collect_one(
        &mut self,
        key: UeKey,
        bearer: SignallingBearer,
        now: ChipInstant,
    ) -> Result<bool, SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        if now.epoch != key.epoch {
            return Err(SignallingError::Origin);
        }
        let am = match bearer {
            SignallingBearer::ControlAm => &mut context.control,
            SignallingBearer::NasAm => &mut context.nas_bearer,
            _ => return Err(SignallingError::Mapping),
        };
        let ingress = &mut self.ingress;
        let result = am
            .deliver_with(key, |delivery| {
                let mut count = 0;
                let mut bytes = 0;
                for (sdu, coverage) in delivery.covered_sdus() {
                    umts_rlc::acknowledged::ReceiveTime::new(coverage.first_received(), now)
                        .map_err(|_| SignallingError::Procedure(ProcedureError::Time))?;
                    count += 1;
                    bytes += sdu.len();
                }
                ingress.admit(key, bearer, count, bytes)?;
                for (sdu, coverage) in delivery.covered_sdus() {
                    ingress.push_covered_admitted(key, bearer, now, sdu, Some(coverage));
                }
                Ok(())
            })
            .map(|value| value.is_some())
            .map_err(|error| match error {
                AmPublishError::Rlc(error) => SignallingError::Am(CommonAmError::Rlc(error)),
                AmPublishError::Downstream(error) => error,
            });
        self.refresh_control_status(key);
        result
    }
    fn refresh_control_status(&mut self, key: UeKey) {
        let pending = self.ingress.pending(key, SignallingBearer::ControlAm);
        if let Some(context) = self.registry.get_mut(key) {
            context
                .control
                .upper_admission_pending(key, pending || context.protection.blocks_status())
                .expect("registered control bearer owner");
        }
    }

    #[cfg(test)]
    pub fn process_one(&mut self) -> Option<Result<UeKey, SignallingError>> {
        self.process_one_inner(None, None, &mut |_, _| Err(SignallingError::Message))
    }
    #[cfg(test)]
    pub fn process_one_at(&mut self, now: SecurityTime) -> Option<Result<UeKey, SignallingError>> {
        self.process_one_inner(Some(now), None, &mut |_, _| Err(SignallingError::Message))
    }
    /// A rejected admission must leave external procedure and cipher state unchanged so retained ingress can retry.
    pub fn process_one_at_with_control(
        &mut self,
        now: ChipInstant,
        security: SecurityTime,
        mut accept: impl FnMut(ReceivedControl<'_>, ControlSecurity<'_>) -> Result<(), SignallingError>,
    ) -> Option<Result<UeKey, SignallingError>> {
        self.process_one_inner(Some(security), Some(now), &mut accept)
    }
    fn process_one_inner(
        &mut self,
        now: Option<SecurityTime>,
        radio_time: Option<ChipInstant>,
        accept: &mut dyn FnMut(
            ReceivedControl<'_>,
            ControlSecurity<'_>,
        ) -> Result<(), SignallingError>,
    ) -> Option<Result<UeKey, SignallingError>> {
        let background = self.service_registration_one();
        if !matches!(background, None | Some(Err(SignallingError::Capacity))) {
            return background;
        }
        let registry = &self.registry;
        let Some(entry) = self
            .ingress
            .front_eligible_with_protection(|key, bearer, protected| {
                registry.get(key).is_none_or(|context| {
                    (now.is_some() || context.protection.integrity.configurations() == 0)
                        && (context.setup.phase() != EstablishmentPhase::AwaitingComplete
                            || bearer == SignallingBearer::ControlAm)
                        // TS 25.331 8.1.4.3 makes RRC release terminal for ongoing procedures.
                        && (context.idle_release.is_none()
                            || !matches!(
                                bearer,
                                SignallingBearer::NasAm | SignallingBearer::LowPriorityNasAm
                            ))
                        && (!context
                            .protection
                            .integrity
                            .uplink_suspended(key, bearer.identity())
                            .unwrap_or(false)
                            || (protected == Some(false)
                                && context
                                    .protection
                                    .integrity
                                    .allows_unprotected_uplink(key, bearer.identity())
                                    .unwrap_or(false)))
                })
            })
        else {
            return background;
        };
        let key = entry.key;
        let received_time = match radio_time {
            Some(now) if now.epoch == key.epoch && now.index >= entry.time.serviced().index => {
                entry
                    .time
                    .serviced_at(now)
                    .expect("validated dispatch time")
            }
            Some(_) => {
                return Some(Err(SignallingError::Dedicated(
                    umts_rrc::dedicated::DedicatedError::Time,
                )));
            }
            None => entry.time,
        };
        let result = (|| {
            let message = entry
                .message
                .as_ref()
                .map_err(|error| SignallingError::Codec(*error))?;
            let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
            if message.value.integrity_check_info.is_some() {
                if context.setup.phase() != EstablishmentPhase::Established {
                    return Err(SignallingError::Phase);
                }
                return protection::Protection::receive(
                    context,
                    &mut self.core,
                    &entry,
                    now.ok_or(SignallingError::Phase)?,
                    received_time,
                    accept,
                );
            }
            if !context
                .protection
                .integrity
                .allows_unprotected_uplink(key, entry.bearer.identity())
                .map_err(SignallingError::Integrity)?
            {
                return Err(SignallingError::Nas(NasError::SecurityRequired));
            }
            if matches!(
                message.value.message,
                ULDCCHMessageType::rrcConnectionSetupComplete(_)
            ) {
                if entry.bearer != SignallingBearer::ControlAm {
                    return Err(SignallingError::Mapping);
                }
                context
                    .setup
                    .complete(key, entry.time.serviced(), message)
                    .map_err(SignallingError::Procedure)?;
            } else {
                if context.setup.phase() != EstablishmentPhase::Established {
                    return Err(SignallingError::Phase);
                }
                if signalling_release::admit(
                    entry.bearer,
                    message,
                    context.router.as_ref(),
                    &mut self.core,
                )? {
                    return Ok(key);
                }
                if lifecycle::release_complete(
                    &mut context.idle_release,
                    context.dedicated_route.is_some(),
                    entry.bearer,
                    message,
                    key,
                    entry.coverage,
                    &mut context.control,
                )? {
                    return Ok(key);
                }
                log_dedicated(key, &message.value.message);
                if context
                    .reports
                    .admit(entry.bearer, &message.value.message)?
                {
                    return Ok(key);
                }
                if control::external(&message.value.message) {
                    let result = accept(
                        ReceivedControl {
                            key,
                            bearer: entry.bearer,
                            time: received_time,
                            message,
                            protection: ControlProtection::Unprotected,
                        },
                        ControlSecurity::new(
                            &context.security,
                            &mut context.protection.ciphers,
                            context.router.as_mut().ok_or(SignallingError::Phase)?,
                            &context.control,
                            &mut context.bearer_change,
                            context.dedicated_route,
                        ),
                    );
                    return context.reports.consume_unsupported(result).map(|()| key);
                }
                if matches!(
                    message.value.message,
                    ULDCCHMessageType::ueCapabilityInformation(_)
                ) {
                    let control = &mut context.control;
                    return context
                        .capabilities
                        .receive_with(
                            key,
                            entry.time.serviced(),
                            entry.bearer.identity(),
                            message,
                            |encoded| {
                                control.enqueue(
                                    key,
                                    Bits::new(&encoded.bytes, encoded.bit_length)
                                        .map_err(|_| CommonAmError::Payload)?,
                                )
                            },
                        )
                        .map(|_| key)
                        .map_err(|error| capability_error(error, am_admission_error));
                }
                if !matches!(context.registration, Registration::Active(_)) {
                    return registration::receive_before_registration(
                        context,
                        &mut self.core,
                        key,
                        entry.bearer,
                        entry.time.serviced(),
                        message,
                        self.limits.tmsi_registration,
                    );
                }
                if !context.deferred.is_empty() {
                    let inspected = umts_rrc::nas::inspect_uplink_nas(entry.bearer, message, false)
                        .map_err(SignallingError::Nas)?;
                    context
                        .deferred
                        .hold(key, inspected)
                        .map_err(|error| match error {
                            umts_rrc::nas::pending::Error::Capacity => SignallingError::Capacity,
                            _ => SignallingError::Phase,
                        })?;
                    return Ok(key);
                }
                let router = context.router.as_mut().ok_or(SignallingError::Phase)?;
                let core = &mut self.core;
                let emergency = context.emergency;
                router
                    .forward_with(key, entry.bearer, message, |event| {
                        core.push_nas(event, emergency)
                    })
                    .map_err(nas_error)?;
            }
            Ok(key)
        })();
        if result.is_ok()
            && matches!(
                entry
                    .message
                    .as_ref()
                    .ok()
                    .map(|message| &message.value.message),
                Some(
                    ULDCCHMessageType::initialDirectTransfer(_)
                        | ULDCCHMessageType::uplinkDirectTransfer(_)
                )
            )
            && let Some(context) = self.registry.get_mut(key)
        {
            context.activity = context.activity.saturating_add(1);
        }
        if matches!(result, Err(SignallingError::Capacity)) {
            self.counters.backpressure = self.counters.backpressure.saturating_add(1);
            if self.counters.backpressure.is_power_of_two() {
                let message = entry.message.as_ref().ok();
                let kind = message
                    .map(|message| control::ControlMessageKind::from(&message.value.message));
                let domain = message.and_then(|message| {
                    umts_rrc::nas::inspect_uplink_nas(entry.bearer, message, false)
                        .ok()
                        .map(|nas| nas.domain())
                });
                let releasing = [umts_iuh::Domain::Cs, umts_iuh::Domain::Ps]
                    .map(|domain| self.core.connection_releasing(key, domain));
                let deferred = self
                    .registry
                    .get(key)
                    .map(|context| !context.deferred.is_empty());
                tracing::debug!(
                    "RRC dedicated message waiting: key={key:?} bearer={:?} kind={kind:?} nas_domain={domain:?} releasing_cs_ps={releasing:?} deferred={deferred:?} count={}",
                    entry.bearer,
                    self.counters.backpressure,
                );
            }
        } else {
            if result.is_err()
                && result != Err(SignallingError::RegistrationPending)
                && result != Err(SignallingError::Duplicate)
            {
                self.counters.rejected = self.counters.rejected.saturating_add(1);
                const INITIAL_REJECTION_REPORTS: u64 = 16;
                let count = self.counters.rejected;
                if count <= INITIAL_REJECTION_REPORTS || count.is_power_of_two() {
                    let message = entry.message.as_ref().ok();
                    let kind = message
                        .map(|message| control::ControlMessageKind::from(&message.value.message));
                    let integrity_present =
                        message.map(|message| message.value.integrity_check_info.is_some());
                    let setup_phase = self.registry.get(key).map(|context| context.setup.phase());
                    const NAS_PROTOCOL_DISCRIMINATOR_MASK: u8 = 0x0f;
                    let nas_header = message.and_then(|message| {
                        umts_rrc::nas::inspect_uplink_nas(entry.bearer, message, false)
                            .ok()
                            .and_then(|nas| {
                                let bytes = nas.nas();
                                Some((
                                    nas.domain(),
                                    bytes.first()? & NAS_PROTOCOL_DISCRIMINATOR_MASK,
                                    *bytes.get(1)?,
                                ))
                            })
                    });
                    tracing::warn!(
                        "RRC dedicated message rejected: key={key:?} bearer={:?} kind={kind:?} error={:?}",
                        entry.bearer,
                        result.as_ref().err(),
                    );
                    tracing::debug!(
                        "RRC dedicated rejection detail: key={key:?} received={:?} serviced={:?} integrity_present={integrity_present:?} setup_phase={setup_phase:?} nas_domain_pd_type={nas_header:?} count={count}",
                        entry.time,
                        radio_time,
                    );
                }
            }
            if result.is_err()
                && result != Err(SignallingError::Duplicate)
                && entry.bearer == SignallingBearer::ControlAm
                && let Some(context) = self.registry.get_mut(key)
            {
                context.protection.reject_control(entry.coverage);
            }
            self.ingress.pop();
            self.refresh_control_status(key);
            self.counters.processed = self.counters.processed.saturating_add(1);
        }
        Some(result)
    }

    // TS 25.469 8.4.2 supplies Context-ID in UE REGISTER ACCEPT, not a local per-domain allocator.
    pub fn registration_accepted(
        &mut self,
        key: UeKey,
        context_id: CoreContextId,
    ) -> Result<(), SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        if context.registration == Registration::Active(context_id) {
            return Ok(());
        }
        if context.registration != Registration::Pending {
            return Err(SignallingError::Phase);
        }
        if self.gateway_contexts.contains_key(&context_id) {
            return Err(SignallingError::Nas(NasError::Context));
        }
        let starts = context.setup.starts().ok_or(SignallingError::Phase)?;
        context.router = Some(NasRouter::new(key, context_id, starts, false));
        context.registration = Registration::Active(context_id);
        self.gateway_contexts.insert(context_id, key);
        Ok(())
    }

    pub fn downlink_nas(
        &mut self,
        connection: CoreConnection,
        priority: DownlinkPriority,
        nas: &[u8],
    ) -> Result<(), SignallingError> {
        let context = self
            .registry
            .get_mut(connection.key.ue)
            .ok_or(SignallingError::Origin)?;
        if context.idle_release.is_some() {
            return Err(SignallingError::Phase);
        }
        let router = context.router.as_mut().ok_or(SignallingError::Phase)?;
        let bearer = &mut context.nas_bearer;
        let deliveries = &mut context.nas_deliveries;
        let result = router
            .downlink_scheduled_with(
                connection,
                priority,
                nas,
                &mut context.protection.integrity,
                |selected, message| {
                    if selected != SignallingBearer::NasAm {
                        return Err(SignallingError::Mapping);
                    }
                    deliveries.enqueue(
                        bearer,
                        connection,
                        Bits::new(&message.bytes, message.bit_length)
                            .map_err(|_| SignallingError::Message)?,
                    )
                },
            )
            .map_err(|error| match error {
                umts_rrc::nas::NasScheduledError::Nas(error) => SignallingError::Nas(error),
                umts_rrc::nas::NasScheduledError::Schedule(error) => {
                    protection::schedule_error(error, |error| error)
                }
            });
        if result.is_ok() {
            context.activity = context.activity.saturating_add(1);
        }
        result
    }

    pub fn publish_rb0(
        &mut self,
        source: &mut dyn CommonTxTarget,
        start: ChipInstant,
    ) -> Result<bool, SignallingError> {
        self.rb0
            .publish_frame(source, start)
            .map_err(SignallingError::Common)
    }
    pub fn publish_bearer(
        &mut self,
        key: UeKey,
        bearer: SignallingBearer,
        source: &mut dyn CommonTxTarget,
        now: ChipInstant,
        start: ChipInstant,
    ) -> Result<bool, SignallingError> {
        if bearer == SignallingBearer::ControlAm {
            self.refresh_control_status(key);
        }
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        if context.dedicated_route.is_some() {
            return Err(SignallingError::Phase);
        }
        transport::publish(context, key, bearer, source, now, start)
    }
    // TS 25.413 8.4.2: a UE whose radio link is lost is released through Iu Release Request
    // with cause "Radio Connection With UE Lost" so the CN ends each connection in order. The
    // context stays until the CN's Iu Release Command completes. Without core connections
    // there is nothing to request and the context is retired at once.
    pub fn release_lost(&mut self, key: UeKey) -> Result<(), SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        let connections = context
            .router
            .as_ref()
            .map_or([None; 2], NasRouter::connections);
        if context.registration == Registration::Pending || connections.iter().all(Option::is_none)
        {
            return self.retire(key);
        }
        for connection in connections.into_iter().flatten() {
            self.core.push_radio_lost_release(connection)?;
        }
        let context = self.registry.get_mut(key).expect("checked above");
        context.release_after_core_disconnect = true;
        Ok(())
    }
    // Radio failure retires both CN domains atomically. Normal Iu Release is a separate procedure.
    pub fn retire(&mut self, key: UeKey) -> Result<(), SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        let context_id = match context.registration {
            Registration::Active(id) => Some(id),
            _ => None,
        };
        self.core.push_radio_failure(
            key,
            context_id,
            context.registration == Registration::Pending,
            context
                .router
                .as_ref()
                .map_or([None; 2], NasRouter::connections),
        )?;
        self.retire_admitted(key);
        Ok(())
    }
    fn retire_admitted(&mut self, key: UeKey) {
        let context = self.registry.get_mut(key).expect("admitted context owner");
        context.deferred.retire(key).expect("validated NAS owner");
        let context = self.registry.remove(key).expect("validated context");
        self.ingress.cancel_for(key);
        self.cell_ids.remove(&context.setup.assigned().crnti());
        self.urntis.remove(&context.setup.assigned().urnti());
        self.identities.remove(&context.identity);
        if let Registration::Active(id) = context.registration {
            self.gateway_contexts.remove(&id);
        }
        self.ledger
            .release(&context.lease)
            .expect("context owns its reservation");
        self.counters.retired = self.counters.retired.saturating_add(1);
    }
    pub fn expire_some(
        &mut self,
        now: ChipInstant,
        slot_budget: NonZeroU32,
    ) -> Result<u32, SignallingError> {
        let mut retired = 0;
        for _ in 0..slot_budget.get().min(self.registry.capacity()) {
            let slot = self.cursor;
            self.cursor = (self.cursor + 1) % self.registry.capacity();
            if let Some((key, _)) = self.registry.at_slot(slot) {
                let context = self.registry.get_mut(key).expect("occupied slot");
                let expired = context
                    .setup
                    .expire(now)
                    .map_err(SignallingError::Procedure)?
                    .is_some()
                    || context.setup.phase() == EstablishmentPhase::Closed;
                let failed = context.control.state() == AmState::Failed
                    || context.nas_bearer.state() == AmState::Failed;
                if expired || failed {
                    tracing::warn!(
                        "RRC context retired: ue={key:?} setup_phase={:?} expired={expired} control={:?} nas={:?}",
                        context.setup.phase(),
                        context.control.state(),
                        context.nas_bearer.state()
                    );
                    self.retire(key)?;
                    retired += 1;
                } else {
                    registration::poll_identification(context, now)?;
                    if context
                        .capabilities
                        .expire(key, now)
                        .map_err(|error| capability_error(error, |()| SignallingError::Phase))?
                    {
                        self.counters.capability_timeouts =
                            self.counters.capability_timeouts.saturating_add(1);
                    }
                }
            }
        }
        Ok(retired)
    }
}
fn logical(bearer: SignallingBearer) -> Result<LogicalChannelId, SignallingError> {
    LogicalChannelId::new(bearer.identity()).map_err(|_| SignallingError::Mapping)
}
fn am_admission_error(error: CommonAmError) -> SignallingError {
    match error {
        CommonAmError::Capacity | CommonAmError::Rlc(umts_rlc::acknowledged::AmError::Capacity) => {
            SignallingError::Capacity
        }
        error => SignallingError::Am(error),
    }
}
fn security_error(error: connection_security::Error<()>) -> SignallingError {
    use connection_security::Error;
    match error {
        Error::Owner | Error::Association => SignallingError::Origin,
        Error::Time => SignallingError::Procedure(ProcedureError::Time),
        Error::Phase => SignallingError::Phase,
        Error::Capacity => SignallingError::Capacity,
        Error::Conflict | Error::RadioPlan | Error::MissingStart | Error::Command(_) => {
            SignallingError::ConfigurationFailure
        }
        Error::Exchange(_) | Error::Scheduled(_) | Error::Downstream(()) => {
            SignallingError::Message
        }
    }
}
fn capability_error<E>(
    error: capabilities::ExchangeError<E>,
    downstream: impl FnOnce(E) -> SignallingError,
) -> SignallingError {
    use capabilities::{Error, ExchangeError};
    match error {
        ExchangeError::Origin => SignallingError::Origin,
        ExchangeError::Time => SignallingError::Procedure(ProcedureError::Time),
        ExchangeError::Phase => SignallingError::Phase,
        ExchangeError::Expired => SignallingError::Capabilities(Error::Expired),
        ExchangeError::Message => SignallingError::Capabilities(Error::Message),
        ExchangeError::Length => SignallingError::Message,
        ExchangeError::Transaction => SignallingError::Capabilities(Error::Transaction),
        ExchangeError::MissingRadioCapability => {
            SignallingError::Capabilities(Error::MissingRadioCapability)
        }
        ExchangeError::InterRat => SignallingError::Capabilities(Error::InterRat),
        ExchangeError::Integrity(umts_rrc::integrity::StreamError::Consumer(error)) => {
            downstream(error)
        }
        ExchangeError::ProtectionRequired | ExchangeError::Integrity(_) => {
            SignallingError::Nas(NasError::SecurityRequired)
        }
        ExchangeError::Codec(error) => SignallingError::Codec(error),
        ExchangeError::Security(_) => SignallingError::ConfigurationFailure,
        ExchangeError::Downstream(error) => downstream(error),
    }
}
fn setup_error(error: PublishError<CommonControlTxError>) -> SignallingError {
    match error {
        PublishError::Procedure(error) => SignallingError::Procedure(error),
        PublishError::Downstream(error) => SignallingError::Common(error),
    }
}
fn nas_error(error: NasPublishError<SignallingError>) -> SignallingError {
    match error {
        NasPublishError::Procedure(error) => SignallingError::Nas(error),
        NasPublishError::Downstream(error) => error,
    }
}

#[cfg(test)]
pub(crate) mod tests;

pub(crate) fn log_radio_link_failure_text(message: &str) {
    tracing::info!("{message}");
}

pub(crate) fn log_radio_link_failure(key: UeKey, identity: crate::dedicated_rx::ReceiveIdentity) {
    tracing::warn!("Radio link failure: ue={key:?} identity={identity:?} released as lost");
}

pub(crate) fn log_common(label: &str, key: UeKey, now: ChipInstant, bytes: &[u8], bits: usize) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    tracing::debug!(
        "{label}: slot={} generation={} chip={} bits={bits} hex={hex}",
        key.slot,
        key.generation,
        now.index
    );
}

pub(crate) fn log_dedicated(key: UeKey, message: &ULDCCHMessageType) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    let kind = control::ControlMessageKind::from(message);
    match message {
        ULDCCHMessageType::securityModeComplete(_)
        | ULDCCHMessageType::radioBearerSetupFailure(_)
        | ULDCCHMessageType::radioBearerReconfigurationFailure(_)
        | ULDCCHMessageType::radioBearerReleaseFailure(_)
        | ULDCCHMessageType::physicalChannelReconfigurationFailure(_)
        | ULDCCHMessageType::transportChannelReconfigurationFailure(_)
        | ULDCCHMessageType::securityModeFailure(_) => {
            tracing::debug!(
                "RRC dedicated uplink: slot={} generation={} kind={kind:?} body={message:?}",
                key.slot,
                key.generation
            );
        }
        _ => tracing::debug!(
            "RRC dedicated uplink: slot={} generation={} kind={kind:?}",
            key.slot,
            key.generation
        ),
    }
}
