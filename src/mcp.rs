//! The MCP face: `search`, `usages`, `grep` and `outline` over stdio, line-delimited JSON-RPC.
//!
//! The server is started in one repository and answers about it unless a call
//! says otherwise. What keeps an agent from being answered about the wrong
//! code is that it is always told where an answer came from when that could be
//! in doubt: at connection (`instructions`), when the repository has worktrees
//! the agent might be in, and whenever a call looked somewhere else.

use crate::daemon::{self, Link};
use crate::engine::{self, Engine};
use crate::roots;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::Path;

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

const OPEN_ROOTS: usize = 4;

pub fn serve(root: &Path, model: Option<&Path>) -> Result<(), String> {
    let home = roots::clean(&root.canonicalize().map_err(|error| format!("{}: {error}", root.display()))?);
    let mut answerer = if daemon::wanted() {
        match daemon::attach(&home, model) {
            Some(link) => Answerer::Daemon(link),
            None => {
                eprintln!("omega: the daemon did not start; answering from this process");
                Answerer::local(&home, model)
            }
        }
    } else {
        Answerer::local(&home, model)
    };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| error.to_string())?;
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").cloned() else {
            continue;
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
            "tools/call" => Ok(answerer.call(&home, model, &message["params"])),
            _ => Err(json!({"code": -32601, "message": "method not found"})),
        };
        let reply = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
        writeln!(stdout, "{reply}").and_then(|()| stdout.flush()).map_err(|error| error.to_string())?;
    }
    if let Answerer::Local(engine) = &answerer {
        engine.persist();
    }
    Ok(())
}

enum Answerer {
    Daemon(Link),
    Local(Engine),
}

impl Answerer {
    fn local(home: &Path, model: Option<&Path>) -> Self {
        std::thread::spawn(|| {
            if let Some(dir) = crate::paths::stores() {
                let _ = crate::store::prune(&dir, crate::store::ABANDONED);
            }
        });
        let engine = Engine::new(model.map(Path::to_path_buf), OPEN_ROOTS);
        engine.pin(home);
        engine.prepare(home);
        Self::Local(engine)
    }

    fn call(&mut self, home: &Path, model: Option<&Path>, params: &Value) -> Value {
        match self {
            Self::Local(engine) => engine.call(home, params),
            Self::Daemon(link) => {
                if let Ok(result) = link.call(home, params) {
                    return result;
                }
                match daemon::attach(home, model) {
                    Some(mut again) => {
                        let result = again.call(home, params);
                        *link = again;
                        result.unwrap_or_else(|error| engine::failure(format!("omega's daemon did not answer: {error}")))
                    }
                    None => {
                        eprintln!("omega: the daemon is gone and did not start again; answering from this process");
                        *self = Self::local(home, model);
                        self.call(home, model, params)
                    }
                }
            }
        }
    }
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
