// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};
use std::{fmt::Write as _, fs::File, io::Write as _, path::PathBuf};
use umts_rrc::{
    establishment::{
        AssignedRntis, CellFachSetup, RejectionReason, SignallingProfile, TransactionId,
        build_cell_fach_setup, build_rejection,
    },
    mobility::build_unknown_context_release,
};
use umts_rrc_codec::{EncodedRrc, decode_uplink_common};
use umts_rrc_schema::class_definitions::ULCCCHMessageType;

const SYNTHETIC_REQUEST: [u8; 10] = [0x20, 0x90, 0x01, 0x01, 0, 0, 0, 0, 0x01, 0x60];
const SYNTHETIC_REQUEST_BITS: usize = 78;
const MAXIMUM_MESSAGE_BYTES: usize = 1024;
const DRX_COEFFICIENT: u8 = 5;
const TRANSACTION: u8 = 2;
const RNC: u16 = 1;
const SRNTI: u32 = 0x12345;
const CRNTI: u16 = 0x2345;
const WAIT_TIME: u8 = 0;

fn wire(name: &str, parameters: Value, encoded: EncodedRrc) -> Result<Value> {
    ensure!(
        encoded.bytes.len() <= MAXIMUM_MESSAGE_BYTES,
        "message exceeds export bound"
    );
    let mut bytes_hex = String::with_capacity(encoded.bytes.len() * 2);
    for byte in &encoded.bytes {
        write!(bytes_hex, "{byte:02x}")?;
    }
    Ok(json!({
        "name": name,
        "parameters": parameters,
        "bytes_hex": bytes_hex,
        "bit_length": encoded.bit_length,
    }))
}

fn main() -> Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(arguments.len() == 1, "common_rrc_wire NEW_JSON_OUTPUT");
    let request = decode_uplink_common(&SYNTHETIC_REQUEST, SYNTHETIC_REQUEST_BITS)
        .map_err(|error| anyhow!("synthetic request: {error:?}"))?;
    let ULCCCHMessageType::rrcConnectionRequest(request) = &request.value.message else {
        return Err(anyhow!("synthetic fixture is not a connection request"));
    };
    let identity = &request.initial_ue_identity;
    let assigned = AssignedRntis::new(RNC, SRNTI, CRNTI)
        .map_err(|error| anyhow!("synthetic assigned identities: {error:?}"))?;
    let transaction = TransactionId::new(TRANSACTION)
        .map_err(|error| anyhow!("synthetic transaction: {error:?}"))?;
    let setup = build_cell_fach_setup(
        identity,
        &CellFachSetup {
            assigned,
            transaction,
            drx_coefficient: DRX_COEFFICIENT,
            signalling: SignallingProfile,
        },
    )
    .map_err(|error| anyhow!("CELL_FACH setup: {error:?}"))?;
    let rejection = build_rejection(
        identity,
        transaction,
        RejectionReason::Unspecified,
        WAIT_TIME,
    )
    .map_err(|error| anyhow!("connection rejection: {error:?}"))?;
    let release = build_unknown_context_release(assigned.urnti(), transaction)
        .map_err(|error| anyhow!("unknown-context release: {error:?}"))?;
    let export = json!({
        "schema_version": 1,
        "scope": "Synthetic identities and production common RRC builders, not received UE messages",
        "request_fixture": {"bytes_hex": "20900101000000000160", "bit_length": SYNTHETIC_REQUEST_BITS},
        "messages": [
            wire("CellFachSetup", json!({
                "identity": "synthetic-imsi", "transaction": TRANSACTION,
                "rnc": RNC, "srnti": SRNTI, "crnti": CRNTI,
                "drx_coefficient": DRX_COEFFICIENT,
            }), setup)?,
            wire("ConnectionReject", json!({
                "identity": "synthetic-imsi", "transaction": TRANSACTION,
                "reason": "unspecified", "wait_time_code": WAIT_TIME,
            }), rejection)?,
            wire("UnknownContextRelease", json!({
                "urnti": assigned.urnti(), "transaction": TRANSACTION,
            }), release)?,
        ],
    });
    let bytes = serde_json::to_vec_pretty(&export)?;
    let path = PathBuf::from(&arguments[0]);
    let mut output = File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .context("create new common RRC export")?;
    output.write_all(&bytes)?;
    output.write_all(b"\n")?;
    Ok(())
}
