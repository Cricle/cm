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
gate passes; the efficiency gate does not. Macro exploration needs denser
per-module coverage before this is a practical memory set.
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
used more tools because the agent still verified source. This is a pilot, not a
general multi-session claim: the build cost needs many follow-up questions or a
coding task with repeated resumptions to amortize. The small config_refactor
multi-session result remains a separate, also-marginal data point.

## What not to claim

`cm` is a memory sidecar. It does not prove reasoning gains from one A/B run;
long-horizon task success and source-correctness remain the deciding metrics.
Only runs with the same task prompt, same model, same repository commit, and the
same available tools count as A/B evidence. A pre-injected brief is a product
demo, not an equal-tools comparison, and must not be mixed into the same table.
