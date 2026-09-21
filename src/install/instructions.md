## Code search: omega

This repository is indexed by the `omega` MCP server (tools `search`, `usages`, `outline`). Use it FIRST to find code. Its answers hold only first-party source -- never `node_modules`, `vendor`, build output, lock files or minified bundles -- with exact line ranges and the code itself, so one call replaces a chain of grep and whole-file reads.

| You need | Call |
|---|---|
| where something is implemented or handled | `search("jwt token refresh interceptor")` -- 3-6 technical words the code would use, not a sentence |
| a function, class or type you can name | `search("ValidateToken")` -- returns the whole declaration; do not read the file again for it |
| who calls it, what a change would break | `usages("ValidateToken")` -- the declaration, then every use labelled with the `[function]` it sits in |
| where an error message, route or config key comes from | `usages("connection refused")`, `usages("/api/users")` |
| what a file or directory holds, before reading it | `outline("src/auth")`, `outline("src/auth/session.ts")` -- then read only the lines you need |

- Go straight to the `path:start-end` returned. Do not grep for what omega already gave you.
- `Low confidence` means the words did not match: rephrase with other technical terms or synonyms, do not read those results.
- Never read a whole unfamiliar file to find something in it: `outline` it first.
- Use Grep/Glob only for regular expressions, for files omega leaves out (vendored, generated, ignored), or after two rephrased searches have missed.

Without MCP (sub-agents), the same three from a shell at the repository root: `omega search "..."`, `omega usages <name or text>`, `omega outline <path>`.
