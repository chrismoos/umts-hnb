// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{Error, OvsfCode, RadioResources, leaves};
use std::num::{NonZeroU8, NonZeroU64};

const DATA_SF: u16 = 16;
const CONTROL_SF: u16 = 128;
const MAX_DATA_CODES: u8 = 15;
const MAX_CONTROL_CODES: usize = MAX_DATA_CODES as usize;

pub struct SecondaryRadioResources {
    resources: RadioResources,
}

impl SecondaryRadioResources {
    pub fn new(
        epoch: umts_types::RadioEpoch,
        primary_cell: &crate::cell::CellInformation,
        scrambling_code: umts_dsp::scrambling::PrimaryScramblingCode,
    ) -> Result<Self, Error> {
        if scrambling_code == primary_cell.primary_code() {
            return Err(Error::Configuration);
        }
        let mut resources =
            RadioResources::new(epoch, primary_cell, std::num::NonZeroUsize::MIN, 0..=0)?;
        resources.primary = scrambling_code.index();
        resources.occupied.fill(false);
        let pilot = OvsfCode::new(256, 0).map_err(|_| Error::Configuration)?;
        resources.occupied[leaves(pilot)].fill(true);
        Ok(Self { resources })
    }

    pub fn reserve_hsdpa(
        &mut self,
        data_count: NonZeroU8,
        control_count: NonZeroU8,
    ) -> Result<HsdpaCodeLease, Error> {
        self.resources.reserve_hsdpa(data_count, control_count)
    }

    pub fn scrambling_code(&self) -> umts_dsp::scrambling::PrimaryScramblingCode {
        umts_dsp::scrambling::PrimaryScramblingCode::new(self.resources.primary)
            .expect("validated secondary scrambling code")
    }
}

#[cfg(test)]
mod secondary_tests {
    use super::*;
    use crate::test_support::{at, count, source};
    use umts_dsp::scrambling::PrimaryScramblingCode;

    #[test]
    fn secondary_owns_fifteen_data_codes_without_primary_reservations() {
        let source = source(true);
        let mut primary =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=2).unwrap();
        let primary_lease = primary
            .reserve_hsdpa(NonZeroU8::new(5).unwrap(), NonZeroU8::new(2).unwrap())
            .unwrap();
        let mut secondary = SecondaryRadioResources::new(
            at(0).epoch,
            source.information(),
            PrimaryScramblingCode::new(470).unwrap(),
        )
        .unwrap();
        let lease = secondary
            .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::new(2).unwrap())
            .unwrap();
        assert_eq!(lease.codes().first_data(), 1);
        assert_eq!(lease.codes().data_count().get(), 15);
        assert_eq!(lease.codes().control_codes().count(), 2);
        assert_eq!(lease.primary_code(), 470);
        let pilot = OvsfCode::new(256, 0).unwrap();
        assert!(lease.codes().data_codes().all(|code| !code.overlaps(pilot)));
        assert!(
            lease
                .codes()
                .control_codes()
                .all(|code| !code.overlaps(pilot))
        );
        let rejected = primary
            .release_hsdpa(lease)
            .expect_err("different carrier owner");
        assert_eq!(rejected.error, Error::Identity);
        assert!(secondary.resources.release_hsdpa(rejected.lease).is_ok());
        assert_eq!(primary.outstanding(), 1);
        assert!(primary.release_hsdpa(primary_lease).is_ok());
    }

    #[test]
    fn secondary_cannot_reuse_primary_scrambling_identity() {
        let source = source(true);
        assert!(
            SecondaryRadioResources::new(
                at(0).epoch,
                source.information(),
                source.information().primary_code()
            )
            .is_err()
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HsdpaCodeSet {
    first_data: u8,
    data_count: NonZeroU8,
    control: [u8; MAX_CONTROL_CODES],
    control_count: NonZeroU8,
}

impl HsdpaCodeSet {
    pub fn first_data(self) -> u8 {
        self.first_data
    }

    pub fn data_count(self) -> NonZeroU8 {
        self.data_count
    }

    pub fn data_codes(&self) -> impl Iterator<Item = OvsfCode> + '_ {
        (self.first_data..self.first_data + self.data_count.get()).map(|index| {
            OvsfCode::new(DATA_SF, u16::from(index)).expect("bounded HS-PDSCH allocation")
        })
    }

    pub fn control_codes(&self) -> impl Iterator<Item = OvsfCode> + '_ {
        self.control[..usize::from(self.control_count.get())]
            .iter()
            .map(|&index| {
                OvsfCode::new(CONTROL_SF, u16::from(index)).expect("bounded HS-SCCH allocation")
            })
    }

    #[cfg(test)]
    fn codes(&self) -> impl Iterator<Item = OvsfCode> + '_ {
        self.data_codes().chain(self.control_codes())
    }
}

#[derive(Debug)]
#[must_use = "Keep the carrier lease until all HSDPA users and scheduled samples retire"]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete observation for diagnostic snapshots and replay."
    )
)]
pub struct HsdpaCodeLease {
    owner: NonZeroU64,
    serial: NonZeroU64,
    primary: u16,
    codes: HsdpaCodeSet,
}

impl HsdpaCodeLease {
    pub(crate) fn primary_code(&self) -> u16 {
        self.primary
    }
    pub fn codes(&self) -> &HsdpaCodeSet {
        &self.codes
    }
}

#[derive(Debug)]
#[cfg(test)]
pub struct HsdpaReleaseError {
    pub error: Error,
    pub lease: HsdpaCodeLease,
}

impl RadioResources {
    pub fn reserve_hsdpa(
        &mut self,
        data_count: NonZeroU8,
        control_count: NonZeroU8,
    ) -> Result<HsdpaCodeLease, Error> {
        if data_count.get() > MAX_DATA_CODES || usize::from(control_count.get()) > MAX_CONTROL_CODES
        {
            return Err(Error::Configuration);
        }
        if self.hsdpa.is_some() {
            return Err(Error::Capacity);
        }
        let serial = self.next.ok_or(Error::Capacity)?;
        for first_data in (1..=DATA_SF as u8 - data_count.get()).rev() {
            let mut codes = HsdpaCodeSet {
                first_data,
                data_count,
                control: [0; MAX_CONTROL_CODES],
                control_count,
            };
            let mut occupied = self.occupied;
            if codes
                .data_codes()
                .any(|code| occupied[leaves(code)].iter().any(|used| *used))
            {
                continue;
            }
            for code in codes.data_codes() {
                occupied[leaves(code)].fill(true);
            }
            let mut selected = 0;
            for index in 0..CONTROL_SF {
                let code = OvsfCode::new(CONTROL_SF, index).expect("bounded HS-SCCH allocation");
                if occupied[leaves(code)].iter().any(|used| *used) {
                    continue;
                }
                codes.control[selected] = index as u8;
                occupied[leaves(code)].fill(true);
                selected += 1;
                if selected == usize::from(control_count.get()) {
                    break;
                }
            }
            if selected != usize::from(control_count.get()) {
                continue;
            }
            self.occupied = occupied;
            self.hsdpa = Some(serial);
            self.next = serial.checked_add(1);
            return Ok(HsdpaCodeLease {
                owner: self.owner,
                serial,
                primary: self.primary,
                codes,
            });
        }
        Err(Error::Capacity)
    }

    /// All shared-pool users and queued radio samples must retire before these codes are reused.
    #[cfg(test)]
    pub fn release_hsdpa(&mut self, lease: HsdpaCodeLease) -> Result<(), HsdpaReleaseError> {
        if lease.owner != self.owner || self.hsdpa != Some(lease.serial) {
            return Err(HsdpaReleaseError {
                error: Error::Identity,
                lease,
            });
        }
        for code in lease.codes.codes() {
            self.occupied[leaves(code)].fill(false);
        }
        self.hsdpa = None;
        Ok(())
    }
}
