// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

const EHS_SDU_LIMIT: usize = 26;
const EHS_SDU_LENGTH_LIMIT: usize = 2047;
const EHS_SDU_HEADER_OCTETS: usize = 2;
const EHS_REORDERING_HEADER_OCTETS: usize = 1;

#[derive(Debug, PartialEq, Eq)]
pub enum QueuePublishError<E> {
    Mac(MacError),
    Admission(E),
}

pub struct PacketQueue {
    format: QueueFormat,
    storage: Box<[u8]>,
    sizes: Box<[u8]>,
    stride: usize,
    head: usize,
    count: usize,
    sequence: u8,
    reserved: Option<PackedPdu>,
    fragment_offset: usize,
}

impl PacketQueue {
    pub fn id(&self) -> u8 {
        self.format.queue
    }

    pub fn new(
        format: QueueFormat,
        capacity: NonZeroUsize,
        maximum_bytes: NonZeroUsize,
    ) -> Result<Self, MacError> {
        let stride = format
            .sizes
            .iter()
            .flatten()
            .map(|size| size.get().div_ceil(8))
            .max()
            .ok_or(MacError::Mapping)?;
        let storage = stride
            .checked_mul(capacity.get())
            .ok_or(MacError::Capacity)?;
        storage
            .checked_add(capacity.get())
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Self>()))
            .filter(|bytes| *bytes <= maximum_bytes.get())
            .ok_or(MacError::Capacity)?;
        Ok(Self {
            format,
            storage: vec![0; storage].into_boxed_slice(),
            sizes: vec![0; capacity.get()].into_boxed_slice(),
            stride,
            head: 0,
            count: 0,
            sequence: 0,
            reserved: None,
            fragment_offset: 0,
        })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn available(&self) -> usize {
        self.sizes.len() - self.count
    }

    pub fn pending_packet_bits(&self) -> usize {
        if self.format.logical_channel.is_some() {
            let mut bits = 0;
            for offset in 0..self.count.min(EHS_SDU_LIMIT) {
                let sid = self.sizes[(self.head + offset) % self.sizes.len()];
                bits += self.format.sizes[usize::from(sid)]
                    .expect("admitted size")
                    .get();
            }
            return if bits == 0 {
                0
            } else {
                bits - self.fragment_offset * 8
                    + (self.count.min(EHS_SDU_LIMIT) * EHS_SDU_HEADER_OCTETS
                        + EHS_REORDERING_HEADER_OCTETS)
                        * 8
            };
        }
        let mut bits = 0;
        let mut runs = 0;
        let mut previous = None;
        for offset in 0..self.count.min(MAX_PDUS) {
            let sid = self.sizes[(self.head + offset) % self.sizes.len()];
            let size = self.format.sizes[usize::from(sid)]
                .expect("admitted size")
                .get();
            if previous != Some(size) {
                if runs == MAX_RUNS {
                    break;
                }
                runs += 1;
            }
            bits += size;
            previous = Some(size);
        }
        if bits == 0 {
            0
        } else {
            bits + FIXED_HEADER_BITS + RUN_HEADER_BITS * runs
        }
    }

    pub fn storage_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.storage.len() + self.sizes.len()
    }

    pub fn enqueue(&mut self, pdu: MacdPdu<'_>) -> Result<(), MacError> {
        let bits = self
            .format
            .sizes
            .get(usize::from(pdu.size_index))
            .copied()
            .flatten()
            .ok_or(MacError::Mapping)?
            .get();
        if pdu.bits.len() != bits {
            return Err(MacError::Mapping);
        }
        if self.available() == 0 {
            return Err(MacError::Capacity);
        }
        let slot = (self.head + self.count) % self.sizes.len();
        let first = slot * self.stride;
        let target = &mut self.storage[first..first + self.stride];
        target.fill(0);
        pdu.bits.copy_into(target, 0)?;
        self.sizes[slot] = pdu.size_index;
        self.count += 1;
        Ok(())
    }

    /// Admission must retain the packed bytes before queued MAC-d PDUs and the TSN advance.
    pub fn publish<T, E>(
        &mut self,
        transport_bits: usize,
        output: &mut [u8],
        admit: impl FnOnce(PackedPdu, Bits<'_>) -> Result<T, E>,
    ) -> Result<Option<T>, QueuePublishError<E>> {
        self.prepare(transport_bits, output, false, admit)
    }

    pub fn stage<T, E>(
        &mut self,
        transport_bits: usize,
        output: &mut [u8],
        admit: impl FnOnce(PackedPdu, Bits<'_>) -> Result<T, E>,
    ) -> Result<Option<T>, QueuePublishError<E>> {
        self.prepare(transport_bits, output, true, admit)
    }

    /// Check the exact uncommitted reservation before a multi-owner release.
    pub fn can_discard(&self, staged: Option<PackedPdu>) -> bool {
        self.reserved == staged
    }

    /// Remove queued data without freeing storage or changing the sequence number.
    pub fn discard(&mut self) -> Result<(), MacError> {
        if self.reserved.is_some() {
            return Err(MacError::Mapping);
        }
        self.head = 0;
        self.count = 0;
        self.fragment_offset = 0;
        Ok(())
    }

    pub fn cancel_staged(&mut self, packet: PackedPdu) -> Result<(), MacError> {
        if self.reserved != Some(packet) {
            return Err(MacError::Mapping);
        }
        self.reserved = None;
        Ok(())
    }

    pub fn commit_staged<T, E>(
        &mut self,
        packet: PackedPdu,
        admit: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, QueuePublishError<E>> {
        if self.reserved != Some(packet) {
            return Err(QueuePublishError::Mac(MacError::Mapping));
        }
        let result = admit().map_err(QueuePublishError::Admission)?;
        self.advance(packet);
        self.reserved = None;
        Ok(result)
    }

    fn advance(&mut self, packet: PackedPdu) {
        let count = packet.pdus - usize::from(packet.end_fragment_octets != 0);
        self.head = (self.head + count) % self.sizes.len();
        self.count -= count;
        self.fragment_offset = packet.end_fragment_octets;
        self.sequence = (self.sequence + 1) % TSN_MODULUS;
    }

    fn prepare<T, E>(
        &mut self,
        transport_bits: usize,
        output: &mut [u8],
        reserve: bool,
        admit: impl FnOnce(PackedPdu, Bits<'_>) -> Result<T, E>,
    ) -> Result<Option<T>, QueuePublishError<E>> {
        if self.reserved.is_some() {
            return Err(QueuePublishError::Mac(MacError::Mapping));
        }
        if let Some(logical) = self.format.logical_channel {
            return self.prepare_ehs(logical, transport_bits, output, reserve, admit);
        }
        let empty = MacdPdu {
            size_index: 0,
            bits: Bits::new(&[], 0).expect("empty bits"),
        };
        let mut pdus = [empty; MAX_PDUS];
        let mut count = 0;
        let mut runs = 0;
        let mut payload = 0_usize;
        let mut previous_size = None;
        for pdu in pdus.iter_mut().take(self.count.min(MAX_PDUS)) {
            let slot = (self.head + count) % self.sizes.len();
            let sid = self.sizes[slot];
            let bits = self.format.sizes[usize::from(sid)]
                .expect("admitted size")
                .get();
            let next_runs = runs + usize::from(previous_size != Some(bits));
            let next_payload = payload
                .checked_add(bits)
                .ok_or(QueuePublishError::Mac(MacError::Capacity))?;
            let total = next_payload
                .checked_add(FIXED_HEADER_BITS + RUN_HEADER_BITS * next_runs)
                .ok_or(QueuePublishError::Mac(MacError::Capacity))?;
            if next_runs > MAX_RUNS || total > transport_bits {
                break;
            }
            let first = slot * self.stride;
            *pdu = MacdPdu {
                size_index: sid,
                bits: Bits::new(&self.storage[first..first + self.stride], bits)
                    .expect("admitted stored PDU"),
            };
            count += 1;
            payload = next_payload;
            runs = next_runs;
            previous_size = Some(bits);
        }
        if count == 0 {
            return Ok(None);
        }
        let output = output
            .get_mut(..transport_bits.div_ceil(8))
            .ok_or(QueuePublishError::Mac(MacError::Capacity))?;
        let packed = self
            .format
            .pack(self.sequence, &pdus[..count], transport_bits, output)
            .map_err(QueuePublishError::Mac)?;
        let result = admit(
            packed,
            Bits::new(output, transport_bits).expect("packed transport"),
        )
        .map_err(QueuePublishError::Admission)?;
        if reserve {
            self.reserved = Some(packed);
        } else {
            self.advance(packed);
        }
        Ok(Some(result))
    }

    fn prepare_ehs<T, E>(
        &mut self,
        logical_channel: u8,
        transport_bits: usize,
        output: &mut [u8],
        reserve: bool,
        admit: impl FnOnce(PackedPdu, Bits<'_>) -> Result<T, E>,
    ) -> Result<Option<T>, QueuePublishError<E>> {
        use crate::hsdpa::{EhsReorderingPdu, EhsSdu, EhsSegmentation, EhsTsnWidth, pack_ehs};
        if transport_bits.div_ceil(8) > output.len() {
            return Err(QueuePublishError::Mac(MacError::Capacity));
        }
        // TS 25.321 9.2.2 places padding after the octet-sized MAC-ehs
        // header and SDUs up to the exact HS-DSCH transport-block size.
        // Table 9.2.3.1 also applies to MAC-ehs: non-octet TBS values
        // leave 1..7 trailing padding bits, not another payload octet.
        let octets = transport_bits / 8;
        let mut sdus = [EhsSdu {
            logical_channel,
            bytes: &[],
        }; EHS_SDU_LIMIT];
        let mut count = 0;
        let mut payload = 0;
        let mut end_fragment_octets = 0;
        for sdu in sdus.iter_mut().take(self.count.min(EHS_SDU_LIMIT)) {
            let slot = (self.head + count) % self.sizes.len();
            let sid = self.sizes[slot];
            let length = self.format.sizes[usize::from(sid)]
                .expect("admitted size")
                .get()
                / 8;
            let offset = if count == 0 { self.fragment_offset } else { 0 };
            let available = octets.saturating_sub(
                payload + (count + 1) * EHS_SDU_HEADER_OCTETS + EHS_REORDERING_HEADER_OCTETS,
            );
            let length_to_send = (length - offset).min(available).min(EHS_SDU_LENGTH_LIMIT);
            if length_to_send == 0 {
                break;
            }
            let first = slot * self.stride + offset;
            *sdu = EhsSdu {
                logical_channel,
                bytes: &self.storage[first..first + length_to_send],
            };
            payload += length_to_send;
            count += 1;
            if offset + length_to_send < length {
                end_fragment_octets = offset + length_to_send;
                break;
            }
        }
        if count == 0 {
            return Ok(None);
        }
        let segmentation = match (self.fragment_offset != 0, end_fragment_octets != 0) {
            (false, false) => EhsSegmentation::Complete,
            (true, false) => EhsSegmentation::Last,
            (false, true) => EhsSegmentation::First,
            (true, true) => EhsSegmentation::LastAndFirstOrMiddle,
        };
        let header = pack_ehs(
            octets,
            EhsTsnWidth::Six,
            &[EhsReorderingPdu {
                queue: self.id(),
                sequence: u16::from(self.sequence),
                segmentation,
                sdus: &sdus[..count],
            }],
            output,
        )
        .map_err(QueuePublishError::Mac)?;
        if !transport_bits.is_multiple_of(8) {
            // Unused low bits of the final octet are zeroed so they enter the PHY CRC
            // deterministically.
            output[octets] = 0;
        }
        let packed = PackedPdu {
            queue: self.id(),
            sequence: self.sequence,
            transport_bits,
            header_bits: header.header_octets * 8,
            payload_bits: payload * 8,
            pdus: count,
            end_fragment_octets,
        };
        let result = admit(
            packed,
            Bits::new(output, transport_bits).expect("validated EHS transport"),
        )
        .map_err(QueuePublishError::Admission)?;
        if reserve {
            self.reserved = Some(packed);
        } else {
            self.advance(packed);
        }
        Ok(Some(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleting_queue_releases_fragments_only_after_staging_custody_returns() {
        let mut sizes = [None; SIZE_INDICES];
        sizes[0] = NonZeroUsize::new(80);
        let mut queue = PacketQueue::new(
            QueueFormat::ehs(0, 1, sizes).unwrap(),
            NonZeroUsize::new(2).unwrap(),
            NonZeroUsize::new(4096).unwrap(),
        )
        .unwrap();
        let storage = queue.storage.as_ptr();
        queue
            .enqueue(MacdPdu {
                size_index: 0,
                bits: Bits::new(&[0x55; 10], 80).unwrap(),
            })
            .unwrap();
        let first = queue
            .publish(64, &mut [0; 8], |packet, _| Ok::<_, ()>(packet))
            .unwrap()
            .unwrap();
        assert_ne!(first.end_fragment_octets, 0);
        let staged = queue
            .stage(64, &mut [0; 8], |packet, _| Ok::<_, ()>(packet))
            .unwrap()
            .unwrap();
        assert!(!queue.can_discard(None));
        assert!(queue.can_discard(Some(staged)));
        assert_eq!(queue.discard(), Err(MacError::Mapping));
        assert_eq!(queue.len(), 1);
        queue.cancel_staged(staged).unwrap();
        queue.discard().unwrap();
        assert!(queue.is_empty());
        assert_eq!(queue.pending_packet_bits(), 0);
        assert_eq!(queue.fragment_offset, 0);
        assert_eq!(queue.sequence, 1);
        assert_eq!(queue.storage.as_ptr(), storage);
        assert_eq!(queue.available(), 2);
    }

    #[test]
    fn ehs_fragment_reservation_retries_without_consuming_the_queue() {
        let mut sizes = [None; SIZE_INDICES];
        sizes[0] = NonZeroUsize::new(80);
        let mut queue = PacketQueue::new(
            QueueFormat::ehs(0, 1, sizes).unwrap(),
            NonZeroUsize::MIN,
            NonZeroUsize::new(4096).unwrap(),
        )
        .unwrap();
        queue
            .enqueue(MacdPdu {
                size_index: 0,
                bits: Bits::new(&[0x55; 10], 80).unwrap(),
            })
            .unwrap();
        let first = queue
            .stage(64, &mut [0; 8], |packet, bits| {
                assert_eq!(bits.read_u32(21, 2).unwrap(), 2);
                Ok::<_, ()>(packet)
            })
            .unwrap()
            .unwrap();
        assert_eq!(first.end_fragment_octets, 5);
        assert_eq!(queue.len(), 1);
        assert!(queue.commit_staged(first, || Err::<(), _>(())).is_err());
        queue.cancel_staged(first).unwrap();
        let retry = queue
            .stage(64, &mut [0; 8], |packet, _| Ok::<_, ()>(packet))
            .unwrap()
            .unwrap();
        assert_eq!(first, retry);
        queue.commit_staged(retry, || Ok::<_, ()>(())).unwrap();
        assert_eq!(queue.len(), 1);
        let last = queue
            .publish(64, &mut [0; 8], |packet, bits| {
                assert_eq!(bits.read_u32(21, 2).unwrap(), 1);
                Ok::<_, ()>(packet)
            })
            .unwrap()
            .unwrap();
        assert_eq!(last.sequence, 1);
        assert_eq!(last.end_fragment_octets, 0);
        assert!(queue.is_empty());
    }

    #[test]
    fn ehs_bit_aligned_blocks_keep_octet_sdus_and_exact_trailing_padding() {
        // CQI C/D rows 1 and 2 are 137 and 173 bits. Also exercise every
        // possible residual-bit count with a dirty destination buffer.
        for transport_bits in [137, 173, 138, 139, 140, 142, 143] {
            let mut sizes = [None; SIZE_INDICES];
            sizes[0] = NonZeroUsize::new(256);
            let mut queue = PacketQueue::new(
                QueueFormat::ehs(0, 3, sizes).unwrap(),
                NonZeroUsize::MIN,
                NonZeroUsize::new(4096).unwrap(),
            )
            .unwrap();
            let payload = [0xa5; 32];
            queue
                .enqueue(MacdPdu {
                    size_index: 0,
                    bits: Bits::new(&payload, 256).unwrap(),
                })
                .unwrap();
            let mut output = [0xff; 32];
            let expected_octets = transport_bits / 8 - 3;
            let packet = queue
                .stage(transport_bits, &mut output, |packet, bits| {
                    assert_eq!(bits.len(), transport_bits);
                    // Decode the independent TS 25.321 header fields: LCH-ID,
                    // octet length, six-bit TSN, SI=First and final-header flag.
                    assert_eq!(bits.read_u32(0, 4).unwrap(), 2);
                    assert_eq!(bits.read_u32(4, 11).unwrap() as usize, expected_octets);
                    assert_eq!(bits.read_u32(15, 6).unwrap(), 0);
                    assert_eq!(bits.read_u32(21, 2).unwrap(), 2);
                    assert_eq!(bits.read_u32(23, 1).unwrap(), 1);
                    for offset in 0..expected_octets {
                        assert_eq!(bits.read_u32(24 + offset * 8, 8).unwrap(), 0xa5);
                    }
                    for offset in (transport_bits / 8 * 8)..transport_bits {
                        assert_eq!(bits.bit(offset), Some(false));
                    }
                    assert_eq!(packet.header_bits, 24);
                    assert_eq!(packet.payload_bits, expected_octets * 8);
                    assert_eq!(packet.transport_bits, transport_bits);
                    Ok::<_, ()>(packet)
                })
                .unwrap()
                .unwrap();
            assert_eq!(output[transport_bits / 8], 0);
            assert_eq!(output[transport_bits.div_ceil(8)], 0xff);
            assert_eq!(queue.len(), 1);
            queue.cancel_staged(packet).unwrap();
            let repeated = queue
                .stage(transport_bits, &mut output, |packet, _| Ok::<_, ()>(packet))
                .unwrap()
                .unwrap();
            assert_eq!(repeated, packet);
            queue.commit_staged(repeated, || Ok::<_, ()>(())).unwrap();
            assert_eq!(queue.fragment_offset, expected_octets);
            assert_eq!(queue.sequence, 1);
        }
    }

    #[test]
    fn ehs_partial_transport_byte_requires_backing_capacity_without_consuming_data() {
        let mut sizes = [None; SIZE_INDICES];
        sizes[0] = NonZeroUsize::new(80);
        let mut queue = PacketQueue::new(
            QueueFormat::ehs(0, 1, sizes).unwrap(),
            NonZeroUsize::MIN,
            NonZeroUsize::new(4096).unwrap(),
        )
        .unwrap();
        queue
            .enqueue(MacdPdu {
                size_index: 0,
                bits: Bits::new(&[0x55; 10], 80).unwrap(),
            })
            .unwrap();
        let mut output = [0xff; 17];
        assert_eq!(
            queue.stage(137, &mut output, |_, _| Ok::<_, ()>(())),
            Err(QueuePublishError::Mac(MacError::Capacity))
        );
        assert_eq!(output, [0xff; 17]);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.fragment_offset, 0);
        assert!(queue.reserved.is_none());
    }

    fn format() -> QueueFormat {
        let mut sizes = [None; SIZE_INDICES];
        sizes[0] = NonZeroUsize::new(8);
        sizes[1] = NonZeroUsize::new(12);
        QueueFormat::new(2, sizes).unwrap()
    }

    #[test]
    fn staged_prefix_can_be_repacked_without_losing_bytes_or_advancing_sequence() {
        let mut queue = PacketQueue::new(
            format(),
            NonZeroUsize::new(3).unwrap(),
            NonZeroUsize::new(4096).unwrap(),
        )
        .unwrap();
        for value in [0x12, 0x34] {
            queue
                .enqueue(MacdPdu {
                    size_index: 0,
                    bits: Bits::new(&[value], 8).unwrap(),
                })
                .unwrap();
        }
        let large = queue
            .stage(40, &mut [0; 5], |packet, _| Ok::<_, ()>(packet))
            .unwrap()
            .unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(large.pdus, 2);
        assert!(
            queue
                .publish(40, &mut [0; 5], |_, _| Ok::<_, ()>(()))
                .is_err()
        );
        assert!(queue.commit_staged(large, || Err::<(), _>("busy")).is_err());
        assert_eq!(queue.len(), 2);
        queue.cancel_staged(large).unwrap();
        let small = queue
            .stage(32, &mut [0; 4], |packet, _| Ok::<_, ()>(packet))
            .unwrap()
            .unwrap();
        assert_eq!(small.pdus, 1);
        assert_eq!(small.sequence, large.sequence);
        queue
            .enqueue(MacdPdu {
                size_index: 0,
                bits: Bits::new(&[0x56], 8).unwrap(),
            })
            .unwrap();
        queue.commit_staged(small, || Ok::<_, ()>(())).unwrap();
        assert_eq!(queue.len(), 2);
        assert!(queue.commit_staged(small, || Ok::<_, ()>(())).is_err());
        queue
            .publish(40, &mut [0; 5], |packet, bits| {
                assert_eq!(packet.sequence, 1);
                assert_eq!(
                    format()
                        .unpack(bits)
                        .unwrap()
                        .pdus()
                        .map(|bits| bits.read_u32(0, 8).unwrap())
                        .collect::<Vec<_>>(),
                    [0x34, 0x56]
                );
                Ok::<_, ()>(())
            })
            .unwrap();
    }

    #[test]
    fn full_category_ten_block_packs_thirty_eight_standard_pdus() {
        let mut sizes = [None; SIZE_INDICES];
        sizes[0] = NonZeroUsize::new(656);
        let format = QueueFormat::new(0, sizes).unwrap();
        let mut queue = PacketQueue::new(
            format,
            NonZeroUsize::new(256).unwrap(),
            NonZeroUsize::new(65_536).unwrap(),
        )
        .unwrap();
        let payload = [0xa5; 82];
        for _ in 0..200 {
            queue
                .enqueue(MacdPdu {
                    size_index: 0,
                    bits: Bits::new(&payload, 656).unwrap(),
                })
                .unwrap();
        }
        assert!(queue.pending_packet_bits() >= 25_558);
        assert_eq!(
            queue.pending_packet_bits(),
            MAX_PDUS * 656 + FIXED_HEADER_BITS + RUN_HEADER_BITS
        );
        let mut output = [0; 25_558_usize.div_ceil(8)];
        queue
            .publish(25_558, &mut output, |packed, bits| {
                assert_eq!(packed.pdus, 38);
                assert_eq!(packed.transport_bits, 25_558);
                assert_eq!(bits.len(), 25_558);
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(queue.len(), 200 - 38);
    }

    #[test]
    fn rejection_retains_bytes_order_and_sequence_then_admission_releases_capacity() {
        let mut queue = PacketQueue::new(
            format(),
            NonZeroUsize::new(3).unwrap(),
            NonZeroUsize::new(4096).unwrap(),
        )
        .unwrap();
        for value in [0x12, 0x34, 0x56] {
            queue
                .enqueue(MacdPdu {
                    size_index: 0,
                    bits: Bits::new(&[value], 8).unwrap(),
                })
                .unwrap();
        }
        let mut output = [0; 5];
        let rejected = queue.publish(40, &mut output, |_, _| Err::<(), _>("full"));
        assert_eq!(rejected, Err(QueuePublishError::Admission("full")));
        let expected = output;
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.pending_packet_bits(), 45);
        queue
            .publish(40, &mut output, |packed, bits| {
                assert_eq!(packed.pdus, 2);
                let decoded = format().unpack(bits).unwrap();
                assert_eq!(decoded.sequence, 0);
                assert_eq!(
                    decoded
                        .pdus()
                        .map(|bits| bits.read_u32(0, 8).unwrap())
                        .collect::<Vec<_>>(),
                    [0x12, 0x34]
                );
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(expected, output);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pending_packet_bits(), 29);
        queue
            .publish(40, &mut output, |_, bits| {
                let decoded = format().unpack(bits).unwrap();
                assert_eq!(decoded.sequence, 1);
                assert_eq!(decoded.pdus().next().unwrap().read_u32(0, 8).unwrap(), 0x56);
                Ok::<_, ()>(())
            })
            .unwrap();
        assert!(queue.is_empty());
        assert_eq!(queue.pending_packet_bits(), 0);
    }

    #[test]
    fn demand_matches_packed_mixed_size_headers_and_bounded_prefix() {
        let mut queue = PacketQueue::new(
            format(),
            NonZeroUsize::new(80).unwrap(),
            NonZeroUsize::new(4096).unwrap(),
        )
        .unwrap();
        for index in 0..80 {
            let sid = (index % 2) as u8;
            let bits = if sid == 0 { 8 } else { 12 };
            queue
                .enqueue(MacdPdu {
                    size_index: sid,
                    bits: Bits::new(&[0x55, 0x50], bits).unwrap(),
                })
                .unwrap();
        }
        let demand = queue.pending_packet_bits();
        assert_eq!(demand, FIXED_HEADER_BITS + MAX_RUNS * RUN_HEADER_BITS + 80);
        let mut output = vec![0; demand.div_ceil(8)];
        queue
            .publish(demand, &mut output, |packed, _| {
                assert_eq!(packed.pdus, MAX_RUNS);
                assert_eq!(packed.header_bits + packed.payload_bits, demand);
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(queue.len(), 72);
    }

    #[test]
    fn non_octet_pdus_survive_ring_and_sequence_wrap_without_reset_on_empty() {
        let mut queue = PacketQueue::new(
            format(),
            NonZeroUsize::MIN,
            NonZeroUsize::new(4096).unwrap(),
        )
        .unwrap();
        for sequence in 0..130 {
            queue
                .enqueue(MacdPdu {
                    size_index: 1,
                    bits: Bits::new(&[0xab, 0xc0], 12).unwrap(),
                })
                .unwrap();
            assert!(
                queue
                    .publish(32, &mut [0; 4], |_, _| Ok::<_, ()>(()))
                    .unwrap()
                    .is_none()
            );
            assert_eq!(queue.len(), 1);
            queue
                .publish(40, &mut [0; 5], |_, bits| {
                    let decoded = format().unpack(bits).unwrap();
                    assert_eq!(
                        usize::from(decoded.sequence),
                        sequence % usize::from(TSN_MODULUS)
                    );
                    assert_eq!(
                        decoded.pdus().next().unwrap().read_u32(0, 12).unwrap(),
                        0xabc
                    );
                    Ok::<_, ()>(())
                })
                .unwrap();
        }
    }
}
