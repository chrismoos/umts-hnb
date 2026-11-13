// SPDX-License-Identifier: AGPL-3.0-or-later

#![cfg(all(feature = "usrsctp", target_os = "linux"))]

mod common;

use common::{UNUSED_SCTP_PORT, backend, limits, loopback, roundtrip_to_kernel};
use sctp_transport::{Association, Error};
use std::num::NonZeroU16;

const LOCAL_UDP_PORT: NonZeroU16 = NonZeroU16::new(29900).unwrap();
const KERNEL_SCTP_PORT: u16 = 29202;

#[test]
fn local_udp_port_is_fixed_by_first_association() {
    Association::connect_with(
        backend(Some(LOCAL_UDP_PORT)),
        loopback(UNUSED_SCTP_PORT),
        limits(),
    )
    .unwrap();
    Association::connect_with(backend(None), loopback(UNUSED_SCTP_PORT), limits()).unwrap();
    let other = NonZeroU16::new(LOCAL_UDP_PORT.get() + 1).unwrap();
    assert!(matches!(
        Association::connect_with(backend(Some(other)), loopback(UNUSED_SCTP_PORT), limits()),
        Err(Error::Configuration)
    ));
}

#[test]
#[ignore = "requires `sysctl net.sctp.udp_port=9899`; run without root so raw SCTP is unavailable"]
fn fixed_local_port_to_kernel_sctp() {
    roundtrip_to_kernel(backend(Some(LOCAL_UDP_PORT)), KERNEL_SCTP_PORT);
}
