// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    bridge::{Driver, State},
    proto::hnb_management_server::HnbManagementServer,
    service::{Access, MAXIMUM_MESSAGE_BYTES, MAXIMUM_SUBSCRIPTIONS, Service},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    io::{self, Read},
    net::SocketAddr,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, mpsc},
    task::{Context as TaskContext, Poll},
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, UnixListener},
    sync::Semaphore,
};
use tokio_stream::Stream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig, server::Connected};

const MAXIMUM_CONNECTIONS: usize = 32;
const MAXIMUM_ADMIN_CONNECTIONS: usize = 4;
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(20);
const MAXIMUM_STREAMS_PER_CONNECTION: u32 = 32;
const MAXIMUM_CONCURRENT_REQUESTS: usize = 16;
const MAXIMUM_HEADER_BYTES: u32 = 16 * 1024;
const SOCKET_READ_MODE: u32 = 0o660;
const SOCKET_ADMIN_MODE: u32 = 0o600;
const DRIVER_INTERVAL: Duration = Duration::from_millis(20);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MAXIMUM_CREDENTIAL_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug)]
pub struct Settings {
    pub socket_path: PathBuf,
    pub administrator_socket_path: PathBuf,
    pub tcp_address: Option<SocketAddr>,
    pub tls_certificate: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub tls_client_ca: Option<PathBuf>,
    pub administrator_certificates_sha256: Vec<String>,
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.socket_path.is_absolute() && self.administrator_socket_path.is_absolute(),
            "management sockets must be absolute paths"
        );
        ensure!(
            self.socket_path != self.administrator_socket_path,
            "read and administrator sockets must differ"
        );
        ensure!(
            self.administrator_certificates_sha256.len() <= 32,
            "too many administrator certificates"
        );
        for fingerprint in &self.administrator_certificates_sha256 {
            ensure!(
                fingerprint.len() == 64
                    && fingerprint
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "administrator fingerprints must be lowercase SHA256 hex"
            );
        }
        let tls = [
            self.tls_certificate.as_ref(),
            self.tls_key.as_ref(),
            self.tls_client_ca.as_ref(),
        ];
        if self.tcp_address.is_some() {
            ensure!(
                tls.iter()
                    .all(|path| path.is_some_and(|path| path.is_absolute())),
                "TCP management requires absolute server certificate, key and client-CA paths"
            );
        } else {
            ensure!(
                tls.iter().all(|path| path.is_none())
                    && self.administrator_certificates_sha256.is_empty(),
                "TLS configuration requires a TCP listener"
            );
        }
        Ok(())
    }
}

pub struct ManagementServer {
    cancellation: CancellationToken,
    worker: Option<JoinHandle<Result<()>>>,
    tcp_address: Option<SocketAddr>,
}

impl ManagementServer {
    pub fn spawn(settings: Settings, driver: Driver, state: Arc<State>) -> Result<Self> {
        settings.validate()?;
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        let (ready, started) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("hnb-management".into())
            .spawn(move || {
                let run = || -> Result<()> {
                    normal_priority()?;
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    runtime.block_on(run(settings, driver, state, cancel, ready))
                };
                run()
            })
            .context("start management worker")?;
        let mut server = Self {
            cancellation,
            worker: Some(worker),
            tcp_address: None,
        };
        match started.recv_timeout(STARTUP_TIMEOUT) {
            Ok(address) => {
                server.tcp_address = address;
                Ok(server)
            }
            Err(_) => {
                server.cancellation.cancel();
                match server.join() {
                    Ok(()) => bail!("management startup did not become ready"),
                    Err(error) => Err(error),
                }
            }
        }
    }

    pub fn stop(&self) {
        self.cancellation.cancel()
    }

    pub fn tcp_address(&self) -> Option<SocketAddr> {
        self.tcp_address
    }

    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub fn join(&mut self) -> Result<()> {
        self.stop();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("management worker panicked"))??;
        }
        Ok(())
    }
}

impl Drop for ManagementServer {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

pub fn normal_priority() -> Result<()> {
    umts_runtime::scheduling::configure_current(umts_runtime::scheduling::Role::Background)
        .context("configure management thread scheduling")
}

async fn run(
    settings: Settings,
    mut driver: Driver,
    state: Arc<State>,
    cancel: CancellationToken,
    ready: mpsc::SyncSender<Option<SocketAddr>>,
) -> Result<()> {
    let (read, read_guard) = bind_socket(&settings.socket_path, SOCKET_READ_MODE)?;
    let (admin, admin_guard) = bind_socket(&settings.administrator_socket_path, SOCKET_ADMIN_MODE)?;
    let subscriptions = Arc::new(Semaphore::new(MAXIMUM_SUBSCRIPTIONS));
    let connections = Arc::new(Semaphore::new(MAXIMUM_CONNECTIONS));
    let administrator_connections = Arc::new(Semaphore::new(MAXIMUM_ADMIN_CONNECTIONS));
    let (health, health_service) = tonic_health::server::health_reporter();
    health
        .set_service_status("", tonic_health::ServingStatus::Serving)
        .await;
    let read_metadata = std::fs::symlink_metadata(&settings.socket_path)?;
    let admin_metadata = std::fs::symlink_metadata(&settings.administrator_socket_path)?;
    let read_service = service(
        Arc::clone(&state),
        Access::LocalRead {
            uid: read_metadata.uid(),
            gid: read_metadata.gid(),
        },
        Arc::clone(&subscriptions),
        cancel.clone(),
    );
    let admin_service = service(
        Arc::clone(&state),
        Access::LocalAdmin {
            uid: admin_metadata.uid(),
        },
        Arc::clone(&subscriptions),
        cancel.clone(),
    );
    let read_cancel = cancel.clone();
    let admin_cancel = cancel.clone();
    let mut read_server = Box::pin(
        server()
            .add_service(health_service.clone())
            .add_service(read_service)
            .serve_with_incoming_shutdown(
                incoming_unix(read, Arc::clone(&connections)),
                async move { read_cancel.cancelled().await },
            ),
    );
    let mut admin_server = Box::pin(
        server()
            .add_service(health_service.clone())
            .add_service(admin_service)
            .serve_with_incoming_shutdown(
                incoming_unix(admin, administrator_connections),
                async move { admin_cancel.cancelled().await },
            ),
    );
    let mut tcp_address = None;
    let tcp = if let Some(address) = settings.tcp_address {
        let listener = TcpListener::bind(address)
            .await
            .context("bind management TLS listener")?;
        tcp_address = Some(listener.local_addr()?);
        let certificate = read_credential(
            settings
                .tls_certificate
                .as_ref()
                .context("TLS certificate missing")?,
        )
        .context("read management TLS certificate")?;
        let key = read_credential(settings.tls_key.as_ref().context("TLS key missing")?)
            .context("read management TLS key")?;
        let ca = read_credential(settings.tls_client_ca.as_ref().context("TLS CA missing")?)
            .context("read management client CA")?;
        let tls = ServerTlsConfig::new()
            .timeout(TLS_HANDSHAKE_TIMEOUT)
            .identity(Identity::from_pem(certificate, key))
            .client_ca_root(Certificate::from_pem(ca));
        let endpoint = service(
            Arc::clone(&state),
            Access::MutualTls {
                administrator_sha256: settings.administrator_certificates_sha256,
            },
            subscriptions,
            cancel.clone(),
        );
        let tcp_cancel = cancel.clone();
        Some(Box::pin(
            server()
                .tls_config(tls)
                .context("configure management mutual TLS")?
                .add_service(health_service)
                .add_service(endpoint)
                .serve_with_incoming_shutdown(incoming_tcp(listener, connections), async move {
                    tcp_cancel.cancelled().await
                }),
        ))
    } else {
        None
    };
    ready
        .send(tcp_address)
        .map_err(|_| anyhow::anyhow!("management startup owner disappeared"))?;
    let mut tcp_server = tcp;
    let mut timer = tokio::time::interval(DRIVER_INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result = loop {
        tokio::select! {
            _ = cancel.cancelled() => break Ok(()),
            result = &mut read_server => { cancel.cancel(); return result.context("read-only gRPC listener stopped") },
            result = &mut admin_server => { cancel.cancel(); return result.context("administrator gRPC listener stopped") },
            result = async { match &mut tcp_server { Some(server) => server.await, None => std::future::pending().await } } => { cancel.cancel(); return result.context("TLS gRPC listener stopped") },
            _ = timer.tick() => {
                if let Err(error) = driver.poll() { break Err(anyhow::anyhow!("management bridge failed: {error}")) }
                let status = if !driver.controller_closed() && state.snapshot().status.radio_ready { tonic_health::ServingStatus::Serving } else { tonic_health::ServingStatus::NotServing };
                health.set_service_status("umts.hnb.management.v1.HnbManagement",status).await;
            }
        }
    };
    cancel.cancel();
    health
        .set_service_status(
            "umts.hnb.management.v1.HnbManagement",
            tonic_health::ServingStatus::NotServing,
        )
        .await;
    let drain = async {
        let _ = tokio::join!(&mut read_server, &mut admin_server);
        if let Some(server) = &mut tcp_server {
            let _ = server.await;
        }
    };
    let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, drain).await;
    drop((read_guard, admin_guard));
    result
}

fn server() -> Server {
    Server::builder()
        .http2_max_header_list_size(MAXIMUM_HEADER_BYTES)
        .concurrency_limit_per_connection(MAXIMUM_CONCURRENT_REQUESTS)
        .max_concurrent_streams(Some(MAXIMUM_STREAMS_PER_CONNECTION))
        .http2_keepalive_interval(Some(Duration::from_secs(30)))
        .http2_keepalive_timeout(Some(Duration::from_secs(10)))
}

fn service(
    state: Arc<State>,
    access: Access,
    subscriptions: Arc<Semaphore>,
    cancel: CancellationToken,
) -> HnbManagementServer<Service> {
    HnbManagementServer::new(Service::new(state, access, subscriptions).with_cancellation(cancel))
        .max_decoding_message_size(MAXIMUM_MESSAGE_BYTES)
        .max_encoding_message_size(MAXIMUM_MESSAGE_BYTES)
}

struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn read_credential(path: &Path) -> Result<Vec<u8>> {
    ensure!(
        std::fs::metadata(path)?.is_file(),
        "TLS credential must be a regular file"
    );
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAXIMUM_CREDENTIAL_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAXIMUM_CREDENTIAL_BYTES,
        "TLS credential is empty or exceeds its size limit"
    );
    Ok(bytes)
}

fn bind_socket(path: &Path, mode: u32) -> Result<(UnixListener, SocketGuard)> {
    let parent = path.parent().context("socket parent missing")?;
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o750)
        .create(parent)
        .context("create management socket directory")?;
    let parent_metadata = std::fs::symlink_metadata(parent)?;
    ensure!(
        parent_metadata.is_dir() && parent_metadata.permissions().mode() & 0o022 == 0,
        "management socket parent must be a real directory without group/world-write access"
    );
    let listener = UnixListener::bind(path).with_context(|| {
        format!(
            "bind management socket {}. Existing paths are never overwritten",
            path.display()
        )
    })?;
    let metadata = std::fs::symlink_metadata(path)?;
    let guard = SocketGuard {
        path: path.to_path_buf(),
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    ensure!(
        parent_metadata.uid() == metadata.uid(),
        "management socket directory must belong to the daemon owner"
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok((listener, guard))
}

pin_project_lite::pin_project! {
    struct LimitedIo<T> { #[pin] inner:T, _permit:tokio::sync::OwnedSemaphorePermit }
}
impl<T: Connected> Connected for LimitedIo<T> {
    type ConnectInfo = T::ConnectInfo;
    fn connect_info(&self) -> Self::ConnectInfo {
        self.inner.connect_info()
    }
}
impl<T: AsyncRead> AsyncRead for LimitedIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.project().inner.poll_read(cx, buf)
    }
}
impl<T: AsyncWrite> AsyncWrite for LimitedIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.project().inner.poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_shutdown(cx)
    }
}

fn incoming_unix(
    listener: UnixListener,
    connections: Arc<Semaphore>,
) -> impl Stream<Item = io::Result<LimitedIo<tokio::net::UnixStream>>> {
    async_stream::stream! { let mut recovering = false; loop {
        match listener.accept().await {
            Ok((inner,_)) => {
                recovering = false;
                if let Ok(permit) = Arc::clone(&connections).try_acquire_owned() { yield Ok(LimitedIo {inner,_permit:permit}) }
            },
            Err(error) => {
                if terminal_accept_error(&error) { yield Err(error); break; }
                if !recovering { tracing::warn!(%error, "Management Unix accept failed, retrying"); }
                recovering = true;
                tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
            }
        }
    } }
}
fn incoming_tcp(
    listener: TcpListener,
    connections: Arc<Semaphore>,
) -> impl Stream<Item = io::Result<LimitedIo<tokio::net::TcpStream>>> {
    async_stream::stream! { let mut recovering = false; loop {
        match listener.accept().await {
            Ok((inner,_)) => {
                recovering = false;
                if let Ok(permit) = Arc::clone(&connections).try_acquire_owned() { yield Ok(LimitedIo {inner,_permit:permit}) }
            },
            Err(error) => {
                if terminal_accept_error(&error) { yield Err(error); break; }
                if !recovering { tracing::warn!(%error, "Management TCP accept failed, retrying"); }
                recovering = true;
                tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
            }
        }
    } }
}

fn terminal_accept_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK)
    )
}

#[cfg(test)]
mod accept_tests {
    use super::*;

    #[test]
    fn transient_accept_errors_do_not_terminate_the_listener() {
        for code in [
            libc::ECONNABORTED,
            libc::EINTR,
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOMEM,
        ] {
            assert!(!terminal_accept_error(&io::Error::from_raw_os_error(code)));
        }
        for code in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
            assert!(terminal_accept_error(&io::Error::from_raw_os_error(code)));
        }
    }
}
