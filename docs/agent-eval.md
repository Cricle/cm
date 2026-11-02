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

## What not to claim

`cm` is a memory sidecar. It does not prove reasoning gains from one A/B run;
long-horizon task success and source-correctness remain the deciding metrics.
