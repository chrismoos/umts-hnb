// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn feedback_is_ordered_bounded_and_cannot_adjust_a_peer_or_a_stale_configuration() {
    let (_source, mut runtime, mut ports) = runtime(2, 1);
    for slot in 0..2 {
        let mut session = owner(slot, 1);
        session
            .configure_downlink_power(DownlinkPowerSettings {
                initial_offset_db: -6.0,
                minimum_offset_db: -20.0,
                maximum_age_chips: CHIPS_PER_FRAME,
            })
            .unwrap();
        install(&mut runtime, &mut ports, session);
        runtime.bank.session_mut(identity(slot, 1)).unwrap().phase = TransmitPhase::Armed;
    }
    runtime.bank.next = at(CHIPS_PER_SLOT * 3);
    let observation = DownlinkControl {
        identity: identity(0, 1),
        measurement_end: at(CHIPS_PER_SLOT),
        command: DownlinkCommand::Down,
    };
    ports
        .feedback
        .publish_downlink_control(observation)
        .unwrap();
    ports
        .feedback
        .publish_downlink_control(DownlinkControl {
            measurement_end: at(2 * CHIPS_PER_SLOT),
            ..observation
        })
        .unwrap();
    assert!(
        ports
            .feedback
            .publish_downlink_control(observation)
            .is_err()
    );
    assert_eq!(runtime.service().unwrap(), 1);
    assert_eq!(
        runtime
            .bank
            .session(identity(0, 1))
            .unwrap()
            .counters()
            .downlink_power
            .attenuation_db,
        1
    );
    assert_eq!(runtime.service().unwrap(), 1);
    assert_eq!(
        runtime
            .bank
            .session(identity(0, 1))
            .unwrap()
            .counters()
            .downlink_power
            .attenuation_db,
        2
    );
    assert_eq!(
        runtime
            .bank
            .session(identity(1, 1))
            .unwrap()
            .counters()
            .downlink_power
            .attenuation_db,
        0
    );
    ports
        .feedback
        .publish_downlink_control(DownlinkControl {
            identity: identity(0, 2),
            ..observation
        })
        .unwrap();
    runtime.service().unwrap();
    assert_eq!(
        runtime
            .bank
            .session(identity(0, 1))
            .unwrap()
            .counters()
            .downlink_power
            .attenuation_db,
        2
    );
    assert_eq!(ports.feedback.outstanding(), 0);
}
