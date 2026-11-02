#!/usr/bin/env python3
"""Deterministic fixture solver used only to smoke-test agent_ab.py.

It is not an AI agent and its numbers must not be reported as a model A/B
result. It reads CM_TASK_ID and applies the known patch so the harness can be
validated end to end without network access.
"""

from __future__ import annotations

import json
import os
from pathlib import Path


def patch_cache(workspace: Path) -> None:
    path = workspace / "src" / "cache.py"
    text = path.read_text(encoding="utf-8")
    text = text.replace(
        "        if len(self.items) >= self.capacity:",
        "        if key not in self.items and len(self.items) >= self.capacity:",
    )
    path.write_text(text, encoding="utf-8")


def patch_config(workspace: Path) -> None:
    for name in ("config.py", "app.py"):
        path = workspace / "src" / name
        text = path.read_text(encoding="utf-8")
        path.write_text(text.replace("old_timeout", "timeout"), encoding="utf-8")


def patch_stats(workspace: Path) -> None:
    path = workspace / "src" / "stats.py"
    text = path.read_text(encoding="utf-8")
    text += """

def median(values):
    if not values:
        raise ValueError("median requires at least one value")
    ordered = sorted(values)
    middle = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[middle]
    return (ordered[middle - 1] + ordered[middle]) / 2
"""
    path.write_text(text, encoding="utf-8")


def main() -> int:
    workspace = Path(os.environ["CM_WORKSPACE"])
    task = os.environ["CM_TASK_ID"]
    {"cache_bug": patch_cache, "config_refactor": patch_config, "feature_median": patch_stats}[task](workspace)
    usage_file = Path(os.environ["CM_USAGE_FILE"])
    usage_file.write_text(
        json.dumps({"tokens": 0, "turns": 1, "tool_calls": 1, "files_read": 1}),
        encoding="utf-8",
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
