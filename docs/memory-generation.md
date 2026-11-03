# Memory generation v0.1

Scope: one coding repository or one coherent workspace. This is not a generic
document ingestion system.

## Page shape

Generate wiki pages, not line-level snippets:

- one topic per page, 2–4 tightly related claims
- 300–450 characters
- `subject = <project>/<module>/<entity>/<aspect>`
- `source = path:symbol@commit` (one or more comma-separated)
- `trust = verified` only after source verification
- optional short Chinese keyword line when the project is used by Chinese
  speakers, e.g. `中文: 重启 恢复 工具 系统提示`
- no raw code blocks, transcripts, progress logs, or generic summaries

## Pipeline

```text
cm explore --mode macro|micro
  -> model reads the guided targets and existing memory index
  -> atomic claim extraction
  -> source verification
  -> source repair (find the right path/symbol, do not silently drop)
  -> module coverage checklist
  -> page merge (2-4 related claims)
  -> duplicate/conflict check
  -> cm absorb
  -> blind answer test
```

`cm explore` supplies the deterministic prompt: repository snapshot, macro/micro
targets, known-memory IDs for `supersede`/`forget`, an exploration frame evidence
ID, and the strict `absorb` JSONL contract. The model still does the reading and
claim extraction; `absorb` validates every source anchor and evidence ID.

### Source repair

A candidate with a true claim and a wrong anchor is not discarded. The repair
stage searches the repository for the real path/symbol and rewrites `source`.
Candidates that remain unsupported are dropped.

### Coverage checklist

For each module check:

- purpose and entrypoint
- public interfaces and data models
- state and persistence
- recovery and failure behavior
- build/test/release procedures
- constraints, limits, and non-obvious behavior

### Page merge

Do not publish one isolated claim per record. Merge related claims into a page
that an agent can use for synthesis. The tested target is 2–4 claims and
300–450 characters; split only when claims answer different queries.

## Test result (pi workspace)

```text
75 raw candidates
49 independently verified
67 repaired/split records
54 wiki pages after merging
average page length 433 characters
7 project/module map pages
0 duplicate subjects
0 source-missing pages
conflicts = 0
doctor = ok
```

Answer-level tests with `cm recall --budget 800`:

- original 10-question set: 8 complete, 1 partial, 1 negative correct
- held-out topic set: 9/9 positive answers correct, 1/1 negative correct

These sets are still authored in-house. The next promotion gate is a query set
written by someone who did not see the generated pages.

## Promotion gate

Do not publish generated pages to a project until:

1. two different projects pass the blind test;
2. every page has a verified source;
3. no unresolved conflicts;
4. held-out questions were authored independently;
5. scope isolation is tested when more than one repo is present.

The generator does not attempt PDFs, issue trackers, chat logs, or unrelated
project trees.
