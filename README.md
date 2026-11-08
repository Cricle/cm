# cm-memory

[![CI](https://github.com/Cricle/cm/actions/workflows/ci.yml/badge.svg)](https://github.com/Cricle/cm/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`cm` is a single-file, compressed, mmap-backed **project memory for coding
agents**. It is a hot-discovery index for source-grounded project knowledge,
not a universal knowledge base.

One file holds an append-only data log, a compressed BM25/MinHash index, and an
optional disposable `cm.cache` cache. It is not Markdown and not a database
server. Writers coordinate through a disposable `cm.lock`; it contains no
memory data. See [positioning](docs/positioning.md) for the target scope and
non-goals.

## Install

```bash
cargo install cm-memory
```

The installed binary is `cm`.

## Quick start

```bash
cm init --file cm
cm remember --file cm --subject "build" --text "Uses Rust 2021" --scope repo --source README
cm recall --file cm --query "build" --scope repo --budget 500
cm status --file cm
```

## Library

The crate also exposes a Rust API; the `cm` binary remains the primary human
interface and uses the same store implementation. Enabling the library does not
require the CLI code:

```toml
[dependencies]
cm-memory = { version = "0.0.2", default-features = false }
```

```rust
use cm_memory::{Cm, NewMemory, RecallQuery};

let cm = Cm::init("cm", true)?;
let written = cm.remember(
    NewMemory::new("decision", "repo", "build command", "Build with cargo build --release.")
        .source("README.md")
        .confidence(0.9),
)?;
let hits = cm.search("cargo release", 5)?;
let context = cm.recall(&RecallQuery {
    query: "release".to_string(),
    budget_tokens: 500,
    ..RecallQuery::default()
})?;
let all = cm.all()?;
# Ok::<(), anyhow::Error>(())
```

The embedding surface covers the full lifecycle:

- memory: `remember`, `get`, `all`, `search`, `search_query`, `recall`
- task context: `brief`, `brief_for`, `brief_with_query`, `frame_set`,
  `frame_get`, `frames`, `claim`, `release`, `finish`
- governance: `forget`, `merge`, `feedback`, `absorb`
- exploration: `explore_plan`, `mechanical_explore`, `explore_cursor`,
  `set_explore_cursor`, `explore_cursor_for`
- maintenance: `status`, `checkpoint`, `reindex`, `verify`, `export_jsonl`,
  `import_jsonl`

`Cm::open_with(StoreOptions { .. })` supports read-only stores, a bounded lock
timeout, an in-memory or disk cache, and a strict source policy that rejects
absolute paths, `..` escapes, and paths outside the project root. Writes are
read-modify-write safe under one file lock; the default lock timeout is 200 ms.
Each handle is cheap and opens the same file per operation, so it is safe to
create one per request. The on-disk format is the same as the CLI; library
writes are durable and use the same schema, checksum, and duplicate detection.

## Install skills and binary

```bash
cm install --detected              # install into detected agent roots + global cm
cm install --all                   # install into every known agent root
cm install --agent claude --agent cursor
cm install --list                  # show agent IDs and paths
cm install --no-exe --dry-run      # skills only, show paths
cm uninstall --all                 # remove installed skills, exe, and our PATH entry
cm uninstall --agent claude
cm install --no-path               # install exe without modifying PATH
```

The global executable is copied or symlinked to `~/.local/bin/cm` on Unix and
`%USERPROFILE%\.local\bin\cm.exe` on Windows. Installation state and backups are
stored in `~/.cm/install.json` and `~/.cm/backups/`; uninstall restores a
pre-existing skill or executable when present. `cm install --all` creates the
known personal skill directories from the agent path list. `cm skill --install`
remains the project-local installation path.

## Everyday use

```text
cm remember --file cm --subject "<subject>" --text "<fact>" [--kind fact] [--scope repo] [--force] [--verify-source] [--replace]
cm recall --file cm --query "<text>" --task <task> [--scope <scope>] [--budget 800] [--full] [--snippets <n>] [--root <dir>]
cm recall --file cm --task <task>    # query comes from the task frame
cm brief --file cm [--task <task>|current] [--budget 1200] [--format json|prompt] [--snippets <n>] [--root <dir>]
cm explore --file cm --root .    # auto mode/records/budget
cm explore --file cm --root . --mechanical --apply  # no-LLM deterministic map
cm wiki plan --root . --out .cm/wiki
cm wiki prompt --page overview
cm wiki apply --page overview --input overview.md
cm wiki status
cm pack --file cm [--task <task>|current] [--budget 1200] [--snippets <n>] [--root <dir>] > pack.prompt
cm absorb --file cm --input result.jsonl
cm claim --file cm --task <task>|current --owner <id> [--lease 30]
cm release --file cm --task <task>|current [--owner <id>]
cm finish --file cm [--task <task>|current] [--summary <text>] [--root <dir>] [--changed] [--strict] [--apply]
cm forget --file cm --id <id>
cm status [--file cm]
cm cache-stats [--file cm]
cm doctor [--strict] [--file cm]
cm skill [--json] [--install] [--check]
```

`recall` returns at most 3 memories and respects the token budget. The first hit is
full text; additional hits are query-focused snippets, so 2–3 related memories
usually fit in 800 tokens. Use `--full` for complete text on every hit.
`--snippets <n>` (with JSON output) inlines the exact source definition around
each `path:symbol`, so the model can inspect the cited code without a separate
file read.

When `--file` is omitted, existing-store commands use `./cm` or the nearest
ancestor `cm`; this lets agents run from a subdirectory. `cm init` creates the
local default and does not claim an ancestor file. Source paths for
`--verify-source` are also resolved from the current directory outward, so a
repo-root-relative `path:symbol` still anchors when the agent is in a subdir.

`--task` is optional and affects only the disposable cache, never ranking. Pass a
stable id for the current work item (branch, issue, or agent task) so `cm.cache`
keeps the blocks read by this task; `--scope` remains the durable workspace
affinity. Result blocks are prefetched into that working set after a query.
When `--query` is omitted, `recall` uses the task frame's goal, next steps, open
questions, and touched paths.

`brief` is the session-resume entry point. With `--task current` it picks the
most recently updated active frame, emits the frame plus a budgeted set of
relevant memories in one JSON object, and tags the cache working set with that
task. Use an explicit task id when several tasks are active. If no frame exists,
pass `--query` to get a plain budgeted brief. `--format prompt` emits a compact
text block intended to be injected into the model prompt before the first tool
call, eliminating the brief tool turn entirely. Memories with repeated `wrong`
feedback or repeated `stale` feedback without `useful` signals are suppressed
from the brief until `govern --apply` forgets them.

`pack` and `absorb` form an optional LLM-assisted compression loop. `pack`
prints a deterministic prompt with the task frame and a budgeted set of evidence
memories; send it to any model and ask for strict JSONL. `absorb` accepts only
`remember`, `supersede`, and `forget` operations, requires evidence IDs, rejects
reserved kinds/scopes, validates `path:symbol` anchors, and reuses the ordinary
exact-duplicate check. `--dry-run` validates without writing. `--replace` makes
`remember` ops supersede the latest active record with the same
`scope+kind+subject`. `--allow-long-text` is only for re-anchoring existing
records that already exceed the normal 800-character compression limit. The
model proposes; `cm` still verifies and stores.

`explore` is the guided repository-onboarding loop. The simple form is
`cm explore --file cm --root .`; it creates or reuses a task frame, scans the
repository surface, indexes existing memories, and chooses macro/micro and the
record budget dynamically. In a git repository the snapshot comes from
`git ls-files`, so `.gitignore` and nested package areas are handled; elsewhere
it walks the tree breadth-first with explicit build/cache directory skips. Macro is for first exploration or architecture
refresh: manifests, entry points, module boundaries, build/test paths, and
invariants. Micro starts from changed files, frame touches, and existing memory
sources, then records deltas and supersedes stale memories. The prompt carries
a frame `EVIDENCE_ID` and the absorb JSONL contract, so the same `cm absorb`
command writes the result. Advanced users can override `--mode`, `--query`,
`--max-items`, or `--budget`; the prompt explicitly allows an empty result.
`explore --apply result.jsonl` is the shorthand for the `absorb` step, including
`--dry-run`, `--replace`, and `--allow-long-text`. `explore` is a guide, not an
autonomous crawler: it reports paths and candidate targets, while the model
reads the chosen files and `absorb` still validates every source and evidence ID.

For code structure, prefer the mechanical path:

```bash
cm explore --file cm --root . --mechanical --apply
```

It uses only the repository scan and manifest scripts. It records repo layout,
build/test commands, entry points, and source-area counts without sending any
code to a model. CodeGraph-style symbol/call-graph indexes remain the right tool
for structural queries; `cm` stores only the durable facts and decisions that
should survive the index.

`cm wiki` is the human-facing counterpart, closer to Qoder-style repository
documentation than to a memory outline:

```bash
cm wiki plan --root . --out .cm/wiki [--max-modules 24]  # page plan + manifest
cm wiki next --root . --out .cm/wiki     # next page id + prompt for the agent
cm wiki prompt --page overview           # prompt for one specific page
cm wiki apply --page overview --input overview.md
cm wiki review --page overview --review-cmd "your-reviewer"
cm wiki status                           # written / planned / stale pages
cm wiki check [--strict]                 # shape, line citations, wiki links
cm wiki find --query "runtime flows"     # rank pages for a task before reading
cm wiki route --query "runtime flows"    # auto-pick none / excerpt / full page
cm wiki excerpt --page overview --query "build test"  # only matching H2 sections
cm wiki generate --agent-cmd "your-agent" \
  [--review-cmd "your-reviewer"] [--review-rounds 2] [--max-pages N] [--page <id>]
```

`plan` creates overview, architecture, build/test, testing, key-flow, and
module pages from one repository scan (up to 24 module pages by default;
`--max-modules` trades breadth for generation cost). `find` ranks pages by
title, summary, and H2 section text so a cross-cutting question can route to
`key-flows` instead of the nearest module page. `route` combines both and returns
`mode: none|excerpt|full` with the chosen page/section and reasons; pass
`--task-size small` to prefer no wiki for narrow tasks. `excerpt` returns the page
title and introduction plus only the H2 sections whose text matches the query,
within `--budget`; use it instead of injecting a full page. Module pages target a
bounded set of concrete key files plus their area; when a module lives inside a
package, the plan also includes the package-root source barrel and `package.json`
so the public export map is part of the read set. Architecture pages use a
bounded set of root manifests, package index files, and entry points rather
than directory targets, which keeps prompts fast on large monorepos. Key-flow
pages prefer real `main`/`cli`/agent-loop/compaction files and exclude example
directories. Their titles combine the package name with the area
inside it (`Module: pi-ai / providers`) so a monorepo does not produce duplicate
page names. Each `prompt` includes a mechanically extracted symbol map, asks
for 800-1500 words of prose (1200-2000 for large module pages), concrete
file/symbol references, Mermaid diagrams on architecture and flow pages, and
explicit unknowns instead of guesses; it also tells the agent to stay read-only
and not run builds or tests. `apply` requires exactly one level-1 heading that
matches the planned title, at least 3 sections and 1200 characters, writes the
page atomically, and records source hashes plus the git commit. `status` diffs
each commit once for all pages, catches staged, unstaged, and untracked changes,
and falls back to content hashes outside git. `check` verifies page shape,
`path:line` citations (including rejecting line ranges and bare `:NN`
shorthand), cross-page links, module target coverage, and the per-page word
budget; `--strict` raises the gate to 4 sections and 2400 characters, requires
Mermaid on architecture/flow pages, requires the module Dependencies/Failure
modes sections, and fails while any page is still planned or stale. `generate --agent-cmd` runs the
prompt/apply loop for every pending page with any local agent command (prompt on
stdin, Markdown on stdout, command cwd set to the repo root); leading agent
preamble and an outer Markdown fence are removed before validation. Before
applying, `generate` re-hashes the page targets and the output directory; an
agent that edits files instead of returning Markdown is rejected. After apply,
`generate` runs page-level mechanical checks for target coverage and citation
resolution; a page that omits a target or cites a missing line is marked stale
and reported as failed. `--page <id>` generates one named page regardless of its
position in the plan. Generated pages live outside `cm` and are not recall
records. `cm wiki remember --file cm` can write one compact pointer record per
review-passed page (kind `procedure`, subject `wiki: <title>`, source pointing
at the page file); normal `cm recall`/`cm brief` then returns the page summary
and location without copying the full page into the hot memory file. `--page`
limits it to one page, `--no-replace` keeps existing records, and
`--include-unreviewed` bypasses the review gate.

Test and production runs can use different commands and output directories.
Use a temporary `--out` plus a cheap `--agent-cmd` for staging, and a separate
production agent/output pair for the real wiki; promote a page only after
`cm wiki check --strict` passes on the complete wiki. Never hand-edit a page
under the wiki output directory: regenerate it through `wiki generate` or feed
review findings back into the generator, so the review gate sees the actual
model output.

`generate --review-cmd` separates generation from acceptance: the generator
returns Markdown, then an independent reviewer command receives the page plus
its generation prompt on stdin and returns
`{"verdict":"pass|fail","score":0-100,"issues":["..."]}`. Failed pages are
revised with the reviewer's or mechanical findings for up to
`--review-rounds` attempts; a page is counted as written only after both the
mechanical gate and the reviewer pass. `cm wiki review
--page <id>` runs the same reviewer as a standalone gate. Review results are
persisted in the manifest (`review.verdict`, `score`, `issues`, `reviewed_at`);
`wiki status` reports `review_passed` and `review_failed`, and any new page
revision clears the old verdict. The reviewer treats these as blocking, not
notes: bare `:NN` shorthand, guard/catch citations for a throw/return claim, a
shared citation for several named exports, Mermaid edges that are not real
call/import relationships, unsupported absolute quantifiers, and a missing
TARGET file or primary export. That makes the generation loop rewrite the page
instead of accepting a merely "passing" page with known citation debt.

`claim --owner <id>` takes a lease on a task frame; a second agent is rejected
while the lease is live. `release` clears it, and `finish` clears it together
with marking the frame done. A live lease also blocks `frame set` and `finish`
unless the matching `--owner` is passed. `finish --summary ... --root <repo>
--apply` also writes one episode summary and runs audit plus governance in the
same command. With no `--summary`, it records `Completed: <goal>` so close-out
never silently forgets the outcome. `--strict` refuses to finish while open
questions remain, a `frame.touches` path has no cited memory, or the source audit
reports drift; `--changed` limits that audit to `git status` sources.

`remember` is idempotent for exact content: it checks the recent tail and, for
small indexes (≤10k docs), active index records. A duplicate returns the existing
ID with `"duplicate": true` instead of appending. Use `--force` for a deliberate
duplicate; `append`/`import` never deduplicate. Large indexes use the cheap
tail-only check to avoid decoding every record on each write. `--verify-source`
refuses a `path:symbol` source that cannot be anchored right now. `--replace`
supersedes the latest active record with the same `scope+kind+subject`, which is
the safe way to update a decision instead of creating a conflict.

`doctor` emits one compact JSON health line; `--strict` exits 1 when it reports a
problem. `cache-stats` reports cache capacity, data/index entries, hits, misses,
and evictions. `frame` stores the current task's goal, status, constraints,
next steps, open questions, and touched paths as a normal memory record; updates
supersede the previous frame. `feedback` records useful/stale/wrong signals;
`govern` turns them into verify/forget suggestions, auto-forgets exact
duplicates with `--apply`, and folds near-duplicates when `--merge` is set.
`audit --root <repo>` checks that each `path:symbol` source still exists, that
the symbol is present, and that the source excerpt hash captured at write time
still matches. `--changed` audits only sources touched in `git status`, making
the check proportional to a change set. `--scope` and `--limit` bound
large-store audits; `--refresh` re-anchors reviewed `source_changed` records
under new IDs, superseding the old records. `--apply` records stale feedback for
remaining `missing_source`, `missing_symbol`, or `source_changed` drift, which
`govern` then surfaces. `--strict` exits non-zero while unresolved issues remain,
so a CI job or a finish gate can block source drift instead of silently reading
JSON.
Source paths are part of the BM25 token stream, so file/symbol queries work
without embeddings. Read commands may create or update the disposable
`cm.cache` cache; `cm` remains unchanged.

## Advanced commands

```text
cm init [--file cm] [--empty]
cm append --file cm --json '<json>' [--durable]
cm import --file cm < records.jsonl
cm export --file cm [--format cmx|jsonl]
cm checkpoint [--in-place] [--file cm]
cm reindex [--file cm]
cm get --file cm --id <id>
cm scan [--file cm] [--limit 100] [--offset 0]
cm cache-stats [--file cm]
cm frame set --file cm --task <id> --goal <text> [--constraint <text>]... [--next <text>]... [--open <text>]... [--touch <path>]...
cm frame get --file cm --task <id>
cm frame list [--file cm]
cm brief --file cm [--task <id>|current] [--budget 1200] [--format json|prompt] [--snippets <n>] [--root <dir>] [--scope <scope>] [--top 16] [--max-items 5] [--min-trust imported] [--query <text>]
cm explore --file cm --root . [--task <id>|current] [--query <text>] [--mode auto|macro|micro] [--budget <n>] [--max-items <n>] [--format prompt|json]
cm explore --apply result.jsonl [--dry-run] [--replace] [--allow-long-text] [--file cm]
cm pack --file cm [--task <id>|current] [--budget 1200] [--snippets <n>] [--root <dir>] [--scope <scope>] [--top 16] [--max-items 8] [--query <text>]
cm absorb --file cm --input result.jsonl [--dry-run] [--replace] [--allow-long-text]
cm claim --file cm --task <id>|current --owner <id> [--lease 30]
cm release --file cm --task <id>|current [--owner <id>]
cm finish --file cm [--task <id>|current] [--summary <text>] [--root <dir>] [--scope <scope>] [--changed] [--strict] [--apply]
cm feedback --id <id> [--signal useful|stale|wrong] [--note <text>]
cm govern [--apply] [--merge] [--stale-days 90] [--wrong-threshold 2] [--file cm]
cm audit --root <repo> [--changed] [--scope <scope>] [--limit <n>] [--refresh] [--apply] [--strict] [--file cm]
cm search --file cm --query <text> [--top 8] [--scope <scope>] [--task <task>] [--kind <kind>] [--min-score <score>] [--min-trust <level>] [--include-expired]
cm context --file cm --query <text> [--budget 2000] [--scope <scope>] [--task <task>] [--kind <kind>] [--min-trust <level>] [--format cmx|json] [--snippets <n>] [--root <dir>]
cm similar --file cm --id <id> [--top 16] [--scope <scope>]
cm merge --file cm --sources <id1,id2> [--text <text>]
cm forget --file cm --id <id>
cm purge --file cm
cm conflicts --file cm
cm doctor [--strict] [--file cm]
cm verify --file cm [--full]
cm skill [--json] [--install] [--check] [--dir <path>]
```

## Export and import

```bash
cm export --file cm --format jsonl > backup.jsonl
cm init --file restored --empty
cm import --file restored < backup.jsonl
```

`jsonl` export preserves ids, timestamps, sources, and merged records.
Import a full export into an empty store. Append-only JSONL import also accepts
`{kind,scope,subject,text,source,confidence}` records.

## COW checkpoint

```bash
cm checkpoint --file cm              # compact checkpoint: stable file size
cm checkpoint --in-place --file cm   # append index generation: fastest
```

Both modes apply tail tombstones/merges/supersedes to the index active bitset
without recompressing data blocks. They do **not** compact dead blocks; use
`cm reindex` for full compaction and space reclamation.

- The index is stored as 64 KiB pages plus a page table. `checkpoint` copies
  immutable blocks and writes a fresh page index: 10k ~20ms, 1M ~1.2s.
- `checkpoint --in-place` only appends dirty index pages and a new page table,
  then flips a dual header slot. Measured: 10k ~9ms, 1M ~0.13s.
- In-place growth is proportional to dirty pages, not the whole index: 10k
  ~16KB per generation; 1M ~47KB per generation (one dirty 64KiB page + 39KB
  page table). Periodic `cm reindex` reclaims old pages.
- Every index is page-backed; there is one on-disk index format. A single-header
  file is upgraded to dual headers on its first checkpoint.
- `cm.lock` is an advisory writer lock; readers remain lock-free.

## Agent skill

The binary embeds a compact usage skill:

```bash
cm skill
cm skill --json
cm skill --check
cm skill --install
```

The skill covers recall-before-work, compact write-back, source citations, and
`cm.cache` maintenance. It is stored in the repository at `assets/SKILL.md` and
installed by default to `.agents/skills/cm-memory`.

## Accuracy

The harness supports both retrieval and the actual recall command:

```bash
python3 eval/run_eval.py --cm target/release/cm.exe --mode search
python3 eval/run_eval.py --cm target/release/cm.exe --mode recall --budget 800
```

On the `pi` fixture (18 records, 22 positive + 3 negative queries):

| Mode | Recall | MRR | Hit@1 | No-answer |
|---|---:|---:|---:|---:|
| `search --top 5` | 0.985 | 0.955 | 0.909 | 0.667 |
| `recall --budget 800 --max-items 3` | 0.939 | 0.955 | 0.909 | 0.667 |

The `search` number is retrieval@5; the recall row uses the actual command with
anchor-full plus query-snippet packing. The harder paraphrased set in
`eval/queries_hard.jsonl` gives `search@5` 0.875 and `recall@3` 0.771.

These are small lexical fixtures, not an end-to-end answer benchmark: the
fixture has only 18 records, many queries share identifiers with the expected
memories, and top-5 alone covers 5/18 of the store. Treat the numbers as
regression checks, not as a general accuracy claim.

## Design

- Single memory file `cm`; optional disposable `cm.cache` cache and `cm.lock` writer lock.
- Append-only data log with 64 KiB compressed blocks.
- Compressed deterministic index; BM25 candidate retrieval; MinHash verification.
- `cm checkpoint` reuses immutable compressed blocks; `--in-place` appends only dirty 64 KiB index pages and commits via dual headers.
- Memory payloads use a schema-aware presence map with varint lengths; defaults are omitted. `cm reindex` re-encodes old records into the compact schema.
- The index uses 8-byte ID-hash slots and delta-varint postings; older index versions remain readable.
- `cm.cache` holds decompressed data blocks and hot 64 KiB index pages (8 MiB); cold index pages are loaded from the `cm` page table on demand through an in-process page cache.
- Header version byte remains `1`. Index, cache header, and cache layout versions also stay `1` until the first public release.
- No SQLite or external database.
- Hot ops use mmap and a bounded, disk-backed block/index-page cache with TinyLFU admission/aging; eviction is frequency-first with recency only as a tie-break, so one-off scans do not evict repeatedly reused blocks.
- Cache entries carry scope and task affinity tags. Scope is the primary eviction priority; task is a tie-break and prefetch tag, so alternating tasks share the cache instead of wiping each other. Query result blocks and index pages are prefetched/tagged. This is a single-workspace working-set accelerator, not a global knowledge cache.
- Cache writes are serialized with an OS file lock and entries carry XXH3 checksums, so parallel agent processes cannot return torn blocks; a damaged cache slot falls back to decompression.

### Compact schema and size

`cm reindex` rewrites active records into the compact schema. Measured:

| Store | Before | After | Change |
|---|---:|---:|---:|
| 10k | 601,355 B | 466,680 B | -22.4% |
| 1M | 83,453,435 B | 68,186,941 B | -18.3% |

The compact payload removes field-ID/type overhead; delta-varint postings reduce the BM25 index from 88 MB of raw postings to ~14 MB. The 1M file is now roughly balanced between data blocks and the page-compressed index.

## Performance

Measured on a 1,000,030-record store (Windows x64, release build):

| Operation | p95 |
|---|---:|
| `scan --limit 100` | ~11 ms |
| `get --id` | ~6 ms |
| `similar --top 16` | ~11 ms |
| `append` | ~8 ms |
| `search --top 8` | ~16 ms |

With lazy and disk-backed index pages, a reindexed 1M store measured 32–48 ms
for `search --top 8` including process startup; a 20-term query that walks 126
index pages measured ~35–48 ms warm. The first process after a fresh build or
cache replacement can be slower while the OS reloads the store.
`cm.cache` remains 8 MiB instead of ~114 MiB. Numbers are workload-dependent.

`checkpoint` (cold process, 1M records) measured ~1.2s; 10k ~20ms.
`checkpoint --in-place` measured ~0.13s / 1M and ~9ms / 10k, adding only one
dirty index page plus the page table per changeset. A full `reindex` of the
same 1M store is roughly 10-20s. Numbers are workload-dependent; use `cm verify`
after bulk changes.

Maintenance on the 1M synthetic store after the bulk active-document scan:
`audit` ~10s (one million source-path checks), `govern` ~12s, `conflicts` ~16s.
These are maintenance operations, not query paths; hot searches remain
sub-100ms. Full-store audit still scales with the number of cited sources.

Working-set checks:

- 90k records, alternating parser/database queries: both plain and `--task`
  measured ~16 ms average because both working sets fit in 8 MiB. The task tag
  is therefore a tie-break/prefetch hint, not a frequency override; it does not
  hurt the plain path.
- 1M diverse synthetic corpus (1M records, 191 MB store): single-term
  `search` measured 27–32 ms warm and a three-term query 45–48 ms; the cache
  held 95 index pages plus 8 data blocks (capacity 127). Doc metadata pages are
  deliberately decompressed in batches and kept out of the shared cache; caching
  them displaced hot postings pages and measured slower.
- 1M page-index store, 20-term query: 32–48 ms warm including process startup;
  the query touched 126 index pages and one data block, using all 127 cache
  slots. Alternating two long task queries measured 21.3 ms average with
  `--task` vs 22.2 ms plain, with more cache hits (2237 vs 2208).
- 8 concurrent processes × 30 mixed queries: 240/240 succeeded; 64 concurrent
  long queries on the 1M store also succeeded. Cache entries carry XXH3
  checksums so a torn slot falls back to the store.
- Agent A/B on the same read-only cache review: with one `cm recall` call the
  subagent used 307k tokens / 17 tool uses / 3 source files; without cm it used
  604k tokens / 26 tool uses / 5 files. Single run; model- and task-dependent.

## Known limits

- Short one-shot tasks pay 20–55% extra tokens/tools; use `cm` only when it can
  replace at least two discovery turns.
- Retrieval is lexical BM25 + MinHash. Paraphrases with no shared tokens are a
  known miss; embeddings are deliberately not used.
- `audit` detects missing sources, missing symbols, and excerpt-hash drift. It
  does not prove that a claim is semantically true.
- Task leases are cooperative: they prevent accidental duplicate work, not
  malicious writers or distributed consensus.
- Multi-session A/B on a small config task: -7.0% tokens and -4.5% wall time,
  but +28.6% tool uses. Multi-session value needs larger repositories and more
  than two sessions; short tasks remain a loss.

## Full automatic wiki loop

`eval/wiki_auto.mjs` runs `plan -> generate batches -> check --strict -> remember`
and stops with a non-zero exit code if the wiki is not clean.

```bash
node eval/wiki_auto.mjs --cm target/release/cm.exe \
  --root C:/wp/empty/pi --out C:/wp/empty/wiki-v10 \
  --agent-cmd "<agent command>" --review-cmd "<review command>" \
  --rounds 2 --batch 5 --max-batches 20 \
  --memory C:/wp/empty/cm --remember
```

`AGENTS.md` is the short agent-facing guide: `brief` once, `wiki route` first,
one full page/excerpt at most, never hand-edit generated pages, and only
`wiki remember` after `check --strict` is clean.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
```

See [agent evaluation](docs/agent-eval.md) for the read-only A/B protocol and
long-task loop.

## License

MIT. See [LICENSE](LICENSE).
