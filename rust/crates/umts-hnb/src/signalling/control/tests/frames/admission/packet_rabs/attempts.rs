// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn active_unrecorded() -> (Environment, PacketRabs<Memory>, Core, Ticket, Setup) {
    let (mut env, initial) = environment();
    let radio = mapping(&env, &initial.procedure, env.plan.clone());
    let id = initial
        .procedure
        .rabs()
        .iter()
        .find(|rab| rab.domain == CnDomain::Ps)
        .unwrap()
        .identity;
    let mut core = Core::new(env.key);
    let mut packets = rabs();
    let ticket = core.prepare(&mut packets, &item(id), radio).unwrap();
    start_radio(&mut env, initial);
    complete_radio(&mut env);
    let setup = core
        .complete(&mut packets, ticket, &mut env.receive)
        .unwrap();
    (env, packets, core, ticket, setup)
}

#[test]
fn pending_retirement_is_idempotent_and_rejects_stale_or_foreign_plane_attempts() {
    let (mut env, initial) = environment();
    let id = initial
        .procedure
        .rabs()
        .iter()
        .find(|rab| rab.domain == CnDomain::Ps)
        .unwrap()
        .identity;
    let item = item(id);
    let mut core = Core::new(env.key);
    let mut packets = rabs();
    let first = core
        .prepare(
            &mut packets,
            &item,
            mapping(&env, &initial.procedure, env.plan.clone()),
        )
        .unwrap();
    assert!(packets.retire_attempt(first).unwrap());
    assert!(!packets.retire_attempt(first).unwrap());
    let authority = core
        .session
        .authorize_rab(first.request(), id, RabOperation::SetupOrModify)
        .unwrap();
    let second = packets
        .prepare(
            authority,
            &item,
            mapping(&env, &initial.procedure, env.plan.clone()),
        )
        .unwrap();
    assert_eq!(first.request(), second.request());
    assert_eq!(first.key(), second.key());
    assert_ne!(first, second);
    assert!(!packets.retire_attempt(first).unwrap());
    let mut foreign = rabs();
    let authority = core
        .session
        .authorize_rab(first.request(), id, RabOperation::SetupOrModify)
        .unwrap();
    let foreign_ticket = foreign
        .prepare(
            authority,
            &item,
            mapping(&env, &initial.procedure, env.plan.clone()),
        )
        .unwrap();
    assert!(matches!(
        packets.retire_attempt(foreign_ticket),
        Err(packet_rabs::Error::Identity)
    ));
    assert!(packets.retire_attempt(second).unwrap());
    assert!(foreign.retire_attempt(foreign_ticket).unwrap());
    env.radio.retire_frame_pool(initial.pool).unwrap();
}

#[test]
fn committed_attempt_retirement_removes_only_its_own_active_transport() {
    let (mut env, mut packets, _core, ticket, _) = active_unrecorded();
    assert!(packets.requested(ticket.key()).is_some());
    assert!(packets.retire_attempt(ticket).unwrap());
    assert!(!packets.retire_attempt(ticket).unwrap());
    assert!(packets.requested(ticket.key()).is_none());
    assert!(packets.sequence_state(ticket.key()).is_err());
    assert_eq!(
        packets.step(&mut env.receive).unwrap().transport.delivered,
        0
    );
}

#[test]
fn old_committed_attempt_cannot_retire_a_same_request_same_route_modification() {
    let (mut env, mut packets, core, first, first_setup) = active_unrecorded();
    let modification = SetupOrModify {
        first: First::modification(first.key().rab),
        second: Second::default(),
    };
    let binding = mapping(
        &env,
        env.admission.procedure(env.identity).unwrap(),
        env.plan.clone(),
    );
    let authority = core
        .session
        .authorize_rab(
            first.request(),
            first.key().rab,
            RabOperation::SetupOrModify,
        )
        .unwrap();
    let second = packets.prepare(authority, &modification, binding).unwrap();
    assert_eq!(first.request(), second.request());
    assert_eq!(first.key(), second.key());
    assert_ne!(first, second);
    assert!(matches!(
        packets.retire_attempt(first),
        Err(packet_rabs::Error::Conflict)
    ));
    let second_setup = core
        .complete(&mut packets, second, &mut env.receive)
        .unwrap();
    assert_eq!(local_teid(&first_setup), local_teid(&second_setup));
    assert!(!packets.retire_attempt(first).unwrap());
    assert!(packets.requested(second.key()).is_some());
    let binding = mapping(
        &env,
        env.admission.procedure(env.identity).unwrap(),
        env.plan.clone(),
    );
    let authority = core
        .session
        .authorize_rab(
            second.request(),
            second.key().rab,
            RabOperation::SetupOrModify,
        )
        .unwrap();
    let third = packets.prepare(authority, &modification, binding).unwrap();
    assert!(matches!(
        packets.retire_attempt(second),
        Err(packet_rabs::Error::Conflict)
    ));
    assert!(packets.retire_attempt(third).unwrap());
    assert!(packets.requested(second.key()).is_some());
    assert_eq!(
        packets.sequence_state(second.key()).unwrap().next_uplink,
        u16::MAX
    );
    assert!(packets.retire_attempt(second).unwrap());
    assert!(!packets.retire_attempt(third).unwrap());
    assert!(packets.requested(second.key()).is_none());
}
