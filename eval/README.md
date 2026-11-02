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
`memory.jsonl`, and `verify.py`. Add an optional `"sessions": ["prompt 1",
"prompt 2"]` array to keep one workspace and one cm store across multiple agent
sessions; the verifier runs once after the last session. `scripted_fixture_agent.py`
only smoke-tests the harness plumbing; do not report its numbers as model results.

For production runners that build the model prompt before the first tool call,
use `cm brief --format prompt` and prepend its stdout to the task. That removes
the brief tool turn entirely; the harness still measures the tool-call path.

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

### Why the overhead: turn amplification

A follow-up cache_bug run removed the ritual calls (`brief` + `recall` +
`remember` + help probes) and kept only one `brief`:

| Mode | Success | Tokens | Tool uses | Wall time |
|---|---|---:|---:|---:|
| with cm, one `brief` | yes | 143,119 | 8 | 41.5s |
| without cm | yes | 118,935 | 6 | 36.2s |

The optimized protocol reduced overhead from +57.8% to +20.3% tokens, but did not
make cm useful: `brief` saved zero file reads because the task prompt and tests
already contained the needed facts, and the workspace had only two source files.
The remaining +24k tokens came from two extra tool turns (one `cm brief` and one
malformed agent call). Each extra turn re-sends the whole conversation and tool
context, so the cost is roughly 12k tokens per turn here — far more than the
1.3KB `cm brief` output.

This is the central rule: **cm must eliminate more tool turns than it adds**.
A session-start `brief` is only net positive when it replaces at least two
discovery/read turns, which requires prior context not already in the prompt and
navigational `path:symbol` sources in a repository large enough to search.

### Realistic read-only A/B

Same model, asking how cache eviction, affinity, cache-stats, source anchors, and
audit work, with exact file/symbol citations:

| Mode | Tokens | Tool uses | Wall time | Files read |
|---|---:|---:|---:|---:|
| with cm brief | 374,808 | 19 | 104.0s | 3 |
| without cm | 986,555 | 34 | 207.1s | 5 |

One brief cut tokens by 62.0%, tool uses by 44.1%, and wall time by 49.8%. That
is the intended use: non-trivial work in a real repository where discovery
would otherwise cost multiple turns.

### Multi-session A/B

Same config_refactor task across two fresh sessions:

| Mode | Session 1 | Session 2 | Total tokens | Tool uses | Wall time |
|---|---:|---:|---:|---:|---:|
| with cm | 80,715 | 122,495 | 203,210 | 18 | 67.4s |
| without cm | 120,346 | 98,179 | 218,525 | 14 | 70.6s |

Both succeeded. The brief helped session 1 but cost more than it saved in
session 2 on this tiny repository; overall it was -7.0% tokens, -4.5% time, and
+28.6% tool uses. Multi-session value needs larger repos and longer sequences to
overcome the fixed per-session brief cost.
