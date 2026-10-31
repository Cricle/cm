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
