// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_dsp::receive_filter::FilteredWindow;
use umts_types::time::SampleInstant;

fn value(index: u64) -> Complex32 {
    Complex32::new(index as f32 + 1.0, -(index as f32) - 0.5)
}

fn feed(history: &mut SampleHistory, first: u64, count: usize, gap: bool) {
    let samples: Vec<_> = (first..first + count as u64).map(value).collect();
    history
        .feed(MatchedSamples {
            window: FilteredWindow {
                first_center: Some(SampleInstant {
                    epoch: history.epoch,
                    index: first,
                }),
                next_input: SampleInstant {
                    epoch: history.epoch,
                    index: first + count as u64,
                },
                sample_stride: 1,
            },
            generation: 1,
            discontinuity: gap,
            samples: &samples,
        })
        .unwrap();
}

#[test]
fn admitted_mirror_prefix_preserves_every_window_across_wrap_gap_and_widening() {
    let epoch = crate::test_support::at(0).epoch;
    for capacity in [1, 2, 3, 7, 17, 32] {
        for initial_window in 1..=capacity {
            for block in 1..=capacity {
                let mut history = SampleHistory::new(epoch, capacity, capacity);
                // History can precede the first admitted receiver.
                feed(&mut history, 37, block, false);
                history.ensure_window(initial_window);
                assert_eq!(history.mirrored_prefix, initial_window - 1);
                let mut window = initial_window;
                let mut next = 37 + block as u64;
                for step in 0..12 {
                    if step == 6 {
                        // Retained samples of the receiver being replaced stay readable after the
                        // window grows.
                        history.ensure_window(capacity);
                        window = capacity;
                    }
                    let gap = step == 4;
                    if gap {
                        next += capacity as u64 + 3;
                    }
                    feed(&mut history, next, block, gap);
                    next += block as u64;
                    let (first, end, _) = history.bounds().unwrap();
                    for begin in first..end {
                        for count in 1..=window.min((end - begin) as usize) {
                            let actual = history.window(begin, count).unwrap();
                            let expected: Vec<_> =
                                (begin..begin + count as u64).map(value).collect();
                            assert_eq!(
                                actual, expected,
                                "capacity={capacity} block={block} window={window} begin={begin}"
                            );
                        }
                    }
                    assert!(history.window(first - 1, 1).is_none());
                    assert!(history.window(end, 1).is_none());
                    assert!(history.window(first, capacity + 1).is_none());
                    if step < 6 {
                        assert!(
                            history.samples.samples()[capacity + history.mirrored_prefix..]
                                .iter()
                                .all(|v| *v == Complex32::default())
                        );
                    }
                }
            }
        }
    }
}
