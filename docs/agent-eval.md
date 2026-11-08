# Agent memory evaluation

This is a small, repeatable protocol for checking whether `cm` changes real agent
behavior rather than only benchmark numbers.

## Read-only A/B

Task: answer how cache eviction, scope/task affinity, and `cache-stats` work in
this repository.

- Run A: call `cm recall --task <id>` first, then inspect only cited files.
- Run B: no memory CLI.
- Record: subagent tokens, tool uses, source files opened, answer correctness.

One measured run on the same task:

| Run | Tokens | Tool uses | Source files |
|---|---:|---:|---:|
| with cm recall | 307k | 17 | 3 |
| without cm | 604k | 26 | 5 |

This is a single run and is model/task dependent. The useful signal is the
direction: recall reduced exploration, not proof of general improvement.

## Long task loop

For a multi-step coding task:

1. `cm frame set --task <id> --goal ... --constraint ... --next ...`
2. `cm claim --task current --owner <agent-id> --lease 30` when multiple agents may work.
3. `cm brief --task current` at the start of each resumed step; `cm recall --task <id>` before implementation.
4. At decisions: write one `cm remember` record and update the frame.
5. Mark only materially useful/wrong memories with `cm feedback`.
6. At the end: `cm finish --task current --summary "<outcome>" --root . --apply`.

Track task success, turns, tokens, repeated file reads, and whether the frame's
next steps stayed current. Compare against the same task with memory disabled.

## Measured long-task harness

`eval/agent_ab.py` runs task fixtures with/without `cm` and verifies success
independently. Three one-session tasks measured with the same model:

| Task | Mode | Success | Tokens | Tool uses | Wall time |
|---|---|---:|---:|---:|---:|
| cache_bug | with cm | yes | 464,609 | 21 | 117.2s |
| cache_bug | without cm | yes | 294,463 | 14 | 87.3s |
| config_refactor | with cm | yes | 346,012 | 20 | 112.6s |
| config_refactor | without cm | yes | 246,496 | 15 | 99.4s |
| feature_median | with cm | yes | 342,462 | 16 | 102.9s |
| feature_median | without cm | yes | 222,302 | 11 | 65.9s |

The honest result: on short one-session tasks, the memory protocol is pure
overhead. `cm` is for cross-session continuity, accumulated decisions, and large
repositories where re-discovery is expensive; it should be skipped for trivial
one-shot edits.

## Realistic read-only A/B

Same model, same question: explain cache eviction, scope/task affinity,
cache-stats, source anchors, and audit with exact file/symbol citations.

| Mode | Tokens | Tool uses | Wall time | Files read |
|---|---:|---:|---:|---:|
| with cm brief | 374,808 | 19 | 104.0s | 3 |
| without cm | 986,555 | 34 | 207.1s | 5 |

`cm` cut tokens by **62.0%**, tool uses by **44.1%**, and wall time by **49.8%**.
This is the intended profile: on a non-trivial task in a real repository, one
brief replaces enough discovery turns to pay for itself. The boundary is task
size, not memory size.

## Multi-session A/B

Same config_refactor task, split into two fresh agent sessions sharing the
workspace. Session 1 only inspects and plans; session 2 applies the rename and
runs tests. The with-cm condition keeps one store and runs one brief per session.

| Mode | Session 1 | Session 2 | Total tokens | Tool uses | Wall time |
|---|---:|---:|---:|---:|---:|
| with cm | 80,715 | 122,495 | 203,210 | 18 | 67.4s |
| without cm | 120,346 | 98,179 | 218,525 | 14 | 70.6s |

Both modes succeeded. With cm saved 7.0% tokens and 4.5% wall time overall, but
used 28.6% more tool calls. Session 1 benefited from the brief; session 2 paid
more for the extra brief than it saved on this tiny repository. The honest
conclusion: multi-session benefit is real but marginal here; it needs larger
repositories and more than two sessions to dominate the fixed per-session brief
cost.

## Prompt-injected A/B

Same architecture question, but the runner precomputed `cm brief --format prompt
--snippets 12` and put it in the task file; the agent made no cm calls.

| Mode | Tokens | Tool uses | Wall time | Files read |
|---|---:|---:|---:|---:|
| pre-injected brief | 302,217 | 14 | 67.9s | 3 |
| without cm | 333,255 | 14 | 83.1s | 4 |

Prompt injection cut tokens by 9.3% and wall time by 18.3% with the same tool
count. It removes the brief tool turn, but it does not replace source reading on
this question; both agents still opened the cited files. Treat injection as a
smaller, safer win than the earlier tool-call A/B, not a replacement for
repo exploration.


## Why it is pure overhead

A follow-up cache_bug run replaced the ritual `brief` + `recall` + `remember` +
help probes with one `brief`:

| Mode | Success | Tokens | Tool uses | Wall time |
|---|---|---:|---:|---:|
| with cm, one brief | yes | 143,119 | 8 | 41.5s |
| without cm | yes | 118,935 | 6 | 36.2s |

Overhead fell from +57.8% to +20.3%, but remained pure cost: the brief saved zero
file reads because the prompt/tests already contained the facts and the workspace
was tiny. The remaining +24k tokens were two extra turns (brief plus one
malformed call); each turn re-sends the full conversation/tool context, about
12k tokens here, versus a 1.3KB brief output.

The design rule is therefore: **cm must remove more tool turns than it adds.**
Use `brief` once, do not follow it with `recall` unless a fact is missing, and do
not write a memory for a one-shot task. Memory sources must be `path:symbol`
pointers so the first read is the right file.


## Pack/absorb smoke test

`cm pack --task cm-help-preinject --budget 1200` produced a 4.8KB prompt. A
subagent returned 3 strict JSONL records using only evidence IDs from the pack:

| Metric | Value |
|---|---:|
| Subagent tokens | 46,565 |
| Tool uses | 1 |
| Wall time | 129.2s |
| `absorb --dry-run` accepted | 3/3 |

This validates the contract end to end; it is not yet a benefit claim. The next
gate is comparing `pack → model → absorb` against direct `remember` on the same
task.

## Direct remember vs pack/absorb

Same task, same starting store, same model, two fresh copies of `cm`. Both were
asked to compress the architecture explanation into three durable memories.

| Path | Agent tokens | Agent tool uses | Wall time | Memories | Source quality |
|---|---:|---:|---:|---:|---|
| direct `remember` | 175,616 | 8 | 52.6s | 3 | 3/3 with `path:symbol:line` |
| `pack → model → absorb` | 42,242 | 1 | 35.6s | 3 | 2/3 with line, 1 path-only |

`absorb` itself was one extra non-agent command. Even counting it, the pack path
used ~75% fewer agent tokens and ~75% fewer tool calls. In this single run it
also produced no subject collision, while direct `remember` created one duplicate
subject. The trade-off is precision: the direct path produced symbol+line anchors
on all three records; the pack path left one source as a bare path.

Working rule: use direct `remember` for a few symbol-precise facts; use
`pack → absorb` when compressing many notes or when the agent should not spend
turns on memory bookkeeping.

## Pi prompt-injected demo (not equal-tools)

Real repository: `C:\wp\empty\pi`. An early run precomputed `cm brief` and placed
it in the memory-side task file; the control received only the task text. This is
a product-use demo, **not an equal-tools A/B**, and it is not used as the
primary evidence for memory value.

| Mode | Tokens | Tool uses | Wall time | Files read |
|---|---:|---:|---:|---:|
| with pre-injected brief | 274,591 | 9 | 107.9s | 4 |
| without cm | 350,430 | 16 | 147.4s | 6 |

## Re-anchoring imported fixtures

The 18 pi fixture records originally carried `agent1-oracle` as their source, so
they could not pass source audit. A subagent mapped every record to a real
`path:symbol` anchor and returned 18 `absorb --replace` ops. `--allow-long-text`
was needed because the original records exceed the normal compression limit.

The first apply exposed a real bug: duplicate detection ran before supersede
semantics, so identical text plus a new source was reported as 18 duplicates and
nothing was written. After making duplicate detection ignore records the new
memory supersedes, `absorb --replace` wrote all 18. `audit --root .` then checked
18 anchors with **0 issues**, and `doctor --strict` reported 20 active records,
0 conflicts, 0 expired, full checksum OK. Identical-text re-anchoring is now a
covered CLI test.

## Pi equal-tools A/B

Two fresh clones of pi HEAD with identical tracked files and the same `cm`
binary available. Both agents received exactly the same task text and the same
instruction to use the store only if useful. Each store had an active `fair-pi`
frame. The treatment store contained the 20 re-anchored compaction memories; the
control store contained only the frame. No brief was pre-injected into either
task: both agents had to call `cm brief` or `cm recall` themselves.

Three paired runs on the same task:

| Run | Mode | Tokens | Tool uses | Wall time |
|---|---|---:|---:|---:|
| 1 | with cm memory | 182,491 | 13 | 402.9s |
| 1 | without cm memory | 890,034 | 28 | 836.9s |
| 2 | with cm memory | 419,809 | 14 | 284.0s |
| 2 | without cm memory | 727,137 | 39 | 333.3s |
| 3 | with cm memory | 305,238 | 18 | 371.6s |
| 3 | without cm memory | 535,085 | 22 | 442.4s |

Aggregate across the three pairs: **-57.8% tokens**, **-49.4% tool uses**, and
**-34.4% wall time** (`907,538` vs `2,152,256` tokens). Per-run token reductions
were 79.5%, 42.3%, and 43.0%; the spread is real, so the aggregate is more
honest than any single run. All six answers were source-correct in spot review
against `findValidCutPoints` (394), `findCutPoint` (446), `compact` (952), and
`tokensBefore` (884). Raw metrics: `eval/results/pi-equal-tools-ab.json`.

## Second project: pir memory gate

To avoid claiming a single-repository success, the same loop was run on `pir`
(commit `305c831`; 2,123 tracked files, 664 Rust source files). A macro
`cm explore` session produced 8 source-grounded memories; a separate clone
without `cm` authored 5 diagnostic questions. The answer agent had the memory
store and source access.

All five answers were independently graded **correct**. The cost was not
acceptable: the answer run used **4,870,052 tokens / 78 tool uses / 874.6s**,
because the 8 macro memories did not cover four of the five questions. A micro
gap-fill pass added 8 more memories (16 total, audit 0), but a single-question
recheck (`ToolScheduler` ordering) still used **986,738 tokens / 35 tools /
242.4s**, versus **983,708 / 22 / 206.4s** cold. The second-project correctness
gate passes; the efficiency gate does not.

A second, `ToolScheduler`-specific micro pass added 8 dense records (24 total,
audit 0). Repeating the same q2 task then used **357,670 tokens / 16 tool uses /
60.4s**, versus **983,708 / 22 / 206.4s** cold: **-63.6% tokens, -27.3% tool
uses, -70.7% wall time**, with a correct verifier verdict. The practical fix is
module-level density, not just raising the macro record count.
Raw metrics: `eval/results/pir-memory-gate.json`.

## Large-repo cross-session pilot (negative)

The pir work also served as a two-session pilot: session 1 built memory (macro
plus micro gap-fill), session 2 answered one `ToolScheduler` question with the
persisted store versus a cold clone. The memory build cost 3,348,911 tokens /
81 tool uses; the session-2 comparison was:

| Session 2 mode | Tokens | Tool uses | Wall time |
|---|---:|---:|---:|
| with persisted memory | 986,738 | 35 | 242.4s |
| cold clone | 983,708 | 22 | 206.4s |

For one question the memory did not pay back at all; it was slightly slower and
used more tools because the agent still verified source. After the second
module-density pass, session 2 q2 improved to **357,670 tokens / 16 tools /
60.4s**, but the total build cost became **5,521,755 tokens / 115 tools** across
three memory-building passes. The session-2 path is now fast; total cost is
still negative for one question and only amortizes across many follow-ups or a
long coding task. The small config_refactor multi-session result remains a
separate, also-marginal data point.

## Dynamic explore e2e

Fresh pi clone with an empty `cm`; command used exactly `cm explore --file cm
--root .` (no mode, record, or budget flags). It selected **macro** and
`MAX_RECORDS: 14`; the model returned 14 records, `cm explore --apply` accepted
and wrote all 14, and `audit --strict` checked 14 with 0 issues.

A new `src/probe.rs` was then added. The same command selected **micro** with
`MAX_RECORDS: 7`; the model returned one delta record, apply accepted it, audit
checked 15 with 0 issues, and `doctor --strict` finished at 16 active records
including the exploration frame. This verifies the simple dynamic path end to
end. Raw metrics: `eval/results/dynamic-explore-e2e.json`.

## What not to claim

`cm` is a memory sidecar. It does not prove reasoning gains from one A/B run;
long-horizon task success and source-correctness remain the deciding metrics.
Only runs with the same task prompt, same model, same repository commit, and the
same available tools count as A/B evidence. A pre-injected brief is a product
demo, not an equal-tools comparison, and must not be mixed into the same table.

## Production content A/B (2026-11-07)

Two production artifacts were tested with the same Explore agent, same task
text, and the same tools. Values are per-task medians. Three wiki tasks and the
cache memory task have three paired runs; modes, extensions, frames, and store
have one. Medians are primary because one core treatment run was a long outlier.

### Production memory (`C:\wp\empty\cm`)

| Task | Mode | Tokens | Tool uses | Wall time | Correct |
|---|---|---:|---:|---:|---:|
| cache/affinity/audit (N=3) | with brief | 473,118 | 27 | 284.6s | yes |
| cache/affinity/audit (N=3) | control | 1,011,000 | 31 | 279.2s | yes |
| frames/brief/governance (N=1) | with brief | 192,941 | 15 | 177.6s | yes |
| frames/brief/governance (N=1) | control | 624,591 | 31 | 328.3s | yes |
| store/index/audit (N=1) | with brief | 683,676 | 15 | 258.0s | yes |
| store/index/audit (N=1) | control | 1,336,412 | 48 | 387.9s | yes |

Average of per-task median deltas: **-57.0% tokens**, **-44.4% tool uses**,
**-25.8% wall time**, with source-correct answers.

### Production wiki (`wiki-v10`, one page)

| Task | Mode | Tokens | Tool uses | Wall time | Correct |
|---|---|---:|---:|---:|---:|
| core compaction/retry/queues (N=5) | wiki | 555,323 | 28 | 156.8s | yes |
| core compaction/retry/queues (N=5) | control | 657,422 | 28 | 198.3s | yes |
| pi-ai providers/auth (N=3) | wiki | 521,505 | 17 | 240.1s | yes |
| pi-ai providers/auth (N=3) | control | 871,434 | 31 | 267.5s | yes |
| pi-durable storage (N=3) | wiki | 330,284 | 15 | 204.4s | yes |
| pi-durable storage (N=3) | control | 1,016,161 | 35 | 317.7s | yes |
| modes/RPC (N=1) | wiki | 183,758 | 14 | 99.1s | yes |
| modes/RPC (N=1) | control | 337,907 | 21 | 168.8s | yes |
| extensions (N=1) | wiki | 326,069 | 18 | 111.1s | yes |
| extensions (N=1) | control | 694,125 | 19 | 269.7s | yes |

Average of per-task median deltas: **-44.4% tokens**, **-28.2% tool uses**,
**-33.4% wall time**, with source-correct answers in every run.

The core task is the noisiest: N=5, with one severe outlier (1.42M tokens, 403s).
Its median still shows a -15.5% token gain but tool use is flat; the other task
types are consistently positive. This is why the aggregate uses medians and why
repeated runs matter.

### Failure mode: multiple pages

Giving both `providers.md` and `auth.md` for one broad task:

| Mode | Tokens | Tool uses | Wall time |
|---|---:|---:|---:|
| two wiki pages | 1,831,138 | 43 | 299.8s |
| no wiki | 689,602 | 27 | 157.2s |

That is **+165.5% tokens**, **+59.3% tool uses**, **+90.7% wall time**. The wiki
is a single-page routing aid, not a multi-page context dump.

Conclusion: use exactly one focused page, or a compact pointer summary. This is
why `cm wiki remember` stores a short pointer record rather than copying page
prose into `cm`, and why `cm wiki check` enforces the per-page word budget.

Caveat: per-task N is 3 for three wiki tasks and the cache memory task; other
tasks are N=1. Directions are consistent but not a significance test.

Raw metrics: `eval/results/prod-memory-ab-2026-11-07.json` and
`eval/results/wiki-production-ab-2026-11-07.json`.

## Wiki section excerpt A/B (2026-11-08)

`cm wiki excerpt --page ... --query ... --budget N --max-sections N` returns the
page title, introduction, and only the H2 sections selected by query terms. The
excerpt files were 3.3-4.1 KB (about 445-918 estimated tokens), versus a full
page of about 19 KB.

Same tasks and control medians as the production wiki A/B; excerpt treatment was
one run per task.

| Task | Control tokens/tools | Full-page tokens/tools | Excerpt tokens/tools | Excerpt vs control |
|---|---:|---:|---:|---:|
| providers/auth | 871,434 / 31 | 521,505 / 17 | 451,920 / 17 | -48.1% tokens |
| durable storage | 1,016,161 / 35 | 330,284 / 15 | 394,028 / 11 | -61.2% tokens |
| modes/RPC | 214,565 / 12 | 183,758 / 14 | 296,181 / 27 | **+38.0% tokens** |
| extensions | 287,231 / 11 | 326,069 / 18 | 1,059,730 / 33 | **+269% tokens** |
| pi-ai auth | 220,655 / 17 | 286,630 / 23 | 250,588 / 20 | **+13.6% tokens** |
| pi-tui components | 1,552,939 / 64 | 1,718,004 / 62 | 1,276,268 / 48 | **-17.8% tokens** |

The core task initially failed the excerpt strategy when routed to the core
module page: its needed material is spread across `State and lifecycle`,
`Failure modes`, and `Key types`, and that page has no dedicated
compaction/retry section. Two runs used 831k and 995k tokens versus a 657k
control median. The failure was page selection, not section extraction: `cm wiki
find --query "compaction retry queued messages failure recovery"` ranks
`key-flows` first, and excerpting its `Failure and recovery` section used 458k
tokens / 28 tools / 107.3s versus 657k / 28 / 198.3s — **-30.3% tokens, -45.9%
wall time**.

Conclusion: format depends on task size and page structure. Small tasks (auth)
are best answered from source directly. Full page is the robust default for
medium/large module pages, positive on core/providers/storage/modes/extensions
and negative on auth/tui. Excerpt wins when the page has a dedicated section
matching the query (providers, storage, key-flows core, tui); it loses on generic
sections (modes, extensions) and small tasks (auth). Use `find -> excerpt` only
when the topic maps cleanly to a section; otherwise use one full page, and skip
wiki entirely for small tasks. Metadata:
`eval/results/wiki-excerpt-ab-2026-11-08.json`.

## Automatic wiki route policy (2026-11-08)

`cm wiki route` combines `find` and `excerpt` and returns
`mode: none|excerpt|full`, the selected page/section, token estimate, reasons,
and the top candidates.

Policy:

1. no page match or top score < 20 -> `none`
2. `--task-size small` -> `none`
3. a selected non-generic H2 heading matches a query term -> `excerpt`
4. otherwise -> one `full` page

Measured decisions against the A/B best format:

| Query | Route | Best measured |
|---|---|---|
| compaction/retry/queues | excerpt key-flows | excerpt |
| provider composition/auth | excerpt providers | excerpt |
| durable storage | full storage | full (excerpt close) |
| modes/RPC | full modes | full |
| extensions | full extensions | full |
| credential resolution (small) | excerpt auth; `--task-size small` -> none | none |
| TUI rendering/input | excerpt tui | excerpt |

So the route matches 6/7 automatically; the small-task case needs
`--task-size small`. The policy deliberately prefers one full page when only
generic sections match, because excerpts of generic sections were worse than
full pages in the measured cases.

## End-to-end `cm wiki route` A/B (2026-11-08)

Treatment ran one `cm wiki route` command, then answered from its output plus
source verification. Control had no wiki. Route decisions were automatic; the
small-task auth run used `--task-size small`.

| Task | Route mode | Control tokens/tools | Route tokens/tools | Delta |
|---|---|---:|---:|---:|
| core compaction/retry/queues | excerpt | 657,422 / 28 | 746,879 / 23 | +13.6% tokens, -17.9% tools, -27.7% wall |
| durable storage | full | 1,016,161 / 35 | 397,018 / 10 | -60.9% tokens, -71.4% tools, -64.3% wall |
| pi-ai auth (small) | none | 220,655 / 17 | 180,021 / 13 | -18.4% tokens, -23.5% tools, -31.8% wall |

Average: **-21.9% tokens**, **-37.6% tool uses**, **-41.3% wall time**. The core
task paid one extra tool turn for the route command, so its token count rose,
but it still used fewer tools and less wall time. Storage and small-task routing
were strongly positive.

Rule: `cm wiki route` is the default entry. Use `--task-size small` for narrow
single-module questions; otherwise let the route choose excerpt vs full.
