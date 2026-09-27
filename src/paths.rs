use std::path::PathBuf;

fn env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from)
}

fn home() -> Option<PathBuf> {
    env("HOME").or_else(|| env("USERPROFILE"))
}

fn data() -> Option<PathBuf> {
    if cfg!(windows) {
        env("LOCALAPPDATA")
    } else {
        env("XDG_DATA_HOME").or_else(|| home().map(|home| home.join(".local/share")))
    }
}

fn cache() -> Option<PathBuf> {
    if cfg!(windows) {
        env("LOCALAPPDATA")
    } else {
        env("XDG_CACHE_HOME").or_else(|| home().map(|home| home.join(".cache")))
    }
}

fn config() -> Option<PathBuf> {
    if cfg!(windows) {
        env("APPDATA")
    } else {
        env("XDG_CONFIG_HOME").or_else(|| home().map(|home| home.join(".config")))
    }
}

#[must_use]
pub fn models() -> Option<PathBuf> {
    Some(data()?.join("omega").join("models"))
}

#[must_use]
pub fn stores() -> Option<PathBuf> {
    Some(cache()?.join("omega").join("index"))
}

#[must_use]
pub fn state() -> Option<PathBuf> {
    Some(cache()?.join("omega"))
}

#[must_use]
pub fn sockets() -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    env("XDG_RUNTIME_DIR").map(|dir| dir.join("omega")).or_else(state)
}

#[must_use]
pub fn settings() -> Option<PathBuf> {
    Some(config()?.join("omega").join("config.json"))
}
