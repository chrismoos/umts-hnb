// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_bearers::{DeliveryMapping, UpperSdu};

const BITS_PER_OCTET: usize = 8;
const REPLAY_PIECES_PER_SERVICE: usize = MAX_SUBFLOWS * MAX_BLOCKS;

struct Slot {
    metadata: Option<UpperSdu>,
    bytes: Box<[u8]>,
}

pub(super) struct PendingUplink {
    slots: Box<[Slot]>,
    head: usize,
    pub(super) queued: usize,
    pub(super) successor: Option<ReceiveIdentity>,
}

impl PendingUplink {
    pub(super) fn storage(limits: Limits) -> Result<usize, Error> {
        limits
            .frames_per_route
            .get()
            .checked_mul(MAX_SUBFLOWS)
            .and_then(|slots| {
                limits
                    .maximum_payload_bytes
                    .get()
                    .checked_add(std::mem::size_of::<Slot>())
                    .and_then(|width| slots.checked_mul(width))
            })
            .ok_or(Error::Capacity)
    }

    pub(super) fn new(limits: Limits) -> Self {
        Self {
            slots: (0..limits.frames_per_route.get() * MAX_SUBFLOWS)
                .map(|_| Slot {
                    metadata: None,
                    bytes: vec![0; limits.maximum_payload_bytes.get()].into_boxed_slice(),
                })
                .collect(),
            head: 0,
            queued: 0,
            successor: None,
        }
    }

    pub(super) fn accepts(&self, identity: ReceiveIdentity) -> bool {
        self.successor.is_none_or(|current| current == identity)
    }

    pub(super) fn has_pending_for(&self, identity: ReceiveIdentity) -> bool {
        self.queued != 0 && self.successor == Some(identity)
    }

    pub(super) fn push(&mut self, delivery: SduDelivery<'_>) -> Result<bool, Error> {
        if !self.accepts(delivery.metadata.identity) {
            return Err(Error::UplinkMapping(UplinkMappingFailure::Route));
        }
        if self.queued == self.slots.len()
            || delivery.metadata.bits != delivery.data.len()
            || delivery.data.len().div_ceil(BITS_PER_OCTET) > self.slots[0].bytes.len()
        {
            return Err(Error::Capacity);
        }
        let index = (self.head + self.queued) % self.slots.len();
        delivery
            .data
            .copy_to(&mut self.slots[index].bytes[..delivery.data.len().div_ceil(BITS_PER_OCTET)])
            .map_err(|_| Error::Capacity)?;
        self.slots[index].metadata = Some(delivery.metadata);
        self.successor = Some(delivery.metadata.identity);
        self.queued += 1;
        Ok(true)
    }

    pub(super) fn drain(&mut self, binding: &mut Binding) -> Result<bool, Error> {
        for _ in 0..REPLAY_PIECES_PER_SERVICE {
            if !self.has_pending_for(binding.identity) {
                return Ok(true);
            }
            if !self.drain_one(binding)? {
                return Ok(true);
            }
        }
        // Do not expire partially replayed frame assemblies at a work-budget boundary.
        Ok(!self.has_pending_for(binding.identity))
    }

    fn drain_one(&mut self, binding: &mut Binding) -> Result<bool, Error> {
        if self.queued == 0 || self.successor != Some(binding.identity) {
            return Ok(false);
        }
        let slot = &self.slots[self.head];
        let metadata = slot.metadata.expect("retained voice SDU");
        let index = binding
            .layout
            .subflows
            .iter()
            .position(|subflow| subflow.bearer == metadata.bearer)
            .ok_or(Error::UplinkMapping(UplinkMappingFailure::Route))?;
        let result = VoiceTraffic::ingest_uplink(
            binding,
            index,
            SduDelivery {
                metadata,
                data: Bits::new(&slot.bytes, metadata.bits).map_err(|_| Error::Capacity)?,
                mapping: DeliveryMapping::Committed,
            },
        );
        if !matches!(result, Ok(false)) {
            self.slots[self.head].metadata = None;
            self.head = (self.head + 1) % self.slots.len();
            self.queued -= 1;
            if self.queued == 0 {
                self.successor = None;
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{at, count};
    use umts_rlc::transparent::Quality;
    use umts_security::ciphering::Scope;
    use umts_types::UeKey;

    fn limits() -> Limits {
        Limits {
            routes: count(1),
            frames_per_route: count(1),
            maximum_payload_bytes: count(2),
            maximum_storage_bytes: 4096,
        }
    }

    fn delivery(configuration: u64) -> SduDelivery<'static> {
        SduDelivery {
            metadata: UpperSdu {
                identity: ReceiveIdentity {
                    ue: UeKey {
                        epoch: at(0).epoch,
                        slot: 0,
                        generation: NonZeroU64::MIN,
                    },
                    configuration: NonZeroU64::new(configuration).unwrap(),
                },
                bearer: BearerId::new(10).unwrap(),
                scope: Scope::User(CnDomain::Cs),
                received: at(CHIPS_PER_FRAME),
                bits: 9,
                quality: Quality::Correct,
                coverage: None,
                transparent_origin: None,
            },
            data: Bits::new(&[0xa5, 0x80], 9).unwrap(),
            mapping: DeliveryMapping::Committed,
        }
    }

    #[test]
    fn pending_storage_preserves_bits_identity_and_bounded_capacity() {
        let mut pending = PendingUplink::new(limits());
        for _ in 0..MAX_SUBFLOWS {
            assert!(pending.push(delivery(2)).unwrap());
        }
        assert!(matches!(pending.push(delivery(2)), Err(Error::Capacity)));
        assert_eq!(pending.queued, MAX_SUBFLOWS);
        assert_eq!(pending.slots[0].metadata, Some(delivery(2).metadata));
        assert_eq!(&*pending.slots[0].bytes, &[0xa5, 0x80]);
        assert!(pending.has_pending_for(delivery(2).metadata.identity));
        assert!(!pending.has_pending_for(delivery(1).metadata.identity));
        assert!(PendingUplink::storage(limits()).unwrap() > MAX_SUBFLOWS * 2);
    }

    #[test]
    fn invalid_generation_and_width_do_not_replace_queued_ownership() {
        let mut pending = PendingUplink::new(limits());
        pending.push(delivery(2)).unwrap();
        assert!(matches!(
            pending.push(delivery(3)),
            Err(Error::UplinkMapping(UplinkMappingFailure::Route))
        ));
        let mut malformed = delivery(2);
        malformed.metadata.bits += 1;
        assert!(matches!(pending.push(malformed), Err(Error::Capacity)));
        assert_eq!(pending.queued, 1);
        assert_eq!(pending.slots[0].metadata, Some(delivery(2).metadata));
    }
}
