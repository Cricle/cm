# cm-memory

[![CI](https://github.com/Cricle/cm/actions/workflows/ci.yml/badge.svg)](https://github.com/Cricle/cm/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`cm` is a single-file, compressed, mmap-backed memory store for AI agents.
It is not Markdown and not a database server: one file holds an append-only
data log, a compressed BM25/MinHash index, and an optional disposable `cm.lru`
cache.

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
cm remember --file cm --subject "<subject>" --text "<fact>" [--kind fact] [--scope repo]
cm recall --file cm --query "<task>" [--scope <scope>] [--budget 800]
cm forget --file cm --id <id>
cm status [--file cm]
cm doctor [--strict] [--file cm]
cm skill [--json] [--install] [--check]
```

`recall` returns at most 3 compact memories and respects the token budget. `doctor` emits one compact JSON health line; `--strict` exits 1 when it reports a problem. Read commands may create or update the disposable `cm.lru` cache; `cm` remains unchanged.

## Advanced commands

```text
cm init [--file cm] [--empty]
cm append --file cm --json '<json>' [--durable]
cm import --file cm < records.jsonl
cm export --file cm [--format cmx|jsonl]
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

## Agent skill

The binary embeds a compact usage skill:

```bash
cm skill
cm skill --json
cm skill --check
cm skill --install
```

The skill covers recall-before-work, compact write-back, source citations, and
`cm.lru` maintenance. It is stored in the repository at `assets/SKILL.md` and
installed by default to `.agents/skills/cm-memory`.

## Accuracy

The repository includes a retrieval accuracy harness in [`eval/`](eval/). Current baseline on the `pi` fixture:

```text
Recall@5   0.985
MRR        0.955
Hit@1      0.909
No-answer  0.667 (default); 1.000 with --min-score 3.0
```

## Design

- Single memory file `cm`; optional disposable `cm.lru` cache.
- Append-only data log with 64 KiB compressed blocks.
- Compressed deterministic index; BM25 candidate retrieval; MinHash verification.
- Header version byte remains `1`.
- No SQLite or external database.
- Hot ops use mmap and a bounded LFU/LRU block cache.

## Performance

Measured on a 1,000,030-record store (Windows x64, release build):

| Operation | p95 |
|---|---:|
| `scan --limit 100` | ~7 ms |
| `get --id` | ~6 ms |
| `similar --top 16` | ~11 ms |
| `append` | ~8 ms |
| `search --top 8` | ~12 ms |

Numbers are workload-dependent; use `cm verify` after bulk changes.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release
```

## License

MIT. See [LICENSE](LICENSE).
