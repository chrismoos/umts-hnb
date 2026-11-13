// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    bridge::{self, Action, State},
    proto::{self, hnb_management_server::HnbManagement},
};
use sha2::{Digest, Sha256};
use std::{pin::Pin, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tokio_stream::Stream;
use tonic::{
    Request, Response, Status,
    transport::server::{TcpConnectInfo, TlsConnectInfo, UdsConnectInfo},
};

pub const MAXIMUM_SUBSCRIPTIONS: usize = 16;
pub const MAXIMUM_MESSAGE_BYTES: usize = 1024 * 1024;
const MINIMUM_METRICS_INTERVAL_MS: u32 = 1_000;
const MAXIMUM_METRICS_INTERVAL_MS: u32 = 60_000;

#[derive(Clone)]
pub enum Access {
    LocalRead { uid: u32, gid: u32 },
    LocalAdmin { uid: u32 },
    MutualTls { administrator_sha256: Vec<String> },
}

#[derive(Clone)]
pub struct Service {
    state: Arc<State>,
    access: Access,
    subscriptions: Arc<Semaphore>,
    cancellation: tokio_util::sync::CancellationToken,
}

impl Service {
    pub fn new(state: Arc<State>, access: Access, subscriptions: Arc<Semaphore>) -> Self {
        Self {
            state,
            access,
            subscriptions,
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub fn with_cancellation(mut self, cancellation: tokio_util::sync::CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    fn authorize<T>(&self, request: &Request<T>, mutation: bool) -> Result<(), Status> {
        let administrator = match &self.access {
            Access::LocalRead { uid, gid } => {
                let credentials = request
                    .extensions()
                    .get::<UdsConnectInfo>()
                    .and_then(|info| info.peer_cred.as_ref())
                    .ok_or_else(|| Status::unauthenticated("Unix peer credentials required"))?;
                if credentials.uid() != *uid && credentials.gid() != *gid {
                    return Err(Status::permission_denied("Unix peer is not authorized"));
                }
                false
            }
            Access::LocalAdmin { uid } => {
                let credentials = request
                    .extensions()
                    .get::<UdsConnectInfo>()
                    .and_then(|info| info.peer_cred.as_ref())
                    .ok_or_else(|| Status::unauthenticated("Unix peer credentials required"))?;
                if credentials.uid() != *uid {
                    return Err(Status::permission_denied(
                        "administrator socket requires daemon owner",
                    ));
                }
                true
            }
            Access::MutualTls {
                administrator_sha256,
            } => {
                let certificates = request
                    .extensions()
                    .get::<TlsConnectInfo<TcpConnectInfo>>()
                    .and_then(TlsConnectInfo::peer_certs)
                    .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
                let certificate = certificates
                    .first()
                    .ok_or_else(|| Status::unauthenticated("client certificate missing"))?;
                let fingerprint = Sha256::digest(certificate.as_ref())
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                administrator_sha256.contains(&fingerprint)
            }
        };
        if mutation && !administrator {
            return Err(Status::permission_denied("administrator access required"));
        }
        Ok(())
    }

    fn submit(
        &self,
        identity: Option<proto::CommandIdentity>,
        action: Action,
        lifetime: Duration,
    ) -> Result<Response<proto::Operation>, Status> {
        let identity =
            identity.ok_or_else(|| Status::invalid_argument("command identity required"))?;
        Ok(Response::new(
            self.state.submit(identity, action, lifetime)?,
        ))
    }
}

type ResponseStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl HnbManagement for Service {
    async fn get_status(
        &self,
        request: Request<proto::GetStatusRequest>,
    ) -> Result<Response<proto::StatusSnapshot>, Status> {
        self.authorize(&request, false)?;
        Ok(Response::new(self.state.status()?))
    }

    async fn get_configuration(
        &self,
        request: Request<proto::GetConfigurationRequest>,
    ) -> Result<Response<proto::Configuration>, Status> {
        self.authorize(&request, false)?;
        Ok(Response::new(self.state.snapshot().configuration.clone()))
    }

    async fn get_capabilities(
        &self,
        request: Request<proto::GetCapabilitiesRequest>,
    ) -> Result<Response<proto::Capabilities>, Status> {
        self.authorize(&request, false)?;
        Ok(Response::new(proto::Capabilities {
            api_version: "umts.hnb.management.v1".into(),
            features: [
                "status",
                "configuration",
                "ues",
                "metrics",
                "events",
                "capture.rx",
                "capture.tx",
                "operations",
                "shutdown",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            maximum_page_size: bridge::MAXIMUM_PAGE_SIZE as u32,
            maximum_capture_mib: bridge::MAXIMUM_CAPTURE_MIB,
            maximum_capture_duration_ms: bridge::MAXIMUM_CAPTURE_DURATION_MS,
            maximum_subscriptions: MAXIMUM_SUBSCRIPTIONS as u32,
            operation_retention_seconds: bridge::OPERATION_RETENTION.as_secs() as u32,
        }))
    }

    async fn list_ues(
        &self,
        request: Request<proto::ListUesRequest>,
    ) -> Result<Response<proto::ListUesResponse>, Status> {
        self.authorize(&request, false)?;
        let request = request.into_inner();
        let page_size = if request.page_size == 0 {
            bridge::MAXIMUM_PAGE_SIZE
        } else {
            request.page_size as usize
        };
        if page_size > bridge::MAXIMUM_PAGE_SIZE {
            return Err(Status::invalid_argument("page_size exceeds maximum"));
        }
        let snapshot = self.state.snapshot();
        let identity = snapshot
            .status
            .identity
            .as_ref()
            .ok_or_else(|| Status::unavailable("snapshot not published"))?;
        let offset = if request.page_token.is_empty() {
            0
        } else {
            if request.page_token.len() > 128 {
                return Err(Status::invalid_argument("page token too long"));
            }
            let pieces = request.page_token.split(':').collect::<Vec<_>>();
            if pieces.len() != 3 {
                return Err(Status::invalid_argument("invalid page token"));
            }
            let sequence = pieces[1]
                .parse::<u64>()
                .map_err(|_| Status::invalid_argument("invalid page sequence"))?;
            if pieces[0] != identity.instance_id || sequence != identity.sequence {
                return Err(Status::aborted("snapshot changed, restart pagination"));
            }
            pieces[2]
                .parse::<usize>()
                .map_err(|_| Status::invalid_argument("invalid page offset"))?
        };
        if offset > snapshot.ues.len() {
            return Err(Status::invalid_argument("page offset out of range"));
        }
        let end = offset.saturating_add(page_size).min(snapshot.ues.len());
        let next_page_token = if end < snapshot.ues.len() {
            format!("{}:{}:{end}", identity.instance_id, identity.sequence)
        } else {
            String::new()
        };
        Ok(Response::new(proto::ListUesResponse {
            identity: Some(identity.clone()),
            ues: snapshot.ues[offset..end].to_vec(),
            next_page_token,
        }))
    }

    async fn get_ue(
        &self,
        request: Request<proto::GetUeRequest>,
    ) -> Result<Response<proto::Ue>, Status> {
        self.authorize(&request, false)?;
        let reference = request
            .into_inner()
            .reference
            .ok_or_else(|| Status::invalid_argument("full UE reference required"))?;
        if reference.epoch == 0 || reference.generation == 0 {
            return Err(Status::invalid_argument("invalid UE generation"));
        }
        self.state
            .snapshot()
            .ues
            .iter()
            .find(|ue| ue.reference.as_ref() == Some(&reference))
            .cloned()
            .map(Response::new)
            .ok_or_else(|| Status::not_found("UE context not active"))
    }

    async fn get_radio_metrics(
        &self,
        request: Request<proto::GetRadioMetricsRequest>,
    ) -> Result<Response<proto::RadioMetrics>, Status> {
        self.authorize(&request, false)?;
        Ok(Response::new(self.state.snapshot().metrics()))
    }

    type WatchMetricsStream = ResponseStream<proto::MetricsUpdate>;
    async fn watch_metrics(
        &self,
        request: Request<proto::WatchMetricsRequest>,
    ) -> Result<Response<Self::WatchMetricsStream>, Status> {
        self.authorize(&request, false)?;
        let interval = request.into_inner().interval_ms;
        let interval = if interval == 0 {
            MINIMUM_METRICS_INTERVAL_MS
        } else {
            interval
        };
        if !(MINIMUM_METRICS_INTERVAL_MS..=MAXIMUM_METRICS_INTERVAL_MS).contains(&interval) {
            return Err(Status::invalid_argument(
                "metrics interval must be 1000..60000 ms",
            ));
        }
        let permit = Arc::clone(&self.subscriptions)
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("subscription budget exhausted"))?;
        let state = Arc::clone(&self.state);
        let cancellation = self.cancellation.clone();
        Ok(Response::new(Box::pin(async_stream::try_stream! {
            let _permit = permit;
            let mut timer = tokio::time::interval(Duration::from_millis(u64::from(interval)));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut previous = None;
            loop {
                tokio::select! { _ = timer.tick() => (), _ = cancellation.cancelled() => break }
                let snapshot = state.snapshot();
                let sequence = snapshot.status.identity.as_ref().map_or(0, |identity| identity.sequence);
                let skipped = previous.map_or(0_u64, |old: u64| sequence.saturating_sub(old).saturating_sub(1));
                previous = Some(sequence);
                yield proto::MetricsUpdate { metrics: Some(snapshot.metrics()), skipped_snapshots: skipped };
            }
        })))
    }

    type WatchEventsStream = ResponseStream<proto::Event>;
    async fn watch_events(
        &self,
        request: Request<proto::WatchEventsRequest>,
    ) -> Result<Response<Self::WatchEventsStream>, Status> {
        self.authorize(&request, false)?;
        let request = request.into_inner();
        let requested = request.after_sequence;
        let snapshot = self.state.snapshot();
        let instance = snapshot
            .status
            .identity
            .as_ref()
            .ok_or_else(|| Status::unavailable("snapshot not published"))?;
        if (requested != 0 || !request.expected_instance_id.is_empty())
            && request.expected_instance_id != instance.instance_id
        {
            return Err(Status::failed_precondition(
                "event cursor requires its original daemon instance",
            ));
        }
        let cursor = if requested == 0 {
            self.state.event_tail()?
        } else {
            requested
        };
        self.state.events_after(cursor)?;
        let permit = Arc::clone(&self.subscriptions)
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("subscription budget exhausted"))?;
        let state = Arc::clone(&self.state);
        let cancellation = self.cancellation.clone();
        Ok(Response::new(Box::pin(async_stream::try_stream! {
            let _permit = permit;
            let mut cursor = cursor;
            loop {
                let notified = state.event_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                for event in state.events_after(cursor)? {
                    cursor = event.sequence;
                    yield event;
                }
                tokio::select! { _ = notified => (), _ = cancellation.cancelled() => break }
            }
        })))
    }

    async fn start_capture(
        &self,
        request: Request<proto::StartCaptureRequest>,
    ) -> Result<Response<proto::Operation>, Status> {
        self.authorize(&request, true)?;
        let lifetime = command_lifetime(&request)?;
        let request = request.into_inner();
        let direction = proto::CaptureDirection::try_from(request.direction)
            .map_err(|_| Status::invalid_argument("unknown capture direction"))?;
        if direction == proto::CaptureDirection::Unspecified
            || request.duration_ms == 0
            || request.duration_ms > bridge::MAXIMUM_CAPTURE_DURATION_MS
            || request.maximum_mib == 0
            || request.maximum_mib > bridge::MAXIMUM_CAPTURE_MIB
        {
            return Err(Status::invalid_argument(
                "capture direction, duration or memory budget invalid",
            ));
        }
        self.submit(
            request.command,
            Action::StartCapture {
                direction,
                duration_ms: request.duration_ms,
                maximum_mib: request.maximum_mib,
            },
            lifetime,
        )
    }

    async fn stop_capture(
        &self,
        request: Request<proto::StopCaptureRequest>,
    ) -> Result<Response<proto::Operation>, Status> {
        self.authorize(&request, true)?;
        let lifetime = command_lifetime(&request)?;
        let request = request.into_inner();
        UuidValidation::check(&request.capture_id)?;
        self.submit(
            request.command,
            Action::StopCapture {
                capture_id: request.capture_id,
            },
            lifetime,
        )
    }

    async fn get_capture(
        &self,
        request: Request<proto::GetCaptureRequest>,
    ) -> Result<Response<proto::Capture>, Status> {
        self.authorize(&request, false)?;
        let id = request.into_inner().capture_id;
        UuidValidation::check(&id)?;
        self.state
            .captures()?
            .into_iter()
            .find(|capture| capture.id == id)
            .map(Response::new)
            .ok_or_else(|| Status::not_found("capture not retained"))
    }

    async fn list_captures(
        &self,
        request: Request<proto::ListCapturesRequest>,
    ) -> Result<Response<proto::ListCapturesResponse>, Status> {
        self.authorize(&request, false)?;
        Ok(Response::new(proto::ListCapturesResponse {
            captures: self.state.captures()?,
        }))
    }

    async fn shutdown(
        &self,
        request: Request<proto::ShutdownRequest>,
    ) -> Result<Response<proto::Operation>, Status> {
        self.authorize(&request, true)?;
        let lifetime = command_lifetime(&request)?;
        self.submit(request.into_inner().command, Action::Shutdown, lifetime)
    }

    async fn get_operation(
        &self,
        request: Request<proto::GetOperationRequest>,
    ) -> Result<Response<proto::Operation>, Status> {
        self.authorize(&request, false)?;
        let selector = request
            .into_inner()
            .selector
            .ok_or_else(|| Status::invalid_argument("operation selector required"))?;
        let id = match &selector {
            proto::get_operation_request::Selector::OperationId(id)
            | proto::get_operation_request::Selector::RequestId(id) => id,
        };
        UuidValidation::check(id)?;
        Ok(Response::new(self.state.operation(&selector)?))
    }
}

struct UuidValidation;
impl UuidValidation {
    fn check(value: &str) -> Result<(), Status> {
        uuid::Uuid::parse_str(value)
            .map(|_| ())
            .map_err(|_| Status::invalid_argument("identifier must be a UUID"))
    }
}

fn command_lifetime<T>(request: &Request<T>) -> Result<Duration, Status> {
    let Some(timeout) = request.metadata().get("grpc-timeout") else {
        return Ok(bridge::COMMAND_LIFETIME);
    };
    let text = timeout
        .to_str()
        .map_err(|_| Status::invalid_argument("invalid grpc-timeout"))?;
    if text.len() < 2 || text.len() > 9 {
        return Err(Status::invalid_argument("invalid grpc-timeout"));
    }
    let (digits, unit) = text.split_at(text.len() - 1);
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Status::invalid_argument("invalid grpc-timeout digits"));
    }
    let value = digits
        .parse::<u64>()
        .map_err(|_| Status::invalid_argument("invalid grpc-timeout"))?;
    let duration = match unit {
        "H" => Duration::from_secs(value * 3_600),
        "M" => Duration::from_secs(value * 60),
        "S" => Duration::from_secs(value),
        "m" => Duration::from_millis(value),
        "u" => Duration::from_micros(value),
        "n" => Duration::from_nanos(value),
        _ => return Err(Status::invalid_argument("invalid grpc-timeout unit")),
    };
    Ok(duration.min(bridge::COMMAND_LIFETIME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_deadlines_are_bounded_without_rejecting_large_valid_units() {
        for (header, expected) in [
            ("99999999H", Duration::from_secs(5)),
            ("2m", Duration::from_millis(2)),
            ("1u", Duration::from_micros(1)),
            ("0n", Duration::ZERO),
        ] {
            let mut request = Request::new(());
            request
                .metadata_mut()
                .insert("grpc-timeout", header.parse().unwrap());
            assert_eq!(command_lifetime(&request).unwrap(), expected);
        }
        for header in ["1", "+1S", "-1S", "123456789S", "1X", "S"] {
            let mut request = Request::new(());
            request
                .metadata_mut()
                .insert("grpc-timeout", header.parse().unwrap());
            assert_eq!(
                command_lifetime(&request).unwrap_err().code(),
                tonic::Code::InvalidArgument
            );
        }
    }
}
