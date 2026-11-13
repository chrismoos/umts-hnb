// SPDX-License-Identifier: AGPL-3.0-or-later

#![cfg(all(feature = "usrsctp", target_os = "linux"))]

mod common;

use common::{KERNEL_UDP_PORT, UNUSED_SCTP_PORT, backend, limits, loopback, roundtrip_to_kernel};
use sctp_transport::{Association, Error};

const KERNEL_SCTP_PORT: u16 = 29205;

#[test]
fn free_local_port_is_reused_and_excludes_fixed_requests() {
    Association::connect_with(backend(None), loopback(UNUSED_SCTP_PORT), limits()).unwrap();
    Association::connect_with(backend(None), loopback(UNUSED_SCTP_PORT), limits()).unwrap();
    assert!(matches!(
        Association::connect_with(
            backend(Some(KERNEL_UDP_PORT)),
            loopback(UNUSED_SCTP_PORT),
            limits()
        ),
        Err(Error::Configuration)
    ));
}

#[test]
#[ignore = "requires `sysctl net.sctp.udp_port=9899`; run without root so raw SCTP is unavailable"]
fn free_local_port_to_kernel_sctp() {
    roundtrip_to_kernel(backend(None), KERNEL_SCTP_PORT);
}
