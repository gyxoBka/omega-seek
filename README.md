# omega

Code search for coding agents, over MCP. The agent asks once and gets the right
lines -- `path:start-end`, what is declared there, and the source -- instead of
a chain of greps and whole-file reads. It finds; the agent does the thinking.

```
> search "session token expiry check"

internal/auth/session.go:31-78  Session, Expired, ValidateToken, refresh
   42| func (s *Session) ValidateToken(token string) error {
   43| 	if s.Expired(time.Now()) {
   ...
```

Three tools, 2 KB of schema in the agent's context:

| tool | answers | instead of |
|---|---|---|
| `search` | where something is implemented; an exact identifier returns its whole declaration | grep + read |
| `usages` | where an identifier is declared and used, or where a literal text is written, each line labelled with the function it sits in | grep for a name or a message |
| `outline` | the table of contents of a file or directory, with line numbers | reading a file to see what is in it |

Any language works: files are cut along indentation and block boundaries, not
by a grammar. Vendored code, build output, lock files and minified bundles
never reach the index, so they never reach an answer.

## Speed

| | |
|---|---|
| index a 100k-line repository | 0.5 s cold, 0.2 s from cache |
| pick up an edited file | 40 ms, automatically, before the next query |
| query | about 1 ms |
| answer | 350-600 tokens |
| binary | 7 MB, no daemon, no database, CPU only |

## Quality

Share of agent-style queries (`auth token refresh interceptor`) whose right
lines come back **first**, on probes written by an agent that never saw this
engine:

| repository | [codesearch](https://github.com/flupkede/codesearch) | [semble](https://github.com/MinishLab/semble) | omega |
|---|---|---|---|
| Go + React | 0.18 | 0.16 | **0.56** |
| PHP / Laravel | 0.14 | 0.28 | **0.68** |
| Nuxt / Vue | 0.18 | 0.40 | **0.82** |
| Astro | 0.10 | 0.20 | **0.62** |
| Python | 0.44 | 0.22 | **0.76** |

The right lines are among the eight answers shown 86-98% of the time, and a
lookup by exact identifier puts the declaration first every time. How this was
measured, and what was tried and dropped: [docs/DESIGN.md](docs/DESIGN.md).

## Quick start

**1. Install the binary** (puts it on PATH and downloads the 32 MB model). The
repository is private, so the GitHub CLI carries the credentials -- run
`gh auth login` once.

```powershell
# Windows
gh api repos/gyxoBka/omega-seek/contents/scripts/install.ps1 -H "Accept: application/vnd.github.raw" | Out-String | iex
```

```sh
# Linux / macOS
gh api repos/gyxoBka/omega-seek/contents/scripts/install.sh -H "Accept: application/vnd.github.raw" | sh
```

Without `gh`: download the archive for your platform from Releases, unpack it,
run the `install.ps1` / `install.sh` inside. From source:
`cargo install --path . && omega model install`.

**2. Connect it to your agents.**

```sh
omega install
```

It detects Claude Code, Codex, Gemini CLI, Opencode, Cursor, VS Code and nine
more, shows what it will write where, and asks before writing: the MCP server
entry, a short block of instructions, and a sub-agent. Running it again changes
nothing; `omega uninstall` restores every file byte for byte.

**3. Restart the agent.** There is nothing to index by hand: the server indexes
the repository it is started in and follows your edits.

From a terminal:

```sh
omega search "retry backoff http client"
omega usages ValidateToken
omega usages "connection refused"
omega outline src/auth
```

## Other repositories and worktrees

omega answers about the repository it was started in. Every tool also takes a
`root`, so an agent can look next door without leaving its own:

```
search("order payload validation", root="../backend")    # a sibling repository
usages("/api/orders", root="..")                         # the whole workspace, as one tree
outline("", root="..")                                   # which repositories are next door
search("retry policy", root="/work/app-wt/fix-auth")     # a git worktree
```

A directory inside a repository means that repository, narrowed to the
directory; an absolute path in any argument implies its repository without a
`root`. The agent is never left guessing where an answer came from: omega says
which directory it indexes when it connects, names the root and gives absolute
paths whenever a call looked elsewhere, and -- when the repository has git
worktrees an agent might be working in -- says so at the top of the answer.
Nothing is remembered between calls, so agents sharing one server cannot
redirect each other.

## Keeping things out of the index

`.gitignore` is honoured, with or without a `.git`. Add a `.omegaignore` (same
syntax) for anything else. `node_modules/`, `vendor/`, `dist/`, `target/`, lock
files, `*.min.*`, source maps and files over 1 MB are always left out.

## Removing it

```sh
install.ps1 -Uninstall -Purge      # or: install.sh --uninstall --purge
```

Takes omega out of the agents, then removes the binary, the PATH entry, the
model and the index cache.
