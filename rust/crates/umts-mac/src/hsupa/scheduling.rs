// SPDX-License-Identifier: AGPL-3.0-or-later

const INFORMATION_BITS: u32 = 18;
const POWER_HEADROOM_SHIFT: u32 = 13;
const TOTAL_BUFFER_SHIFT: u32 = 8;
const PRIORITY_BUFFER_SHIFT: u32 = 4;
const FIVE_BIT_MASK: u32 = 0x1f;
const FOUR_BIT_MASK: u32 = 0xf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulingInformation {
    pub power_headroom_index: u8,
    pub total_buffer_index: u8,
    pub priority_buffer_index: u8,
    pub highest_priority_logical_channel: u8,
}

impl SchedulingInformation {
    // TS 25.321 Figure 9.2.5.3.2-1 orders UPH, TEBS, HLBS, HLID from MSB to LSB.
    pub fn from_word(word: u32) -> Option<Self> {
        (word < 1 << INFORMATION_BITS).then_some(Self {
            power_headroom_index: ((word >> POWER_HEADROOM_SHIFT) & FIVE_BIT_MASK) as u8,
            total_buffer_index: ((word >> TOTAL_BUFFER_SHIFT) & FIVE_BIT_MASK) as u8,
            priority_buffer_index: ((word >> PRIORITY_BUFFER_SHIFT) & FOUR_BIT_MASK) as u8,
            highest_priority_logical_channel: (word & FOUR_BIT_MASK) as u8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduling_fields_follow_the_eighteen_bit_wire_layout() {
        let fields = SchedulingInformation::from_word(0x0002_27a3).unwrap();
        assert_eq!(fields.power_headroom_index, 17);
        assert_eq!(fields.total_buffer_index, 7);
        assert_eq!(fields.priority_buffer_index, 10);
        assert_eq!(fields.highest_priority_logical_channel, 3);
        assert!(SchedulingInformation::from_word(1 << INFORMATION_BITS).is_none());
        assert!(SchedulingInformation::from_word(u32::MAX).is_none());
        assert_eq!(
            SchedulingInformation::from_word(0)
                .unwrap()
                .total_buffer_index,
            0
        );
    }
}
