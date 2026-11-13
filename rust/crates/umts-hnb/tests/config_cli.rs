// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{path::PathBuf, process::Command};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_umts-hnb"))
}

#[test]
fn deployment_yaml_is_accepted_without_hardware() {
    let output = command()
        .args(["--check", "-c"])
        .arg(root().join("config/example.yaml"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"Config is okay.\n");
}

#[test]
fn operator_overrides_resolve_against_explicit_public_defaults() {
    let operator = root().join("rust/test-support/fixtures/config/operator.yaml");
    let defaults = root().join("config/defaults.yaml");
    let output = command()
        .args(["--check", "-c"])
        .arg(&operator)
        .arg("--defaults")
        .arg(&defaults)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"Config is okay.\n");
}

#[test]
fn packet_mode_profiles_preflight_and_invalid_dc_frequencies_fail_without_hardware() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("operator.yaml");
    let example = std::fs::read_to_string(root().join("config/example.yaml")).unwrap();
    let mut operator: serde_yaml::Value = serde_yaml::from_str(&example).unwrap();
    operator.as_mapping_mut().unwrap().remove("hspa");
    operator["cell"]["secondary_carrier"] = serde_yaml::from_str("{uarfcn_dl: 4425}").unwrap();
    for mode in ["rel99", "hspa", "hspa-plus", "dc-hspa"] {
        operator["cell"]["packet_modes"] = serde_yaml::to_value([mode]).unwrap();
        operator["cell"]["secondary_carrier"]["uarfcn_dl"] = 4425.into();
        std::fs::write(&path, serde_yaml::to_string(&operator).unwrap()).unwrap();
        let output = command()
            .args(["--check", "--config"])
            .arg(&path)
            .arg("--defaults")
            .arg(root().join("config/defaults.yaml"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"Config is okay.\n");
    }
    for frequency in [4450, 11000] {
        operator["cell"]["secondary_carrier"]["uarfcn_dl"] = frequency.into();
        std::fs::write(&path, serde_yaml::to_string(&operator).unwrap()).unwrap();
        let output = command()
            .args(["--check", "--config"])
            .arg(&path)
            .arg("--defaults")
            .arg(root().join("config/defaults.yaml"))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("must be in the primary FDD band and exactly 5 MHz")
        );
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn incomplete_runtime_refuses_to_pretend_that_a_cell_is_up() {
    let output = command()
        .arg("-c")
        .arg(root().join("config/example.yaml"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("No radio or network connection was opened")
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn startup_errors_use_the_logging_filter_and_preserve_failure_status() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing.yaml");
    for (filter, expected_log) in [("off", false), ("error", true)] {
        let output = command()
            .env("RUST_LOG", filter)
            .args(["--check", "--config"])
            .arg(&missing)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        if expected_log {
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains("ERROR"), "{error}");
            assert!(error.contains("HNB stopped"), "{error}");
        } else {
            assert!(output.stderr.is_empty());
        }
    }
}

#[test]
fn fresh_checkout_defaults_preflight_without_creating_runtime_files() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("config")).unwrap();
    std::fs::copy(
        root().join("config/defaults.yaml"),
        directory.path().join("config/defaults.yaml"),
    )
    .unwrap();
    std::fs::copy(
        root().join("config/example.yaml"),
        directory.path().join("site.local.yaml"),
    )
    .unwrap();
    let output = command()
        .current_dir(directory.path())
        .arg("--check")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"Config is okay.\n");
    assert!(!directory.path().join(".runtime").exists());
    let names: std::collections::BTreeSet<_> = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        names,
        ["config", "site.local.yaml"]
            .map(std::ffi::OsString::from)
            .into_iter()
            .collect()
    );
}
