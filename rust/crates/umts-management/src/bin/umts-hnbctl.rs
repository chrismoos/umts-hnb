// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use umts_management::{
    client::{self, Connection},
    proto::{self, hnb_management_client::HnbManagementClient},
};

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_CAPTURE_DURATION_MS: u32 = 2_000;
const DEFAULT_CAPTURE_MIB: u32 = 128;
const OPERATION_POLL_MS: u64 = 200;
const OPERATION_WAIT_SECONDS: u64 = 45;
const STREAM_TIMEOUT_SECONDS: u64 = 3_600;

#[derive(Parser)]
#[command(
    name = "umts-hnbctl",
    version,
    about = "Manage the Rust UMTS HNB over gRPC"
)]
struct Args {
    #[arg(long, global = true)]
    endpoint: Option<String>,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true, default_value_t = DEFAULT_TIMEOUT_MS, value_parser = clap::value_parser!(u64).range(1..=60_000))]
    timeout_ms: u64,
    #[arg(long, global = true)]
    tls_ca: Option<PathBuf>,
    #[arg(long, global = true)]
    tls_cert: Option<PathBuf>,
    #[arg(long, global = true)]
    tls_key: Option<PathBuf>,
    #[arg(long, global = true)]
    server_name: Option<String>,
    #[arg(long, global = true)]
    request_id: Option<uuid::Uuid>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Status,
    Config,
    Capabilities,
    Ue {
        #[command(subcommand)]
        command: UeCommand,
    },
    Radio {
        #[command(subcommand)]
        command: RadioCommand,
    },
    Events {
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, requires = "after")]
        instance: Option<String>,
    },
    Capture {
        #[command(subcommand)]
        command: CaptureCommand,
    },
    Operation {
        id: Option<String>,
    },
    Shutdown {
        #[arg(long)]
        wait: bool,
    },
}

#[derive(Subcommand)]
enum UeCommand {
    List,
    Show {
        #[arg(help = "epoch:slot:generation from ue list")]
        reference: String,
    },
}

#[derive(Subcommand)]
enum RadioCommand {
    Stats {
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 1_000)]
        interval_ms: u32,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Direction {
    Rx,
    Tx,
}

#[derive(Subcommand)]
enum CaptureCommand {
    Start {
        direction: Direction,
        #[arg(long, default_value_t = DEFAULT_CAPTURE_DURATION_MS)]
        duration_ms: u32,
        #[arg(long, default_value_t = DEFAULT_CAPTURE_MIB)]
        max_mib: u32,
        #[arg(long)]
        wait: bool,
    },
    Stop {
        id: String,
        #[arg(long)]
        wait: bool,
    },
    Status {
        id: String,
    },
    List,
}

impl Args {
    fn validate(&self) -> Result<()> {
        if let Command::Operation { id } = &self.command {
            ensure!(
                id.is_some() != self.request_id.is_some(),
                "select exactly one operation ID or --request-id"
            );
        }
        Ok(())
    }
}

impl Command {
    fn mutation(&self) -> bool {
        matches!(
            self,
            Self::Shutdown { .. }
                | Self::Capture {
                    command: CaptureCommand::Start { .. } | CaptureCommand::Stop { .. }
                }
        )
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    args.validate()?;
    let _logging =
        umts_runtime::logging::Logging::initialize("info").map_err(anyhow::Error::from_boxed)?;
    let timeout = Duration::from_millis(args.timeout_ms);
    let connection = Connection {
        endpoint: args
            .endpoint
            .clone()
            .unwrap_or_else(|| client::default_endpoint(args.command.mutation())),
        timeout,
        ca: args.tls_ca.clone(),
        certificate: args.tls_cert.clone(),
        key: args.tls_key.clone(),
        server_name: args.server_name.clone(),
    };
    let mut api = connection.connect().await?;
    macro_rules! call {
        ($method:ident, $message:expr) => {
            api.$method(client::request($message, timeout))
                .await
                .context(stringify!($method))?
                .into_inner()
        };
    }
    match &args.command {
        Command::Status => {
            let value = call!(get_status, proto::GetStatusRequest {});
            if args.json {
                output(&value)?
            } else {
                let phase =
                    proto::Phase::try_from(value.phase).unwrap_or(proto::Phase::Unspecified);
                println!(
                    "{}  core={} radio_ready={} UEs={} bearers={}",
                    phase.as_str_name(),
                    value.core_connected,
                    value.radio_ready,
                    value.registered_ues,
                    value.active_bearers
                );
                println!(
                    "version={} commit={} config={}",
                    value.build_version, value.build_commit, value.configuration_sha256
                );
                if let Some(id) = value.identity {
                    println!(
                        "instance={} epoch={} snapshot={} uptime_ms={}",
                        id.instance_id, id.epoch, id.sequence, id.uptime_ms
                    )
                }
                if !value.failure_reason.is_empty() {
                    println!("failure={}", value.failure_reason)
                }
            }
        }
        Command::Config => output(&call!(get_configuration, proto::GetConfigurationRequest {}))?,
        Command::Capabilities => {
            output(&call!(get_capabilities, proto::GetCapabilitiesRequest {}))?
        }
        Command::Ue {
            command: UeCommand::List,
        } => {
            let mut token = String::new();
            let mut ues = Vec::new();
            loop {
                let page = call!(
                    list_ues,
                    proto::ListUesRequest {
                        page_size: 128,
                        page_token: token
                    }
                );
                ues.extend(page.ues);
                ensure!(
                    ues.len() <= umts_management::bridge::MAXIMUM_UES,
                    "server exceeded UE result bound"
                );
                token = page.next_page_token;
                if token.is_empty() {
                    break;
                }
            }
            if args.json {
                let ues = ues
                    .iter()
                    .map(umts_management::json::to_value)
                    .collect::<Result<Vec<_>, _>>()?;
                println!("{}", serde_json::to_string_pretty(&ues)?);
            } else {
                println!("REFERENCE  IMSI  RRC  CS  PS  BEARERS");
                for ue in ues {
                    let id = ue.reference.context("server omitted UE reference")?;
                    println!(
                        "{}:{}:{}  {}  {}  {}  {}  {}",
                        id.epoch,
                        id.slot,
                        id.generation,
                        ue.imsi.as_deref().unwrap_or("unknown"),
                        proto::RrcState::try_from(ue.rrc_state)
                            .unwrap_or(proto::RrcState::Unspecified)
                            .as_str_name(),
                        proto::DomainState::try_from(ue.cs_state)
                            .unwrap_or(proto::DomainState::Unspecified)
                            .as_str_name(),
                        proto::DomainState::try_from(ue.ps_state)
                            .unwrap_or(proto::DomainState::Unspecified)
                            .as_str_name(),
                        ue.bearers.len()
                    );
                    for carrier in &ue.hsdpa_carriers {
                        let role = proto::HsdpaCarrierRole::try_from(carrier.role)
                            .unwrap_or(proto::HsdpaCarrierRole::Unspecified)
                            .as_str_name();
                        let state = proto::HsdpaCarrierState::try_from(carrier.state)
                            .unwrap_or(proto::HsdpaCarrierState::Unspecified)
                            .as_str_name();
                        println!(
                            "  {}  {}  CQI={}  CODES={}  MAX_TB_BITS={}",
                            role,
                            state,
                            carrier
                                .cqi
                                .map_or_else(|| "unknown".into(), |value| value.to_string()),
                            carrier
                                .hs_pdsch_codes
                                .map_or_else(|| "unknown".into(), |value| value.to_string()),
                            carrier.maximum_new_transport_bits
                        );
                    }
                }
            }
        }
        Command::Ue {
            command: UeCommand::Show { reference },
        } => {
            let id = parse_reference(reference)?;
            output(&call!(
                get_ue,
                proto::GetUeRequest {
                    reference: Some(id)
                }
            ))?;
        }
        Command::Radio {
            command: RadioCommand::Stats { watch: false, .. },
        } => output(&call!(get_radio_metrics, proto::GetRadioMetricsRequest {}))?,
        Command::Radio {
            command:
                RadioCommand::Stats {
                    watch: true,
                    interval_ms,
                },
        } => {
            let mut stream = api
                .watch_metrics(client::request(
                    proto::WatchMetricsRequest {
                        interval_ms: *interval_ms,
                    },
                    Duration::from_secs(STREAM_TIMEOUT_SECONDS),
                ))
                .await?
                .into_inner();
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    message = stream.message() => match message? { Some(value) => output_line(&value)?, None => break }
                }
            }
        }
        Command::Events { after, instance } => {
            ensure!(
                *after == 0 || instance.is_some(),
                "resuming events requires --instance from the original event stream"
            );
            let mut stream = api
                .watch_events(client::request(
                    proto::WatchEventsRequest {
                        after_sequence: *after,
                        expected_instance_id: instance.clone().unwrap_or_default(),
                    },
                    Duration::from_secs(STREAM_TIMEOUT_SECONDS),
                ))
                .await?
                .into_inner();
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    message = stream.message() => match message? { Some(value) => output_line(&value)?, None => break }
                }
            }
        }
        Command::Capture {
            command: CaptureCommand::Status { id },
        } => output(&call!(
            get_capture,
            proto::GetCaptureRequest {
                capture_id: id.clone()
            }
        ))?,
        Command::Capture {
            command: CaptureCommand::List,
        } => output(&call!(list_captures, proto::ListCapturesRequest {}))?,
        Command::Operation { id } => {
            let selector = match (id, &args.request_id) {
                (Some(id), None) => proto::get_operation_request::Selector::OperationId(id.clone()),
                (None, Some(id)) => {
                    proto::get_operation_request::Selector::RequestId(id.to_string())
                }
                _ => bail!("select exactly one operation ID or --request-id"),
            };
            output(&call!(
                get_operation,
                proto::GetOperationRequest {
                    selector: Some(selector)
                }
            ))?;
        }
        Command::Capture { command } => {
            let identity = command_identity(&mut api, &args, timeout).await?;
            let (operation, wait) = match command {
                CaptureCommand::Start {
                    direction,
                    duration_ms,
                    max_mib,
                    wait,
                } => {
                    let direction = match direction {
                        Direction::Rx => proto::CaptureDirection::Rx,
                        Direction::Tx => proto::CaptureDirection::Tx,
                    };
                    (
                        call!(
                            start_capture,
                            proto::StartCaptureRequest {
                                command: Some(identity),
                                direction: direction.into(),
                                duration_ms: *duration_ms,
                                maximum_mib: *max_mib
                            }
                        ),
                        *wait,
                    )
                }
                CaptureCommand::Stop { id, wait } => (
                    call!(
                        stop_capture,
                        proto::StopCaptureRequest {
                            command: Some(identity),
                            capture_id: id.clone()
                        }
                    ),
                    *wait,
                ),
                _ => unreachable!(),
            };
            finish_operation(&mut api, operation, wait, timeout, args.json).await?;
        }
        Command::Shutdown { wait } => {
            let identity = command_identity(&mut api, &args, timeout).await?;
            let operation = call!(
                shutdown,
                proto::ShutdownRequest {
                    command: Some(identity)
                }
            );
            finish_operation(&mut api, operation, *wait, timeout, args.json).await?;
        }
    }
    Ok(())
}

async fn command_identity(
    api: &mut HnbManagementClient<tonic::transport::Channel>,
    args: &Args,
    timeout: Duration,
) -> Result<proto::CommandIdentity> {
    let status = api
        .get_status(client::request(proto::GetStatusRequest {}, timeout))
        .await?
        .into_inner();
    let identity = status
        .identity
        .context("server omitted instance identity")?;
    let command = proto::CommandIdentity {
        request_id: args
            .request_id
            .unwrap_or_else(uuid::Uuid::new_v4)
            .to_string(),
        expected_instance_id: identity.instance_id,
        expected_epoch: identity.epoch,
    };
    tracing::info!(
        "request_id={} instance={} epoch={}",
        command.request_id,
        command.expected_instance_id,
        command.expected_epoch
    );
    Ok(command)
}

async fn finish_operation(
    api: &mut HnbManagementClient<tonic::transport::Channel>,
    mut operation: proto::Operation,
    wait: bool,
    timeout: Duration,
    json: bool,
) -> Result<()> {
    tracing::info!(
        "operation={} request_id={}",
        operation.id,
        operation.request_id
    );
    let began = Instant::now();
    while wait
        && matches!(
            proto::OperationPhase::try_from(operation.phase),
            Ok(proto::OperationPhase::Queued | proto::OperationPhase::Running)
        )
    {
        ensure!(
            began.elapsed() < Duration::from_secs(OPERATION_WAIT_SECONDS),
            "operation wait timed out, query the operation ID before retrying"
        );
        tokio::time::sleep(Duration::from_millis(OPERATION_POLL_MS)).await;
        operation = api
            .get_operation(client::request(
                proto::GetOperationRequest {
                    selector: Some(proto::get_operation_request::Selector::OperationId(
                        operation.id.clone(),
                    )),
                },
                timeout,
            ))
            .await
            .context("query accepted operation, outcome is unknown if daemon disconnected")?
            .into_inner();
    }
    let failed = operation.phase == i32::from(proto::OperationPhase::Failed);
    if json {
        output(&operation)?
    } else {
        println!(
            "{} {} capture={}",
            operation.id,
            proto::OperationPhase::try_from(operation.phase)
                .unwrap_or(proto::OperationPhase::Unspecified)
                .as_str_name(),
            operation.capture_id
        );
        if failed {
            println!("error={} {}", operation.error_code, operation.error_message)
        }
    }
    ensure!(!failed, "operation failed");
    Ok(())
}

fn parse_reference(value: &str) -> Result<proto::UeRef> {
    let pieces = value.split(':').collect::<Vec<_>>();
    ensure!(
        pieces.len() == 3,
        "UE reference must be epoch:slot:generation"
    );
    let result = proto::UeRef {
        epoch: pieces[0].parse()?,
        slot: pieces[1].parse()?,
        generation: pieces[2].parse()?,
    };
    ensure!(
        result.epoch != 0 && result.generation != 0,
        "UE epoch and generation must be nonzero"
    );
    Ok(result)
}

fn output(value: &impl prost::Name) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&umts_management::json::to_value(value)?)?
    );
    Ok(())
}
fn output_line(value: &impl prost::Name) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string(&umts_management::json::to_value(value)?)?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_request_id_is_order_independent_and_unambiguous() {
        let request = "d111d7af-8106-4d3e-b500-3fe2b27a0199";
        for args in [
            vec!["umts-hnbctl", "--request-id", request, "operation"],
            vec!["umts-hnbctl", "operation", "--request-id", request],
            vec!["umts-hnbctl", "operation", "operation-id"],
        ] {
            Args::try_parse_from(args).unwrap().validate().unwrap();
        }
        for args in [
            vec!["umts-hnbctl", "operation"],
            vec![
                "umts-hnbctl",
                "--request-id",
                request,
                "operation",
                "operation-id",
            ],
        ] {
            assert!(Args::try_parse_from(args).unwrap().validate().is_err());
        }
    }

    #[test]
    fn ue_reference_preserves_full_generation() {
        assert_eq!(
            parse_reference("9223372036854775808:3:12").unwrap().epoch,
            9223372036854775808
        );
        for invalid in ["1:2", "0:1:2", "1:1:0", "1:4294967296:1", "bad"] {
            assert!(parse_reference(invalid).is_err())
        }
    }
}
