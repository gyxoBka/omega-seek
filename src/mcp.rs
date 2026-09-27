//! The MCP face: `search`, `usages`, `grep` and `outline` over stdio, line-delimited JSON-RPC.
//!
//! The server is started in one repository and answers about it unless a call
//! says otherwise. What keeps an agent from being answered about the wrong
//! code is that it is always told where an answer came from when that could be
//! in doubt: at connection (`instructions`), when the repository has worktrees
//! the agent might be in, and whenever a call looked somewhere else.

use crate::access::Access;
use crate::index::Index;
use crate::roots::{self, Resolved};
use crate::search::{Content, Options, render, search};
use crate::watch::{Changes, Watcher};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

// Every session pays for these words before it asks anything: each says what
// the tool is for, what it replaces and what comes back, once.

const DESCRIPTION: &str = "Find code in this repository by what it does or what it is called. Use \
INSTEAD of grep/glob/find to locate where something is implemented, defined or handled. Answers \
are `path:start-end  declared names` and the numbered source lines: read or edit from them without \
another lookup. Ask in 3-6 words the code itself would use (`retry backoff http client`, \
`UserRepository save`); an exact identifier returns its whole declaration and how widely it is \
used. After a miss rephrase with synonyms, do not fall back to grep.";

const USAGES_DESCRIPTION: &str = "Where an identifier is declared and used, or where a literal text \
is written. Use INSTEAD of grep for callers, references and the impact of a change. An identifier \
(`ValidateToken`, `Session::refresh`) matches as a whole word, case-sensitively, in code, comment \
lines dropped, the declaration first. Anything else (an error message, a route like `/api/users`, \
a config key, a dotted name the code writes in quotes) matches exactly as written, in code and \
configuration. Each line carries the [declaration] it sits in; files grouped, tests last, output \
bounded. Matching is by name, not by type.";

const GREP_DESCRIPTION: &str = "Regular-expression search over this repository's own source. Use \
INSTEAD of grep, rg or a built-in Grep tool, which also walk node_modules, vendor, build output \
and generated files. One line at a time, case-sensitive; `(?i)` ignores case. Each matching line \
carries its number and the [declaration] it sits in; files grouped, tests last, output bounded. \
For a name or a literal text use `usages`; to find code by what it does, `search`.";

const OUTLINE_DESCRIPTION: &str = "The table of contents of a file (every declaration with its \
line and signature, nesting kept) or of a directory (its files with what each declares, and its \
subdirectories). Call BEFORE reading a file you have not seen: a few dozen lines instead of the \
whole file, and the line to start reading from.";

/// Said in full once, where an agent's first call usually goes, and at
/// connection (`instructions`); the other tools only recall it.
const ROOT_DESCRIPTION: &str = "Look in this directory instead of omega's repository: the git \
worktree you work in, or a directory the user gave this repository access to (`../backend`, a \
parent of several `..`). Absolute, or relative to omega's repository. Omit only when you work in \
omega's own repository.";
const ROOT_RECALLED: &str = "Another directory to look in, as in `search`: your git worktree, or one this repository was given access to.";

/// Roots kept indexed at once; the least recently asked about makes room.
const OPEN_ROOTS: usize = 4;
/// A root named in a call with more indexable files than this was almost
/// certainly a mistake. Home is exempt: it is where the agent was started, and
/// a monorepo that large is still the code being worked on.
const WIDEST_ROOT: usize = 60_000;
const SYNC: Duration = Duration::from_millis(300);
const CHECK_EVERY: Duration = Duration::from_secs(600);
const GIVE_UP: u32 = 3;

struct Open {
    root: PathBuf,
    /// Taken out while it is being refreshed, which consumes it.
    index: Option<Index>,
    used: Instant,
    watching: Watching,
}

type Opened = Result<(Index, Option<Watcher>), String>;

struct Watching {
    watcher: Option<Watcher>,
    misses: u32,
    checked: Instant,
}

impl Watching {
    fn start(root: &Path) -> Self {
        let watcher = if std::env::var_os("OMEGA_NO_WATCH").is_some() { None } else { Watcher::start(root) };
        Self::from(watcher)
    }

    fn from(watcher: Option<Watcher>) -> Self {
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

struct Server {
    home: PathBuf,
    model: Option<PathBuf>,
    open: Vec<Open>,
    /// Home, being indexed since the server started. An agent gives a server
    /// seconds to answer `initialize`, and a first index of a large repository
    /// takes longer: the first call waits for its words instead, and the
    /// vectors follow while it answers.
    indexing: Option<Receiver<Opened>>,
}

impl Server {
    /// The index of `root`, opened on first use and brought up to date.
    fn index(&mut self, root: &Path) -> Result<&Index, String> {
        if root == self.home {
            if let Some(indexing) = self.indexing.take() {
                // A failure is reported to this call; the next one tries again.
                let (index, watcher) = indexing.recv().map_err(|_| "indexing stopped unexpectedly".to_owned())??;
                self.open.push(Open {
                    root: root.to_path_buf(),
                    index: Some(index),
                    used: Instant::now(),
                    watching: Watching::from(watcher),
                });
            }
        }
        let position = match self.open.iter().position(|open| open.root == root) {
            Some(position) => position,
            None => {
                if root != self.home && Index::holds_more_than(root, WIDEST_ROOT) {
                    return Err(format!(
                        "{} holds more than {WIDEST_ROOT} source files: name a repository, or a directory that holds a few of them",
                        roots::shown(root)
                    ));
                }
                let watching = Watching::start(root);
                let (mut index, upkeep) = Index::open_lexical(root, self.model.as_deref(), &|_| {})?;
                if !upkeep.is_idle() {
                    std::thread::spawn(move || upkeep.run(&|_| {}));
                }
                if root != self.home {
                    index.label = format!("{}/", roots::shown(root));
                }
                if self.open.len() == OPEN_ROOTS {
                    // Never the home root: every call without a `root` wants it.
                    let oldest = (0..self.open.len())
                        .filter(|&at| self.open[at].root != self.home)
                        .min_by_key(|&at| self.open[at].used);
                    if let Some(oldest) = oldest {
                        self.open.swap_remove(oldest);
                    }
                }
                self.open.push(Open {
                    root: root.to_path_buf(),
                    index: Some(index),
                    used: Instant::now(),
                    watching,
                });
                self.open.len() - 1
            }
        };
        let open = &mut self.open[position];
        open.used = Instant::now();
        let index = open.index.take();
        open.index = index.map(|index| open.watching.refresh(&open.root, index));
        open.index.as_ref().ok_or_else(|| "the index was lost while refreshing".to_owned())
    }
}

pub fn serve(root: &Path, model: Option<&Path>) -> Result<(), String> {
    let home = roots::clean(&root.canonicalize().map_err(|error| format!("{}: {error}", root.display()))?);
    // The stores of repositories no longer worked on go, out of the way.
    std::thread::spawn(|| {
        if let Some(dir) = crate::paths::stores() {
            let _ = crate::store::prune(&dir, crate::store::ABANDONED);
        }
    });
    let (sender, indexing) = channel();
    {
        let (home, model) = (home.clone(), model.map(Path::to_path_buf));
        std::thread::spawn(move || {
            let watching = Watching::start(&home);
            match Index::open_lexical(&home, model.as_deref(), &|_| {}) {
                Ok((index, upkeep)) => {
                    let _ = sender.send(Ok((index, watching.watcher)));
                    let _ = upkeep.run(&|_| {});
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                }
            }
        });
    }
    let mut server = Server {
        home: home.clone(),
        model: model.map(Path::to_path_buf),
        open: Vec::new(),
        indexing: Some(indexing),
    };

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| error.to_string())?;
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue; // a notification
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": message["params"]["protocolVersion"].as_str().unwrap_or("2024-11-05"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "omega", "version": env!("CARGO_PKG_VERSION")},
                "instructions": instructions(&home),
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => Ok(call(&mut server, &message["params"])),
            _ => Err(json!({"code": -32601, "message": "method not found"})),
        };
        let reply = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
        writeln!(stdout, "{reply}").and_then(|()| stdout.flush()).map_err(|error| error.to_string())?;
    }
    // The session is over: what it read again is kept for the next one.
    for open in &mut server.open {
        if let Some(index) = open.index.as_mut() {
            index.persist();
        }
    }
    Ok(())
}

/// What the agent is told once, at connection: which directory a call without
/// a `root` is about. It knows its own working directory; this is the other
/// half of noticing that the two differ.
fn instructions(home: &Path) -> String {
    format!(
        "omega is indexing {home}. Calls without `root` search this directory only. If your working \
         directory is a different checkout -- a git worktree, a sibling repository -- pass `root` with \
         that directory, or you will be answered about code you are not editing. Directories outside \
         this repository are readable only once the user has given it access to them (`omega access \
         add`); `outline` with `root: \"..\"` says which are.",
        home = roots::shown(home)
    )
}

fn tools() -> Value {
    let root = json!({"type": "string", "description": ROOT_DESCRIPTION});
    let recalled = json!({"type": "string", "description": ROOT_RECALLED});
    json!([
        {
            "name": "search",
            "description": DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "What the code does, or an identifier."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 30, "description": "Results (default 8)."},
                    "path": {"type": "string", "description": "Only files whose path contains this."},
                    "content": {"type": "string", "enum": ["code", "docs", "config", "all"], "description": "What to search (default code; a document whose heading says what was asked is answered anyway)."},
                    "root": root,
                },
                "required": ["query"],
            },
        },
        {
            "name": "usages",
            "description": USAGES_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": {"type": "string", "description": "An identifier, or literal text as written."},
                    "path": {"type": "string", "description": "Only files whose path contains this."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Lines (default 40)."},
                    "root": recalled,
                },
                "required": ["symbol"],
            },
        },
        {
            "name": "grep",
            "description": GREP_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Rust/RE2 syntax, no look-around or backreferences: `func \\w+Handler\\(`, `(?i)todo|fixme`."},
                    "path": {"type": "string", "description": "Only files whose path contains this."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Lines (default 40)."},
                    "root": recalled,
                },
                "required": ["pattern"],
            },
        },
        {
            "name": "outline",
            "description": OUTLINE_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "A file or directory, relative to the root; a bare file name works when unique. Empty for the root."},
                    "root": recalled,
                },
                "required": ["path"],
            },
        },
    ])
}

fn text(text: String) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}

fn failure(text: String) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": true})
}

fn call(server: &mut Server, params: &Value) -> Value {
    let tool = params["name"].as_str().unwrap_or_default();
    let arguments = &params["arguments"];
    if !matches!(tool, "search" | "usages" | "grep" | "outline") {
        return failure(format!("Unknown tool `{tool}`. The tools are `search`, `usages`, `grep` and `outline`."));
    }
    let resolved = match roots::resolve(&server.home, arguments["root"].as_str(), arguments["path"].as_str()) {
        Ok(resolved) => resolved,
        Err(reason) => return failure(reason),
    };
    // Refused before anything is read or indexed. Settings that cannot be
    // read give nothing, rather than stopping the server.
    if resolved.elsewhere {
        let access = Access::load().unwrap_or_default();
        if !access.allows(&server.home, &resolved.root) {
            let refusal = access.refusal(&server.home, &resolved.root);
            // `outline("", root="..")` is how an agent asks what is next
            // door: what it may read is the answer, not an error.
            return if tool == "outline" { text(refusal) } else { failure(refusal) };
        }
    }
    let home = server.home.clone();
    let index = match server.index(&resolved.root) {
        Ok(index) => index,
        Err(reason) => return failure(reason),
    };
    let body = match tool {
        "search" => {
            let Some(query) = arguments["query"].as_str().filter(|query| !query.trim().is_empty()) else {
                return failure("`query` is required.".to_owned());
            };
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
            let Some(symbol) = arguments["symbol"].as_str() else {
                return failure("`symbol` is required.".to_owned());
            };
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
            let Some(pattern) = arguments["pattern"].as_str() else {
                return failure("`pattern` is required.".to_owned());
            };
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
    };
    text(format!("{}{body}", heading(&home, &resolved)))
}

/// Where this answer came from, said only when it could be in doubt: the call
/// looked somewhere other than home, or home has worktrees the agent may be in.
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
