# cm positioning

`cm` is a **project memory file for coding agents**. It is a hot-discovery
index for source-grounded project knowledge, not a universal knowledge base.

## Target

- One coding agent working in one repository or a small, coherent workspace.
- 2–10 related projects only when they share contracts, services, or domain
  vocabulary; otherwise use separate `cm` files.
- Questions about architecture, behavior, interfaces, constraints, build/test,
  recovery, and prior decisions.

## Non-goals

- Company-wide or multi-tenant memory.
- General document search, PDF/issue/chat ingestion, or web knowledge.
- A full wiki that humans read.
- Exhaustive symbol-by-symbol documentation.
- A replacement for source code, tests, or official docs. Source remains the
  source of truth.
- Semantic embedding retrieval. `cm` uses explicit scope/kind filters and BM25
  over compact memories.

`cm.cache` is not a knowledge layer. It is a disposable, workspace-local
working-set accelerator: scope affinity keeps the current project hot, task
tags identify the current work item and break eviction ties, and query results
are prefetched. Nothing in the cache is authoritative; deleting it only costs a
cold start.

The task frame is the small working-memory layer: goal, status, constraints,
next steps, open questions, and touched paths for the current work item. It is
stored as a normal memory record (`kind=frame`), superseded on update, and kept
separate from project facts. `cm brief` composes the active frame with a
budgeted task working set so a new session or another agent can resume without
re-reading the repository.

## Memory shape

A useful `cm` has three layers:

1. **Project map** — purpose, stack, entrypoints, build/test, package layout.
2. **Module maps** — responsibilities, key files/symbols, dependencies, limits.
3. **Wiki pages** — one topic each, with 2–4 tightly related claims: fact,
   decision, constraint, procedure, or gotcha.

Pages should be 300–450 characters, carry `path:symbol@commit` evidence, and
use a canonical subject such as:

```text
<project>/<module>/<entity>/<aspect>
```

Long essays, raw code blocks, transcripts, progress logs, and generic summaries
do not belong in the hot-discovery layer.

## Generation scope (v0.1)

Input:

- repository tree, README/manifests, source symbols, docs, tests, git metadata;
- one repository or a coherent workspace.

Not input:

- arbitrary PDFs, chat logs, issue trackers, or unrelated project trees;
- inferred facts without a traceable source.

Required gates:

1. every page has a source path/symbol and `source_commit`;
2. one topic per page, 2–4 related claims; split only when claims answer
   different queries;
3. independent verifier checks the claims against source;
4. source anchors are repaired, not silently dropped;
5. exact/near duplicates removed;
6. `cm conflicts` and `cm doctor --strict` pass before publishing.

## Quality bar

- source coverage: 100%
- unsupported claims: 0
- average page length: 300–450 characters
- exact duplicate rate: <1%
- near-duplicate rate: <2%
- blind answer test on held-out project questions; do not use the generator's
  own prompts as the acceptance set
- cross-project isolation: every recall is scope-filtered or explicitly
  workspace-scoped; no silent mixing of unrelated projects
