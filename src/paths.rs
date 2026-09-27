//! Every place on disk omega writes to, in one list: what an install creates,
//! an uninstall has to find, and the two must not drift apart.
//!
//! Windows keeps data and caches under `%LOCALAPPDATA%` and settings under
//! `%APPDATA%`; elsewhere the XDG directories are used, with their defaults.

use std::path::PathBuf;

fn env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from)
}

fn home() -> Option<PathBuf> {
    env("HOME").or_else(|| env("USERPROFILE"))
}

/// Where downloaded data lives: the model.
fn data() -> Option<PathBuf> {
    if cfg!(windows) {
        env("LOCALAPPDATA")
    } else {
        env("XDG_DATA_HOME").or_else(|| home().map(|home| home.join(".local/share")))
    }
}

/// Where what can be rebuilt lives: the index stores.
fn cache() -> Option<PathBuf> {
    if cfg!(windows) {
        env("LOCALAPPDATA")
    } else {
        env("XDG_CACHE_HOME").or_else(|| home().map(|home| home.join(".cache")))
    }
}

/// Where what the user decided lives: the settings.
fn config() -> Option<PathBuf> {
    if cfg!(windows) {
        env("APPDATA")
    } else {
        env("XDG_CONFIG_HOME").or_else(|| home().map(|home| home.join(".config")))
    }
}

/// The directory the models are installed under, one subdirectory each.
#[must_use]
pub fn models() -> Option<PathBuf> {
    Some(data()?.join("omega").join("models"))
}

/// The directory of the index stores, one file per repository and model.
#[must_use]
pub fn stores() -> Option<PathBuf> {
    Some(cache()?.join("omega").join("index"))
}

/// omega's settings: the access each repository has been given.
#[must_use]
pub fn settings() -> Option<PathBuf> {
    Some(config()?.join("omega").join("config.json"))
}
