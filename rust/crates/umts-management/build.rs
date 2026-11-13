// SPDX-License-Identifier: AGPL-3.0-or-later

fn main() {
    let output =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("build output directory"));
    let descriptors = output.join("management-descriptors.bin");
    let mut prost = tonic_prost_build::Config::new();
    prost.file_descriptor_set_path(&descriptors);
    prost.enable_type_names();
    println!("cargo:rerun-if-changed=proto/umts/hnb/management/v1/management.proto");
    tonic_prost_build::configure()
        .compile_with_config(
            prost,
            &["proto/umts/hnb/management/v1/management.proto"],
            &["proto"],
        )
        .expect("compile HNB management protocol");
}
