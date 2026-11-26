//! The process environment. A simulation has its own, which starts empty (`sim::set_env`).

use std::ffi::{OsStr, OsString};

use super::sim;

pub fn var_os(name: impl AsRef<OsStr>) -> Option<OsString> {
    match sim::env(name.as_ref()) {
        Some(simulated) => simulated,
        None => std::env::var_os(name),
    }
}

pub fn var(name: impl AsRef<OsStr>) -> Result<String, std::env::VarError> {
    match var_os(name) {
        Some(value) => value.into_string().map_err(std::env::VarError::NotUnicode),
        None => Err(std::env::VarError::NotPresent),
    }
}

pub fn vars_os() -> Vec<(OsString, OsString)> {
    sim::envs().unwrap_or_else(|| std::env::vars_os().collect())
}

pub fn vars() -> Vec<(String, String)> {
    vars_os().into_iter().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect()
}
