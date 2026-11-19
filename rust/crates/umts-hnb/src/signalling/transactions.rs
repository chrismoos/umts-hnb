// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_rrc::establishment::TransactionId;

const NORMAL_IDENTIFIERS: u8 = 4;

#[derive(Clone, Copy)]
pub(super) enum MessageType {
    CapabilityEnquiry,
    SecurityModeCommand,
    RadioBearerSetup,
    RadioBearerRelease,
    RadioBearerReconfiguration,
    Paging,
    RrcConnectionRelease,
    MeasurementControl,
    CellUpdateConfirm,
    PhysicalChannelReconfiguration,
}

#[derive(Default)]
pub(super) struct Transactions {
    last: [Option<TransactionId>; 10],
}

impl Transactions {
    // TS 25.331 8.6.3.11 and 10.3.3.36 scope a normal identifier to its message type, not to all procedures.
    pub fn next(&self, message: MessageType) -> TransactionId {
        TransactionId::new(
            self.last[message as usize].map_or(0, |last| (last.get() + 1) % NORMAL_IDENTIFIERS),
        )
        .expect("normal transaction identifier")
    }
    pub fn admitted(&mut self, message: MessageType, transaction: TransactionId) {
        self.last[message as usize] = Some(transaction);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_rotate_only_after_admission_and_are_independent_by_message_type_and_owner() {
        let mut first = Transactions::default();
        let second = Transactions::default();
        for expected in [0, 1, 2, 3, 0, 1] {
            for _ in 0..3 {
                assert_eq!(first.next(MessageType::SecurityModeCommand).get(), expected);
            }
            assert_eq!(first.next(MessageType::CapabilityEnquiry).get(), 0);
            assert_eq!(second.next(MessageType::SecurityModeCommand).get(), 0);
            let selected = first.next(MessageType::SecurityModeCommand);
            first.admitted(MessageType::SecurityModeCommand, selected);
        }
        first.admitted(
            MessageType::CapabilityEnquiry,
            TransactionId::new(2).unwrap(),
        );
        assert_eq!(first.next(MessageType::CapabilityEnquiry).get(), 3);
        assert_eq!(first.next(MessageType::SecurityModeCommand).get(), 2);
    }
}
