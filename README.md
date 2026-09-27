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

Four tools, 4 KB of schema in the agent's context:

| tool | answers | instead of |
|---|---|---|
| `search` | where something is implemented; an exact identifier returns its whole declaration | grep + read |
| `usages` | where an identifier is declared and used, or where a literal text is written, each line labelled with the function it sits in | grep for a name or a message |
| `grep` | every line a regular expression matches, in first-party source only, labelled the same way | grep / rg, which also walk dependencies and build output |
| `outline` | the table of contents of a file or directory, with line numbers | reading a file to see what is in it |

Any language works: files are cut along indentation and block boundaries, not
by a grammar. Vendored code, build output, lock files and minified bundles
never reach the index, so they never reach an answer.

## Speed

| | |
|---|---|
| index 27,000 files | 4 s to answer by words, 15 s with vectors; 0.2 s from the store |
| look for changes before a query | ~10 ms on 26,000 files: the system reports them, the tree is not walked |
| pick up an edited file | 25 ms on 2,200 files, automatically, before the next query |
| query | about 1 ms |
| answer | 350-600 tokens |
| binary | 8 MB, no database, CPU only; the daemon starts and stops by itself |

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

**1. Install the binary** (puts it on PATH and downloads the 32 MB model).

```powershell
# Windows
irm https://raw.githubusercontent.com/gyxoBka/omega-seek/master/scripts/install.ps1 | iex
```

```sh
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/gyxoBka/omega-seek/master/scripts/install.sh | sh
```

Or download the archive for your platform from
[Releases](https://github.com/gyxoBka/omega-seek/releases), unpack it and run
the `install.ps1` / `install.sh` inside. From source:
`cargo install --path . && omega model install`. Later, `omega update`
brings the installed binary to the latest release.

**2. Connect it to your agents.**

```sh
omega install
```

It detects Claude Code, Codex, Gemini CLI, Opencode, Cursor, VS Code and nine
more, shows what it will write where, and asks before writing: the MCP server
entry, a short block of instructions, and a sub-agent. Running it again changes
nothing; `omega uninstall` restores every file byte for byte.

**3. Restart the agent.** There is nothing to set up per repository: the server
indexes the repository it is started in and follows your edits. In a large repository
-- tens of thousands of files -- `omega index` run there once beforehand spares
the first session the wait; an interrupted index resumes where it stopped.

From a terminal:

```sh
omega search "retry backoff http client"
omega usages ValidateToken
omega usages "connection refused"
omega grep 'func \w+Handler\('
omega outline src/auth
omega index            # the whole index now, before a session needs it
```

## Other repositories and worktrees

omega answers about the repository it was started in. Every tool also takes a
`root`, so an agent can look into a git worktree of its repository, or into a
directory you have given that repository access to:

```
search("retry policy", root="/work/app-wt/fix-auth")     # a git worktree: always readable
search("order payload validation", root="../backend")    # a sibling repository, once given
usages("/api/orders", root="..")                         # a parent given: all of it, as one tree
outline("", root="..")                                   # what is readable from here
```

A harness usually keeps an agent inside its repository, and an MCP server is a
process the harness does not restrain, so omega keeps to the same line: the
repository and its worktrees, and beyond them only what you give. Access is
given to the repository you run the command in, and every worktree of it has it
too; agents started anywhere else do not:

```sh
cd ~/work/app
omega access add ../backend     # agents started in app may read backend
omega access                    # what app may read; pick any to take back
omega access list --all         # every repository and what it was given
```

It is kept in omega's settings, not in the repository, where an agent could
write it for itself. A call outside is refused before anything is read or
indexed, and the refusal names the command to run. The command line is not
restrained: an agent reaches it through a shell, which the harness governs.

A directory inside a repository means that repository, narrowed to the
directory; an absolute path in any argument implies its repository without a
`root`. The agent is never left guessing where an answer came from: omega says
which directory it indexes when it connects, names the root and gives absolute
paths whenever a call looked elsewhere, and -- when the repository has git
worktrees an agent might be working in -- says so at the top of the answer.
Nothing is remembered between calls, so agents sharing one server cannot
redirect each other.

## One daemon for every session

Every agent starts its own `omega mcp`, and several agents in one large
repository would each hold its index. So the first session starts a daemon, one
per user, and every session after it asks that daemon: one index per
repository, however many agents, and a second session starts with its index
warm. Each session says which directory it was started in, so an agent in a
worktree is answered from the worktree. It goes after half an hour with no
session; if it dies, the next call starts it again. Nothing needs setting up:

```sh
omega daemon status     # what it holds, for how many sessions
omega daemon stop       # stop it now; the next session starts it again
omega daemon disable    # every session answers from its own process, as before
```

## The cache

Each repository's index is a file under `%LOCALAPPDATA%\omega\index` or
`~/.cache/omega/index`. It cleans itself: a repository not opened for 30 days,
and what an earlier release wrote, are removed without being asked.

```sh
omega cache              # where it is, how large, how it is cleaned
omega cache clean        # clean now
omega cache clear        # remove every index; each is built again when next used
omega cache auto 14      # clean what is not opened for 14 days; `auto off` keeps it
```

## Keeping things out of the index

`.gitignore` is honoured, with or without a `.git`. Add a `.omegaignore` (same
syntax) for anything else. `node_modules/`, `vendor/`, `dist/`, `target/`, lock
files, `*.min.*`, source maps and files over 1 MB are always left out.

## Removing it

```sh
omega uninstall
```

It lists what omega left on this machine -- the integrations in each agent, the
index caches, the model, the settings, the PATH entry, the binary -- with where
each is and how large, all ticked; untick what should stay, and it says what a
choice would leave broken before it removes anything. `--yes` removes everything
without asking, `--keep-data` keeps the model, the caches and the settings for
a reinstall, `--agents` / `--integrations` take omega out of agents only, and
`--dry-run` shows the plan. If the binary is gone already, the installer scripts
do the same:

```powershell
# Windows
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/gyxoBka/omega-seek/master/scripts/install.ps1))) -Uninstall -Purge
```

```sh
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/gyxoBka/omega-seek/master/scripts/install.sh | sh -s -- --uninstall --purge
```

Without `-Purge` / `--purge` the model, the caches and the settings stay.

## Licence

MIT. The embedding model, `potion-code-16M-v2`, is MIT too; the stop-word
lists are Snowball's (BSD), as PostgreSQL ships them.
