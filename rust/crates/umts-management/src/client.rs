// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::proto::hnb_management_client::HnbManagementClient;
use anyhow::{Context, Result, ensure};
use hyper_util::rt::TokioIo;
use std::{path::PathBuf, time::Duration};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

pub const READ_SOCKET: &str = ".runtime/management.sock";
pub const ADMIN_SOCKET: &str = ".runtime/admin.sock";

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
