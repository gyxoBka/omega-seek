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

## Index and cache

Built in memory at start. Before a query, at most every two seconds, the tree
is walked again: files whose size and modification time are unchanged are not
re-read (a no-change check costs ~5 ms on 1000 files, picking up an edited file
~40 ms). What each file was read as is kept under the user cache directory
(`%LOCALAPPDATA%\omega\index`, `$XDG_CACHE_HOME/omega/index`), one
file per repository and model; a stale or unreadable cache is ignored and
deleting the directory is always safe.

A file that is mostly assertions is treated as tests wherever it sits (Rust
aside, whose tests live in the file they test).

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

- Never yet used by an agent in real work: every number here is from probes.
- The index lives in memory; the largest repository measured is 116k lines of
  first-party code.
- Stemming is English and Russian only.
- Releases, the PATH step of `install.ps1`, Linux and macOS are untested.
