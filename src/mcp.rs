//! The MCP face: `search`, `usages` and `outline` over stdio, line-delimited JSON-RPC.
//!
//! The server is started in one repository and answers about it unless a call
//! says otherwise. What keeps an agent from being answered about the wrong
//! code is that it is always told where an answer came from when that could be
//! in doubt: at connection (`instructions`), when the repository has worktrees
//! the agent might be in, and whenever a call looked somewhere else.

use crate::index::Index;
use crate::roots::{self, Resolved};
use crate::search::{Content, Options, render, search};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DESCRIPTION: &str = "Search this repository's code. Use INSTEAD of grep/glob/find whenever you \
need to locate where something is implemented, defined or handled. Returns the best matching code \
blocks as `path:start-end  declared names` followed by the source lines with line numbers, so the \
result can be read or edited directly without another lookup.\n\
Query tips: write what the code does or is called, using words likely to appear in identifiers and \
comments (e.g. `retry backoff http client`, `parse config file`, `UserRepository save`). An exact \
identifier returns its whole declaration and how widely it is used. If the first answer misses, \
rephrase with synonyms rather than falling back to grep.";

const USAGES_DESCRIPTION: &str = "Every place an identifier is declared and used -- or every place \
a literal text is written. Use INSTEAD of grep. For an identifier (`ValidateToken`, `Session::refresh`): \
whole-word, case-sensitive matches in code, comment lines dropped, the declaration first; use it \
for callers, references and the impact of a change. For anything else (an error message, a route \
like `/api/users`, a config key, any phrase, in any language): exact matches as written, in code \
and configuration. Either way each line is labelled with the [declaration] it sits in, files are \
grouped, tests come last, and output is bounded. Matching is by name, not by type.";

const OUTLINE_DESCRIPTION: &str = "The table of contents of a file or directory. For a file: every \
declaration with its line number and signature, nesting kept. For a directory: its files with \
what each declares, and its subdirectories. Call this BEFORE reading a file you have not seen: \
it costs a few dozen lines instead of the whole file, tells you whether the file matters, and \
gives the line to start reading from.";

const ROOT_DESCRIPTION: &str = "Search this directory instead of the repository omega was started \
in: a git worktree you are working in, a sibling repository (`../backend`), or a parent holding \
several repositories (`..`). Absolute, or relative to omega's repository. Omit it for the current \
repository.";

/// How long an index is trusted before its tree is looked at again.
const FRESH_FOR: Duration = Duration::from_secs(2);
/// Roots kept indexed at once; the least recently asked about makes room.
const OPEN_ROOTS: usize = 4;
/// A root with more indexable files than this was almost certainly a mistake.
const WIDEST_ROOT: usize = 60_000;

struct Open {
    root: PathBuf,
    /// Taken out while it is being refreshed, which consumes it.
    index: Option<Index>,
    checked: Instant,
    used: Instant,
}

struct Server {
    home: PathBuf,
    model: Option<PathBuf>,
    open: Vec<Open>,
}

impl Server {
    /// The index of `root`, opened on first use and brought up to date.
    fn index(&mut self, root: &Path) -> Result<&Index, String> {
        let position = match self.open.iter().position(|open| open.root == root) {
            Some(position) => position,
            None => {
                if Index::holds_more_than(root, WIDEST_ROOT) {
                    return Err(format!(
                        "{} holds more than {WIDEST_ROOT} source files: name a repository, or a directory that holds a few of them",
                        roots::shown(root)
                    ));
                }
                let mut index = Index::open(root, self.model.as_deref())?;
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
                    checked: Instant::now(),
                    used: Instant::now(),
                });
                self.open.len() - 1
            }
        };
        let open = &mut self.open[position];
        open.used = Instant::now();
        if open.checked.elapsed() > FRESH_FOR {
            open.index = open.index.take().map(Index::refreshed);
            open.checked = Instant::now();
        }
        open.index.as_ref().ok_or_else(|| "the index was lost while refreshing".to_owned())
    }
}

pub fn serve(root: &Path, model: Option<&Path>) -> Result<(), String> {
    let home = roots::clean(&root.canonicalize().map_err(|error| format!("{}: {error}", root.display()))?);
    let mut server = Server {
        home: home.clone(),
        model: model.map(Path::to_path_buf),
        open: Vec::new(),
    };
    server.index(&home)?;

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
    Ok(())
}

/// What the agent is told once, at connection: which directory a call without
/// a `root` is about. It knows its own working directory; this is the other
/// half of noticing that the two differ.
fn instructions(home: &Path) -> String {
    format!(
        "omega is indexing {home}. Calls without `root` search this directory only. If your working \
         directory is a different checkout -- a git worktree, a sibling repository -- pass `root` with \
         that directory, or you will be answered about code you are not editing. `outline` with \
         `root: \"..\"` lists the repositories beside this one.",
        home = roots::shown(home)
    )
}

fn tools() -> Value {
    let root = json!({"type": "string", "description": ROOT_DESCRIPTION});
    json!([
        {
            "name": "search",
            "description": DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "What the code does, or an identifier."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 30, "description": "Results to return (default 8)."},
                    "path": {"type": "string", "description": "Only files whose path contains this text."},
                    "content": {"type": "string", "enum": ["code", "docs", "config", "all"], "description": "What to search (default code)."},
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
                    "symbol": {"type": "string", "description": "An identifier (`ValidateToken`, `Session::refresh`), or literal text to find as written (`\"/api/users\"`, `connection refused`)."},
                    "path": {"type": "string", "description": "Only files whose path contains this text."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Lines to print (default 40)."},
                    "root": root,
                },
                "required": ["symbol"],
            },
        },
        {
            "name": "outline",
            "description": OUTLINE_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "A file or directory, relative to the repository root; a bare file name works when it is unique. Empty for the root."},
                    "root": root,
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
    if !matches!(tool, "search" | "usages" | "outline") {
        return failure(format!("Unknown tool `{tool}`. The tools are `search`, `usages` and `outline`."));
    }
    let resolved = match roots::resolve(&server.home, arguments["root"].as_str(), arguments["path"].as_str()) {
        Ok(resolved) => resolved,
        Err(reason) => return failure(reason),
    };
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
            if index.model.is_none() {
                if !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str("(lexical ranking only: run `omega model install` once to add the semantic channel)");
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
