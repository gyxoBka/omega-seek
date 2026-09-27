use crate::access::Access;
use crate::index::Index;
use crate::roots::{self, Resolved};
use crate::search::{Content, Options, render, search};
use crate::watch::{Changes, Watcher};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WIDEST_ROOT: usize = 60_000;
const SYNC: Duration = Duration::from_millis(300);
const CHECK_EVERY: Duration = Duration::from_secs(600);
const GIVE_UP: u32 = 3;

type Opened = Result<(Index, Watching), String>;

struct Watching {
    watcher: Option<Watcher>,
    misses: u32,
    checked: Instant,
}

impl Watching {
    fn start(root: &Path) -> Self {
        let watcher = if std::env::var_os("OMEGA_NO_WATCH").is_some() { None } else { Watcher::start(root) };
        Self {
            watcher,
            misses: 0,
            checked: Instant::now(),
        }
    }

    fn refresh(&mut self, root: &Path, index: Index) -> Index {
        let Some(watcher) = &self.watcher else {
            return index.refreshed();
        };
        match watcher.sync(SYNC) {
            Some(Changes::Paths(paths)) => {
                self.misses = 0;
                let index = index.refreshed_with(&paths);
                if self.checked.elapsed() < CHECK_EVERY {
                    return index;
                }
                self.checked = Instant::now();
                let (index, missed) = index.refreshed_full();
                if missed {
                    eprintln!("omega: the watcher on {} missed changes; walking the tree from now on", roots::shown(root));
                    self.watcher = None;
                }
                index
            }
            Some(Changes::Everything) => {
                self.misses = 0;
                index.refreshed()
            }
            None => {
                self.misses += 1;
                self.watcher = if self.misses < GIVE_UP { Watcher::start(root) } else { None };
                index.refreshed()
            }
        }
    }
}

struct Slot {
    root: PathBuf,
    state: Mutex<State>,
}

struct State {
    index: Option<Index>,
    watching: Option<Watching>,
    pending: Option<Receiver<Opened>>,
    used: Instant,
}

#[derive(Debug, Clone)]
pub struct Opening {
    pub root: PathBuf,
    pub files: usize,
    pub chunks: usize,
    pub busy: bool,
}

pub struct Engine {
    model: Option<PathBuf>,
    most: usize,
    slots: Mutex<Vec<Arc<Slot>>>,
    pinned: Mutex<HashMap<PathBuf, usize>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").field("model", &self.model).finish_non_exhaustive()
    }
}

impl Engine {
    #[must_use]
    pub fn new(model: Option<PathBuf>, most: usize) -> Self {
        Self {
            model,
            most,
            slots: Mutex::new(Vec::new()),
            pinned: Mutex::new(HashMap::new()),
        }
    }

    pub fn pin(&self, home: &Path) {
        if let Ok(mut pinned) = self.pinned.lock() {
            *pinned.entry(home.to_path_buf()).or_default() += 1;
        }
    }

    pub fn unpin(&self, home: &Path) {
        if let Ok(mut pinned) = self.pinned.lock() {
            if let Some(count) = pinned.get_mut(home) {
                *count -= 1;
                if *count == 0 {
                    pinned.remove(home);
                }
            }
        }
    }

    pub fn prepare(&self, root: &Path) {
        let Ok(mut slots) = self.slots.lock() else { return };
        if slots.iter().any(|slot| slot.root == root) {
            return;
        }
        let (sender, receiver) = channel();
        let (opened_root, model) = (root.to_path_buf(), self.model.clone());
        std::thread::spawn(move || {
            let watching = Watching::start(&opened_root);
            match Index::open_lexical(&opened_root, model.as_deref(), &|_| {}) {
                Ok((index, upkeep)) => {
                    let _ = sender.send(Ok((index, watching)));
                    let _ = upkeep.run(&|_| {});
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                }
            }
        });
        slots.push(Arc::new(Slot {
            root: root.to_path_buf(),
            state: Mutex::new(State {
                index: None,
                watching: None,
                pending: Some(receiver),
                used: Instant::now(),
            }),
        }));
        self.evict(&mut slots);
    }

    fn evict(&self, slots: &mut Vec<Arc<Slot>>) {
        if slots.len() <= self.most {
            return;
        }
        let pinned = self.pinned.lock().map(|pinned| pinned.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
        let oldest = slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| Arc::strong_count(slot) == 1 && !pinned.contains(&slot.root))
            .filter_map(|(at, slot)| slot.state.try_lock().ok().map(|state| (at, state.used)))
            .min_by_key(|&(_, used)| used)
            .map(|(at, _)| at);
        if let Some(oldest) = oldest {
            let slot = slots.swap_remove(oldest);
            if let Ok(mut state) = slot.state.lock() {
                if let Some(index) = state.index.as_mut() {
                    index.persist();
                }
            }
        }
    }

    fn slot(&self, home: &Path, root: &Path) -> Result<Arc<Slot>, String> {
        if let Some(slot) = self.slots.lock().ok().and_then(|slots| slots.iter().find(|slot| slot.root == root).cloned()) {
            return Ok(slot);
        }
        if root != home && Index::holds_more_than(root, WIDEST_ROOT) {
            return Err(format!(
                "{} holds more than {WIDEST_ROOT} source files: name a repository, or a directory that holds a few of them",
                roots::shown(root)
            ));
        }
        let mut slots = self.slots.lock().map_err(|_| "the index registry is poisoned".to_owned())?;
        if let Some(slot) = slots.iter().find(|slot| slot.root == root) {
            return Ok(Arc::clone(slot));
        }
        let slot = Arc::new(Slot {
            root: root.to_path_buf(),
            state: Mutex::new(State {
                index: None,
                watching: None,
                pending: None,
                used: Instant::now(),
            }),
        });
        slots.push(Arc::clone(&slot));
        self.evict(&mut slots);
        Ok(slot)
    }

    fn with_index<R>(&self, home: &Path, root: &Path, work: impl FnOnce(&Index) -> R) -> Result<R, String> {
        let slot = self.slot(home, root)?;
        let mut state = slot.state.lock().map_err(|_| "an index is poisoned".to_owned())?;
        state.used = Instant::now();
        if state.index.is_none() {
            let (index, watching) = match state.pending.take() {
                Some(pending) => pending.recv().map_err(|_| "indexing stopped unexpectedly".to_owned())??,
                None => {
                    let watching = Watching::start(root);
                    let (index, upkeep) = Index::open_lexical(root, self.model.as_deref(), &|_| {})?;
                    if !upkeep.is_idle() {
                        std::thread::spawn(move || upkeep.run(&|_| {}));
                    }
                    (index, watching)
                }
            };
            state.index = Some(index);
            state.watching = Some(watching);
        }
        let index = state.index.take();
        let mut watching = state.watching.take();
        state.index = index.map(|index| match watching.as_mut() {
            Some(watching) => watching.refresh(root, index),
            None => index.refreshed(),
        });
        state.watching = watching;
        let index = state.index.as_mut().ok_or("the index was lost while refreshing")?;
        index.label = if root == home { String::new() } else { format!("{}/", roots::shown(root)) };
        Ok(work(index))
    }

    pub fn persist(&self) {
        let slots = self.slots.lock().map(|slots| slots.clone()).unwrap_or_default();
        for slot in slots {
            if let Ok(mut state) = slot.state.lock() {
                if let Some(index) = state.index.as_mut() {
                    index.persist();
                }
            }
        }
    }

    #[must_use]
    pub fn open(&self) -> Vec<Opening> {
        let slots = self.slots.lock().map(|slots| slots.clone()).unwrap_or_default();
        slots
            .iter()
            .map(|slot| match slot.state.try_lock() {
                Ok(state) if state.index.is_none() => Opening {
                    root: slot.root.clone(),
                    files: 0,
                    chunks: 0,
                    busy: true,
                },
                Ok(state) => Opening {
                    root: slot.root.clone(),
                    files: state.index.as_ref().map_or(0, |index| index.files.len()),
                    chunks: state.index.as_ref().map_or(0, |index| index.chunks.len()),
                    busy: false,
                },
                Err(_) => Opening {
                    root: slot.root.clone(),
                    files: 0,
                    chunks: 0,
                    busy: true,
                },
            })
            .collect()
    }

    #[must_use]
    pub fn call(&self, home: &Path, params: &Value) -> Value {
        let tool = params["name"].as_str().unwrap_or_default();
        let arguments = &params["arguments"];
        if !matches!(tool, "search" | "usages" | "grep" | "outline") {
            return failure(format!("Unknown tool `{tool}`. The tools are `search`, `usages`, `grep` and `outline`."));
        }
        let resolved = match roots::resolve(home, arguments["root"].as_str(), arguments["path"].as_str()) {
            Ok(resolved) => resolved,
            Err(reason) => return failure(reason),
        };
        if resolved.elsewhere {
            let access = Access::load().unwrap_or_default();
            if !access.allows(home, &resolved.root) {
                let refusal = access.refusal(home, &resolved.root);
                return if tool == "outline" { text(refusal) } else { failure(refusal) };
            }
        }
        let answered = self.with_index(home, &resolved.root, |index| answer(index, tool, arguments, &resolved));
        match answered {
            Ok(Ok(body)) => text(format!("{}{body}", heading(home, &resolved))),
            Ok(Err(reason)) | Err(reason) => failure(reason),
        }
    }
}

fn answer(index: &Index, tool: &str, arguments: &Value, resolved: &Resolved) -> Result<String, String> {
    Ok(match tool {
        "search" => {
            let query = arguments["query"].as_str().filter(|query| !query.trim().is_empty()).ok_or("`query` is required.")?;
            let mut options = Options {
                path: resolved.within.clone(),
                ..Options::default()
            };
            if let Some(limit) = arguments["limit"].as_u64() {
                options.limit = (limit as usize).clamp(1, 30);
            }
            if let Some(content) = arguments["content"].as_str().and_then(Content::parse) {
                options.content = content;
            }
            let hits = search(index, query, &options);
            let mut body = render(index, query, &hits, &options);
            let note = if index.model.is_none() {
                Some("(lexical ranking only: run `omega model install` once to add the semantic channel)")
            } else if index.embedding() {
                Some("(lexical ranking only for now: the semantic channel is still being built)")
            } else {
                None
            };
            if let Some(note) = note {
                if !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str(note);
            }
            body
        }
        "usages" => {
            let symbol = arguments["symbol"].as_str().ok_or("`symbol` is required.")?;
            let mut options = crate::usages::Options {
                path: resolved.within.clone(),
                ..Default::default()
            };
            if let Some(limit) = arguments["limit"].as_u64() {
                options.limit = (limit as usize).clamp(1, 200);
            }
            crate::usages::usages(index, symbol, &options)
        }
        "grep" => {
            let pattern = arguments["pattern"].as_str().ok_or("`pattern` is required.")?;
            let mut options = crate::usages::Options {
                path: resolved.within.clone(),
                ..Default::default()
            };
            if let Some(limit) = arguments["limit"].as_u64() {
                options.limit = (limit as usize).clamp(1, 200);
            }
            crate::usages::grep(index, pattern, &options)
        }
        _ => crate::outline::outline(index, resolved.within.as_deref().unwrap_or_default()),
    })
}

#[must_use]
pub fn text(text: String) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}

#[must_use]
pub fn failure(text: String) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": true})
}

fn heading(home: &Path, resolved: &Resolved) -> String {
    if resolved.elsewhere {
        return format!("in {}:\n\n", roots::shown(&resolved.root));
    }
    let worktrees = roots::linked_worktrees(home);
    if worktrees.is_empty() {
        return String::new();
    }
    let listed: Vec<String> = worktrees
        .iter()
        .map(|worktree| match &worktree.branch {
            Some(branch) => format!("{} ({branch})", roots::shown(&worktree.path)),
            None => roots::shown(&worktree.path),
        })
        .collect();
    format!(
        "Answering from {} (main checkout). Worktrees: {}. If you are working in one, pass it as `root`.\n\n",
        roots::shown(home),
        listed.join(", ")
    )
}
