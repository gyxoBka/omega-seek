use crate::access::Access;
use crate::store::{self, Standing};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_DAYS: u64 = 30;

fn days() -> Option<u64> {
    Access::load().map_or(Some(DEFAULT_DAYS), |settings| settings.cache_days())
}

#[must_use]
pub fn clean() -> (usize, u64) {
    let Some(dir) = crate::paths::stores() else { return (0, 0) };
    store::prune(&dir, days().map(|days| Duration::from_secs(days * 24 * 3600)))
}

fn size(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1u64 << 20) as f64)
}

fn files(dir: &Path) -> Vec<(PathBuf, u64, Standing)> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    let standing = store::standing(&path)?;
                    let bytes = entry.metadata().map_or(0, |meta| meta.len());
                    Some((path, bytes, standing))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn described(days: Option<u64>) -> String {
    match days {
        Some(1) => "stores not opened for a day are removed".to_owned(),
        Some(days) => format!("stores not opened for {days} days are removed"),
        None => "off: only outdated stores are removed".to_owned(),
    }
}

pub fn run(arguments: &[String]) -> Result<(), String> {
    let dir = crate::paths::stores().ok_or("cannot tell where the index stores are kept")?;
    match arguments.first().map(String::as_str) {
        None | Some("status") => {
            let found = files(&dir);
            let current: Vec<_> = found.iter().filter(|(.., standing)| *standing == Standing::Current).collect();
            let outdated: Vec<_> = found.iter().filter(|(.., standing)| *standing != Standing::Current).collect();
            println!("  {}", dir.display());
            println!(
                "  {} store{}, {}",
                current.len(),
                if current.len() == 1 { "" } else { "s" },
                size(current.iter().map(|(_, bytes, _)| bytes).sum())
            );
            if !outdated.is_empty() {
                println!(
                    "  {} outdated or left behind, {}: `omega cache clean` removes them",
                    outdated.len(),
                    size(outdated.iter().map(|(_, bytes, _)| bytes).sum())
                );
            }
            println!("  automatic cleaning: {}", described(days()));
            Ok(())
        }
        Some("clean") => {
            let (count, bytes) = clean();
            println!("  removed {count} file{} ({}) from {}", if count == 1 { "" } else { "s" }, size(bytes), dir.display());
            Ok(())
        }
        Some("clear") => {
            let stopped = crate::daemon::stop_all();
            if stopped > 0 {
                println!("  stopped the daemon, which holds the stores open");
            }
            let found = files(&dir);
            let mut removed = (0usize, 0u64);
            for (path, bytes, _) in &found {
                if std::fs::remove_file(path).is_ok() {
                    removed.0 += 1;
                    removed.1 += bytes;
                }
            }
            let _ = std::fs::remove_dir(&dir);
            println!(
                "  removed {} file{} ({}); each repository is indexed again at its next call",
                removed.0,
                if removed.0 == 1 { "" } else { "s" },
                size(removed.1)
            );
            if removed.0 < found.len() {
                println!("  {} could not be removed: a session running without the daemon may hold them", found.len() - removed.0);
            }
            Ok(())
        }
        Some("auto") => {
            let days = match arguments.get(1).map(String::as_str) {
                Some("off") => None,
                Some(days) => Some(days.parse::<u64>().ok().filter(|&days| days > 0).ok_or("`omega cache auto` takes a number of days, or off")?),
                None => {
                    println!("  automatic cleaning: {}", described(self::days()));
                    return Ok(());
                }
            };
            let mut settings = Access::load()?;
            settings.set_cache_days(days);
            settings.save()?;
            println!("  automatic cleaning: {}", described(days));
            Ok(())
        }
        Some(other) => Err(format!("unknown `omega cache {other}`; use status, clean, clear, auto <days|off>")),
    }
}
