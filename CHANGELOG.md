# Changelog

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
