// SPDX-License-Identifier: AGPL-3.0-or-later

pub(crate) fn smooth_fft_length(required: usize) -> usize {
    let mut best = required.next_power_of_two();
    let mut two = 1;
    while two <= best {
        let mut three = two;
        while three <= best {
            let mut five = three;
            while five <= best {
                if five >= required {
                    best = five;
                    break;
                }
                five *= 5;
            }
            three *= 3;
        }
        two *= 2;
    }
    best
}

#[derive(Default)]
pub(crate) struct CompensatedSum {
    sum: f64,
    correction: f64,
}
impl CompensatedSum {
    pub(crate) fn add(&mut self, value: f64) {
        let next = self.sum + value;
        self.correction += if self.sum.abs() >= value.abs() {
            (self.sum - next) + value
        } else {
            (value - next) + self.sum
        };
        self.sum = next;
    }
    pub(crate) fn value(&self) -> f64 {
        (self.sum + self.correction).max(0.0)
    }
}
