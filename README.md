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

## Everyday use

```text
cm remember --file cm --subject "<subject>" --text "<fact>" [--kind fact] [--scope repo] [--force]
cm recall --file cm --query "<task>" [--scope <scope>] [--budget 800] [--full]
cm forget --file cm --id <id>
cm status [--file cm]
cm doctor [--strict] [--file cm]
cm skill [--json] [--install] [--check]
```

`recall` returns at most 3 memories and respects the token budget. The first hit is
full text; additional hits are query-focused snippets, so 2–3 related memories
usually fit in 800 tokens. Use `--full` for complete text on every hit.

`remember` is idempotent for exact content: it checks the recent tail and, for
small indexes (≤10k docs), active index records. A duplicate returns the existing
ID with `"duplicate": true` instead of appending. Use `--force` for a deliberate
duplicate; `append`/`import` never deduplicate. Large indexes use the cheap
tail-only check to avoid decoding every record on each write.

`doctor` emits one compact JSON health line; `--strict` exits 1 when it reports a
problem. Read commands may create or update the disposable `cm.cache` cache; `cm`
remains unchanged.

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
cm search --file cm --query <text> [--top 8] [--scope <scope>] [--kind <kind>] [--min-score <score>] [--min-trust <level>] [--include-expired]
cm context --file cm --query <text> [--budget 2000] [--scope <scope>] [--kind <kind>] [--min-trust <level>] [--format cmx|json]
cm similar --file cm --id <id> [--top 16] [--scope <scope>]
cm merge --file cm --sources <id1,id2> [--text <text>]
cm consolidate --file cm [--apply] [--top 8] [--limit 50] [--offset 0] [--threshold 0.85]
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
- Old single-header and single-blob indexes migrate on their first checkpoint;
  subsequent checkpoints use page-level COW.
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
- Header version byte remains `1`.
- No SQLite or external database.
- Hot ops use mmap and a bounded block cache with TinyLFU admission/aging + LFU/LRU victim selection; one-off scans do not evict repeatedly reused blocks.

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
| `scan --limit 100` | ~7 ms |
| `get --id` | ~6 ms |
| `similar --top 16` | ~11 ms |
| `append` | ~8 ms |
| `search --top 8` | ~8 ms |

`checkpoint` (cold process, 1M records) measured ~1.2s; 10k ~20ms.
`checkpoint --in-place` measured ~0.13s / 1M and ~9ms / 10k, adding only one
dirty index page plus the page table per changeset. A full `reindex` of the
same 1M store is roughly 10-20s. Numbers are workload-dependent; use `cm verify`
after bulk changes.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
```

## License

MIT. See [LICENSE](LICENSE).
