## Code search: omega

This repository is indexed by the `omega` MCP server. Use it FIRST to find code. Its answers hold only first-party source -- never `node_modules`, `vendor`, build output, lock files or minified bundles -- with exact line ranges and the code itself: one call replaces a chain of greps and reads.

| You need | Call |
|---|---|
| where something is implemented or handled | `search("jwt token refresh interceptor")` -- 3-6 technical words the code would use |
| a function, class or type you can name | `search("ValidateToken")` -- returns the whole declaration: do not read the file for it |
| who calls it, what a change would break | `usages("ValidateToken")` -- the declaration, then every use with the `[function]` it sits in |
| where a message, route, key or task id is written | `usages("connection refused")`, `usages("/api/users")`; documentation by its words: `search(..., content="docs")` |
| a regular expression | `grep("func \w+Handler\(")` -- not the built-in grep/rg, which also walk dependencies and build output |
| what a file, directory or document holds | `outline("src/auth")`, `outline("docs/DESIGN.md")` -- then read only the lines you need |
| code in another repository | `search("payload validation", root="../backend")`, once given access; `outline("", root="..")` lists what is |
| anything, from a git worktree or a checkout other than omega's | add `root="<that directory>"` -- else the answer is about code you are not editing |

- Go straight to the `path:start-end` returned. Do not grep for what omega already gave you.
- `Low confidence` means the words did not match: rephrase with other technical terms or synonyms, do not read those.
- Never read a whole unfamiliar file: `outline` it first.
- Built-in Grep/Glob only for files omega leaves out (vendored, generated, ignored), or after two rephrased searches have missed.

Without MCP (sub-agents), from a shell in the checkout you work in: `omega search "..."`, `omega usages <name or text>`, `omega grep <regex>`, `omega outline <path>`; `--root ../backend` looks next door.
