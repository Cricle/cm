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

Current baseline:

```text
recall@5          0.985
MRR               0.955
hit@1             0.909
no-answer accuracy 0.000
```

The no-answer result is intentionally left visible: `cm search` currently has no score threshold or abstention policy. This is the next accuracy task, not a hidden failure.
