//! `omega update`: the installed binary brought to the latest release.
//!
//! A release carries, beside its archives, the bare binary of each platform
//! and its SHA-256, so an update is one small download and a check, with no
//! archive to unpack. The running binary cannot be overwritten on Windows but
//! can be renamed, so the new one takes its place and the old steps aside;
//! on Unix the new one is renamed over it. Integrations already installed
//! into agents are then written again, so an instruction text that changed
//! with the release reaches them.

use sha2::{Digest, Sha256};
use std::io::Read as _;
use std::path::{Path, PathBuf};

const REPOSITORY: &str = "gyxoBka/omega-seek";
/// A binary is a few megabytes; this is a bound, not a size.
const MAX_BYTES: u64 = 64 << 20;

/// The release asset built for this machine, if the release builds one.
fn asset() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "omega-x86_64-pc-windows-msvc.exe",
        ("linux", "x86_64") => "omega-x86_64-unknown-linux-gnu",
        ("macos", "aarch64") => "omega-aarch64-apple-darwin",
        _ => return None,
    })
}

/// The tag of the latest release, read off where GitHub redirects
/// `releases/latest` to: no API, no token, no rate limit to speak of.
fn latest_tag(repository: &str) -> Result<String, String> {
    let url = format!("https://github.com/{repository}/releases/latest");
    let agent = ureq::Agent::config_builder().max_redirects(0).build().new_agent();
    let response = agent.get(&url).call();
    let location = match response {
        Ok(response) => response.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_owned),
        Err(ureq::Error::StatusCode(code)) if (300..400).contains(&code) => None,
        // GitHub answers 404 for a repository the caller may not see.
        Err(ureq::Error::StatusCode(404)) => {
            return Err(format!("{url}: not found -- no release yet, or a private repository, which needs the installer script and the GitHub CLI"));
        }
        Err(error) => return Err(format!("{url}: {error}")),
    };
    // ureq 3 hands a redirect back as a response when told not to follow it.
    let location = location.ok_or_else(|| format!("{url}: no release yet"))?;
    tag_of(&location).ok_or_else(|| format!("{url}: redirected to {location}, which names no release"))
}

/// `v0.1.8` from `.../releases/tag/v0.1.8`.
fn tag_of(location: &str) -> Option<String> {
    location.rsplit_once("/tag/").map(|(_, tag)| tag.trim_end_matches('/').to_owned())
}

/// Whether `latest` is newer than what is running: tags are `vMAJOR.MINOR.PATCH`.
fn newer(latest: &str, running: &str) -> bool {
    let parse = |text: &str| -> Option<Vec<u64>> {
        text.trim_start_matches('v').split('.').map(|part| part.parse().ok()).collect()
    };
    match (parse(latest), parse(running)) {
        (Some(latest), Some(running)) => latest > running,
        _ => latest.trim_start_matches('v') != running,
    }
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    ureq::get(url)
        .call()
        .map_err(|error| format!("{url}: {error}"))?
        .into_body()
        .into_reader()
        .take(MAX_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{url}: {error}"))?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The new binary in place of the running one, the old one out of the way.
fn replace(exe: &Path, bytes: &[u8]) -> Result<(), String> {
    let fresh = exe.with_extension("new");
    std::fs::write(&fresh, bytes).map_err(|error| format!("{}: {error}", fresh.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("{}: {error}", fresh.display()))?;
        std::fs::rename(&fresh, exe).map_err(|error| format!("{}: {error}", exe.display()))?;
    }
    #[cfg(windows)]
    {
        // A running binary can be renamed but not overwritten or deleted;
        // it steps aside under a name of its own, since the one from the
        // last update may still be held by a session running since then.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default();
        let aside = exe.with_extension(format!("exe.old-{stamp}"));
        std::fs::rename(exe, &aside).map_err(|error| format!("{}: {error}", exe.display()))?;
        if let Err(error) = std::fs::rename(&fresh, exe) {
            let _ = std::fs::rename(&aside, exe);
            return Err(format!("{}: {error}", exe.display()));
        }
        // Whatever no server holds any more goes now; the rest at a later update.
        if let Some(dir) = exe.parent() {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with("omega.exe.old") {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }
    Ok(())
}

/// Check for a newer release and, unless `check_only`, install it and write
/// the installed integrations again.
pub fn run(check_only: bool) -> Result<(), String> {
    let repository = std::env::var("OMEGA_REPO").unwrap_or_else(|_| REPOSITORY.to_owned());
    let running = env!("CARGO_PKG_VERSION");
    let latest = latest_tag(&repository)?;
    if !newer(&latest, running) {
        println!("omega {running} is the latest release.");
        return Ok(());
    }
    println!("omega {running} -> {latest}");
    if check_only {
        return Ok(());
    }
    let asset = asset().ok_or_else(|| {
        format!(
            "no release is built for {} {}; build from source with `cargo install --git https://github.com/{repository}`",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let base = format!("https://github.com/{repository}/releases/download/{latest}");
    eprintln!("downloading {asset}");
    let bytes = download(&format!("{base}/{asset}"))?;
    let expected = String::from_utf8_lossy(&download(&format!("{base}/{asset}.sha256"))?)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let digest = hex(&Sha256::digest(&bytes));
    if digest != expected {
        return Err(format!("{asset}: digest {digest} is not the published {expected}"));
    }
    let exe: PathBuf = std::env::current_exe().map_err(|error| error.to_string())?;
    replace(&exe, &bytes)?;
    println!("installed {latest} at {}", exe.display());

    // The instructions and the sub-agent text may have changed with the
    // release; what was installed is installed again, and unchanged files
    // stay byte for byte.
    let request = crate::install::Request { yes: true, ..Default::default() };
    crate::install::run(crate::install::Mode::Install, request)
}

#[cfg(test)]
mod tests {
    use super::{newer, tag_of};

    #[test]
    fn a_release_tag_is_read_off_the_redirect_and_compared_by_number() {
        assert_eq!(tag_of("https://github.com/x/y/releases/tag/v0.1.8").as_deref(), Some("v0.1.8"));
        assert_eq!(tag_of("https://github.com/x/y/releases"), None);
        assert!(newer("v0.1.9", "0.1.8"));
        assert!(newer("v0.2.0", "0.1.12"));
        assert!(newer("v1.0.0", "0.9.9"));
        assert!(!newer("v0.1.8", "0.1.8"));
        assert!(!newer("v0.1.7", "0.1.8"));
    }
}
