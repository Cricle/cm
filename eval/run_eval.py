#!/usr/bin/env python3
"""Retrieval accuracy baseline for cm.

Creates a temporary empty cm store, imports a full JSONL fixture, and runs
the query set through `cm search`. The fixture must contain complete records
(id, kind, scope, subject, text, source, confidence, sources, merged, created_at).
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


def find_cm(explicit: str | None) -> str:
    if explicit:
        return explicit
    if os.environ.get("CM"):
        return os.environ["CM"]
    root = Path(__file__).resolve().parents[1]
    for candidate in (
        root / "cm",
        root / "cm.exe",
        root / "target" / "release" / "cm",
        root / "target" / "release" / "cm.exe",
        root / "target" / "debug" / "cm",
        root / "target" / "debug" / "cm.exe",
    ):
        if candidate.exists():
            return str(candidate)
    found = shutil.which("cm")
    if found:
        return found
    raise SystemExit("cm binary not found; pass --cm or set CM")


def run(cmd: list[str], stdin_path: Path | None = None) -> str:
    stdin = stdin_path.open("rb") if stdin_path else None
    try:
        result = subprocess.run(
            cmd,
            stdin=stdin,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            text=False,
        )
    finally:
        if stdin:
            stdin.close()
    if result.returncode != 0:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(cmd)}\n"
            f"{result.stderr.decode('utf-8', 'replace')}"
        )
    return result.stdout.decode("utf-8", "replace")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cm", help="path to cm binary")
    parser.add_argument("--fixture", type=Path, default=Path(__file__).with_name("fixtures") / "pi.jsonl")
    parser.add_argument("--queries", type=Path, default=Path(__file__).with_name("queries.jsonl"))
    parser.add_argument("--top", type=int, default=5)
    parser.add_argument("--min-score", type=float, default=0.0)
    parser.add_argument("--json-out", type=Path, help="write full metrics JSON")
    args = parser.parse_args()

    cm = find_cm(args.cm)
    queries = [json.loads(line) for line in args.queries.read_text(encoding="utf-8").splitlines() if line.strip()]

    with tempfile.TemporaryDirectory(prefix="cm-eval-") as tmp:
        store = Path(tmp) / "cm"
        run([cm, "init", "--file", str(store), "--empty"])
        run([cm, "import", "--file", str(store)], stdin_path=args.fixture)
        verify = json.loads(run([cm, "verify", "--file", str(store)]))

        details = []
        for item in queries:
            query = item["query"]
            scope = item.get("scope")
            expected = item.get("expected", [])
            cmd = [cm, "search", "--file", str(store), "--query", query, "--top", str(args.top), "--min-score", str(args.min_score)]
            if scope:
                cmd += ["--scope", scope]
            payload = json.loads(run(cmd))
            got = [c["id"] for c in payload.get("candidates", [])]
            expected_set = set(expected)
            hits = [cid for cid in got if cid in expected_set]
            if expected:
                recall = len(hits) / len(expected)
                precision = len(hits) / len(got) if got else 0.0
                reciprocal = 0.0
                for rank, cid in enumerate(got, 1):
                    if cid in expected_set:
                        reciprocal = 1.0 / rank
                        break
                hit_at_1 = 1.0 if got and got[0] in expected_set else 0.0
                no_answer = None
            else:
                recall = precision = reciprocal = hit_at_1 = None
                no_answer = len(got) == 0
            details.append(
                {
                    "query": query,
                    "expected": expected,
                    "got": got,
                    "recall": recall,
                    "precision": precision,
                    "mrr": reciprocal,
                    "hit_at_1": hit_at_1,
                    "no_answer": no_answer,
                }
            )

        positive = [d for d in details if d["expected"]]
        negative = [d for d in details if not d["expected"]]
        avg = lambda values: sum(values) / len(values) if values else 0.0
        metrics = {
            "store_verify": verify,
            "queries": len(details),
            "positive_queries": len(positive),
            "negative_queries": len(negative),
            "recall_at_k": avg([d["recall"] for d in positive]),
            "precision_at_k": avg([d["precision"] for d in positive]),
            "mrr": avg([d["mrr"] for d in positive]),
            "hit_at_1": avg([d["hit_at_1"] for d in positive]),
            "no_answer_accuracy": avg([1.0 if d["no_answer"] else 0.0 for d in negative]),
            "top": args.top,
        }

    result = {"metrics": metrics, "details": details}
    print(json.dumps(result, ensure_ascii=False, indent=2))
    if args.json_out:
        args.json_out.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
