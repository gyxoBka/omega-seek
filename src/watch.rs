use notify::{EventKind, RecursiveMode, Watcher as _};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const COOKIE: &str = ".omega-cookie-";
const MANY: usize = 256;

#[derive(Debug)]
pub enum Changes {
    Paths(Vec<PathBuf>),
    Everything,
}

#[derive(Default)]
struct State {
    changed: HashSet<PathBuf>,
    everything: bool,
    cookies: HashSet<String>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    arrived: Condvar,
}

pub struct Watcher {
    _watcher: notify::RecommendedWatcher,
    shared: Arc<Shared>,
    root: PathBuf,
    next: AtomicU64,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher").field("root", &self.root).finish_non_exhaustive()
    }
}

impl Watcher {
    #[must_use]
    pub fn start(root: &Path) -> Option<Self> {
        let shared = Arc::new(Shared::default());
        let reported = Arc::clone(&shared);
        let watched = root.to_path_buf();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            record(&reported, &watched, event);
        })
        .ok()?;
        watcher.watch(root, RecursiveMode::Recursive).ok()?;
        sweep_cookies(root);
        Some(Self {
            _watcher: watcher,
            shared,
            root: root.to_path_buf(),
            next: AtomicU64::new(0),
        })
    }

    #[must_use]
    pub fn sync(&self, timeout: Duration) -> Option<Changes> {
        let name = format!("{COOKIE}{}-{}", std::process::id(), self.next.fetch_add(1, Ordering::Relaxed));
        let cookie = self.root.join(&name);
        std::fs::write(&cookie, b"").ok()?;
        let deadline = Instant::now() + timeout;
        let mut state = self.shared.state.lock().ok()?;
        let arrived = loop {
            if state.cookies.remove(&name) {
                break true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break false;
            }
            state = self.shared.arrived.wait_timeout(state, left).ok()?.0;
        };
        let changed = std::mem::take(&mut state.changed);
        let everything = std::mem::take(&mut state.everything);
        drop(state);
        let _ = std::fs::remove_file(&cookie);
        if !arrived {
            return None;
        }
        Some(if everything || changed.len() > MANY || changed.contains(&self.root) {
            Changes::Everything
        } else {
            let mut changed: Vec<PathBuf> = changed.into_iter().collect();
            changed.sort();
            Changes::Paths(changed)
        })
    }
}

fn record(shared: &Shared, root: &Path, event: notify::Result<notify::Event>) {
    let Ok(mut state) = shared.state.lock() else { return };
    let Ok(event) = event else {
        state.everything = true;
        return;
    };
    if event.need_rescan() {
        state.everything = true;
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return;
    }
    for path in event.paths {
        let relative = path.strip_prefix(root).unwrap_or(&path);
        let name = relative.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        if name.starts_with(COOKIE) && relative.parent().is_some_and(|parent| parent.as_os_str().is_empty()) {
            if !matches!(event.kind, EventKind::Remove(_)) {
                state.cookies.insert(name);
                shared.arrived.notify_all();
            }
            continue;
        }
        if relative.components().next().is_some_and(|first| first.as_os_str() == ".git") {
            if relative.ends_with("info/exclude") {
                state.everything = true;
            }
            continue;
        }
        if matches!(name.as_str(), ".gitignore" | ".ignore" | ".omegaignore") {
            state.everything = true;
        }
        state.changed.insert(path);
    }
}

fn sweep_cookies(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(60));
        if stale && entry.file_name().to_string_lossy().starts_with(COOKIE) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
