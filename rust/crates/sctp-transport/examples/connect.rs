// SPDX-License-Identifier: AGPL-3.0-or-later

use sctp_transport::{Association, Backend, Limits, Receive, UdpEncapsulation};
use std::{
    num::NonZeroU16,
    num::NonZeroUsize,
    time::{Duration, Instant},
};

const UDP_ENCAPSULATION_PORT: NonZeroU16 = NonZeroU16::new(9899).unwrap();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let backend = match args.next().as_deref() {
        Some("native") => Backend::Native,
        Some("usrsctp") => Backend::Usrsctp { udp: None },
        Some("usrsctp-udp") => Backend::Usrsctp {
            udp: Some(UdpEncapsulation {
                local_port: None,
                remote_port: UDP_ENCAPSULATION_PORT,
            }),
        },
        _ => return Err("usage: connect native|usrsctp|usrsctp-udp IP:PORT".into()),
    };
    let peer = args.next().ok_or("missing peer")?.parse()?;
    let limits = Limits {
        maximum_record_bytes: NonZeroUsize::new(65536).unwrap(),
        receive_chunk_bytes: NonZeroUsize::new(4096).unwrap(),
        outbound_streams: NonZeroU16::new(8).unwrap(),
        inbound_streams: NonZeroU16::new(8).unwrap(),
    };
    let mut association = Association::connect_with(backend, peer, limits)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !association.complete_connect()? {
        if Instant::now() >= deadline {
            return Err("association timeout".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    loop {
        match association.receive_once()? {
            Receive::Notification(value) => {
                println!("{backend:?}: {value:?}");
                break;
            }
            Receive::Idle => {}
            value => return Err(format!("unexpected {value:?}").into()),
        }
        if Instant::now() >= deadline {
            return Err("notification timeout".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}
