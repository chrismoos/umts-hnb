// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::{Deserialize, Deserializer, de::Error};
use std::path::{Path, PathBuf};

pub(crate) fn default_runtime_path(name: &str) -> PathBuf {
    let path = Path::new(".runtime").join(name);
    // Serde defaults are infallible. An unresolved fallback fails absolute-path validation.
    std::path::absolute(&path).unwrap_or(path)
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
    let path = PathBuf::deserialize(deserializer)?;
    if path.as_os_str().is_empty() {
        return Err(D::Error::custom("runtime path must not be empty"));
    }
    std::path::absolute(path).map_err(D::Error::custom)
}
