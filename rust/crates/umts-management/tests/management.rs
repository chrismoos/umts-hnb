// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{sync::Arc, time::Duration};
use tonic::Code;
use umts_management::{
    bridge::{self, Action, Completion, Snapshot},
    client::{self, Connection},
    proto::{self, get_operation_request::Selector},
    server::{ManagementServer, Settings},
};

fn initial() -> Snapshot {
    Snapshot {
        status: proto::StatusSnapshot {
            identity: Some(proto::SnapshotIdentity {
                epoch: 42,
                ..Default::default()
            }),
            phase: proto::Phase::Running.into(),
            radio_ready: true,
            core_connected: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn private_directory() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

fn identity(state: &bridge::State) -> proto::CommandIdentity {
    proto::CommandIdentity {
        request_id: uuid::Uuid::new_v4().to_string(),
        expected_instance_id: state
            .snapshot()
            .status
            .identity
            .as_ref()
            .unwrap()
            .instance_id
            .clone(),
        expected_epoch: 42,
    }
}

#[test]
fn idempotency_does_not_reenqueue_and_never_accepts_changed_parameters() {
    let (mut controller, mut driver, state) = bridge::channel(initial());
    let request = identity(&state);
    let first = state
        .submit(request.clone(), Action::Shutdown, Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        state
            .submit(request.clone(), Action::Shutdown, Duration::from_secs(1))
            .unwrap(),
        first
    );
    assert_eq!(
        state
            .submit(
                request.clone(),
                Action::StopCapture {
                    capture_id: uuid::Uuid::new_v4().to_string()
                },
                Duration::from_secs(1)
            )
            .unwrap_err()
            .code(),
        Code::AlreadyExists
    );
    let command = controller.try_next().unwrap();
    assert_eq!(command.operation_id, first.id);
    assert!(controller.try_next().is_none());
    assert!(
        controller
            .try_complete(Completion::Finished {
                operation_id: first.id.clone(),
                result: Ok(())
            })
            .is_ok()
    );
    driver.poll().unwrap();
    let done = state
        .operation(&Selector::RequestId(request.request_id))
        .unwrap();
    assert_eq!(done.phase, i32::from(proto::OperationPhase::Succeeded));
}

#[test]
fn stale_instance_epoch_and_zero_lifetime_never_publish() {
    let (mut controller, _driver, state) = bridge::channel(initial());
    let mut id = identity(&state);
    id.expected_instance_id = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        state
            .submit(id, Action::Shutdown, Duration::from_secs(1))
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    let mut id = identity(&state);
    id.expected_epoch = 41;
    assert_eq!(
        state
            .submit(id, Action::Shutdown, Duration::from_secs(1))
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );
    assert_eq!(
        state
            .submit(identity(&state), Action::Shutdown, Duration::ZERO)
            .unwrap_err()
            .code(),
        Code::DeadlineExceeded
    );
    assert!(controller.try_next().is_none());
}

#[test]
fn saturation_is_explicit_and_does_not_create_an_orphan_operation() {
    let (mut controller, _driver, state) = bridge::channel(initial());
    for _ in 0..bridge::COMMAND_CAPACITY {
        state
            .submit(identity(&state), Action::Shutdown, Duration::from_secs(1))
            .unwrap();
    }
    let refused = identity(&state);
    assert_eq!(
        state
            .submit(refused.clone(), Action::Shutdown, Duration::from_secs(1))
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    assert_eq!(
        state
            .operation(&Selector::RequestId(refused.request_id))
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    for _ in 0..bridge::COMMAND_CAPACITY {
        assert!(controller.try_next().is_some());
    }
    assert!(controller.try_next().is_none());
}

#[test]
fn slow_event_consumers_receive_explicit_history_gap() {
    let (mut controller, mut driver, state) = bridge::channel(initial());
    for index in 0..bridge::MAXIMUM_EVENTS + 10 {
        let mut snapshot = initial();
        snapshot.status.phase = if index % 2 == 0 {
            proto::Phase::Starting.into()
        } else {
            proto::Phase::Running.into()
        };
        controller.publish(snapshot).unwrap();
        driver.poll().unwrap();
    }
    let events = state.events_after(1).unwrap();
    assert_eq!(events.first().unwrap().kind, "stream.gap");
    assert!(events.first().unwrap().lost_events > 0);
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
    assert_eq!(
        state.events_after(u64::MAX).unwrap_err().code(),
        Code::OutOfRange
    );
}

fn settings(root: &std::path::Path) -> Settings {
    Settings {
        socket_path: root.join("read.sock"),
        administrator_socket_path: root.join("admin.sock"),
        tcp_address: None,
        tls_certificate: None,
        tls_key: None,
        tls_client_ca: None,
        administrator_certificates_sha256: Vec::new(),
    }
}

fn connection(path: &std::path::Path) -> Connection {
    Connection {
        endpoint: format!("unix://{}", path.display()),
        timeout: Duration::from_secs(2),
        ca: None,
        certificate: None,
        key: None,
        server_name: None,
    }
}

#[tokio::test]
async fn saturated_read_connections_do_not_exhaust_administrator_capacity() {
    let directory = private_directory();
    let settings = settings(directory.path());
    let (_controller, driver, state) = bridge::channel(initial());
    let mut server = ManagementServer::spawn(settings.clone(), driver, state).unwrap();
    let mut readers = Vec::new();
    for _ in 0..32 {
        let mut client = connection(&settings.socket_path).connect().await.unwrap();
        client.get_status(proto::GetStatusRequest {}).await.unwrap();
        readers.push(client);
    }
    let mut admin = connection(&settings.administrator_socket_path)
        .connect()
        .await
        .unwrap();
    assert!(
        admin
            .get_status(proto::GetStatusRequest {})
            .await
            .unwrap()
            .into_inner()
            .radio_ready
    );
    drop(readers);
    drop(admin);
    server.join().unwrap();
}

#[tokio::test]
async fn real_unix_grpc_enforces_read_only_and_delivers_owned_command_results() {
    let directory = private_directory();
    let settings = settings(directory.path());
    let (mut controller, driver, state) = bridge::channel(initial());
    let mut server = ManagementServer::spawn(settings.clone(), driver, Arc::clone(&state)).unwrap();
    let mut read = connection(&settings.socket_path).connect().await.unwrap();
    let status = read
        .get_status(client::request(
            proto::GetStatusRequest {},
            Duration::from_secs(1),
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(status.radio_ready);
    let command = proto::ShutdownRequest {
        command: Some(identity(&state)),
    };
    assert_eq!(
        read.shutdown(client::request(command.clone(), Duration::from_secs(1)))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    let mut admin = connection(&settings.administrator_socket_path)
        .connect()
        .await
        .unwrap();
    let operation = admin
        .shutdown(client::request(command, Duration::from_secs(1)))
        .await
        .unwrap()
        .into_inner();
    let command = controller.try_next().unwrap();
    assert_eq!(command.operation_id, operation.id);
    assert!(
        controller
            .try_complete(Completion::Finished {
                operation_id: operation.id.clone(),
                result: Err((Code::FailedPrecondition, "test refusal".into()))
            })
            .is_ok()
    );
    let mut completed = None;
    for _ in 0..20 {
        let value = read
            .get_operation(client::request(
                proto::GetOperationRequest {
                    selector: Some(Selector::OperationId(operation.id.clone())),
                },
                Duration::from_secs(1),
            ))
            .await
            .unwrap()
            .into_inner();
        if value.phase == i32::from(proto::OperationPhase::Failed) {
            completed = Some(value);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        completed.unwrap().error_code,
        i32::from(Code::FailedPrecondition)
    );
    server.join().unwrap();
    assert!(!settings.socket_path.exists() && !settings.administrator_socket_path.exists());
}

#[tokio::test]
async fn real_grpc_pagination_is_snapshot_bound_and_streams_release_capacity() {
    let directory = private_directory();
    let settings = settings(directory.path());
    let mut snapshot = initial();
    for slot in 0..3 {
        snapshot.ues.push(proto::Ue {
            reference: Some(proto::UeRef {
                epoch: 42,
                slot,
                generation: 1,
            }),
            ..Default::default()
        })
    }
    let (mut controller, driver, state) = bridge::channel(snapshot.clone());
    let mut server = ManagementServer::spawn(settings.clone(), driver, state).unwrap();
    let mut read = connection(&settings.socket_path).connect().await.unwrap();
    let page = read
        .list_ues(proto::ListUesRequest {
            page_size: 1,
            page_token: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.ues.len(), 1);
    controller.publish(snapshot).unwrap();
    assert_eq!(
        read.list_ues(proto::ListUesRequest {
            page_size: 1,
            page_token: page.next_page_token
        })
        .await
        .unwrap_err()
        .code(),
        Code::Aborted
    );
    let mut streams = Vec::new();
    for _ in 0..umts_management::service::MAXIMUM_SUBSCRIPTIONS {
        let mut stream = read
            .watch_metrics(proto::WatchMetricsRequest { interval_ms: 1000 })
            .await
            .unwrap()
            .into_inner();
        assert!(stream.message().await.unwrap().is_some());
        streams.push(stream);
    }
    assert_eq!(
        read.watch_metrics(proto::WatchMetricsRequest { interval_ms: 1000 })
            .await
            .unwrap_err()
            .code(),
        Code::ResourceExhausted
    );
    drop(streams);
    let mut reopened = false;
    for _ in 0..20 {
        match read
            .watch_metrics(proto::WatchMetricsRequest { interval_ms: 1000 })
            .await
        {
            Ok(response) => {
                drop(response);
                reopened = true;
                break;
            }
            Err(status) if status.code() == Code::ResourceExhausted => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            Err(error) => panic!("unexpected stream reopen error: {error}"),
        }
    }
    assert!(
        reopened,
        "canceled streams must return subscription capacity"
    );
    server.join().unwrap();
}

#[test]
fn socket_startup_never_removes_an_existing_file_and_unwinds_partial_bind() {
    let directory = private_directory();
    let settings = settings(directory.path());
    std::fs::write(&settings.administrator_socket_path, b"retain me").unwrap();
    let (_controller, driver, state) = bridge::channel(initial());
    assert!(ManagementServer::spawn(settings.clone(), driver, state).is_err());
    assert_eq!(
        std::fs::read(&settings.administrator_socket_path).unwrap(),
        b"retain me"
    );
    assert!(!settings.socket_path.exists());
}

#[test]
fn controller_close_retains_existing_idempotency_but_rejects_new_commands() {
    let (mut controller, mut driver, state) = bridge::channel(initial());
    let request = identity(&state);
    let operation = state
        .submit(request.clone(), Action::Shutdown, Duration::from_secs(1))
        .unwrap();
    controller.close_commands();
    assert_eq!(
        state
            .submit(request, Action::Shutdown, Duration::from_secs(1))
            .unwrap()
            .id,
        operation.id
    );
    assert_eq!(
        state
            .submit(identity(&state), Action::Shutdown, Duration::from_secs(1))
            .unwrap_err()
            .code(),
        Code::Unavailable
    );
    let command = controller.try_next().unwrap();
    assert!(
        controller
            .try_complete(Completion::Finished {
                operation_id: command.operation_id,
                result: Ok(())
            })
            .is_ok()
    );
    driver.poll().unwrap();
    assert_eq!(
        state
            .operation(&Selector::OperationId(operation.id))
            .unwrap()
            .phase,
        i32::from(proto::OperationPhase::Succeeded)
    );
}

#[test]
fn state_events_ignore_measurement_only_changes() {
    let mut snapshot = initial();
    snapshot.ues.push(proto::Ue {
        reference: Some(proto::UeRef {
            epoch: 42,
            slot: 0,
            generation: 1,
        }),
        cs_state: proto::DomainState::Connected.into(),
        ..Default::default()
    });
    let (mut controller, mut driver, state) = bridge::channel(snapshot.clone());
    driver.poll().unwrap();
    let cursor = state.event_tail().unwrap();
    snapshot.ues[0].power = Some(proto::PowerControl {
        sir_db: Some(9.0),
        ..Default::default()
    });
    controller.publish(snapshot.clone()).unwrap();
    driver.poll().unwrap();
    assert_eq!(state.event_tail().unwrap(), cursor);
    snapshot.ues[0].ps_state = proto::DomainState::Connected.into();
    controller.publish(snapshot).unwrap();
    driver.poll().unwrap();
    let events = state.events_after(cursor).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "ue.state");
    assert_eq!(events[0].ue.as_ref().unwrap().generation, 1);
}

#[test]
fn standalone_cli_uses_real_rpc_json_authorization_and_request_identity() {
    let directory = private_directory();
    let settings = settings(directory.path());
    let (mut controller, driver, state) = bridge::channel(initial());
    let mut server = ManagementServer::spawn(settings.clone(), driver, Arc::clone(&state)).unwrap();
    let invoke = |path: &std::path::Path, args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_umts-hnbctl"))
            .args([
                "--endpoint",
                &format!("unix://{}", path.display()),
                "--timeout-ms",
                "2000",
                "--json",
            ])
            .args(args)
            .output()
            .unwrap()
    };
    let status = invoke(&settings.socket_path, &["status"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["identity"]["epoch"], "42");
    let denied = invoke(&settings.socket_path, &["shutdown"]);
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr)
            .to_ascii_lowercase()
            .contains("permission")
    );
    assert!(controller.try_next().is_none());
    let request_id = "d111d7af-8106-4d3e-b500-3fe2b27a0199";
    let queued = invoke(
        &settings.administrator_socket_path,
        &["--request-id", request_id, "shutdown"],
    );
    assert!(
        queued.status.success(),
        "{}",
        String::from_utf8_lossy(&queued.stderr)
    );
    assert!(String::from_utf8_lossy(&queued.stderr).contains(request_id));
    let command = controller.try_next().unwrap();
    assert!(matches!(command.action, Action::Shutdown));
    assert!(controller.try_next().is_none());
    let outcome = invoke(
        &settings.socket_path,
        &["operation", "--request-id", request_id],
    );
    assert!(
        outcome.status.success(),
        "{}",
        String::from_utf8_lossy(&outcome.stderr)
    );
    let outcome: serde_json::Value = serde_json::from_slice(&outcome.stdout).unwrap();
    assert_eq!(outcome["id"], command.operation_id);
    assert_eq!(outcome["requestId"], request_id);
    let prefix = invoke(
        &settings.socket_path,
        &["--request-id", request_id, "operation"],
    );
    assert!(
        prefix.status.success(),
        "{}",
        String::from_utf8_lossy(&prefix.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&prefix.stdout).unwrap(),
        outcome
    );
    server.join().unwrap();
}

#[test]
fn controller_loss_fails_unresolved_operations_and_reports_not_ready() {
    let (controller, mut driver, state) = bridge::channel(initial());
    let operation = state
        .submit(identity(&state), Action::Shutdown, Duration::from_secs(1))
        .unwrap();
    drop(controller);
    driver.poll().unwrap();
    let outcome = state
        .operation(&Selector::OperationId(operation.id))
        .unwrap();
    assert_eq!(outcome.phase, i32::from(proto::OperationPhase::Failed));
    assert_eq!(outcome.error_code, i32::from(Code::Unavailable));
    let status = state.status().unwrap();
    assert_eq!(status.phase, i32::from(proto::Phase::Failed));
    assert!(!status.radio_ready);
}

#[tokio::test]
async fn relative_unix_endpoints_resolve_against_the_launch_directory() {
    use std::os::unix::fs::PermissionsExt;
    let cwd = std::env::current_dir().unwrap();
    let directory = tempfile::Builder::new()
        .prefix(".hnb-")
        .tempdir_in(&cwd)
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let settings = settings(directory.path());
    let (_controller, driver, state) = bridge::channel(initial());
    let mut server = ManagementServer::spawn(settings.clone(), driver, state).unwrap();
    for path in [&settings.socket_path, &settings.administrator_socket_path] {
        let relative = path.strip_prefix(&cwd).unwrap();
        let mut client = connection(relative).connect().await.unwrap();
        assert!(
            client
                .get_status(proto::GetStatusRequest {})
                .await
                .unwrap()
                .into_inner()
                .radio_ready
        );
    }
    server.join().unwrap();
}
