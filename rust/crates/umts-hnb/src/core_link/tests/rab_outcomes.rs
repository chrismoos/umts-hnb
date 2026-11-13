// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_iuh::{
    ranap::rab::{Assignment, AssignmentResponse, Cause, Failed, First, Second, SetupOrModify},
    session::RabOutcome,
};

#[test]
fn control_step_publishes_only_a_complete_canonical_rab_response() {
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (_, context, _) = register(&mut link, &core, &mut radio, &source, 0, 3);
    let assignment = Assignment {
        setup: [5, 6]
            .map(|id| SetupOrModify {
                first: First::modification(id),
                second: Second::default(),
            })
            .into(),
        release: vec![],
    };
    core.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: assignment.encode(Limits::default()).unwrap(),
    });
    let mut request = None;
    let result = link
        .drive(ControlTime(6), &mut radio, |_, event| {
            let Event::RabAssignment { request: value, .. } = event else {
                panic!("expected RAB assignment")
            };
            request = Some(*value);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert!(result.gateway.delivered);
    assert!(!result.rab_published);
    let request = request.unwrap();
    let first = Failed::new(5, Cause::from_code(32).unwrap());
    let second = Failed::new(6, Cause::from_code(32).unwrap());
    link.gateway_mut()
        .record_rab_outcome(
            ControlTime(7),
            request,
            RabOutcome::SetupFailed(second.clone()),
        )
        .unwrap();
    assert!(!drive(&mut link, &mut radio, 7).rab_published);
    link.gateway_mut()
        .record_rab_outcome(
            ControlTime(8),
            request,
            RabOutcome::SetupFailed(first.clone()),
        )
        .unwrap();
    let result = drive(&mut link, &mut radio, 8);
    assert!(result.rab_published);
    assert!(!result.rab_blocked);
    let rua::Message::Direct {
        domain,
        context: returned,
        ranap,
    } = core.last_rua()
    else {
        panic!("expected RAB response")
    };
    assert_eq!((domain, returned), (Domain::Cs, context));
    assert_eq!(
        AssignmentResponse::decode(&ranap, Limits::default())
            .unwrap()
            .0
            .failed,
        [first, second]
    );
    assert!(!drive(&mut link, &mut radio, 9).rab_published);
    assert!(
        link.gateway_mut()
            .record_rab_outcome(
                ControlTime(9),
                request,
                RabOutcome::SetupFailed(Failed::new(5, Cause::from_code(32).unwrap()))
            )
            .is_err()
    );
}
