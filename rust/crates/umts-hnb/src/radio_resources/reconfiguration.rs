// SPDX-License-Identifier: AGPL-3.0-or-later

#[cfg(test)]
use super::AllocatedRadio;
use super::{
    Error, LEAVES, RadioLease, RadioResources, Reservation, ReservationChange, ScramblingTree,
    leaves,
};
use crate::rel99_plan::CompiledRel99;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use umts_dsp::dedicated_transport::TransportPlanLimits;
use umts_rrc::{capabilities::rel99::Rel99Limits, dedicated::Rel99Configuration};
use umts_types::{UeKey, channel_codes::OvsfCode};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StagedReservation {
    pub(super) serial: NonZeroU64,
    pub(super) tree: ScramblingTree,
    pub(super) codes: Vec<OvsfCode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RetiringReservation {
    pub(super) serial: NonZeroU64,
    pub(super) tree: ScramblingTree,
    pub(super) codes: Vec<OvsfCode>,
}

#[must_use = "Cancel or commit the staged radio change"]
pub struct PreparedRadioChange {
    owner: NonZeroU64,
    key: UeKey,
    previous_serial: NonZeroU64,
    serial: NonZeroU64,
    tree: ScramblingTree,
    codes: Vec<OvsfCode>,
    plan: Arc<CompiledRel99>,
}

impl std::fmt::Debug for PreparedRadioChange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedRadioChange")
            .field("owner", &self.owner)
            .field("key", &self.key)
            .field("previous_serial", &self.previous_serial)
            .field("serial", &self.serial)
            .field("tree", &self.tree)
            .field("codes", &self.codes)
            .finish_non_exhaustive()
    }
}

impl PreparedRadioChange {
    #[cfg(test)]
    pub fn key(&self) -> UeKey {
        self.key
    }

    pub fn plan(&self) -> &Arc<CompiledRel99> {
        &self.plan
    }

    pub fn receive_identity(&self) -> crate::dedicated_rx::ReceiveIdentity {
        crate::dedicated_rx::ReceiveIdentity {
            ue: self.key,
            configuration: self.serial,
        }
    }
}

#[must_use = "Retain the change token until the old radio generation is drained"]
#[derive(Debug)]
pub struct ReconfigurationCancelError {
    pub error: Error,
    pub change: PreparedRadioChange,
}

#[must_use = "Retain the retired-code proof until the old generation is drained"]
#[derive(Debug)]
pub struct RetiredRadioCodes {
    owner: NonZeroU64,
    key: UeKey,
    previous_serial: NonZeroU64,
    serial: NonZeroU64,
    tree: ScramblingTree,
    codes: Vec<OvsfCode>,
}

#[must_use = "Retain the new lease and retired-code proof until handoff completes"]
pub struct CommittedRadioChange {
    lease: RadioLease,
    plan: Arc<CompiledRel99>,
    retired: RetiredRadioCodes,
}

impl std::fmt::Debug for CommittedRadioChange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommittedRadioChange")
            .field("lease", &self.lease)
            .field("retired", &self.retired)
            .finish_non_exhaustive()
    }
}

impl CommittedRadioChange {
    #[cfg(test)]
    pub fn lease(&self) -> &RadioLease {
        &self.lease
    }

    pub fn into_parts(self) -> (RadioLease, Arc<CompiledRel99>, RetiredRadioCodes) {
        (self.lease, self.plan, self.retired)
    }
}

#[must_use = "Retain both ownership tokens and roll back or retry"]
#[derive(Debug)]
pub struct ReconfigurationCommitError {
    pub error: Error,
    pub active: RadioLease,
    pub change: PreparedRadioChange,
}

#[must_use = "Retain the proof and retry retirement after the owner is valid"]
#[derive(Debug)]
pub struct RetireError {
    pub error: Error,
    pub proof: RetiredRadioCodes,
}

impl RadioResources {
    #[cfg(test)]
    pub fn prepare_reconfiguration(
        &mut self,
        active: &RadioLease,
        current: &CompiledRel99,
        template: &Rel99Configuration,
        maximum_total_coded_bits: NonZeroUsize,
        limits: TransportPlanLimits,
    ) -> Result<PreparedRadioChange, Error> {
        self.prepare_reconfiguration_checked(
            active,
            current,
            template,
            maximum_total_coded_bits,
            limits,
            |_| Ok(()),
        )
    }

    pub fn prepare_reconfiguration_for_ue(
        &mut self,
        active: &RadioLease,
        current: &CompiledRel99,
        template: &Rel99Configuration,
        signalling: &crate::signalling::CommonSignalling,
        maximum_total_coded_bits: NonZeroUsize,
        limits: TransportPlanLimits,
    ) -> Result<PreparedRadioChange, Error> {
        let report = signalling
            .capabilities(active.key)
            .ok_or(Error::MissingCapabilities)?;
        if report.key() != active.key {
            return Err(Error::Identity);
        }
        let capabilities = Rel99Limits::from_report(report).map_err(Error::Capability)?;
        capabilities
            .validate_dch(template)
            .map_err(Error::Capability)?;
        self.prepare_reconfiguration_checked(
            active,
            current,
            template,
            maximum_total_coded_bits,
            limits,
            |plan| {
                let bits = plan
                    .maximum_downlink_matched_tti_bits()
                    .map_err(Error::Plan)?;
                capabilities
                    .validate_downlink_matched_bits(bits)
                    .map_err(Error::Capability)
            },
        )
    }

    fn prepare_reconfiguration_checked(
        &mut self,
        active: &RadioLease,
        current: &CompiledRel99,
        template: &Rel99Configuration,
        maximum_total_coded_bits: NonZeroUsize,
        limits: TransportPlanLimits,
        validate: impl FnOnce(&CompiledRel99) -> Result<(), Error>,
    ) -> Result<PreparedRadioChange, Error> {
        self.check(active)?;
        let slot = active.key.slot as usize;
        let reservation = self.reservations[slot]
            .as_ref()
            .expect("validated resource lease");
        if !matches!(&reservation.change, ReservationChange::Stable) {
            return Err(Error::Capacity);
        }
        validate_current_geometry(self, reservation, current)?;
        let serial = self.next.ok_or(Error::Capacity)?;
        let mut physical = template.physical().clone();
        let count = physical.downlink_codes.len();
        if count == 0 || count > super::MAX_DL_CODES {
            return Err(Error::Configuration);
        }
        let old_mask = reservation_leaf_mask(&reservation.codes);
        let (tree, codes) = select_codes(
            self,
            physical.downlink_sf,
            count,
            reservation.tree,
            &reservation.codes,
            &old_mask,
        )?;
        physical.primary_scrambling = self.primary;
        physical.secondary_scrambling = tree.secondary();
        physical.uplink_scrambling = reservation.uplink;
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
        let mut stored_codes = Vec::new();
        stored_codes
            .try_reserve_exact(codes.len())
            .map_err(|_| Error::Capacity)?;
        stored_codes.extend(codes.iter().copied());
        let staged = StagedReservation {
            serial,
            tree,
            codes: stored_codes,
        };
        let change = PreparedRadioChange {
            owner: self.owner,
            key: active.key,
            previous_serial: active.serial,
            serial,
            tree,
            codes,
            plan,
        };
        for code in &change.codes {
            self.tree_mut(change.tree)[leaves(*code)].fill(true);
        }
        self.reservations[slot]
            .as_mut()
            .expect("validated resource lease")
            .change = ReservationChange::Staged(staged);
        self.next = serial.checked_add(1);
        Ok(change)
    }

    pub fn cancel_reconfiguration(
        &mut self,
        change: PreparedRadioChange,
    ) -> Result<(), ReconfigurationCancelError> {
        if let Err(error) = self.validate_change(&change) {
            return Err(ReconfigurationCancelError { error, change });
        }
        let slot = change.key.slot as usize;
        let old_mask = {
            let reservation = self.reservations[slot]
                .as_mut()
                .expect("validated staged change");
            if !matches!(&reservation.change, ReservationChange::Staged(_)) {
                unreachable!("validated staged change");
            }
            reservation.change = ReservationChange::Stable;
            if reservation.tree == change.tree {
                reservation_leaf_mask(&reservation.codes)
            } else {
                [false; LEAVES]
            }
        };
        free_only(self, change.tree, &change.codes, &old_mask);
        Ok(())
    }

    pub fn commit_reconfiguration(
        &mut self,
        active: RadioLease,
        change: PreparedRadioChange,
    ) -> Result<CommittedRadioChange, Box<ReconfigurationCommitError>> {
        if let Err(error) = self.validate_change(&change).and_then(|_| {
            if active.owner != change.owner
                || active.key != change.key
                || active.serial != change.previous_serial
            {
                return Err(Error::Identity);
            }
            Ok(())
        }) {
            return Err(Box::new(ReconfigurationCommitError {
                error,
                active,
                change,
            }));
        }
        let slot = change.key.slot as usize;
        let staged_matches =
            self.reservations[slot]
                .as_ref()
                .is_some_and(|reservation| match &reservation.change {
                    ReservationChange::Staged(staged) => {
                        staged.serial == change.serial
                            && staged.tree == change.tree
                            && staged.codes == change.codes
                    }
                    ReservationChange::Stable | ReservationChange::Retiring(_) => false,
                });
        if !staged_matches {
            return Err(Box::new(ReconfigurationCommitError {
                error: Error::Identity,
                active,
                change,
            }));
        }
        let previous_codes_copy = match clone_codes(
            &self.reservations[slot]
                .as_ref()
                .expect("validated staged change")
                .codes,
        ) {
            Ok(codes) => codes,
            Err(error) => {
                return Err(Box::new(ReconfigurationCommitError {
                    error,
                    active,
                    change,
                }));
            }
        };
        let PreparedRadioChange {
            serial,
            tree,
            codes: candidate_codes,
            plan,
            key,
            ..
        } = change;
        let reservation = self.reservations[slot]
            .as_mut()
            .expect("validated staged change");
        let previous_serial = reservation.serial;
        let previous_tree = reservation.tree;
        let previous_codes = std::mem::take(&mut reservation.codes);
        reservation.serial = serial;
        reservation.tree = tree;
        reservation.codes = candidate_codes;
        let staged = match std::mem::replace(&mut reservation.change, ReservationChange::Stable) {
            ReservationChange::Staged(staged) => staged,
            ReservationChange::Stable | ReservationChange::Retiring(_) => {
                unreachable!("validated staged change")
            }
        };
        debug_assert_eq!(staged.serial, serial);
        debug_assert_eq!(staged.tree, tree);
        debug_assert_eq!(staged.codes, reservation.codes);
        reservation.change = ReservationChange::Retiring(RetiringReservation {
            serial: previous_serial,
            tree: previous_tree,
            codes: previous_codes_copy,
        });
        let retired = RetiredRadioCodes {
            owner: self.owner,
            key,
            previous_serial,
            serial,
            tree: previous_tree,
            codes: previous_codes,
        };
        Ok(CommittedRadioChange {
            lease: RadioLease {
                owner: self.owner,
                key,
                serial,
            },
            plan,
            retired,
        })
    }

    /// Retire only after TX tail submission, conservative device time, RX drain, and old mapping removal are proven.
    pub fn retire_reconfiguration_codes(
        &mut self,
        proof: RetiredRadioCodes,
    ) -> Result<(), RetireError> {
        if let Err(error) = self.validate_proof(&proof) {
            return Err(RetireError { error, proof });
        }
        let slot = proof.key.slot as usize;
        let (retiring, active_mask) = {
            let reservation = self.reservations[slot]
                .as_mut()
                .expect("validated retiring proof");
            let retiring =
                match std::mem::replace(&mut reservation.change, ReservationChange::Stable) {
                    ReservationChange::Retiring(retiring) => retiring,
                    ReservationChange::Stable | ReservationChange::Staged(_) => {
                        unreachable!("validated retiring proof")
                    }
                };
            let active_mask = if reservation.tree == retiring.tree {
                reservation_leaf_mask(&reservation.codes)
            } else {
                [false; LEAVES]
            };
            (retiring, active_mask)
        };
        debug_assert_eq!(retiring.serial, proof.previous_serial);
        debug_assert_eq!(retiring.tree, proof.tree);
        debug_assert_eq!(retiring.codes, proof.codes);
        free_only(self, retiring.tree, &retiring.codes, &active_mask);
        Ok(())
    }

    fn validate_change(&self, change: &PreparedRadioChange) -> Result<(), Error> {
        if change.owner != self.owner || change.key.epoch != self.epoch {
            return Err(Error::Identity);
        }
        let reservation = self
            .reservations
            .get(change.key.slot as usize)
            .and_then(Option::as_ref)
            .ok_or(Error::Identity)?;
        if reservation.key != change.key
            || reservation.serial != change.previous_serial
            || !matches!(
                &reservation.change,
                ReservationChange::Staged(staged)
                    if staged.serial == change.serial && staged.tree == change.tree && staged.codes == change.codes
            )
        {
            return Err(Error::Identity);
        }
        Ok(())
    }

    fn validate_proof(&self, proof: &RetiredRadioCodes) -> Result<(), Error> {
        if proof.owner != self.owner || proof.key.epoch != self.epoch {
            return Err(Error::Identity);
        }
        let reservation = self
            .reservations
            .get(proof.key.slot as usize)
            .and_then(Option::as_ref)
            .ok_or(Error::Identity)?;
        if reservation.key != proof.key
            || reservation.serial != proof.serial
            || !matches!(
                &reservation.change,
                ReservationChange::Retiring(retiring)
                    if retiring.serial == proof.previous_serial && retiring.tree == proof.tree && retiring.codes == proof.codes
            )
        {
            return Err(Error::Identity);
        }
        Ok(())
    }
}

fn validate_current_geometry(
    resources: &RadioResources,
    reservation: &Reservation,
    current: &CompiledRel99,
) -> Result<(), Error> {
    let physical = current.configuration().physical();
    if physical.primary_scrambling != resources.primary
        || physical.secondary_scrambling != reservation.tree.secondary()
        || physical.uplink_scrambling != reservation.uplink
        || physical.downlink_codes.len() != reservation.codes.len()
    {
        return Err(Error::Identity);
    }
    for (index, code) in physical.downlink_codes.iter().zip(&reservation.codes) {
        if OvsfCode::new(physical.downlink_sf, *index).map_err(|_| Error::Configuration)? != *code {
            return Err(Error::Identity);
        }
    }
    Ok(())
}

fn reservation_leaf_mask(codes: &[OvsfCode]) -> [bool; LEAVES] {
    let mut mask = [false; LEAVES];
    for code in codes {
        mask[leaves(*code)].fill(true);
    }
    mask
}

fn select_codes(
    resources: &RadioResources,
    spreading_factor: u16,
    count: usize,
    old_tree: ScramblingTree,
    old_codes: &[OvsfCode],
    old_mask: &[bool; LEAVES],
) -> Result<(ScramblingTree, Vec<OvsfCode>), Error> {
    if old_codes.len() == count
        && old_codes
            .iter()
            .all(|code| code.spreading_factor() == spreading_factor)
    {
        return clone_codes(old_codes).map(|codes| (old_tree, codes));
    }
    let mut codes = Vec::new();
    codes
        .try_reserve_exact(count)
        .map_err(|_| Error::Capacity)?;
    for tree in
        std::iter::once(old_tree).chain(ScramblingTree::all().filter(|tree| *tree != old_tree))
    {
        codes.clear();
        for index in 0..spreading_factor {
            let code = OvsfCode::new(spreading_factor, index).map_err(|_| Error::Configuration)?;
            if leaves(code)
                .all(|leaf| !resources.tree(tree)[leaf] || tree == old_tree && old_mask[leaf])
            {
                codes.push(code);
            }
            if codes.len() == count {
                return Ok((tree, codes));
            }
        }
    }
    Err(Error::Capacity)
}

fn free_only(
    resources: &mut RadioResources,
    tree: ScramblingTree,
    codes: &[OvsfCode],
    retained: &[bool; LEAVES],
) {
    for code in codes {
        for leaf in leaves(*code) {
            if !retained[leaf] {
                resources.tree_mut(tree)[leaf] = false;
            }
        }
    }
}

fn clone_codes(codes: &[OvsfCode]) -> Result<Vec<OvsfCode>, Error> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(codes.len())
        .map_err(|_| Error::Capacity)?;
    copy.extend(codes.iter().copied());
    Ok(copy)
}

#[cfg(test)]
impl AllocatedRadio {
    pub(crate) fn lease(&self) -> &RadioLease {
        &self.lease
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, count, rel99 as fixture};
    use std::num::{NonZeroU8, NonZeroU64};
    use umts_dsp::{coding::DecodeBudget, dedicated_transport::TransportPlanLimits};

    const MAXIMUM_CODED_BITS: usize = 200_000;
    const MAXIMUM_MAPPING_ENTRIES: usize = 300_000;
    const FIRST_UPLINK_CODE: u32 = 17;

    fn key(slot: u32) -> UeKey {
        UeKey {
            epoch: test_support::at(0).epoch,
            slot,
            generation: NonZeroU64::MIN,
        }
    }

    fn limits() -> TransportPlanLimits {
        TransportPlanLimits {
            maximum_coded_bits: count(MAXIMUM_CODED_BITS),
            maximum_mapping_entries: count(MAXIMUM_MAPPING_ENTRIES),
            decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
        }
    }

    fn make_resources(capacity: usize, uplink_end: u32) -> RadioResources {
        let cell = test_support::plan(false);
        RadioResources::new(
            key(0).epoch,
            cell.information(),
            count(capacity),
            FIRST_UPLINK_CODE..=uplink_end,
        )
        .unwrap()
    }

    fn prepare(resources: &mut RadioResources, slot: u32) -> AllocatedRadio {
        resources
            .prepare(
                key(slot),
                &fixture::configuration(true, true, 10),
                count(MAXIMUM_CODED_BITS),
                limits(),
            )
            .unwrap()
    }

    #[test]
    fn cross_tree_change_cancels_and_retires_only_its_owned_tree() {
        let mut resources = make_resources(1, FIRST_UPLINK_CODE);
        let active = prepare(&mut resources, 0);
        resources.occupied.fill(true);
        let before = resources.occupied;
        let mut physical = active.plan().configuration().physical().clone();
        physical
            .downlink_codes
            .push((physical.downlink_codes[0] + 1) % physical.downlink_sf);
        let target = active
            .plan()
            .configuration()
            .with_physical(physical)
            .unwrap();
        let change = resources
            .prepare_reconfiguration(
                active.lease(),
                active.plan(),
                &target,
                count(MAXIMUM_CODED_BITS),
                limits(),
            )
            .unwrap();
        assert_eq!(change.tree.secondary(), Some(1));
        resources.cancel_reconfiguration(change).unwrap();
        assert_eq!(resources.occupied, before);
        assert!(
            resources
                .secondary_occupied
                .iter()
                .flatten()
                .all(|used| !*used)
        );
        let change = resources
            .prepare_reconfiguration(
                active.lease(),
                active.plan(),
                &target,
                count(MAXIMUM_CODED_BITS),
                limits(),
            )
            .unwrap();
        let committed = resources
            .commit_reconfiguration(active.into_lease(), change)
            .unwrap();
        let (lease, plan, retired) = committed.into_parts();
        let secondary = resources.secondary_occupied;
        assert_eq!(
            plan.configuration().physical().secondary_scrambling,
            Some(1)
        );
        assert_eq!(retired.tree, ScramblingTree::PRIMARY);
        resources.retire_reconfiguration_codes(retired).unwrap();
        assert_eq!(resources.secondary_occupied, secondary);
        resources.release(lease).unwrap();
        assert!(
            resources
                .secondary_occupied
                .iter()
                .flatten()
                .all(|used| !*used)
        );
    }

    #[test]
    fn staged_change_keeps_old_and_candidate_codes_until_cancel() {
        let mut resources = make_resources(2, FIRST_UPLINK_CODE + 1);
        let active = prepare(&mut resources, 0);
        let old = active.plan().configuration().clone();
        let mut target = fixture::configuration(true, true, 10);
        let mut target_physical = target.physical().clone();
        target_physical.downlink_codes.push(4);
        target = std::sync::Arc::new(target.with_physical(target_physical).unwrap());
        let occupied_before = resources.occupied;
        let change = resources
            .prepare_reconfiguration(
                active.lease(),
                active.plan(),
                &target,
                count(MAXIMUM_CODED_BITS),
                limits(),
            )
            .unwrap();
        assert_ne!(
            change.receive_identity().configuration,
            active.receive_identity().configuration
        );
        assert_eq!(
            old.physical().uplink_scrambling,
            change.plan().configuration().physical().uplink_scrambling
        );
        assert_eq!(resources.outstanding(), 1);
        let rejected = resources.cancel(active).unwrap_err();
        assert_eq!(rejected.error, Error::Capacity);
        assert!(resources.cancel_reconfiguration(change).is_ok());
        assert_eq!(resources.occupied, occupied_before);
        assert!(Arc::ptr_eq(
            rejected.allocation.plan().configuration(),
            &old
        ));
        resources.cancel(rejected.allocation).unwrap();
    }

    #[test]
    fn commit_requires_retirement_before_release_and_then_frees_old_only_codes() {
        let mut resources = make_resources(1, FIRST_UPLINK_CODE);
        let mut active_template = fixture::configuration(true, true, 10);
        let mut active_physical = active_template.physical().clone();
        active_physical.downlink_codes.push(4);
        active_template =
            std::sync::Arc::new(active_template.with_physical(active_physical).unwrap());
        let active = resources
            .prepare(
                key(0),
                &active_template,
                count(MAXIMUM_CODED_BITS),
                limits(),
            )
            .unwrap();
        let old_occupied = resources.occupied;
        let change = resources
            .prepare_reconfiguration(
                active.lease(),
                active.plan(),
                &fixture::configuration(false, true, 10),
                count(MAXIMUM_CODED_BITS),
                limits(),
            )
            .unwrap();
        assert_eq!(
            resources
                .prepare_reconfiguration(
                    active.lease(),
                    active.plan(),
                    &fixture::configuration(true, true, 10),
                    count(MAXIMUM_CODED_BITS),
                    limits(),
                )
                .unwrap_err(),
            Error::Capacity
        );
        let active_lease = active.into_lease();
        let committed = resources
            .commit_reconfiguration(active_lease, change)
            .unwrap();
        let lease = committed.lease().key();
        assert_eq!(lease, key(0));
        let (lease, plan, proof) = committed.into_parts();
        assert_eq!(
            resources
                .prepare_reconfiguration(
                    &lease,
                    &plan,
                    &fixture::configuration(true, true, 10),
                    count(MAXIMUM_CODED_BITS),
                    limits(),
                )
                .unwrap_err(),
            Error::Capacity
        );
        let rejected = resources.release(lease).unwrap_err();
        assert_eq!(rejected.error, Error::Capacity);
        resources.retire_reconfiguration_codes(proof).unwrap();
        resources.release(rejected.lease).unwrap();
        assert!(
            resources
                .occupied
                .iter()
                .zip(old_occupied)
                .any(|(now, old)| { old && !now })
        );
    }

    #[test]
    fn foreign_or_stale_change_is_returned_without_mutation() {
        let mut resources = make_resources(2, FIRST_UPLINK_CODE + 1);
        let active = prepare(&mut resources, 0);
        let change = resources
            .prepare_reconfiguration(
                active.lease(),
                active.plan(),
                &fixture::configuration(true, true, 10),
                count(MAXIMUM_CODED_BITS),
                limits(),
            )
            .unwrap();
        let mut other = make_resources(1, FIRST_UPLINK_CODE);
        let active = active.into_lease();
        let error = other.commit_reconfiguration(active, change).unwrap_err();
        assert_eq!(error.error, Error::Identity);
        let change = error.change;
        let active = error.active;
        resources.cancel_reconfiguration(change).unwrap();
        resources.release(active).unwrap();
    }

    #[test]
    fn two_ue_stages_and_retirement_keep_independent_code_reservations() {
        let mut resources = make_resources(2, FIRST_UPLINK_CODE + 1);
        let common = resources.occupied;
        let first = prepare(&mut resources, 0);
        let second = prepare(&mut resources, 1);
        let second_codes = resources.reservations[1].as_ref().unwrap().codes.clone();
        let mut physical = first.plan().configuration().physical().clone();
        physical
            .downlink_codes
            .push((physical.downlink_codes[0] + 1) % physical.downlink_sf);
        let target = first
            .plan()
            .configuration()
            .with_physical(physical)
            .unwrap();
        let first_change = resources
            .prepare_reconfiguration(
                first.lease(),
                first.plan(),
                &target,
                count(MAXIMUM_CODED_BITS),
                limits(),
            )
            .unwrap();
        assert!(
            first_change
                .codes
                .iter()
                .all(|candidate| second_codes.iter().all(|peer| !candidate.overlaps(*peer)))
        );
        let second_change = resources
            .prepare_reconfiguration(
                second.lease(),
                second.plan(),
                &target,
                count(MAXIMUM_CODED_BITS),
                limits(),
            )
            .unwrap();
        assert!(second_change.codes.iter().all(|candidate| {
            first_change
                .codes
                .iter()
                .all(|peer| !candidate.overlaps(*peer))
        }));
        assert_eq!(resources.outstanding(), 2);
        let committed = resources
            .commit_reconfiguration(second.into_lease(), second_change)
            .unwrap();
        let (second_lease, second_plan, retired) = committed.into_parts();
        resources.cancel_reconfiguration(first_change).unwrap();
        resources.cancel(first).unwrap();
        assert_eq!(resources.outstanding(), 1);
        for index in &second_plan.configuration().physical().downlink_codes {
            let code =
                OvsfCode::new(second_plan.configuration().physical().downlink_sf, *index).unwrap();
            assert!(resources.occupied[leaves(code)].iter().all(|leaf| *leaf));
        }
        resources.retire_reconfiguration_codes(retired).unwrap();
        resources.release(second_lease).unwrap();
        assert_eq!(resources.outstanding(), 0);
        assert_eq!(resources.occupied, common);
    }

    #[test]
    fn capability_revalidation_failure_leaves_active_reservation_unchanged() {
        use crate::signalling::tests::{
            COMPLETE, controller, establish, peer, send_capability_report,
        };
        use umts_rrc::establishment::TransactionId;

        let source = test_support::source(false);
        let mut signalling = controller(&source);
        let ue = establish(&mut signalling, &source, 0);
        let cell = test_support::plan(false);
        let mut resources = RadioResources::new(
            ue.epoch,
            cell.information(),
            count(1),
            FIRST_UPLINK_CODE..=FIRST_UPLINK_CODE,
        )
        .unwrap();
        let valid = fixture::configuration(true, false, 10);
        let active = resources
            .prepare(ue, &valid, count(MAXIMUM_CODED_BITS), limits())
            .unwrap();
        signalling
            .request_capabilities(ue, test_support::at(0), TransactionId::new(2).unwrap())
            .unwrap();
        let mut peer = peer(&source);
        peer.enqueue(&COMPLETE).unwrap();
        peer.publish_next(test_support::at(0), |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        send_capability_report(&mut signalling, ue, &mut peer);
        assert_eq!(signalling.process_one(), Some(Ok(ue)));

        let mut physical = valid.physical().clone();
        physical.downlink_codes.push(4);
        let unsupported = valid.with_physical(physical).unwrap();
        let occupied_before = resources.occupied;
        assert!(matches!(
            resources.prepare_reconfiguration_for_ue(
                active.lease(),
                active.plan(),
                &unsupported,
                &signalling,
                count(MAXIMUM_CODED_BITS),
                limits(),
            ),
            Err(Error::Capability(_))
        ));
        assert_eq!(resources.occupied, occupied_before);
        assert_eq!(resources.outstanding(), 1);
        resources.cancel(active).unwrap();
    }
}
