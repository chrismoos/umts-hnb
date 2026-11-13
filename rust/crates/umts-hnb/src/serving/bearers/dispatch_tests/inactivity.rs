// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::serving::inactivity::Inactivity;
use umts_types::time::CHIPS_PER_SECOND;

#[test]
fn expired_idle_timer_requests_core_release_without_deleting_an_open_connection() {
    let mut env = environment();
    let mut config = config();
    config.rrc.inactivity_release_seconds = 1;
    config.rrc.inactivity_delete_seconds = 2;
    let mut lifecycle = Inactivity::new(&config).unwrap();
    let mut traffic = traffic(&config);
    let assignments = RabHandoff::new(Limits::default());
    let (key, _) = register(&mut env);
    for frame in 0..400 {
        lifecycle
            .service(
                &mut env.controller,
                &mut traffic,
                &assignments,
                at(frame * CHIPS_PER_SECOND / 100),
            )
            .unwrap();
        let radio = env.controller.radio_mut();
        while radio
            .core()
            .publish_one(|event| {
                assert_eq!(event.key, key);
                assert!(matches!(
                    event.kind,
                    crate::signalling::CoreEventKind::InactivityRelease { .. }
                ));
                Ok::<_, ()>(())
            })
            .unwrap()
            .is_some()
        {}
    }
    assert_eq!(lifecycle.counters.iu_requests, 1);
    assert_eq!(lifecycle.counters.rrc_releases, 0);
    assert_eq!(lifecycle.counters.timed_out, 0);
    assert!(env.controller.radio().phase(key).is_some());
}
