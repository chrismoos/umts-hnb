// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_CALENDAR: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
pub struct ChannelUse {
    start: ChipInstant,
    downlink: ChipInstant,
    end: ChipInstant,
    control: u16,
    first: u8,
    count: u8,
}

impl ChannelUse {
    pub fn new(
        pool: HsdpaCodeSet,
        downlink: ChipInstant,
        control: u16,
        first: u8,
        count: u8,
    ) -> Result<Self, Error> {
        if count == 0
            || first < pool.first_data()
            || u16::from(first) + u16::from(count)
                > u16::from(pool.first_data()) + u16::from(pool.data_count().get())
            || !pool.control_codes().any(|code| code.index() == control)
        {
            return Err(Error::Codes);
        }
        let start = ChipInstant {
            index: downlink
                .index
                .checked_sub(CONTROL_LEAD_CHIPS)
                .filter(|index| index.is_multiple_of(TTI_SLOTS as u64 * CHIPS_PER_SLOT))
                .ok_or(Error::Time)?,
            ..downlink
        };
        let end = ChipInstant {
            index: start
                .index
                .checked_add(BURST_CHIPS as u64)
                .ok_or(Error::Time)?,
            ..start
        };
        Ok(Self {
            start,
            downlink,
            end,
            control,
            first,
            count,
        })
    }

    pub fn start(self) -> ChipInstant {
        self.start
    }
    pub fn end(self) -> ChipInstant {
        self.end
    }

    pub fn conflicts(self, other: Self) -> bool {
        if self.start.epoch != other.start.epoch {
            return false;
        }
        (self.control == other.control
            && overlaps(
                self.start.index,
                self.start.index + TTI_SLOTS as u64 * CHIPS_PER_SLOT,
                other.start.index,
                other.start.index + TTI_SLOTS as u64 * CHIPS_PER_SLOT,
            ))
            || (overlaps(
                self.downlink.index,
                self.end.index,
                other.downlink.index,
                other.end.index,
            ) && overlaps(
                u64::from(self.first),
                u64::from(self.first) + u64::from(self.count),
                u64::from(other.first),
                u64::from(other.first) + u64::from(other.count),
            ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReservationId {
    owner: NonZeroU64,
    serial: NonZeroU64,
}

struct Reservation {
    id: ReservationId,
    ue: UeKey,
    channels: ChannelUse,
    published: bool,
}

pub struct AllocationCalendar {
    owner: NonZeroU64,
    pool: HsdpaCodeSet,
    frontier: ChipInstant,
    next: Option<NonZeroU64>,
    entries: Box<[Option<Reservation>]>,
}

impl AllocationCalendar {
    pub fn new(
        pool: HsdpaCodeSet,
        frontier: ChipInstant,
        capacity: NonZeroUsize,
    ) -> Result<Self, Error> {
        if capacity.get() > MAX_PENDING {
            return Err(Error::Capacity);
        }
        let owner = NEXT_CALENDAR
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or(Error::Capacity)?;
        Ok(Self {
            owner,
            pool,
            frontier,
            next: Some(NonZeroU64::MIN),
            entries: (0..capacity.get()).map(|_| None).collect(),
        })
    }

    pub fn reserve(
        &mut self,
        ue: UeKey,
        downlink: ChipInstant,
        control: u16,
        first: u8,
        count: u8,
    ) -> Result<ReservationId, Error> {
        let channels = self.available(ue, downlink, control, first, count)?;
        let next = self.next.ok_or(Error::Capacity)?;
        let slot = self
            .entries
            .iter_mut()
            .find(|entry| entry.is_none())
            .ok_or(Error::Capacity)?;
        let id = ReservationId {
            owner: self.owner,
            serial: next,
        };
        *slot = Some(Reservation {
            id,
            ue,
            channels,
            published: false,
        });
        self.next = next.get().checked_add(1).and_then(NonZeroU64::new);
        Ok(id)
    }

    pub fn available(
        &self,
        ue: UeKey,
        downlink: ChipInstant,
        control: u16,
        first: u8,
        count: u8,
    ) -> Result<ChannelUse, Error> {
        let channels = ChannelUse::new(self.pool, downlink, control, first, count)?;
        let span = ((1u32 << count) - 1) << first;
        if crate::radio_resources::lent_data_codes() & span != 0 {
            return Err(Error::Conflict);
        }
        if ue.epoch != self.frontier.epoch
            || channels.start.epoch != self.frontier.epoch
            || channels.start.index < self.frontier.index
        {
            return Err(Error::Time);
        }
        if self.entries.iter().flatten().any(|entry| {
            channels.conflicts(entry.channels)
                || (entry.ue == ue
                    && overlaps(
                        channels.downlink.index,
                        channels.end.index,
                        entry.channels.downlink.index,
                        entry.channels.end.index,
                    ))
        }) {
            return Err(Error::Conflict);
        }
        if self.next.is_none() || self.entries.iter().all(Option::is_some) {
            return Err(Error::Capacity);
        }
        Ok(channels)
    }

    pub fn publish(&mut self, id: ReservationId) -> Result<(), Error> {
        let entry = self
            .entries
            .iter_mut()
            .flatten()
            .find(|entry| entry.id == id)
            .ok_or(Error::Identity)?;
        if entry.published {
            return Err(Error::Unavailable);
        }
        entry.published = true;
        Ok(())
    }

    pub fn validate_unpublished(&self, id: ReservationId) -> Result<(), Error> {
        let entry = self
            .entries
            .iter()
            .flatten()
            .find(|entry| entry.id == id)
            .ok_or(Error::Identity)?;
        if entry.published {
            Err(Error::Unavailable)
        } else {
            Ok(())
        }
    }

    pub fn cancel(&mut self, id: ReservationId) -> Result<(), Error> {
        let slot = self
            .entries
            .iter_mut()
            .find(|entry| entry.as_ref().is_some_and(|entry| entry.id == id))
            .ok_or(Error::Identity)?;
        if slot.as_ref().is_some_and(|entry| entry.published) {
            return Err(Error::Unavailable);
        }
        *slot = None;
        Ok(())
    }

    pub fn advance(&mut self, frontier: ChipInstant) -> Result<(), Error> {
        if frontier.epoch != self.frontier.epoch || frontier.index < self.frontier.index {
            return Err(Error::Time);
        }
        if self
            .entries
            .iter()
            .flatten()
            .any(|entry| !entry.published && entry.channels.start.index < frontier.index)
        {
            return Err(Error::Unavailable);
        }
        for slot in &mut self.entries {
            if slot
                .as_ref()
                .is_some_and(|entry| entry.published && entry.channels.end.index <= frontier.index)
            {
                *slot = None;
            }
        }
        self.frontier = frontier;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        radio_resources::RadioResources,
        test_support::{at, count, source},
    };
    use std::num::NonZeroU8;

    #[test]
    fn lent_codes_are_avoided_until_returned() {
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::MIN)
            .unwrap();
        let pool = *lease.codes();
        let control = pool.control_codes().next().unwrap().index();
        let ue = UeKey {
            epoch: at(0).epoch,
            slot: 0,
            generation: NonZeroU64::MIN,
        };
        let calendar = AllocationCalendar::new(pool, at(0), count(2)).unwrap();
        let downlink = at(CONTROL_LEAD_CHIPS);
        let first = pool.first_data();
        crate::radio_resources::set_lent_data_codes(1 << first);
        assert_eq!(
            calendar.available(ue, downlink, control, first, 15).err(),
            Some(Error::Conflict)
        );
        assert_eq!(
            calendar.available(ue, downlink, control, first, 1).err(),
            Some(Error::Conflict)
        );
        assert!(
            calendar
                .available(ue, downlink, control, first + 1, 14)
                .is_ok()
        );
        crate::radio_resources::set_lent_data_codes(0);
        assert!(calendar.available(ue, downlink, control, first, 15).is_ok());
    }

    #[test]
    fn peers_share_disjoint_codes_and_published_reservations_outlive_cancellation() {
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::new(5).unwrap(), NonZeroU8::new(2).unwrap())
            .unwrap();
        let pool = *lease.codes();
        let control: Vec<_> = pool.control_codes().map(|code| code.index()).collect();
        let ue = UeKey {
            epoch: at(0).epoch,
            slot: 0,
            generation: NonZeroU64::MIN,
        };
        let peer = UeKey { slot: 1, ..ue };
        let mut calendar = AllocationCalendar::new(pool, at(0), count(2)).unwrap();
        let first = calendar
            .reserve(ue, at(CONTROL_LEAD_CHIPS), control[0], pool.first_data(), 2)
            .unwrap();
        assert_eq!(
            calendar.reserve(
                peer,
                at(CONTROL_LEAD_CHIPS),
                control[0],
                pool.first_data() + 2,
                2
            ),
            Err(Error::Conflict)
        );
        assert_eq!(
            calendar.reserve(
                peer,
                at(CONTROL_LEAD_CHIPS),
                control[1],
                pool.first_data() + 1,
                2
            ),
            Err(Error::Conflict)
        );
        let second = calendar
            .reserve(
                peer,
                at(CONTROL_LEAD_CHIPS),
                control[1],
                pool.first_data() + 2,
                2,
            )
            .unwrap();
        assert_eq!(calendar.advance(at(1)), Err(Error::Unavailable));
        calendar.cancel(second).unwrap();
        calendar.publish(first).unwrap();
        assert_eq!(calendar.cancel(first), Err(Error::Unavailable));
        calendar.advance(at(BURST_CHIPS as u64)).unwrap();
        assert_eq!(calendar.publish(first), Err(Error::Identity));
        assert!(calendar.entries.iter().all(Option::is_none));
        assert_eq!(
            calendar.reserve(ue, at(CONTROL_LEAD_CHIPS), control[0], pool.first_data(), 1),
            Err(Error::Time)
        );
        resources.release_hsdpa(lease).unwrap();
    }
}
