// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::proto::hnb_management_client::HnbManagementClient;
use anyhow::{Context, Result, ensure};
use hyper_util::rt::TokioIo;
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

const LOCAL_RUNTIME_DIRECTORY: &str = ".runtime";
/// Must match the working directory in `packaging/debian/umts-hnb.service`.
const PACKAGED_RUNTIME_DIRECTORY: &str = "/var/lib/umts-hnb/.runtime";
const READ_SOCKET: &str = "management.sock";
const ADMIN_SOCKET: &str = "admin.sock";

/// Returns the Unix socket endpoint used when no endpoint is given.
///
/// A socket under `.runtime/` in the working directory takes precedence. Otherwise the packaged
/// service's socket is used when it exists or cannot be inspected, so a permission error names
/// the packaged path.
pub fn default_endpoint(administrator: bool) -> String {
    let name = if administrator {
        ADMIN_SOCKET
    } else {
        READ_SOCKET
    };
    let path = select_socket(
        Path::new(LOCAL_RUNTIME_DIRECTORY).join(name),
        Path::new(PACKAGED_RUNTIME_DIRECTORY).join(name),
        present,
    );
    format!("unix://{}", path.display())
}

fn present(path: &Path) -> bool {
    !matches!(
        std::fs::symlink_metadata(path),
        Err(error) if error.kind() == ErrorKind::NotFound
    )
}

fn select_socket(local: PathBuf, packaged: PathBuf, present: impl Fn(&Path) -> bool) -> PathBuf {
    if !present(&local) && present(&packaged) {
        packaged
    } else {
        local
    }
}

pub struct Connection {
    pub endpoint: String,
    pub timeout: Duration,
    pub ca: Option<PathBuf>,
    pub certificate: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub server_name: Option<String>,
}

impl Connection {
    pub async fn connect(&self) -> Result<HnbManagementClient<Channel>> {
        ensure!(
            !self.timeout.is_zero(),
            "connection timeout must be positive"
        );
        let channel = if let Some(path) = self.endpoint.strip_prefix("unix://") {
            ensure!(
                self.ca.is_none()
                    && self.certificate.is_none()
                    && self.key.is_none()
                    && self.server_name.is_none(),
                "TLS options do not apply to Unix sockets"
            );
            ensure!(!path.is_empty(), "Unix socket path must not be empty");
            let path = std::path::absolute(path).context("resolve management socket path")?;
            Endpoint::from_static("http://localhost")
                .connect_timeout(self.timeout)
                .connect_with_connector(tower::service_fn(move |_| {
                    let path = path.clone();
                    async move {
                        tokio::net::UnixStream::connect(path)
                            .await
                            .map(TokioIo::new)
                    }
                }))
                .await
                .context("connect to HNB Unix socket")?
        } else {
            ensure!(
                self.endpoint.starts_with("https://"),
                "TCP management requires https and mutual TLS"
            );
            let ca = self.ca.as_ref().context("--tls-ca required for TCP")?;
            let certificate = self
                .certificate
                .as_ref()
                .context("--tls-cert required for TCP")?;
            let key = self.key.as_ref().context("--tls-key required for TCP")?;
            let mut tls = ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(
                    tokio::fs::read(ca).await.context("read TLS CA")?,
                ))
                .identity(Identity::from_pem(
                    tokio::fs::read(certificate)
                        .await
                        .context("read client TLS certificate")?,
                    tokio::fs::read(key).await.context("read client TLS key")?,
                ));
            if let Some(name) = &self.server_name {
                tls = tls.domain_name(name)
            }
            Endpoint::from_shared(self.endpoint.clone())
                .context("parse management endpoint")?
                .connect_timeout(self.timeout)
                .tls_config(tls)
                .context("configure mutual TLS")?
                .connect()
                .await
                .context("connect to HNB TLS endpoint")?
        };
        Ok(HnbManagementClient::new(channel)
            .max_decoding_message_size(crate::service::MAXIMUM_MESSAGE_BYTES)
            .max_encoding_message_size(crate::service::MAXIMUM_MESSAGE_BYTES))
    }
}

pub fn request<T>(message: T, timeout: Duration) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.set_timeout(timeout);
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select(local_present: bool, packaged_present: bool) -> PathBuf {
        let local = PathBuf::from(".runtime/management.sock");
        let packaged = PathBuf::from("/var/lib/umts-hnb/.runtime/management.sock");
        select_socket(local.clone(), packaged.clone(), |path| {
            if path == local {
                local_present
            } else {
                packaged_present
            }
        })
    }

    #[test]
    fn working_directory_socket_takes_precedence() {
        assert_eq!(select(true, true), Path::new(".runtime/management.sock"));
        assert_eq!(select(true, false), Path::new(".runtime/management.sock"));
    }

    #[test]
    fn packaged_socket_is_used_without_a_local_socket() {
        assert_eq!(
            select(false, true),
            Path::new("/var/lib/umts-hnb/.runtime/management.sock")
        );
    }

    #[test]
    fn missing_sockets_report_the_working_directory_path() {
        assert_eq!(select(false, false), Path::new(".runtime/management.sock"));
    }

    #[test]
    fn default_endpoints_name_the_socket_for_each_role() {
        assert!(default_endpoint(false).ends_with("/management.sock"));
        assert!(default_endpoint(true).ends_with("/admin.sock"));
        assert!(default_endpoint(false).starts_with("unix://"));
    }
}
