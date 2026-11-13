// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{AdmissionPhase, Entry, ReceiveIdentity, Rel99Admission};
use std::num::NonZeroUsize;

fn bounded_scan(
    length: usize,
    cursor: usize,
    maximum: NonZeroUsize,
    mut inspect: impl FnMut(usize) -> bool,
) -> (usize, usize) {
    if length == 0 {
        return (0, 0);
    }

    let inspections = maximum.get().min(length);
    let mut next = cursor % length;
    let mut visited = 0;
    for _ in 0..inspections {
        let slot = next;
        next = slot
            .checked_add(1)
            .filter(|next| *next < length)
            .unwrap_or(0);
        if inspect(slot) {
            visited += 1;
        }
    }
    (next, visited)
}

impl Rel99Admission {
    pub fn visit_release_assignments(
        &mut self,
        maximum: NonZeroUsize,
        mut visit: impl FnMut(&mut Self, ReceiveIdentity),
    ) -> usize {
        let (next, visited) = bounded_scan(
            self.entries.len(),
            self.release_owner_cursor,
            maximum,
            |slot| {
                let identity = self.entries[slot]
                    .as_ref()
                    .filter(|entry| {
                        entry
                            .lifecycle
                            .release()
                            .is_some_and(|release| release.core.is_some())
                    })
                    .map(|entry| entry.identity);
                let Some(identity) = identity else {
                    return false;
                };
                visit(self, identity);
                true
            },
        );
        self.release_owner_cursor = next;
        visited
    }
    pub fn visit_additional_assignments(
        &mut self,
        maximum: NonZeroUsize,
        mut visit: impl FnMut(&mut Self, ReceiveIdentity),
    ) -> usize {
        let (next, visited) = bounded_scan(
            self.entries.len(),
            self.addition_owner_cursor,
            maximum,
            |slot| {
                let identity = self.entries[slot]
                    .as_ref()
                    .filter(|entry| {
                        entry
                            .lifecycle
                            .addition()
                            .is_some_and(|addition| addition.assignment.is_some())
                    })
                    .map(|entry| entry.identity);
                let Some(identity) = identity else {
                    return false;
                };
                visit(self, identity);
                true
            },
        );
        self.addition_owner_cursor = next;
        visited
    }

    /// The budget counts inspected slots, including empty ones, so insertion cannot extend a call.
    pub fn visit_rab_assignments<F>(&mut self, maximum: NonZeroUsize, mut visit: F) -> usize
    where
        F: FnMut(&mut Self, ReceiveIdentity),
    {
        let (next, visited) =
            bounded_scan(self.entries.len(), self.rab_owner_cursor, maximum, |slot| {
                let identity = self.entries[slot]
                    .as_ref()
                    .filter(|entry| entry.rab_assignment.is_some())
                    .map(|entry| entry.identity);
                let Some(identity) = identity else {
                    return false;
                };
                visit(self, identity);
                true
            });
        self.rab_owner_cursor = next;
        visited
    }

    // Sharing the media cursor can starve retirement when both scans have a one-slot budget.
    pub fn visit_retired_initial_rabs<F>(&mut self, maximum: NonZeroUsize, mut visit: F) -> usize
    where
        F: FnMut(&mut Self, ReceiveIdentity),
    {
        let (next, visited) = bounded_scan(
            self.entries.len(),
            self.retirement_owner_cursor,
            maximum,
            |slot| {
                let identity = self.entries[slot].as_ref().and_then(|entry: &Entry| {
                    (entry.lifecycle.phase() == AdmissionPhase::Retired
                        && entry.lifecycle.release().is_none())
                    .then_some(entry.identity)
                });
                let Some(identity) = identity else {
                    return false;
                };
                visit(self, identity);
                true
            },
        );
        self.retirement_owner_cursor = next;
        visited
    }
}

#[cfg(test)]
mod tests {
    use super::bounded_scan;
    use crate::dedicated_rx::ReceiveIdentity;
    use std::num::{NonZeroU64, NonZeroUsize};
    use umts_types::{RadioEpoch, UeKey};

    #[derive(Clone, Copy)]
    struct Slot {
        identity: Option<ReceiveIdentity>,
        retired: bool,
        scheduled_release: bool,
    }

    fn identity(slot: u32) -> ReceiveIdentity {
        ReceiveIdentity {
            ue: UeKey {
                epoch: RadioEpoch::new(NonZeroU64::new(1).unwrap()),
                slot,
                generation: NonZeroU64::new(1).unwrap(),
            },
            configuration: NonZeroU64::new(1).unwrap(),
        }
    }

    #[test]
    fn bounded_scan_counts_empty_slots_and_wraps() {
        let entries = [Some(identity(0)), None, Some(identity(2)), None];
        let mut found = Vec::new();
        let (next, visited) =
            bounded_scan(entries.len(), 3, NonZeroUsize::new(3).unwrap(), |slot| {
                if let Some(entry) = entries[slot] {
                    found.push(entry);
                    true
                } else {
                    false
                }
            });
        assert_eq!(next, 2);
        assert_eq!(visited, 1);
        assert_eq!(found.iter().map(|id| id.ue.slot).collect::<Vec<_>>(), [0]);
        let (next, visited) = bounded_scan(entries.len(), next, NonZeroUsize::MIN, |slot| {
            assert_eq!(entries[slot], Some(identity(2)));
            true
        });
        assert_eq!((next, visited), (3, 1));
    }

    #[test]
    fn independent_cursors_do_not_starve_each_other() {
        let entries = [Some(identity(0)), Some(identity(1)), Some(identity(2))];
        let one = NonZeroUsize::new(1).unwrap();
        let mut media = Vec::new();
        let (media_next, _) = bounded_scan(entries.len(), 0, one, |slot| {
            media.push(entries[slot].unwrap());
            true
        });
        let mut cleanup = Vec::new();
        let (cleanup_next, _) = bounded_scan(entries.len(), 0, one, |slot| {
            cleanup.push(entries[slot].unwrap());
            true
        });
        assert_eq!(media[0].ue.slot, 0);
        assert_eq!(cleanup[0].ue.slot, 0);
        let mut media_after = Vec::new();
        bounded_scan(entries.len(), media_next, one, |slot| {
            media_after.push(entries[slot].unwrap());
            true
        });
        let mut cleanup_after = Vec::new();
        bounded_scan(entries.len(), cleanup_next, one, |slot| {
            cleanup_after.push(entries[slot].unwrap());
            true
        });
        assert_eq!(media_after[0].ue.slot, 1);
        assert_eq!(cleanup_after[0].ue.slot, 1);
    }

    #[test]
    fn retired_predicate_excludes_cancelled_and_scheduled_entries() {
        let entries = [
            Some(Slot {
                identity: Some(identity(0)),
                retired: true,
                scheduled_release: false,
            }),
            Some(Slot {
                identity: Some(identity(1)),
                retired: true,
                scheduled_release: true,
            }),
            Some(Slot {
                identity: Some(identity(2)),
                retired: false,
                scheduled_release: false,
            }),
        ];
        let mut found = Vec::new();
        let (_, visited) = bounded_scan(entries.len(), 0, NonZeroUsize::new(3).unwrap(), |slot| {
            let entry = entries[slot].unwrap();
            if entry.retired && !entry.scheduled_release {
                found.push(entry.identity.unwrap());
                true
            } else {
                false
            }
        });
        assert_eq!(visited, 1);
        assert_eq!(found.iter().map(|id| id.ue.slot).collect::<Vec<_>>(), [0]);
    }
}
