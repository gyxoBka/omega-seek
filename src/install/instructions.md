## Code search: omega

This repository is indexed by the `omega` MCP server. Use it FIRST to find code. Its answers hold only first-party source -- never `node_modules`, `vendor`, build output, lock files or minified bundles -- with exact line ranges and the code itself: one call replaces a chain of greps and reads.

| You need | Call |
|---|---|
| where something is implemented or handled | `search(query="jwt token refresh interceptor")` -- 3-6 technical words the code would use |
| a function, class or type you can name | `search(query="ValidateToken")` -- returns the whole declaration: do not read the file for it |
| references to an exact identifier | `usages(symbol="ValidateToken")` -- the declaration, then every use with the `[function]` it sits in |
| exact occurrences of a message, route or key | `usages(symbol="connection refused")`, `usages(symbol="/api/users")`; documentation by its words: `search(query="...", content="docs")` |
| a regular expression | `grep(pattern="func \w+Handler\(")` -- not the built-in grep/rg, which also walk dependencies and build output |
| what a file, directory or document holds | `outline(path="src/auth")`, `outline(path="docs/DESIGN.md")` -- then read only the lines you need |
| code in another repository | `search(query="payload validation", root="../backend")`, once given access; `outline(path="", root="..")` lists what is |
| anything, from a git worktree or a checkout other than omega's | add `root="<that directory>"` -- else the answer is about code you are not editing |

- Go straight to the `path:start-end` returned. Do not grep for what omega already gave you.
- `Low confidence` means the words did not match: rephrase with other technical terms or synonyms, do not read those.
- Never read a whole unfamiliar file: `outline` it first.
- Built-in Grep/Glob only for files omega leaves out (vendored, generated, ignored), or after two rephrased searches have missed.

Without MCP (sub-agents), from a shell in the checkout you work in: `omega search "..."`, `omega usages <name or text>`, `omega grep <regex>`, `omega outline <path>`; `--root ../backend` looks next door.
