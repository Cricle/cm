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
2. `cm brief --task current` at the start of each resumed step; `cm recall --task <id>` before implementation.
3. At decisions: write one `cm remember` record and update the frame.
4. Mark only materially useful/wrong memories with `cm feedback`.
5. At the end: `cm frame set --task <id> --status done`, `cm audit --root . --apply`,
   then `cm govern`.

Track task success, turns, tokens, repeated file reads, and whether the frame's
next steps stayed current. Compare against the same task with memory disabled.

## What not to claim

`cm` is a memory sidecar. It does not prove reasoning gains from one A/B run;
long-horizon task success and source-correctness remain the deciding metrics.
