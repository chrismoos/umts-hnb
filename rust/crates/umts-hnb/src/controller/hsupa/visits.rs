// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroUsize;

pub(super) fn occupied<'a, T>(
    entries: &'a mut [Option<T>],
    cursor: &'a mut usize,
    budget: NonZeroUsize,
) -> impl Iterator<Item = &'a mut T> {
    let capacity = entries.len();
    let start = *cursor;
    let (before, after) = entries.split_at_mut(start);
    // Scan at most one registry rotation so a sparse cell cannot delay feedback or revisit a UE.
    after
        .iter_mut()
        .enumerate()
        .map(move |(index, entry)| (start + index, entry))
        .chain(before.iter_mut().enumerate())
        .filter_map(move |(index, entry)| {
            let session = entry.as_mut()?;
            *cursor = (index + 1) % capacity;
            Some(session)
        })
        .take(budget.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_active_ue_is_visited_on_every_pass_despite_unused_capacity() {
        for slot in [0, 7, 15] {
            let mut entries = [None; 16];
            entries[slot] = Some(slot);
            let mut cursor = 0;
            for _ in 0..16 {
                let visited: Vec<_> =
                    occupied(&mut entries, &mut cursor, NonZeroUsize::new(2).unwrap())
                        .map(|value| *value)
                        .collect();
                assert_eq!(visited, [slot]);
            }
        }
    }

    #[test]
    fn occupied_budget_preserves_round_robin_order_across_wrap_and_removal() {
        let mut entries = [None; 16];
        for slot in [0, 3, 8, 12, 15] {
            entries[slot] = Some(slot);
        }
        let mut cursor = 0;
        for expected in [[0, 3], [8, 12], [15, 0], [3, 8]] {
            let visited: Vec<_> =
                occupied(&mut entries, &mut cursor, NonZeroUsize::new(2).unwrap())
                    .map(|value| *value)
                    .collect();
            assert_eq!(visited, expected);
        }
        entries[12] = None;
        let visited: Vec<_> = occupied(&mut entries, &mut cursor, NonZeroUsize::new(2).unwrap())
            .map(|value| *value)
            .collect();
        assert_eq!(visited, [15, 0]);
    }

    #[test]
    fn a_pass_never_repeats_a_session_and_empty_registries_terminate() {
        let mut entries = [Some(0), None, Some(2)];
        let mut cursor = 2;
        let visited: Vec<_> = occupied(&mut entries, &mut cursor, NonZeroUsize::new(64).unwrap())
            .map(|value| *value)
            .collect();
        assert_eq!(visited, [2, 0]);
        entries.fill(None);
        assert_eq!(
            occupied(&mut entries, &mut cursor, NonZeroUsize::MIN).count(),
            0
        );
        let mut empty: [Option<usize>; 0] = [];
        let mut cursor = 0;
        assert_eq!(
            occupied(&mut empty, &mut cursor, NonZeroUsize::MIN).count(),
            0
        );
    }
}
