Find code with `omega`, not grep. Its answers hold only first-party source -- no vendored code, build output or lock files -- with exact line ranges. Run it from the repository root.

```bash
omega search "jwt token refresh interceptor"     # where it is implemented: 3-6 technical words, not a sentence
omega search "ValidateToken"                     # an exact name returns the whole declaration
omega search "UserRepository save" --path src/domain
omega usages ValidateToken                       # declaration, then every use labelled with the function it sits in
omega usages "connection refused"                # where a message, route or config key is written
omega grep 'func \w+Handler\('               # a regular expression, over first-party source only
omega outline src/auth/session.ts                # what a file declares, with line numbers, before reading it
```

- Read the `path:start-end` returned, not the whole file; `outline` an unfamiliar file before reading it.
- `Low confidence` means the words did not match: rephrase with other technical terms or synonyms.
- Fall back to the system grep only after two rephrased searches have missed, or for files omega leaves out.
- Report file paths with line ranges, and quote only the lines that answer the question.
