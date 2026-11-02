# Accuracy baseline

This directory contains the M0 retrieval evaluation for `cm`.

- `fixtures/pi.jsonl`: 18 full memory records about the `pi` codebase.
- `queries.jsonl`: 25 queries, including 3 no-answer queries.
- `run_eval.py`: creates an empty store, imports the fixture, runs `cm search`, and reports recall/precision/MRR/no-answer accuracy.
- `baseline.json`: current baseline snapshot.

Run from the crate root:

```bash
cargo build --release
python3 eval/run_eval.py
```

Current baseline (default `--min-score 0`):

```text
recall@5           0.985
MRR                0.955
hit@1              0.909
no-answer accuracy 0.667
```

With the documented abstention threshold:

```bash
python3 eval/run_eval.py --min-score 3.0
```

```text
recall@5           0.985
no-answer accuracy 1.000
```

This is why the eval deliberately reports both modes: thresholding is an explicit accuracy/precision trade-off, not a hidden default.

## Long-task A/B

`agent_ab.py` runs each task in a fresh workspace twice: with a seeded `cm`
store and memory-first prompt, and without `cm`. Success comes from the task
verifier, not the agent's self-report. Tokens/turns are read from an optional
`CM_USAGE_FILE` JSON written by the agent wrapper.

```bash
python3 eval/agent_ab.py \
  --cm target/release/cm.exe \
  --agent-cmd "my-agent --prompt {prompt} --cwd {workspace}" \
  --runs 3
```

Task fixtures live in `eval/tasks/<id>/`: `task.json`, `workspace/`,
`memory.jsonl`, and `verify.py`. `scripted_fixture_agent.py` only smoke-tests the
harness plumbing; do not report its numbers as model results.

### Measured short-task A/B

Three one-session tasks, one run each, using the same model and verifier:

| Task | Mode | Success | Subagent tokens | Tool uses | Wall time |
|---|---|---:|---:|---:|---:|
| cache_bug | with cm | yes | 464,609 | 21 | 117.2s |
| cache_bug | without cm | yes | 294,463 | 14 | 87.3s |
| config_refactor | with cm | yes | 346,012 | 20 | 112.6s |
| config_refactor | without cm | yes | 246,496 | 15 | 99.4s |
| feature_median | with cm | yes | 342,462 | 16 | 102.9s |
| feature_median | without cm | yes | 222,302 | 11 | 65.9s |

All tasks passed in both modes. On these small one-shot tasks, `cm` added about
40–55% tokens and wall time and did not reduce file reads. The correct usage
boundary is therefore: skip memory for trivial one-shot work; use it when work
spans sessions, requires prior decisions, or the repository is large enough that
re-discovery is expensive. The harness is designed to measure those cases.
