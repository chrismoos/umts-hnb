// SPDX-License-Identifier: AGPL-3.0-or-later

use std::process::Command;

fn main() {
    let schema = "proto/umts/hnb/state/v1/system_information.proto";
    println!("cargo:rerun-if-changed={schema}");
    prost_build::Config::new()
        .compile_protos(&[schema], &["proto"])
        .expect("compile system-information state protocol");
    println!("cargo:rerun-if-changed=build.rs");
    let output = Command::new("git").args(["rev-parse", "HEAD"]).output();
    let commit = output
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=HNB_BUILD_COMMIT={}", commit.trim());
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--git-path", "HEAD"])
        .output()
        && output.status.success()
    {
        let head = String::from_utf8_lossy(&output.stdout);
        println!("cargo:rerun-if-changed={}", head.trim());
    }
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--symbolic-full-name", "HEAD"])
        .output()
        && output.status.success()
        && let Ok(reference) = String::from_utf8(output.stdout)
        && let Ok(output) = Command::new("git")
            .args(["rev-parse", "--git-path", reference.trim()])
            .output()
        && output.status.success()
    {
        println!(
            "cargo:rerun-if-changed={}",
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
}
