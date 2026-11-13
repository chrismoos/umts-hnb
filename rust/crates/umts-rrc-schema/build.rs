// SPDX-License-Identifier: AGPL-3.0-or-later

use rasn_compiler::{OutputMode, prelude::*};
use std::{env, fs, path::PathBuf};

fn main() {
    let schema = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("schema/rrc.asn1");
    println!("cargo:rerun-if-changed={}", schema.display());
    let source = fs::read_to_string(&schema).expect("read RRC ASN.1 schema");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("rrc.rs");
    let warnings = Compiler::<RasnBackend, _>::new()
        .add_asn_literal(source)
        .set_output_mode(OutputMode::SingleFile(output))
        .compile()
        .expect("generate Rust RRC codecs");
    assert!(
        warnings.is_empty(),
        "RRC schema generation warnings: {warnings:?}"
    );
}
