// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn connectionless_page_cannot_block_later_registration_even_when_radio_page_queue_is_full() {
    for full in [false, true] {
        let source = source(false);
        let mut radio = controller(&source);
        let (mut link, core) = ready(&mut radio);
        if full {
            for index in 0..16 {
                radio.receive_paging(&ranap::paging::Request {
                    domain: Domain::Ps,
                    imsi: hnbap::Imsi::from_digits(&format!("001010{index:09}")).unwrap(),
                    temporary: None,
                    area: None,
                    cause: None,
                    drx_coefficient: Some(6),
                    search_other_domain: true,
                });
            }
        }
        let hex = "000e40150000020003400180001740095000010121436587f9";
        let wire = hex
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        core.rua(rua::Message::Connectionless { ranap: wire });
        assert!(drive(&mut link, &mut radio, 3).gateway.delivered);
        assert_eq!(radio.counters().paging.capacity_dropped, u64::from(full));
        let (key, context, _) = register(&mut link, &core, &mut radio, &source, 0, 4);
        assert_eq!(link.gateway().context(key), Some(context));
        assert_eq!(link.gateway().phase(), umts_core::Phase::Serving);
    }
}
