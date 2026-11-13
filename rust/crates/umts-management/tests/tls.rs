// SPDX-License-Identifier: AGPL-3.0-or-later

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer,
    KeyPair, KeyUsagePurpose,
};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tonic::{
    Code,
    transport::{Certificate as TlsCertificate, ClientTlsConfig, Endpoint},
};
use umts_management::{
    bridge::{self, Action, Snapshot},
    client::{self, Connection},
    proto,
    server::{ManagementServer, Settings},
};

const RPC_TIMEOUT: Duration = Duration::from_secs(2);

fn authority() -> (Certificate, Issuer<'static, KeyPair>) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let certificate = params.self_signed(&key).unwrap();
    (certificate, Issuer::new(params, key))
}

struct Identity {
    certificate: PathBuf,
    key: PathBuf,
    fingerprint: String,
}

fn identity(
    root: &Path,
    name: &str,
    issuer: &Issuer<'_, KeyPair>,
    purpose: ExtendedKeyUsagePurpose,
) -> Identity {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec![name.to_owned()]).unwrap();
    params.extended_key_usages = vec![purpose];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let certificate = params.signed_by(&key, issuer).unwrap();
    let fingerprint = format!("{:x}", Sha256::digest(certificate.der()));
    let certificate_path = root.join(format!("{name}.pem"));
    let key_path = root.join(format!("{name}.key"));
    std::fs::write(&certificate_path, certificate.pem()).unwrap();
    std::fs::write(&key_path, key.serialize_pem()).unwrap();
    Identity {
        certificate: certificate_path,
        key: key_path,
        fingerprint,
    }
}

fn connection(address: std::net::SocketAddr, ca: &Path, identity: &Identity) -> Connection {
    Connection {
        endpoint: format!("https://{address}"),
        timeout: RPC_TIMEOUT,
        ca: Some(ca.to_owned()),
        certificate: Some(identity.certificate.clone()),
        key: Some(identity.key.clone()),
        server_name: Some("localhost".into()),
    }
}

async fn authorized(connection: &Connection) -> bool {
    match connection.connect().await {
        Ok(mut client) => client
            .get_status(client::request(proto::GetStatusRequest {}, RPC_TIMEOUT))
            .await
            .is_ok(),
        Err(_) => false,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn mutual_tls_checks_chain_server_name_and_administrator_role() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let (ca, issuer) = authority();
    let ca_path = root.path().join("ca.pem");
    std::fs::write(&ca_path, ca.pem()).unwrap();
    let server_identity = identity(
        root.path(),
        "localhost",
        &issuer,
        ExtendedKeyUsagePurpose::ServerAuth,
    );
    let reader = identity(
        root.path(),
        "reader",
        &issuer,
        ExtendedKeyUsagePurpose::ClientAuth,
    );
    let administrator = identity(
        root.path(),
        "administrator",
        &issuer,
        ExtendedKeyUsagePurpose::ClientAuth,
    );
    let (_, other_issuer) = authority();
    let stranger = identity(
        root.path(),
        "stranger",
        &other_issuer,
        ExtendedKeyUsagePurpose::ClientAuth,
    );
    let initial = Snapshot {
        status: proto::StatusSnapshot {
            identity: Some(proto::SnapshotIdentity {
                epoch: 42,
                ..Default::default()
            }),
            phase: proto::Phase::Running.into(),
            radio_ready: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let (mut controller, driver, state) = bridge::channel(initial);
    let mut server = ManagementServer::spawn(
        Settings {
            socket_path: root.path().join("read.sock"),
            administrator_socket_path: root.path().join("admin.sock"),
            tcp_address: Some("127.0.0.1:0".parse().unwrap()),
            tls_certificate: Some(server_identity.certificate),
            tls_key: Some(server_identity.key),
            tls_client_ca: Some(ca_path.clone()),
            administrator_certificates_sha256: vec![administrator.fingerprint.clone()],
        },
        driver,
        state.clone(),
    )
    .unwrap();
    let address = server.tcp_address().unwrap();
    assert_ne!(address.port(), 0);
    let read_connection = connection(address, &ca_path, &reader);
    let mut read = read_connection.connect().await.unwrap();
    let status = read
        .get_status(client::request(proto::GetStatusRequest {}, RPC_TIMEOUT))
        .await
        .unwrap()
        .into_inner();
    let command = proto::CommandIdentity {
        request_id: uuid::Uuid::new_v4().to_string(),
        expected_instance_id: status.identity.unwrap().instance_id,
        expected_epoch: 42,
    };
    assert_eq!(
        read.shutdown(client::request(
            proto::ShutdownRequest {
                command: Some(command.clone())
            },
            RPC_TIMEOUT
        ))
        .await
        .unwrap_err()
        .code(),
        Code::PermissionDenied
    );
    assert!(controller.try_next().is_none());
    let mut admin = connection(address, &ca_path, &administrator)
        .connect()
        .await
        .unwrap();
    admin
        .shutdown(client::request(
            proto::ShutdownRequest {
                command: Some(command),
            },
            RPC_TIMEOUT,
        ))
        .await
        .unwrap();
    assert!(matches!(
        controller.try_next().unwrap().action,
        Action::Shutdown
    ));
    assert!(controller.try_next().is_none());
    assert!(!authorized(&connection(address, &ca_path, &stranger)).await);
    let mut wrong_name = read_connection;
    wrong_name.server_name = Some("not-the-hnb.invalid".into());
    assert!(!authorized(&wrong_name).await);
    let bare = Endpoint::from_shared(format!("https://{address}"))
        .unwrap()
        .connect_timeout(RPC_TIMEOUT)
        .tls_config(
            ClientTlsConfig::new()
                .domain_name("localhost")
                .ca_certificate(TlsCertificate::from_pem(ca.pem())),
        )
        .unwrap()
        .connect()
        .await;
    if let Ok(channel) = bare {
        let mut bare = proto::hnb_management_client::HnbManagementClient::new(channel);
        assert!(
            bare.get_status(client::request(proto::GetStatusRequest {}, RPC_TIMEOUT))
                .await
                .is_err()
        );
    }
    let retry = std::process::Command::new(env!("CARGO_BIN_EXE_umts-hnbctl"))
        .args([
            "--endpoint",
            &format!("https://{address}"),
            "--tls-ca",
            ca_path.to_str().unwrap(),
            "--tls-cert",
            reader.certificate.to_str().unwrap(),
            "--tls-key",
            reader.key.to_str().unwrap(),
            "--server-name",
            "localhost",
            "--json",
            "status",
        ])
        .output()
        .unwrap();
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let output: serde_json::Value = serde_json::from_slice(&retry.stdout).unwrap();
    assert_eq!(output["identity"]["epoch"], "42");
    server.join().unwrap();
}

#[test]
fn oversized_tls_input_unwinds_listeners_without_reading_unbounded_credentials() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = root.path().join("oversized.pem");
    std::fs::write(&path, vec![b'x'; 256 * 1024 + 1]).unwrap();
    let (_controller, driver, state) = bridge::channel(Snapshot::default());
    let read = root.path().join("read.sock");
    let admin = root.path().join("admin.sock");
    assert!(
        ManagementServer::spawn(
            Settings {
                socket_path: read.clone(),
                administrator_socket_path: admin.clone(),
                tcp_address: Some("127.0.0.1:0".parse().unwrap()),
                tls_certificate: Some(path.clone()),
                tls_key: Some(path.clone()),
                tls_client_ca: Some(path),
                administrator_certificates_sha256: vec![],
            },
            driver,
            state
        )
        .is_err()
    );
    assert!(!read.exists());
    assert!(!admin.exists());
}
