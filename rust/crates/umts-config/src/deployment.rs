// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{fs::File, io::Read, path::Path};

use anyhow::{Context, Result, bail};
use serde_yaml::Value;

use crate::Config;

const MAXIMUM_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;
const MAXIMUM_NESTING: usize = 64;

mod patch;
use patch::ROUTES;

struct Route {
    target: &'static str,
    required: bool,
    check: fn(&Value, usize) -> Result<()>,
}

pub struct Deployment {
    pub config: Config,
    pub effective: Value,
}

impl Deployment {
    pub fn load(operator: &Path, defaults: &Path) -> Result<Self> {
        Self::parse(&read_document(defaults)?, &read_document(operator)?)
            .with_context(|| format!("loading {} over {}", operator.display(), defaults.display()))
    }

    pub fn parse(defaults: &str, operator: &str) -> Result<Self> {
        let mut defaults = parse_patch(defaults).context("system defaults")?;
        let operator = parse_patch(operator).context("operator configuration")?;
        validate_mode_list(&defaults)?;
        validate_mode_list(&operator)?;
        for path in ROUTES
            .iter()
            .filter(|route| route.required)
            .map(|route| route.target)
        {
            if lookup(&defaults, path).is_some() {
                bail!("operator property {path} must not be supplied by system defaults");
            }
            if lookup(&operator, path).is_none() {
                bail!("operator configuration must specify {path}");
            }
        }
        let explicit = operator.clone();
        merge(&mut defaults, operator);
        crate::profiles::resolve(&mut defaults)?;
        let mut config: Config =
            serde_yaml::from_value(defaults).context("decoding resolved settings")?;
        config.resolve_packet_modes(&explicit)?;
        config.validate()?;
        let effective = serde_yaml::to_value(&config)?;
        Ok(Self { config, effective })
    }
}

fn validate_mode_list(value: &Value) -> Result<()> {
    if let Some(value) = lookup(value, "cell.packet_modes") {
        let modes: Vec<crate::PacketMode> = serde_yaml::from_value(value.clone())?;
        if modes.is_empty() {
            bail!("cell.packet_modes must contain at least one mode");
        }
        for (index, mode) in modes.iter().enumerate() {
            if modes[..index].contains(mode) {
                bail!("cell.packet_modes must not contain duplicate modes");
            }
        }
    }
    Ok(())
}

fn parse_patch(text: &str) -> Result<Value> {
    if text.len() as u64 > MAXIMUM_DOCUMENT_BYTES {
        bail!("configuration exceeds {MAXIMUM_DOCUMENT_BYTES} bytes");
    }
    let value: Value = serde_yaml::from_str(text).context("parsing YAML")?;
    check_paths(&value, "", 0)?;
    Ok(value)
}

fn check_paths(value: &Value, path: &str, depth: usize) -> Result<()> {
    check_depth(depth)?;
    if let Some(route) = ROUTES.iter().find(|route| route.target == path) {
        return (route.check)(value, depth)
            .with_context(|| format!("configuration property {path}"));
    }
    let mapping = value
        .as_mapping()
        .with_context(|| format!("{path} must be a mapping"))?;
    for (key, value) in mapping {
        let key = key.as_str().context("configuration keys must be strings")?;
        let child = if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        };
        if ROUTES
            .iter()
            .any(|route| route.target == child || route.target.starts_with(&(child.clone() + ".")))
        {
            check_paths(value, &child, depth + 1)?;
        } else {
            bail!("unknown configuration property {child}");
        }
    }
    Ok(())
}

fn check_depth(depth: usize) -> Result<()> {
    if depth > MAXIMUM_NESTING {
        bail!("configuration nesting exceeds {MAXIMUM_NESTING}");
    }
    Ok(())
}

fn read_document(path: &Path) -> Result<String> {
    let mut bytes = Vec::new();
    File::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .take(MAXIMUM_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAXIMUM_DOCUMENT_BYTES {
        bail!("{} exceeds {MAXIMUM_DOCUMENT_BYTES} bytes", path.display());
    }
    String::from_utf8(bytes).with_context(|| format!("reading UTF-8 from {}", path.display()))
}

fn lookup<'a>(mut value: &'a Value, path: &str) -> Option<&'a Value> {
    for key in path.split('.') {
        value = value.as_mapping()?.get(Value::String(key.to_owned()))?;
    }
    Some(value)
}

#[cfg(test)]
fn insert(value: &mut Value, path: &str, item: Value) -> Result<()> {
    let (head, tail) = path
        .split_once('.')
        .map_or((path, None), |(head, tail)| (head, Some(tail)));
    let mapping = value
        .as_mapping_mut()
        .context("configuration property collision")?;
    let key = Value::String(head.to_owned());
    if let Some(tail) = tail {
        insert(
            mapping
                .entry(key)
                .or_insert_with(|| Value::Mapping(serde_yaml::Mapping::new())),
            tail,
            item,
        )
    } else {
        if mapping.insert(key, item).is_some() {
            bail!("duplicate deployment destination {path}");
        }
        Ok(())
    }
}

fn merge(defaults: &mut Value, operator: Value) {
    match (defaults, operator) {
        (Value::Mapping(defaults), Value::Mapping(operator)) => {
            for (key, value) in operator {
                if let Some(previous) = defaults.get_mut(&key) {
                    merge(previous, value);
                } else {
                    defaults.insert(key, value);
                }
            }
        }
        (defaults, operator) => *defaults = operator,
    }
}

#[cfg(test)]
mod tests;
