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

## What not to claim

`cm` is a memory sidecar. It does not prove reasoning gains from one A/B run;
long-horizon task success and source-correctness remain the deciding metrics.
