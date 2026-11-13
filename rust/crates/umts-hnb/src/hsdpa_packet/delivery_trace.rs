// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    fmt,
    sync::atomic::{AtomicU16, Ordering},
};
use umts_mac::hsdpa::PackedPdu;
use umts_types::bits::{BITS_PER_BYTE, Bits};

const MINIMUM_TRANSPORT_BITS: usize = 30_000;
const MAXIMUM_TRANSPORT_BITS: usize = 65_536;
const MAXIMUM_PACKETS: u16 = 128;
const MAXIMUM_EVENTS: u16 = 2048;
const MAXIMUM_CONTROL_PDUS: u16 = 512;
const MAXIMUM_CONTROL_BYTES: usize = 128;
static PACKETS: AtomicU16 = AtomicU16::new(0);
static EVENTS: AtomicU16 = AtomicU16::new(0);
static CONTROLS: AtomicU16 = AtomicU16::new(0);

fn reserve(counter: &AtomicU16, limit: u16) -> Option<u16> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            (value < limit).then(|| value + 1)
        })
        .ok()
        .map(|value| value + 1)
}

pub(super) fn encoded(packet: PackedPdu, bits: Bits<'_>) -> Option<u16> {
    if packet.transport_bits < MINIMUM_TRANSPORT_BITS
        || packet.transport_bits > MAXIMUM_TRANSPORT_BITS
        || !tracing::enabled!(target: "umts_hnb::delivery_trace", tracing::Level::DEBUG)
    {
        return None;
    }
    let id = reserve(&PACKETS, MAXIMUM_PACKETS)?;
    let mut bytes = vec![0; bits.len().div_ceil(BITS_PER_BYTE)];
    bits.copy_to(&mut bytes).expect("packed diagnostic bytes");
    tracing::debug!(target: "umts_hnb::delivery_trace", packet_id = id,
        packet = ?packet, hex = %Hex(&bytes), "Downlink delivery encoded");
    Some(id)
}

pub(crate) fn event_enabled(id: Option<u16>) -> bool {
    id.is_some()
        && tracing::enabled!(target: "umts_hnb::delivery_trace", tracing::Level::DEBUG)
        && reserve(&EVENTS, MAXIMUM_EVENTS).is_some()
}

pub(crate) fn control_enabled() -> bool {
    tracing::enabled!(target: "umts_hnb::delivery_trace", tracing::Level::DEBUG)
        && PACKETS.load(Ordering::Relaxed) != 0
        && reserve(&CONTROLS, MAXIMUM_CONTROL_PDUS).is_some()
}

pub(crate) fn control_prefix(bytes: &[u8]) -> impl fmt::Display + '_ {
    Hex(&bytes[..bytes.len().min(MAXIMUM_CONTROL_BYTES)])
}

struct Hex<'a>(&'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
