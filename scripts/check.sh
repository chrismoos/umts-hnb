#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$repo_root"

# Only Linux has kernel SCTP. Other hosts build the userspace transport.
features=()
if [[ $(uname -s) != Linux ]]; then
    features=(--features umts-hnb/usrsctp)
    if [[ -z ${BINDGEN_EXTRA_CLANG_ARGS:-} ]] && command -v brew >/dev/null; then
        export BINDGEN_EXTRA_CLANG_ARGS="-I$(brew --prefix)/include"
    fi
fi

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --release ${features[@]+"${features[@]}"} -- -D warnings
cargo test --workspace --release ${features[@]+"${features[@]}"}
