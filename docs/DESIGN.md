# Design, evaluation, and what did not work

## How a query is answered

- **Chunking** follows indentation and block boundaries, so no grammar is
  needed and any language works; a declaration keeps its doc comment.
- **Lexical channel**: BM25 over code-aware terms. Identifiers are split
  (`cleanup_prepared_assets` -> `cleanup`, `prepar`, `asset`, plus the whole
  name), stemmed (English and Russian), weighted by declared names and path,
  and scaled by how many of the query's terms a chunk covers.
- **File channel**: the same BM25 with a whole file as the document; a file's
  rank is lent to its chunks.
- **File dense channel**: the mean direction of a file's chunks, at half the
  weight of the lexical file rank, skipped for a lone identifier.
- **Dense channel**: a static model2vec model (`potion-code-16M-v2`), one text
  per call (batches pad, and padding is averaged into the vector).
- **Fusion**: RRF (k=60); a boost for chunks declaring an identifier-shaped
  query word; lower weight for tests, fixtures, migrations, import blocks and C
  headers; a file's opening lines change places with a nearly-as-good chunk of
  the same file; adjacent hits of one file are joined into one answer; further
  hits of the same file decay gently.

## What an answer costs

Fused scores are made of ranks and carry no confidence; how many of the
question's terms an answer contains does (thresholds measured over 390 probes
on three repositories):

- An exact identifier whose declaration is clearly first returns that
  declaration whole, how widely it is named, and one runner-up: about 350
  tokens, with no follow-up read.
- Otherwise the first three answers carry twelve lines, later ones four, and
  ones that cover little of the question only their heading: about 600 tokens.
- When none of the first three contains half the question's terms the answer
  says so. The right lines were among them 29% of the time in that case,
  against 87% otherwise, so the agent is told to rephrase before reading.

## Declarations

Names come from one line at a time: a declaring keyword that opens its line
behind modifiers at most (`pub(crate) async fn`, `export const`), a callable
without a keyword when a type precedes it (C, Java, C#, a TypeScript member),
with the brace on the next line or the signature wrapped over several, a
`#define`, a typedef named by its closing `} name_t;`, and a constant by its
spelling. A `const` inside a body counts only when it is a function or a
constant; a statement (`switch len(x) {`) never counts. One table-driven test
covers C, Rust, TypeScript, Go, Python and PHP.

A stylesheet (CSS, SCSS, Less, and the `<style>` section of a component or a
page) declares differently, so its lines are read differently: a rule declares
the first class or id of its selector, and there are mixins, functions,
keyframes, placeholders, top-level `$variables` and `--custom-properties`.
Hyphens belong to these names, in a query too. A nested rule is named by the
rules it sits in, read off the indentation: under `.card`, `&__title` declares
`card__title` -- the name the markup uses, which no search by text can find in
the stylesheet. In HTML an element's `id` is its name. On 54 probes written for
the stylesheets of three private repositories (span R@1, before -> after):
0.33 -> 0.83, 0.50 -> 0.75, 0.33 -> 0.67, with the code probes unchanged.

A document declares its headings: `## Method and limits` declares `Method and
limits`, and a fenced block of code inside it declares nothing, since a README
that shows `fn main` is not where `main` is defined. An outline of a document
is its headings with their lines; a directory of documents shows each one's
title. Code is searched first, because most questions are about it; but when
a document has a heading that says what was asked -- the query names the
heading, the heading says the query and little else, or a one-word query is
the heading's first word, as a task id is -- the document is the answer, and
so is a confident document answer when the code has none. `usages` looks for
literal text in documents too.

## Index and cache

The disk is the only source of truth: an answer is never older than the call
-- whoever edited, an agent a moment ago or a person by hand. Files whose size
and modification time are unchanged are not re-read. Finding out what changed
used to mean walking the whole tree before every answer, ~40 ms on 26,000
files, most of it the ignore rules. Now the operating system's watcher says
which paths changed, and only those are walked to, with the same rules on the
way. A watcher's report arrives after the write it reports, so an edit made
just before a call might not have been reported yet: every call writes a file
of its own, a cookie, at the root and waits for its report. Reports come in
order, so once the cookie's has come, every change made before it has too --
what watchman does. Whatever cannot be told that way is walked: a watcher that
could not start (a watch limit, a file system without events), a report of
lost events, a cookie not back within 300 ms (the watcher is started again, and
given up after three), a change to `.gitignore`, `.ignore`, `.omegaignore` or
`.git/info/exclude`, more than 256 paths at once. Every ten minutes a walk
checks the watcher and replaces it for good if it missed anything. On 26,000
files a query went from ~55 ms to ~15 ms. On Linux the watcher needs a watch per
directory, ignored ones included; past the system's limit it does not start,
and the tree is walked as before. `OMEGA_NO_WATCH=1` walks always.

What each file was read as is kept in a store under the user cache directory
(`%LOCALAPPDATA%\omega\index`, `$XDG_CACHE_HOME/omega/index`), one file per
repository and model. The store is a run of segments, each a whole index of
some files: their chunks and names, a term dictionary with postings as varint
gaps, and their vectors as one byte per dimension with a scale per row. It is
mapped, not read: a start builds the tables every answer walks -- files, chunks,
their names -- and leaves the postings and vectors where they lie. On 27,000
files (215,000 chunks) a start that finds nothing changed costs ~0.2 s, against
~2.5 s when every file's entry was decoded and the postings rebuilt from them,
and the store is 135 MB against 467 MB: a term is written once per segment
rather than once per chunk. Measured on four repositories, bytes for vectors
changed no ranking.

The store is only appended to, or replaced whole by a file written beside it
and renamed over; never truncated or written in place, so a process that mapped
it keeps reading what it mapped (on Windows the file is opened to allow that,
as Unix does). A segment cut short by a crash is told by its missing trailer.
Files read again during a session go into a delta held in memory, written to
the store at the end of the session or once it holds 256 files; picking up an
edited file costs ~25 ms on 2,200 files, query included: what changed is found
by comparing the walk with the last one, and the tables are merged, what did
not change moved into place rather than read out of the segments again. On
26,000 files a query costs ~55 ms, of which the walk is ~40: the walk, not the
index, is what grows with the tree. Entries of files read
again are dead weight in older segments; past eight segments or a quarter of
dead chunks, the live entries are merged into one segment. A stale or
unreadable store is ignored and deleting the directory is always safe.

A first index is written as it goes, a segment every 2,048 files, so one
interrupted -- an agent closed, a machine asleep -- resumes where it stopped. It
is done in two passes: the words first, a fifth of the work and all that
`usages`, `grep` and `outline` need, then the vectors, segment by segment, each
written as soon as it is done. The server answers once the words are read,
searching by words alone meanwhile and saying so. On 27,000 files the words
take ~4 s and the vectors ~11 s. `omega index` does the same ahead of a session
and leaves the store merged into one segment.

A file that is mostly assertions is treated as tests wherever it sits (Rust
aside, whose tests live in the file they test).

## Which directory a call is about

A server is started in one repository, and an agent does not always stay in
it. `root` (absolute, or relative to the server's repository) names another:
it is snapped to the checkout it lies in -- the nearest ancestor holding
`.git`, as a directory or as a worktree's file -- and what remains becomes the
path filter; a directory under no checkout, such as one holding several
repositories, is indexed as one tree, which is all that searching a workspace
takes. An absolute path in `path` implies its checkout the same way; a checkout
nested under a hidden directory of the home repository (where harnesses put
worktrees, and where the home walk never looks) stands apart. Up to four roots
stay indexed, the home root always among them; a drive, a home directory or a
tree of more than 60,000 source files named as a root is refused before it
costs minutes. The home root is never refused for its size: it is where the
agent was started, a monorepo that large is still the code being worked on,
and a server that exits at start shows only as "failed". It is indexed in the
background from the start, so `initialize` is answered at once, and the first
call waits for its words (see Index and cache).

What a call may name is limited to what the agent could read anyway. A
harness usually keeps an agent inside the repository it was started in; the
MCP server is a process of its own that the harness does not restrain, and
without a rule of ours a `root` would read and index any directory on the
machine. The home directory, everything under it and every checkout of the same
repository -- found by the git directory they share, a worktree's `.git` file
leading to it through `commondir` -- are readable; anything else only once the
user has run `omega access add <dir>` in that repository. Access is kept in
omega's settings, keyed by the main checkout so that every worktree has what
the repository has, and not in the repository, where the agent could write it.
It is given to one repository, not to every agent on the machine: the smallest
grant that does the job, as `additionalDirectories` is in a Claude Code
project. Nothing gives it as a side effect -- not `install`, run from wherever,
nor `index`. A refused call is answered before anything is read or indexed,
with the command that would allow it and what is readable instead;
`outline("", root="..")`, which is how an agent asks what is next door, gets
that list as its answer. The command line is not restrained: an agent reaches
it through a shell, which the harness already governs. The settings are read
on every call, so access given takes effect without a restart, and settings
that cannot be read give nothing rather than stopping the server.

The stores clean themselves: when the daemon starts, and once a day while it
runs, a store not opened for thirty days -- its time is set on every opening --
goes, as a repository no longer worked on, and so does what a crash left aside.
A store an earlier release wrote goes whatever its age: no later release reads
it. The thirty days are a setting (`omega cache auto <days>`, or `off`), and
`omega cache clean` does the same at once; `omega cache clear` removes every
store, stopping the daemon first since it holds them open. A store whose first
record is of an earlier format is written anew, not appended to: its records
would never be read past the first, and the file would only grow.

No harness tells an MCP server where its agent currently is -- roots are
deprecated in the protocol, and hooks would tie this to one harness -- so the
server tells the agent instead, and only when it matters: `instructions` at
connection name the indexed directory; an answer about another root opens with
it and carries absolute paths; an answer from a main checkout that has linked
worktrees (read from `.git/worktrees`) names them. Calls are stateless:
sub-agents can share one server process, and one in a worktree must not
redirect the others.

A worktree is another root, and was indexed from nothing, though it holds
what its main checkout holds but for the files a branch changed: the stamps
differ, the contents do not. Each file's entry now carries a hash of its
contents, and a checkout indexed for the first time looks in the stores of the
other checkouts of its repository first: an entry with the same path and the
same hash is copied as it is -- chunks, postings, vectors -- under the new
stamp, the way segments are merged, and only the rest is read. On a copy of a
2,100-file repository with ten files changed, the worktree read and embedded
ten. Nothing is stored twice: the stores of the checkouts are each other's
cache.

## One daemon for every session

An `omega mcp` per agent means an index per agent: the postings and vectors are
shared anyway, a mapping of one file, but the tables of files and chunks, the
watcher and the deltas are each session's own, and five agents in a large
repository hold five of them. A daemon holds them once. It was long left out
for one reason: a server per repository cannot tell which checkout an agent
works in, and would answer an agent in a worktree from the main checkout.
`omega mcp` stays what the agent starts, in the agent's directory, and becomes
a proxy: it answers `initialize` and `tools/list` itself and sends every call
to the daemon with its own home, so the daemon always knows whose call it is;
roots, access and headings are worked out from that home, per call.

One daemon runs per user, model and data directory, on a named pipe on Windows
and a socket in a directory only the user can enter elsewhere (an abstract
socket on Linux could be reached by any user). The first session starts it,
detached, out of the agent's job and without the agent's pipes: a daemon that
kept the session's standard handles would keep the agent waiting for an end of
output that never comes. Sessions are counted by the hello each sends; with none
for half an hour the daemon writes what it read again and leaves. A proxy that
loses it starts it again and asks once more, and one that cannot answers from
its own process for the rest of the session, as it does when the daemon is
disabled (`omega daemon disable`, or `OMEGA_NO_DAEMON=1`). `omega daemon stop`
returns once the deltas are on disk; a daemon that does not answer within ten
seconds is killed by the pid it recorded. After `omega update` the old
version's daemon is stopped if no session uses it, and otherwise goes when they
end. Through the daemon a query costs what it did in a process of its own, and
a second session's first call is answered in ~15 ms instead of waiting for an
index.

## Installing into agents

`omega install` offers three independent integrations per agent: the MCP
server entry, a marked block of instructions in the agent's AGENTS.md /
CLAUDE.md, and a sub-agent that uses the CLI. Everything written is an entry
under our key, a block between our markers or a file of our own, so uninstall
restores other entries, key order and surrounding text byte for byte, and
installing again -- also after the binary moved -- leaves one of each. A JSON
config with comments is left alone and the entry is printed to add by hand.
`--agents claude,codex --integrations mcp,instructions --yes` makes it
scriptable; `OMEGA_HOME=<dir>` rehearses it against a scratch home.

`omega uninstall` removes everything omega put on the machine, not only what
`install` wrote into agents: every place omega writes is named in one module,
`paths`, which the uninstaller reads the same list from. It shows only what is
there, all ticked, and warns of a choice that leaves something broken -- an
agent still starting a binary about to go. The daemon is stopped first when
the binary or the data go, since on Windows it holds them. A running binary
cannot be deleted on Windows, so it steps aside as `omega update` has it do and
a detached `cmd` deletes it once the process has ended; one not put there by
omega's installer, as `cargo install` does, is left with a word on how to
remove it. The installer scripts' `-Uninstall` / `--uninstall` call it, and
keep their own steps for a machine where the binary is already gone.

`omega update` brings the installed binary to the latest release without an
installer: the release's tag is read off where GitHub redirects
`releases/latest` to, which needs no API call and no token; a release carries
the bare binary of each platform and its SHA-256 beside the archives, so the
update is one small download and a check. The running binary is renamed aside
on Windows, where it cannot be overwritten, and renamed over on Unix. Then the
new binary is run as `omega install --yes --refresh`: the new one, because the
instruction and sub-agent texts are compiled in and the old process would
write the old ones (and on Linux no longer knows its own path); `--refresh`,
because it writes again only what is already installed -- our entry under its
key, our block between our markers, our file -- so an agent that was never
given an integration does not gain one, and unchanged files stay byte for
byte. A test replaces the binary under a process that holds it open.

The model is read from `--model DIR`, else `OMEGA_MODEL`, else where
`model install` put it (pinned revision, sha256-verified), else the Hugging
Face cache; without one, search is lexical only and says so.

## Evaluation

Probes are `query<TAB>path[<TAB>anchor]`. *file* rank is the place of the
expected file among distinct files answered; *span* rank is the place of the
first answer whose lines contain the anchor (the declaration line). `--hits 8`
scores only what an agent is actually shown.

    omega eval eval/semble/agent.tsv --root <semble checkout> --hits 8
    sh eval/scripts/all.sh                      # one line per repository listed in eval/repos.local
    python eval/scripts/semble_span.py <root> eval/semble/agent.tsv                 # semble, same scoring
    python eval/scripts/codesearch_span.py <exe> <indexed root> eval/semble/agent.tsv   # flupkede/codesearch

Every probe set was written by an agent that saw only the repository, never
this engine: `symbol` (an identifier), `agent` (3-7 technical keywords, the way
an agent asks), `paraphrase` (plain language that avoids the code's words).
Two sets ship here, written against public repositories (`eval/semble`,
`eval/cbm`); the other rows below were measured on private code, whose probes
are kept outside this repository and listed in the git-ignored `eval/repos.local`.

`agent` sets, span R@1 / MRR, measured 2026-09-21:

| repository | codesearch | semble | omega |
|---|---|---|---|
| Go + React (private) | 0.18 / 0.29 | 0.16 / 0.24 | 0.56 / 0.65 |
| PHP / Laravel (private) | 0.14 / 0.26 | 0.28 / 0.40 | 0.68 / 0.78 |
| Nuxt + Vue + TS (private) | 0.18 / 0.27 | 0.40 / 0.52 | 0.82 / 0.88 |
| Astro + TS (private) | 0.10 / 0.16 | 0.20 / 0.33 | 0.62 / 0.76 |
| semble (Python) | 0.44 / 0.55 | 0.22 / 0.27 | 0.76 / 0.83 |
| codebase-memory-mcp (C) | - | 0.20 / 0.27 | 0.58 / 0.68 |

codesearch does not index `.vue` or `.astro`, which caps it near 0.6 on those
two repositories; Go, PHP and Python are the like-for-like rows. It indexes in
31-196 s per repository against 0.1-0.5 s, and answers in 140-550 ms per CLI
call.

Symbol lookups put the declaration first on every repository. Plain-language
queries that avoid the code's vocabulary stay weak (span R@1 0.08-0.20, where
a transformer embedder does somewhat better): that is the ceiling of a static
embedding model, and the tool descriptions steer the agent toward technical
wording instead.

## Tried, measured, dropped

Each was run over the same probes (right lines first, mean of five
repositories, baseline 0.632 at the time):

- **Boosting a declaration whose name the query spells out** (`parse config
  file` for `ParseConfigFile`): 0.540. Two-word names are covered by too
  many queries; the name weight inside BM25 already does this gently.
- **Pairs of neighbouring terms as terms** (`ring buffer` meeting `ringBuffer`
  as one): neutral, for twice the postings.
- **Demoting a file's opening lines outright**: better span ranks, but four
  file-level probes lost where a module's documentation was the answer.
  Changing places within the file (kept) gains the same and loses none: 0.680.
- **Expanding the query with the model's nearest corpus words**: a static
  model knows morphology, which stemming covers, not synonyms.
- **Cross-encoder reranking of the top 20**, the one neural step that costs
  nothing at index time: `ms-marco-MiniLM-L-6` made every repository worse
  (PHP: 0.68 to 0.32); `jina-reranker-v2-base-multilingual` (278M, on a GPU,
  60-90 ms a query) was level on Go and Vue and worse on PHP (0.68 to 0.40).
- **A transformer embedder instead of the static one**: about twice the
  recall on plain-language queries that avoid the code's words, for minutes
  of indexing instead of half a second. Agents do not write such queries.

## Known gaps

- The numbers here are from probes; what agents reported from real work is
  what shaped the answers -- a dotted operation name, a wrong path, a
  stylesheet, a document -- and none of it is measured as a whole.
- The index lives in memory; the largest repository measured is 116k lines of
  first-party code.
- Stemming is English and Russian only.
- Linux and macOS are covered by the release workflow's tests only; nobody has
  used omega there. There is no Intel macOS build.
