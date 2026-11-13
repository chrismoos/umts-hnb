// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{check_depth, *};
use crate::*;
use std::path::PathBuf;

pub(super) trait Patch {
    fn check(value: &Value, depth: usize) -> Result<()>;
}

pub(super) fn check<T: Patch>(value: &Value, depth: usize) -> Result<()> {
    check_depth(depth)?;
    T::check(value, depth)
}

impl<T: Patch> Patch for Option<T> {
    fn check(value: &Value, depth: usize) -> Result<()> {
        if value.is_null() {
            Ok(())
        } else {
            check::<T>(value, depth + 1)
        }
    }
}

impl<T: Patch> Patch for Vec<T> {
    fn check(value: &Value, depth: usize) -> Result<()> {
        for (index, item) in value
            .as_sequence()
            .context("expected a list")?
            .iter()
            .enumerate()
        {
            check::<T>(item, depth + 1).with_context(|| format!("list item {index}"))?;
        }
        Ok(())
    }
}

impl Patch for String {
    fn check(value: &Value, _: usize) -> Result<()> {
        if value.is_string() {
            Ok(())
        } else {
            bail!("expected a string")
        }
    }
}

impl Patch for PathBuf {
    fn check(value: &Value, depth: usize) -> Result<()> {
        String::check(value, depth)
    }
}

macro_rules! scalars {
    ($($ty:ty),* $(,)?) => { $(
        impl Patch for $ty {
            fn check(value: &Value, _: usize) -> Result<()> {
                serde_yaml::from_value::<Self>(value.clone()).map(|_| ()).context(stringify!($ty))
            }
        }
    )* };
}

macro_rules! floats {
    ($($ty:ty),* $(,)?) => { $(
        impl Patch for $ty {
            fn check(value: &Value, _: usize) -> Result<()> {
                let number = serde_yaml::from_value::<Self>(value.clone())?;
                if number.is_finite() { Ok(()) } else { bail!("expected a finite number") }
            }
        }
    )* };
}
floats!(f32, f64);

macro_rules! records {
    ($($ty:ty { $($field:ident: $value:ty),* $(,)? })*) => { $(
        impl Patch for $ty {
            fn check(value: &Value, depth: usize) -> Result<()> {
                for (key, value) in value.as_mapping().context("expected a mapping")? {
                    let key = key.as_str().context("configuration keys must be strings")?;
                    match key {
                        $(stringify!($field) => check::<$value>(value, depth + 1),)*
                        _ => bail!("unknown configuration property {key}"),
                    }.with_context(|| format!("property {key}"))?;
                }
                Ok(())
            }
        }
    )* };
}

macro_rules! properties {
    ($($target:literal: $ty:ty, $required:literal;)*) => {
        pub(super) const ROUTES: &[Route] = &[$(Route {
            target: $target, required: $required, check: check::<$ty>,
        }),*];
    };
}

include!("properties.rs");
