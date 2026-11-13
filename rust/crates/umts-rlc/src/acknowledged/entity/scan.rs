// SPDX-License-Identifier: AGPL-3.0-or-later

use super::TxSlot;
use std::ops::Range;

pub(super) fn forward(slots: &[TxSlot], range: Range<u64>) -> impl Iterator<Item = (u64, &TxSlot)> {
    let start = (range.start % slots.len() as u64) as usize;
    range.zip(slots[start..].iter().chain(slots.iter().cycle()))
}

pub(super) fn backward(
    slots: &[TxSlot],
    range: Range<u64>,
) -> impl Iterator<Item = (u64, &TxSlot)> {
    let end = (range.end % slots.len() as u64) as usize;
    range
        .rev()
        .zip(slots[..end].iter().rev().chain(slots.iter().rev().cycle()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(count: usize) -> Vec<TxSlot> {
        (0..count)
            .map(|index| TxSlot {
                sequence: Some(index as u64),
                bytes: Vec::new(),
                attempts: 0,
                acknowledged: index % 3 == 0,
                retransmit: index % 7 == 0,
            })
            .collect()
    }

    #[test]
    fn ordered_scans_match_modulo_across_wraps_and_empty_ranges() {
        for count in [1, 3, 32, 2048] {
            let slots = slots(count);
            for start in [0, 1, 31, u64::MAX - 10_000] {
                for length in [0, 1, count - 1, count, count * 2 + 1] {
                    let range = start..start + length as u64;
                    let expected: Vec<_> = range
                        .clone()
                        .map(|index| (index, Some(index % count as u64)))
                        .collect();
                    let actual: Vec<_> = forward(&slots, range.clone())
                        .map(|(index, slot)| (index, slot.sequence))
                        .collect();
                    assert_eq!(actual, expected);
                    let actual: Vec<_> = backward(&slots, range)
                        .map(|(index, slot)| (index, slot.sequence))
                        .collect();
                    assert_eq!(actual, expected.into_iter().rev().collect::<Vec<_>>());
                }
            }
        }
    }
}
