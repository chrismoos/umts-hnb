// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    cell::CellInformation,
    rel99_plan::{CompiledRel99, Rel99PlanError},
};
use std::{
    collections::BTreeSet,
    num::{NonZeroU64, NonZeroUsize},
    ops::RangeInclusive,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use umts_dsp::dedicated_transport::TransportPlanLimits;
use umts_rrc::{
    capabilities::rel99::Rel99Limits,
    dedicated::{NormalDpch, Rel99Configuration},
};
use umts_types::{RadioEpoch, UeKey, channel_codes::OvsfCode};

mod hsdpa;
mod hsupa;
mod reconfiguration;
pub use hsdpa::SecondaryRadioResources;
pub(crate) use hsdpa::lent_data_codes;
#[cfg(test)]
pub(crate) use hsdpa::set_lent_data_codes;
pub use hsdpa::{HsdpaCodeLease, HsdpaCodeSet};
pub use hsupa::{HsupaCodeLease, HsupaPoolIdentity, HsupaUserLease};
pub use reconfiguration::{PreparedRadioChange, RetiredRadioCodes};

const LEAVES: usize = 512;
const MAX_UL_CODE: u32 = (1 << 24) - 1;
const MAX_DL_CODES: usize = 8;
const SECONDARY_TREES: u8 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScramblingTree(u8);

impl ScramblingTree {
    const PRIMARY: Self = Self(0);

    fn all() -> impl Iterator<Item = Self> {
        (0..=SECONDARY_TREES).map(Self)
    }

    fn secondary(self) -> Option<u8> {
        (self != Self::PRIMARY).then_some(self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Identity,
    Capacity,
    Configuration,
    MissingCapabilities,
    Capability(umts_rrc::capabilities::rel99::Error),
    Plan(Rel99PlanError),
}

#[derive(Debug)]
#[must_use = "Retain the lease until radio users retire and scheduled samples leave the radio"]
pub struct RadioLease {
    owner: NonZeroU64,
    key: UeKey,
    serial: NonZeroU64,
}
impl RadioLease {
    #[cfg(test)]
    pub fn key(&self) -> UeKey {
        self.key
    }

    pub(crate) fn receive_identity(&self) -> crate::dedicated_rx::ReceiveIdentity {
        crate::dedicated_rx::ReceiveIdentity {
            ue: self.key,
            configuration: self.serial,
        }
    }
}
#[must_use = "Transfer this allocation into admission or explicitly cancel it"]
pub struct AllocatedRadio {
    lease: RadioLease,
    plan: Arc<CompiledRel99>,
}
impl std::fmt::Debug for AllocatedRadio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AllocatedRadio")
            .field("lease", &self.lease)
            .finish_non_exhaustive()
    }
}
impl AllocatedRadio {
    pub(crate) fn receive_identity(&self) -> crate::dedicated_rx::ReceiveIdentity {
        self.lease.receive_identity()
    }
    pub fn key(&self) -> UeKey {
        self.lease.key
    }
    pub fn plan(&self) -> &Arc<CompiledRel99> {
        &self.plan
    }
    pub(crate) fn into_lease(self) -> RadioLease {
        self.lease
    }
}
#[derive(Debug)]
pub struct CancelError {
    pub error: Error,
    pub allocation: AllocatedRadio,
}
#[derive(Debug)]
pub struct ReleaseError {
    pub error: Error,
    pub lease: RadioLease,
}
#[derive(Debug, PartialEq, Eq)]
enum ReservationChange {
    Stable,
    Staged(reconfiguration::StagedReservation),
    Retiring(reconfiguration::RetiringReservation),
}
struct Reservation {
    key: UeKey,
    serial: NonZeroU64,
    uplink: u32,
    tree: ScramblingTree,
    codes: Vec<OvsfCode>,
    change: ReservationChange,
}
pub struct RadioResources {
    epoch: RadioEpoch,
    owner: NonZeroU64,
    primary: u16,
    occupied: [bool; LEAVES],
    secondary_occupied: [[bool; LEAVES]; SECONDARY_TREES as usize],
    reservations: Vec<Option<Reservation>>,
    uplink: BTreeSet<u32>,
    range: RangeInclusive<u32>,
    next: Option<NonZeroU64>,
    hsdpa: Option<NonZeroU64>,
    hsupa: Option<hsupa::Pool>,
    lends_data_codes: bool,
    loan: Option<hsdpa::DataCodeLoan>,
}
fn leaves(code: OvsfCode) -> std::ops::Range<usize> {
    // TS 25.213 4.3.1.1 places all descendants of one OVSF code in a contiguous subtree.
    let width = LEAVES / usize::from(code.spreading_factor());
    let first = usize::from(code.index()) * width;
    first..first + width
}
impl RadioResources {
    pub fn new(
        epoch: RadioEpoch,
        cell: &CellInformation,
        capacity: NonZeroUsize,
        uplink_codes: RangeInclusive<u32>,
    ) -> Result<Self, Error> {
        if uplink_codes.is_empty() || *uplink_codes.end() > MAX_UL_CODE {
            return Err(Error::Configuration);
        }
        static NEXT: AtomicU64 = AtomicU64::new(NonZeroU64::MIN.get());
        let owner = NEXT
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| Error::Capacity)?;
        let mut occupied = [false; LEAVES];
        for code in cell.downlink_code_reservations() {
            if occupied[leaves(code)].iter().any(|used| *used) {
                return Err(Error::Configuration);
            }
            occupied[leaves(code)].fill(true);
        }
        let mut reservations = Vec::new();
        reservations
            .try_reserve_exact(capacity.get())
            .map_err(|_| Error::Capacity)?;
        reservations.resize_with(capacity.get(), || None);
        Ok(Self {
            epoch,
            owner: NonZeroU64::new(owner).expect("nonzero resource owner"),
            primary: cell.primary_code().index(),
            occupied,
            secondary_occupied: [[false; LEAVES]; SECONDARY_TREES as usize],
            reservations,
            uplink: BTreeSet::new(),
            range: uplink_codes,
            next: Some(NonZeroU64::MIN),
            hsdpa: None,
            hsupa: None,
            lends_data_codes: true,
            loan: None,
        })
    }
    pub fn outstanding(&self) -> usize {
        self.uplink.len() + usize::from(self.hsdpa.is_some()) + usize::from(self.hsupa.is_some())
    }

    pub fn slot_reserved(&self, slot: usize) -> bool {
        self.reservations.get(slot).is_some_and(Option::is_some)
    }
    /// Additional simultaneous common-channel and HSPA load requires a separate capability budget.
    pub fn prepare_for_ue(
        &mut self,
        key: UeKey,
        template: &Rel99Configuration,
        signalling: &crate::signalling::CommonSignalling,
        maximum_total_coded_bits: NonZeroUsize,
        limits: TransportPlanLimits,
    ) -> Result<AllocatedRadio, Error> {
        let report = signalling
            .capabilities(key)
            .ok_or(Error::MissingCapabilities)?;
        if report.key() != key {
            return Err(Error::Identity);
        }
        let capabilities = Rel99Limits::from_report(report).map_err(Error::Capability)?;
        capabilities
            .validate_dch(template)
            .map_err(Error::Capability)?;
        self.prepare_checked(key, template, maximum_total_coded_bits, limits, |plan| {
            let bits = plan
                .maximum_downlink_matched_tti_bits()
                .map_err(Error::Plan)?;
            capabilities
                .validate_downlink_matched_bits(bits)
                .map_err(Error::Capability)
        })
    }
    #[cfg(test)]
    pub fn prepare(
        &mut self,
        key: UeKey,
        template: &Rel99Configuration,
        maximum_total_coded_bits: NonZeroUsize,
        limits: TransportPlanLimits,
    ) -> Result<AllocatedRadio, Error> {
        self.prepare_checked(key, template, maximum_total_coded_bits, limits, |_| Ok(()))
    }

    pub(crate) fn check_downlink_space(&self, physical: &NormalDpch) -> Result<(), Error> {
        self.available_downlink_codes(physical).map(|_| ())
    }

    pub(crate) fn check_primary_downlink_space(&self, physical: &NormalDpch) -> Result<(), Error> {
        self.available_downlink_codes_in(physical, std::iter::once(ScramblingTree::PRIMARY))
            .map(|_| ())
    }

    fn tree(&self, tree: ScramblingTree) -> &[bool; LEAVES] {
        if tree == ScramblingTree::PRIMARY {
            &self.occupied
        } else {
            &self.secondary_occupied[usize::from(tree.0 - 1)]
        }
    }

    fn tree_mut(&mut self, tree: ScramblingTree) -> &mut [bool; LEAVES] {
        if tree == ScramblingTree::PRIMARY {
            &mut self.occupied
        } else {
            &mut self.secondary_occupied[usize::from(tree.0 - 1)]
        }
    }

    fn available_downlink_codes(
        &self,
        physical: &NormalDpch,
    ) -> Result<(ScramblingTree, Vec<OvsfCode>), Error> {
        self.available_downlink_codes_in(physical, ScramblingTree::all())
    }

    fn available_downlink_codes_in(
        &self,
        physical: &NormalDpch,
        trees: impl IntoIterator<Item = ScramblingTree>,
    ) -> Result<(ScramblingTree, Vec<OvsfCode>), Error> {
        let count = physical.downlink_codes.len();
        if count == 0 || count > MAX_DL_CODES {
            return Err(Error::Configuration);
        }
        let mut codes = Vec::new();
        codes
            .try_reserve_exact(count)
            .map_err(|_| Error::Capacity)?;
        for (tree, borrow) in with_borrowing(trees) {
            codes.clear();
            for index in 0..physical.downlink_sf {
                let code =
                    OvsfCode::new(physical.downlink_sf, index).map_err(|_| Error::Configuration)?;
                if self.code_free(tree, code, borrow, |_| false) {
                    codes.push(code);
                }
                if codes.len() == count {
                    return Ok((tree, codes));
                }
            }
        }
        Err(Error::Capacity)
    }

    fn code_free(
        &self,
        tree: ScramblingTree,
        code: OvsfCode,
        borrow: bool,
        retained: impl Fn(usize) -> bool,
    ) -> bool {
        let lendable = (borrow && tree == ScramblingTree::PRIMARY)
            .then(|| self.lendable())
            .flatten()
            .map(leaves);
        leaves(code).all(|leaf| {
            !self.tree(tree)[leaf]
                || retained(leaf)
                || lendable.as_ref().is_some_and(|range| range.contains(&leaf))
        })
    }

    fn prepare_checked(
        &mut self,
        key: UeKey,
        template: &Rel99Configuration,
        maximum_total_coded_bits: NonZeroUsize,
        limits: TransportPlanLimits,
        validate: impl FnOnce(&CompiledRel99) -> Result<(), Error>,
    ) -> Result<AllocatedRadio, Error> {
        let slot = key.slot as usize;
        if key.epoch != self.epoch || slot >= self.reservations.len() {
            return Err(Error::Identity);
        }
        if self.reservations[slot].is_some() {
            return Err(Error::Capacity);
        }
        let serial = self.next.ok_or(Error::Capacity)?;
        let mut uplink = *self.range.start();
        for used in self.uplink.range(self.range.clone()) {
            if *used > uplink {
                break;
            }
            uplink = uplink.checked_add(1).ok_or(Error::Capacity)?;
        }
        if uplink > *self.range.end() {
            return Err(Error::Capacity);
        }
        let mut physical = template.physical().clone();
        let (tree, codes) = self.available_downlink_codes(&physical)?;
        physical.primary_scrambling = self.primary;
        physical.secondary_scrambling = tree.secondary();
        physical.uplink_scrambling = uplink;
        physical.downlink_codes = codes.iter().map(|code| code.index()).collect();
        let configuration = Arc::new(
            template
                .with_physical(physical)
                .map_err(|_| Error::Configuration)?,
        );
        let plan = Arc::new(
            CompiledRel99::new(configuration, maximum_total_coded_bits, limits)
                .map_err(Error::Plan)?,
        );
        validate(&plan)?;
        self.uplink.insert(uplink);
        self.lend_for(tree, &codes);
        for code in &codes {
            self.tree_mut(tree)[leaves(*code)].fill(true)
        }
        self.reservations[slot] = Some(Reservation {
            key,
            serial,
            uplink,
            tree,
            codes,
            change: ReservationChange::Stable,
        });
        self.next = serial.checked_add(1);
        Ok(AllocatedRadio {
            lease: RadioLease {
                owner: self.owner,
                key,
                serial,
            },
            plan,
        })
    }
    pub fn cancel(&mut self, allocation: AllocatedRadio) -> Result<(), CancelError> {
        if let Err(error) = self.check(&allocation.lease) {
            return Err(CancelError { error, allocation });
        }
        if self.reservations[allocation.lease.key.slot as usize]
            .as_ref()
            .is_some_and(|reservation| !matches!(&reservation.change, ReservationChange::Stable))
        {
            return Err(CancelError {
                error: Error::Capacity,
                allocation,
            });
        }
        self.release(allocation.into_lease())
            .expect("validated resource lease");
        Ok(())
    }
    fn check(&self, lease: &RadioLease) -> Result<(), Error> {
        if lease.owner != self.owner || lease.key.epoch != self.epoch {
            return Err(Error::Identity);
        }
        self.reservations
            .get(lease.key.slot as usize)
            .and_then(Option::as_ref)
            .filter(|entry| entry.key == lease.key && entry.serial == lease.serial)
            .map(|_| ())
            .ok_or(Error::Identity)
    }
    /// All scheduled samples using the leased codes must have left the radio before reuse.
    /// Callers must retain any reconfiguration retirement proof until that drain is proven.
    pub fn release(&mut self, lease: RadioLease) -> Result<(), ReleaseError> {
        if let Err(error) = self.check(&lease) {
            return Err(ReleaseError { error, lease });
        }
        if self
            .hsupa
            .as_ref()
            .is_some_and(|pool| pool.has_user(lease.key))
        {
            return Err(ReleaseError {
                error: Error::Capacity,
                lease,
            });
        }
        if self.reservations[lease.key.slot as usize]
            .as_ref()
            .is_some_and(|reservation| !matches!(&reservation.change, ReservationChange::Stable))
        {
            return Err(ReleaseError {
                error: Error::Capacity,
                lease,
            });
        }
        let reservation = self.reservations[lease.key.slot as usize]
            .take()
            .expect("validated resource lease");
        assert!(self.uplink.remove(&reservation.uplink));
        for code in reservation.codes {
            self.tree_mut(reservation.tree)[leaves(code)].fill(false)
        }
        self.reclaim_loan();
        Ok(())
    }
}

fn with_borrowing(
    trees: impl IntoIterator<Item = ScramblingTree>,
) -> impl Iterator<Item = (ScramblingTree, bool)> {
    trees.into_iter().flat_map(|tree| {
        let borrow = (tree == ScramblingTree::PRIMARY).then_some((tree, true));
        std::iter::once((tree, false)).chain(borrow)
    })
}

#[cfg(test)]
mod tests;
