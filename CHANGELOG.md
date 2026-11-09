# Changelog

## 0.3.0 - 2026-11-08

- `ops` module: the full CLI command set is now callable without the `cli`
  feature (govern, audit, conflicts, similar, purge, doctor, scan, pack,
  finish-with-strict/apply, frame-set merge, export/import, checkpoint,
  reindex, verify).
- `Cm` facade additions: `remember_with`, `append_json`, `brief_with`, `pack`,
  `govern`, `audit`, `conflicts`, `similar`, `purge`, `doctor`, `scan`,
  `finish_with`, `frame_set_with`, `export`, `import_jsonl_text`,
  `cache_stats`, `verify_report`, `checkpoint_op`, `reindex_op`.
- Public `AppendInput`/`MemoryOut`, `ops::*Options`/report types, and
  `SKILL_TEXT` for embedders.
- `cm` CLI remains a thin wrapper over the same operations; output shapes are
  unchanged.
- Hardening: imports enforce `SourcePolicy`, reject `frame`/`feedback` and
  reserved scopes, cap trust at `imported`, regenerate ids and strip
  supersede/merge metadata; source anchors/snippets/audit reads are confined to
  the canonical project root; `remember --replace` supersedes every active
  duplicate; task-frame selection never resurrects done/paused frames.

## 0.2.0 - 2026-11-08

- Single-file `cm` store with append-only records, 64 KiB compressed index
  pages, mmap page table, XXH3 checksums, tombstones, and atomic purge/reindex.
- Library API (`cm_memory::Cm`) alongside the `cm` CLI, with an in-memory or
  disk cache, source policies, and the same on-disk format as the CLI.
- Cognitive layer: task frames, budgeted `cm brief`, `cm recall`, feedback
  governance, source-anchor audit, finish close-out, and owner leases.
- Repository wiki: deterministic plan, per-page prompts, `apply`, independent
  `review`, `check --strict`, `generate`, `find`, `route`, `excerpt`, and
  review-passed `remember` pointers.
- Wiki generation respects target coverage, exact citations, source freshness,
  page word budgets, and exposes `mode: none|excerpt|full` through
  `cm wiki route`.
- Evaluation harnesses and recorded A/B results for memory, full wiki pages,
  section excerpts, and automatic routing.
