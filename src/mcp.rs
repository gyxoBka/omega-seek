//! The MCP face: `search`, `usages` and `outline` over stdio, line-delimited JSON-RPC.

use crate::index::Index;
use crate::search::{Content, Options, render, search};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::{Duration, Instant};

const DESCRIPTION: &str = "Search this repository's code. Use INSTEAD of grep/glob/find whenever you \
need to locate where something is implemented, defined or handled. Returns the best matching code \
blocks as `path:start-end  declared names` followed by the source lines with line numbers, so the \
result can be read or edited directly without another lookup.\n\
Query tips: write what the code does or is called, using words likely to appear in identifiers and \
comments (e.g. `retry backoff http client`, `parse config file`, `UserRepository save`). An exact \
identifier returns its whole declaration and how widely it is used. If the first answer misses, rephrase with synonyms rather than \
falling back to grep.";

/// How long an index is trusted before the tree is looked at again.
const FRESH_FOR: Duration = Duration::from_secs(2);

pub fn serve(root: &Path, model: Option<&Path>) -> Result<(), String> {
    let mut index = Index::open(root, model)?;
    let mut checked = Instant::now();

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
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": [tool(), usages_tool(), outline_tool()]})),
            "tools/call" => {
                if checked.elapsed() > FRESH_FOR {
                    index = index.refreshed();
                    checked = Instant::now();
                }
                Ok(call(&index, &message["params"]))
            }
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

fn tool() -> Value {
    json!({
        "name": "search",
        "description": DESCRIPTION,
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What the code does, or an identifier."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 30, "description": "Results to return (default 8)."},
                "path": {"type": "string", "description": "Only files whose path contains this text."},
                "content": {"type": "string", "enum": ["code", "docs", "config", "all"], "description": "What to search (default code)."},
            },
            "required": ["query"],
        },
    })
}

fn call(index: &Index, params: &Value) -> Value {
    match params["name"].as_str() {
        Some("usages") => return call_usages(index, &params["arguments"]),
        Some("outline") => {
            let path = params["arguments"]["path"].as_str().unwrap_or_default();
            return json!({"content": [{"type": "text", "text": crate::outline::outline(index, path)}]});
        }
        Some("search") => {}
        other => {
            let text = format!("Unknown tool `{}`. The tools are `search`, `usages` and `outline`.", other.unwrap_or_default());
            return json!({"content": [{"type": "text", "text": text}], "isError": true});
        }
    }
    let arguments = &params["arguments"];
    let Some(query) = arguments["query"].as_str().filter(|query| !query.trim().is_empty()) else {
        return json!({"content": [{"type": "text", "text": "`query` is required."}], "isError": true});
    };
    let mut options = Options::default();
    if let Some(limit) = arguments["limit"].as_u64() {
        options.limit = (limit as usize).clamp(1, 30);
    }
    if let Some(content) = arguments["content"].as_str().and_then(Content::parse) {
        options.content = content;
    }
    options.path = arguments["path"].as_str().map(|path| path.replace('\\', "/"));
    let hits = search(index, query, &options);
    let mut text = render(index, query, &hits, &options);
    if index.model.is_none() {
        text.push_str("(lexical ranking only: run `omega model install` once to add the semantic channel)");
    }
    json!({"content": [{"type": "text", "text": text}]})
}

const USAGES_DESCRIPTION: &str = "Every place an identifier is declared and used -- or every place \
a literal text is written. Use INSTEAD of grep. For an identifier (`ValidateToken`, `Session::refresh`): \
whole-word, case-sensitive matches in code, comment lines dropped, the declaration first; use it \
for callers, references and the impact of a change. For anything else (an error message, a route \
like `/api/users`, a config key, any phrase, in any language): exact matches as written, in code \
and configuration. Either way each line is labelled with the [declaration] it sits in, files are \
grouped, tests come last, and output is bounded. Matching is by name, not by type.";

fn usages_tool() -> Value {
    json!({
        "name": "usages",
        "description": USAGES_DESCRIPTION,
        "inputSchema": {
            "type": "object",
            "properties": {
                "symbol": {"type": "string", "description": "An identifier (`ValidateToken`, `Session::refresh`), or literal text to find as written (`\"/api/users\"`, `connection refused`)."},
                "path": {"type": "string", "description": "Only files whose path contains this text."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Lines to print (default 40)."},
            },
            "required": ["symbol"],
        },
    })
}

fn call_usages(index: &Index, arguments: &Value) -> Value {
    let Some(symbol) = arguments["symbol"].as_str() else {
        return json!({"content": [{"type": "text", "text": "`symbol` is required."}], "isError": true});
    };
    let mut options = crate::usages::Options::default();
    if let Some(limit) = arguments["limit"].as_u64() {
        options.limit = (limit as usize).clamp(1, 200);
    }
    options.path = arguments["path"].as_str().map(|path| path.replace('\\', "/"));
    json!({"content": [{"type": "text", "text": crate::usages::usages(index, symbol, &options)}]})
}

const OUTLINE_DESCRIPTION: &str = "The table of contents of a file or directory. For a file: every \
declaration with its line number and signature, nesting kept. For a directory: its files with \
what each declares, and its subdirectories. Call this BEFORE reading a file you have not seen: \
it costs a few dozen lines instead of the whole file, tells you whether the file matters, and \
gives the line to start reading from.";

fn outline_tool() -> Value {
    json!({
        "name": "outline",
        "description": OUTLINE_DESCRIPTION,
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "A file or directory, relative to the repository root; a bare file name works when it is unique. Empty for the root."},
            },
            "required": ["path"],
        },
    })
}
