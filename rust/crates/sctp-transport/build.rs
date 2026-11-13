// SPDX-License-Identifier: AGPL-3.0-or-later

fn main() {
    #[cfg(feature = "usrsctp")]
    {
        let library = pkg_config::Config::new()
            .atleast_version("0.9.5")
            .probe("usrsctp")
            .expect("install usrsctp development headers and pkg-config metadata");
        let mut builder = bindgen::Builder::default()
            .header_contents("sctp_wrapper.h", "#include <usrsctp.h>")
            .allowlist_function("usrsctp_.*")
            .allowlist_type("sctp_.*")
            .allowlist_var("SCTP_.*|MSG_NOTIFICATION")
            .generate_comments(false)
            .derive_default(true)
            .layout_tests(false);
        for path in library.include_paths {
            builder = builder.clang_arg(format!("-I{}", path.display()));
        }
        builder
            .generate()
            .expect("generate usrsctp bindings")
            .write_to_file(
                std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("bindings.rs"),
            )
            .expect("write usrsctp bindings");
    }
}
