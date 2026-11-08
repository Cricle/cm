# Agent guide: cm memory and repository wiki

This repository ships a single-file memory store (`cm`) and a generated
repository wiki under `.cm/wiki`.

## Session start

If a `cm` memory file exists, run once:

```bash
cm brief --file cm --task current --budget 1200
```

If there is no active frame and you know the task, pass `--query` instead.
Do not follow a sufficient brief with another `recall` call.

## Wiki context: route first

Before reading a generated wiki page, let `cm` choose the cheapest useful form:

```bash
cm wiki route --root . --out .cm/wiki \
  --query "<task or question>" \
  --budget 800 --max-sections 2 --task-size auto
```

Interpret `mode`:

- `none` — do not use wiki; read source directly.
- `excerpt` — use the returned `text`, then verify key claims in source.
- `full` — use the returned one-page text as coherent context.

Rules:

- For a narrow single-symbol or single-file task, pass `--task-size small`.
- Never read multiple full wiki pages for one task.
- If route returns `full`, do not open a second wiki page.
- If route returns `excerpt` but the question spans several sections, use one
  full page instead.

If you need candidates first:

```bash
cm wiki find --query "<task>" --limit 5
cm wiki excerpt --page <id> --query "<task>" --budget 800 --max-sections 2
```

## Wiki maintenance

Never hand-edit `.cm/wiki/*.md`: generated pages are model output and are
validated by `cm wiki check`.

```bash
cm wiki status --root . --out .cm/wiki
cm wiki check --root . --out .cm/wiki --strict
cm wiki generate --root . --out .cm/wiki \
  --agent-cmd "<agent command>" --review-cmd "<review command>" \
  --review-rounds 2 --max-pages 5
```

For a fully automated loop, use `eval/wiki_auto.mjs`.

A page is production-usable only when:

1. `cm wiki check --strict` reports zero issues.
2. Its reviewer verdict is `pass` with empty `issues`.
3. `cm wiki remember` accepts it.

## Durable memory writes

Write a memory only when a non-obvious decision must survive the session:

```bash
cm remember --file cm --kind decision --scope repo \
  --subject "<short key>" --text "<what changed and why>" \
  --source "<path:symbol>" --trust authored
```

Do not store transcripts, secrets, or long code blocks. Mark memories
`feedback --signal wrong|stale` when they mislead you.

## Wiki -> memory

After a complete strict-checked wiki exists:

```bash
cm wiki remember --file cm --root . --out .cm/wiki
```

This writes compact `procedure` pointer records (summary + page path + targets),
not full page prose. Recall then routes an agent to a single page or excerpt.
